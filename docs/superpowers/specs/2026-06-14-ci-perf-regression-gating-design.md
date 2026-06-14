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
  read/serve, ingest, and refresh paths (an extra copy, a reintroduced whole-file
  slurp, a reintroduced O(N) tree rebuild). The fsync-storm signal needs a real
  FUSE mount and is recorded at release rather than per-PR (see Lane 1 ingest).
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

The real regression guard. **No new production code** — every counter it asserts
already exists. Two homes, both run today on every non-doc PR:

- Read/serve + ingest: a new test module `musefs-core/tests/perf_counters.rs`,
  compiled under `--features metrics`, run by the existing `check` job's "Core
  metrics-feature tests" step (`cargo test -p musefs-core --features metrics`).
  In-memory, deterministic, sub-second.
- Refresh: a new `#[cfg(test)]` unit test **appended to the end of the existing
  test module in `musefs-core/src/tree.rs`** (so no line shift above the
  `InodeAllocator` mutants anchors — no `.cargo/mutants.toml` re-anchor needed).
  `apply_changes` is `pub(crate)`, so the assertion must be in-module, not an
  integration test.

Golden exact-equality assertions on the portable signals (a regression flips a
count → hard fail). Every corpus parameter that moves a counter is pinned so the
goldens are reproducible:

- **Read/serve** — iterate a **fixed format list** (`[Flac, Mp3, M4a, Ogg, Wav]`
  written literally, NOT the env-overridable `bench_formats()`, so
  `MUSEFS_BENCH_FORMAT_MIX` can't silently narrow the gate). Per format a single
  pinned corpus: `albums=1, tracks_per_album=1, seed=42`, `bytes_per_track`
  fixed (4 MiB), and **two art variants** — `art_bytes_per_track=0` for the
  audio-only counter goldens and one fixed nonzero size (e.g. 64 KiB) for an
  `art_chunks` assertion. Read chunk size pinned at 128 KiB. Assert exact
  `(preads, pread_bytes, art_chunks, binary_tag_chunks)` from
  `metrics::snapshot()` for:
  - a whole-file sequential read (`fh = None`, 128 KiB chunks),
  - a cold-first read (fresh mount, read once),
  - a deep seek read (one 128 KiB read near EOF) — guards the SP4 invariant that
    a seek scans a bounded backward window, not the whole page index. Asserted as
    a **concrete expected number** per format (frozen TDD-style: write the test,
    observe the count, pin it), not a relation.
- **Ingest** — scan a corpus whose `bytes_per_track` is **larger than the ~1 MiB
  bounded metadata window** (e.g. 2 MiB × a few tracks, fixed seed) so a
  reintroduced whole-file slurp shows up as inflated `scan_bytes_read` (below the
  window, bounded and slurp read the same bytes — SP1 §4 — so the corpus must
  straddle it). Assert exact `(scan_opens, scan_preads, scan_bytes_read)`.
  **The fsync 403→0 guard is NOT in this in-memory lane** — fsync counting needs
  the real `musefs-latencyfs` mount (requires `/dev/fuse`), which would make the
  lane neither in-memory nor sub-second. It stays where it already lives: the
  `#[ignore]` `bench_scan_under_latency` test, run at release (Lane 3).
- **Refresh** — a single-track re-tag applied at two library sizes (e.g. 128 and
  2048 tracks). Assert the value `apply_changes` **already returns** — the count
  of `rebuild_subtree` calls (`tree.rs:752`, documented as the O(changed)
  observability) — is **identical across both sizes**. That count, not "nodes
  mutated," is the size-invariant signal (a `rebuild_subtree` is itself
  O(subtree); only the *number of rebuilds* is O(changed)). Catches
  reintroduction of the full `VirtualTree::build_with` reconstruction.

`metrics` counters are process-global; the read/ingest test module must
serialize its cases and `metrics::reset()` between them (mirror the
`METRICS_LOCK` mutex in `musefs-core/tests/metrics.rs`).

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
- **Trigger event — stays on `pull_request`.** The job checks out **and builds**
  PR HEAD, so it must NOT use `pull_request_target` (that pairs a writable token
  with untrusted-code execution). On `pull_request`, fork PRs get a read-only
  token — handled below.
- **Job:** new `perf-ab` job in `ci.yml`, `if: needs.changes.outputs.perf ==
  'true'`, `permissions: { contents: read, pull-requests: write }`. Full-history
  checkout (base merge-base must be present). Installs libfuse3. Installs
  `critcmp` (not present in the repo today) via a SHA/version-pinned step —
  prefer a pinned prebuilt binary or `cargo install critcmp --locked --version
  <pinned>` with a cargo-bin cache to avoid a multi-minute build each run. Drives
  an in-tree `scripts/perf-ab.sh`:
  1. `cargo bench -p musefs-core --bench read_throughput -- --save-baseline base`
     on the base SHA,
  2. checkout PR HEAD, same with `--save-baseline pr`,
  3. `critcmp base pr` → delta table.
