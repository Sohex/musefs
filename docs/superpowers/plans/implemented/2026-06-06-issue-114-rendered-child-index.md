# Issue #114 Rendered Child Index Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Remove sibling fan-out scans from rendered-name navigation in incremental tree refresh.

**Architecture:** Add `VirtualTree::rendered_children`, a secondary parent/rendered-name/disambiguated-name index maintained beside `children`. Keep public lookup/readdir behavior on disambiguated names, preserve current same-rendered iteration order, and make `children_by_rendered` an indexed lookup used by `deepest_existing_ancestor`.

**Tech Stack:** Rust 2021, `im::{HashMap as ImHashMap, OrdMap}`, Cargo tests, existing `bench_refresh` ignored timing harness.

---

## File Structure

- Modify `musefs-core/src/tree.rs`: add the rendered-name child index, index-maintenance helpers, indexed lookup helper, and focused unit tests.
- Modify `musefs-core/tests/bench_refresh.rs`: add an ignored root fan-out refresh benchmark using many top-level artists.
- Modify `BENCHMARKS.md`: record benchmark command and results after running the ignored benchmark.

No new module is needed. The index is an internal invariant of `VirtualTree`, so it belongs in `tree.rs` with the existing tree maps and mutation primitives.

## Task 1: Add Focused Tree Tests

**Files:**
- Modify: `musefs-core/src/tree.rs`

- [ ] **Step 1: Add rendered lookup regression tests**

In `musefs-core/src/tree.rs`, inside the existing `#[cfg(test)] mod tests`, replace the current `child_by_rendered_finds_disambiguated_node` test with this stronger version and add the four new tests immediately after it:

```rust
    #[test]
    fn child_by_rendered_finds_disambiguated_node() {
        let t = VirtualTree::build(&[(10, "D/song.flac".into()), (20, "D/song.flac".into())]);
        let d = t.lookup(VirtualTree::ROOT, "D").unwrap();
        let base = t.lookup(d, "song.flac").unwrap();
        let suffixed = t.lookup(d, "song (2).flac").unwrap();

        assert_eq!(t.children_by_rendered(d, "song.flac"), vec![base, suffixed]);
        assert_eq!(t.children_by_rendered_examined_for_test(d, "song.flac"), 2);
    }

    #[test]
    fn deepest_existing_ancestor_preserves_rendered_dir_vs_file_order() {
        let t = VirtualTree::build(&[(1, "X".into()), (2, "X/a.flac".into())]);
        let file = t.lookup(VirtualTree::ROOT, "X").unwrap();
        let dir = t.lookup(VirtualTree::ROOT, "X (2)").unwrap();

        assert_eq!(t.children_by_rendered(VirtualTree::ROOT, "X"), vec![file, dir]);
        assert_eq!(
            t.deepest_existing_ancestor("X/new.flac"),
            (dir, 1),
            "same-rendered file must not hide the matching directory"
        );
    }

    #[test]
    fn children_by_rendered_updates_when_collision_member_removed() {
        let mut alloc = InodeAllocator::new();
        let mut t = VirtualTree::build_with(
            &[(10, "D/song.flac".into()), (20, "D/song.flac".into())],
            &mut alloc,
        );
        let d = t.lookup(VirtualTree::ROOT, "D").unwrap();
        let survivor = t.lookup(d, "song (2).flac").unwrap();

        t.remove_track(10, &mut alloc);

        assert_eq!(t.children_by_rendered(d, "song.flac"), vec![survivor]);
        assert_eq!(t.children_by_rendered_examined_for_test(d, "song.flac"), 1);
    }

    #[test]
    fn children_by_rendered_updates_when_empty_dir_pruned() {
        let mut alloc = InodeAllocator::new();
        let mut t = VirtualTree::build_with(
            &[
                (10, "A/B/x.flac".into()),
                (20, "A/C/y.flac".into()),
            ],
            &mut alloc,
        );
        let a = t.lookup(VirtualTree::ROOT, "A").unwrap();
        assert_eq!(t.children_by_rendered_examined_for_test(a, "B"), 1);

        t.remove_track(10, &mut alloc);

        assert!(t.children_by_rendered(a, "B").is_empty());
        assert_eq!(t.children_by_rendered_examined_for_test(a, "B"), 0);
        assert_eq!(t.children_by_rendered_examined_for_test(a, "C"), 1);
    }

    #[test]
    fn deepest_existing_ancestor_rendered_miss_does_not_scan_root_fanout() {
        let entries: Vec<(i64, String)> = (0..1024)
            .map(|i| {
                (
                    i64::from(i),
                    format!("Artist {i:04}/Album {i:04}/Track.flac"),
                )
            })
            .collect();
        let t = VirtualTree::build(&entries);

        assert_eq!(
            t.deepest_existing_ancestor("Unknown/Unknown/new.flac"),
            (VirtualTree::ROOT, 0)
        );
        assert_eq!(
            t.children_by_rendered_examined_for_test(VirtualTree::ROOT, "Unknown"),
            0,
            "rendered-name miss should examine no same-rendered candidates"
        );
    }
```

