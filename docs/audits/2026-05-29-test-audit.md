# musefs Test Suite Audit — 2026-05-29

**Status:** in progress
**Spec:** docs/superpowers/specs/2026-05-29-test-audit-design.md
**Deliverable type:** _full audit_ | _red-test halt report_ (set at the Phase A gate)

## 1. Executive summary

_pending — filled last._

## 2. Environment & tooling (Phase 0)

| Tool | Present? | Version | Notes |
|------|----------|---------|-------|
| cargo-llvm-cov | _pending_ | | |
| cargo-fuzz | _pending_ | | |
| cargo-mutants | _pending_ | | |
| beets venv (pytest/pytest-cov/mutagen) | _pending_ | | |

Blocked surfaces (network/tooling): _none recorded yet._

## 3. Test inventory & counts (Phase A)

Counting methodology: per-category, unique tests only (never summed raw invocation totals).

| Category | Command | Count | Pass/Fail/Skip |
|----------|---------|-------|----------------|
| (a) workspace (incl. fuzzing proptests) | `cargo test --workspace` | _pending_ | |
| (b) FUSE e2e (`--ignored`) | | _pending_ | |
| (b) FUSE concurrency (`--features metrics`) | | _pending_ | |
| (b) core metrics (`--features metrics`) | | _pending_ | |
| (b) interop emitter + mutagen | | _pending_ | |
| (c) beets pytest (default / musefs_bin / e2e) | | _pending_ | |
| (d) cargo-fuzz targets | | _pending_ | |

## 4. Tier-1 test set & flakiness (Phase A)

Enumerated Tier-1 set: _pending._
Flaky tests (file:line): _pending._
Metrics-gated stability (Tier-2): _pending._

## 5. Red-test gate decision

_pending — PASS (continue to Phase B/C) or TRIP (red-test halt report)._

## 6. Coverage (Phase A)

Basis: default CI Rust coverage (`cargo-llvm-cov` over `cargo test --workspace`; includes fuzzing proptests, excludes `#[ignore]`d e2e and the `musefs-fuse` crate).

_per-crate / per-module table pending._

## 7. Fuzz smoke & corpus health (Phase A)

_pending._

## 8. Schema-parity check (Phase A)

_pending._

## 9. Mutation testing (Phase B)

_pending — or "blocked — suite not green" if the gate tripped._

## 10. Per-area scorecard

_pending. Columns: Coverage | Quality | Edge cases, per Tier-1/Tier-2 area._

## 11. Findings

_pending. Format: `file:line` — description — severity (P0/P1/P2)._

## 12. Prioritized remediation backlog

_pending. P0/P1/P2; each item names the target test file and exactly what to add/fix._
