# beets-musefs plugin — design

**Date:** 2026-05-25
**Status:** Approved for planning
**Scope:** A beets plugin that syncs beets' canonical metadata (tags + cover art)
into the musefs SQLite store, so a live musefs mount re-synthesizes FLAC/MP3
headers from beets without rewriting any audio bytes. First integration target
toward the roadmap's "beets / picard plugins" item.

---

## 1. Goal & motivation

musefs presents a virtually re-tagged view of a music library backed by a SQLite
store; the roadmap explicitly targets that store as the *contract* external tools
write to. This plugin is the first such tool. It lets the author drive musefs
against a real beets library end to end:

```
musefs scan ~/music --db ~/musefs.db          # rows + structural columns + seed tags/art
beet musefs sync                               # overwrite tags/art from beets (whole library)
musefs mount ~/mnt --db ~/musefs.db \
    --template '$albumartist/$album/$tracknumber - $title'
# later: `beet modify -w …` → after_write hook re-syncs → mount auto-refreshes (no remount)
```

The headline outcome: browsing `~/mnt` shows FLAC/MP3 files whose synthesized
metadata reflects beets, while the original audio bytes are served verbatim.

## 2. Division of labour (the central invariant)

The DB has two kinds of columns on `tracks`, owned by two different tools:

| Owner | Columns / tables | How |
|---|---|---|
| **`musefs scan`** | track existence; `audio_offset`, `audio_length`, `backing_size`, `backing_mtime`, `format`; seeds `tags`/`track_art` from embedded metadata | probes each backing file |
| **this plugin** | `tags`; `track_art` (when beets has art) | writes beets' view onto rows scan already created |

**The plugin never creates track rows and never writes structural columns.** A
beets item whose path is not already a track row is skipped. This is deliberate:
only musefs's Rust probing can compute the byte-surgery offsets correctly, and the
synthesis path depends on them. The plugin's writes are exactly the "external tag
edits" that `scan --revalidate` is documented to preserve.

The plugin depends only on the SQLite *contract* (schema + triggers), not on the
musefs binary. Sync needs no musefs process running; `scan` and `mount` are
separate steps the user runs.

## 3. The contract the plugin honours

Derived from the current code (`musefs-db/src/schema.rs`,
`musefs-core/src/scan.rs`, `musefs-format/src/{flac,mp3}.rs`):

### 3.1 Track identity — `backing_path`

`scan` stores `backing_path = std::fs::canonicalize(path)` (absolute, symlinks
resolved). The plugin must produce a **byte-identical** key to match a row, so it
keys on `os.path.realpath(item.path)` (beets stores `item.path` as bytes; decode
with the filesystem encoding). `realpath` mirrors `canonicalize` on Linux for
existing files. No matching row → skip the item.

### 3.2 Tag keys — Vorbis-style lowercase

DB tag keys do double duty: they drive both path-template fields and format
synthesis. The format layer expects canonical lowercase keys:

- **MP3** (`mp3.rs::key_to_frame`): `title`→TIT2, `artist`→TPE1, `album`→TALB,
  `albumartist`→TPE2, `tracknumber`→TRCK, `discnumber`→TPOS, `date`→TDRC,
  `genre`→TCON, `composer`→TCOM. Unknown keys become `TXXX` user frames.
- **FLAC** (`flac.rs`): keys are written as Vorbis comments (upper-cased field
  names).

beets' own field names differ, so the plugin maps them (see §5).

### 3.3 Schema version guard

`schema.rs::migrate` sets `PRAGMA user_version` to the number of applied
migrations; the current schema is `user_version = 1`. The plugin reads
`PRAGMA user_version` and **refuses to run** unless it equals the version it was
written against (1), with a message telling the user their musefs/plugin versions
have diverged. This is the safety net for the direct-SQLite-write approach.

### 3.4 Art — content-addressed, deduplicated

`art` rows are content-addressed by `sha256` (UNIQUE) with
`INSERT … ON CONFLICT(sha256) DO NOTHING` then `SELECT id`. The plugin mirrors
this exactly with lowercase `hashlib.sha256(data).hexdigest()`. `track_art` links
a track to an art row with `picture_type` (3 = front cover), `description`,
`ordinal` (PK is `(track_id, ordinal)`). The size cap scan applies is
`MAX_ART_BYTES = 16 MiB − 64 KiB`; the plugin honours the same cap.

### 3.5 Triggers do the rest

Any insert/update/delete on `tags` or `track_art` fires triggers that bump the
track's `content_version` and `updated_at`. Committing the transaction changes the
whole-DB `PRAGMA data_version`. musefs's `HeaderCache` rebuilds the affected
layout on a `content_version` mismatch, and `Musefs::poll_refresh` rebuilds the
tree + clears the cache on a `data_version` change. So a committed sync surfaces
at the mount with no remount — the plugin does nothing special to trigger this.