- [ ] **Step 2: Run tests to verify they fail for the expected reason**

Run:

```bash
rtk cargo test -p musefs-core --lib rendered -- --nocapture
```

Expected: FAIL at compile time with errors like:

```text
no method named `children_by_rendered_examined_for_test` found for struct `VirtualTree`
```

Do not implement the helper in this task. The failing test pins the next task's public test surface.

- [ ] **Step 3: Leave the failing tests uncommitted**

Run:

```bash
rtk git status --short
```

Expected: `musefs-core/src/tree.rs` is modified. Do not commit yet; Task 2 will commit tests and implementation together after the focused tests pass.

## Task 2: Add And Maintain `rendered_children`

**Files:**
- Modify: `musefs-core/src/tree.rs`

- [ ] **Step 1: Add the `rendered_children` field and root initialization**

In `VirtualTree`, add the secondary index field:

```rust
pub struct VirtualTree {
    nodes: ImHashMap<u64, Node>,
    children: ImHashMap<u64, OrdMap<String, u64>>,
    rendered_children: ImHashMap<u64, OrdMap<String, OrdMap<String, u64>>>,
    track_to_inode: ImHashMap<i64, u64>,
}
```

In `VirtualTree::build_with`, initialize it and add an empty root entry:

```rust
let mut tree = VirtualTree {
    nodes: ImHashMap::new(),
    children: ImHashMap::new(),
    rendered_children: ImHashMap::new(),
    track_to_inode: ImHashMap::new(),
};
```

After `tree.children.insert(Self::ROOT, OrdMap::new());`, add:

```rust
tree.rendered_children.insert(Self::ROOT, OrdMap::new());
```

- [ ] **Step 2: Add index maintenance helpers**

In `impl VirtualTree`, place these helpers near `lookup`/`children_by_rendered` so all child-index operations are local:

```rust
    fn insert_rendered_child(&mut self, parent: u64, rendered: &str, name: &str, inode: u64) {
        self.rendered_children
            .entry(parent)
            .or_insert_with(OrdMap::new)
            .entry(rendered.to_string())
            .or_insert_with(OrdMap::new)
            .insert(name.to_string(), inode);
    }

    fn remove_rendered_child(&mut self, parent: u64, rendered: &str, name: &str) {
        let Some(by_rendered) = self.rendered_children.get_mut(&parent) else {
            return;
        };
        let remove_bucket = match by_rendered.get_mut(rendered) {
            Some(same_rendered) => {
                same_rendered.remove(name);
                same_rendered.is_empty()
            }
            None => false,
        };
        if remove_bucket {
            by_rendered.remove(rendered);
        }
    }
```

Do not replace these helpers with ad hoc updates at each call site.

- [ ] **Step 3: Update insert paths**

In `insert_file`, after:

```rust
self.children.get_mut(&dir).unwrap().insert(name, inode);
```

change the insertion to preserve a clone for both maps:

```rust
self.children.get_mut(&dir).unwrap().insert(name.clone(), inode);
self.insert_rendered_child(dir, raw_name, &name, inode);
```

In `ensure_dir`, after inserting the child into `children`, add the rendered index update. The end of the method should look like this:

```rust
self.children.insert(inode, OrdMap::new());
self.rendered_children.insert(inode, OrdMap::new());
self.children
    .get_mut(&parent)
    .unwrap()
    .insert(unique.clone(), inode);
self.insert_rendered_child(parent, name, &unique, inode);
(inode, full)
```

Preserve the existing early return:

```rust
if let Some(&existing) = self.children[&parent].get(name) {
    if self.is_dir(existing) {
        return (existing, join_path(parent_path, name));
    }
}
```

Do not replace that check with a rendered-bucket search.

- [ ] **Step 4: Update removal and pruning**

In `remove_track`, read both names before deleting the node:

```rust
let ino = self.track_to_inode.remove(&track_id)?;
let parent = self.nodes.get(&ino)?.parent;
let names = self
    .nodes
    .get(&ino)
    .map(|n| (n.name.clone(), n.rendered_name.clone()));
self.nodes.remove(&ino);
if let Some((name, rendered)) = names {
    if let Some(kids) = self.children.get_mut(&parent) {
        kids.remove(&name);
    }
    self.remove_rendered_child(parent, &rendered, &name);
}
Some(self.prune_empty_dirs_upward(parent))
```

