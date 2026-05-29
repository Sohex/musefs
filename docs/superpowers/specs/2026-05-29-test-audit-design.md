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

This is an audit of an *already-substantial* suite — roughly 275 `#[test]` fns
plus 6 `proptest!` blocks across the workspace at the time of writing (**a stale
figure; Phase A re-derives the exact counts and the report uses those, never this
number**), run by `cargo test --workspace` — which **already includes the `fuzzing`-gated format
proptests** via workspace feature unification (`musefs-core`'s dev-dependency
enables `musefs-format/fuzzing`, `musefs-core/Cargo.toml:26`) — plus separate
surfaces the default run excludes: `#[ignore]`d FUSE e2e tests, the mutagen
interop suite, the beets-plugin pytest suite, and the cargo-fuzz targets, with
`cargo-llvm-cov` coverage wired to Codecov in CI. The job is to find the gaps and
weak spots, not to bootstrap testing from nothing.

**Counting methodology.** Phase A reports counts per category, never one number:
(a) `cargo test --workspace` (this already counts the `fuzzing` proptests — they
are *not* a separate additive category); (b) `#[ignore]`d and metrics-gated Rust
tests run explicitly (FUSE e2e, the `--features metrics` concurrency + core
metrics tests, interop emitter); (c) beets pytest — split out, since default
`python -m pytest` excludes the `musefs_bin`/`e2e` markers, so its count is lower
than the file count and the marked runs add to it; (d) cargo-fuzz targets.
**Count tests unique to each surface, never the raw invocation totals** — the
explicit runs overlap with the workspace run (running `-p musefs-format --features
fuzzing` or `-p musefs-core --test proptest_read_fidelity` reruns tests already in
(a); the `--features metrics` runs rerun other tests alongside the gated ones), so
summing raw totals double-counts. Any headline figure names which categories it
sums and uses unique counts.

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
  **Divergence from CI, intentional:** CI installs via
  `pip install -e "contrib/beets[test]"` (resolving the `pyproject.toml`
  `[project.optional-dependencies] test = ["pytest>=7"]` extra). The audit uses
  `requirements.txt` + explicit extra installs instead, for finer control over the
  exact `pytest-cov`/`mutagen` versions. The report notes that the beets env is
  not a byte-for-byte CI reproduction.
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
- `cargo test -p musefs-core --features metrics --test metrics -- --test-threads=1`
  **separately** — `musefs-core/tests/metrics.rs` is `#![cfg(feature = "metrics")]`
  under `musefs-core`'s *own* `metrics` feature; the `musefs-fuse --features
  metrics` run enables core's feature as a dependency but does **not** run core's
  metrics integration tests. Without this the Tier-2 concurrency/perf surface is
  missing from the baseline.
- The `fuzzing`-gated format proptests and `proptest_read_fidelity` **already run
  under `cargo test --workspace` above** (feature unification). Running
  `cargo test -p musefs-format --features fuzzing` /
  `cargo test -p musefs-core --test proptest_read_fidelity` in isolation is
  optional (useful only to time or debug them alone) and must **not** be added to
  the workspace count.
- mutagen interop suite, two steps sharing one temp dir `$D`:
  `MUSEFS_INTEROP_DIR=$D cargo test -p musefs-core --test interop_emit -- --ignored
  emit_interop_fixtures`, then `MUSEFS_INTEROP_DIR=$D python -m pytest
  tests/interop/test_mutagen_roundtrip.py` (in the Phase-0 venv with
  `mutagen==1.47.0`). `$D` is a fresh temp dir, not a tracked path.
- **beets plugin suite** (`contrib/beets/`, run in the Phase-0 venv): first
  `cargo build -p musefs-cli` **and confirm it succeeded** before invoking pytest
  — a failed build leaves the binary absent, silently skipping the `musefs_bin`
  and `e2e` suites (recorded as not-run, but the dependency must be explicit so a
  build failure is surfaced, not masked as a skip). Then `python -m pytest` (unit +
  integration),
  `python -m pytest -m musefs_bin` (path-gate vs the real binary), and
  `python -m pytest -m e2e` (beets → mount → playback). **Skips count as not-run,
  not pass:** record `passed/failed/skipped` per invocation and, for any skip,
  the reason (missing `beet`, `fusermount`, `ffmpeg`, `/dev/fuse`, or binary). An
  all-skipped suite is reported as "not exercised," never as green. For coverage
  when `pytest-cov` is available, the three invocations span the surface: pass
  `--cov=beetsplug` on the first and `--cov=beetsplug --cov-append` on the
  `musefs_bin` and `e2e` runs (pytest-cov resets the data file each run otherwise,
  so the cell would reflect only the last invocation). Alternatively report each
  invocation's coverage separately — but never a single number from one run.
- **Schema-parity check.** The beets suite builds its temp DB from a hand-copied
  fixture, `contrib/beets/tests/schema_v1.sql`, *not* from production
  `musefs-db/src/schema.rs` (`MIGRATION_V1`). Diff the two normalized for
  whitespace/formatting **and strip the trailing `PRAGMA user_version = 1;` from
  the SQL fixture first** — `MIGRATION_V1` deliberately omits it (`migrate()` sets
  `user_version` separately via `pragma_update`, `schema.rs:95`), so a naive diff
  false-positives on that line. After that normalization, any remaining difference
  is real drift: record it as a high-severity finding, treat the beets results as
  suspect until reconciled, and flag a drift-detection test for the backlog.
- **Fuzz smoke surface** (mirrors `.github/workflows/fuzz.yml` per-PR job):
  `cargo +nightly fuzz build`, then each target
  (`flac mp3 mp4 ogg wav ogg_page b64 vorbiscomment`) run with the CI bounds
  `-max_len=131072 -rss_limit_mb=2048 -max_total_time=15` (15 s/target, per
  `fuzz.yml:45`). Goal is to confirm every target still builds and reaches its
  parser, not a full campaign. Record any broken/unreachable target.
- **Fuzz corpus health** (the corpus has grown to ~8,800 entries across 8
  targets). Beyond the smoke run: (a) `cargo +nightly fuzz coverage <target>` on a
  representative subset to confirm the corpus actually reaches the parser/synthesis
  code it's meant to (per `CLAUDE.md`), flagging any target whose coverage looks
  shallow; (b) note corpus size per target and whether it would benefit from
  minimization (`cargo +nightly fuzz cmin <target>`) to drop redundant entries —
  recommended as a backlog item, not run destructively here. This is a
  **time-boxed survey**, not a full coverage campaign; if the nightly tooling is
  blocked (Phase 0), record it as not-measured and move on.
- `cargo-llvm-cov` over the workspace excluding `musefs-fuse`, matching CI.
  Capture per-crate and per-module line + region coverage. **This is "default CI
  Rust coverage":** it instruments the `cargo test --workspace` run, so it
  **includes** the `fuzzing`-gated format proptests (feature unification) but does
  *not* reflect `#[ignore]`d e2e tests (they don't run by default) or the
  `musefs-fuse` crate (excluded). Coverage numbers in the report are labeled with
  exactly this basis.