## 4. Components

A single-file beets plugin in **`contrib/beets/beetsplug/musefs.py`**, packaged as
a `beetsplug` namespace package (with a minimal `pyproject.toml` and a README
showing the `pluginpath`/config setup). Pieces:

- **`MusefsPlugin(BeetsPlugin)`** — reads config (`db`, optional `fields`
  override), registers the command and event listeners.
- **`commands()`** → `beet musefs sync [QUERY]` (a beets `Subcommand`) with
  options `--db PATH`, `--dry-run`, `--verbose`. Resolves items from `QUERY`
  (default: the whole library), opens the DB, calls `sync_items`, prints a
  summary.
- **Event listeners** → `after_write` (covers `beet modify -w` and import writes)
  and `item_imported` / `album_imported`, each calling `sync_items` for the
  affected item(s). Same code path as the command.
- **`sync_items(items, conn, opts) -> SyncStats`** — the core. Opens one
  transaction; for each item: resolve `realpath` → look up `track_id` → replace
  tags (§5) and, when beets has art, replace `track_art` (§6). Returns counts.
- **`map_fields(item) -> list[(key, value)]`** — pure function, beets fields →
  musefs keys. Unit-testable with no DB.
- **DB helpers** — small functions for the exact SQL: `track_id_for_path`,
  `replace_tags`, `upsert_art`, `replace_track_art`, `check_schema_version`.

`sync_items` accepts an open `conn` so the command (whole-library, one
transaction) and the event hooks (per-affected-item) share identical logic.

## 5. Field mapping (tags)

Default beets-field → musefs-key table, **overridable/extendable** via the
`musefs.fields` config:

| beets field | musefs key | note |
|---|---|---|
| `title`, `artist`, `albumartist`, `album`, `genre`, `composer` | same | direct copy |
| `track` | `tracknumber` | int → str; omit if 0 |
| `disc` | `discnumber` | int → str; omit if 0 |
| `year` (+ `month`, `day` if present) | `date` | `YYYY`, or `YYYY-MM-DD` when month/day are set |

Rules:
- Empty strings and zero numerics are **omitted** — no empty tags written.
- v1 writes **one value per key** at `ordinal 0`. (The schema supports multi-value
  via `ordinal`; multi-valued artists/genres are a noted future extension.)
- Writing replaces the track's whole tag set: `DELETE FROM tags WHERE track_id=?`
  then batched `INSERT`. beets is authoritative for tags, overwriting scan's seed.
- A `musefs.fields` config entry maps an extra beets field to a musefs key, e.g.
  `comments: comment` → written as a `TXXX`/`COMMENT` Vorbis comment.

## 6. Art sync

Source is the item's **album cover** via `item.get_album().artpath` (the external
cover file beets manages, e.g. `cover.jpg`). Algorithm per sync run:

1. Group items by album; read+hash each distinct `artpath` **once per run** (cache
   `art file → art_id`).
2. Skip files larger than `MAX_ART_BYTES`; count as `skipped_art`.
3. Sniff mime from magic bytes (`FF D8 FF` → `image/jpeg`, `89 50 4E 47` →
   `image/png`); fall back to the file extension. `width`/`height` are stored
   `NULL` (no image-decode dependency in v1).
4. `INSERT INTO art (sha256, mime, width, height, byte_len, data) VALUES (…)
   ON CONFLICT(sha256) DO NOTHING`, then `SELECT id`.
5. Replace the track's art: `DELETE FROM track_art WHERE track_id=?` then one
   `INSERT` with `picture_type=3`, `description=''`, `ordinal=0`.

**Conditional replacement (important):** `track_art` is replaced **only when beets
has art for the item**. If beets has no album art, the plugin leaves `track_art`
untouched, preserving any embedded art `scan` ingested — beets art wins when
present, embedded art survives otherwise. (Tags, by contrast, are always fully
replaced.) A future `musefs.art` mode (`prefer-beets` | `replace` | `skip`) can
make this configurable; v1 is fixed at `prefer-beets`.

**Orphaned art:** replacing `track_art` can leave previously-referenced `art` rows
unreferenced. The plugin does **not** GC; `musefs scan --revalidate` already
garbage-collects orphaned art. Documented, not handled here.

v1 art source is `album.artpath` only; extracting embedded art from files (via a
tag library) is out of scope.

## 7. Configuration

beets `config.yaml`:

```yaml
musefs:
  db: ~/musefs.db        # required; path to the musefs SQLite store
  fields:                # optional: extend/override the default field map
    comments: comment
```

