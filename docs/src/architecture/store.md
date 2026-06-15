# The SQLite store

## The SQLite store

`musefs-db/src/schema.rs` defines the schema as a single baseline migration
(`MIGRATIONS`); `user_version` records the schema version (1).
The store is the **interface external tools write to** — the beets and Picard
plugins under `contrib/` write tags and art here out-of-band.

- The **baseline schema** (`MIGRATION_V1`): the core tables — `tracks` (one row
  per backing file: path, format, audio byte range, size/nanosecond-mtime/ctime
  stamps, `content_version`), `tags` (multi-value key/value rows ordered by
  `ordinal`, with an optional `value_blob` for binary tags), `art`
  (content-addressed, deduplicated image blobs), `track_art` (per-track art
  links with picture type and ordering), and `structural_blocks` (read-only,
  derived-from-file FLAC `STREAMINFO`/`SEEKTABLE` metadata, **not** part of the
  editable contract). Deleting a track cascades to its `tags` and `track_art`
  rows. Triggers bump the owning track's `content_version`/`updated_at` on any
  `tags`/`track_art` edit; `CHECK` constraints enforce the contract invariants
  below at commit time. A bounded, self-pruning `track_changes` ring (capacity
  8192, `CHANGELOG_CAP`) fed by triggers on `tracks` gives O(changed) refresh —
  every metadata edit funnels through an `UPDATE` on the tracks row, relying on
  SQLite's nested trigger activation (on by default). Freshness-superset
  triggers make `content_version` cover every DB-knowable input to synthesized
  bytes: `art_reject_content_update` (art is content-addressed and immutable),
  `art_ad` (a deleted art row bumps referencing tracks so an orphan rebuilds to
  a clean serve-time error), `tracks_geometry_au` (scanner-owned geometry
  changes), and `structural_blocks_ai`/`_ad`.

### The external-writer contract

**Ownership.** External tools get full read/write on `tags`, `art`, and
`track_art`. The scanner owns the structural columns of `tracks` (`id`,
`backing_path`, `format`, `audio_offset`, `audio_length`, `backing_size`,
`backing_mtime_ns`, `backing_ctime_ns`, `content_version`, `updated_at`) and
all of `structural_blocks`: those are derived from probing the file, and
external tools must run `musefs scan` rather than compute them.

`tracks.fingerprint` and `tracks.content_hash` are also scanner-owned,
read-only-derived columns — like `structural_blocks`, they are never part of
the editable tag contract and external tools never write them.
`fingerprint` is a SHA-256 over the probe's parsed output (deterministic per
file, excludes filesystem stamps such as `mtime`/`ctime`), computed in the
parallel probe worker at zero extra I/O. `content_hash` is a full-file
SHA-256, stored as 64-char hex; it is computed only at the `full` checksum
tier (`--checksum=full`), which requires an eager whole-file read. Neither
column is `UNIQUE` by design — duplicate-content tracks legitimately share
both values. On a normal `scan`, when a probed file's path is not yet in the
store and its fingerprint matches exactly one orphaned row (a row whose
`backing_path` no longer exists on disk), the scanner retargets that row to
the new path in place, preserving its `id`, tags, and art rather than
orphaning them. This is how musefs recovers from a backing-library move or
reorganization: run `musefs scan` after moving files, and existing store rows
follow their backing files to the new locations.

**What the store enforces.** SQLite `CHECK` constraints reject the
malformed *shapes* at commit, so an external writer cannot persist them:

- an unknown `format` string, or a negative length/offset/size/version;
- an `audio_offset + audio_length` running past the stored `backing_size`;
- a binary tag row whose `value` is non-empty;
- an `art.byte_len` that disagrees with its blob, or a `sha256` of the wrong
  length;
- a `picture_type` outside `0..=20`;
- a `tags.key` over 256 chars or `tags.value` over 256 KiB;
- `tags.key` must be non-empty and contain no ASCII control characters (a DB
  `CHECK` enforces this, rejecting violating writes — with one blind spot: an
  embedded NUL terminates SQLite's `length()`/`GLOB`, so a key like `a\0b` slips
  the `CHECK`. The scanner's own floor drops it before insert, and the Vorbis
  path rejects it on synthesis). Additionally, only keys within the Vorbis
  field-name grammar (ASCII `0x20`–`0x7D`, excluding `=`) survive FLAC/Ogg
  synthesis — others are dropped and logged. MP3/M4A custom keys may use the
  wider set (e.g. `=`, `:`, spaces, non-ASCII).
- a `value_blob` over `MAX_BINARY_TAG_BYTES`;
- an `art.mime` over 255 chars or `byte_len` over `MAX_ART_BYTES`;
- a `track_art.description` over 1 KiB;
- a `structural_blocks` row with an unknown `kind`, negative `ordinal`, or `body`
  over the FLAC 24-bit block limit.

