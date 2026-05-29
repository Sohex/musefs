# Test Suite Audit — Design

**Date:** 2026-05-29
**Status:** Approved (design); ready for implementation planning
**Deliverable:** An audit report + a prioritized remediation backlog, produced by
executing the audit. No production-code changes; this effort produces findings and
a plan, not fixes.

## Goal

Audit musefs's existing test suite for **coverage**, **quality**, and **edge-case
handling**, and produce a single report whose closing section is a prioritized
remediation backlog ready to execute later.

This is an audit of an *already-substantial* suite (~281 Rust tests across five
crates, plus proptest invariants, cargo-fuzz targets, mutagen interop, and
`cargo-llvm-cov` coverage wired to Codecov in CI). The job is to find the gaps and
weak spots in that suite, not to bootstrap testing from nothing.

## Scope decisions

- **Prioritization:** risk-weighted. Depth goes to correctness-critical paths;
  the rest is surveyed lightly. See Tiers below.
- **Method:** empirical. Run real tooling (coverage + mutation testing) to ground
  every claim, supplemented by judgment review.
- **Output:** report at `docs/audits/2026-05-29-test-audit.md`, plus the
  implementation plan (written next via the writing-plans skill) that executes the
  audit phases.

## Methodology — phases

### Phase 0 — Tooling preflight (reproducible setup)

Mutation and fuzz tooling is not all present in the environment, so the
implementation plan must make setup explicit and reproducible before any phase
that depends on it:

- `cargo-llvm-cov` — already installed; verify on PATH (`~/.cargo/bin`) with
  `cargo llvm-cov --version`. No install needed.
- `cargo-mutants` — **not installed.** Install a pinned version with
  `cargo install cargo-mutants --version <pin> --locked` (record the exact pin in
  the report so the run is reproducible). Verify with `cargo mutants --version`.
- `cargo-fuzz` — **not installed.** Install with `cargo install cargo-fuzz`
  **without** `--locked` (matching `.github/workflows/fuzz.yml`, whose comment
  notes the pinned `rustix` fails to compile on current nightly). Requires the
  `nightly` toolchain (present).
- **Network-unavailable fallback:** if `cargo install` cannot reach the registry,
  the dependent phase is marked **blocked** in the report (not silently skipped):
  Phase B emits "mutation testing blocked — cargo-mutants unavailable" and Phase A
  emits the same for the fuzz smoke surface, and the affected scorecard cells read
  "not measured (tooling unavailable)". The coverage and judgment phases still run.

### Phase A — Ground truth (empirical baseline)

Run the full test surface and record current state:

- `cargo test --workspace` (default suite).
- `cargo test -p musefs-fuse -- --ignored` (FUSE e2e; `/dev/fuse` is available in
  this environment).
- `cargo test -p musefs-format --features fuzzing` and
  `cargo test -p musefs-core --test proptest_read_fidelity` (proptests).
- mutagen interop suite (`MUSEFS_INTEROP_DIR=... cargo test ... -- --ignored
  emit_interop_fixtures` then `python -m pytest tests/interop`).
- **beets plugin suite** (`contrib/beets/`): `python -m pytest` (unit +
  integration), `python -m pytest -m musefs_bin` (path-gate vs the real `musefs`
  binary), and `python -m pytest -m e2e` (beets → mount → playback). Record pass
  state and, where practical, `pytest-cov` line coverage for `beetsplug/`.
- **Fuzz smoke surface** (mirrors `.github/workflows/fuzz.yml` per-PR job):
  `cargo +nightly fuzz build`, then a short run of each target
  (`flac mp3 mp4 ogg wav ogg_page b64 vorbiscomment`) with a bounded
  `-max_total_time`. Goal is to confirm every target still builds and reaches its
  parser, not a full fuzzing campaign. Record any broken/unreachable target.
- `cargo-llvm-cov` (installed; ensure `~/.cargo/bin` on PATH) over the workspace
  excluding `musefs-fuse`, matching CI. Capture per-crate and per-module line +
  region coverage.

