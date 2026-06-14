# Runtime telemetry surface (`.musefs-metrics`) — design

Closes #394 (telemetry half of #360). Provides a way to observe handle / queue /
allocator state on a *live* mount, without rebuilding with a cargo feature and
re-mounting (which destroys the live state being investigated).

## Problem

The `metrics` module (`musefs-core/src/metrics.rs`) is gated behind the `metrics`
cargo feature and is read only by tests and benches. Nothing surfaces internal
state on a running daemon — no endpoint, no signal handler. So a "the mount got
slow" / "RSS grew over days" report cannot be diagnosed in place: the relevant
state (file-handle table size, read-dispatch backlog, directory-handle count,
allocator behavior) is invisible without a rebuild that loses the live state.

## Surface & gating

A runtime flag exposes a synthetic, `/proc`-style directory inside the mount
itself — idiomatic for a FUSE daemon and requiring no new socket/signal/port.

- Flag: `--expose-metrics` (env `MUSEFS_EXPOSE_METRICS`), boolean, **default off**,
  parsed with `BoolishValueParser` like the other boolean mount flags.
  Named distinctly from the compile-time `metrics` cargo feature to avoid
  confusion (the feature separately gates the syscall counters; see below).
- When on, a synthetic directory `.musefs-metrics/` appears at the mount root,
  containing exactly one file: `.musefs-metrics/metrics`.
- Format: **Prometheus exposition format** (one multi-line file, like
  `/proc/meminfo`). A human runs `cat .musefs-metrics/metrics` during an
  incident; a scraper ingests the identical bytes via the Prometheus
  textfile-collector convention. `# HELP`/`# TYPE` lines give an operator the
  value *and* its ceiling/context inline (e.g. inflight vs its cap).
- Dot-prefixed: hidden from plain `ls`, visible to `ls -a`. Collision with a real
  root entry named `.musefs-metrics` is absurd in a real library and handled
  asymmetrically by the chosen mechanism (mirroring the Spotlight marker, which
  appends without dedup): `lookup(ROOT, ".musefs-metrics")` short-circuits to the
  synthetic inode (so a real entry is *shadowed* on lookup), but root `readdir`
  appends the synthetic entry after the real children with no dedup, so a real
  same-named entry would appear *twice* in the listing. This is deemed acceptable
  given the collision is not realistic; the spec does not claim a true shadow on
  readdir.
- When off: nothing is injected, and every affected FUSE op short-circuits on a
  single boolean check — zero behavioral change, near-zero cost.

## Architecture (split: render in core, plumb in fuse)

The telemetry values are scattered across layers, and the synthetic file is
*dynamic* (regenerated per read), unlike the rest of the tree (DB-derived,
cached, rebuilt into an `im::HashMap` on refresh). It must **not** go through the
`VirtualTree` rebuild or the `RegionLayout`/segment model — the cardinal
audio-read path stays untouched.

- **`musefs-core::telemetry`** (new module) owns the snapshot types and **all
  Prometheus rendering** (`render_prometheus(...) -> String`). Rendering and most
  of the data live in core, so it is unit-testable there. `Musefs` gains a
  read-only `telemetry() -> CoreTelemetry` accessor.
- **`musefs-fuse`** owns the synthetic-inode dispatch. It gathers
  `CoreTelemetry` + its own counters + an optional allocator probe, then calls
  the core renderer.
- **`musefs` binary** builds the jemalloc probe closure (behind
  `#[cfg(feature = "jemalloc")]`) and passes it in as a **separate constructor
  argument** to the mount entry point / `MusefsFs::new`, **not** through
  `FuseConfig`. `FuseConfig` derives `Debug + Clone` over plain scalars
  (`lib.rs:41`) and is cloned in `parse_mount_config`; a `dyn Fn` field would
  break both derives. The probe is therefore threaded as its own
  `Option<Arc<dyn Fn() -> AllocatorStats + Send + Sync>>` argument, keeping
  `FuseConfig` a pure-data POD. `None` (default / non-jemalloc build) omits the
  allocator block. Stays thin.

Data ownership by layer:

| Source | Owner | Values |
| ------ | ----- | ------ |
| core | `Musefs` (`facade.rs`) | handle-table size, header cache, size cache, virtual-tree / inode footprint, refresh health |
| fuse | `MusefsFs` (`lib.rs`) | inflight reads + cap, dir handles + cap, thread-pool active/queued/workers, passthrough state, uptime |
| binary | `main.rs` | jemalloc allocated/resident/active/retained (jemalloc feature) |
| core (feature-gated) | `metrics.rs` | syscall counters (metrics feature) |

## Synthetic-inode mechanics (fuse)

This mirrors the existing macOS Spotlight marker
(`musefs-fuse/src/platform/spotlight.rs`) almost exactly — same module shape
(`*_lookup` / `is_*` / `*_dir_entry` / `*_attr`), same reserved-inode rationale —
but as an **all-platform** namespace gated by the runtime `expose_metrics` flag
(not `#[cfg(target_os)]`), and as a *directory containing a file* (two inodes)
rather than a single root file. A new `musefs-fuse/src/metrics_dir.rs` (or
`platform/`-sibling) module owns these helpers.

