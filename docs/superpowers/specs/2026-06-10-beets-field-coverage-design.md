# beets plugin: full field coverage + stateful merge sync

**Status:** design / approved for planning
**Date:** 2026-06-10
**Scope:** `contrib/beets/`, `contrib/python-musefs/` (shared lib), docs. Picard
gets the naming additions only. No Rust changes, no `musefs-db` schema change.

## Problem

The beets plugin syncs a deliberately minimal 9-field core
(`title, artist, albumartist, album, genre, composer, tracknumber, discnumber,
date`) plus album art and `beets_path`. Everything else a user has in beets —
ReplayGain, MusicBrainz IDs, comment, lyrics, grouping, isrc, multi-valued
artists, and arbitrary fields — is **lost** in the mounted view, because
`sync_one` fully replaces a track's text tags with only the mapped set. A
re-tagged view that drops the user's ReplayGain (breaking player volume
normalization) and identity tags is not what users want.

Two confirmed facts shape the fix:

1. The Rust format layer already renders **arbitrary** tag keys: Vorbis
   (FLAC/Ogg) uppercases an unknown key, mp3 wraps it in `TXXX` (or emits a
   4-byte key like `TBPM` as that text frame), mp4 emits a `----` freeform
   atom. So nothing on the Rust side blocks broader coverage.
2. `musefs scan`'s `ingest()` calls `db.replace_tags()` on every (re)scan
   (`musefs-core/src/scan.rs`), so a re-scan resets a track's stored text tags
   to the file's embedded set. Autoscan runs `musefs scan` before every sync by
   default. This is the baseline the merge model builds on.

## Goals

A `beet musefs` sync — and an automatic sync via the import/write hooks — should
make the mounted view reflect the user's beets library: every tag beets would
write to the file, under recognizable names, with beets' values winning over the
file's embedded values, the file's other embedded tags preserved, and deletions
that *stick*.

## Non-goals

- Any Rust change, including extending `tagmap.rs` VOCAB for long-tail fidelity
  or adding native `tracktotal`/`disctotal` keys.
- `musefs-db` schema changes (and therefore the Python schema-mirror regen).
- Changing `musefs scan`'s tag-ingest behavior.
- Porting the merge/state model to Picard or Lidarr.

## Model

Let `B` = the track's embedded tags (seeded into the store by `musefs scan`)
and `M` = the tags beets manages for that track.

- For any key in both, **M wins**.
- For a key only in `B`, **B persists** (untouched).
- `M` may contain **arbitrary** beets fields, returned faithfully on read.
- Deleting a key from `M` **removes it and keeps it removed** across re-scans;
  it is not refilled from `B`. An opt-in flag reverts to "let `B` come back".

## Design

### 1. Field boundary — "what beets writes to the file"

`M` is derived from `item._media_tag_fields` — beets' own definition of the
fields it writes to a file as tags (~70 fields: `title`, `artist`, `rg_*`,
`mb_*`, `comments`, `bpm`, `isrc`, `lyrics`, `grouping`, `tracktotal`,
`disctotal`, `comp`, `asin`, `catalognum`, …). Read-only file facts
(`bitrate, length, samplerate, bitdepth, channels, format, bitrate_mode,
encoder_info, encoder_settings`) are **excluded automatically** because beets
classifies them in `_media_fields` but not `_media_tag_fields` — no
hand-maintained denylist, and the set tracks beets as it evolves.

**Drop predicate (precise).** Drop a value only when it is `None` or an
empty-after-strip string. **Numeric zero is kept** — `replaygain_track_gain ==
0.00 dB`, `bpm == 0` mean different things but are both legitimate, and dropping
`0 dB` would silently break the ReplayGain guarantee this design exists to
deliver. The sole drop-on-zero exceptions are `tracknumber` and `discnumber`
(a 0 track/disc is noise, as today). `tracktotal`/`disctotal` of 0 are dropped
too (no meaningful "of 0").

