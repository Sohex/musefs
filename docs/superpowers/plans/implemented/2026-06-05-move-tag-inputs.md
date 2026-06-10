# Move Tag Strings into Synthesis Inputs (#131) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Eliminate the per-resolve double-copy of tag key/value strings by making `tags_to_inputs` consume its `Vec<Tag>` and move the strings into `TagInput`s.

**Architecture:** `HeaderCache::resolve` (`musefs-core/src/reader.rs:162`) fetches `Vec<Tag>` solely to feed `tags_to_inputs`, which today clones every key/value. Taking the rows by value and moving `t.key`/`t.value` removes the copies with zero lifetime plumbing — `TagInput` stays an owned struct, so `musefs-format`, its tests, and the out-of-workspace fuzz crate are untouched. The `ArtInput` half of issue #131 is already moot (`track_art_to_inputs` moves its strings); the PR body notes this.

**Tech Stack:** Rust (musefs-core only). Spec: `docs/superpowers/specs/2026-06-05-allocation-cleanups-design.md` (PR 1 section).

---

### Task 1: Branch setup

**Files:** none

- [ ] **Step 1: Create the branch from up-to-date main**

```bash
git checkout main && git pull && git checkout -b move-tag-inputs
```

Expected: `Switched to a new branch 'move-tag-inputs'`

### Task 2: `tags_to_inputs` takes `Vec<Tag>` by value

**Files:**
- Modify: `musefs-core/src/mapping.rs` (function at ~line 7, unit test `inputs_preserve_order_including_multivalue` at ~line 100)
- Modify: `musefs-core/src/reader.rs:162-163`

This is a behavior-preserving signature change, so the "failing test" is the existing test updated to the new calling convention — it fails to compile until the implementation follows.

- [ ] **Step 1: Update the unit test to pass the rows by value**

In `musefs-core/src/mapping.rs`, test `inputs_preserve_order_including_multivalue`, change the call (the `tags` local is already an owned `Vec<Tag>`):

```rust
        let inputs = tags_to_inputs(tags);
```

(was `tags_to_inputs(&tags)`; everything else in the test stays.)

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p musefs-core inputs_preserve_order`
Expected: compile error — `mismatched types: expected '&[Tag]', found 'Vec<Tag>'`

- [ ] **Step 3: Change `tags_to_inputs` to consume and move**

Replace the function in `musefs-core/src/mapping.rs` (keep its position; `TagInput` is already imported and its fields are `pub`):

```rust
/// Convert DB tag rows into the ordered list of synthesis inputs (one per value),
/// moving the strings out of the rows rather than copying them.
/// `Db::get_tags` already returns rows ordered by `(key, ordinal)`, so order is preserved.
pub(crate) fn tags_to_inputs(tags: Vec<Tag>) -> Vec<TagInput> {
    tags.into_iter()
        .map(|t| TagInput {
            key: t.key,
            value: t.value,
        })
        .collect()
}
```

- [ ] **Step 4: Update the resolve call site**

In `musefs-core/src/reader.rs` (~line 162), replace:

```rust
                let tags = db.get_tags(track.id)?;
                let inputs = tags_to_inputs(&tags);
```

with:

```rust
                let inputs = tags_to_inputs(db.get_tags(track.id)?);
```

- [ ] **Step 5: Run the crate tests**

Run: `cargo test -p musefs-core`
Expected: all tests PASS (the rewritten unit test plus the reader/facade tests that exercise resolve end to end).

- [ ] **Step 6: Lint and format check**

Run: `cargo clippy --all-targets -p musefs-core && cargo fmt --all --check`
Expected: no warnings, no diff. (`--all-targets` matters: `musefs-core/benches/read_throughput.rs` and the `tests/` dirs are API consumers a plain build skips.)

- [ ] **Step 7: Commit**

```bash
git add musefs-core/src/mapping.rs musefs-core/src/reader.rs
git commit -m "$(cat <<'EOF'
Move tag strings into synthesis inputs instead of cloning (#131)

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>
EOF
)"
```

### Task 3: Gates and PR

**Files:** none (verification only)

- [ ] **Step 1: Workspace tests**

Run: `cargo test`
Expected: all crates pass (FUSE e2e stays `#[ignore]`d).

- [ ] **Step 2: In-diff mutation gate (CI parity)**

Always `-j2`, output on `/tmp`, do NOT set `TMPDIR`. The `grep -q` guard matters: an empty diff mutates nothing and exits 0 — a silent false pass.

```bash
git diff "$(git merge-base main HEAD)...HEAD" -- '*.rs' > mutants.diff
grep -q '^@@ ' mutants.diff
cargo mutants --in-diff mutants.diff -j2 --exclude 'musefs-latencyfs/**' --output /tmp/mutants-out/in-diff
```

Expected: exit 0, no missed mutants. The diff is small; the move-map closure is killed by `inputs_preserve_order_including_multivalue`.

- [ ] **Step 3: Push and open the PR**

```bash
git push -u origin move-tag-inputs
gh pr create --title "Move tag strings into synthesis inputs instead of cloning (#131)" --body "$(cat <<'EOF'
Closes #131.

`tags_to_inputs` cloned every tag key/value out of the already-owned
`Vec<Tag>` DB rows on each header resolve. It now consumes the rows and
moves the strings; the only remaining allocation is the unavoidable
SQLite-row materialization. `TagInput` stays an owned struct, so
`musefs-format` signatures, format tests, and the fuzz crate are untouched.

Note: the `ArtInput` half of #131 is already resolved in the current code —
`track_art_to_inputs` moves `meta.mime`/`ta.description` out of its locally
fetched rows; there was no second copy to remove.

Borrowed `TagInput<'a>`/`Cow` designs were considered and rejected in the
spec (`docs/superpowers/specs/2026-06-05-allocation-cleanups-design.md`):
they save zero further allocations while rippling through five format
modules, ~11 test files, and the out-of-workspace fuzz helpers.

🤖 Generated with [Claude Code](https://claude.com/claude-code)
EOF
)"
```

Expected: PR URL printed; CI (`ci-ok`/`coverage-ok`) goes green.
