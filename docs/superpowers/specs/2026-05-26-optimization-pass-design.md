# musefs optimization pass — design

*Date: 2026-05-26*

## Summary

A holistic performance pass over musefs aimed at real-world deployment: many
concurrent readers (e.g. Plex/Jellyfin streaming several files at once) plus a
library scanner walking the tree, over a backing store that may live on SSD, HDD,
or NFS, at a library scale of **50k–500k+ tracks**.

The work is one design, sequenced into independently shippable phases. The
cardinal invariant is unchanged and non-negotiable: **original audio bytes are
never copied or modified, and served audio stays byte-identical.** Every phase
keeps the existing e2e mount tests green.

## Why (current bottlenecks)

Traced through `musefs-fuse` → `musefs_core::Musefs` → `reader::read_at`:

1. **Single-threaded dispatch.** `MusefsFs` owns `Musefs` and every op is
   `&mut self`; `fuser::mount2` serializes all ops on one thread. A single slow
   backing read (HDD seek, NFS round-trip) blocks *everything*, including
   lightweight `lookup`/`getattr`/`readdir`. No reader parallelism.
2. **Backing fd reopened on every `read()`.** `read_at` calls
   `File::open(backing_path)` per FUSE read request (each ≤128 KB). A 40 MB FLAC
   means hundreds of open/close cycles. `open()`/`release()` are not implemented,
   so nothing is cached per handle. Worst case is NFS (a round-trip per `open`).
3. **`stat()` per read.** `HeaderCache::resolve` calls `fs::metadata` to validate
   size+mtime on *every* read, even cache hits — another syscall/round-trip per
   128 KB chunk.
4. **`getattr` may synthesize.** Reporting file size requires the synthesized
   `total_len`, so a manager walking the tree can trigger full metadata synthesis
   per file.
5. **M4A/M4B slurps the whole file into RAM** (`std::fs::read`) on resolve just to
   parse `moov` — a large memory + I/O spike per audiobook.
6. **`poll_refresh` fires `PRAGMA data_version` on every metadata op** — a manager
   walking N files makes N pragma calls; no debounce. The rebuild it can trigger
   is O(library) with an N+1 `get_tags` query loop, runs on the blocking thread,
   reassigns all inodes, and wholesale-clears the caches.
7. **O(n) LRU bookkeeping** — `HeaderCache` uses a `Vec` with `position`+`remove`
   per access.
8. **No negative-lookup caching** — players/managers probe for sidecars
   (`cover.jpg`, `folder.jpg`, `.DS_Store`); each is an uncached `ENOENT`.

Relevant existing facts that shape the design:
- `Db` opens SQLite in **WAL mode with a busy timeout** (`musefs-db/src/lib.rs`),
  so concurrent read connections — and an external writer (beets/scan) — do not
  block each other.
- `rusqlite::Connection` is `Send` but not `Sync`.
- fuser dispatches on a single thread, but its `Reply*` objects are `Send` and may
  be answered from another thread.
- Entry/attr `TTL` is a hard-coded `Duration::from_secs(1)` in
  `musefs-fuse/src/lib.rs`.

## Goals

- A slow backing read never blocks unrelated metadata ops or other reads.
- Sequential streaming approaches: ~1 `open()` per file (not per chunk), 0
  `stat()` per read, kernel readahead engaged.
- A full-library metadata walk does not trigger redundant file I/O or O(library)
  stalls, and stays responsive while reads are in flight.
- External edits (scan / beets / picard) still appear without remounting, with
  caches staying warm across refreshes.
- Bounded peak memory regardless of file size or library size.

## Non-goals

- Writability or inbound tag writes (still out of scope per `docs/ROADMAP.md`).
- New formats or codec coverage.
- Changing the SQLite schema as a *contract* surface (see rejected alternative
  below). In-process derived state is fine; persisted derived columns that
  external writers must maintain are not.

## Phases

### Phase 0 — Benchmark & instrumentation harness (build first)