**`fields:` override.** The existing `fields:` config is applied as a *final
override layer after* the §2 rename table: last-wins on store-key collision, and
a `fields:` entry **may re-introduce an auto-excluded file fact** (explicit user
intent beats the auto-exclusion). This is the one path by which a non-tag field
can reach the store; it is opt-in and tested.

**`beets_path`.** Still emitted when `write_path` is on (as today), but it is now
a first-class **plugin-synthesized managed key**: it is included in `keys(M)` /
`musefs_managed` (§4) so that turning `write_path: no` lands it in `delete_keys`
and removes the stale row. Because it is synthesized (no `B` counterpart), the
`--restore-backing` flag (§5) does not apply to it — when dropped it is simply
deleted.

Multi-valued fields expand to one store row per value (§2 twin handling).

### 2. Naming, multi-value, and value formatting

Mapping from beets field name to canonical musefs key, applied in this order:

1. **Twin pre-pass (multi-value).** `_media_tag_fields` contains *both* singular
   and plural forms for some concepts (`artist`+`artists`,
   `albumartist`+`albumartists`), and plural-only forms for others (`genres`,
   `composers`, `lyricists`, `arrangers`), plus `*_sort`/`*_credit` variants
   (`artist_sort`/`artists_sort`, `artist_credit`/`artists_credit`, …). For each
   such pair, **prefer the plural list when present and non-empty, else the
   singular scalar**, and remove the non-chosen form from the iterated set so
   each canonical store key is produced by exactly one source (no double rows).
   The chosen plural expands to one row per value. The canonical store keys are
   the musefs singular forms (`artist`, `albumartist`, `genre`, `composer`,
   `lyricist`, …). *This fixes today's multi-artist collapse.*
2. **Rename table** (only where the conventional musefs key differs from the
   beets field name):
   - `rg_track_gain → replaygain_track_gain` (+ `album_gain`, `track_peak`,
     `album_peak`)
   - `mb_albumid → musicbrainz_albumid`, `mb_artistid → musicbrainz_artistid`,
     and the rest of the `mb_* → musicbrainz_*` family (`mb_trackid`,
     `mb_albumartistid`, `mb_releasegroupid`, `mb_releasetrackid`, `mb_workid`)
   - `comments → comment`
3. **Everything else** passes through under its lowercased beets field name.
4. **`fields:` override** (§1) is applied last.

**Stringification.** Default rule: ints render as `str(int)` with no trailing
`.0`; bools render as `"1"`/`"0"` (matching beets/MediaFile's on-file form for
`comp`, **not** `"True"`); lists are handled by the twin pre-pass; everything
else is `str(value).strip()`. The **value-formatter table is the explicit set of
exceptions** to that default: the four `rg_*` gains format as `"-7.50 dB"`,
the `rg_*` peaks as a plain float string, and `date` is assembled from
`year`/`month`/`day` (as today). `r128_*` values pass through as integers (no
`replaygain_*` rename — a distinct convention). Each formatter exception gets a
dedicated test.

**Fidelity limitation (explicit non-goal).** musefs renders the keys in its
native VOCAB (`musefs-format/src/tagmap.rs`, a curated subset of roughly two
dozen keys) idiomatically per format. Of the renamed family, only
`musicbrainz_albumid`/`musicbrainz_artistid` and the `replaygain_*` set are in
VOCAB; the remaining `mb_*`, the sort/credit fields, `asin`, `catalognum`, and
`tracktotal`/`disctotal` are **long tail** — they pass through and *render*
(Vorbis uppercased, mp3 `TXXX`, mp4 freeform) but are **not** guaranteed
byte-identical to what beets itself writes to that format (e.g. mp3 wants `TRCK`
"N/M" for the track/total rather than a `TRACKTOTAL` `TXXX`). Full per-format
fidelity for the long tail would require extending the Rust VOCAB and is out of
scope. FLAC/Ogg (Vorbis) get the closest match because the uppercased key *is*
the convention.

