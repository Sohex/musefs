# Phase 1 — Quick Fixes & Mutation-Discovery Harness Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Unblock the `metrics` test, close the beets foreign-key parity gap, and stand up a reusable mutation-testing harness (script + CI) that produces a verified survivor inventory for remediation phases 2–4.

**Architecture:** Two trivially-correct code fixes (one Rust struct-literal deletion, one Python connection-routing change) plus a CI/script harness modeled on the existing `.github/workflows/fuzz.yml` (fast per-PR `--in-diff` job + scheduled/dispatchable full matrix). The heavy mutation campaign runs on CI (disk headroom); local runs are limited to `--list` smoke checks because the VPS has only ~7.3 GB free.

**Tech Stack:** Rust (cargo-mutants 27.0.0, nightly for `llvm-tools-preview`), Python/pytest (beets plugin tests), GitHub Actions, Bash.

**Spec:** `docs/superpowers/specs/test-audit-remediation/2026-05-29-phase1-fixes-mutation-harness-design.md`
**Tracking:** `docs/superpowers/specs/test-audit-remediation/2026-05-29-remediation-tracking.md`

---

## File structure

| File | Responsibility | Action |
|------|----------------|--------|
| `musefs-core/tests/metrics.rs` | Remove stale `backing_mtime_secs` field from the `NewTrack` literal | Modify |
| `contrib/beets/tests/test_db.py` | Add FK characterization test (raw conn = off, `connect()` = on) | Modify |
| `contrib/beets/tests/test_plugin.py` | Route 6 raw `sqlite3.connect` calls through `_core.connect()` | Modify |
| `scripts/mutants.sh` | Canonical cargo-mutants invocation (disk-budgeted, per-crate) | Create |
| `.gitignore` | Ignore mutation scratch + output dirs | Modify |
| `.github/workflows/mutants.yml` | PR `--in-diff` job + scheduled/dispatchable full matrix | Create |
| `docs/superpowers/specs/test-audit-remediation/2026-05-29-mutation-inventory.md` | Verified survivor inventory (seeded from CI) | Create |

---

## Task 1: Fix the `metrics.rs` compile error (A1)

**Files:**
- Modify: `musefs-core/tests/metrics.rs:177`

The `NewTrack` struct (`musefs-db/src/models.rs:80`) has a single `backing_mtime: i64` field and no `backing_mtime_secs`. The test literal at `metrics.rs:171-179` contains **both** `backing_mtime_secs: 0,` (line 177) and `backing_mtime: 0,` (line 178), so the file fails to compile and blocks all 4 metrics tests. Fix = delete line 177.

- [ ] **Step 1: Run the metrics tests to observe the compile failure**

Run:
```bash
cargo test -p musefs-core --features metrics --test metrics -- --test-threads=1
```
Expected: FAIL — compile error `struct NewTrack has no field named backing_mtime_secs` at `musefs-core/tests/metrics.rs:177`.

- [ ] **Step 2: Delete the stale field line**

Remove this exact line (177) from the `NewTrack { ... }` literal:
```rust
                backing_mtime_secs: 0,
```
The literal should then read:
```rust
            .upsert_track(&NewTrack {
                backing_path: "/x/ghost.mp3".to_string(),
                format: Format::Mp3,
                audio_offset: 0,
                audio_length: 0,
                backing_size: 0,
                backing_mtime: 0,
            })
```

- [ ] **Step 3: Run the metrics tests to verify they pass**

Run:
```bash
cargo test -p musefs-core --features metrics --test metrics -- --test-threads=1
```
Expected: PASS — 4 passed; 0 failed.

- [ ] **Step 4: Commit**

