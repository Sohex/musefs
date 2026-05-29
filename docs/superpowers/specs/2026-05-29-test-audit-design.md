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

## Methodology — three phases

### Phase A — Ground truth (empirical baseline)

Run the full test surface and record current state:

- `cargo test --workspace` (default suite).
- `cargo test -p musefs-fuse -- --ignored` (FUSE e2e; `/dev/fuse` is available in
  this environment).
- `cargo test -p musefs-format --features fuzzing` and
  `cargo test -p musefs-core --test proptest_read_fidelity` (proptests).
- mutagen interop suite (`MUSEFS_INTEROP_DIR=... cargo test ... -- --ignored
  emit_interop_fixtures` then `python -m pytest tests/interop`).
- `cargo-llvm-cov` (installed; ensure `~/.cargo/bin` on PATH) over the workspace
  excluding `musefs-fuse`, matching CI. Capture per-crate and per-module line +
  region coverage.

Output of Phase A: a coverage table and a note of any currently failing or flaky
tests.

### Phase B — Mutation testing (test quality)

`cargo install cargo-mutants`, then run it **scoped** to the risk-critical files
(whole-workspace mutation is too slow). Surviving mutants — code mutated without
any test failing — are the sharpest signal of weak or missing assertions.

- Targets: `musefs-format` (synthesis, `ogg/`, format parsers) and `musefs-core`
  (`reader.rs`, `tree.rs`, `scan.rs`, `facade.rs`).
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

**Tier 3 — survey only:** CLI arg parsing, template rendering, mapping. Confirm
coverage exists and note gaps; go deep only if something looks alarming.

## Audit dimensions (scored per Tier-1/Tier-2 area)

For each area the report scores three dimensions explicitly:

- **Coverage** — uncovered lines/regions from `cargo-llvm-cov`.
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
written next (via writing-plans) is therefore a plan to **execute Phases A–C and
produce the report**; the report's backlog section is the remediation deliverable.

## Non-goals

- No changes to production (non-test) code.
- No writing of the remediation tests themselves in this effort (that is the
  backlog's job, executed later).
- No deep audit of Tier-3 areas beyond confirming coverage and noting gaps.
- No changes to CI configuration or coverage thresholds.
