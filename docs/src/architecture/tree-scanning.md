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
ctime_ns, ino)` tuple from the **probed file descriptor** using a pre/post
`fstat` sandwich: if the file's metadata changes between the two stats, the
entry is dropped. `ctime` defeats an mtime-forging writer (e.g. `touch -m`),
and `ino` covers the one case the timestamps cannot: a backing filesystem that
stores no sub-second times — FAT32's two-second granularity and no ctime at
all, or ext3/HFS+/some SMB and NFS mounts truncating the nanosecond fields —
where a same-size *replacement* inside the granularity window leaves all three
identical. It does not help against a true in-place rewrite, which is a POSIX
timestamp limit rather than something musefs can fix; it catches the shape
almost every tagger actually produces, writing a temporary file and renaming
over the original.

A stored inode of zero means "not recorded" — every row a store migrated into
v4 carries, until a scan or `musefs scan --revalidate` fills it in — and such a
row is compared on the other three fields alone rather than failing closed on a
field the store has nothing to say about. `revalidate` re-probes exactly those
rows, which is what makes it the repopulation path for an upgraded store. This
comparison is deliberately *not* equality: a stamp with no recorded inode
matches two live files that do not match each other, so it is a named,
asymmetric check rather than an `==` that would not be transitive. The
`HeaderCache` (`reader.rs`) — a byte-budgeted concurrent cache (64 MiB
default) of resolved layouts — keys each entry on it *and* on the
**backing-source identity** the entry was built from: the row's
`backing_path` and its stamp. Both axes are load-bearing. `content_version`
answers a question about content, so a scan that retargets a row to a moved
file — same bytes, new locator — deliberately leaves it alone, while the
cached entry still carries the pre-move path that `open_handle` opens and the
pre-move stamp that every serve validates against. A hit that mismatches
either axis rebuilds. The `getattr` size cache compares `content_version` and
the stamp for the same reason; it needs no path comparison of its own, because
it re-stats the live path and holds no locator anything opens
([#679](https://github.com/Sohex/musefs/issues/679)).
Independently of the cache, **every**
resolve re-stats the backing file and errors with `BackingChanged` if its
size, mtime, ctime, or inode drifted from the scanned values, so a silently replaced
backing file is never spliced at stale offsets. The per-handle read path
re-stats the held descriptor on every read too, so this guarantee holds on the
hot path and not only through `resolve()`.

**`--trust-backing-mtime`** opts out of the `getattr` half of that, and of
nothing else ([#668](https://github.com/Sohex/musefs/issues/668)). On a
size-cache hit the mount then serves the cached size and mtime without the
re-stat, because on NFS, SMB, or a spun-down array that stat is a network round
trip or a head seek rather than a microsecond — one per track per traversal, on
every traversal after the first. Resolve, `open`, and the per-handle read path
keep validating unconditionally, so a silently replaced backing is still caught
before a single byte is served, and the cold traversal that populates the cache
stats regardless. What the flag trades away is the freshness of the one
metadata surface that can outrun a backing change: between such a change and
the next `open`, a `stat` reports the pre-change size and mtime. Off by
default. `musefs_trust_backing_mtime` in `.musefs-metrics` reports the flag
state, which is what tells a quiet `musefs_backing_stats_total` from a disabled
counter.

**`data_version`** (`PRAGMA data_version`, whole-DB) answers *"did anyone
commit anything?"*. `Musefs::poll_refresh` compares it to the last seen
value; on a change it consults the `track_changes` ring and applies an
**incremental, O(changed)** rebuild: only the affected tracks' tree entries
are re-rendered, exactly the removed tracks' cache entries are dropped, and
the inodes whose `content_version` rose are reported to the FUSE layer. Any
poll whose changelog names a track advances the refresh generation — not only
one that changed a render key — because an open handle caches its resolved
layout, backing path and stamp included, until that generation moves. If
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
(`disambiguate`). A node therefore carries two names: the `rendered_name` it
came from and the possibly-ranked `name` it is served under. Which directory a
path component belongs to is decided by the *rendered* name
(`dir_child_named`), so every track under one rendered directory lands in one
directory even when that directory was ranked away from its base name, and a
directory whose rendered name is literally someone else's rank stays a separate
directory.

The names the FUSE layer injects at the mount root — `.musefs-metrics` and
`.metadata_never_index`, listed in `RESERVED_ROOT_NAMES` — are reserved in that
namespace. The synthetic entry holds the base key the way a sibling node would
(`taken`), so a rendered root component landing on one is ranked to ` (2)` by
the ordinary collision path and `readdir` can never emit a root name `lookup`
will not return (#681). The reservation is unconditional — independent of
`--expose-metrics` and of the host OS — so a library mounts to the same paths
and inodes however the flag is set and wherever it runs.

`mapping.rs` bridges DB tag rows to the format layer's inputs and to template
fields — ordering and multi-value semantics live there.

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
per-item sizes (`MAX_ART_BYTES`, `MAX_BINARY_TAG_BYTES`, and the store's
`tags.key`/`tags.value`/`art.mime`/`track_art.description` limits) so a crafted
file cannot balloon the store.

`check_storable` applies every one of those caps in a single place, before
anything is written. **A file that exceeds any of them fails — that file, not
the scan.** It is logged with its path, what was too big, its size, and the
limit (`RUST_LOG=warn`), and counted `failed`; the rest of the directory scans
normally. Nothing partial is stored for it, so the mount never contains a track
that is quietly missing a tag or its cover art. FLAC gets one extra check: tags
merged from a leading ID3v2 tag (#602) can push the total past what a
`VORBIS_COMMENT` block can hold, which would scan clean and then serve `EIO` on
every read, so that total is checked at scan time too. A supported-extension
file that fails to parse, or errors mid-probe, is likewise logged with the
reason and counted `failed`.

`check_storable` covers the caps the scanner knows to look for, and no set of
pre-checks can cover the rest: `CHECK`, `UNIQUE` and primary-key constraints are
enforced by SQLite inside the ingest transaction, which is where #659 was
discovered and where the next unanticipated shape will be. So the outcome is
classified at the ingest boundary instead of enumerated ahead of time. A
constraint violation (`SQLITE_CONSTRAINT`, any extended code) is attributable to
the rows one file wrote: it fails that file, which is named in the log with the
constraint text and counted in the `rejected` bucket of `failed`. Anything else
— a corrupt, full, read-only or I/O-failing store, and any code this build does
not recognise — still aborts the run, because carrying on would produce one
identical failure per remaining file. `SQLITE_BUSY` is neither, and belongs to
the writer's locking policy.

The production path commits through `BulkWriter`, whose transaction holds a
whole batch, so failing one file means undoing only its rows. Each file is
ingested inside a `SAVEPOINT` (`BulkWriter::item`) for exactly that: a
statement-level `ABORT` rolls back only the statement that hit the constraint,
which would otherwise leave the batch committing a half-ingested track — a
`tracks` row whose tags or art never landed. The savepoint rolls the file back
whole, and the rest of the batch stays committable (#662).

Before v1.4 an over-cap picture or binary tag was instead dropped with a warning
and the rest of the track stored. That left users with a mount quietly missing
data behind a `warn` that is easy to lose in a scan of ten thousand files, and
it did not cover text tags at all — an over-cap `tags.value` reached the DB
`CHECK` and aborted the entire scan with an error naming neither the file nor
the limit (#644).

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

`revalidate` is the maintenance pass: it re-probes only files whose
`(size, mtime_ns, ctime_ns, ino)` freshness stamp changed — a ctime-only move
(e.g. a forged-mtime in-place rewrite) is still re-probed — and it preserves any
external tag edits in the DB by refreshing only Layer A. It also re-probes rows
the stamp *cannot* fully decide: a FLAC missing its structural blocks, a row
below the requested checksum tier, and a row with no recorded inode. That last
one is what makes it the repopulation path for a store upgraded to v4, where
every row starts without one. New files are
ignored: `revalidate` only touches rows that already exist in the store.
Deletion is opt-in via `--prune`, which removes tracks under the scanned root
whose backing file is gone and garbage-collects now-unreferenced art. Pruning
is scoped to the scanned root, so revalidating one library root never removes
tracks belonging to another. Because a track is keyed by its *canonical*
backing path, a file scanned via `--follow-symlinks` whose real target lives
outside the scanned root falls outside the prune scope: if that target later
disappears, its stale row is not pruned by revalidating this root.

## The contrib ecosystem

External writers live under `contrib/`: `python-musefs` is the shared
store-contract library (schema-version check, tag/art writes, sha256 art
content-addressing, the `musefs scan` shell-out); the
[beets plugin](../integrations/beets.md), the
[Picard plugin](../integrations/picard.md), and the
[Lidarr integration](../integrations/lidarr.md) (a Custom Script workflow)
build host-specific tag mapping on top of it. Each one's README covers its own
setup and behavior;
[CONTRIBUTING](../contributing/setup.md) covers their test suites and the
generated-schema/vendoring mechanics.