```bash
git add musefs-core/tests/metrics.rs
git commit -m "fix(test): drop stale backing_mtime_secs from metrics NewTrack literal

The NewTrack struct has only backing_mtime; the test literal carried both
fields, blocking compilation of all 4 metrics tests (audit finding #13).

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Task 2: beets FK characterization test (A2, part 1)

**Files:**
- Modify: `contrib/beets/tests/test_db.py`

Add a test that documents the exact gap: a raw `sqlite3.connect()` reports
`PRAGMA foreign_keys = 0` (off) while `_core.connect()` reports `1` (on). This
locks in why test code must use `connect()`. `test_db.py` already imports both
`sqlite3` and `connect`.

- [ ] **Step 1: Add the failing-then-passing characterization test**

Append to `contrib/beets/tests/test_db.py`:
```python
def test_connect_enables_foreign_keys_unlike_raw_sqlite(db_path):
    # Raw sqlite3 connections default foreign_keys OFF; _core.connect() turns
    # them ON. Test code that opens raw connections silently loses FK
    # enforcement, so it must route through connect() (audit finding #6).
    raw = sqlite3.connect(db_path)
    try:
        assert raw.execute("PRAGMA foreign_keys").fetchone()[0] == 0
    finally:
        raw.close()

    conn = connect(db_path)
    try:
        assert conn.execute("PRAGMA foreign_keys").fetchone()[0] == 1
    finally:
        conn.close()
```

- [ ] **Step 2: Run the new test**

Run:
```bash
cd contrib/beets && .venv/bin/python -m pytest tests/test_db.py::test_connect_enables_foreign_keys_unlike_raw_sqlite -v
```
Expected: PASS (this is a characterization test — it documents existing correct
`connect()` behavior and the raw-connection default; both assertions hold today).

- [ ] **Step 3: Commit**

```bash
git add contrib/beets/tests/test_db.py
git commit -m "test(beets): characterize FK on connect() vs raw sqlite3

Documents the gap behind audit finding #6: raw sqlite3 connections default
foreign_keys OFF, _core.connect() turns them ON.

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Task 3: Route test_plugin.py raw connections through connect() (A2, part 2)

**Files:**
- Modify: `contrib/beets/tests/test_plugin.py:10` (import)
- Modify: `contrib/beets/tests/test_plugin.py` (6 call sites: 115, 141, 181, 197, 219, 242)

These 6 sites open `sqlite3.connect(db_path)` directly, bypassing FK enforcement
that production (`_core.connect`, `_core.py:127`) applies. Route them through
`connect()`.

- [ ] **Step 1: Extend the import on line 10**

Change:
```python
from beetsplug._core import map_fields  # noqa: E402
```
to:
```python
from beetsplug._core import connect, map_fields  # noqa: E402
```

- [ ] **Step 2: Replace each raw connection**

At each of the 6 sites (originally lines 115, 141, 181, 197, 219, 242), replace:
```python
    conn = sqlite3.connect(db_path)
```
with:
```python
    conn = connect(db_path)
```
Use a global replace of the exact string `conn = sqlite3.connect(db_path)` →
`conn = connect(db_path)` within `test_plugin.py`. Leave the `import sqlite3` line
intact only if another `sqlite3.` reference remains; otherwise remove it.

- [ ] **Step 3: Check whether `import sqlite3` is now unused**

Run:
```bash
cd contrib/beets && grep -n "sqlite3\." tests/test_plugin.py
```
If no matches remain, delete the `import sqlite3` line at the top of
`test_plugin.py` (ruff would flag F401 otherwise).

- [ ] **Step 4: Run the full beets suite + lint**

Run:
```bash
cd contrib/beets && .venv/bin/python -m pytest && ruff check tests/test_plugin.py
```
Expected: all tests pass; ruff reports no errors.

- [ ] **Step 5: Commit**

```bash
git add contrib/beets/tests/test_plugin.py
git commit -m "test(beets): route test_plugin connections through _core.connect

The 6 raw sqlite3.connect(db_path) sites bypassed foreign_keys enforcement that
production applies via _core.connect (audit finding #6).

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Task 4: Ignore mutation scratch/output dirs

**Files:**
- Modify: `.gitignore`

Current `.gitignore` ignores `/target`, `/.claude/`, `.worktrees/`,
`__pycache__/`, `*.pyc`. Add the mutation paths so neither scratch `TMPDIR` nor
reports get committed. A stray `mutants.out.old/` is already in the working tree
from the audit.

- [ ] **Step 1: Append the mutation ignore block**

Add to `.gitignore`:
```gitignore

