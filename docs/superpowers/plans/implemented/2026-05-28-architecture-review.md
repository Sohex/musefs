# Architecture Review Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking. This is a review plan, not a code-change plan: tasks collect evidence, analyze architecture, and produce a written report.

**Goal:** Produce a prioritized architecture review of musefs using the "original audio bytes are never modified" invariant as the route through the codebase, including the beets plugin as an external SQLite writer.

**Architecture:** The review follows the data/invariant path from scan and external metadata writes, through SQLite, format synthesis, core read assembly, cache refresh, FUSE exposure, CLI entrypoints, and beets integration. The deliverable is a report with file/line evidence, strengths, risks, and scoped recommendations; no production behavior is changed.

**Tech Stack:** Rust 2021 Cargo workspace (`musefs-db`, `musefs-format`, `musefs-core`, `musefs-fuse`, `musefs-cli`), SQLite via `rusqlite`, FUSE via `fuser`, Python beets plugin under `contrib/beets`, shell inspection commands, Cargo verification commands.

**Spec:** `docs/superpowers/specs/2026-05-28-architecture-review-design.md`

---

## File Structure

- Create: `docs/superpowers/reviews/2026-05-28-architecture-review.md` — final architecture report.
- Read: `CLAUDE.md`, `README.md`, `docs/ROADMAP.md`, and the approved spec for documented architecture and scope.
- Read: `musefs-db/src/schema.rs`, `musefs-db/src/tracks.rs`, `musefs-db/src/tags.rs`, `musefs-db/src/art.rs` for the SQLite contract.
- Read: `musefs-format/src/layout.rs`, `musefs-format/src/flac.rs`, `musefs-format/src/mp3.rs`, `musefs-format/src/mp4.rs`, `musefs-format/src/ogg/mod.rs`, `musefs-format/src/wav.rs`, and focused tests for synthesis boundaries.
- Read: `musefs-core/src/scan.rs`, `musefs-core/src/mapping.rs`, `musefs-core/src/reader.rs`, `musefs-core/src/tree.rs`, `musefs-core/src/facade.rs`, `musefs-core/src/db_pool.rs`, and `musefs-core/src/ogg_index.rs`.
- Read: `musefs-fuse/src/lib.rs`, `musefs-cli/src/lib.rs`, `musefs-cli/src/main.rs`.
- Read: `contrib/beets/beetsplug/musefs.py`, `contrib/beets/beetsplug/_core.py`, `contrib/beets/tests/test_sync.py`, and `contrib/beets/tests/test_map_fields.py`.
- Read: `.github/workflows/ci.yml`, `.github/workflows/fuzz.yml`, `.github/workflows/audit.yml` for verification coverage.

---

### Task 1: Establish Baseline Evidence

**Files:**
- Read: `CLAUDE.md`
- Read: `README.md`
- Read: `docs/ROADMAP.md`
- Read: `docs/superpowers/specs/2026-05-28-architecture-review-design.md`
- Read: `Cargo.toml`

- [ ] **Step 1: Confirm worktree and branch state**

Run: `git status --short`

Expected: only unrelated user-owned files may be untracked or modified. Do not edit or stage unrelated files.

Run: `git branch --show-current`

Expected: prints the current branch name.

- [ ] **Step 2: Read the architecture docs**

Run: `sed -n '1,220p' CLAUDE.md`

Expected: captures the central invariant, crate dependency direction, `RegionLayout`, cache/version semantics, virtual tree behavior, and scanning conventions.

Run: `sed -n '1,220p' README.md`

Expected: captures user-facing claims about supported formats, tag handling, read-only behavior, tuning, and testing.

Run: `sed -n '1,220p' docs/ROADMAP.md`

Expected: captures delivered scope and explicit deferrals.

- [ ] **Step 3: Record the review route**

Add a short "Review Route" note to the working notes or directly to the report draft:

```markdown
## Review Route

This review traces the invariant from backing-file scan and external SQLite
writers, through the DB contract, format synthesis, read assembly, cache refresh,
FUSE exposure, CLI workflows, and the beets plugin.
```

---

### Task 2: Review the SQLite Contract and External Writer Path

