# Optimization pass (2026-05-30) — tracking document

*Started: 2026-05-30*

This is the umbrella tracking doc for a second optimization pass over musefs.
Each sub-project (SP) is independently shippable, has its own spec in this
directory, and records before/after numbers in the results log below.

## Cardinal invariant (non-negotiable, every SP)

**Original audio bytes are never copied or modified, and served audio stays
byte-identical.** Every SP keeps all existing crate tests and the `#[ignore]`d
FUSE e2e mount tests green; the byte-identical audio round-trip is the hard gate.

## Relationship to the 2026-05-26 pass

A prior pass (`docs/superpowers/specs/2026-05-26-optimization-pass-design.md`,
Phases 0–7) already optimized the **serving/read side**. This pass is scoped
around what that one did *not* deliver. Reconciliation:

| Area | Prior pass | Shipped? | Open here |
|---|---|---|---|
| Measurement harness | Phase 0 | `metrics.rs` (atomic counters + in-process per-syscall latency injection via `MUSEFS_FAULT_OPEN_US`/`STAT_US`/`PREAD_US` + `snapshot`/`reset`) + one micro-bench (`read_throughput`). No corpus generator, no tiers, **no FUSE-level injection** (so SQLite/write-durability latency is unmodelled and there is no fsync counter), no real-mount/concurrent benches. | **SP0** completes it |
| Ingestion / scan | *not in scope* (serving-side only) | scan still `fs::read`s whole files; no transaction batching; single-threaded | **SP1** — largest untouched win |
| Refresh | Phase 4 | batched query, debounce, off-thread rebuild, stable inodes — but still a **full** rebuild on any edit | **SP2** — changed-only rebuild |
| Read/serve | Phases 1–3 | worker pool, per-handle fd, sharded *header* cache | **SP3** residuals: `handles` + `size_cache` are single `Mutex<HashMap>`; `read_segments` double-allocates backing audio |
| Storage-aware serving | Phase 5 | `max_readahead` / async-read / parallel-dirops in `init` | **SP4** residual: Ogg whole-region index scan on first read |

## Decomposition

- **SP0 — Measurement foundation.** Complete the Phase-0 harness: synthetic
  library generator (tiered + custom/real-library), scan/refresh/read benches
  (single + concurrent), a deterministic latency-injection layer (passthrough
  FUSE), and comparable reporting. *Spec: `SP0-measurement-foundation.md`.*
- **SP1 — Ingestion scalability.** Bounded probing reads (stop slurping whole
  files), transaction batching across the scan loop, parallel probing
  (default-on, `--jobs` knob), bulk-write pragma tuning. *Spec:
  `SP1-ingestion-scalability.md`.*
- **SP2 — Incremental tree refresh.** Rebuild only changed tracks on a
  `data_version` bump instead of reloading and re-rendering the whole library.
  *Spec: TBD.*