# cargo-mutants scratch + reports
.mutants-tmp/
mutants-out/
mutants.out/
mutants.out.old/
```

- [ ] **Step 2: Verify the stray dir is now ignored**

Run:
```bash
git check-ignore mutants.out.old/ && git status --short
```
Expected: `git check-ignore` prints `mutants.out.old/`; `git status` no longer
shows `mutants.out.old/` as untracked.

- [ ] **Step 3: Commit**

```bash
git add .gitignore
git commit -m "chore: gitignore cargo-mutants scratch and report dirs

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Task 5: Create `scripts/mutants.sh`

**Files:**
- Create: `scripts/mutants.sh`

Canonical, disk-budgeted cargo-mutants invocation for the three in-scope crates.
Mirrors the env learnings from audit §9. Uses an isolated `--target-dir` wiped
between crates (keeps the primary `target/` dep cache intact), `--jobs 1`, no time
cap, and a no-`set -e` loop so one crate's failure doesn't abort the rest.

- [ ] **Step 1: Write the script**

Create `scripts/mutants.sh`:
```bash
#!/usr/bin/env bash
# Run cargo-mutants over the three logic-bearing crates with a disk budget that
# fits a small VPS. Known-good cargo-mutants version: 27.0.0.
#
# musefs-cli and musefs-fuse are intentionally out of scope (thin glue / e2e-only;
# see the remediation tracking doc).
#
# Usage: scripts/mutants.sh [crate ...]   (default: all three in-scope crates)
# Env:   MUTANTS_TMP  scratch PARENT dir off the /tmp tmpfs (default: ./.mutants-tmp).
#                     cargo-mutants builds inside a unique child we create here;
#                     a caller-provided parent is never deleted, only our children.
#        MUTANTS_LIST set to 1 to only enumerate mutants (no build/run)
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

# Scratch parent. If the caller supplied MUTANTS_TMP we treat it as a shared
# parent and must NOT remove it on exit (could be /tmp or another shared dir);
# we only ever remove the unique children we mktemp inside it. Our own default
# repo-local parent we do clean up.
if [ -n "${MUTANTS_TMP:-}" ]; then
  SCRATCH_PARENT="$MUTANTS_TMP"; OWN_PARENT=0
else
  SCRATCH_PARENT="$ROOT/.mutants-tmp"; OWN_PARENT=1
fi
mkdir -p "$SCRATCH_PARENT"
cleanup() { [ "$OWN_PARENT" = 1 ] && rm -rf "$SCRATCH_PARENT"; }
trap cleanup EXIT

OUT_ROOT="$ROOT/mutants-out"

# Per-crate args. --test-workspace=true for musefs-db: its dependents' tests are
# cheap to build, so workspace-wide checking buys stronger mutant detection.
# =false for core/format: workspace mode pulls in criterion/proptest scratch
# builds that blew the disk/time budget in the audit; crate-local tests suffice.
#
# cargo-mutants 27.0.0 has no --target-dir; it builds inside a copy of the tree
# under TMPDIR. We point TMPDIR at a unique per-crate child so peak disk is one
# build tree at a time, removed before the next crate.
run_crate() {
  local crate="$1"; shift
  local out="$OUT_ROOT/$crate"
  local tmp
  tmp="$(mktemp -d "$SCRATCH_PARENT/${crate}.XXXXXX")"
  echo "== mutants: $crate (scratch: $tmp) =="
  local list_flag=""
  [ "${MUTANTS_LIST:-0}" = "1" ] && list_flag="--list"
  TMPDIR="$tmp" cargo mutants -p "$crate" \
    --jobs 1 \
    --output "$out" \
    $list_flag "$@"
  local rc=$?
  rm -rf "$tmp"
  return "$rc"
}

status=0

crates=("$@")
[ "${#crates[@]}" -eq 0 ] && crates=(musefs-db musefs-core musefs-format)

# Collect every crate's result; do NOT abort on the first failure so the
# inventory stays complete. Exit non-zero at the end if any crate failed.
for crate in "${crates[@]}"; do
  case "$crate" in
    musefs-db)
      run_crate musefs-db --test-workspace=true \
        --file musefs-db/src/schema.rs \
        --file musefs-db/src/lib.rs \
        --file musefs-db/src/tracks.rs \
        --file musefs-db/src/art.rs \
        --file musefs-db/src/tags.rs
      ;;
    musefs-core)
      run_crate musefs-core --test-workspace=false \
        --file musefs-core/src/reader.rs \
        --file musefs-core/src/tree.rs \
        --file musefs-core/src/scan.rs \
        --file musefs-core/src/facade.rs \
        --file musefs-core/src/ogg_index.rs
      ;;
    musefs-format)
      run_crate musefs-format --test-workspace=false --features fuzzing \
        --file musefs-format/src/flac.rs \
        --file musefs-format/src/mp3.rs \
        --file musefs-format/src/mp4.rs \
        --file musefs-format/src/wav.rs \
        --file musefs-format/src/ogg/mod.rs \
        --file musefs-format/src/ogg/page.rs \
        --file musefs-format/src/ogg/crc.rs \
        --file musefs-format/src/ogg/b64.rs
      ;;
    *)
      echo "unknown crate: $crate" >&2; status=1; continue
      ;;
  esac
  # cargo-mutants exits non-zero when mutants survive (2) or on error (>2).
  rc=$?
  [ "$rc" -ne 0 ] && status=$rc
done

exit "$status"
```

