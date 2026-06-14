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
- Dot-prefixed: hidden from plain `ls`, visible to `ls -a`. The synthetic root
  entry shadows any real root entry of the same name (documented; collision is
  absurd in a real library).
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
  `#[cfg(feature = "jemalloc")]`) and injects it into `FuseConfig`. Stays thin.

Data ownership by layer:

| Source | Owner | Values |
| ------ | ----- | ------ |
| core | `Musefs` (`facade.rs`) | handle-table size, header cache, size cache, virtual-tree / inode footprint, refresh health |
| fuse | `MusefsFs` (`lib.rs`) | inflight reads + cap, dir handles + cap, thread-pool active/queued/workers, passthrough state, uptime |
| binary | `main.rs` | jemalloc allocated/resident/active/retained (jemalloc feature) |
| core (feature-gated) | `metrics.rs` | syscall counters (metrics feature) |

## Synthetic-inode mechanics (fuse)

Two reserved inodes at the top of the u64 space: `METRICS_DIR_INO`,
`METRICS_FILE_INO`, with a reserved base (e.g. `1 << 63`) guaranteed disjoint
from the `InodeAllocator`'s monotonic low-range assignments. A `debug_assert` in
the allocator guarantees it never reaches the reserved base. Dispatch checks
`ino >= METRICS_INO_BASE` first and routes to the metrics handler **without
consulting core's tree**.

All touch-points are guarded by the `expose_metrics` boolean:

- `lookup(ROOT, ".musefs-metrics")` → synthetic dir attr;
  `lookup(METRICS_DIR_INO, "metrics")` → synthetic file attr.
- `getattr` for the two reserved inodes → synthetic attrs. The file reports
  **size 0** (proc-style).
- `opendir(ROOT)` → append the synthetic `.musefs-metrics` entry to the snapshot
  listing. `readdir(METRICS_DIR_INO)` → fully synthetic (`.`, `..`, `metrics`).
- `open(METRICS_FILE_INO)` → render the snapshot **once into a buffer**, stash it
  in a small fuse-side handle map (mirrors `dir_handles`), and return the handle
  with **`FOPEN_DIRECT_IO`** so the kernel always calls `read` to EOF despite the
  size-0 stat, and each fd observes one consistent snapshot. `read` slices the
  buffer; `release` drops it.

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
musefs_cache_header_hits_total  musefs_cache_header_misses_total   # iff quick_cache exposes them
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

Two implementation decisions baked in:

1. **`musefs_handles_open` is served by a new `AtomicUsize`**, bumped in
   `open_handle` / `release_handle`. `sharded_slab::Slab` has no O(1) `len()`,
   and an O(n) `unique_iter().count()` per scrape is undesirable. The counter is
   exact and cheap.
2. **Header-cache hit/miss lines are included only if `quick_cache` exposes those
   counters.** Confirm during planning; if not exposed, drop those two lines
   cleanly (entries / bytes / capacity remain).

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
- **e2e (`#[ignore]`, real mount):** mount with `--expose-metrics`, `cat
  .musefs-metrics/metrics`, assert it parses and reflects a known action (open a
  file → `musefs_handles_open` / `musefs_reads_inflight` move); confirm the audio
  invariant is unaffected by the presence of the metrics namespace. Committed
  in-tree as a runnable script invoked by CI.
- **feature matrix:** the syscall-counter and jemalloc sections require building
  with `--features metrics` / `--features jemalloc`. Account for the known
  `metrics`-feature CI gap (local `--workspace` skips it) and the exact-count
  sensitivity of the metrics-feature tests.

## Docs

- **README:** document `--expose-metrics` / `MUSEFS_EXPOSE_METRICS`, the
  `.musefs-metrics/metrics` file, and a sample.
- **ARCHITECTURE.md:** a short "synthetic telemetry namespace" subsection —
  reserved inodes, dynamic per-read generation, and why it deliberately bypasses
  the virtual tree and the segment model.

## Out of scope

- No HTTP/socket/signal surface (the in-mount file is the whole mechanism).
- No always-on syscall counters on default builds (would add hot-path atomics);
  the syscall block remains gated by the existing `metrics` feature.
- The allocator-tuning / `#[global_allocator]` half of #360 is handled
  separately and is not part of this work.