**Files:**
- Read: `musefs-db/src/schema.rs`
- Read: `musefs-db/src/tracks.rs`
- Read: `musefs-db/src/tags.rs`
- Read: `musefs-db/src/art.rs`
- Read: `musefs-db/tests/triggers.rs`
- Read: `musefs-db/tests/change_detection.rs`
- Read: `contrib/beets/beetsplug/musefs.py`
- Read: `contrib/beets/beetsplug/_core.py`
- Read: `contrib/beets/tests/test_sync.py`
- Read: `contrib/beets/tests/test_map_fields.py`

- [ ] **Step 1: Inspect schema and triggers**

Run: `sed -n '1,260p' musefs-db/src/schema.rs`

Expected: identify tables, indexes, foreign keys, triggers, `content_version`, `updated_at`, and `PRAGMA user_version` behavior.

- [ ] **Step 2: Inspect DB access modules**

Run: `sed -n '1,260p' musefs-db/src/tracks.rs`

Expected: identify track upsert semantics, file identity assumptions, audio bounds storage, and content-version behavior.

Run: `sed -n '1,260p' musefs-db/src/tags.rs`

Expected: identify tag replacement ordering and multi-value semantics.

Run: `sed -n '1,260p' musefs-db/src/art.rs`

Expected: identify content-addressed art storage, chunk reads, track-art links, and GC behavior.

- [ ] **Step 3: Inspect beets writer behavior**

Run: `sed -n '1,280p' contrib/beets/beetsplug/musefs.py`

Expected: identify how the plugin discovers files, invokes scan, maps beets fields, writes tags/art, handles moved files, and commits DB changes.

Run: `sed -n '1,260p' contrib/beets/beetsplug/_core.py`

Expected: identify reusable mapping/sync functions and whether they duplicate or diverge from Rust mapping semantics.

- [ ] **Step 4: Inspect tests for DB/beets contract coverage**

Run: `sed -n '1,220p' musefs-db/tests/triggers.rs`

Expected: verify trigger coverage for tag/art mutations.

Run: `sed -n '1,260p' contrib/beets/tests/test_sync.py`

Expected: verify plugin lifecycle coverage for scan/sync/prune/art.

Run: `sed -n '1,220p' contrib/beets/tests/test_map_fields.py`

Expected: verify field mapping coverage.

- [ ] **Step 5: Write report notes**

In `docs/superpowers/reviews/2026-05-28-architecture-review.md`, capture concrete findings under:

```markdown
## SQLite Contract and External Writers

### Strengths

### Risks and Recommendations
```

Include file/line references from `nl -ba <file> | sed -n '<start>,<end>p'` for each finding.

---

### Task 3: Review Format Synthesis and `RegionLayout`

**Files:**
- Read: `musefs-format/src/layout.rs`
- Read: `musefs-format/src/flac.rs`
- Read: `musefs-format/src/mp3.rs`
- Read: `musefs-format/src/mp4.rs`
- Read: `musefs-format/src/ogg/mod.rs`
- Read: `musefs-format/src/wav.rs`
- Read: `musefs-format/src/fuzz_check.rs`
- Read: `musefs-format/tests/proptest_flac.rs`
- Read: `musefs-format/tests/proptest_mp3.rs`
- Read: `musefs-format/tests/proptest_mp4.rs`
- Read: `musefs-format/tests/proptest_ogg.rs`
- Read: `musefs-format/tests/proptest_wav.rs`

- [ ] **Step 1: Inspect segment model**

Run: `sed -n '1,220p' musefs-format/src/layout.rs`

Expected: identify which segment variants own bytes, which only reference backing/art bytes, and whether the types make byte ownership obvious.

- [ ] **Step 2: Inspect format modules at their public boundaries**

Run: `rg -n "pub fn|pub struct|pub enum|fn synthesize_layout|fn locate_audio|fn read_structure|fn read_metadata|fn read_tags|fn read_pictures" musefs-format/src`

Expected: map public parser/synthesis entrypoints and large private helpers.

- [ ] **Step 3: Inspect large synthesis modules**

Run: `sed -n '1,240p' musefs-format/src/mp4.rs`

Run: `sed -n '240,520p' musefs-format/src/mp4.rs`

Run: `sed -n '520,860p' musefs-format/src/mp4.rs`

