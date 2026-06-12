# Folded-Mount Inode Stability Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** In case-insensitive mounts, a surviving track keeps its inode when an unrelated deletion flips a merged directory's display casing (#305).

**Architecture:** `InodeAllocator` (`musefs-core/src/tree.rs`) keys inodes by a path string and persists across full rebuilds. We make that key *case-folded* in case-insensitive mode, so `fold("Foo/B") == fold("foo/B")` and the inode no longer depends on which casing a rebuild happens to pick. The fix is confined to `musefs-core`; `musefs-fuse`/CLI/binary are untouched.

**Tech Stack:** Rust, `im` persistent collections (`ImHashMap`), `std::borrow::Cow` (already imported at `tree.rs:2`).

**Spec:** `docs/superpowers/specs/2026-06-12-folded-inode-stability-design.md`

---

## File structure

- **Modify `musefs-core/src/tree.rs`** — add a `fold_keys` field to `InodeAllocator`, change `new()` → `new(case_insensitive: bool)`, add a private `key()` transform, route `intern`/`prune_retired` through it, update the struct doc, fix every `InodeAllocator::new()` call site, and add two regression tests in the existing `tests` module.
- **Modify `musefs-core/src/facade.rs`** — construct the live allocator from `config.case_insensitive` in `Musefs::open`; mechanical `new()` → `new(false)` for the one case-sensitive test site.
- **Modify `ARCHITECTURE.md`** — one clause noting the key is case-folded on case-insensitive mounts (docs-only commit).

Why the incremental path is not touched: `poll_refresh_notify` forces case-insensitive mounts through `rebuild_full` → `build_with_ci`, so the fold-keyed allocator is only ever driven by `build_with_ci` + `prune_retired`, never by `apply_changes`/`rebuild_incremental`.

---

## Task 1: Fold the inode key in case-insensitive mode

**Files:**
- Modify: `musefs-core/src/tree.rs` (`InodeAllocator` struct ~`:27-31`, `new` ~`:34-37`, `intern` ~`:40`, `prune_retired` ~`:56`, struct doc ~`:21`, all `InodeAllocator::new()` call sites)
- Modify: `musefs-core/src/facade.rs` (`Musefs::open` ~`:239`, test ~`:1787`)
- Test: `musefs-core/src/tree.rs` (`tests` module, after `case_insensitive_removal_keeps_folded_lookup_consistent`)

- [ ] **Step 1: Add the `fold_keys` field to the struct**

In `musefs-core/src/tree.rs`, change the struct definition:

```rust
pub struct InodeAllocator {
    paths: ImHashMap<String, u64>,
    next: u64,
    fold_keys: bool,
}
```

- [ ] **Step 2: Extend the struct doc comment**

Append a sentence to the existing `InodeAllocator` doc block (just above the `#[derive(...)]`), so the last line reads:

```rust
/// Retired paths are dropped by `prune_retired` once they outnumber live ones,
/// bounding the map at 2x the live tree between prunes; a path that returns
/// after a prune gets a fresh inode rather than its old one.
/// In case-insensitive mounts the key is case-folded, so a track keeps its
/// inode when an unrelated deletion flips a merged directory's display casing
/// (#305).
```

- [ ] **Step 3: Change `new` to take the case-sensitivity flag**

Replace the body of `InodeAllocator::new`:

```rust
pub fn new(case_insensitive: bool) -> InodeAllocator {
    let mut paths = ImHashMap::new();
    paths.insert(String::new(), VirtualTree::ROOT); // root path "" -> inode 1
    InodeAllocator {
        paths,
        next: 2,
        fold_keys: case_insensitive,
    }
}
```

(`fold("")` is `""`, so the root key is unaffected by folding.)

- [ ] **Step 4: Add the `key()` transform and route `intern`/`prune_retired` through it**

Add this private method inside `impl InodeAllocator` (e.g. directly after `new`):

```rust
/// Transform a path into its map key: case-folded when the mount is
/// case-insensitive (so a survivor keeps its inode when an unrelated deletion
/// flips a merged directory's display casing, #305), identity otherwise.
fn key<'a>(&self, path: &'a str) -> Cow<'a, str> {
    if self.fold_keys {
        Cow::Owned(fold(path))
    } else {
        Cow::Borrowed(path)
    }
}
```

Replace the body of `intern`:

```rust
fn intern(&mut self, path: &str) -> u64 {
    let key = self.key(path);
    if let Some(&ino) = self.paths.get(key.as_ref()) {
        return ino;
    }
    let ino = self.next;
    self.next += 1;
    self.paths.insert(key.into_owned(), ino);
    ino
}
```

Replace the body of `prune_retired`:

```rust
pub(crate) fn prune_retired(&mut self, tree: &VirtualTree) {
    if self.paths.len() <= 2 * tree.nodes.len() {
        return;
    }
    let mut live = ImHashMap::new();
    for &ino in tree.nodes.keys() {
        live.insert(self.key(&tree.path_of(ino)).into_owned(), ino);
    }
    self.paths = live;
}
```

- [ ] **Step 5: Update the bulk call sites (mechanical)**

The signature change makes every `InodeAllocator::new()` a compile error. Fix the common (case-sensitive) case in bulk:

```bash
sed -i 's/InodeAllocator::new()/InodeAllocator::new(false)/g' \
  musefs-core/src/tree.rs musefs-core/src/facade.rs
```

- [ ] **Step 6: Flip the case-insensitive call sites to `new(true)`**

The three inline allocators paired with a case-insensitive tree (`build_with_ci(..., true)`) must fold. Run:

```bash
sed -i 's/InodeAllocator::new(false), true)/InodeAllocator::new(true), true)/g' \
  musefs-core/src/tree.rs
```

This targets exactly the three `build_with_ci(&entries, &mut InodeAllocator::new(false), true)` sites (`tree.rs:1077`, `:1097`, `:1128`) and nothing else — the case-sensitive control at `:1111` ends `..., false)` and is left untouched. (The throwaway allocator passed to `remove_track` at `:1132` stays `false` — `remove_track` takes `_alloc` and never interns or prunes, so its flag is irrelevant.) Note `:1128` lives in `case_insensitive_removal_keeps_folded_lookup_consistent`, which is also the Step 9 insertion anchor; flipping its allocator to `new(true)` is correct (the tree is case-insensitive) and harmless.

- [ ] **Step 7: Wire the live allocator to the mount config**

In `musefs-core/src/facade.rs`, the `Musefs::open` allocator must follow the mount's case sensitivity. Change:

```rust
    pub fn open(db: Db, config: MountConfig) -> Result<Musefs> {
        let mut alloc = InodeAllocator::new(false);
```

to:

```rust
    pub fn open(db: Db, config: MountConfig) -> Result<Musefs> {
        let mut alloc = InodeAllocator::new(config.case_insensitive);
```

(The other `facade.rs` site, in a test with `case_insensitive: false`, is correctly left at `new(false)` by Step 5.)

- [ ] **Step 8: Confirm there are no stragglers and the crate builds**

Run:

```bash
grep -rn "InodeAllocator::new()" . ; cargo build -p musefs-core
```

Expected: the `grep` prints **zero results** (every site now passes a bool); `cargo build` succeeds. (A leftover bare `new()` is both a grep hit and a compile error pointing at the exact line.)

- [ ] **Step 9: Write the two regression tests**

Insert these into the `tests` module of `musefs-core/src/tree.rs`, immediately after the `case_insensitive_removal_keeps_folded_lookup_consistent` test:

```rust
#[test]
fn folded_dir_recase_keeps_survivor_inode() {
    // #305: track 1 seen first wins the merged directory's casing ("Foo");
    // track 2 merges under it. Removing track 1 lets the next full rebuild
    // re-derive the casing from track 2 alone, rendering "foo". Track 2's
    // rendered path is unchanged, so its inode must be stable.
    let mut alloc = InodeAllocator::new(true);
    let entries = vec![(1i64, "Foo/A".to_string()), (2i64, "foo/B".to_string())];
    let tree = VirtualTree::build_with_ci(&entries, &mut alloc, true);
    let before = tree.inode_of_track(2).expect("track 2 inode");

    let rebuilt =
        VirtualTree::build_with_ci(&[(2i64, "foo/B".to_string())], &mut alloc, true);
    let after = rebuilt.inode_of_track(2).expect("track 2 inode after rebuild");

    assert_eq!(before, after, "survivor inode must survive a folded dir re-case");
    // Scope is inode stability only: the directory legitimately re-cases to "foo".
    assert!(rebuilt.lookup(VirtualTree::ROOT, "foo").is_some());
}

#[test]
fn folded_inode_key_keeps_disambiguated_siblings_distinct() {
    // Guard that folding the key never collapses a legitimately-disambiguated
    // pair: "Song" and "song" fold equal, so the second becomes "song (2)";
    // their fold-keys ("dir/song" vs "dir/song (2)") differ, so do their inodes.
    let mut alloc = InodeAllocator::new(true);
    let entries = vec![(1i64, "Dir/Song".to_string()), (2i64, "Dir/song".to_string())];
    let tree = VirtualTree::build_with_ci(&entries, &mut alloc, true);
    let a = tree.inode_of_track(1).expect("track 1 inode");
    let b = tree.inode_of_track(2).expect("track 2 inode");
    assert_ne!(a, b, "disambiguated folded siblings must not collapse to one inode");

    let dir = tree.lookup(VirtualTree::ROOT, "Dir").expect("Dir");
    assert_eq!(tree.lookup(dir, "Song"), Some(a));
    assert_eq!(tree.lookup(dir, "song (2)"), Some(b));
}
```