A reproducible bench so every later phase is validated, not guessed.

- **Syscall / query counters** behind a `metrics` cargo feature: `open()`,
  `stat()`, `pread()` per MB read; DB queries per `getattr`; layout-cache hit
  rate. Lightweight atomic counters, logged per N ops.
- **Bench scenarios** (criterion micro-benches + an `#[ignore]` real-mount
  harness): sequential stream throughput, time-to-first-byte, random-seek reads,
  and a "manager walk" (`getattr` + header read across the whole tree). Each run
  **single-stream** and **concurrent** (M streams + a walker).
- **Latency injection** to emulate HDD seeks / NFS round-trips: a thin backing
  shim that sleeps per syscall, so SSD/HDD/NFS profiles are measurable on one
  machine.

Target metrics to demonstrate by the end of the pass: `open()`/MB → ~1,
`stat()`/read → 0, metadata-ops/sec unaffected by a concurrent slow read, bounded
peak RSS.

### Phase 1 — Concurrency foundation

Move off single-threaded execution using the standard high-performance fuser
pattern: keep dispatch single-threaded but never block on it.

- `MusefsFs` becomes a thin holder of `Arc<Shared>`. Each FUSE method does only
  cheap work on the dispatch thread, then **moves the `Reply*` object plus cloned
  `Arc`s onto a bounded worker pool** for anything that touches disk or DB; the
  worker computes and calls `reply.data(...)` / `reply.error(...)`.
- `Shared` holds:
  - `tree: ArcSwap<VirtualTree>` — lock-free reads, atomic swap on refresh.
  - the caches (see Phase 3), sharded for concurrent access.
  - a **bounded worker pool**. Each worker thread keeps a **thread-local
    read-only `rusqlite::Connection`**. WAL makes these contention-free, and a
    fixed pool bounds the connection count — no pool-management crate required.
    Pool size is configurable; default is oversized relative to CPU count because
    the work is I/O-bound (especially on NFS).
- Op routing:
  - `lookup`, `readdir` — pure in-memory tree snapshot reads; stay inline on the
    dispatch thread.
  - `open`, `read`, `release`, and `getattr` size synthesis — offloaded to the
    pool.

Verify point: confirm fuser 0.14 `Reply*` types are `Send` and can be moved into a
worker (expected true).

### Phase 2 — File-handle lifecycle (`open` / `release`)

Implement `open()`:
- Resolve the layout (via the caches in Phase 3), open the backing fd, validate
  size+mtime **once**.
- Store `Handle { resolved: Arc<ResolvedFile>, file: Arc<File> }` in a concurrent
  handle table; return the generated `fh`.

`read()` looks up the handle and serves from the cached layout + the pre-opened
fd: **no `open()` and no `stat()` per read**. The Ogg lazy page index already
lives on `ResolvedFile` behind a `OnceCell`, so it is built once per handle and
reused.

`release()` removes the handle and drops the fd.

Defensive fallback: a `read` arriving with `fh == 0` (no prior `open`) falls back
to today's resolve-on-read path, so no access pattern breaks.

This is the single biggest NFS/HDD win.

### Phase 3 — Two-tier caching with lazy invalidation

- **Size/attr cache** — tiny entries (`total_len`, `mtime`, `content_version`),
  effectively unbounded (500k × ~32 B ≈ 16 MB). Serves `getattr`/`lookup`,
  separating the metadata-walk path from the streaming path. A miss computes via
  synthesis (parallelized across the pool) and populates both this and the layout
  cache.
- **Layout cache** — the existing byte-bounded LRU of full `ResolvedFile`s,
  reworked with **O(1) LRU** bookkeeping (intrusive linked list / linked-hash-map)
  instead of the O(n) `Vec` scan, and sharded for concurrency.
- **Lazy invalidation** — refresh no longer wholesale-`clear()`s the caches. The
  per-entry `content_version` check in `resolve()` already self-invalidates a
  changed track, so caches stay **warm across refreshes**. Only entries for tracks
  that no longer exist are evicted.

