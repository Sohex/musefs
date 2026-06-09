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
  are never buffered whole.
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

## The SQLite store

`musefs-db/src/schema.rs` defines the schema as an append-only list of
migrations (`MIGRATIONS`); `user_version` records how many have been applied.
The store is the **interface external tools write to** — the beets and Picard
plugins under `contrib/` write tags and art here out-of-band.

- **V1** — the core tables: `tracks` (one row per backing file: path, format,
  audio byte range, size/mtime stamps, `content_version`), `tags` (multi-value
  key/value rows ordered by `ordinal`), `art` (content-addressed by sha256,
  deduplicated image blobs), and `track_art` (per-track art links with
  picture type and ordering). Deleting a track cascades to its `tags` and
  `track_art` rows. Triggers bump the owning track's `content_version` and
  `updated_at` on any `tags`/`track_art` insert/update/delete.
- **V2** — binary tags and structural blocks: `tags.value_blob` (a row is
  binary iff `value_blob IS NOT NULL`) and the `structural_blocks` table —
  read-only, derived-from-file metadata (FLAC `STREAMINFO`/`SEEKTABLE`) that
  is **not** part of the editable contract.
- **V3** — the `track_changes` changelog: a bounded, self-pruning ring
  (capacity 8192, `CHANGELOG_CAP`) fed by triggers on `tracks`. Every
  metadata edit funnels through an `UPDATE` on the tracks row (the V1
  triggers), so triggers on `tracks` alone capture all writers — this relies
  on SQLite's nested trigger activation (on by default). Writers maintain the
  ring via the pruning trigger; the mount's read-only connections never need
  to.

### The external-writer contract

External tools get full read/write on `tags`, `art`, and `track_art`. The
scanner owns the structural columns of `tracks` (`id`, `backing_path`,
`format`, `audio_offset`, `audio_length`, `backing_size`, `backing_mtime`,
`content_version`, `updated_at`) and all of `structural_blocks`: those are
derived from probing the file, and external tools must run `musefs scan`
rather than compute them. Nothing in SQLite *prevents* an external writer
from mutating scanner-owned fields — musefs treats such rows as untrusted
input and degrades to a controlled `BackingChanged`/layout error when a row
no longer matches its backing file, never undefined behavior.

The shared Python library (`contrib/python-musefs/`) encodes this contract
for plugin authors, including a generated copy of the schema
(`musefs_common/schema.py`, regenerated from `schema.rs` by a drift-guarded
test — see [CONTRIBUTING](CONTRIBUTING.md)). The [Lidarr integration](contrib/lidarr/README.md)
uses the same shared library from a Custom Script workflow. Its Lidarr
destination tree is only a tracking aid, made of symlinks by default; musefs
remains the consumer-facing filesystem.

External tools can also offload path layout entirely: a plugin evaluates its own
(arbitrarily complex) path logic, writes the resulting relative path into a
custom text tag — e.g. `INSERT INTO tags (track_id, key, value, ordinal) VALUES
(?, 'beets_path', 'Pink Floyd/Animals/01 Pigs', 0)` — and the user mounts with
`--template '$!{beets_path}'`. Because the field map is just the (lowercased) tag
keys, any number of such tags (`beets_path`, `lidarr_path`, …) can back
different concurrent mounts. The path field keeps embedded `/` as directory
separators but sanitizes each segment and drops empty/`.`/`..` segments, so a
misbehaving writer cannot inject traversal or empty components into the tree.

Connections are mode-typed (`Db<ReadWrite>` / `Db<ReadOnly>`), opened in WAL
mode with a busy timeout. The serve path uses a `DbPool` whose per-thread
variant hands each reader thread its own connection — WAL reads never contend.

## Freshness: two version counters

Two distinct counters drive correctness; they answer different questions.

**`content_version`** (per-track column) answers *"did this track's served
bytes change?"*. The DB triggers increment it on any tag/art edit. The
`HeaderCache` (`reader.rs`) — a byte-budgeted concurrent cache (64 MiB
default) of resolved layouts — keys each entry on it: a hit with a stale
`content_version` rebuilds the layout. Independently of the cache, **every**
resolve re-stats the backing file and errors with `BackingChanged` if its
size or mtime drifted from the scanned values, so a silently replaced backing
file is never spliced at stale offsets. The per-handle read path re-stats the
held descriptor on every read too, so this guarantee holds on the hot path and
not only through `resolve()`.

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
their literals when every field they reference is empty. Plain values are
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
different file. A path that vanished degrades to `ENOENT`, bounded by the
entry/attr TTL. (Retired paths are pruned once they outnumber live ones,
bounding the allocator at twice the live tree; a path that returns after a
prune gets a fresh inode.)

## Scanning

`scan_directory` (`musefs-core/src/scan.rs`) ingests a backing directory:
collect supported audio files, probe each (format detection → audio
offset/length, tags, pictures, structural blocks) on a parallel probe
pipeline feeding a single DB writer, committing in batches. Probing reads
are bounded — the scanner never slurps whole files — and ingestion caps
per-item sizes (`MAX_ART_BYTES`, `MAX_BINARY_TAG_BYTES`) so a crafted file
cannot balloon the store.

Symlinks are **not followed by default**: a symlinked file or directory is
logged (`RUST_LOG=info`/`warn`) and skipped, which keeps the walk immune to
directory-symlink cycles. Passing `--follow-symlinks` resolves them — symlinked
audio files and directories are scanned — guarded by a visited `(dev, ino)` set
so symlink cycles terminate. Broken symlinks are logged and skipped without
aborting the scan. The `root` argument is always followed regardless of the
flag; only links encountered during recursion are gated.

`revalidate` is the maintenance pass (`scan --revalidate`): re-probe only
files whose size/mtime changed (skipping unchanged files **preserves
external tag edits** in the DB), delete tracks under the scanned root whose
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
[beets plugin](contrib/beets/README.md) and the
[Picard plugin](contrib/picard/README.md) build host-specific tag mapping on
top of it. Each plugin's README covers its own setup and behavior;
[CONTRIBUTING](CONTRIBUTING.md) covers their test suites and the
generated-schema/vendoring mechanics.