- **Harness/ID drift.** Each side builds the bench from **its own** checkout, so
  a PR that renames/adds/removes benchmark IDs (or edits the shared
  `tests/common` harness) produces a base/PR set that only partially overlaps.
  `critcmp` compares only common IDs; the script must detect a shrunken/empty
  common set and say so in the comment rather than silently reporting "no
  regressions." (Note: a harness-only PR won't trigger this job — the path filter
  excludes `tests/`/`benches/` — but a `src` PR that also touches the harness
  will, hence the guard.)
- **Surface:** a **sticky PR comment** via a specific SHA-pinned action
  (`marocchino/sticky-pull-request-comment`, pinned to a **commit** SHA per the
  annotated-tag pin trap) with the critcmp table and a header flagging any bench
  regressed >10%. **Never blocks** — informational only, not a required check.
  The comment step is guarded with `if: github.event.pull_request.head.repo.fork
  == false` (fork PRs have no write token); on a fork the table is written to
  `$GITHUB_STEP_SUMMARY` instead.

Cost: builds criterion twice. Bounded because the job only runs on
core/format `src/**` changes.

### Lane 3 — Full bench record (release tags)

A recorded snapshot, not a gate.

- **Job:** new `benchmarks` job in `release.yml`, on tag push. Runs the full
  `read_throughput` criterion suite plus the ignored `bench_ingest` /
  `bench_refresh` pinned to the **`ci` tier** (`MUSEFS_BENCH_TIER=ci`, ~200
  tracks — small enough for a shared GHA runner; the `bandwidth` 30 MiB×1k and
  `large-compute` 100k tiers risk multi-hour runs / OOM and are explicitly
  excluded), `--nocapture`, via an in-tree `scripts/perf-release-bench.sh`. This
  records absolute numbers and the in-process ingest counters; it is the home of
  the latency-FS fsync==0 signal (`bench_scan_under_latency`) only if `/dev/fuse`
  is available on the runner, otherwise that leg is skipped and noted.
- **Output:** uploads captured results as a release **artifact**
  (`actions/upload-artifact`, SHA-pinned) for hand-curation into `BENCHMARKS.md`.
  No auto-commit. **Non-blocking** (`continue-on-error` / not a required check) —
  a slow/noisy bench must never block a release.

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
- **No fsync==0 in the PR gate.** The 403→0 ingest signal needs the latency-FS
  mount (`/dev/fuse`), so it lives only in the release lane / the existing
  `#[ignore]` bench, not the per-PR gate. A reintroduced *slurp* is still caught
  per-PR via `scan_bytes_read` (Lane 1 ingest); a reintroduced *per-file fsync
  storm* is caught only at release.
- **Golden-number maintenance.** Exact counter assertions must be updated
  whenever read/ingest/refresh work legitimately changes — intentional friction.
- **Refresh test edits `tree.rs`.** Even appended at the end of the test module,
  confirm `check_mutant_anchors.py` passes before committing; if any
  `.cargo/mutants.toml` anchor shifts, re-anchor in the same commit.
- **Fork PRs.** Lane 2's PR comment degrades to a job summary (no write token).
- **Pre-commit gate.** The new `perf_counters.rs` runs under
  `cargo test -p musefs-core --features metrics`, which the workspace default
  `cargo test` skips — ensure the metrics-feature leg is green before pushing
  (the full workspace pre-commit hook covers it; bare `cargo test` does not).

## Sequencing

Three independently-shippable PRs, in order:

1. **Lane 1 — counter gate.** Pure tests: `perf_counters.rs` + the `tree.rs`
   refresh unit test. Touches no production code or workflows; must land green
   under the metrics-feature leg (pre-commit runs the full suite).
2. **Lane 2 — A/B job.** Workflow + `scripts/perf-ab.sh` + the `perf` filter
   output. Pure CI/scripts.
3. **Lane 3 — release record.** Workflow job + `scripts/perf-release-bench.sh`.

Keeping Lane 1 separate prevents a churny golden-number test from blocking the
CI-only work in Lanes 2–3.

## Affected files

- `musefs-core/tests/perf_counters.rs` — new golden read/ingest counter module
  (reuses existing `metrics` counters; no production change).
- `musefs-core/src/tree.rs` — new refresh unit test appended to the existing
  `#[cfg(test)]` module asserting the `apply_changes` rebuild count is
  size-invariant (no production-body change).
- `.github/workflows/ci.yml` — `perf` output on `changes`; new `perf-ab` job.
- `.github/workflows/release.yml` — new `benchmarks` job.
- `scripts/perf-ab.sh`, `scripts/perf-release-bench.sh` — new harness scripts.
- `CONTRIBUTING.md`, `BENCHMARKS.md` — docs.
