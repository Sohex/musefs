# Test Suite Audit — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Execute the test-suite audit defined in `docs/superpowers/specs/2026-05-29-test-audit-design.md` (Phases 0–C) and produce the audit report with a prioritized remediation backlog at `docs/audits/2026-05-29-test-audit.md`.

**Architecture:** An investigative, not feature-building, plan. Each task runs a concrete command, captures raw output to an untracked scratch dir (`/tmp/musefs-audit/`), and distils findings into the single tracked deliverable (`docs/audits/2026-05-29-test-audit.md`). A red-test gate after Phase A branches the deliverable to a narrowed "red-test halt report" when any Tier-1 test fails or is flaky. **No production (non-test) code is modified; no remediation tests are written here** — the report's backlog is the only forward artifact.

**Tech Stack:** Rust workspace (`cargo test`, `cargo-llvm-cov`, `cargo-mutants`, `cargo-fuzz` on nightly), SQLite, FUSE (`/dev/fuse`), Python beets plugin (`pytest`, `pytest-cov`, `mutagen`).

---

## Conventions for this plan

- **Scratch dir:** all raw command logs go to `/tmp/musefs-audit/` (untracked). Create once in Task 1.
- **Deliverable:** `docs/audits/2026-05-29-test-audit.md` only. Commit it at the end of each phase (Tasks 2, 9, 13, 14).
- **Findings format (every finding, everywhere):** `` `file:line` — description — severity (P0/P1/P2) ``.
- **"Expected" in a step** means *what to verify before recording*, not a pass/fail build gate — this is an audit. If a command errors in a way the spec anticipates (tooling missing, gate tripped), follow the branch named in the step rather than stopping.
- **Do not modify** any file outside `docs/audits/` and this plan's checkboxes. If a command wants to write into the repo (e.g. `fuzz cmin`), do **not** run it destructively — only record the recommendation.

---

## Task 1: Scaffold the report and scratch dir (Phase 0 start)

**Files:**
- Create: `docs/audits/2026-05-29-test-audit.md`
- Create (untracked): `/tmp/musefs-audit/`

- [ ] **Step 1: Create the scratch dir**

Run:
```bash
mkdir -p /tmp/musefs-audit && echo ok
```
Expected: `ok`.

- [ ] **Step 2: Create the report skeleton**

Create `docs/audits/2026-05-29-test-audit.md` with exactly this content:

```markdown
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
```

- [ ] **Step 3: Commit the skeleton**

```bash
git add docs/audits/2026-05-29-test-audit.md
git commit -m "audit: scaffold test-suite audit report"
```

---

## Task 2: Phase 0 — tooling preflight & versions

**Files:**
- Modify: `docs/audits/2026-05-29-test-audit.md` (§2)
- Logs: `/tmp/musefs-audit/phase0-*.log`

- [ ] **Step 1: Ensure cargo bin on PATH and detect Rust tools**

Run:
```bash
export PATH="$HOME/.cargo/bin:$PATH"
{ cargo llvm-cov --version; echo "---"; cargo fuzz --version; echo "---"; cargo mutants --version; } 2>&1 | tee /tmp/musefs-audit/phase0-tools.log
```
Expected: `cargo-llvm-cov` and `cargo-fuzz 0.13.x` print versions; `cargo mutants` likely errors ("no such subcommand") — that's the install trigger in Step 2.

- [ ] **Step 2: Install cargo-mutants only if absent**

If Step 1 showed no `cargo mutants` version, run:
```bash
cargo install cargo-mutants --version 27.0.0 --locked 2>&1 | tee /tmp/musefs-audit/phase0-mutants-install.log
cargo mutants --version
```
Expected: prints `cargo-mutants 27.0.0`.
**Network fallback:** if `cargo install` fails to reach the registry, record cargo-mutants as **blocked** in §2 and §9 ("mutation testing blocked — cargo-mutants unavailable"); Phase B will be skipped and its scorecard cells read "not measured (tooling unavailable)". Continue the audit.

- [ ] **Step 3: Build the beets venv with extras**

