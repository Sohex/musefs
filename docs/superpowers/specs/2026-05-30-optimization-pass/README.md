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
  files), transaction batching across the scan loop, optional parallel probing,
  bulk-write pragma tuning. *Spec: TBD after SP0.*
- **SP2 — Incremental tree refresh.** Rebuild only changed tracks on a
  `data_version` bump instead of reloading and re-rendering the whole library.
  *Spec: TBD.*
- **SP3 — Read/serve residuals.** Remove the `read_segments` double-allocation;
  reduce `handles` / `size_cache` global-mutex contention. *Spec: TBD.*
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
| SP0a | Implemented | `SP0-measurement-foundation.md` | `../../plans/2026-05-30-optimization-sp0a-corpus-and-benches.md` | Corpus generator + compute benches + reporting; no /dev/fuse — runs now. See "Running the SP0a harness" below |
| SP0b | Plan drafted | `SP0-measurement-foundation.md` | `../../plans/2026-05-30-optimization-sp0b-latency-fuse.md` | `musefs-latencyfs` passthrough latency-injection FUSE + fsync counter; needs /dev/fuse — VPS |
| SP1 | Not started | — | — | |
| SP2 | Not started | — | — | |
| SP3 | Not started | — | — | |
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

## Results log

*(Per-SP before/after numbers land here as each ships. Format: tier · storage
class · wall time · op counts · fsyncs · peak RSS.)*