**Reserved inodes.** Following the Spotlight precedent — whose doc comment
explicitly states that a fixed "high" constant like `1 << 63` is *not* safe
because `InodeAllocator::intern` (`tree.rs:61`) is unbounded monotonic with no
ceiling to sit above — the metrics inodes sit at the very top of the u64 space:

```
METRICS_DIR_INO  = u64::MAX - 1
METRICS_FILE_INO = u64::MAX - 2
METRICS_INO_BASE = u64::MAX - 2   // dispatch routes ino >= base to the metrics handler
```

The allocator starts at 2 and only increments, so the top band is unreachable in
practice and needs **no allocator change** (no `debug_assert`, no checked
increment). On macOS the Spotlight `MARKER_INO == u64::MAX` is disjoint from both
metrics inodes; the metrics dispatch and the marker dispatch must be ordered and
non-overlapping (both inject into root readdir via an append; both intercept
`lookup`/`getattr`). The metrics handler runs **without consulting core's tree**.

**Read contract (committed).** The file is rendered **once at `open`** into a
per-fd buffer and addressed by **absolute offset** (not consume-once):

- `getattr`/`lookup` report **size 0** (proc-style). Acknowledged trade-off:
  `wc -c`, `stat`, `ls -l`, and naive `read(fd, buf, st_size)` loops will see size
  0 and may read nothing; `cat` and Prometheus textfile collectors read to EOF
  and work. This is the standard `/proc` behavior; documented in the README.
- `open(METRICS_FILE_INO)` returns the handle with **`FOPEN_DIRECT_IO`** and
  **without** `FOPEN_NONSEEKABLE` (the marker/poll precedent pairs them for
  consume-once; we omit `NONSEEKABLE` to permit absolute-offset / `pread`
  addressing). The render is stashed in a small fuse-side handle map keyed by a
  fresh fh (mirrors `dir_handles`).
- `read(off, size)` returns `buf[min(off, len) .. min(off + size, len)]`; a read
  at or past `len` returns empty `reply.data(&[])` to signal EOF. A re-read at
  offset 0 re-reads the same frozen snapshot. `release` drops the buffer.

**Directory touch-points** (all guarded by the `expose_metrics` boolean):

- `lookup(ROOT, ".musefs-metrics")` → synthetic dir attr (directory, size 0);
  `lookup(METRICS_DIR_INO, "metrics")` → synthetic file attr.
- `getattr` for the two reserved inodes → synthetic attrs.
- `opendir(ROOT)` → append the static `.musefs-metrics` entry to the snapshot
  listing (the entry name is static text; the dynamic content is produced only at
  `open(METRICS_FILE_INO)`, so the snapshot has no staleness concern).