Run: `sed -n '1,260p' musefs-format/src/ogg/mod.rs`

Run: `sed -n '260,620p' musefs-format/src/ogg/mod.rs`

Expected: identify ownership boundaries, parser/synthesizer coupling, bounded-memory strategy, and any module-size pressure.

- [ ] **Step 4: Inspect property and fuzz hooks**

Run: `sed -n '1,220p' musefs-format/src/fuzz_check.rs`

Run: `sed -n '1,180p' musefs-format/tests/proptest_mp4.rs`

Run: `sed -n '1,180p' musefs-format/tests/proptest_ogg.rs`

Expected: identify how invariant-level properties protect format code and where assertions are format-specific rather than shared.

- [ ] **Step 5: Write report notes**

Add:

```markdown
## Format Synthesis and Layout

### Strengths

### Risks and Recommendations
```

Prioritize any recommendations that improve local reasoning without changing synthesized bytes.

---

### Task 4: Review Core Read Assembly, Caching, and Refresh

**Files:**
- Read: `musefs-core/src/scan.rs`
- Read: `musefs-core/src/mapping.rs`
- Read: `musefs-core/src/reader.rs`
- Read: `musefs-core/src/tree.rs`
- Read: `musefs-core/src/facade.rs`
- Read: `musefs-core/src/db_pool.rs`
- Read: `musefs-core/src/ogg_index.rs`
- Read: `musefs-core/tests/read_at.rs`
- Read: `musefs-core/tests/reader.rs`
- Read: `musefs-core/tests/facade.rs`
- Read: `musefs-core/tests/proptest_read_fidelity.rs`

- [ ] **Step 1: Inspect scan and mapping boundaries**

Run: `sed -n '1,260p' musefs-core/src/scan.rs`

Run: `sed -n '1,260p' musefs-core/src/mapping.rs`

Expected: identify how source bytes become DB rows, how tags/art become format inputs, and whether mapping is centralized enough.

- [ ] **Step 2: Inspect reader and cache behavior**

Run: `sed -n '1,260p' musefs-core/src/reader.rs`

Run: `sed -n '260,560p' musefs-core/src/reader.rs`

Run: `sed -n '560,920p' musefs-core/src/reader.rs`

Expected: identify `HeaderCache`, LRU behavior, `ResolvedFile`, `read_at`, lazy art streaming, backing-file validation, and Ogg page/index handling.

- [ ] **Step 3: Inspect virtual tree and facade refresh**

Run: `sed -n '1,300p' musefs-core/src/tree.rs`

Run: `sed -n '1,460p' musefs-core/src/facade.rs`

Expected: identify path rendering, collision handling, inode allocation, `PRAGMA data_version` polling, cache pruning, and changed-inode reporting.

- [ ] **Step 4: Inspect core invariant tests**

Run: `sed -n '1,260p' musefs-core/tests/proptest_read_fidelity.rs`

Run: `sed -n '1,260p' musefs-core/tests/read_at.rs`

Run: `sed -n '1,260p' musefs-core/tests/facade.rs`

Expected: identify which cache, refresh, read, and byte-fidelity paths are covered.

- [ ] **Step 5: Write report notes**

Add:

```markdown
## Core Read Assembly and Refresh

### Strengths

### Risks and Recommendations
```

Call out any places where core knows too much about a format, or where cache invalidation assumptions are hard to audit.

---

### Task 5: Review FUSE and CLI Boundaries

**Files:**
- Read: `musefs-fuse/src/lib.rs`
- Read: `musefs-fuse/tests/mount.rs`
- Read: `musefs-fuse/tests/concurrency.rs`
- Read: `musefs-fuse/tests/keep_cache.rs`
- Read: `musefs-cli/src/lib.rs`
- Read: `musefs-cli/src/main.rs`
- Read: `musefs-cli/tests/cli.rs`
- Read: `musefs-cli/tests/scan.rs`

- [ ] **Step 1: Inspect FUSE adapter responsibilities**

Run: `sed -n '1,520p' musefs-fuse/src/lib.rs`

Expected: identify how thin the adapter is, where blocking work is offloaded, how file handles map to core reads, how mount options are applied, and how invalidation is triggered.

- [ ] **Step 2: Inspect CLI responsibilities**