**FUSE coverage strategy.** `cargo-llvm-cov` excludes `musefs-fuse` (real mounts
don't instrument cleanly), so FUSE-only behaviors are **not** scored by
line/region coverage. Most of the underlying logic lives in `musefs-core` and is
covered there (e.g. `poll_refresh_notify` / the inode allocator); the thin
`musefs-fuse` adapter glue — `--keep-cache` `inval_inode` dispatch, worker-pool
offload wiring — is scored instead by **e2e evidence**: presence and assertion
strength of the relevant `#[ignore]`d mount tests (`keep_cache.rs`,
`concurrency.rs`, `ogg_read_through.rs`, `playback_pcm.rs`), confirmed green on
`/dev/fuse` in Phase A. The report states this scoring basis explicitly per
FUSE-only area rather than implying a coverage number exists.

Output of Phase A: a coverage table, the fuzz-smoke result, the beets suite
result, and a note of any currently failing or flaky tests.

### Phase B — Mutation testing (test quality)

With `cargo-mutants` installed (Phase 0), run it **scoped** to the risk-critical
files (whole-workspace mutation is too slow). Surviving mutants — code mutated
without any test failing — are the sharpest signal of weak or missing assertions.

- Targets: `musefs-format` (synthesis, `ogg/`, format parsers) and `musefs-core`
  (`reader.rs`, `tree.rs`, `scan.rs`, `facade.rs`, and `ogg_index.rs` — central to
  Tier-1 Ogg read correctness: lazy page indexing, sequence renumbering, CRC
  patching, payload serving).
- Bounded with a per-mutant timeout multiplier and `--file` globs to keep runtime
  practical. Record surviving mutants with `file:line` and the mutation applied.

### Phase C — Judgment review (edge cases + structural quality)

Read the tests on the critical paths, cross-referenced against Phase A gaps and
Phase B survivors. Look for:

- Assertions that don't actually pin behavior (e.g. asserting only that a call
  returns `Ok`, not what it produced).
- Tests that exercise a trivial path while claiming to cover a real one.
- Missing boundary / adversarial inputs.
- Untested error paths.
- Determinism and isolation issues (shared temp state, ordering dependence,
  reliance on wall-clock or filesystem timing).

## Risk-weighted surface (where depth goes)

**Tier 1 — deepest scrutiny:**

- **Byte-identical invariant:** `reader::read_at` splicing across every `Segment`
  variant; per-format `synthesize_layout` (FLAC/MP3/MP4/Ogg/WAV); Ogg page
  renumber + per-page CRC patching; MP4 `stco`/`co64` offset patching.
- **Resolution & freshness:** `HeaderCache` `content_version` invalidation,
  `BackingChanged` (size/mtime drift) detection, `poll_refresh` / `data_version`
  rebuild, inode stability across rebuilds, `--keep-cache` inode invalidation.

**Tier 2 — solid but lighter:**

- **Concurrency:** worker-pool offload of blocking reads/getattr, `ArcSwap` tree
  swap, per-handle I/O.
- **Scan / revalidate:** upsert, prune of gone backing files, orphaned-art GC,
  preservation of external tag edits.
- **beets plugin (`contrib/beets/`):** the plugin writes the SQLite store that is
  the cross-tool contract, so its correctness matters. Audit the pytest suite for
  the path-matching gate, tag/art sync, file move/rename reconciliation, and the
  e2e (beets → mount → playback) path. Scored by `pytest` pass state and
  `pytest-cov` over `beetsplug/` (not Rust coverage).

**Tier 3 — survey only:** CLI arg parsing, template rendering, mapping. Confirm
coverage exists and note gaps; go deep only if something looks alarming.

## Audit dimensions (scored per Tier-1/Tier-2 area)

For each area the report scores three dimensions explicitly:

- **Coverage** — uncovered lines/regions from `cargo-llvm-cov`. For FUSE-only
  areas, scored by e2e evidence instead (see FUSE coverage strategy); for the
  beets plugin, by `pytest-cov` over `beetsplug/`. The report states the basis.
- **Quality** — surviving mutants from Phase B plus assertion-strength notes.
- **Edge cases** — a checklist of boundary/adversarial conditions, each marked
  covered / partial / missing. Seed checklist (extend per area): empty /
  truncated / very large files; malformed or out-of-spec headers; multi-value and
  Unicode tags; path collisions and disambiguation; concurrent refresh during an
  in-flight read; zero-byte and oversized embedded art; chained/multiplexed Ogg
  (skipped by design — confirm the skip is tested); mode boundaries
  (synthesis vs structure-only).

## Deliverable: the report

One markdown report at `docs/audits/2026-05-29-test-audit.md`, structured as:

1. **Executive summary** — overall health, headline numbers, top risks.
2. **Per-area scorecard** — the three dimensions (§ above) for each Tier-1/2 area.
3. **Findings** — concrete gaps, weak tests, and surviving mutants, each with
   `file:line` references.
4. **Prioritized remediation backlog (P0/P1/P2)** — each item names the target
   test file and exactly what to add or fix. This section *is* the remediation
   plan.

## Sequencing note

The audit's findings do not exist until the audit runs. The implementation plan
written next (via writing-plans) is therefore a plan to **execute Phases 0–C and
produce the report**; the report's backlog section is the remediation deliverable.

## Non-goals

- No changes to production (non-test) code.
- No writing of the remediation tests themselves in this effort (that is the
  backlog's job, executed later).
- No deep audit of Tier-3 areas beyond confirming coverage and noting gaps.
- No changes to CI configuration or coverage thresholds.