**Flakiness detection.** The gate and Phase B's caveat both depend on knowing
which tests are flaky, so Phase A must actually look. **The implementation plan
must enumerate the exact Tier-1 test set (file + test names) as its first step**,
then run it **3× total** (e2e mounts also at `--test-threads=1` and the default)
to surface ordering/timing nondeterminism. Concrete starting set to enumerate from
(not exhaustive — the plan finalizes it):
  - Byte-identical / read path: `musefs-core/tests/read_at.rs`,
    `tests/reader.rs`, `tests/proptest_read_fidelity.rs`;
    `musefs-format/tests/layout.rs`, `proptest_*.rs`, `synthesize_*.rs`,
    `roundtrip.rs`; the `ogg_index.rs` inline test.
  - Resolution / freshness: the `facade.rs` / `tests/facade.rs` tests for
    `poll_refresh`, `content_version`, `BackingChanged`; `tests/tree.rs` for inode
    stability; **`tests/external_contract.rs`** — directly exercises the
    backing-drift contract (mutates `audio_length` in the DB, asserts `resolve()`
    returns `CoreError::BackingChanged`), a core Tier-1 freshness guarantee.
  - Tier-1 e2e (`--ignored`): `musefs-fuse/tests/mount.rs`,
    `ogg_read_through.rs`, `keep_cache.rs`, `playback_pcm.rs`.
