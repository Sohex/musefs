# Phase 1 — Quick Fixes & Mutation-Discovery Harness

**Part of:** Test-audit remediation (`docs/superpowers/specs/test-audit-remediation/2026-05-29-remediation-tracking.md`)
**Source audit:** `docs/audits/2026-05-29-test-audit.md`
**Date:** 2026-05-29

## Purpose

Two goals:

1. Unblock the test suite by fixing the broken `metrics` test and the beets FK
   parity gap — both correctly, since the audit's prescriptions for both are
   wrong.
2. Stand up a durable mutation-testing harness (script + CI, modeled on the
   existing `fuzz.yml`) and use it to produce a **verified** survivor inventory
   that drives remediation phases 2–4.

## Guiding principle

The audit was executed by a weaker model. Findings are leads, not facts. Every
change here was verified against the live code first; the two corrected fixes
below are the evidence and the precedent for later phases.

## Component A — Corrected quick fixes

### A1. `metrics.rs` compile error (Finding #13)

**Audit said:** rename `backing_mtime_secs` → `backing_mtime` at
`musefs-core/tests/metrics.rs:177`.

**Reality:** `NewTrack` (`musefs-db/src/models.rs:80`) has a single
`backing_mtime: i64` field and no `backing_mtime_secs`. The test literal at
`metrics.rs:177-178` already contains **both** `backing_mtime_secs: 0,` and
`backing_mtime: 0,`. Renaming would create a duplicate field — still a compile
error.

**Fix:** delete line 177 (`backing_mtime_secs: 0,`).

**Verification:**
```
cargo test -p musefs-core --features metrics --test metrics -- --test-threads=1
```
All 4 metrics tests compile and pass.

### A2. beets foreign-key parity (Finding #6)

**Audit said:** add `PRAGMA foreign_keys = ON` to the `db_path` fixture
(`contrib/beets/tests/conftest.py:16`).

**Reality:** the `db_path` fixture opens a connection only to apply the schema,
then closes it (`conftest.py:16-19`). `foreign_keys` is a per-connection pragma,
so setting it on that throwaway connection has no effect on tests that later open
their own connection. The actual gap is in `test_plugin.py`, which opens raw
`sqlite3.connect(db_path)` at 6 sites (115, 141, 181, 197, 219, 242), bypassing
FK enforcement. `_core.connect()` already enables FK (`_core.py:127`).

**Fix:** route those 6 raw connections through `beetsplug._core.connect()`.
`test_plugin.py:10` currently imports only `map_fields`; extend it to
`from beetsplug._core import connect, map_fields`, then replace each
`sqlite3.connect(db_path)` with `connect(db_path)`. Add one regression assertion
that a `connect()`-opened connection reports `PRAGMA foreign_keys` = 1.

**Verification:**
```
cd contrib/beets && pytest
```
Full beets suite stays green; the new FK assertion passes.

## Component B — Mutation harness

Mirrors the structure of `.github/workflows/fuzz.yml`: a fast per-PR job plus a
long-running scheduled (and manually dispatchable) campaign.

### B1. `scripts/mutants.sh`