- [ ] **Step 10: Run the new tests — expect PASS**

Run:

```bash
cargo test -p musefs-core folded_dir_recase_keeps_survivor_inode folded_inode_key_keeps_disambiguated_siblings_distinct
```

Expected: both tests PASS.

- [ ] **Step 11: Prove the tests guard the fix (temporary revert)**

Confirm the tests actually fail without the fold. Temporarily change the `key()` true-branch from `Cow::Owned(fold(path))` to `Cow::Borrowed(path)`, then run:

```bash
cargo test -p musefs-core folded_dir_recase_keeps_survivor_inode
```

Expected: FAIL on `assert_eq!(before, after, ...)` (the survivor gets a fresh inode). **Restore** the `Cow::Owned(fold(path))` line and re-run — expected: PASS.

- [ ] **Step 12: Full crate gate**

Run:

```bash
cargo fmt
cargo clippy --all-targets -- -D warnings
cargo test -p musefs-core
```

Expected: clippy clean (no `dead_code`/`if_same_then_else`), all `musefs-core` tests green — including the existing `case_*` and `prune_retired_*` tests, which pin the unchanged case-sensitive contract.

- [ ] **Step 13: Commit**

```bash
git add musefs-core/src/tree.rs musefs-core/src/facade.rs
git commit -m "$(cat <<'EOF'
fix(core): fold inode key in case-insensitive mode (#305)

In case-insensitive mounts a merged directory keeps the first-seen child's
display casing. A full folded rebuild after an unrelated deletion can flip
that casing, changing a survivor's disambiguated display path and — because
InodeAllocator keyed on that path — reassigning its inode despite a stable
rendered path. Key the allocator on the case-folded path in folded mode so
the inode is casing-independent; case-sensitive mounts are unchanged.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>
EOF
)"
```

The pre-commit hook runs the full workspace suite; this commit is green.

---

## Task 2: Document the case-folded key in ARCHITECTURE.md

**Files:**
- Modify: `ARCHITECTURE.md` (inode-stability paragraph, ~`:323-330`)

- [ ] **Step 1: Add the folding clause**

In the "Inodes are **stable across rebuilds**" paragraph, change:

```
(`InodeAllocator`) reuses an unchanged rendered path's inode and never
recycles a retired one, so a descriptor held open across a refresh keeps
resolving to the same node and a stale FUSE handle can never alias a
different file.
```

to:

```
(`InodeAllocator`) reuses an unchanged rendered path's inode and never
recycles a retired one, so a descriptor held open across a refresh keeps
resolving to the same node and a stale FUSE handle can never alias a
different file. On case-insensitive mounts the key is case-folded, so a
survivor keeps its inode even when an unrelated deletion flips a merged
directory's display casing (#305).
```

- [ ] **Step 2: Commit (docs-only)**

```bash
git add ARCHITECTURE.md
git commit -m "$(cat <<'EOF'
docs(arch): note case-folded inode key on folded mounts (#305)

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>
EOF
)"
```

(Docs-only commit; the pre-commit hook skips the cargo gate.)

---

## Definition of done

- [ ] `folded_dir_recase_keeps_survivor_inode` and `folded_inode_key_keeps_disambiguated_siblings_distinct` pass, and were shown to fail without the fold (Task 1 Step 11).
- [ ] `cargo fmt --all --check`, `cargo clippy --all-targets -- -D warnings`, and `cargo test` are all green (the full workspace suite runs in the pre-commit hook).
- [ ] No production behavior change for case-sensitive mounts (existing `case_*`/`prune_retired_*` tests unchanged and green).
- [ ] In-diff mutation gate: run the local in-diff `cargo mutants --in-place` gate over the diff before pushing. The `key()` mutants are already covered — the never-fold mutants are killed by `folded_dir_recase_keeps_survivor_inode`, and the always-fold mutant (condition → `true`) is killed by the existing `case_sensitive_keeps_both_dirs_separate` test, whose `assert_ne!(lookup "Foo", lookup "foo")` compares the interned dir inodes (which the mutant would collide onto one). No extra assertion or `exclude_re` is expected; if the run reports an unkilled `key()` survivor anyway, add an `assert_ne!`-on-inodes line to that case-sensitive test rather than excluding it.