### 3. Sync semantics — per-key merge

A new shared primitive in `contrib/python-musefs/src/musefs_common/store.py`,
beside the existing `replace_tags` (which Picard keeps unchanged):

```
merge_tags(conn, track_id, managed_pairs, delete_keys)
```

- For each key present in `managed_pairs`: delete that key's **text** rows, then
  insert the managed values. The delete is scoped
  `DELETE FROM tags WHERE track_id=? AND key=? AND value_blob IS NULL` so
  scanner-written **binary** tags (`value_blob NOT NULL`) sharing a key are never
  clobbered. Inserted ordinals run `0..n` per key (mirroring `replace_tags`,
  which the store reads back `ORDER BY key, ordinal`).
- For each key in `delete_keys`: delete its text rows, scoped the same
  (`value_blob IS NULL`).
- Every other text row (unmanaged `B`) is left untouched; binary tags untouched.

beets' `sync_one`/`sync_files` path switches from `replace_tags` to
`merge_tags`; Picard's path is unchanged.

### 4. Stateful deletion via a beets flexattr — on **both** sync paths

Per item, the plugin stores `musefs_managed` — the sorted list of canonical keys
written in the last sync, including `beets_path` when emitted — as a beets
**flexattr** (lives in the beets DB; never written to the audio file, never sent
to the musefs store).

Each sync (the explicit `beet musefs` command **and** the passive
`_reconcile_pending` `cli_exit` hook — see §4a):

1. Autoscan resets the store's text tags to `B`.
2. Compute `M` (§1–2).
3. Read `prev` from `musefs_managed`; `delete_keys = prev − keys(M)`.
4. `merge_tags(conn, track_id, M, delete_keys)` — unless restore-backing is set
   (§5), in which case pass `delete_keys = ∅`.