Run:
```bash
python3 -m venv /tmp/musefs-audit/venv
/tmp/musefs-audit/venv/bin/pip install -r contrib/beets/requirements.txt pytest-cov mutagen==1.47.0 2>&1 | tee /tmp/musefs-audit/phase0-venv.log
/tmp/musefs-audit/venv/bin/pip list 2>&1 | grep -Ei "beets|pytest|pytest-cov|mutagen"
```
Expected: `beets`, `pytest`, `pytest-cov`, `mutagen 1.47.0` all listed.
**Note in §2:** this diverges from CI (`pip install -e "contrib/beets[test]"`) intentionally, for version control over pytest-cov/mutagen — the beets env is not a byte-for-byte CI reproduction.
**Fallback:** if `pytest-cov` won't install, record it; beets coverage cells later read "not measured (pytest-cov unavailable)". If `mutagen` won't install, the interop pytest is blocked — record it.

- [ ] **Step 4: Record the environment table**

Fill §2 of the report from the logs: present?/version/notes for each tool, the beets venv contents, the CI-divergence note, and any blocked surfaces. (Committed with Phase A in Task 9.)

---

## Task 3: Phase A — enumerate the exact Tier-1 test set

The spec mandates this as the first Phase-A step (it drives flakiness detection and the gate).

**Files:**
- Modify: `docs/audits/2026-05-29-test-audit.md` (§4)
- Logs: `/tmp/musefs-audit/tier1-*.log`

- [ ] **Step 1: List candidate Tier-1 tests by name**

Run:
```bash
export PATH="$HOME/.cargo/bin:$PATH"
# Byte-identical / read path + freshness (non-ignored)
cargo test -p musefs-core --test read_at --test reader --test proptest_read_fidelity \
  --test facade --test tree --test external_contract -- --list 2>/dev/null \
  | tee /tmp/musefs-audit/tier1-core.log
cargo test -p musefs-format --features fuzzing \
  --test layout --test roundtrip --test synthesize_tags --test synthesize_art \
  --test proptest_flac --test proptest_mp3 --test proptest_mp4 --test proptest_ogg --test proptest_wav \
  -- --list 2>/dev/null | tee /tmp/musefs-audit/tier1-format.log
# Tier-1 e2e (ignored)
cargo test -p musefs-fuse --test mount --test ogg_read_through --test keep_cache --test playback_pcm \
  -- --ignored --list 2>/dev/null | tee /tmp/musefs-audit/tier1-e2e.log
```
Expected: each prints `<name>: test` lines. The `ogg_index.rs` inline test (`build_index_renumbers_and_preserves_payload_length`) is in `musefs-core` lib tests — capture it via `cargo test -p musefs-core --lib -- --list | grep ogg_index`.