Any test not stable across all runs is recorded as **flaky** (`file:line`),
feeding both the gate and the Phase B confidence note.

The **metrics-gated tests** (`musefs-core/tests/metrics.rs`,
`musefs-fuse/tests/concurrency.rs`) are Tier-2 but inherently timing-sensitive
(global atomic counters serialized via a `METRICS_LOCK`). Run them 3× too —
always with `--test-threads=1` as they require — but their instability is recorded
as a Tier-2 flakiness finding and does **not** trip the Tier-1 halt gate. The
report notes whether any nondeterminism there reflects a real concurrency bug
versus test-harness contention.

**Red-test gate.** If any **Tier-1** test fails or is flaky in this phase,
**halt and report before Phase B/C.** Mutation results are meaningless against a
suite that isn't green — a mutant "caught" by an already-failing test tells us
nothing. A Tier-2/Tier-3 or tooling-blocked failure is recorded and the audit
continues, noting reduced confidence for that area.

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

- Targets: `musefs-format` (synthesis, `ogg/`, format parsers); `musefs-core`
  (`reader.rs`, `tree.rs`, `scan.rs`, `facade.rs`, and `ogg_index.rs` — central to
  Tier-1 Ogg read correctness: lazy page indexing, sequence renumbering, CRC
  patching, payload serving); and `musefs-db` (`schema.rs`, triggers,
  change-detection) — the foundation every tier builds on, where a silent schema
  or `content_version`-trigger bug corrupts everything above it. The `musefs-db`
  run uses the plain test command (no extra features).
- **Concrete bounds** (mutation on format parsers can otherwise run for hours):
  scope each invocation with `--file` globs to the target list above (never the
  whole workspace); set `--timeout-multiplier 2.0` plus a `--minimum-test-timeout`
  floor; and cap wall-clock per crate at **~30 min** (`timeout 1800 cargo
  mutants ...`). If a crate hits the cap, record it as a **partial** run (mutants
  tested / total) rather than failing — partial mutation data is still useful and
  the report says so. **`musefs-format` is the likeliest to hit the cap** (large
  files — `mp4.rs` ~1,400 lines, `ogg/mod.rs` ~1,040 — plus the slow `--features
  fuzzing` proptests in its test command); budget it first and expect a partial
  run there before the others. **Order files within `musefs-format` by Tier-1
  risk so a cap-truncated run still covers what matters:** the byte-surgery /
  offset-patching paths first — `ogg/` (page renumber + CRC), then `mp4.rs`
  (`stco`/`co64` patching), then `flac.rs` / `wav.rs` / `mp3.rs` — and lower-risk
  helpers last. Compilation overhead (~5–10 min before any mutant runs) comes out
  of the 30-min budget, so the priority ordering is what guarantees signal.
- **Test command / features.** A default `cargo-mutants` run uses a plain
  `cargo test` and would miss `musefs-format`'s `--features fuzzing` proptests —
  the strongest checks on exactly the format code under audit — inflating the
  surviving-mutant count. Pass the feature through (`--features fuzzing` for the
  `musefs-format` run, e.g. via `cargo mutants ... -- --features fuzzing`) so the
  proptests participate in killing mutants. The report records the exact
  `cargo-mutants` invocation (features included) per crate.
