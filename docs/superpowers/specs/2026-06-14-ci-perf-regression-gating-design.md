# CI performance-regression gating (issue #211)

## Problem

musefs's product is read latency, yet the only performance signals are
criterion wall-clock benches (`musefs-core/benches/read_throughput.rs`) and the
ignored `bench_ingest`/`bench_refresh` tests, all run by hand and recorded
manually in `BENCHMARKS.md`. No CI job runs `cargo bench` or compares against a
baseline, so a latency/throughput regression in the splice/read, ingest, or
refresh path can merge with the suite fully green.

GitHub Actions shared `ubuntu-latest` runners vary run-to-run, so naive
wall-clock micro-gating against a stored baseline is famously flaky. The project
already treats **deterministic work counters** (preads, bytes read, fsync count,
copy counts) as its "portable signal" — see `BENCHMARKS.md` (fsync 403→0;
`bytes_read` equal both sides) — and already asserts exact getattr/read/open
counts in `musefs-core/tests/metrics.rs` under the `metrics` feature.

## Goals

- A hard, zero-flake CI gate that catches **algorithmic** regressions on the
  read/serve, ingest, and refresh paths (an extra copy, a reintroduced per-file
  fsync/slurp, a reintroduced O(N) tree rebuild).
- Per-PR visibility into **constant-factor** wall-clock regressions on the
  read/synthesis surface, without flaky pass/fail.
- A recorded full-bench snapshot at release time for hand-curation into
  `BENCHMARKS.md`.

## Non-goals

- Hard-blocking any PR or release on wall-clock numbers (GHA noise makes this
  dishonest).
- Auto-committing benchmark results into `BENCHMARKS.md` (it is hand-curated by
  design; see its preamble).
- Self-hosted or dedicated bench runners.

## Design — three lanes

### Lane 1 — Deterministic counter gate (hard, every non-doc PR)

The real regression guard. A new test module
`musefs-core/tests/perf_counters.rs`, compiled under `--features metrics`, run
by the existing `check` job's "Core metrics-feature tests" step
(`cargo test -p musefs-core --features metrics`). No new CI job. In-memory,
deterministic, sub-second.

Golden exact-equality assertions on the portable signals (a regression flips a
count → hard fail):

- **Read/serve** — for each `bench_formats()` format (flac, mp3, m4a, ogg, wav),
  a fixed-size synthesized single-track corpus. Assert exact
  `(preads, pread_bytes, art_chunks, binary_tag_chunks)` from
  `metrics::snapshot()` for:
  - a whole-file sequential read (`fh = None`, 128 KiB chunks),
  - a cold-first read (fresh mount, read once),
  - a deep seek read (one 128 KiB read near EOF) — guards the SP4 invariant that
    a seek scans a bounded backward window, not the whole page index
    (→ bounded `preads`/`pread_bytes`, not file-size-proportional).
- **Ingest** — scan a fixed `ci`-tier corpus (single format, fixed seed/size).
  Assert exact `(scan_opens, scan_preads, scan_bytes_read)`, and **fsync count
  == 0** through the latency-FS path (the 403→0 guard against a reintroduced
  per-file-commit / whole-file slurp). Reuses the `bench_scan_under_latency`
  fsync-counting mechanism already exercised by `bench_ingest.rs`.
- **Refresh** — a single-track re-tag at two library sizes (e.g. 128 and 2048
  tracks). Assert the **node-touch count is bounded and identical across both
  sizes** (O(changed), not O(N)) — catches reintroduction of the full
  `VirtualTree::build_with` reconstruction that `apply_changes` replaced.

**New production code (the only piece):** a node-touch counter under
`#[cfg(feature = "metrics")]`, incremented once per node mutated in
`VirtualTree::apply_changes` (`musefs-core/src/tree.rs:642`), surfaced via a new
field on `metrics::Snapshot` with a no-op stub in the non-`metrics` build (mirror
the existing counter pattern in `musefs-core/src/metrics.rs`).

Legitimate strategy changes update the golden numbers in the same PR — the same
intended friction as today's getattr/read-count assertions.

### Lane 2 — Same-runner A/B wall-clock (warn-only, path-gated)

Per-PR constant-factor visibility, robust to runner variance by benching both
commits on the **same** runner.