5. Persist `keys(M)` back to `musefs_managed` via `item.store()` (skipped only on
   the command path's `-n` dry-run, which writes nothing).

First-ever sync has no `prev` → no deletions, just writes `M`. A key the user
never managed is never in `prev`, so it is never deleted → `B` persists. A key
the user managed then dropped is in `prev` but not `M` → deleted and, because the
plugin re-applies this after every autoscan, stays deleted.

#### 4a. Both paths, identically — the passive `cli_exit` path is primary

The plugin's *most common* sync is the passive one: `after_write` /
`item_imported` / `album_imported` listeners record touched items, and
`_reconcile_pending` (registered on `cli_exit`, `musefs.py`) syncs them at their
final path. A user who removes ReplayGain via `beet modify -w` or re-imports goes
through **this** path, not `beet musefs`. Therefore the full §4 cycle (read
`prev` → compute `delete_keys` → `merge_tags` → persist `musefs_managed`) **must
run identically in `_reconcile_pending`**, not only in the command. The passive
path has no dry-run, so step 5 always persists.

**Re-entrancy.** Persisting the flexattr uses `item.store()` (a beets *library
DB* write), which does **not** emit `after_write` (that event fires only on
`item.write()`, a file write). So writing `musefs_managed` inside the hook cannot
re-trigger the record→reconcile loop. The plan must use `item.store()`, never
`item.write()`, for this state.

**Best-effort contract preserved.** `_reconcile_pending` already swallows
environmental errors into warnings so a passive hook never aborts the beets
operation; the added flexattr/merge work stays inside that same guard.

### 5. The restore-backing flag

`--restore-backing` on the `beet musefs` subcommand, plus `restore_backing: no`
in config as the default. (The passive path honors the config default; the flag
is command-only.)

- **Default (off):** deletions stick — `delete_keys` is applied, suppressing the
  backing value for any key dropped from `M`.
- **On:** `delete_keys` is skipped, so after the autoscan reset, `B`'s embedded
  value for a dropped key reappears in the view. (Synthesized keys like
  `beets_path` have no `B` and are deleted regardless — §1.)

**Caveat — the guarantee is contingent on the store holding `B` at sync time.**
That is what `autoscan: yes` (the default) guarantees by resetting the store
before each sync. With `autoscan: no`, step 1 does not run, the store may hold
stale prior-sync rows rather than fresh `B`, and a deletion only reconciles on
the next manual `musefs scan` + sync. Docs steer deletion-sensitive users to
keep autoscan on. Query/partial syncs (`beet musefs QUERY`, which scans only
matched files) are per-item safe: each item owns its own `musefs_managed`
flexattr, so syncing a subset never disturbs unmatched items' managed state.

### 6. Picard

Picard receives only the §2 naming additions (so a Picard sync also carries
ReplayGain / MusicBrainz / comment / lyrics / grouping / isrc under canonical
keys). Picard keeps `replace_tags` and gains **no** merge or flexattr
machinery: Picard writes the file's tags directly and is the authority on them,
so full-replace is the correct semantics there. The `merge_tags` primitive is
beets-only.

## Testing

- **`merge_tags` unit** (python-musefs): M-wins over B; unmanaged B persists;
  `delete_keys` suppresses; binary tags (`value_blob NOT NULL`) survive a
  same-key text merge and a same-key delete; multi-value ordinals contiguous
  per key.
- **`map_fields` / `build_records`** (beets): boundary derives from
  `_media_tag_fields`; file-facts excluded; twin pre-pass picks plural-over-
  singular and emits exactly one source per store key (incl. `artist`/`artists`,
  `albumartist`/`albumartists`, the `*_sort`/`*_credit` variants); rename table
  applied; ReplayGain `"x.xx dB"` / peak formatting; bool `comp` → `"1"`/`"0"`;
  int with no trailing `.0`; **numeric-zero `replaygain_track_gain` survives**;
  `fields:` override wins on collision and can re-introduce a file fact;
  `beets_path` present in `keys(M)`.
- **Stateful deletion integration — both paths:** (a) `beet musefs` → delete a
  tag in beets → re-sync → tag stays gone across the intervening autoscan; (b)
  the same via the `cli_exit` reconcile path (simulate `item_imported` /
  `after_write` → `_reconcile_pending`); `musefs_managed` flexattr round-trips
  and is written with `item.store()` (assert no `after_write` re-entrancy).
- **`beets_path` lifecycle:** `write_path: yes` then `no` → the `beets_path` row
  is removed (it was in the managed set), unaffected by `--restore-backing`.
- **`--restore-backing`:** dropped key reappears from `B` with the flag, stays
  gone without it.
- **e2e (`python -m pytest -m e2e`):** scan → beets tags incl. ReplayGain +
  MusicBrainz + an arbitrary field → mount → all visible and correctly named;
  delete a tag → absent at mount; with flag → present again. Byte-identical
  audio preserved.

Commit any new harness/fixtures in-tree as runnable scripts per repo convention;
keep large generated artifacts gitignored.

## Documentation

- `contrib/beets/README.md`: field coverage, merge vs replace, the
  `musefs_managed` flexattr, `--restore-backing` / `restore_backing`, the
  passive-path behavior, and the autoscan-off caveat.
- `contrib/python-musefs/README.md`: document `merge_tags` as a new public
  store-contract primitive alongside `replace_tags`.
- `ARCHITECTURE.md` external-writer-contract section: a writer may merge rather
  than fully replace text tags; note the §2 long-tail fidelity limitation.

## Open risks the plan must resolve

- Confirm `item.store()` inside a `cli_exit` hook persists a flexattr without
  firing `after_write` on the running beets version(s) the suite targets (the
  re-entrancy assumption in §4a). If it does fire, the plan needs a guard.
- Confirm `_media_tag_fields` is stable across the supported beets versions
  (1.x vs 2.x) and that the twin pairs (§2) exist in each; fall back gracefully
  (singular-only) where a plural is absent.