- `opendir(METRICS_DIR_INO)` → served with a **stateless/reserved fh** (mirroring
  the marker's fh-0 trick); it **bypasses `dir_handles` and the `MAX_DIR_HANDLES`
  cap entirely**. `readdir(METRICS_DIR_INO)` is served **inline** (`.`, `..`,
  `metrics`), never via the `dir_handles` snapshot map, so reading the metrics dir
  never competes for the cap.

## Metrics

Prometheus names; gauges are bare, counters get `_total`. Each gets `# HELP` and
`# TYPE`. Feature-gated blocks are simply absent when their feature is not
compiled in.

Always-on operational state:

```
musefs_uptime_seconds
musefs_handles_open
musefs_reads_inflight                 musefs_reads_inflight_max
musefs_dir_handles                    musefs_dir_handles_max
musefs_pool_workers  musefs_pool_active  musefs_pool_queued
musefs_cache_header_entries  musefs_cache_header_bytes  musefs_cache_header_bytes_max
musefs_cache_header_hits_total  musefs_cache_header_misses_total
musefs_cache_size_entries
musefs_tree_nodes  musefs_inode_paths
musefs_refresh_generation  musefs_refresh_gap_fallbacks_total  musefs_refresh_needs_rebuild
musefs_passthrough_disabled  musefs_passthrough_active
```

jemalloc feature only:

```
musefs_alloc_allocated_bytes  musefs_alloc_resident_bytes
musefs_alloc_active_bytes     musefs_alloc_retained_bytes
```

`metrics` feature only (existing syscall counters):

```
musefs_backing_opens_total  musefs_backing_stats_total
musefs_backing_preads_total musefs_backing_pread_bytes_total
musefs_art_chunks_total     musefs_binary_tag_chunks_total
musefs_scan_opens_total     musefs_scan_preads_total  musefs_scan_bytes_total
```

Source mappings (verified against the pinned crate versions):

| Metric | Source | Notes |
| ------ | ------ | ----- |
| `musefs_handles_open` | new `AtomicUsize` (see below) | `sharded_slab::Slab` has no O(1) `len()` |
| `musefs_cache_header_entries` | `quick_cache` `Cache::len()` | |
| `musefs_cache_header_bytes` | `Cache::weight()` | byte-weighted (`CacheBytesWeighter`) |
| `musefs_cache_header_bytes_max` | `Cache::capacity()` | weight budget |
| `musefs_cache_header_hits_total` / `_misses_total` | `Cache::hits()` / `misses()` | **raw-key** semantics — see caveat |
| `musefs_pool_workers` / `_active` / `_queued` | `threadpool` `max_count()` / `active_count()` / `queued_count()` | |
| `musefs_passthrough_disabled` | `PassthroughState` `disabled: AtomicBool` | |
| `musefs_passthrough_active` | locked `map.len()` (see below) | |

Implementation decisions baked in:

1. **`musefs_handles_open` is served by a new always-on `AtomicUsize`** on
   `Musefs`, incremented **only on the successful slab insert** in `open_handle`
   (`facade.rs:1184`, after the fallible resolve/`File::open` `?`-returns, so a
   failed open never leaks the counter) and decremented in `release_handle`
   (`facade.rs:1194`). These are the only slab insert/remove sites. Invariant: the
   counter tracks **core slab occupancy**, not FUSE opens — the marker/passthrough
   fh-0 open paths bypass `open_handle` and must not bump it (they don't call it,
   so they won't).
2. **Header-cache hit/miss are raw-key counters.** `quick_cache` exposes
   `hits()`/`misses()`, but they count raw key presence, **not** musefs's
   content-version-validated hits: `HeaderCache::resolve` (`reader.rs:113-134`)
   does a raw `cache.get`, then re-checks `content_version` and rebuilds even on a
   key-present hit if the version moved. So `musefs_cache_header_hits_total`
   over-counts relative to "served without a rebuild." This is acceptable and
   documented in the `# HELP` line ("raw header-cache key hits; a hit may still
   trigger a content-version rebuild"); we do not invent a musefs-level counter.
3. **`musefs_passthrough_active` is the locked `map.len()`** of the passthrough
   registration map, read on demand (scrapes are rare, so taking the passthrough
   lock per scrape is acceptable; no parallel always-on counter is added). On
   non-Linux builds `PassthroughState` is a unit struct with no map, so
   `musefs_passthrough_active` is absent (or always 0); `musefs_passthrough_disabled`
   is meaningful only on StructureOnly Linux mounts.

## Error handling

Telemetry is strictly best-effort and may never panic the daemon or perturb a
read:

- Probe closures that can fail (jemalloc ctl) return defaults and log at debug.
- The `inodes` mutex read uses the existing poison-recovery (`into_inner`).
- Rendering is infallible string building.

## Testing

- **core unit tests:** `render_prometheus` format correctness (labels, `# HELP`/
  `# TYPE`, value placement); sections omitted when allocator / syscall probes
  are absent; `musefs_handles_open` counter accuracy across open/release cycles.
- **fuse unit tests:** synthetic dispatch — `lookup` / `getattr` / `readdir` /
  `read` of the metrics file produce valid Prometheus text; the namespace is
  absent when the flag is off; reserved inodes are disjoint from the allocator.
- **e2e (`#[ignore]`, real mount):** mount with `--expose-metrics` and assert,
  beyond "it parses": (1) `cat .musefs-metrics/metrics` reads the **whole** file
  despite `st_size == 0` (guards the direct-IO/EOF contract); (2) a `pread` at a
  **non-zero offset** returns the correct slice, not consume-once behavior (guards
  absolute-offset addressing); (3) opening a track moves `musefs_handles_open` /
  `musefs_reads_inflight`; (4) `ls -a` shows `.musefs-metrics`, plain `ls` hides
  it, and a normal track read still md5-matches the backing file with the
  namespace present (guards the cardinal audio invariant). Committed in-tree as a
  runnable script invoked by CI.
- **feature matrix:** the syscall-counter and jemalloc sections require building
  with `--features metrics` / `--features jemalloc`. Account for the known
  `metrics`-feature CI gap (local `--workspace` skips it) and the exact-count
  sensitivity of the metrics-feature tests.

## Docs

- **README:** document `--expose-metrics` / `MUSEFS_EXPOSE_METRICS`, the
  `.musefs-metrics/metrics` file, and a sample. One sentence disambiguating the
  runtime `--expose-metrics` flag (exposes the surface) from the compile-time
  `metrics` cargo feature (adds the syscall counters), and noting the `/proc`-style
  `st_size == 0` behavior (use EOF-aware readers like `cat`, not `read`-by-size).
- **ARCHITECTURE.md:** a short "synthetic telemetry namespace" subsection —
  reserved inodes, dynamic per-read generation, and why it deliberately bypasses
  the virtual tree and the segment model.

## Out of scope

- No HTTP/socket/signal surface (the in-mount file is the whole mechanism).
- No always-on syscall counters on default builds (would add hot-path atomics);
  the syscall block remains gated by the existing `metrics` feature.
- The allocator-tuning / `#[global_allocator]` half of #360 is handled
  separately and is not part of this work.
