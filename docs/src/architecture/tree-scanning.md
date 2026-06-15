# Freshness, tree & scanning

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
[beets plugin](../../../contrib/beets/README.md), the
[Picard plugin](../../../contrib/picard/README.md), and the
[Lidarr integration](../../../contrib/lidarr/README.md) (a Custom Script workflow)
build host-specific tag mapping on top of it. Each one's README covers its own
setup and behavior;
[CONTRIBUTING](../contributing/setup.md) covers their test suites and the
generated-schema/vendoring mechanics.