Single canonical entry point for local and CI use. Mutation scope is the three
logic-bearing crates only — `musefs-db`, `musefs-core`, `musefs-format`;
`musefs-cli` and `musefs-fuse` are out of scope by decision (see the tracking
doc's Scope section). Responsibilities:

- **Version pin:** invoke a known-good `cargo-mutants` (the audit used 27.0.0).
  Like `fuzz.yml` with `cargo-fuzz`, CI installs it unpinned for nightly-compat
  reasons, but the script and the spec record 27.0.0 as the known-good version
  for reproducibility; bump deliberately.
- Set `TMPDIR` to a path off the `/tmp` tmpfs (the audit exhausted the 3.9 GB
  tmpfs). Accept an override env var; default to a repo-local `.mutants-tmp/`
  that callers can place on the roomiest volume.
- **Disk budget without nuking the dep cache:** do *not* `cargo clean` the working
  `target/`. Instead give cargo-mutants its own isolated build dir
  (`--target-dir target/mutants` or equivalent under `TMPDIR`) and `rm -rf` that
  dir between crates. Peak extra disk is one isolated tree at a time, and the
  primary `target/` (with its compiled deps, ~minutes to rebuild) is left intact.
  (Local disk is tight: 7.3 GB free, `target/` ~5.6 GB.)
- Run **one crate at a time**, `--jobs 1`, so peak disk is a single mutants tree.
- **Error-handling contract:** run with `set -uo pipefail` but **not** `set -e` on
  the per-crate loop — a non-zero exit from one crate (surviving mutants, or a
  build failure) must **not** abort the remaining crates. Collect every crate's
  result, then exit non-zero at the end if any crate had survivors or errored, so
  CI still flags failure while the inventory stays complete. (Setup steps before
  the loop *do* fail fast.)
- Per-crate invocation captures the audit's env learnings. `--test-workspace`
  controls whether each mutant is checked against the **whole workspace** test
  suite (`true`, stronger detection) or only the **mutated crate's own** tests
  (`false`, far cheaper to build/run):
  - `musefs-db` — `--test-workspace=true`: its dependents' tests are cheap to
    build, so workspace-wide checking buys stronger detection within budget.
    Files `schema.rs`, `lib.rs` (and `tracks.rs`, `art.rs`, `tags.rs` for the
    full sweep).
  - `musefs-core` — `--test-workspace=false`: workspace mode drags in
    criterion/proptest scratch builds that blew the disk/time budget in the audit;
    crate-local tests already cover its logic. Files `reader.rs`, `tree.rs`,
    `scan.rs`, `facade.rs`, `ogg_index.rs`.
  - `musefs-format` — `--test-workspace=false --features fuzzing`, same rationale
    as core. All format files including the 7 the audit never reached
    (`ogg/mod.rs`, `ogg/page.rs`, `ogg/crc.rs`, `ogg/b64.rs`, `mp4.rs`, `wav.rs`,
    `mp3.rs`, plus `flac.rs`).
- Emit per-crate survivor reports into a fixed, predictable location —
  `--output mutants-out/<crate>/` — for collection. cargo-mutants' default output
  dir is `mutants.out/` (and it rotates a prior run to `mutants.out.old/`); the
  script pins `--output` so paths are deterministic.
- **No time cap** (the audit's 30-min cap is why only `flac.rs` was reached).

**Scratch/output hygiene** (`.gitignore` currently only ignores `/target`,
`/.claude/`, `.worktrees/`, `__pycache__/`, `*.pyc`): phase 1 must add
`.mutants-tmp/`, `mutants-out/`, `mutants.out/`, and `mutants.out.old/` to
`.gitignore` so neither the scratch `TMPDIR` nor the reports are ever committed.
(A stray `mutants.out.old/` is already present in the working tree from the audit
— it must be ignored, not committed.) The script additionally removes its own
`.mutants-tmp/` on exit.

### B2. `.github/workflows/mutants.yml`

- **PR job** — gated by a `paths:` trigger on the three in-scope crates'
  sources (`musefs-db/**`, `musefs-core/**`, `musefs-format/**`,
  `.github/workflows/mutants.yml`), mirroring how `fuzz.yml` scopes by path. The
  trigger keeps CLI/FUSE-only PRs from running the job at all; `--in-diff` then
  constrains mutation to the changed lines within those crates — so no extra
  `-p`/`--package` filtering is needed. `--in-diff` requires a concrete diff file
  and PR checkouts are shallow, so the job materializes the diff against the merge
  base with the lighter targeted fetch (not a full `fetch-depth: 0` history):
  - `actions/checkout`, then `git fetch --depth=1 origin $GITHUB_BASE_REF` to pull
    just the base ref into the shallow clone.
  - `git diff FETCH_HEAD...HEAD -- '*.rs' > mutants.diff` to capture the changed
    Rust lines on the merge-base three-dot range.
  - `cargo mutants --in-diff mutants.diff -j1`.
  - If `mutants.diff` is empty (no in-scope Rust changes), the job is a no-op
    pass rather than an error.
- **Scheduled + dispatchable job** (`schedule` cron, e.g. weekly like fuzz, plus
  `workflow_dispatch`): per-crate matrix (`musefs-db`, `musefs-core`,
  `musefs-format`), install `cargo-mutants` and `llvm-tools-preview`, run via
  `scripts/mutants.sh`, upload per-crate survivor reports as artifacts. GitHub
  runners have the disk headroom local does not.

`workflow_dispatch` is what we trigger to seed the initial inventory (Component C)
without touching local disk.

### B3. cargo-mutants Default limitation (record, don't fix here)

`musefs-db` mutation testing is currently vacuous: every mutant replaces a body
with `Ok(Default::default())`/`Ok(0/1/-1)`, and `Db` has no `Default`, so all
fail to compile (19/19 unviable). Format has the same pattern in a few spots.
Implementing `Default for Db` (or otherwise making these viable) is **deferred to
phase 4**; phase 1 only records it in the inventory.

## Component C — Verified survivor inventory

`docs/superpowers/specs/test-audit-remediation/2026-05-29-mutation-inventory.md`,
seeded from one `workflow_dispatch` run of `mutants.yml` (CI, not local).
Contents:

- Complete per-crate / per-file survivor list (caught/missed/unviable/timeout),
  superseding the audit's partial §9 (which only reached `flac.rs`).
- Each survivor tagged with its target remediation phase (2/3/4).
- The Default-impl tool limitation noted with its phase-4 follow-up.

This document is the authoritative input to phases 2–4. Phases consume it instead
of the audit's §9.

## Component D — Tracking doc

`docs/superpowers/specs/test-audit-remediation/2026-05-29-remediation-tracking.md`
(already written): records the decomposition, finding→phase map, and live status
per phase.

## Out of scope (this phase)

- Writing any new format/core/db unit tests to *kill* survivors — that is phases
  2–4. Phase 1 only fixes the two broken items and produces the harness + data.
- Implementing `Default for Db` (phase 4).
- The two document-only findings #15 (ESTALE) and the doc portion of #16.

## Sequencing

1. Component A (independent, lands immediately). A1 unblocks `cargo test
   --features metrics` (a compile error today); A2 closes a correctness gap in
   the beets tests (not a build blocker).
2. Component B (script + workflow).
3. Component C (dispatch CI run, collect artifacts, write inventory) — the long
   pole; depends on B.
4. Component D maintained throughout.

## Verification summary

| Item | Command / check |
|------|-----------------|
| A1 | `cargo test -p musefs-core --features metrics --test metrics -- --test-threads=1` → 4 pass |
| A2 | `cd contrib/beets && pytest` → green incl. new FK assertion |
| B1 | `scripts/mutants.sh` runs a single crate locally under disk budget without exhausting `/` |
| B2 | `mutants.yml` PR job runs `--in-diff`; `workflow_dispatch` completes the matrix |
| C | inventory doc exists, lists survivors per crate, supersedes §9 |
