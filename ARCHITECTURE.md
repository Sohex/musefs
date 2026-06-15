# Architecture

This is the technical reference for musefs internals: how a virtual file is
assembled, how the workspace is layered, what the SQLite store guarantees, and
how external edits become visible without a remount. For usage, see the
[README](README.md); for the development workflow, see
[CONTRIBUTING](CONTRIBUTING.md); for per-format behavior, see the format docs
under [`docs/`](docs/).

## Design overview

musefs is a read-only passthrough FUSE filesystem with one cardinal invariant:
**original audio bytes are never copied or modified.** A served file is not a
transcoded or rewritten copy — it is assembled on the fly by splicing a
freshly generated metadata region in front of positioned reads of the
untouched backing file. The SQLite store is the source of truth for tags, art,
and each file's audio byte range; the backing directory is the source of truth
for the audio itself.

## Crate layout

A strict layered Cargo workspace; dependencies point one way only:

```
musefs-db   ─┐                 SQLite store: schema/migrations, tracks/tags/art access
musefs-format┘← (db)           format byte-surgery: metadata synthesis + RegionLayout
        ↑
musefs-core ← (db, format)     orchestration: virtual tree, resolution, scanning, refresh
        ↑
musefs-fuse ← (core)           thin FUSE adapter (fuser)
        ↑
musefs-cli  ← (core, fuse, db) clap commands library (scan/mount logic)
musefs      ← (cli)            thin binary entrypoint; published as `musefs`
```

`musefs-core` is the integration layer — cross-cutting logic belongs there.
`musefs-fuse`, `musefs-cli`, and the `musefs` binary crate are deliberately
thin; the FUSE adapter's job is translating kernel requests into core calls
(and dispatching blocking reads onto a worker pool with per-thread reusable
buffers, so a slow backing read never stalls the FUSE dispatch thread).

The workspace also carries `musefs-latencyfs`, a dev/bench-only crate
(`publish = false`): a latency-injecting passthrough FUSE filesystem used by
the [BENCHMARKS.md](BENCHMARKS.md) harness to simulate slow backing stores. It
is not part of the shipping dependency graph (core uses it only as a
dev-dependency).

## The segment model

A synthesized virtual file is described by a `RegionLayout`
(`musefs-format/src/layout.rs`): an ordered list of `Segment`s whose lengths
sum to the served file size. Six variants:

- `Inline(Vec<u8>)` — generated framing/text bytes (an ID3v2 tag, FLAC
  metadata blocks, a RIFF front), fully materialized at resolve time.
- `ArtImage { art_id, len }` — embedded cover art; only the length lives in
  the layout. Image bytes stream from the DB blob in chunks at read time and
  are never buffered whole. This invariant also holds for Ogg synthesis,
  where page CRCs are computed from page-bounded `ArtSource` windows
  (previously the documented exception).
- `BackingAudio { offset, len }` — a run of the original file's audio frames,
  served by positioned reads (`read_exact_at`) against the backing file.
- `OggAudio { offset, len, seq_delta }` — original Ogg audio pages served
  with each page's sequence number shifted by `seq_delta` and its CRC
  recomputed in place (a resized header changes the page count). The byte
  length is unchanged — renumbering patches, never recopies.
- `OggArtSlice { art_id, offset, len, base64, art_total }` — a window of an
  embedded picture served lazily from the blob store; when `base64`, the
  window is base64-encoded incrementally at read time.
- `BinaryTag { payload_id, len }` — an opaque binary tag payload (e.g. an ID3
  `PRIV` frame body or a FLAC `APPLICATION` block body) streamed from the DB
  at read time.

`read_at` (`musefs-core/src/reader.rs`) serves a byte range by walking the
segments and splicing: inline bytes are copied, art and binary-tag payloads
are read from the DB in chunks, backing audio comes from positioned reads of
the original file, and Ogg pages are renumbered and CRC-patched in flight.
This is how the cardinal invariant holds end to end. Layouts that stream
binary tags are flagged (`RegionLayout::has_binary_tag`) so the reader can
wrap those reads in a transactional `content_version` guard — a concurrent
retag cannot interleave bytes from two generations of a tag.