`--db` on the command overrides `musefs.db`. No other config in v1.

## 8. Error handling & concurrency

- **DB file missing** → clear error: run `musefs scan --db <path>` first.
- **`user_version` mismatch** → refuse with a version-divergence message (§3.3).
- **Item path not a track row** → counted as `skipped`, sync continues.
- **SQLite locking** → set `busy_timeout` (e.g. 5 s); keep transactions short so
  the mount's `data_version` poll sees updates promptly. The command uses one
  transaction for the run; event hooks use a short transaction per affected item.
- **Read-only / unwritable DB** → surface the SQLite error with context.
- **`--dry-run`** → resolve + map + report counts, write nothing, no transaction
  committed.
- **Summary** always reports: `synced`, `skipped` (no row), `art_linked`,
  `skipped_art` (oversized / no art).

## 9. Testing

- **Unit (`map_fields`)** — direct fields; `track`→`tracknumber`,
  `disc`→`discnumber`; `year`/`month`/`day`→`date` formatting; empty/zero omission;
  `musefs.fields` override applied. No DB needed.
- **Unit (mime sniff)** — JPEG/PNG magic-byte detection and extension fallback.
- **Integration (pytest)** — create a temp DB with the musefs schema
  (`PRAGMA user_version = 1` + the V1 DDL/triggers), insert a fake track row, run
  `sync_items` over constructed beets items, then assert: `tags` rows match the
  mapping; `content_version` bumped; art deduped (same bytes on two tracks → one
  `art` row, two `track_art` rows); conditional art replacement (no beets art →
  pre-existing `track_art` preserved).
- **Manual end-to-end** — a documented `scan → sync → mount` script over a small
  fixture library, verifying the mounted files carry beets' tags/art. Full
  cross-language e2e in CI is out of scope for v1.

The integration test embeds a copy of the V1 schema DDL; if it drifts from
`musefs-db`, the `user_version` guard (§3.3) and this test are the two places it
surfaces.

### 9.1 Path-matching robustness (gate)

The `realpath`/`canonicalize` agreement (§3.1, §11) is the single silent-failure
point, so it gets its own test tier that runs against the **real `musefs scan`
binary** — never a hand-built DB — to prove the plugin's key is byte-identical to
what scan actually stored. Marked `#[ignore]`-style / opt-in like the FUSE e2e
since it shells out to a built `musefs` binary; CI builds it first.

The test harness, for each case:
1. materialises a fixture file under a temp tree,
2. runs `musefs scan <tree> --db <tmp.db>`,
3. reads back the stored `backing_path` from `tracks`,
4. computes the plugin's key from the beets-style item path, and
5. asserts the two strings are **exactly equal** — and, end to end, that
   `track_id_for_path(plugin_key)` returns the row scan created.

Cases that must pass (each as the path beets would hand the plugin):
- plain nested file (`Artist/Album/01 Track.flac`);
- a **symlinked directory component** in the path (resolved away by both sides);
- a **symlink to the file** itself;
- input given as a **relative** path and as a path containing `.`/`..` segments;
- a **trailing slash** on a parent and other non-normalised forms;
- **non-ASCII / Unicode** filename (e.g. accented + CJK), and a filename with
  **spaces and `%`**;
- (documented, not asserted to match) a path under a **different mount/bind** than
  the scanned tree — asserts it is reported as `skipped (no row)`, never a silent
  wrong-row hit.

A failure here is a hard stop, not a warning: if any case mismatches, the keying
strategy is wrong and must be fixed (e.g. normalise both sides identically) before
the plugin is usable. The unit-level path test is subsumed by this tier.

## 10. Explicit non-goals (v1)

- **Picard plugin** — separate effort later.
- **Multi-valued tags** — one value per key for now.
- **Embedded-art extraction** — art comes from `album.artpath` only.
- **Path remapping** (beets `directory` ≠ the tree musefs scanned) — assume the
  same tree; both sides canonicalized via `realpath`/`canonicalize`.
- **Creating track rows / writing structural columns** — owned by `musefs scan`.
- **Orphaned-art GC** — owned by `musefs scan --revalidate`.

## 11. Open risk / validation note

`os.path.realpath` (Python) vs `std::fs::canonicalize` (Rust) are expected to
agree for existing files on Linux, but this is the single point where a mismatch
would silently cause every item to be "skipped (no row)." This risk is retired by
the **§9.1 path-matching gate**, which asserts byte-identical keys against a DB
produced by the real `musefs scan` binary across symlink, relative/`..`,
trailing-slash, and Unicode/space cases — a mismatch there is a hard stop on the
keying strategy, not a warning.
