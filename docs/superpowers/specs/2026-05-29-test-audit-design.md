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

This is an audit of an *already-substantial* suite: ~259 Rust tests run by
`cargo test --workspace`, plus separate surfaces that the default run excludes —
`#[ignore]`d FUSE e2e tests, proptest invariants (some behind the `fuzzing`
feature), the mutagen interop suite, the beets-plugin pytest suite, and the
cargo-fuzz targets — with `cargo-llvm-cov` coverage wired to Codecov in CI. The
job is to find the gaps and weak spots, not to bootstrap testing from nothing.

**Counting methodology.** Test counts vary by surface, so Phase A reports them
per category rather than as one number: (a) `cargo test --workspace` default
count; (b) `#[ignore]`d tests (FUSE e2e, interop emitter) run explicitly; (c)
proptest/`fuzzing`-feature tests; (d) beets pytest count; (e) cargo-fuzz target
count. Any single headline figure in the report names which categories it sums.

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

The plan must make setup explicit and reproducible before any phase that depends
on it. For every tool: **detect presence, record the version, install only if
missing** (don't assume the install state captured here is still true — it drifts).
The report lists the actual resolved version of each tool used.

- `cargo-llvm-cov` — present at design time; `cargo llvm-cov --version` (ensure
  `~/.cargo/bin` on PATH). Install only if absent.
- `cargo-fuzz` — present at design time (`cargo-fuzz 0.13.1`); record the version.
  If absent, install with `cargo install cargo-fuzz` **without** `--locked`
  (matching `.github/workflows/fuzz.yml`, whose comment notes the pinned `rustix`
  fails to compile on current nightly). Requires the `nightly` toolchain (present).
- `cargo-mutants` — absent at design time. If still absent, install the
  confirmed-current pin with `cargo install cargo-mutants --version 27.0.0
  --locked`. Record the resolved version (`cargo mutants --version`).
- **beets plugin venv.** `contrib/beets/requirements.txt` pins `beets` + `pytest`
  only — **no `pytest-cov`** and no mutagen. Phase 0 builds the venv from that
  file, then additionally installs `pytest-cov` (for `beetsplug/` coverage) and
  `mutagen==1.47.0` (from `tests/interop/requirements.txt`, needed by the interop
  pytest). If `pytest-cov` cannot be installed, the beets suite is still run for
  pass/fail and its coverage cell reads "not measured (pytest-cov unavailable)".
- **Network-unavailable fallback:** if `cargo install` / `pip install` cannot
  reach a registry, the dependent surface is marked **blocked** in the report (not
  silently skipped): Phase B emits "mutation testing blocked — cargo-mutants
  unavailable", Phase A emits the same for the fuzz smoke surface and for any
  Python dep it couldn't install, and the affected scorecard cells read "not
  measured (tooling unavailable)". The coverage and judgment phases still run.

### Phase A — Ground truth (empirical baseline)

Run the full test surface and record current state:

- `cargo test --workspace` (default suite).
- `cargo test -p musefs-fuse -- --ignored` (FUSE e2e; `/dev/fuse` is available in
  this environment).
- `cargo test -p musefs-fuse --features metrics -- --ignored --test-threads=1`
  **separately** — `concurrency.rs` is `#![cfg(feature = "metrics")]` and is
  silently absent from the plain run above. The audit must not cite concurrency
  e2e evidence without this run.
- `cargo test -p musefs-format --features fuzzing` and
  `cargo test -p musefs-core --test proptest_read_fidelity` (proptests).
- mutagen interop suite, two steps sharing one temp dir `$D`:
  `MUSEFS_INTEROP_DIR=$D cargo test -p musefs-core --test interop_emit -- --ignored
  emit_interop_fixtures`, then `MUSEFS_INTEROP_DIR=$D python -m pytest
  tests/interop/test_mutagen_roundtrip.py` (in the Phase-0 venv with
  `mutagen==1.47.0`). `$D` is a fresh temp dir, not a tracked path.
- **beets plugin suite** (`contrib/beets/`, run in the Phase-0 venv): first
  `cargo build -p musefs-cli` so the `musefs_bin` and `e2e` suites don't auto-skip
  on a missing binary. Then `python -m pytest` (unit + integration),
  `python -m pytest -m musefs_bin` (path-gate vs the real binary), and
  `python -m pytest -m e2e` (beets → mount → playback). **Skips count as not-run,
  not pass:** record `passed/failed/skipped` per invocation and, for any skip,
  the reason (missing `beet`, `fusermount`, `ffmpeg`, `/dev/fuse`, or binary). An
  all-skipped suite is reported as "not exercised," never as green. Add
  `--cov=beetsplug` when `pytest-cov` is available.
- **Fuzz smoke surface** (mirrors `.github/workflows/fuzz.yml` per-PR job):
  `cargo +nightly fuzz build`, then each target
  (`flac mp3 mp4 ogg wav ogg_page b64 vorbiscomment`) run with the CI bounds
  `-max_len=131072 -rss_limit_mb=2048 -max_total_time=15` (15 s/target, per
  `fuzz.yml:45`). Goal is to confirm every target still builds and reaches its
  parser, not a full campaign. Record any broken/unreachable target.
- `cargo-llvm-cov` over the workspace excluding `musefs-fuse`, matching CI.
  Capture per-crate and per-module line + region coverage. **This is "default CI
  Rust coverage":** it instruments the plain `cargo test` run, so it does *not*
  reflect `#[ignore]`d e2e tests or the `--features fuzzing` format proptests.
  Coverage numbers in the report are labeled as such. (Optional, time permitting:
  a second `--features fuzzing` coverage profile for `musefs-format` to show what
  the proptests add; only if it's cheap — it is not required for the scorecard.)

**Red-test gate.** If any **Tier-1** test (byte-identical invariant or
resolution/freshness, including the relevant `#[ignore]`d e2e tests) fails or is
flaky in this phase, **halt and report before Phase B/C.** Mutation results are
meaningless against a suite that isn't green — a mutant "caught" by an
already-failing test tells us nothing. A Tier-2/Tier-3 or tooling-blocked failure
is recorded and the audit continues, noting reduced confidence for that area.

**FUSE coverage strategy.** `cargo-llvm-cov` excludes `musefs-fuse` (real mounts
don't instrument cleanly), so FUSE-only behaviors are **not** scored by
line/region coverage. Most of the underlying logic lives in `musefs-core` and is
covered there (e.g. `poll_refresh_notify` / the inode allocator); the thin
`musefs-fuse` adapter glue — `--keep-cache` `inval_inode` dispatch, worker-pool
offload wiring — is scored instead by **e2e evidence**: presence and assertion
strength of the relevant `#[ignore]`d mount tests (`keep_cache.rs`,
`concurrency.rs` — only via the `--features metrics` run above —
`ogg_read_through.rs`, `playback_pcm.rs`), confirmed green on `/dev/fuse` in
Phase A. The report states this scoring basis explicitly per FUSE-only area
rather than implying a coverage number exists.

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
- **Concrete bounds** (mutation on format parsers can otherwise run for hours):
  scope each invocation with `--file` globs to the target list above (never the
  whole workspace); set `--timeout-multiplier 2.0` plus a `--minimum-test-timeout`
  floor; and cap wall-clock per crate at **~30 min** (`timeout 1800 cargo
  mutants ...`). If a crate hits the cap, record it as a **partial** run (mutants
  tested / total) rather than failing — partial mutation data is still useful and
  the report says so.
- **Test command / features.** A default `cargo-mutants` run uses a plain
  `cargo test` and would miss `musefs-format`'s `--features fuzzing` proptests —
  the strongest checks on exactly the format code under audit — inflating the
  surviving-mutant count. Pass the feature through (`--features fuzzing` for the
  `musefs-format` run, e.g. via `cargo mutants ... -- --features fuzzing`) so the
  proptests participate in killing mutants. The report records the exact
  `cargo-mutants` invocation (features included) per crate.
- Record surviving mutants with `file:line` and the mutation applied.

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

- **Coverage** — uncovered lines/regions from default CI Rust coverage
  (`cargo-llvm-cov`, plain `cargo test`; does not include `#[ignore]`d e2e or
  `--features fuzzing` proptests — labeled as such, see Phase A). For FUSE-only
  areas, scored by e2e evidence instead (see FUSE coverage strategy); for the
  beets plugin, by `pytest-cov` over `beetsplug/`. The report states the basis.
- **Quality** — surviving mutants from Phase B plus assertion-strength notes.
- **Edge cases** — a checklist of boundary/adversarial conditions, each marked
  covered / partial / missing. Seed checklist (extend per area): empty /
  truncated / very large files; malformed or out-of-spec headers; multi-value and
  Unicode tags; path collisions and disambiguation; concurrent refresh during an
  in-flight read; backing file modified between `open()` and `read()` (size/mtime
  drift → `BackingChanged`); NFS-style stale file handles (`ESTALE`) on a backing
  read; zero-byte and oversized embedded art; chained/multiplexed Ogg
  (skipped by design — confirm the skip is tested); mode boundaries
  (synthesis vs structure-only).

## Deliverable: the report

One markdown report at `docs/audits/2026-05-29-test-audit.md` (the plan creates
the `docs/audits/` directory, which does not yet exist), structured as:

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