- **SP3 — Read/serve residuals.** Remove the `read_segments` backing-audio
  double-allocation (read directly into `out`'s reserved tail); replace the
  global `handles` mutex with a lock-free `sharded-slab` (the slab key becomes the
  FUSE `fh`, subsuming `next_fh`) and the global `size_cache` mutex with a
  `DashMap`. Deferred (noted, not in scope): art-chunk zero-copy and zero-copy
  into the FUSE reply buffer. *Spec: `SP3-read-serve-residuals.md`.*
- **SP4 — Storage-aware serving residuals.** Mitigate the Ogg first-read
  whole-region index scan on HDD/NFS. *Spec: TBD.*

## Ordering & rationale

SP0 → SP1 → SP2 → SP3 → SP4.

- **SP0 first** — the harness gates every later SP (changes are measured, not
  guessed) and is still mostly unbuilt.
- **SP1 next** — largest genuinely-open win, and a fast scan is what lets us
  build large test corpora for SP2–SP4.
- **SP2 before SP3/SP4** — at 100k tracks a full rebuild storm would swamp the
  finer read-path gains, so fix refresh cost first.
- **SP3/SP4 last** — small, well-scoped residual items.

## Conventions

- Each SP records before/after numbers (from SP0's harness) in the results log.
- Storage-bound claims (SP1, SP4) are validated under both injected latency and
  a real mount; compute-bound claims (SP2, SP3) on tempfs is sufficient.
- CI runs only the `ci` tier on tempfs; everything heavier is opt-in / env-gated.

## Active-environment note

The current dev box is storage-constrained: only the `ci` and `large-compute`
tiers run here. The `bandwidth` tier, real-mount runs, and the `custom`
real-library tier run later on the VPS that hosts the actual music library. SP0
is built for the full capability regardless.

## Status

| SP | State | Spec | Plan | Notes |
|---|---|---|---|---|
| SP0a | Implemented | `SP0-measurement-foundation.md` | `../../plans/2026-05-30-optimization-sp0a-corpus-and-benches.md` | Corpus generator + compute benches + reporting; no /dev/fuse — runs now. See "Running the SP0a harness" below; per-format sweep added (`SP0a-per-format-coverage.md`) |
| SP0b | Implemented | `SP0-measurement-foundation.md` | `../../plans/2026-05-30-optimization-sp0b-latency-fuse.md` | `musefs-latencyfs` passthrough latency-injection FUSE + fsync counter; needs /dev/fuse — VPS. See "Latency-injected runs" below. |
| SP1 | Implemented | `SP1-ingestion-scalability.md` | `../../plans/2026-05-31-sp1-ingestion-scalability.md` | Hybrid bounded reads (window+`NeedMore` for FLAC/MP3/OGG/WAV, seek reader for M4A) · parallel-probe/serial-writer pipeline (`--jobs`) · byte-budget backpressure · txn batching · bulk pragmas · `failed` resilience. Bounded≡full equivalence gate + byte-identical PCM e2e green. Bench baselines in `BENCHMARKS.md` (durable-storage cold scan 20–500× faster). |
| SP2 | Implemented | `SP2-incremental-tree-refresh.md` | `../../plans/2026-05-31-sp2-incremental-tree-refresh.md` | In-memory identity diff (changed/added/removed) → changed-only render (Stage A) → im-backed in-place tree mutation with introducing-id dirty propagation + full-`build_with` fallback (Stage B); equivalence gate (proptest 64 cases + debug-assert) green; refresh-1 still slopes with N at Stage B due to O(N) render-key scan (tree mutation itself is O(changed)); `VirtualTree::build_with` full reconstruction eliminated. |
| SP3 | Spec | `SP3-read-serve-residuals.md` | — | Three residuals: `read_segments` backing-audio reads into `out`'s tail (no temp buf); `handles` → lock-free `sharded-slab` (slab key = FUSE `fh`, drops `next_fh`); `size_cache` → `DashMap`. Validated against SP0's `concurrent_read_walk`; `ci` `sequential_read` >10% gate. Art-chunk zero-copy + FUSE-reply-buffer zero-copy explicitly deferred. |
| SP4 | Not started | — | — | |

## Running the SP0a harness

```bash
# Read throughput + concurrent read/walk (Criterion):
cargo bench -p musefs-core --bench read_throughput

# Cold scan + revalidate timing (prints a table):
cargo test -p musefs-core --features metrics --test bench_ingest -- --ignored --nocapture

# Refresh timing, 1 vs N changed tracks:
cargo test -p musefs-core --test bench_refresh -- --ignored --nocapture

# Scale / storage knobs (any of the timing/bench commands above):
MUSEFS_BENCH_TIER=large-compute \
MUSEFS_BENCH_DIR=/mnt/ssd/musefs-bench \
  cargo test -p musefs-core --features metrics --test bench_ingest -- --ignored --nocapture

# Run against a real library (never written to; DB goes to MUSEFS_BENCH_DB or a tempfile):
MUSEFS_BENCH_LIBRARY=/srv/music \
MUSEFS_BENCH_DB=/tmp/musefs-bench.db \
  cargo test -p musefs-core --features metrics --test bench_ingest -- --ignored --nocapture
```

Notes:
- **Per-format sweep:** `bench_ingest` and the `read_throughput` sequential bench
  run against every supported format (FLAC, MP3, M4A moov-first, M4A moov-last,
  Ogg, WAV) by default, one report row / Criterion line per format (see the
  `format` column). `bench_refresh` stays FLAC-only (it times a format-independent
  DB-driven tree rebuild).
- `MUSEFS_BENCH_FORMAT_MIX` (comma list of `flac,mp3,m4a,m4a-last,ogg,wav`)
  restricts the sweep to those formats; unset = all. In a `bench_ingest` sweep,
  `MUSEFS_BENCH_DB` is ignored (each format gets its own DB under a per-format
  subdir); a real `MUSEFS_BENCH_LIBRARY` run does a single `mixed` scan instead of
  sweeping.
- A reused `MUSEFS_BENCH_DIR` is re-scanned cold: `prepare` deletes any prior
  `musefs-bench.db` (+ `-wal`/`-shm`) so scan timings start from an empty DB.
- The `bench_ingest` `opens`/`preads` columns read ≈0: the metrics counters
  instrument the serve path, not the scan path (per-file scan I/O counting
  lands in SP1). `wall_ms` and `peak_rss_kib` are the SP1 signals here.
- `fsyncs` shows `n/a` for every SP0a run; the SP0b passthrough FS fills it.
- **Regression gate (convention for SP1–SP4):** treat the Criterion `ci`
  `sequential_read` median as the baseline; a change is a regression if the
  median rises **>10%** run-over-run on the same machine (Criterion prints the
  median + its noise estimate). SP1–SP4 record before/after medians in the
  results log and must not breach this gate.

### Latency-injected runs (SP0b — needs /dev/fuse)

```bash
# Functional + gating tests for the passthrough FS (5 tests across 3 files):
cargo test -p musefs-latencyfs -- --ignored --nocapture

# Scan a generated corpus through an injected-latency mount (real fsync counts):
MUSEFS_BENCH_LATENCY_PROFILE=nfs-hdd MUSEFS_BENCH_TIER=large-compute \
  cargo test -p musefs-core --features metrics \
  --test bench_ingest bench_scan_under_latency -- --ignored --nocapture
```

Profiles: `ssd` (≈0), `hdd`, `nfs-ssd`, `nfs-hdd`. The corpus is generated on a
real backing dir; only the scan + DB I/O traverse the latency layer, so the row's
`fsyncs` column is the real kernel fsync count for the scan's DB writes (the
SP1-batching signal). `peak_rss_kib` reads `n/a` here (the FS shares this process,
so VmHWM no longer isolates the scan's footprint — use the SP0a tempfs
`bench_cold_scan_and_revalidate` for the RSS signal). Without
`MUSEFS_BENCH_LATENCY_PROFILE` the test no-ops with a hint.

## Results log

*(Per-SP before/after numbers land here as each ships. Format: tier · storage
class · wall time · op counts · fsyncs · peak RSS. Full tables + reproducing
commands live in the repo-root [`BENCHMARKS.md`](../../../../BENCHMARKS.md).)*

- **SP1 — Ingestion scalability** (2026-05-31, AMD EPYC 6c · SSD): on durable
  storage cold scan is **20–500× faster** — `ci`/SSD FLAC 8949 ms → 17 ms
  (526×); `bandwidth`/SSD 30 MiB FLAC 170 263 ms → 8735 ms (19.5×) with
  `bytes_read` 30 GiB → 1.05 GiB. Mechanism: per-file commits at
  `synchronous=FULL` → batched at `NORMAL` = **403 → 0 fsyncs** (latency-FS, 200
  files). Caveat: on tempfs/RAM with sub-window files (`large-compute`) the
  pipeline overhead makes it ~1.9× slower (no fsync cost to amortize). See
  `BENCHMARKS.md` §1–§4.
- **SP2 Stage A — Incremental tree refresh (baseline)** (2026-05-31, box under
  load · tempfs · FLAC): Stage A already renders incrementally (only the changed
  track is re-rendered, O(changed)); the remaining O(N) cost is the
  `VirtualTree::build_with` full tree reconstruction, which Stage B eliminates.
  Hence refresh-1 still scales ~linearly with N. Library-size sweep (refresh-1
  wall, release): **100→0 ms, 1000→4 ms, 5000→41 ms**. Caveat: single-album
  corpus (no disambiguation), so `build_with` time is slightly optimistic vs a
  real multi-album library. This is the Stage A baseline; Stage B (in-place tree
  mutation) targets a flat refresh-1 vs N. Harness:
  `bench_refresh_one_across_library_sizes`. See `BENCHMARKS.md` "SP2 —
  Incremental tree refresh".
- **SP2 Stage B — In-place tree mutation** (2026-05-31, box under load · tempfs
  · FLAC): Stage B replaces `VirtualTree::build_with` with im-backed in-place
  `apply_changes` (O(changed) tree mutation). Library-size sweep (refresh-1 wall,
  release, three runs averaged): **100→~1–6 ms, 1000→~10–22 ms, 5000→~38–94
  ms**. The full `build_with` reconstruction is eliminated; the residual linear
  slope is from the O(N) `list_render_keys` scan + `new_snapshot` HashMap rebuild
  that still precedes `apply_changes` — a future pass could cache this. The tree
  mutation itself is O(changed). Equivalence gate: 64-case proptest + per-refresh
  debug-assert (incremental ≡ full) green. Fallback test (forced `Err(())` →
  full-rebuild) green. FUSE byte-identical PCM e2e green. See `BENCHMARKS.md`
  "SP2 — Incremental tree refresh".
  - **Follow-up (known residual, not addressed in SP2):** `rebuild_incremental`
    still performs two O(library) steps before the O(changed) `apply_changes`:
    the `Db::list_render_keys` identity scan (every track's `(id,
    content_version, format)`) and the full `new_snapshot` reconstruction (a
    fresh `HashMap<i64, TrackRenderState>` rebuilt each refresh, cloning the
    cached path for every unchanged track). These are cheap relative to the
    eliminated `build_with` (no rendering, no tree ops) but keep `poll_refresh`
    O(N) rather than strictly O(changed), so the library-size sweep is not flat.
    Making it truly O(changed) end-to-end means mutating the snapshot in place
    (apply only changed/added/removed against the retained `prev_snapshot`) and
    a changed-set DB query instead of the full identity scan — see the SP2 spec
    "Out of scope (YAGNI)". Deferred: the residual is low-tens-of-ms at ~5k–1M
    rows and was never the bottleneck the full rebuild was.