In `prune_empty_dirs_upward`, remove the pruned directory from both child maps and remove the directory's own empty rendered map. The loop body after `let names = ...;` should become:

```rust
self.children.remove(&dir);
self.rendered_children.remove(&dir);
self.nodes.remove(&dir);
if let Some((name, rendered)) = &names {
    if let Some(kids) = self.children.get_mut(&parent) {
        kids.remove(name);
    }
    self.remove_rendered_child(parent, rendered, name);
}
last_pruned = names;
dir = parent;
```

- [ ] **Step 5: Replace `children_by_rendered` with indexed lookup and add test helper**

Replace the existing `children_by_rendered` method with these methods:

```rust
    fn children_by_rendered_with_examined(&self, dir: u64, rendered: &str) -> (Vec<u64>, usize) {
        match self
            .rendered_children
            .get(&dir)
            .and_then(|kids| kids.get(rendered))
        {
            None => (Vec::new(), 0),
            Some(same_rendered) => {
                let children: Vec<u64> = same_rendered.values().copied().collect();
                let examined = same_rendered.len();
                (children, examined)
            }
        }
    }

    /// Inodes of `dir`'s direct children whose pre-disambiguation name is `rendered`.
    pub fn children_by_rendered(&self, dir: u64, rendered: &str) -> Vec<u64> {
        self.children_by_rendered_with_examined(dir, rendered).0
    }

    #[cfg(test)]
    fn children_by_rendered_examined_for_test(&self, dir: u64, rendered: &str) -> usize {
        self.children_by_rendered_with_examined(dir, rendered).1
    }
```

`children_by_rendered_with_examined` must treat `examined` as the number of
direct child entries inspected by the lookup, not merely the number of returned
matches. With the rendered index, an absent rendered-name bucket inspects `0`
child entries and a present bucket inspects `same_rendered.len()` child entries.
If this helper were ever implemented by scanning `children[dir]`, it would need
to count every sibling visited, so the root fan-out miss test would report the
full fan-out and fail.

This keeps `deepest_existing_ancestor` unchanged; it should continue to call `children_by_rendered`.

- [ ] **Step 6: Run focused tree tests**

Run:

```bash
rtk cargo test -p musefs-core --lib rendered -- --nocapture
```

Expected: PASS.

- [ ] **Step 7: Run all tree unit tests**

Run:

```bash
rtk cargo test -p musefs-core --lib tree -- --nocapture
```

Expected: PASS.

- [ ] **Step 8: Commit the implementation**

```bash
rtk git add musefs-core/src/tree.rs
rtk git commit -m "fix(core): index rendered child lookups"
```

## Task 3: Verify Incremental Tree Equivalence

**Files:**
- No new source edits expected. Fix `musefs-core/src/tree.rs` only if these tests expose an index-maintenance bug.

- [ ] **Step 1: Run incremental refresh tests**

Run:

```bash
rtk cargo test -p musefs-core --test incremental_refresh -- --nocapture
```

Expected: PASS.

- [ ] **Step 2: Run core tests**

Run:

```bash
rtk cargo test -p musefs-core
```

Expected: PASS.

- [ ] **Step 3: Commit any fixes required by equivalence tests**

If Step 1 or Step 2 required a code fix, commit it:

```bash
rtk git add musefs-core/src/tree.rs
rtk git commit -m "fix(core): maintain rendered child index during refresh"
```

If no files changed, do not create an empty commit.

## Task 4: Add Root Fan-Out Benchmark

**Files:**
- Modify: `musefs-core/tests/bench_refresh.rs`

- [ ] **Step 1: Add the ignored benchmark**

In `musefs-core/tests/bench_refresh.rs`, add this test after `bench_refresh_one_across_library_sizes`:

```rust
#[test]
#[ignore = "issue #114 timing harness; run with --ignored --nocapture"]
fn bench_refresh_root_fanout_one_across_library_sizes() {
    let tier = std::env::var("MUSEFS_BENCH_TIER").unwrap_or_else(|_| "ci".into());
    println!("\n{}", RunReport::header());
    for n in [100usize, 1000, 5000, 20000] {
        // Many albums with one track each produce many top-level artists under
        // `$artist/$album/$title`, exercising `deepest_existing_ancestor` at
        // root fan-out when the changed track moves to fallback `Unknown`.
        let tmp = tempfile::tempdir().unwrap();
        let params = CorpusParams::single(Format::Flac, n, 1);
        let target = prepare_format(&params, tmp.path(), params.format_mix[0]);

        let db = Db::open(&target.db_path).unwrap();
        scan_directory(&db, &target.corpus_dir).unwrap();
        let fs = Musefs::open(db, config()).unwrap();

        let one_ms = time_refresh(&target.db_path, &fs, 1);
        println!(
            "{}",
            RunReport {
                label: format!("refresh-root-fanout-1@{n}"),
                format: "flac".into(),
                tier: tier.clone(),
                storage: "tempfs".into(),
                wall_ms: one_ms,
                opens: 0,
                preads: 0,
                fsyncs: None,
                bytes_read: 0,
                peak_rss_kib: None,
            }
            .row()
        );
    }
    println!();
}
```