- [ ] **Step 2: Make it executable**

Run:
```bash
chmod +x scripts/mutants.sh
```

- [ ] **Step 3: Smoke-test with `--list` (no build, fits disk)**

Run:
```bash
MUTANTS_LIST=1 scripts/mutants.sh musefs-db
```
Expected: cargo-mutants prints the list of mutants it *would* test for
`musefs-db` (no compilation, no disk pressure) and exits 0. This verifies the
invocation wiring without running the full campaign.

- [ ] **Step 4: Commit**

```bash
git add scripts/mutants.sh
git commit -m "build(mutants): add scripts/mutants.sh disk-budgeted harness

Per-crate cargo-mutants invocation for the three in-scope crates with isolated
--target-dir, --jobs 1, no time cap, and a fail-at-end (not fail-fast) loop.

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Task 6: Create `.github/workflows/mutants.yml`

**Files:**
- Create: `.github/workflows/mutants.yml`

PR job runs `cargo mutants --in-diff` on changed Rust lines (scoped by `paths:`
trigger like `fuzz.yml`). Scheduled + `workflow_dispatch` job runs the full
per-crate matrix via `scripts/mutants.sh` and uploads survivor reports.

- [ ] **Step 1: Write the workflow**

Create `.github/workflows/mutants.yml`:
```yaml
name: Mutants

on:
  pull_request:
    paths:
      - 'musefs-db/**'
      - 'musefs-core/**'
      - 'musefs-format/**'
      - 'scripts/mutants.sh'
      - '.github/workflows/mutants.yml'
  schedule:
    - cron: '0 4 * * 1'  # Mondays 04:00 UTC
  workflow_dispatch:

concurrency:
  group: mutants-${{ github.ref }}
  cancel-in-progress: true

permissions:
  contents: read

