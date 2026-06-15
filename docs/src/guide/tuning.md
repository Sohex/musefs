# Tuning & metrics

### Tuning

The defaults are sensible for most setups, including the two measured storage wins —
daemon-level backing read-ahead (`--read-ahead-budget-mib`, the single biggest win for
NFS/remote) and keeping the kernel page cache across opens (`--keep-cache`, on by default,
~3× faster reopen on HDD/NFS). The *kernel*-level read-ahead / background knobs have little
measurable effect (see [BENCHMARKS.md](../benchmarks.md#storage-tunables) for the methodology
and numbers).

| Flag | Default | What it does |
| ---- | ------- | ------------ |
| `--poll-interval-ms` | `1000` | Debounce window for detecting external DB edits. |
| `--read-ahead-budget-mib` | `64` | Per-mount RAM budget (MiB) for **backing read-ahead**: the daemon coalesces a stream's small FUSE reads into one large positioned read, so the backing client can pipeline/parallelize them. **The biggest lever for slow/high-latency backing** — ~5–6× single-stream throughput over a 200 ms-RTT NFS mount; neutral on local disk. Shared across all active streams with LRU eviction; `0` disables it. |
| `--read-ahead-prefetch` | disabled | Advanced: add background prefetch threads on top of read amplification. Off by default — benchmarks found amplification alone delivers the entire read-ahead win, while the threads add ~10% overhead with no measured benefit. Enable only when profiling a backend where a single large read does not self-pipeline. |
| `--keep-cache <true\|false>` | `true` | Keep the kernel page cache across opens. **On by default** — it is the one measured storage win: repeat opens of a file are served from cache instead of re-read over slow storage (~3× faster reopen on HDD/NFS in our benches). External re-tags auto-invalidate the affected files, so cached bytes never go stale. Disable with `--keep-cache false` (e.g. on a memory-constrained host where the page cache is contended). |
| `--attr-ttl-ms` | `1000` | How long the kernel may trust cached entry/attr lookups. Higher cuts `lookup`/`getattr` traffic — useful for metadata-heavy clients (library scanners) over high-latency backing — but bounds how fast external edits become visible. |
| `--max-readahead-kib` | `512` | *Kernel* read-ahead window (clamped to the kernel maximum). Distinct from `--read-ahead-budget-mib` (the daemon-level read-ahead, which is the effective one): this kernel knob does **not** speed up musefs streaming, since reads reach the daemon in fixed FUSE-sized chunks regardless. On HDD, values well above the default can even hurt. Leave at the default unless your own profiling shows otherwise. |
| `--max-background` | `64` | Max outstanding background (read-ahead/async) requests the kernel keeps in flight. Does **not** bound foreground reads (those scale with client concurrency), so it has little effect on read throughput; left for completeness. |
| `--case-insensitive <true\|false>` | OS default | Compare filenames case-insensitively. Case-variant directories merge into one (first-seen casing wins) and case-variant files get a numeric suffix (e.g. `Song (2)`). Defaults to `true` on macOS and `false` on Linux/FreeBSD; case-insensitive mounts refresh via a full rebuild rather than the incremental fast path. |

### Metrics

`musefs mount` optionally exposes runtime telemetry through a synthetic
`.musefs-metrics/` directory at the mount root:

```bash
musefs mount /mnt/music --db library.db --expose-metrics   # or: MUSEFS_EXPOSE_METRICS=1
cat /mnt/music/.musefs-metrics/metrics
```

```text
# HELP musefs_uptime_seconds Seconds since the mount started.
# TYPE musefs_uptime_seconds gauge
musefs_uptime_seconds 60
# HELP musefs_handles_open Open file handles in the core slab.
# TYPE musefs_handles_open gauge
musefs_handles_open 3
# HELP musefs_cache_header_hits_total Raw header-cache key hits; a hit may still trigger a content-version rebuild.
# TYPE musefs_cache_header_hits_total counter
musefs_cache_header_hits_total 100
```

`--expose-metrics` (default off) is a **runtime** flag that gates the virtual
file; it is unrelated to the compile-time `metrics` cargo feature, which adds
syscall counters (opens, preads, etc.) to the output. The jemalloc allocator
stats require a build with the `jemalloc` feature, which is the default.

The `metrics` file advertises `st_size == 0` (like `/proc`), so use an
EOF-aware reader — `cat`, `head -c`, or the Prometheus textfile collector —
not a stat-and-`read`-by-size approach.
