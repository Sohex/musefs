# The store & external-writer contract

## The SQLite store

`musefs-db/src/schema.rs` defines the schema as an ordered list of migrations
(`MIGRATIONS`: the `MIGRATION_V1` baseline, `MIGRATION_V2`, which adds the
scanner-owned `fingerprint`/`content_hash` columns, `MIGRATION_V3`, which
widens the `tags.value` and `track_art.description` caps, and `MIGRATION_V4`,
which rebuilds every core table — a never-reused `AUTOINCREMENT` id, the
path as bytes, an inode stamp, storage-class constraints throughout, independent
ordinal spaces for text and binary tags, the picture's description moved off the
shared blob and onto the art link, immutable row ownership, and the retirement of
every fingerprint written
before the value included sampled audio); `user_version` records the schema
version (4).
The store is the **interface external tools write to** — the beets and Picard
plugins under `contrib/` write tags and art here out-of-band.

- The **baseline schema** (`MIGRATION_V1`): the core tables — `tracks` (one row
  per backing file: path, format, audio byte range, the
  size/nanosecond-mtime/ctime freshness stamp — joined by the inode in v4 —
  and `content_version`), `tags` (multi-value key/value rows ordered by
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
  changes), and `structural_blocks_ai`/`_ad`. `tags_reject_reparent` and
  `track_art_reject_reparent` make row ownership immutable, for the same reason
  art content is.

### Transparent and gated migrations