jobs:
  in-diff:
    # Per-PR: mutate only the lines changed in this PR. Fast gate.
    if: github.event_name == 'pull_request'
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@de0fac2e4500dabe0009e67214ff5f5447ce83dd
        with:
          persist-credentials: false
          # Full history so the merge base for the three-dot diff is present;
          # a shallow clone often lacks it and `git diff base...HEAD` then fails.
          fetch-depth: 0
      - uses: dtolnay/rust-toolchain@29eef336d9b2848a0b548edc03f92a220660cdb8
        with:
          toolchain: stable
      - uses: Swatinem/rust-cache@e18b497796c12c097a38f9edb9d0641fb99eee32
      - name: Install cargo-mutants
        # Unpinned (like fuzz.yml installs cargo-fuzz) for toolchain-compat;
        # known-good version is 27.0.0 (documented in scripts/mutants.sh).
        run: cargo install cargo-mutants
      - name: Build the merge-base diff
        env:
          BASE_SHA: ${{ github.event.pull_request.base.sha }}
        run: |
          git diff "$BASE_SHA...HEAD" -- '*.rs' > mutants.diff
          echo "Changed Rust lines:"; wc -l mutants.diff
      - name: Mutate changed lines
        run: |
          if [ ! -s mutants.diff ]; then
            echo "No in-scope Rust changes; nothing to mutate."; exit 0
          fi
          cargo mutants --in-diff mutants.diff -j1

  full:
    # Scheduled or manually dispatched: full per-crate campaign.
    if: github.event_name != 'pull_request'
    runs-on: ubuntu-latest
    strategy:
      fail-fast: false
      matrix:
        crate: [musefs-db, musefs-core, musefs-format]
    steps:
      - uses: actions/checkout@de0fac2e4500dabe0009e67214ff5f5447ce83dd
        with:
          persist-credentials: false
      - uses: dtolnay/rust-toolchain@29eef336d9b2848a0b548edc03f92a220660cdb8
        with:
          toolchain: nightly
          components: llvm-tools-preview
      - uses: Swatinem/rust-cache@e18b497796c12c097a38f9edb9d0641fb99eee32
      - name: Install cargo-mutants
        # Unpinned (like fuzz.yml installs cargo-fuzz) for toolchain-compat;
        # known-good version is 27.0.0 (documented in scripts/mutants.sh).
        run: cargo install cargo-mutants
      - name: Run mutation campaign
        run: scripts/mutants.sh ${{ matrix.crate }}
      - name: Upload survivor report
        if: always()
        uses: actions/upload-artifact@ea165f8d65b6e75b540449e92b4886f43607fa02
        with:
          name: mutants-${{ matrix.crate }}
          path: mutants-out/${{ matrix.crate }}
          if-no-files-found: warn
```

- [ ] **Step 2: Validate YAML syntax**

Run:
```bash
python3 -c "import yaml; yaml.safe_load(open('.github/workflows/mutants.yml'))" && echo OK
```
Expected: `OK` (no YAML parse error).

- [ ] **Step 3: Commit**

```bash
git add .github/workflows/mutants.yml
git commit -m "ci(mutants): add PR --in-diff gate + scheduled full campaign

Mirrors fuzz.yml: path-scoped per-PR --in-diff job and a weekly/dispatchable
per-crate matrix that runs scripts/mutants.sh and uploads survivor reports.

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Task 7: Seed the verified survivor inventory

**Files:**
- Create: `docs/superpowers/specs/test-audit-remediation/2026-05-29-mutation-inventory.md`

The full campaign runs on CI (local disk is too small). This task creates the
inventory skeleton, then fills it from a dispatched CI run. The skeleton is
committed first so phases 2–4 have a known path even before the run completes.

- [ ] **Step 1: Create the inventory skeleton**