- [ ] **Step 2: Triage the FULL format test-file list (don't silently omit)**

The Step-1 commands list a *starting* subset. `musefs-format/tests/` has more files that touch the byte-identical synthesis paths and may be Tier-1, not Tier-2. List them all and classify each explicitly:
```bash
ls musefs-format/tests/*.rs
```
For each — including `flac_pictures.rs`, `mp3_pictures.rs`, `mp3_synthesize.rs`, `wav_synthesize.rs`, `synthesize_*.rs`, `read_metadata.rs`, `read_comments.rs`, `mp3_read_tags.rs`, `wav_read_tags.rs`, `locate.rs`, `mp3_locate.rs`, `wav_locate.rs`, `mp4_oracle.rs`, `roundtrip.rs`, `layout.rs` — decide **Tier-1 (byte-identical synthesis/splice) vs Tier-2 (tag/metadata read, locate helpers)** and record the rationale in §4. Synthesis/picture tests that assert served-byte identity are Tier-1 and **belong in the 3× flakiness set**; pure tag-read/locate tests are Tier-2. Do not drop a file just because Step 1 didn't name it.

- [ ] **Step 3: Write the finalized Tier-1 set into §4**

In §4, list the exact `file::test_name` set chosen for the 3× flakiness runs (reflecting the Step-2 triage), grouped as: byte-identical/read-path, resolution/freshness (incl. `external_contract.rs` and the `ogg_index` lib test), Tier-1 e2e. This is the authoritative set Task 5 reruns — **update the Task 5 `cargo test` invocations to match it** if the triage added Tier-1 format test files beyond the Step-1 subset.

---

## Task 4: Phase A — run the full test surface (counts & pass state)

**Files:**
- Modify: `docs/audits/2026-05-29-test-audit.md` (§3)
- Logs: `/tmp/musefs-audit/surface-*.log`

- [ ] **Step 1: Workspace run (category a — already includes fuzzing proptests)**

Run:
```bash
export PATH="$HOME/.cargo/bin:$PATH"
cargo test --workspace 2>&1 | tee /tmp/musefs-audit/surface-workspace.log
# Count tests ACTUALLY RUN (passed+failed), not --list: libtest --list also counts
# #[ignore]d tests, which cargo test --workspace does not run. Sum the run summaries:
grep -E '^test result:' /tmp/musefs-audit/surface-workspace.log \
  | awk '{p+=$4; f+=$6; ig+=$8} END{printf "ran(passed+failed)=%d passed=%d failed=%d ignored(not run)=%d\n", p+f, p, f, ig}' \
  | tee /tmp/musefs-audit/surface-workspace-count.log
```
Expected: all green; the printed `ran(passed+failed)` is the unique category-(a) count (the `proptest!` blocks each count as one libtest case, and the `fuzzing` proptests are included via feature unification; the `ignored` figure is reported separately, not added). **Record this number; do not reuse the spec's stale ~275.**

- [ ] **Step 2: FUSE e2e + metrics-gated runs (category b)**

Run:
```bash
export PATH="$HOME/.cargo/bin:$PATH"
cargo test -p musefs-fuse -- --ignored 2>&1 | tee /tmp/musefs-audit/surface-fuse-e2e.log
cargo test -p musefs-fuse --features metrics -- --ignored --test-threads=1 2>&1 | tee /tmp/musefs-audit/surface-fuse-metrics.log
cargo test -p musefs-core --features metrics --test metrics -- --test-threads=1 2>&1 | tee /tmp/musefs-audit/surface-core-metrics.log
```
Expected: e2e mounts pass on `/dev/fuse`; `concurrency.rs` runs only in the metrics build; core `metrics.rs` (4 tests) runs only via core's own metrics feature. Record pass/fail counts; note these are **unique** to category (b) (the metrics runs rerun other tests — count only the gated ones).

- [ ] **Step 3: mutagen interop (category b)**

Run:
```bash
export PATH="$HOME/.cargo/bin:$PATH"
D=$(mktemp -d)
MUSEFS_INTEROP_DIR="$D" cargo test -p musefs-core --test interop_emit -- --ignored emit_interop_fixtures 2>&1 | tee /tmp/musefs-audit/surface-interop-emit.log
MUSEFS_INTEROP_DIR="$D" /tmp/musefs-audit/venv/bin/python -m pytest tests/interop/test_mutagen_roundtrip.py 2>&1 | tee /tmp/musefs-audit/surface-interop-pytest.log
```
Expected: emitter writes fixtures into `$D`; pytest reads the same `$D` and passes. If `mutagen` was blocked in Task 2, record interop as blocked.

- [ ] **Step 4: Record §3 counts table**

Fill §3 with per-category command, unique count, and pass/fail/skip. (beets and fuzz rows filled in Tasks 6–7.)

---

## Task 5: Phase A — flakiness detection (Tier-1 ×3) and the red-test gate

**Files:**
- Modify: `docs/audits/2026-05-29-test-audit.md` (§4, §5)
- Logs: `/tmp/musefs-audit/flaky-*.log`

- [ ] **Step 1: Run the Tier-1 set 3× (unit/integration)**

Run (using the finalized set from Task 3; example invocation):
```bash
export PATH="$HOME/.cargo/bin:$PATH"
for i in 1 2 3; do
  echo "=== Tier-1 non-e2e run $i ===" | tee -a /tmp/musefs-audit/flaky-core.log
  cargo test -p musefs-core --test read_at --test reader --test proptest_read_fidelity \
    --test facade --test tree --test external_contract 2>&1 | tee -a /tmp/musefs-audit/flaky-core.log
  # The ogg_index Tier-1 test is an inline #[cfg(test)] in the lib, NOT an integration
  # test file — it must be run explicitly or the gate can falsely pass while it flakes:
  cargo test -p musefs-core --lib build_index_renumbers_and_preserves_payload_length \
    2>&1 | tee -a /tmp/musefs-audit/flaky-core.log
  cargo test -p musefs-format --features fuzzing --test layout --test roundtrip \
    --test synthesize_tags --test synthesize_art --test proptest_flac --test proptest_mp3 \
    --test proptest_mp4 --test proptest_ogg --test proptest_wav 2>&1 | tee -a /tmp/musefs-audit/flaky-format.log
done
```
Expected: identical pass results all 3 runs (including the `ogg_index` lib test). Any test that varies → record as **flaky** (`file:line`) in §4.

- [ ] **Step 2: Run the Tier-1 e2e mounts 3×, at both thread settings**

Run:
```bash
export PATH="$HOME/.cargo/bin:$PATH"
for i in 1 2 3; do
  echo "=== e2e run $i (threads=1) ===" | tee -a /tmp/musefs-audit/flaky-e2e.log
  cargo test -p musefs-fuse --test mount --test ogg_read_through --test keep_cache --test playback_pcm \
    -- --ignored --test-threads=1 2>&1 | tee -a /tmp/musefs-audit/flaky-e2e.log
  echo "=== e2e run $i (threads=default) ===" | tee -a /tmp/musefs-audit/flaky-e2e.log
  cargo test -p musefs-fuse --test mount --test ogg_read_through --test keep_cache --test playback_pcm \
    -- --ignored 2>&1 | tee -a /tmp/musefs-audit/flaky-e2e.log
done
```
Expected: stable across all runs/thread settings. Record any instability as flaky.

- [ ] **Step 3: Run the metrics-gated tests 3× (Tier-2, non-halting)**

Run:
```bash
export PATH="$HOME/.cargo/bin:$PATH"
for i in 1 2 3; do
  cargo test -p musefs-core --features metrics --test metrics -- --test-threads=1 2>&1 | tee -a /tmp/musefs-audit/flaky-metrics.log
  cargo test -p musefs-fuse --features metrics -- --ignored --test-threads=1 2>&1 | tee -a /tmp/musefs-audit/flaky-metrics.log
done
```
Expected: record any instability as a **Tier-2** flakiness finding; note whether it looks like a real concurrency bug or `METRICS_LOCK`/atomic-counter test-harness contention. This does **not** trip the gate.

- [ ] **Step 4: Record the red-test gate decision (§5) — but finish Phase A regardless**

The gate halts **Phase B/C only**, not Phase A. The rest of Phase A (beets, schema parity, fuzz, coverage — Tasks 6–8) must still run and be recorded, because the halt report in Task 13 depends on those §2–§8 sections being filled.

Decision:
- **If any Tier-1 test failed or was flaky** in Steps 1–2 → set §5 to **TRIP** and set the report's deliverable type to "red-test halt report".
- **Else** → set §5 to **PASS**.

Record the decision and its evidence (`file:line` of any offending test) in §5. **Then continue to Task 6 either way.** The actual branch (Phase B/C vs. halt-report assembly) is taken at Task 9, Step 3 — after Phase A is fully recorded and committed.

---

## Task 6: Phase A — beets suite & schema-parity check

**Files:**
- Modify: `docs/audits/2026-05-29-test-audit.md` (§3 beets row, §8)
- Logs: `/tmp/musefs-audit/beets-*.log`, `/tmp/musefs-audit/schema-*.log`

- [ ] **Step 1: Build the CLI binary and confirm success**

Run:
```bash
export PATH="$HOME/.cargo/bin:$PATH"
cargo build -p musefs-cli 2>&1 | tee /tmp/musefs-audit/beets-build.log
ls -l target/debug/musefs && echo "binary present"
```
Expected: `binary present`. **If the build fails, stop and surface it** (do not proceed to pytest as a "skip" — a failed build masking the `musefs_bin`/`e2e` suites must be reported as a build failure, not a skip).

- [ ] **Step 2: Run the three beets pytest invocations with cumulative coverage**

Run (use absolute paths in a subshell so this works regardless of the executor's cwd — pytest must run with `contrib/beets` as rootdir so `beetsplug` imports and the `pyproject.toml` markers/testpaths resolve):
```bash
REPO=$(git rev-parse --show-toplevel)
VENV=/tmp/musefs-audit/venv
( cd "$REPO/contrib/beets" && $VENV/bin/python -m pytest --cov=beetsplug -p no:cacheprovider ) 2>&1 | tee /tmp/musefs-audit/beets-default.log
( cd "$REPO/contrib/beets" && $VENV/bin/python -m pytest -m musefs_bin --cov=beetsplug --cov-append ) 2>&1 | tee /tmp/musefs-audit/beets-musefs_bin.log
( cd "$REPO/contrib/beets" && $VENV/bin/python -m pytest -m e2e --cov=beetsplug --cov-append ) 2>&1 | tee /tmp/musefs-audit/beets-e2e.log
```
Expected: record `passed/failed/skipped` per invocation **and the skip reason** for any skip (missing `beet`, `fusermount`, `ffmpeg`, `/dev/fuse`, or binary). **An all-skipped marker run is "not exercised," never green.** If `pytest-cov` was blocked, drop `--cov*` and record coverage as not measured.

- [ ] **Step 3: Schema-parity check (§8)**

Run:
```bash
# Normalizer: drop SQL line-comments (-- ...), collapse whitespace, drop blanks, sort.
norm() { sed -E 's/--.*$//; s/[[:space:]]+/ /g; s/^ //; s/ $//' | grep -v '^$' | sort; }
# Fixture: also strip the trailing PRAGMA user_version the Rust migration deliberately omits.
grep -v -i 'PRAGMA user_version' contrib/beets/tests/schema_v1.sql | norm > /tmp/musefs-audit/schema-fixture.norm
# Production: print ONLY the raw-string body — start after the `const MIGRATION_V1 ... r"` line,
# stop before the closing `";` line — so the `const`/delimiter lines never enter the diff.
awk '
  /const MIGRATION_V1/ {f=1; next}
  f && /^[[:space:]]*";[[:space:]]*$/ {f=0; next}
  f {print}
' musefs-db/src/schema.rs | norm > /tmp/musefs-audit/schema-prod.norm
diff /tmp/musefs-audit/schema-fixture.norm /tmp/musefs-audit/schema-prod.norm | tee /tmp/musefs-audit/schema-diff.log
```
Expected: with comments and the Rust `const`/`r"`/`";` delimiters stripped, the diff should be near-empty for an in-sync schema. It remains a coarse comparison (sorted lines) — read it by eye; the goal is to spot a missing/renamed column, table, trigger, or index. **If real drift exists**, record a **high-severity (P0)** finding in §8 and §11, mark beets results suspect, and add a "schema drift-detection test" backlog item. If clean, state so explicitly.

---

## Task 7: Phase A — fuzz smoke & corpus health

**Files:**
- Modify: `docs/audits/2026-05-29-test-audit.md` (§3 fuzz row, §7)
- Logs: `/tmp/musefs-audit/fuzz-*.log`

- [ ] **Step 1: Build all fuzz targets**

Run:
```bash
export PATH="$HOME/.cargo/bin:$PATH"
cargo +nightly fuzz build 2>&1 | tee /tmp/musefs-audit/fuzz-build.log
```
Expected: all targets build. **If nightly/cargo-fuzz is blocked, record the fuzz surface as not-measured and skip to Task 8.**

- [ ] **Step 2: Smoke-run each target (15s, CI bounds)**

Run:
```bash
export PATH="$HOME/.cargo/bin:$PATH"
for t in flac mp3 mp4 ogg wav ogg_page b64 vorbiscomment; do
  echo "== $t ==" | tee -a /tmp/musefs-audit/fuzz-smoke.log
  cargo +nightly fuzz run "$t" -- -max_len=131072 -rss_limit_mb=2048 -max_total_time=15 2>&1 \
    | tee -a /tmp/musefs-audit/fuzz-smoke.log
done
```
Expected: each target runs ~15s with no crash. Record any broken/unreachable target as a finding.

- [ ] **Step 3: Corpus health survey (time-boxed)**

Run:
```bash
export PATH="$HOME/.cargo/bin:$PATH"
# Per-target corpus sizes
for t in flac mp3 mp4 ogg wav ogg_page b64 vorbiscomment; do
  printf "%s: " "$t"; find "fuzz/corpus/$t" -type f 2>/dev/null | wc -l
done | tee /tmp/musefs-audit/fuzz-corpus-sizes.log
# Coverage reach on a representative subset (Tier-1 byte-surgery parsers)
for t in ogg mp4 flac; do
  echo "== coverage $t ==" | tee -a /tmp/musefs-audit/fuzz-coverage.log
  cargo +nightly fuzz coverage "$t" 2>&1 | tail -20 | tee -a /tmp/musefs-audit/fuzz-coverage.log
done
```
Expected: record corpus size per target; flag any subset target whose coverage looks shallow (doesn't reach the parser/synthesis code). **Do NOT run `cargo fuzz cmin` (it rewrites the corpus)** — only record a backlog recommendation to minimize redundant entries where sizes look bloated.

---

## Task 8: Phase A — coverage via cargo-llvm-cov

**Files:**
- Modify: `docs/audits/2026-05-29-test-audit.md` (§6)
- Logs: `/tmp/musefs-audit/coverage-*.log`

- [ ] **Step 1: Generate workspace coverage (matching CI)**

Run:
```bash
export PATH="$HOME/.cargo/bin:$PATH"
cargo llvm-cov --workspace --exclude musefs-fuse --summary-only 2>&1 | tee /tmp/musefs-audit/coverage-summary.log
cargo llvm-cov --workspace --exclude musefs-fuse --json --output-path /tmp/musefs-audit/coverage.json 2>&1 | tail -5
```
Expected: a per-file/region/line summary. This run instruments `cargo test --workspace`, so it **includes** the fuzzing-gated proptests and **excludes** `#[ignore]`d e2e and the `musefs-fuse` crate.

- [ ] **Step 2: Build the §6 per-crate / per-module table**

From the summary, fill §6 with line + region coverage per crate and per notable module (`reader.rs`, `tree.rs`, `scan.rs`, `facade.rs`, `ogg_index.rs`, the `musefs-format` synthesis/ogg files, `musefs-db/schema.rs`). Label the basis exactly as above. Note `musefs-fuse` is scored by e2e evidence, not coverage (FUSE coverage strategy).

---

## Task 9: Commit Phase A results

**Files:**
- Modify: `docs/audits/2026-05-29-test-audit.md`

- [ ] **Step 1: Verify §2–§8 are filled**

Confirm the report's environment, counts, Tier-1 set, flakiness, gate decision, coverage, fuzz, and schema-parity sections all contain real data (no `_pending_`).

- [ ] **Step 2: Commit**

```bash
git add docs/audits/2026-05-29-test-audit.md
git commit -m "audit: record Phase 0 + Phase A baseline (env, counts, flakiness, coverage, fuzz, schema parity)"
```

- [ ] **Step 3: Branch on the gate**

If §5 = **TRIP**, go to **Task 13** (red-test halt report). If §5 = **PASS**, continue to **Task 10**.

---

## Task 10: Phase B — mutation testing (skip if gate tripped)

**Files:**
- Modify: `docs/audits/2026-05-29-test-audit.md` (§9)
- Logs: `/tmp/musefs-audit/mutants-*.log`

> Skip this entire task if §5 = TRIP or cargo-mutants was blocked in Task 2 (set §9 to the blocked message and continue).

**Runtime risk to record up front:** `--test-workspace=true` (used for `musefs-db` and `musefs-core` below) runs the **whole workspace test suite per mutant** and rebuilds the mutated crate + its dependents under instrumentation. The one-time baseline build plus per-mutant incremental compiles can eat **10–15 min** of the 30-min cap before many mutants run, so **heavily partial runs for `musefs-db`/`musefs-core` are expected and acceptable** — record mutants-tested/total honestly. If the baseline build alone approaches the cap, note it in §9 and either narrow the `--file` set or flag a follow-up run with a higher cap rather than reporting a misleadingly low survivor count. (`cargo mutants` prints a baseline timing first; use it to gauge remaining budget.)

- [ ] **Step 1: Mutate musefs-db (workspace tests, plain command)**

Run:
```bash
export PATH="$HOME/.cargo/bin:$PATH"
timeout 1800 cargo mutants -p musefs-db --test-workspace=true \
  --timeout-multiplier 2.0 --minimum-test-timeout 60 \
  --file musefs-db/src/schema.rs --file musefs-db/src/lib.rs \
  2>&1 | tee /tmp/musefs-audit/mutants-db.log
```
Expected: a list of caught/missed/unviable mutants. Record survivors (`file:line` + mutation). If `timeout` kills it, mark **partial** (mutants tested / total).

- [ ] **Step 2: Mutate musefs-core (workspace tests)**

Run:
```bash
export PATH="$HOME/.cargo/bin:$PATH"
timeout 1800 cargo mutants -p musefs-core --test-workspace=true \
  --timeout-multiplier 2.0 --minimum-test-timeout 60 \
  --file musefs-core/src/reader.rs --file musefs-core/src/tree.rs \
  --file musefs-core/src/scan.rs --file musefs-core/src/facade.rs \
  --file musefs-core/src/ogg_index.rs \
  2>&1 | tee /tmp/musefs-audit/mutants-core.log
```
Expected: survivors recorded; partial if capped.

- [ ] **Step 3: Mutate musefs-format (crate-local, --features fuzzing, risk-ordered)**

Run (file order = Tier-1 risk first; the cap will likely truncate, which is why order matters):
```bash
export PATH="$HOME/.cargo/bin:$PATH"
timeout 1800 cargo mutants -p musefs-format --test-workspace=false \
  --timeout-multiplier 2.0 --minimum-test-timeout 60 --features fuzzing \
  --file musefs-format/src/ogg/mod.rs --file musefs-format/src/ogg/page.rs \
  --file musefs-format/src/ogg/crc.rs --file musefs-format/src/ogg/b64.rs \
  --file musefs-format/src/mp4.rs --file musefs-format/src/flac.rs \
  --file musefs-format/src/wav.rs --file musefs-format/src/mp3.rs \
  2>&1 | tee /tmp/musefs-audit/mutants-format.log
```
Expected: survivors recorded. **Almost certainly a partial run** — record mutants-tested/total and which files were reached. **Label these results crate-local-only** in §9.

- [ ] **Step 4: Record §9**

For each crate: invocation used (features + test-workspace setting), caught/missed/unviable totals (or partial counts), and each surviving mutant as `file:line` — mutation. Add the **flakiness caveat**: if any flaky test was found in a mutated crate (Task 5), mark that crate's killed-mutant counts lower-confidence and note re-running suspect mutants. Note `db_pool.rs` and the beets Python code are out of mutation scope by decision (db_pool assessed in Phase C; Python mutation deferred).

---

## Task 11: Phase C — judgment review (skip if gate tripped)

**Files:**
- Modify: `docs/audits/2026-05-29-test-audit.md` (§10, §11)
- Logs/notes: `/tmp/musefs-audit/phasec-notes.md`

> Skip if §5 = TRIP.

- [ ] **Step 1: Form your own thin-path candidates first (anti-anchoring)**

Before reading the named callouts, from the §6 coverage table and §9 survivors, write into `/tmp/musefs-audit/phasec-notes.md` your independent list of the thinly-tested Tier-1/Tier-2 paths you'd prioritize. Only then proceed to Steps 2–4 and reconcile.

- [ ] **Step 2: Inspect the two named callouts**

Read and assess:
- `musefs-core/src/ogg_index.rs:124` (`build_index_renumbers_and_preserves_payload_length`) — is the single inline case sufficient for Tier-1 Ogg correctness (continued pages, multi-page packets, EOS, CRC edges)? Do **not** claim "no direct test exists."
- `musefs-core/tests/proptest_read_fidelity.rs` (~47 lines) — does it actually span segment variants, offsets, partial reads, or just a happy path?

Record findings as `file:line` — description — severity.

- [ ] **Step 3: Audit the test infrastructure / oracles**

Read and assess each, recording findings:
- `musefs-format/tests/common/mod.rs` — `resolve_layout()` reimplements the production splicer (oracle); confirm the `unreachable!()` at `mod.rs:80-81` means it cannot oracle `OggAudio`/`OggArtSlice`, and determine whether Ogg correctness has any independent oracle elsewhere (if not → Tier-1 gap).
- `musefs-core/tests/common/mod.rs` — correctness of `write_flac()` / `minimal_m4a()` fixture builders.
- `contrib/beets/tests/conftest.py` — the `db_path` fixture's missing `PRAGMA foreign_keys = ON` vs production (`musefs-db/src/lib.rs:44`) and the `make_track`/`musefs_connect()` path; flag the fixture/prod parity gap.
- `musefs-format/src/fuzz_check.rs` — quick correctness glance (feeds `external_contract.rs` + fuzz seeds).
- `musefs-core/src/db_pool.rs` (3 unit tests) — per-worker WAL connections; check for stale-connection / missing-`foreign_keys`-pragma risks on worker connections.

- [ ] **Step 4: Walk the edge-case checklist per Tier-1/Tier-2 area**

For each area, mark each condition covered / partial / missing: empty/truncated/very-large files; malformed/out-of-spec headers; multi-value & Unicode tags; path collisions & disambiguation; concurrent refresh during in-flight read; backing file modified between `open()` and `read()` (→ `BackingChanged`); NFS-style `ESTALE` on a backing read; zero-byte & oversized embedded art; chained/multiplexed Ogg skip (confirm tested); synthesis vs structure-only mode boundaries.

- [ ] **Step 5: Fill §10 scorecard and §11 findings**

Build §10 (Coverage | Quality | Edge cases per area, stating the basis for FUSE-only and beets areas) and §11 (all findings, uniform `file:line` — description — severity format). Reconcile your Step-1 candidate list against the callouts and note anything they missed.

---

## Task 12: Assemble the prioritized backlog and executive summary

**Files:**
- Modify: `docs/audits/2026-05-29-test-audit.md` (§1, §12)

> For the full-audit path. (Red-test path uses Task 13.)

- [ ] **Step 1: Write §12 — prioritized remediation backlog**

From §11 findings, produce a P0/P1/P2 backlog. **Each item names the target test file and exactly what to add or fix** (e.g. "Add `musefs-format/tests/` Ogg oracle independent of `resolve_layout`; assert byte-identity across `OggAudio`/`OggArtSlice`"). Order P0 → P2.

- [ ] **Step 2: Write §1 — executive summary**

Overall health, the re-derived headline counts (per-category), top risks, and whether the byte-identical invariant's test backing is adequate. Set report status to "complete (full audit)".

- [ ] **Step 3: Commit and finish**

```bash
git add docs/audits/2026-05-29-test-audit.md
git commit -m "audit: complete Phase B/C, scorecard, findings, and remediation backlog"
```
Then go to **Task 14**.

---

## Task 13: Red-test halt report (only if §5 = TRIP)

**Files:**
- Modify: `docs/audits/2026-05-29-test-audit.md`

- [ ] **Step 1: Mark the deliverable type and Quality column**

Set report status to "complete (red-test halt report)" and the deliverable type line to "red-test halt report". In §9 and §10, the Quality column reads **"blocked — suite not green"**; state explicitly that Phase B/C mutation/judgment depth was **not reached**.

- [ ] **Step 2: Write the narrowed P0 backlog**

In §12, list the failing/flaky Tier-1 tests (`file:line` + a minimal repro from the Task 5 logs) and make the **first P0 items "make Tier-1 green/stable"**, with Phase B/C deferred until then. Keep the §2–§8 Phase A data already recorded (coverage, fuzz, beets, schema parity all still valid).

- [ ] **Step 3: Write §1 executive summary (halt framing)**

Summarize: audit halted at the Phase A gate, why, the Phase A data obtained, and the path back (fix Tier-1, then resume B/C).

- [ ] **Step 4: Commit**

```bash
git add docs/audits/2026-05-29-test-audit.md
git commit -m "audit: red-test halt report (Tier-1 not green; B/C deferred)"
```

---

## Task 14: Final verification

**Files:**
- Read: `docs/audits/2026-05-29-test-audit.md`

- [ ] **Step 1: Placeholder scan**

Run:
```bash
grep -n '_pending_' docs/audits/2026-05-29-test-audit.md || echo "no placeholders remain"
```
Expected: `no placeholders remain` (every `_pending_` replaced, or explicitly marked "blocked"/"not measured"/"deferred" with a reason).

- [ ] **Step 2: Findings-format check**

Confirm every entry in §11 (and §12 source findings) uses `` `file:line` — description — severity (P0/P1/P2) `` and that every backlog item names a target test file. Fix any that don't, then re-commit if changed.

- [ ] **Step 3: Confirm scope was respected**

Run:
```bash
git diff --name-only main...HEAD
```
Expected: only `docs/audits/2026-05-29-test-audit.md` (and this plan). **If any non-test production file changed, that violates the non-goals — revert it.**

---

## Self-review against the spec

- **Phase 0** → Task 1 (scaffold) + Task 2 (tools, mutants pin 27.0.0, beets venv + extras, CI-divergence note, network fallback). ✔
- **Phase A enumerate Tier-1 set** → Task 3. ✔
- **Phase A full surface + unique counting** → Task 4 (workspace/fuzzing-unification note, metrics-gated separate runs, interop two-step shared `$D`). ✔
- **Flakiness ×3 + metrics-gated ×3 + red-test gate** → Task 5. ✔
- **beets suite (build-success-first, skips≠pass, --cov-append) + schema parity (strip PRAGMA user_version)** → Task 6. ✔
- **Fuzz smoke (15s CI bounds) + corpus health (non-destructive, no cmin)** → Task 7. ✔
- **Coverage (default-CI basis, includes fuzzing proptests, excludes e2e + musefs-fuse; FUSE e2e-evidence)** → Task 8 / §6. ✔
- **Phase B (db/core --test-workspace, format crate-local + --features fuzzing, risk-ordered, 30-min cap/partial, db_pool & Python out of scope, flakiness caveat)** → Task 10. ✔
- **Phase C (candidates-first anti-anchoring, two callouts, test-infra/oracles incl. resolve_layout Ogg gap & conftest FK gap & db_pool, edge-case checklist)** → Task 11. ✔
- **Deliverable structure (exec summary, scorecard, findings, backlog) + red-test alternate** → Tasks 12 & 13; report skeleton in Task 1. ✔
- **Time budget / escalation** → covered by the spec's budget; executor escalates per the >1.5× rule (note in §1 if any phase overran). ✔
- **Non-goals (no prod changes, no remediation tests, Tier-3 light, no CI changes)** → enforced in Conventions + Task 14 Step 3. ✔