Each entry in `MIGRATIONS` declares itself **transparent** or **gated**. A transparent step is
applied as a side effect of opening the store, which is how every migration up
to 1.3.0 behaved: nobody running `mount` or `scan` learns it happened, and for a
step whose cost and consequences they would not notice that is right. A gated
step is one that rewrites data nobody asked to have rewritten, transiently needs
the store's size again in free disk, or ends compatibility with the binary they
were running yesterday. Opening a store that needs one refuses with
`DbError::StoreNeedsMigration`, naming
[`musefs migrate`](../guide/maintenance.md#upgrading-the-store-musefs-migrate).
That command drives `PendingMigration`, the one door that applies a gated step:
it opens the store *without* migrating or validating it — the schema is by
definition not the one this build expects — exposes the versions, the step
list, an exclusive claim and a `VACUUM INTO` snapshot for the command's
pre-flight, and hands back an ordinary `Db` once the migration has run.

**A gated step ships only in a major release.** This is the contract, and the
compiler enforces it: each entry records the release that introduced it, and a
`const` assertion rejects the build if a `Gated` step names anything but an
`x.0.0`. What a user needs to know is then not which `user_version` they are on
but whether the release they are moving to crossed a major boundary — crossing
one may ask for `musefs migrate`, a minor or a patch never will. The converse is
deliberately not asserted: a major is free to carry only transparent steps, as
`MIGRATION_V3` is, or none at all.

Nothing about the classification is stored: the binary owns it, so a user
jumping from 1.2 straight to 2.1 is still gated on the step that needs it. Two
further rules follow from what the runner already knows. A store being *created*
is exempt — it has no data to endanger, and gating it would stop `scan` from
ever building a new library. And the transparent steps ahead of a gate are
applied and committed before the refusal, because a step is transparent for
reasons a later gated step does not retroactively change.

`DbError::StoreNeedsMigration` is the opposite direction from
`DbError::StoreTooNew`, and carries the opposite remedy: upgrade the store, not
the binary.

### The external-writer contract

**Ownership.** External tools get full read/write on `tags`, `art`, and
`track_art`. The scanner owns the structural columns of `tracks` (`id`,
`backing_path`, `format`, `audio_offset`, `audio_length`, `backing_size`,
`backing_mtime_ns`, `backing_ctime_ns`, `backing_ino`, `content_version`,
`updated_at`) and all of `structural_blocks`: those are derived from probing
the file, and external tools must run `musefs scan` rather than compute them.

**`backing_ino` is a bit pattern, not a magnitude.** From schema v4 `tracks`
records the backing file's inode as part of the freshness stamp, stored as the
inode's two's-complement `i64` bit pattern — so a file whose inode is above
`i64::MAX` has a *negative* value in the column. SQLite has no unsigned 64-bit
integer and `st_ino` is a full `u64`, so some encoding is forced; this one is a
bijection, and the column is only ever compared for equality (the invalidation
trigger, and the Rust freshness stamp), never ordered or summed. Zero is the
sentinel for "not recorded", which every row in a store upgraded to v4 carries
until a scan fills it in. A reader decoding this column must cast the bit
pattern back rather than treat a negative value as invalid.

**`backing_path` is bytes, not text.** From schema v4 it is a `BLOB` and the
Rust model is a `PathBuf`, because a filesystem path is a byte string and the
lossy text round-trip collapsed two distinct files onto one row. This matters to
a *reader* as much as a writer:
SQLite never compares a `TEXT` value equal to a `BLOB`, so a lookup that binds a
string matches nothing at all rather than failing, and a `CHECK` pins the
storage class so the two spellings cannot become two rows for one file. The
`contrib` helpers encode and decode at the boundary (`path_param` and
`path_value` in `musefs_common`); a third-party writer must do the same.

`tracks.fingerprint` and `tracks.content_hash` are also scanner-owned,
read-only-derived columns — like `structural_blocks`, they are never part of
the editable tag contract and external tools never write them.

`fingerprint` is a SHA-256 over the probe's parsed output — format, audio
bounds, text tags, art, binary tags, structural blocks — plus three bounded
windows of sampled audio, taken at the start, midpoint and end of the audio
region. It is deterministic per file and excludes every filesystem stamp such
as `mtime`/`ctime`. The audio windows are what make it content-discriminating
outside FLAC: the parsed output alone carries no audio bytes for MP3, M4A, Ogg
or WAV, so two different files with the same tags, the same art and an equal
audio length used to share one fingerprint, and a move could retarget the wrong
one. Sampling adds at most 24 KiB of positioned reads per file, against the
descriptor the probe already holds. It samples the audio rather than hashing all
of it, so the fingerprint stays a heuristic: two files agreeing on every sampled
window and differing only between them still collide, which is what
`content_hash` arbitrates.

`content_hash` is a full-file SHA-256 of the *current* backing file, stored as
64-char hex. It is computed only at the `full` checksum tier
(`--checksum=full`), which requires an eager whole-file read.

Two rules keep "of the current backing file" true. Both checksums are derived
inside the probe's `fstat` sandwich, from its own descriptor rather than by
reopening the pathname, so a file that changes mid-probe is discarded as raced
instead of committing a row whose stamp, geometry and tags describe one
generation and whose hash describes another. And a pass that computes no full
hash never leaves a stale one behind: every checksum write carries an explicit
intent — keep the stored value, set a new one, or clear it — and a pass below
the `full` tier clears the column whenever it observes that the recorded bytes
changed. A pass over a file that has not changed keeps what is stored, so a
cheap pass never undoes an expensive one.

Neither column is `UNIQUE` by design — duplicate-content tracks legitimately
share both values. On a normal `scan`, when a probed file's path is not yet in
the store and its fingerprint matches exactly one orphaned row (a row whose
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
- a `tags.key` over 256 chars or `tags.value` over 16 MiB − 1 bytes (FLAC's
  24-bit metadata-block ceiling — the largest tag synthesis could serve, so the
  store never refuses a tag the format could carry);
- `tags.key` must be non-empty and contain no ASCII control characters (a DB
  `CHECK` enforces this, rejecting violating writes — with one blind spot: an
  embedded NUL terminates SQLite's `length()`/`GLOB`, so a key like `a\0b` slips
  the `CHECK`. The scanner's own floor drops it before insert, and the Vorbis
  path rejects it on synthesis; see **The NUL blind spot** below for what the
  readers do about a row an external writer plants). Additionally, only keys within the Vorbis
  field-name grammar (ASCII `0x20`–`0x7D`, excluding `=`) survive FLAC/Ogg
  synthesis — others are dropped and logged. MP3/M4A custom keys may use the
  wider set (e.g. `=`, `:`, spaces, non-ASCII).
- a `value_blob` over `MAX_BINARY_TAG_BYTES`;
- an `art.byte_len` over `MAX_ART_BYTES`;
- a `track_art.mime` over 255 chars or `description` over 8 KiB;
- a `structural_blocks` row with an unknown `kind`, negative `ordinal`, or `body`
  over the FLAC 24-bit block limit.

**The NUL blind spot.** SQLite permits an embedded U+0000 in a TEXT value and
`length()` counts characters only up to the first one, so every `CHECK` above
that caps a TEXT column in *characters* — `tags.key`, `track_art.mime`,
`track_art.description` — measures 1 for a value of `"X\0"` followed by a
hundred megabytes ([#693](https://github.com/Sohex/musefs/issues/693)). Blob
columns are unaffected: `length()` on a BLOB counts bytes, which is why
`tags.value` is capped as `length(CAST(value AS BLOB))`.

The readers do not rely on those caps. Every reader that materializes one of
these fields first projects *both* `length(col)` and `length(CAST(col AS
BLOB))`, and rejects the row if either is over — the character cap the schema
states, or the byte ceiling that cap implies, which is four bytes per character
because that is UTF-8's widest scalar value. The byte bound is the one that
matters against a hostile row: `Row::get::<String>` allocates the column's full
byte length, so without it a NUL-prefixed field is an unbounded allocation on
the serve path. Rejection is decided from the two lengths alone, never from the
value, so an over-cap field provably cannot be materialized in order to reject
it.

The ceiling does not narrow what a field may hold: a `tags.key` of 256
four-byte characters sits exactly on both bounds and reads back intact. Nor is
it a ban on NUL — a short NUL-bearing value still reads. Forbidding NUL outright
in the `CHECK`s is a schema change, and rides the 2.0.0 store migration.

`get_art` is the one reader that materializes a whole `art` row, image blob
included, rather than streaming it. It therefore guards both of its unbounded
columns from lengths first — `sha256` as above, and `length(data)` against the
`art.byte_len` cap, which a crafted store can have been written without since
both that cap and `byte_len = length(data)` are `CHECK`s. `art.sha256` is the
identity case the character cap never really guaranteed: `length(sha256) = 64`
is satisfied by 64 hex characters, a NUL, and any amount of suffix. The mime is
guarded the same way where it now lives, by the `track_art` readers.

**One ordinal space per key.** `tags`' primary key is `(track_id, key,
ordinal)`, which does not discriminate on `value_blob`: a track's text rows and
its binary rows are numbered in the *same* space per key. A writer that holds a
key in both classes must not restart at 0 for the binary rows, or the insert
fails with `UNIQUE constraint failed: tags.track_id, tags.key, tags.ordinal`.
The scanner numbers text rows first and continues the same counters for the
binary rows ([#659](https://github.com/Sohex/musefs/issues/659)), so a track's
binary rows for a key begin above however many text values the scan seeded
under it.

The rule this leaves for an external writer: a rewrite of the text rows alone —
which is what `musefs_common.store`'s `replace_tags` / `merge_tags` do, scoping
their `DELETE` to `value_blob IS NULL` so scanner-written payloads survive a
sync — must not grow a key past the lowest ordinal its binary rows already
hold. In practice the two key namespaces barely meet: binary keys are
`APPLICATION` / `CUESHEET` (FLAC), uppercase four-character ID3 frame ids such
as `PRIV`, `GEOB`, `MCDI`, `SYLT`, `UFID` (MP3/WAV), or `----:<mean>:<name>`
(MP4, while the text path keys the same atom on its bare `name`). The primary
key compares byte-exactly under the default `BINARY` collation, so a lowercase
`cuesheet` row can never collide with the FLAC block's `CUESHEET` row, and the
beets plugin — which lowercases every key it emits — cannot produce a colliding
row at all.

Case-folding cuts the other way for the *delete* half, and the difference is
worth holding onto: `merge_tags` clears by `lower(key) = lower(?)`, so that same
lowercase `cuesheet` does remove the scan-seeded `CUESHEET` *text* row
([#407](https://github.com/Sohex/musefs/issues/407) —
Vorbis keys render case-insensitively, and an exact-case delete would leave the
scan row behind as a visible duplicate). The binary row is untouched, being
scoped out by `value_blob IS NULL`, and keeps whatever ordinal it was given.
Nothing breaks — ordinals need not be dense — but a writer reasoning about
these keys should expect the case-insensitive match when clearing text rows and
the byte-exact one when the constraint is checked. Splitting
the two classes into independent ordinal spaces would take a schema migration
(the primary key replaced by two partial unique indexes on `value_blob IS
NULL`); it was judged not worth a store older builds refuse to open, and
[#663](https://github.com/Sohex/musefs/issues/663) records that decision.

**Schema identity.** On open, musefs also validates schema identity: a
`sqlite_master` comparison against a freshly-migrated reference plus `PRAGMA
foreign_key_check`, rejecting anything that is not the canonical latest schema
with a message telling the user to run `musefs scan`. A store whose
`user_version` is *newer* than this binary's latest migration (a future or
third-party tool bumped the schema) is refused up front with a distinct
"store is newer than this binary" error rather than silently treated as
already-migrated — an older binary must not risk misreading a newer contract.

**Migrations announce themselves.** The opposite direction — an open that finds
an *older* store and upgrades it in place — is irreversible (the store stops
opening with the previous musefs build), so it is logged at `warn`, the default
filter level: the store path, the version found and the version reached,
followed by a completion line at `info`. Creating a store from scratch is not a
one-way step for existing data and logs at `info` only, and the common case — a
store already at the latest version — stays silent, since that path runs on
every open and every mount.

**`art` holds bytes; `track_art` describes the embedding.** The MIME type,
dimensions, colour depth and indexed-colour count live on the link, because they
describe one file's picture block rather than the image every file shares. While
`art` owned them, two files holding byte-identical art served whichever one the
scan reached first — including its declared MIME type. An `art` row is now the
content and its identity and nothing else: `id`, `sha256`, `byte_len`, `data`.
A writer that supplies no `mime` on the link produces a picture block declaring
the empty string, which is the writer's to get right.

**Art is immutable once written.** `art` rows are content-addressed by
`sha256`; a trigger rejects any in-place `UPDATE` of an art row's **key or**
content columns (`id`, `data`, `sha256`, `byte_len`)
with `RAISE(ABORT)` — a multi-row `UPDATE art` touching any of them aborts the
whole statement. `id` is in that list because changing it changes no content
column: the guard's `WHEN` was false, so the one write that orphans every link
to the row was the one write it did not stop. To change a track's art, insert a new content-addressed row
and relink it via `track_art` (which bumps `content_version`); do not mutate an
existing row. Deleting an `art` row still referenced by `track_art` (possible
only with `foreign_keys` OFF) bumps every referencing track so the mount serves
a clean `EIO` on the now-orphaned reference instead of stale bytes.

**A digest has to name its bytes.** Nothing in the schema can tie `art.sha256`
to `art.data` — SQLite has no hash a `CHECK` could call — so a writer can file a
row under the digest of an image it does not hold. The writers do not trust
such a row when they meet one: when an image dedups onto an existing row, that
row's bytes are compared with the image's before the id is returned, and a
mismatch is refused rather than linked
([#724](https://github.com/Sohex/musefs/issues/724)). The scanner fails the one
file that would have linked it, as it does for a constraint the store refuses,
and compares each distinct row at most once per scan; the `contrib` helper
`upsert_art` raises instead of returning the id. A fresh insert needs no check,
since it just stored those bytes.

What this does not do is audit the table: a poisoned row nothing ever dedups
onto is never compared, and the readers serve whatever a link points at. Filing
every row under the digest of its own bytes remains the external writer's job.

**Row ownership is immutable too.** A `tags` or `track_art` row may not move
between tracks: `tags_reject_reparent` and `track_art_reject_reparent` abort a
`track_id` change with the same shape of message. Replace by delete-then-insert,
which is what both `contrib` helpers already do. The reason is the same one that
makes `art` immutable — an invalidation trigger that has to *enumerate*
everything needing a bump fails silently by serving stale bytes when it gets
that wrong, while a refusal fails loudly at the write. (The `UPDATE` triggers
bump both the old and the new owner regardless, so the accounting is correct on
its own terms rather than only because the refusal forbids the case.) Naming
`track_id` in a `SET` list without changing its value is not a reparent and is
allowed.

**Text and binary tag rows have independent ordinal spaces.** `tags` has no
primary key; a unique index on `(track_id, key, ordinal, (value_blob IS NULL))`
enforces uniqueness *within* each class. A writer that rewrites one class alone
— as both `contrib` helpers do, scoping their `DELETE` to `value_blob IS NULL`
so scanner-written binary payloads survive a sync — can therefore reuse an
ordinal the other class holds under the same key, which a single shared key
space rejected.

The class is a column of one index rather than the predicate of two partial
ones, because a partial index only serves a query whose `WHERE` implies its
predicate. A reader that wants *both* classes at once — `tags_for_track` in the
`contrib` helpers — implies neither, and against two partial indexes it plans as
a full table scan plus a sort. With `track_id` leading a single index, every
read shape stays on it.

**What musefs defends at serve time.** CHECKs cannot catch a scanner-owned
field mutated to a *well-formed* value that no longer matches the real file
on disk: `backing_size`, `backing_mtime_ns`/`backing_ctime_ns` or
`backing_ino` that drift from the actual file's stat, or audio bounds that fit the stored
`backing_size` but overrun the file once it has shrunk. musefs re-stats the
backing file on every resolve and treats such rows as untrusted input,
degrading to a controlled
`BackingChanged`/layout error, never undefined behavior. The store's
`CHECK` rejects art over `MAX_ART_BYTES` (16 MiB − 64 KiB) at write time;
resolve also re-checks it (`ArtTooLarge`, all formats) to backstop a writer
that disables check enforcement, and the scanner refuses to ingest a file
carrying such art at all (#644).
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
test — see [CONTRIBUTING](../contributing/setup.md)). Its tag/art replace operations
each wrap their `DELETE`+`INSERT` in a SQLite savepoint, so they are
individually atomic and the "caller owns the transaction" guarantee holds even
on an autocommit connection. The [Lidarr integration](../integrations/lidarr.md)
uses the same shared library from a Custom Script workflow. Its Lidarr
destination tree is only a tracking aid, made of symlinks by default; musefs
remains the consumer-facing filesystem.

CI proves this contract end to end in the `contract` job (see
[CONTRIBUTING](../contributing/setup.md)): a Python writer's tags/art, layered on a
scanned track, are synthesized by the Rust serve path and read back by an
independent reader.

External writers prune in one of two ways depending on how they own files.
For in-place writers (e.g. the beets plugin), existence-based pruning — dropping
the row of a removed backing file — is a deliberate act owned by `musefs
revalidate --prune`; the plugin never prunes on its own (it exposes the
revalidate pass via `beet musefs --revalidate`). The `prune_missing` helper in
`musefs_common` implements the same by-existence delete for writers that prefer
to own pruning themselves; like `revalidate --prune` it deletes only on a
confirmed "not found" and keeps any row whose path it merely failed to stat. Link-tree writers (e.g. the Lidarr integration) never
delete the backing files they point at, so they prune by identity instead: a
source-reported album/artist deletion removes the rows carrying the matching
MusicBrainz id.

Connections are mode-typed (`Db<ReadWrite>` / `Db<ReadOnly>`), opened in WAL
mode with a busy timeout. The serve path uses a `DbPool` whose per-thread
variant hands each reader thread its own connection — WAL reads never contend.