Create `docs/superpowers/specs/test-audit-remediation/2026-05-29-mutation-inventory.md`:
```markdown
# Mutation Survivor Inventory

**Source:** `mutants.yml` `full` job (CI). Supersedes the audit's partial §9
(which only reached `flac.rs`).
**Scope:** `musefs-db`, `musefs-core`, `musefs-format`. `musefs-cli` /
`musefs-fuse` out of scope by decision (see remediation tracking doc).
**Run:** _PENDING — fill from the first dispatched CI run._

## How to (re)generate

1. Trigger the campaign: GitHub → Actions → **Mutants** → **Run workflow**
   (`workflow_dispatch`), or wait for the Monday cron.
2. Download the `mutants-<crate>` artifacts from the run.
3. Transcribe `caught.txt` / `missed.txt` / `unviable.txt` / `timeout.txt` per
   crate into the tables below.

## Tool limitations to revisit (phase 4)

- `musefs-db`: every mutant replaces a body with `Ok(Default::default())` /
  `Ok(0|1|-1)`; `Db` has no `Default`, so all are unviable. Implementing
  `Default for Db` (phase 4) makes db mutation testing meaningful.
- A few `musefs-format` mutants share the `Ok(Default::default())` unviable
  pattern.

## musefs-db

| File | Caught | Missed | Unviable | Timeout | Notes |
|------|-------:|-------:|---------:|--------:|-------|
| _pending_ | | | | | |

### Surviving mutants → phase

| File:line | Mutation | Phase |
|-----------|----------|------:|
| _pending_ | | |

## musefs-core

| File | Caught | Missed | Unviable | Timeout | Notes |
|------|-------:|-------:|---------:|--------:|-------|
| _pending_ | | | | | |

### Surviving mutants → phase

| File:line | Mutation | Phase |
|-----------|----------|------:|
| _pending_ | | 4 |

## musefs-format

| File | Caught | Missed | Unviable | Timeout | Notes |
|------|-------:|-------:|---------:|--------:|-------|
| _pending_ | | | | | |

### Surviving mutants → phase

| File:line | Mutation | Phase |
|-----------|----------|------:|
| _pending_ (ogg/*) | | 2 |
| _pending_ (flac/mp3/mp4/wav) | | 3 |
```

- [ ] **Step 2: Commit the skeleton**

```bash
git add docs/superpowers/specs/test-audit-remediation/2026-05-29-mutation-inventory.md
git commit -m "docs(remediation): add mutation inventory skeleton

Authoritative survivor list for phases 2-4; filled from the first dispatched
mutants.yml CI run.

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

- [ ] **Step 3: Trigger the CI campaign and wait**

After the branch with `mutants.yml` is pushed, trigger the `full` job via
`workflow_dispatch` (Actions → Mutants → Run workflow) on that branch. The run
takes a while (the audit's was capped at 30 min and only reached `flac.rs`; this
runs uncapped per crate in parallel). Wait for all three matrix legs to finish.

Expected: three `mutants-<crate>` artifacts available on the run.

- [ ] **Step 4: Fill the inventory from the artifacts**

Download each `mutants-<crate>` artifact and transcribe its
`caught.txt`/`missed.txt`/`unviable.txt`/`timeout.txt` into the per-crate tables.
Tag each surviving mutant with its remediation phase (2 = ogg/ogg_index, 3 =
flac/mp3/mp4/wav non-ogg, 4 = reader/scan/facade/tree/db). Set the **Run:** line
to the run URL + date.

- [ ] **Step 5: Commit the filled inventory**

```bash
git add docs/superpowers/specs/test-audit-remediation/2026-05-29-mutation-inventory.md
git commit -m "docs(remediation): fill mutation survivor inventory from CI run

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Task 8: Mark phase 1 complete in the tracking doc

**Files:**
- Modify: `docs/superpowers/specs/test-audit-remediation/2026-05-29-remediation-tracking.md`

- [ ] **Step 1: Update the Phase 1 status line**

Change:
```markdown
### Phase 1 — Quick fixes & mutation-discovery harness  ⟶ STATUS: spec
```
to:
```markdown
### Phase 1 — Quick fixes & mutation-discovery harness  ⟶ STATUS: done
```

- [ ] **Step 2: Commit**

```bash
git add docs/superpowers/specs/test-audit-remediation/2026-05-29-remediation-tracking.md
git commit -m "docs(remediation): mark phase 1 done

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Final verification

- [ ] `cargo test -p musefs-core --features metrics --test metrics -- --test-threads=1` → 4 pass (Task 1)
- [ ] `cd contrib/beets && .venv/bin/python -m pytest` → all green, incl. the FK characterization test (Tasks 2–3)
- [ ] `MUTANTS_LIST=1 scripts/mutants.sh musefs-db` → lists mutants, exit 0 (Task 5)
- [ ] `git check-ignore mutants.out.old/` → printed (Task 4)
- [ ] `python3 -c "import yaml; yaml.safe_load(open('.github/workflows/mutants.yml'))"` → OK (Task 6)
- [ ] Inventory doc exists and is filled from a real CI run (Task 7)
- [ ] Tracking doc shows Phase 1 done (Task 8)