- [ ] **Step 2: Run just the benchmark test in release mode**

Run:

```bash
rtk cargo test -p musefs-core --release --test bench_refresh bench_refresh_root_fanout_one_across_library_sizes -- --ignored --nocapture
```

Expected: PASS and print four `refresh-root-fanout-1@N` rows.

- [ ] **Step 3: Commit the benchmark**

```bash
rtk git add musefs-core/tests/bench_refresh.rs
rtk git commit -m "bench(core): add root fanout refresh sweep"
```

## Task 5: Record Benchmark Evidence

**Files:**
- Modify: `BENCHMARKS.md`

- [ ] **Step 1: Capture benchmark output and generate markdown rows**

Run the benchmark once more and capture its output:

```bash
rtk cargo test -p musefs-core --release --test bench_refresh bench_refresh_root_fanout_one_across_library_sizes -- --ignored --nocapture | rtk tee /tmp/musefs-issue114-root-fanout.txt
```

Generate the markdown table body:

```bash
rtk awk '$1 ~ /^refresh-root-fanout-1@/ { split($1, a, "@"); printf "| %s | %s |\n", a[2], $5 }' /tmp/musefs-issue114-root-fanout.txt
```

Expected: four rows, one each for `100`, `1000`, `5000`, and `20000`, with numeric millisecond values in the second column.

- [ ] **Step 2: Add the benchmark results section**

In `BENCHMARKS.md`, in the refresh benchmark area near the existing "Phase 6 PR 1 — Refresh O(changed) (#69)" section, add this section. Under the table header, paste the four numeric rows generated in Step 1.

```markdown
---

## Issue #114 — Rendered Child Lookup Root Fan-Out

*Measured 2026-06-06 (same machine as implementation run; release build; ignored harness).*

Harness:

```bash
cargo test -p musefs-core --release --test bench_refresh \
  bench_refresh_root_fanout_one_across_library_sizes -- --ignored --nocapture
```

The corpus uses `CorpusParams::single(Format::Flac, n, 1)`, so `$artist/$album/$title`
creates `n` top-level artist directories. The timed update retags one track with
only `COMMENT`, moving it to fallback `Unknown/Unknown/...`; this exercises an
absent rendered-name lookup at root in `deepest_existing_ancestor`.

| library size (top-level artists) | refresh-root-fanout-1 wall (ms) |
|---------------------------------:|--------------------------------:|

The rendered-name child index turns the root lookup into an indexed miss, so the
tree-side lookup no longer scans unrelated artists. Overall wall time may still
include SQLite changelog reads, changed-track rendering, and test harness setup,
but the root sibling scan from issue #114 is removed.
```

The committed section must contain four table body rows. Each second-column cell must be only a number.

- [ ] **Step 3: Verify the table cells are numeric**

Run:

```bash
rtk awk '/^## Issue #114/{in_section=1} in_section && /^## / && $0 !~ /^## Issue #114/{in_section=0} in_section && /^\| [0-9]+[[:space:]]+\|/ { rows++; if ($4 !~ /^[0-9]+$/) bad=1 } END { if (rows != 4 || bad) exit 1 }' BENCHMARKS.md
```

Expected: exit 0. If it exits 1, fix the table so it has four body rows and numeric wall-time cells.

- [ ] **Step 4: Commit the benchmark results**

```bash
rtk git add BENCHMARKS.md
rtk git commit -m "docs: record issue 114 refresh benchmark"
```

## Task 6: Final Verification

**Files:**
- No source edits expected. Fix only regressions discovered by verification.

- [ ] **Step 1: Run formatting check**

Run:

```bash
rtk cargo fmt --check
```

Expected: PASS.

- [ ] **Step 2: Run core tests**

Run:

```bash
rtk cargo test -p musefs-core
```

Expected: PASS.

- [ ] **Step 3: Run workspace tests**

Run:

```bash
rtk cargo test
```

Expected: PASS.

- [ ] **Step 4: Run clippy**

Run:

```bash
rtk cargo clippy --all-targets -- -D warnings
```

Expected: PASS.

- [ ] **Step 5: Inspect final diff**

Run:

```bash
rtk git status --short
rtk git log --oneline -6
```

Expected: clean worktree, with commits for tests, implementation, benchmark, and benchmark documentation.