- **Test selection — confidence vs. runtime, decided per crate.** By default
  `cargo-mutants` runs only the mutated crate's own tests
  (<https://mutants.rs/workspaces.html>), which misses cross-crate contract
  breakage caught only by dependent crates' tests. Resolve this deliberately:
  - `musefs-db` and `musefs-core` — run **workspace/dependent tests**
    (`--test-workspace=true`). These are the shared-contract crates: a `musefs-db`
    schema/trigger mutation or a `musefs-core` resolution mutation is often caught
    only by a downstream crate's test, so crate-local mutation would overstate
    survivors. Confidence wins here.
  - `musefs-format` — stay **crate-local** (default `--test-workspace=false`),
    since it's the runtime bottleneck (above) and its own proptests are the
    strongest oracle anyway. The report **labels `musefs-format` mutation results
    as crate-local-only** so they aren't read as workspace-wide.
- Record surviving mutants with `file:line` and the mutation applied.
- **Out of mutation scope, by decision:** `db_pool.rs` (concurrency/connection
  pooling — assessed directly in Phase C, since its 3 tests and WAL/pragma
  behavior are awkward to mutate meaningfully under a wall-clock cap) and **the
  beets plugin Python code** (`beetsplug/_core.py`, `musefs.py`). Python mutation
  (`mutmut`/`cosmic-ray`) is **deferred**: it needs separate tooling and a venv
  harness, and the beets suite's own value depends on the schema-parity and
  FK-pragma checks above more than on mutation score. Recorded as a possible
  follow-up, not run in this audit.
- **Flakiness caveat.** A mutant "caught" by a flaky test is unreliable (it may
  have failed for an unrelated reason). If Phase A flagged any flaky test in a
  mutation-target crate, note it here and treat that crate's killed-mutant counts
  as lower-confidence; re-run a suspect mutant before trusting a "caught" result.

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

**Form your own candidates first.** Before reading the two named callouts below,
independently list the thinly-tested Tier-1/Tier-2 paths *you* would prioritize
(from the Phase A coverage table and Phase B survivors). The callouts are prior
knowledge, not the full set — treating them as the answer risks anchoring and
missing other gaps. Reconcile your list against them afterward.

Two specific spots to inspect by name:

- **`ogg_index.rs` has one inline unit test** (`ogg_index.rs:124`,
  `build_index_renumbers_and_preserves_payload_length`) — a single case, plus
  transitive exercise via `facade.rs` / `ogg_read_through.rs` / `read_at.rs`.
  Assess whether that one case is *sufficient* for Tier-1 Ogg correctness (does it
  cover continued pages, multi-page packets, EOS, CRC edge cases?); thin direct
  coverage is a likely backlog item, but do not claim "no direct test exists."
- **`proptest_read_fidelity.rs` is ~47 lines** — thin for the Tier-1 read-fidelity
  property. Assess whether it actually spans segment variants, offsets, and
  partial reads, or just a happy path, independent of what Phase B reports.

**Audit the test infrastructure itself** (oracles and fixture builders are
untested code that every test trusts; a bug here makes green tests meaningless):

- `musefs-format/tests/common/mod.rs` — `resolve_layout()` is an **independent
  reimplementation of the production splicer** used as the format-layer oracle. A
  bug here means tests pass against a wrong oracle. Note specifically that it has
  `unreachable!()` for `OggAudio`/`OggArtSlice` (`mod.rs:80-81`), so it **cannot
  serve as an oracle for Ogg layouts** — any Ogg test routed through it panics
  rather than failing gracefully. Assess whether Ogg correctness has an
  equivalent independent oracle anywhere; if not, that's a Tier-1 gap.
- `musefs-core/tests/common/mod.rs` — `write_flac()` / `minimal_m4a()` fixture
  builders; silent corruption here poisons every downstream test.
- `contrib/beets/tests/conftest.py` — the `db_path` fixture uses raw
  `sqlite3.connect()` + `executescript()` **without `PRAGMA foreign_keys = ON`**,
  while production (`musefs-db Db::configure`, `lib.rs:44`) and the `make_track`
  fixture (`musefs_connect()`) enable FK enforcement. Tests on `db_path` can insert
  orphaned `tags`/`track_art` rows that production would reject — a fixture/prod
  parity gap to flag.
- `musefs-format/src/fuzz_check.rs` — the minimal-fixture module feeding
  `external_contract.rs` and the fuzz seeds; a quick correctness glance.
- `musefs-core/src/db_pool.rs` (3 unit tests) — per-worker WAL connections for the
  concurrency model. A stale connection or a worker connection missing the
  `foreign_keys` pragma could silently corrupt reads. Not a Phase B mutation
  target (see Phase B note); assess its tests directly here.

Findings are recorded uniformly as **`file:line` — description — severity
(P0/P1/P2)**, so Phase C output drops straight into the report's findings and
backlog sections.

## Time budget

A rough wall-clock estimate so the plan can timebox and the executor knows when to
escalate rather than silently grind:

- **Phase 0** (tooling/venv install, mostly `cargo install cargo-mutants` compile)
  — ~15–30 min, dominated by the mutants build.
- **Phase A** (workspace + e2e + metrics + interop + beets ×3 + fuzz smoke ×8 +
  flakiness ×3 + coverage) — **~45–90 min.**
- **Phase B** (mutation, capped ~30 min/crate × 3 target crates) — **~60–90 min**,
  expecting a partial `musefs-format` run.
- **Phase C** (judgment reading) — bounded by attention, not commands; target
  ~60–90 min and stop at diminishing returns.

**Total ≈ 3–5 hours.** If any single phase runs >1.5× its estimate, the executor
records progress so far, notes the overrun in the report, and escalates rather
than blocking indefinitely. Tooling-blocked surfaces (Phase 0 fallback) shorten
this materially.

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
- **DB schema & triggers (`musefs-db`):** the store is the cross-tool contract;
  the `content_version`/`updated_at` triggers and change detection underpin
  freshness for every tier above. A silent trigger or schema bug is high-blast-
  radius, hence Tier-2 rather than survey-only.
- **beets plugin (`contrib/beets/`):** the plugin writes the SQLite store that is
  the cross-tool contract, so its correctness matters. Audit the pytest suite for
  the path-matching gate, tag/art sync, file move/rename reconciliation, and the
  e2e (beets → mount → playback) path. Scored by `pytest` pass state and
  `pytest-cov` over `beetsplug/` (not Rust coverage).

**Tier 3 — survey only:** CLI arg parsing, template rendering, mapping. Confirm
coverage exists and note gaps; go deep only if something looks alarming. The
`read_throughput` Criterion bench (`musefs-core`) is **noted but out of scope** —
it's a performance probe, not a correctness test; the audit confirms it still
builds/runs and flags it for a separate perf-regression effort, nothing more.

## Audit dimensions (scored per Tier-1/Tier-2 area)

For each area the report scores three dimensions explicitly:

- **Coverage** — uncovered lines/regions from default CI Rust coverage
  (`cargo-llvm-cov` over `cargo test --workspace`; **includes** the `fuzzing`
  proptests via feature unification, **excludes** `#[ignore]`d e2e and the
  `musefs-fuse` crate — see Phase A). For FUSE-only areas, scored by e2e evidence
  instead (see FUSE coverage strategy); for the beets plugin, by `pytest-cov` over
  `beetsplug/`. The report states the basis.
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

**Alternate deliverable if the red-test gate trips.** When a Tier-1 test fails or
is flaky in Phase A, Phases B/C don't run, so the full mutation-backed scorecard
can't exist. The report is then written as a **"red-test halt report"** at the
same path: executive summary + the Phase A results obtained so far (coverage
table, fuzz/beets/interop status) + the failing/flaky tests with `file:line` and
repro + a **narrowed P0 backlog** whose first items are "make Tier-1 green/stable"
(Phases B/C deferred until then). The scorecard's Quality column reads "blocked —
suite not green"; the report says explicitly that mutation/judgment depth was not
reached. This satisfies the deliverable contract without faking a scorecard.

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