### Backing read-ahead

Every backing read — `BackingAudio` splices and the `serve_ogg_window` page walk
alike — flows through a single `BackingReader::read_exact_at`
(`musefs-core/src/readahead.rs`). It caches *raw backing-file bytes keyed by
absolute backing offset* in a per-handle adaptive window: a sequential miss reads
one large `pread` (geometric growth up to a per-stream cap) instead of the
≤256 KiB FUSE chunk, so a high-latency backing client (NFS, remote) can pipeline
the RPCs behind one syscall; a seek resets the window to the floor. All handles
draw from one process-wide RAM budget (`--read-ahead-budget-mib`, default 64) with
deadlock-free `try_lock` LRU eviction. Keying on the absolute backing offset (not
the synthesized output) makes the cache retag-immune, and serving still flows
through the per-read `validate_opened_backing` re-stat, so the cardinal
audio-bytes invariant and freshness semantics are untouched. An optional Phase-2
background-prefetch layer (`--read-ahead-prefetch`) exists but is off by default —
read amplification carries the whole win (see
[BENCHMARKS.md](BENCHMARKS.md#backing-read-ahead-255)).

How each format builds its layout differs enough to warrant its own document:
[FLAC](docs/FLAC.md), [MP3](docs/MP3.md), [M4A](docs/M4A.md),
[Ogg](docs/OGG.md), [WAV](docs/WAV.md).

## Mount modes

`musefs_core::Mode` selects one of two behaviors at mount time:

- **`Synthesis`** (default) — the metadata region is generated from the DB
  and spliced ahead of the backing audio, as above. Resolve-time validation
  guards the stored audio bounds: if `audio_offset + audio_length` runs past
  the backing file's current length, the row no longer matches the file and
  the resolve fails with a controlled `BackingChanged` error.
- **`StructureOnly`** — pure passthrough: the layout is a single whole-file
  `BackingAudio` segment, so the original bytes are served verbatim under the
  templated tree. Stored audio bounds are irrelevant (the whole file is
  served) and are not validated in this mode.

In `StructureOnly` mode, on kernels with FUSE passthrough (6.9+) and a daemon
holding `CAP_SYS_ADMIN` (kernel-gated: run as root or
`setcap cap_sys_admin=ep` the binary), each open registers the backing fd
with the kernel and reads bypass the daemon entirely. The capability check is
performed at mount time and its absence pre-announced; if registration fails
at runtime anyway, passthrough is disabled for the rest of the session
(later opens skip the doomed ioctl) and reads fall back to the daemon
silently. Freshness for a passthrough handle is open-time-only — it is a
plain POSIX fd onto the backing file. In `Synthesis` mode no single fd
represents the spliced bytes, so passthrough never applies.

## Synthetic telemetry namespace

When `--expose-metrics` is on, the root directory gains a synthetic
`.musefs-metrics/` entry backed by reserved inodes at `u64::MAX - 1` (dir) and
`u64::MAX - 2` (file) — the same "top of the u64 space" trick the Spotlight
marker uses, since `InodeAllocator` starts at 2 and only increments. The
directory and file are disjoint from the macOS Spotlight marker at `u64::MAX`.

The metrics file is `/proc`-style: it advertises `st_size == 0` and is served
via `FOPEN_DIRECT_IO`, so readers must read to EOF rather than trusting the
stated size. Content is rendered at `open` time from a snapshot of
`CoreTelemetry` (header/size caches, read-ahead budget/charge, virtual-tree
footprint, refresh health), `FuseTelemetry` (uptime, read/dir-handle gates,
worker pool, passthrough state), and optional jemalloc/syscall counters
(including read-ahead hit/miss) — see
[`musefs-core/src/telemetry.rs`](musefs-core/src/telemetry.rs) for the full
metric list. This namespace deliberately bypasses the virtual tree
(`VirtualTree`) and the `RegionLayout` / segment model: it is injected into
root-directory `readdir` and resolved by direct inode checks, so the cardinal
audio path is untouched.

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
test — see [CONTRIBUTING](CONTRIBUTING.md)). Its tag/art replace operations
each wrap their `DELETE`+`INSERT` in a SQLite savepoint, so they are
individually atomic and the "caller owns the transaction" guarantee holds even
on an autocommit connection. The [Lidarr integration](contrib/lidarr/README.md)
uses the same shared library from a Custom Script workflow. Its Lidarr
destination tree is only a tracking aid, made of symlinks by default; musefs
remains the consumer-facing filesystem.

CI proves this contract end to end in the `contract` job (see
[CONTRIBUTING](CONTRIBUTING.md)): a Python writer's tags/art, layered on a
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

## Freshness: two version counters

Two distinct counters drive correctness; they answer different questions.

**`content_version`** (per-track column) answers *"did this track's served
bytes change?"*. The DB triggers increment it on any input the database can see that changes
synthesized bytes: tag and `track_art` edits, `art`-row deletes that orphan a
reference, scanner-owned geometry changes (`format`, audio bounds, backing
size/nanosecond-mtime), and FLAC structural-block changes. It is
therefore a superset key — the one input it cannot cover is an on-disk backing
change with no DB write, which `resolve` (and, since #279, a size-cache
`getattr` hit) catches by re-statting the backing file and degrading to
`BackingChanged`. The scanner stamps the backing file's `(size, mtime_ns,
ctime_ns)` tuple from the **probed file descriptor** using a pre/post `fstat`
sandwich: if the file's metadata changes between the two stats, the entry is
dropped. `ctime` defeats an mtime-forging writer (e.g. `touch -m`). The
`HeaderCache` (`reader.rs`) — a byte-budgeted concurrent cache (64 MiB
default) of resolved layouts — keys each entry on it: a hit with a stale
`content_version` rebuilds the layout. Independently of the cache, **every**
resolve re-stats the backing file and errors with `BackingChanged` if its
size, mtime, or ctime drifted from the scanned values, so a silently replaced
backing file is never spliced at stale offsets. The per-handle read path
re-stats the held descriptor on every read too, so this guarantee holds on the
hot path and not only through `resolve()`.

**`data_version`** (`PRAGMA data_version`, whole-DB) answers *"did anyone
commit anything?"*. `Musefs::poll_refresh` compares it to the last seen
value; on a change it consults the `track_changes` ring and applies an
**incremental, O(changed)** rebuild: only the affected tracks' tree entries
are re-rendered, exactly the removed tracks' cache entries are dropped, and
the inodes whose `content_version` rose are reported to the FUSE layer. If
the mount slept past the ring's capacity (or the ring was truncated), it
falls back to a full tree rebuild — correct by construction, and a bulk
change wants one anyway. The new version stamp is committed **only after** a
successful rebuild; failures arm a retry backoff.

The FUSE layer fires `poll_refresh` on metadata ops (`lookup`, `readdir`,
…) off the dispatch thread, so external edits appear **without remounting**.
Polling is debounced (`--poll-interval-ms`) and rebuilds are single-flighted:
a metadata-op storm costs at most one rebuild per interval. When mounted with
`--keep-cache`, the changed-inode notifications drive kernel page-cache
invalidation (`inval_inode`), so a re-tagged file never serves stale cached
bytes.

## Virtual tree

`VirtualTree::build` (`musefs-core/src/tree.rs`) materializes an inode → node
mapping from rendered paths. Paths come from beets-style templates
(`template.rs`): `$field` / `${field}` substitutions (with `${a|b}` fallback
chains) over the track's tag fields, each resolving through per-field fallbacks
and then a global `default_fallback`; `[...]` conditional sections suppress
their literals when every field they reference is empty. With `skip_on_missing`
set (CLI `--skip-on-missing`), an unresolved *top-level* field instead drops the
track from the mount: `render_one` returns `None`, so the track enters neither
the snapshot nor the tree, and the incremental refresh path reclassifies a track
that loses (or regains) such a field as a removal (or addition). Plain values are
sanitized to a single path component ('/' and control characters become '_',
components equal to `.` or `..` are dropped, and any component is truncated to
255 bytes on a UTF-8 boundary so it stays within NAME_MAX),
while a `$!{field}` path field keeps '/' as directory separators (sanitizing
each segment and dropping empty/`.`/`..` segments) so a precomputed multi-level
path expands into real directories. Path collisions are resolved
deterministically by appending ` (k)` before the extension
(`disambiguate`). `mapping.rs` bridges DB tag rows to the format layer's
inputs and to template fields — ordering and multi-value semantics live
there.

Inodes are **stable across rebuilds**: a persistent path→inode allocator
(`InodeAllocator`) reuses an unchanged rendered path's inode and never
recycles a retired one, so a descriptor held open across a refresh keeps
resolving to the same node and a stale FUSE handle can never alias a
different file. On case-insensitive mounts the key is case-folded, so a
survivor keeps its inode even when an unrelated deletion flips a merged
directory's display casing (#305). A path that vanished degrades to
`ENOENT`, bounded by the entry/attr TTL. (Retired paths are pruned once they outnumber live ones,
bounding the allocator at twice the live tree; a path that returns after a
prune gets a fresh inode.)

## Scanning

`scan_directory` (`musefs-core/src/scan.rs`) ingests a backing directory:
collect supported audio files, probe each (format detection → audio
offset/length, tags, pictures, structural blocks) on a parallel probe
pipeline feeding a single DB writer, committing in batches. Probing reads
are bounded — the scanner never slurps whole files — and ingestion caps
per-item sizes (`MAX_ART_BYTES`, `MAX_BINARY_TAG_BYTES`) so a crafted file
cannot balloon the store. An over-cap picture or binary tag is dropped and
logged (`RUST_LOG=warn`) rather than vanishing silently, so a track that
appears to have lost its cover art has an explanation in the logs; a
supported-extension file that fails to parse, or errors mid-probe, is
likewise logged with the reason and counted `failed`.

Symlinks are **not followed by default**: a symlinked file or directory is
logged (`RUST_LOG=info`/`warn`) and skipped, which keeps the walk immune to
directory-symlink cycles. Passing `--follow-symlinks` resolves them — symlinked
audio files and directories are scanned — guarded by a visited `(dev, ino)` set
so symlink cycles terminate, and by a second file-level `(dev, ino)` set so a
file reached via both a real path and a symlink is ingested once rather than
upserting its canonical track row twice. Because that set keys on `(dev, ino)`,
multiple hardlinks to the same inode are likewise collapsed to a single track
under `--follow-symlinks`. Broken symlinks are logged and skipped without
aborting the scan. The `root` argument is always followed regardless of the
flag; only links encountered during recursion are gated.

`revalidate` is the maintenance pass (`scan --revalidate`): re-probe only
files whose `(size, mtime_ns, ctime_ns)` freshness stamp changed — a
ctime-only move (e.g. a forged-mtime in-place rewrite) is still re-probed
(skipping unchanged files **preserves external tag edits** in the DB),
delete tracks under the scanned root whose
backing file is gone, and garbage-collect now-unreferenced art. Pruning is
scoped to the scanned root, so revalidating one library root never removes
tracks belonging to another. Because a track is keyed by its *canonical*
backing path, a file scanned via `--follow-symlinks` whose real target lives
outside the scanned root falls outside the prune scope: if that target later
disappears, its stale row is not pruned by revalidating this root.

## The contrib ecosystem

External writers live under `contrib/`: `python-musefs` is the shared
store-contract library (schema-version check, tag/art writes, sha256 art
content-addressing, the `musefs scan` shell-out); the
[beets plugin](contrib/beets/README.md), the
[Picard plugin](contrib/picard/README.md), and the
[Lidarr integration](contrib/lidarr/README.md) (a Custom Script workflow)
build host-specific tag mapping on top of it. Each one's README covers its own
setup and behavior;
[CONTRIBUTING](CONTRIBUTING.md) covers their test suites and the
generated-schema/vendoring mechanics.