Run: `sed -n '1,260p' musefs-cli/src/lib.rs`

Run: `sed -n '1,160p' musefs-cli/src/main.rs`

Expected: identify command parsing, mode/tuning options, DB opening behavior, and whether CLI logic leaks domain decisions.

- [ ] **Step 3: Inspect boundary tests**

Run: `sed -n '1,260p' musefs-fuse/tests/concurrency.rs`

Run: `sed -n '1,260p' musefs-fuse/tests/keep_cache.rs`

Run: `sed -n '1,220p' musefs-cli/tests/cli.rs`

Expected: identify coverage for concurrency, cache invalidation, and argument-to-config mapping.

- [ ] **Step 4: Write report notes**

Add:

```markdown
## FUSE and CLI Boundaries

### Strengths

### Risks and Recommendations
```

Recommendations should preserve the principle that `musefs-fuse` and `musefs-cli` stay thin.

---

### Task 6: Review CI, Fuzzing, and Documentation Coverage

**Files:**
- Read: `.github/workflows/ci.yml`
- Read: `.github/workflows/fuzz.yml`
- Read: `.github/workflows/audit.yml`
- Read: `fuzz/Cargo.toml`
- Read: `fuzz/src/bin/generate_seeds.rs`
- Read: `tests/interop/test_mutagen_roundtrip.py`
- Read: `musefs-core/tests/interop_emit.rs`
- Read: `CHANGELOG.md`

- [ ] **Step 1: Inspect workflow coverage**

Run: `sed -n '1,260p' .github/workflows/ci.yml`

Run: `sed -n '1,260p' .github/workflows/fuzz.yml`

Run: `sed -n '1,220p' .github/workflows/audit.yml`

Expected: identify what runs per PR, what runs on schedule, and what high-risk behavior is only manually tested.

- [ ] **Step 2: Inspect interop and fuzz entrypoints**

Run: `sed -n '1,260p' musefs-core/tests/interop_emit.rs`

Run: `sed -n '1,260p' tests/interop/test_mutagen_roundtrip.py`

Run: `sed -n '1,240p' fuzz/src/bin/generate_seeds.rs`

Expected: identify how independent-reader compatibility and fuzz coverage map to supported formats.

- [ ] **Step 3: Write report notes**

Add:

```markdown
## Verification and Documentation Coverage

### Strengths

### Risks and Recommendations
```

Separate "must fix before release" risks from "good hardening follow-up" recommendations.

---

### Task 7: Assemble and Verify the Final Report

**Files:**
- Create: `docs/superpowers/reviews/2026-05-28-architecture-review.md`

- [ ] **Step 1: Create the report with this structure**

```markdown
# musefs Architecture Review

## Executive Summary

## Findings

### High Priority

### Medium Priority

### Low Priority / Future Refactors

## SQLite Contract and External Writers

## Format Synthesis and Layout

## Core Read Assembly and Refresh

## FUSE and CLI Boundaries

## Verification and Documentation Coverage

## Strengths to Preserve

## Recommended Follow-Up Plan
```

- [ ] **Step 2: Ensure every finding has evidence**

For each finding, include:

```markdown
**Risk:** one concrete risk.
**Evidence:** file and line reference, plus a short explanation.
**Recommendation:** one scoped action.
```

- [ ] **Step 3: Run lightweight verification**

Run: `cargo fmt --all -- --check`

Expected: passes. This confirms the workspace is still formatted; no code changes should have been made.

Run: `cargo test --workspace`

Expected: passes, or any failure is recorded in the report's verification section with the exact failing command and first relevant error.

Run: `git diff --stat`

Expected: only `docs/superpowers/reviews/2026-05-28-architecture-review.md` is changed during review execution.

- [ ] **Step 4: Self-review the report**

Run: `rg -n "TBD|TODO|FIXME|placeholder|unclear|maybe|probably" docs/superpowers/reviews/2026-05-28-architecture-review.md`

Expected: no placeholders. If "unclear", "maybe", or "probably" appears, rewrite the sentence to state a concrete assumption or recommendation.

- [ ] **Step 5: Commit the report**

```bash
git add docs/superpowers/reviews/2026-05-28-architecture-review.md
git commit -m "docs: add architecture review"
```
