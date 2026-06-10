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

## Goal

A `beet musefs` sync should make the mounted view reflect the user's beets
library: every tag beets would write to the file, under recognizable names,
with beets' values winning over the file's embedded values, the file's other
embedded tags preserved, and deletions that *stick*.

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

The existing `fields:` config still merges into / overrides this set.
`beets_path` continues to be emitted as today (governed by `write_path`).
Empty/zero values are dropped; multi-valued fields expand to one row per value.

### 2. Naming & value formatting

Mapping from beets field name to canonical musefs key:

- **Rename table** (only where the conventional musefs key differs):
  - `rg_track_gain → replaygain_track_gain`, `rg_album_gain →
    replaygain_album_gain`, `rg_track_peak → replaygain_track_peak`,
    `rg_album_peak → replaygain_album_peak`
  - `mb_albumid → musicbrainz_albumid`, `mb_artistid → musicbrainz_artistid`,
    and the rest of the `mb_* → musicbrainz_*` family
    (`mb_trackid`, `mb_albumartistid`, `mb_releasegroupid`, `mb_releasetrackid`,
    `mb_workid`)
  - `comments → comment`
- **Plural → singular multi-value twins** (generalizes today's
  genre/composer logic and **fixes the multi-artist collapse**): `artists →
  artist`, `albumartists → albumartist`, `genres → genre`, `composers →
  composer`, `lyricists → lyricist`, etc. Prefer the plural list when present,
  else the singular scalar; expand to one store row per value.
- **Everything else** passes through under its (lowercased) beets field name.
- **Value formatters** (a per-field hook): `rg_*` gains format as `"-7.50 dB"`,
  peaks as a plain float string, matching beets/MediaFile's on-file form. The
  existing `date` assembly from `year`/`month`/`day` stays. `r128_*` values
  pass through as integers (no `replaygain_*` rename — distinct convention).

**Fidelity limitation (explicit non-goal).** musefs renders the ~22 keys in its
native VOCAB (`musefs-format/src/tagmap.rs`) idiomatically per format. The long
tail (sort fields, `asin`, `catalognum`, and `tracktotal`/`disctotal` on
mp3/mp4) passes through and *renders* — Vorbis uppercased, mp3 `TXXX`, mp4
freeform — but is **not** guaranteed byte-identical to what beets itself would
write to that format (e.g. mp3 wants `TRCK` "N/M" rather than a `TRACKTOTAL`
`TXXX`). Full per-format fidelity for the long tail would require extending the
Rust VOCAB and is out of scope here. FLAC/Ogg (Vorbis) get the closest match
because the uppercased key *is* the convention.

### 3. Sync semantics — per-key merge

A new shared primitive in `contrib/python-musefs/src/musefs_common/store.py`,
beside the existing `replace_tags` (which Picard keeps unchanged):

```
merge_tags(conn, track_id, managed_pairs, delete_keys)
```

- For each key present in `managed_pairs`: delete that key's text rows, then
  insert the managed values (M-wins, multi-value safe, ordinals per key).
- For each key in `delete_keys`: delete its text rows (suppress the `B` value).
- Every other text row (unmanaged `B`) is left untouched.
- Binary tags (`value_blob NOT NULL`, scanner-written) are untouched, exactly
  as `replace_tags` already guarantees.

beets' `sync_one`/`sync_files` path switches from `replace_tags` to
`merge_tags`; Picard's path is unchanged.

### 4. Stateful deletion via a beets flexattr

Per item, the plugin stores `musefs_managed` — the sorted list of canonical
keys written in the last sync — as a beets **flexattr** (lives in the beets DB;
never written to the audio file, never sent to the musefs store).

Each sync:

1. Autoscan resets the store's text tags to `B`.
2. Compute `M` (§1–2).
3. Read `prev` from `musefs_managed`; `delete_keys = prev − keys(M)`.
4. `merge_tags(conn, track_id, M, delete_keys)` — unless restore-backing is set
   (see §5), in which case pass `delete_keys = ∅`.
5. Write `keys(M)` back to `musefs_managed` (skipped on dry-run).

First-ever sync has no `prev` → no deletions, just writes `M`. A key the user
never managed is never in `prev`, so it is never deleted → `B` persists. A key
the user managed then dropped is in `prev` but not `M` → deleted and, because
the plugin re-applies this after every autoscan, stays deleted.

### 5. The restore-backing flag

`--restore-backing` on the `beet musefs` subcommand, plus
`restore_backing: no` in config as the default.

- **Default (off):** deletions stick — `delete_keys` is applied, suppressing the
  backing value for any key dropped from `M`.
- **On:** `delete_keys` is skipped, so after the autoscan reset, `B`'s embedded
  value for a dropped key reappears in the view.

**Caveat:** the deletion guarantee depends on autoscan resetting `B` before each
sync. With `autoscan: no`, a deletion only reconciles on the next manual
`musefs scan` + sync; this is documented as a limitation of that mode.

### 6. Picard

Picard receives only the §2 naming additions (so a Picard sync also carries
ReplayGain / MusicBrainz / comment / lyrics / grouping / isrc under canonical
keys). Picard keeps `replace_tags` and gains **no** merge or flexattr
machinery: Picard writes the file's tags directly and is the authority on them,
so full-replace is the correct semantics there. The `merge_tags` primitive is
beets-only.

## Out of scope

- Any Rust change, including extending `tagmap.rs` VOCAB for long-tail fidelity
  or adding native `tracktotal`/`disctotal` keys.
- `musefs-db` schema changes (and therefore the Python schema-mirror regen).
- Changing `musefs scan`'s tag-ingest behavior.
- Porting the merge/state model to Picard or Lidarr.

## Testing

- **`merge_tags` unit** (python-musefs): M-wins over B; unmanaged B persists;
  `delete_keys` suppresses; binary tags survive; multi-value ordinals correct.
- **`map_fields` / `build_records`** (beets): boundary derives from
  `_media_tag_fields`; file-facts excluded; rename table applied; plural→singular
  multi-value expansion (incl. `artists`/`albumartists`); ReplayGain value
  formatting; `fields:` override still works.
- **Stateful deletion integration:** sync → delete a tag in beets → re-sync →
  tag stays gone across the intervening autoscan; `musefs_managed` flexattr
  round-trips.
- **`--restore-backing`:** dropped key reappears from `B` with the flag, stays
  gone without it.
- **e2e (`m e2e`):** scan → beets tags incl. ReplayGain + MusicBrainz +
  an arbitrary field → mount → all visible and correctly named; delete a tag →
  absent at mount; with flag → present again. Byte-identical audio preserved.

Commit any new harness/fixtures in-tree as runnable scripts per repo convention;
keep large generated artifacts gitignored.

## Docs to update

- `contrib/beets/README.md`: field coverage, merge vs replace, the
  `musefs_managed` flexattr, `--restore-backing` / `restore_backing`, and the
  autoscan-off caveat.
- `ARCHITECTURE.md` external-writer-contract section: a writer may merge rather
  than fully replace text tags; note the §2 long-tail fidelity limitation.