- **Path filter:** a new `perf` output on the existing `changes` job, set `true`
  when `git diff --name-only "$base...HEAD"` matches
  `^(musefs-core/src/|musefs-format/src/)` (the read/synthesis surface; crate
  `tests/` and `benches/` dirs are excluded by the glob). Mirrors the existing
  `fuse`/`lidarr` output pattern.
- **Job:** new `perf-ab` job in `ci.yml`, `if: needs.changes.outputs.perf ==
  'true'`, `permissions: { contents: read, pull-requests: write }`. Full-history
  checkout (base merge-base must be present). Installs libfuse3. Drives an
  in-tree `scripts/perf-ab.sh`:
  1. `cargo bench -p musefs-core --bench read_throughput -- --save-baseline base`
     on the base SHA,
  2. checkout PR HEAD, same with `--save-baseline pr`,
  3. `critcmp base pr` → delta table.
- **Surface:** a **sticky PR comment** (e.g. `marocchino/sticky-pull-request-comment`,
  SHA-pinned) with the critcmp table, a header flagging any bench regressed
  >10%. **Never blocks** — informational only, not a required check. On fork PRs
  the write token is absent, so the comment step is guarded and the table falls
  back to `$GITHUB_STEP_SUMMARY`.

Cost: builds criterion twice. Bounded because the job only runs on
core/format `src/**` changes.

### Lane 3 — Full bench record (release tags)

A recorded snapshot, not a gate.

- **Job:** new `benchmarks` job in `release.yml`, on tag push. Runs the full
  `read_throughput` criterion suite plus the ignored `bench_ingest` /
  `bench_refresh` at a representative tier (`MUSEFS_BENCH_TIER`), `--nocapture`,
  via an in-tree `scripts/perf-release-bench.sh`.
- **Output:** uploads captured results as a release **artifact**
  (`actions/upload-artifact`) for hand-curation into `BENCHMARKS.md`. No
  auto-commit. **Non-blocking** — a slow/noisy bench must never block a release.

## Documentation & harness

- `scripts/perf-ab.sh` and `scripts/perf-release-bench.sh` live in-tree and run
  locally; CI only invokes them.
- `CONTRIBUTING.md` test-tiers section: document the Lane 1 counter gate and the
  Lane 2 A/B job (when it runs, that it is warn-only).
- `BENCHMARKS.md`: a short "CI regression gating" subsection describing the
  three lanes and the manual-vs-CI split.

## Risks & tradeoffs

- **Algorithmic-only hard gate.** Lane 1 catches changes in work *counts*, not
  constant-factor slowdowns; the latter are surfaced only by Lane 2, only on
  core-`src` PRs, only as a warning. This is the honest limit of gating on noisy
  GHA runners.
- **Mutants anchors.** Adding the touch counter in `tree.rs` shifts line numbers
  → re-anchor `.cargo/mutants.toml` in the same commit (the
  `check_mutant_anchors.py` pre-commit guard fails otherwise).
- **Golden-number maintenance.** Exact counter assertions must be updated
  whenever read/ingest/refresh work legitimately changes — intentional friction.
- **Fork PRs.** Lane 2's PR comment degrades to a job summary (no write token).
- **Pre-commit gate.** The new `perf_counters.rs` runs under
  `cargo test -p musefs-core --features metrics`, which the workspace default
  `cargo test` skips — ensure the metrics-feature leg is green before pushing
  (the full workspace pre-commit hook covers it; bare `cargo test` does not).

## Affected files

- `musefs-core/src/metrics.rs` — new touch counter + `Snapshot` field (+ stub).
- `musefs-core/src/tree.rs` — increment touch counter in `apply_changes`.
- `musefs-core/tests/perf_counters.rs` — new golden-counter test module.
- `.cargo/mutants.toml` — re-anchor after `tree.rs` line shift.
- `.github/workflows/ci.yml` — `perf` output on `changes`; new `perf-ab` job.
- `.github/workflows/release.yml` — new `benchmarks` job.
- `scripts/perf-ab.sh`, `scripts/perf-release-bench.sh` — new harness scripts.
- `CONTRIBUTING.md`, `BENCHMARKS.md` — docs.