**Schema identity.** On open, musefs also validates schema identity: a
`sqlite_master` comparison against a freshly-migrated reference plus `PRAGMA
foreign_key_check`, rejecting anything that is not the canonical latest schema
with a message telling the user to run `musefs scan`. A store whose
`user_version` is *newer* than this binary's latest migration (a future or
third-party tool bumped the schema) is refused up front with a distinct
"store is newer than this binary" error rather than silently treated as
already-migrated — an older binary must not risk misreading a newer contract.

**Art is immutable once written.** `art` rows are content-addressed by
`sha256`; a trigger rejects any in-place `UPDATE` of an art row's
content columns (`data`, `sha256`, `mime`, `byte_len`, `width`, `height`) with
`RAISE(ABORT)` — a multi-row `UPDATE art` touching any content column aborts the
whole statement. To change a track's art, insert a new content-addressed row
and relink it via `track_art` (which bumps `content_version`); do not mutate an
existing row. Deleting an `art` row still referenced by `track_art` (possible
only with `foreign_keys` OFF) bumps every referencing track so the mount serves
a clean `EIO` on the now-orphaned reference instead of stale bytes.

**What musefs defends at serve time.** CHECKs cannot catch a scanner-owned
field mutated to a *well-formed* value that no longer matches the real file
on disk: `backing_size` or `backing_mtime_ns`/`backing_ctime_ns` that drift
from the actual file's stat, or audio bounds that fit the stored
`backing_size` but overrun the file once it has shrunk. musefs re-stats the
backing file on every resolve and treats such rows as untrusted input,
degrading to a controlled
`BackingChanged`/layout error, never undefined behavior. The store's
`CHECK` rejects art over `MAX_ART_BYTES` (16 MiB − 64 KiB) at write time;
resolve also re-checks it (`ArtTooLarge`, all formats) to backstop a writer
that disables check enforcement, and the scanner's ingest-time drop is
tracked in #284.
Referential gaps are treated the same way: a `track_art` row whose `art_id`
has no matching `art` row (an orphan an external writer can produce with FK
enforcement disabled) fails the serve with `EIO` rather than silently dropping
the art.

**Merge vs. replace.** An external writer may **merge** rather than fully
replace text tags — overwriting only the keys it manages and leaving the rest
of the scan-seeded set in place — provided it tracks its own managed-key set
out of band (the beets plugin uses a beets flexattr; the store is not the
place for plugin state). musefs renders tags outside its native VOCAB
(`musefs-format/src/tagmap.rs`) by passthrough (Vorbis uppercased, mp3
`TXXX`, mp4 freeform), so such tags appear but are not guaranteed
byte-identical to a given tagger's own per-format encoding. A merge matches
the keys it manages **case-insensitively**, so a writer's canonical
(lowercase) key replaces a scan-seeded row stored under the backing file's
native case (e.g. Vorbis `LABEL`) instead of coexisting with it — Vorbis keys
render case-insensitively, so two such rows would otherwise duplicate.

**Path layout offload.** External tools can also offload path layout
entirely: a plugin evaluates its own (arbitrarily complex) path logic, writes
the resulting relative path into a custom text tag — e.g. `INSERT INTO tags
(track_id, key, value, ordinal) VALUES (?, 'beets_path', 'Pink
Floyd/Animals/01 Pigs', 0)` — and the user mounts with `--template
'$!{beets_path}'`. Because the field map is just the (lowercased) tag keys,
any number of such tags (`beets_path`, `lidarr_path`, …) can back different
concurrent mounts. The path field keeps embedded `/` as directory separators
but sanitizes each segment and drops empty/`.`/`..` segments, so a
misbehaving writer cannot inject traversal or empty components into the tree.

**The shared Python library.** `contrib/python-musefs/` encodes this contract
for plugin authors, including a generated copy of the schema
(`musefs_common/schema.py`, regenerated from `schema.rs` by a drift-guarded
test — see [CONTRIBUTING](../../../CONTRIBUTING.md)). Its tag/art replace operations
each wrap their `DELETE`+`INSERT` in a SQLite savepoint, so they are
individually atomic and the "caller owns the transaction" guarantee holds even
on an autocommit connection. The [Lidarr integration](../../../contrib/lidarr/README.md)
uses the same shared library from a Custom Script workflow. Its Lidarr
destination tree is only a tracking aid, made of symlinks by default; musefs
remains the consumer-facing filesystem.

CI proves this contract end to end in the `contract` job (see
[CONTRIBUTING](../../../CONTRIBUTING.md)): a Python writer's tags/art, layered on a
scanned track, are synthesized by the Rust serve path and read back by an
independent reader.

External writers prune in one of two ways depending on how they own files.
In-place writers (e.g. the beets plugin) prune by file existence — a removed
backing file drops its row via `prune_missing`. Link-tree writers (e.g. the
Lidarr integration) never delete the backing files they point at, so they prune
by identity instead: a source-reported album/artist deletion removes the rows
carrying the matching MusicBrainz id.

Connections are mode-typed (`Db<ReadWrite>` / `Db<ReadOnly>`), opened in WAL
mode with a busy timeout. The serve path uses a `DbPool` whose per-thread
variant hands each reader thread its own connection — WAL reads never contend.