### Phase 4 — Incremental refresh, inode stability, debounce

- **Debounce `poll_refresh`** — read `PRAGMA data_version` at most once per
  configurable interval (default ~1s) and **single-flight** the rebuild, so a
  `lookup` storm makes one pragma call, not N.
- **Build the tree off the dispatch thread** and swap it in via `ArcSwap` when
  ready; readers keep using the prior snapshot until the swap.
- **Batched query** — replace the N+1 `get_tags`-per-track loop in `build_tree`
  with one grouped query (critical at 500k tracks).
- **Stable inodes** — a persistent path→inode allocator reused across rebuilds.
  Unchanged paths keep their inode (active streams survive a refresh); new paths
  get fresh inodes; retired numbers are not recycled immediately, to avoid
  aliasing a stale held handle.

### Phase 5 — Kernel / mount tuning

Implement `Filesystem::init` and use `KernelConfig` to:
- raise **`max_readahead`** — hides NFS/HDD latency during sequential playback;
- enable **`FUSE_CAP_ASYNC_READ`** — the kernel issues concurrent reads for one
  stream, giving real prefetch parallelism in combination with Phase 1;
- enable **parallel dirops** where available.

Make the entry/attr **`TTL` configurable** (currently hard-coded 1s). A longer TTL
reduces `lookup`/`getattr` storms; it also bounds how quickly external edits become
visible, which is the existing freshness trade-off.

Verify point: confirm the exact `KernelConfig` setters/capabilities available in
fuser 0.14.

### Phase 6 — Bounded-memory format fix

M4A/M4B currently `std::fs::read`s the entire file to parse `moov`. Change to read
only what is needed: stat the size and read the `moov` region (locating it without
slurping the `mdat` payload). Removes a multi-hundred-MB spike per audiobook
resolve. Served audio stays byte-identical (the existing mp4 oracle test guards
this).

## Lower-priority / conditional items

- **Kernel-level negative-lookup caching** (`nodeid = 0` entries with a TTL so the
  kernel caches sidecar misses like `cover.jpg`). Included only if cleanly
  expressible in fuser 0.14; otherwise dropped as low value, since the in-process
  tree lookup is already O(1) and the debounced poll removes the per-lookup pragma
  cost.

## Testing & verification

- Each phase ships with before/after bench deltas from Phase 0.
- All existing crate tests and the `#[ignore]` e2e mount tests
  (`musefs-fuse`) must stay green; byte-identical audio round-trips are the
  hard gate.
- Concurrency correctness: a test that issues a slow (latency-injected) read
  concurrently with a metadata walk and asserts the walk is not stalled.
- Refresh correctness: a test that mutates the DB mid-stream and asserts an open
  handle to an unchanged track keeps serving (inode stability), while a changed
  track re-resolves.

## New dependencies (kept minimal)

- `arc-swap` — lock-free tree snapshot swap.
- a concurrent map (`dashmap`) or sharded `parking_lot` mutexes — handle table and
  cache shards.
- a bounded threadpool (`rayon` or `threadpool`, or hand-rolled with thread-local
  DB connections).
- `criterion` (dev-dependency) for Phase 0 micro-benches.

## Rejected alternatives

- **Persisting synthesized size in the DB.** Would make `getattr` a pure DB read
  even on a cold walk, but the synthesized size is a function of format logic that
  external writers (beets/picard) cannot recompute. Those writers bump
  `content_version` via triggers but cannot update a `synth_size` column, so it
  would silently go stale — breaking the "external tools just write the DB"
  contract. Rejected in favor of the in-memory size cache, which recomputes
  lazily on a `content_version` change.
- **Fully multi-threaded fuser dispatch.** fuser's `Filesystem` methods take
  `&mut self`, so true multi-threaded dispatch is not the library's model. The
  offload-replies-to-a-pool pattern achieves the same parallelism without fighting
  the framework.
