# InodeAllocator Pruning + DbPool Residuals Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Bound the `InodeAllocator`'s path→inode map over long-lived mounts (#126) and make `DbPool`'s `with`/`with_poll` nest-safe on every variant while documenting the Drop lifetime contract (#127).

**Architecture:** Two independent fixes in `musefs-core`, shipped as **two separate PRs**. PR 1 adds a threshold-amortized prune to `InodeAllocator` (`tree.rs`) called from the two rebuild sites in `facade.rs`. PR 2 swaps `std::sync::Mutex` for `parking_lot::ReentrantMutex` in `db_pool.rs` (parking_lot is already compiled via fuser) and promotes the Drop residual into a documented contract.

**Tech Stack:** Rust, `im` persistent maps, `parking_lot` 0.12, cargo-mutants in-diff gate.

**Spec:** `docs/superpowers/specs/2026-06-04-inode-prune-dbpool-residuals-design.md`

**Branching:** Both branches cut from current local `main` (which carries the spec commits — they ride with whichever PR merges first; rebase the other before merge). Repo `main` is protected; everything lands via PR.

---

## PR 1 — #126: threshold-amortized InodeAllocator prune

### Task 1: Branch for PR 1

**Files:** none

- [ ] **Step 1: Create the branch**

```bash
cd /home/cfutro/git/musefs
git checkout main && git checkout -b 126-inode-allocator-prune
```

### Task 2: `InodeAllocator::prune_retired` (TDD)

**Files:**
- Modify: `musefs-core/src/tree.rs` (struct doc at lines 13–16, new method in `impl InodeAllocator` after `intern`, new tests in `mod tests`)

The allocator and `VirtualTree` live in the same module, so the method may read `tree.nodes` and call the private `tree.path_of` directly. The `tests` module is a child of the same module and may read the private `paths` field.

- [ ] **Step 1: Write the failing tests**

Add to `mod tests` in `musefs-core/src/tree.rs` (alongside `build_with_does_not_recycle_a_vanished_inode`):

```rust
    #[test]
    fn prune_retired_bounds_map_under_churn() {
        let mut alloc = InodeAllocator::new();
        for gen in 0..100 {
            let entries = vec![(1, format!("Gen{gen}/a.flac"))];
            let tree = VirtualTree::build_with(&entries, &mut alloc);
            alloc.prune_retired(&tree);
            assert!(
                alloc.paths.len() <= 2 * tree.nodes.len(),
                "gen {gen}: map {} exceeds 2x live {}",
                alloc.paths.len(),
                tree.nodes.len()
            );
        }
    }

    #[test]
    fn prune_retired_keeps_live_inodes_stable() {
        let mut alloc = InodeAllocator::new();
        let tree = VirtualTree::build_with(&[(1, "Keep/song.flac".into())], &mut alloc);
        let keep_dir = tree.lookup(VirtualTree::ROOT, "Keep").unwrap();
        let keep_file = tree.lookup(keep_dir, "song.flac").unwrap();
        let mut last = tree;
        for gen in 0..10 {
            let entries = vec![
                (1, "Keep/song.flac".to_string()),
                (2, format!("Gen{gen}/x.flac")),
            ];
            last = VirtualTree::build_with(&entries, &mut alloc);
            alloc.prune_retired(&last);
        }
        let d = last.lookup(VirtualTree::ROOT, "Keep").unwrap();
        let f = last.lookup(d, "song.flac").unwrap();
        assert_eq!((d, f), (keep_dir, keep_file), "live paths must keep inodes");
    }

    #[test]
    fn pruned_path_reborn_gets_fresh_inode_never_recycled() {
        let mut alloc = InodeAllocator::new();
        let t1 = VirtualTree::build_with(&[(1, "Gone/x.flac".into())], &mut alloc);
        let gone_dir = t1.lookup(VirtualTree::ROOT, "Gone").unwrap();
        let gone_file = t1.lookup(gone_dir, "x.flac").unwrap();
        // Churn well past the threshold so a prune drops the retired entries.
        for gen in 0..10 {
            let t = VirtualTree::build_with(&[(1, format!("Gen{gen}/x.flac"))], &mut alloc);
            alloc.prune_retired(&t);
        }
        assert!(!alloc.paths.contains_key("Gone"), "retired path must be pruned");
        // Rebirth: same rendered path, strictly fresh inodes (next is monotone).
        let t2 = VirtualTree::build_with(&[(1, "Gone/x.flac".into())], &mut alloc);
        let d2 = t2.lookup(VirtualTree::ROOT, "Gone").unwrap();
        let f2 = t2.lookup(d2, "x.flac").unwrap();
        assert!(d2 > gone_file && f2 > gone_file, "fresh inodes, never recycled");
        assert_ne!(d2, gone_dir);
        assert_ne!(f2, gone_file);
    }

    #[test]
    fn prune_retired_waits_for_threshold() {
        // Drives the map to exactly 2x live nodes: prune must NOT fire at
        // equality (pins the `<=` boundary for the mutation gate).
        let mut alloc = InodeAllocator::new();
        let t1 = VirtualTree::build_with(&[(1, "A/x.flac".into())], &mut alloc);
        let a_dir = t1.lookup(VirtualTree::ROOT, "A").unwrap();
        // paths: "", A, A/x.flac = 3
        let t2 = VirtualTree::build_with(&[(1, "B/x.flac".into())], &mut alloc);
        alloc.prune_retired(&t2); // paths 5, live 3 -> 5 <= 6, no prune
        let t3 = VirtualTree::build_with(&[(1, "B/y.flac".into())], &mut alloc);
        alloc.prune_retired(&t3); // paths 6, live 3 -> 6 <= 6, still no prune
        assert_eq!(
            alloc.paths.get("A"),
            Some(&a_dir),
            "at exactly 2x live the retired entries must survive"
        );
        let t4 = VirtualTree::build_with(&[(1, "C/x.flac".into())], &mut alloc);
        alloc.prune_retired(&t4); // paths 8 > 6: prune fires
        assert!(!alloc.paths.contains_key("A"), "past 2x live the prune must fire");
        assert_eq!(alloc.paths.len(), t4.nodes.len(), "pruned map is exactly the live set");
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

```bash
cargo test -p musefs-core --lib tree::tests::prune
```

Expected: compile error — `no method named 'prune_retired' found for struct 'InodeAllocator'`.

- [ ] **Step 3: Implement `prune_retired`**

In `musefs-core/src/tree.rs`, add to `impl InodeAllocator` directly after `intern` (line 39):

```rust
    /// Rebuild `paths` from the live tree once retired entries outnumber live
    /// ones (map > 2x live nodes), keeping each live path's existing inode.
    /// `next` is untouched, so a retired inode is never reissued. A retired
    /// path that reappears after a prune gets a fresh inode: a kernel dentry
    /// cached for its old inode resolves ENOENT for at most one entry TTL,
    /// the same degradation as any vanished path.
    pub(crate) fn prune_retired(&mut self, tree: &VirtualTree) {
        if self.paths.len() <= 2 * tree.nodes.len() {
            return;
        }
        let mut live = ImHashMap::new();
        for &ino in tree.nodes.keys() {
            live.insert(tree.path_of(ino), ino);
        }
        self.paths = live;
    }
```

(`path_of(ROOT)` returns `""`, reproducing the root entry `new()` seeds.)

- [ ] **Step 4: Update the struct doc comment**

Replace lines 13–16 of `musefs-core/src/tree.rs`:

```rust
/// Assigns stable inodes keyed by rendered path, persisted across tree rebuilds:
/// an unchanged path keeps its inode, a new path gets a fresh one, and a retired
/// inode is never recycled (a stale FUSE handle can't alias a different node).
/// The map grows monotonically with the universe of distinct paths ever rendered.
```

with:

```rust
/// Assigns stable inodes keyed by rendered path, persisted across tree rebuilds:
/// an unchanged path keeps its inode, a new path gets a fresh one, and a retired
/// inode is never recycled (a stale FUSE handle can't alias a different node).
/// Retired paths are dropped by `prune_retired` once they outnumber live ones,
/// bounding the map at 2x the live tree between prunes; a path that returns
/// after a prune gets a fresh inode rather than its old one.
```

- [ ] **Step 5: Run the tests to verify they pass**

```bash
cargo test -p musefs-core --lib tree::
```

Expected: all `tree::tests` pass, including the four new tests and the untouched `build_with_keeps_inodes_stable_across_rebuilds` / `build_with_does_not_recycle_a_vanished_inode`.

- [ ] **Step 6: Commit**

```bash
git add musefs-core/src/tree.rs
git commit -m "Add threshold-amortized prune to InodeAllocator (#126)"
```

### Task 3: Wire the prune into both facade rebuild sites

**Files:**
- Modify: `musefs-core/src/facade.rs:303-305` (`rebuild_full`) and `musefs-core/src/facade.rs:468-470` (incremental site)

Both sites already hold the `inodes` lock; the call goes on the guard (auto-deref).

- [ ] **Step 1: Prune in `rebuild_full`**

In `musefs-core/src/facade.rs`, `rebuild_full` (~line 303), change:

```rust
        let mut alloc = crate::lock::lock_or_flag(&self.inodes, &self.needs_rebuild, "inodes");
        let tree = VirtualTree::build_with(&entries, &mut alloc);
        drop(alloc);
```

to:

```rust
        let mut alloc = crate::lock::lock_or_flag(&self.inodes, &self.needs_rebuild, "inodes");
        let tree = VirtualTree::build_with(&entries, &mut alloc);
        alloc.prune_retired(&tree);
        drop(alloc);
```

- [ ] **Step 2: Prune in the incremental site**

In the incremental rebuild (~line 468), on the unified `tree` after the `match applied { ... }` block — this covers both the incremental-success arm and the fallback-`build_with` arm — change:

```rust
        };
        self.tree.store(Arc::new(tree));
        drop(alloc);
```

to:

```rust
        };
        alloc.prune_retired(&tree);
        self.tree.store(Arc::new(tree));
        drop(alloc);
```

(The `#[cfg(debug_assertions)]` parity check clones the allocator *before* this point and is unaffected. `Musefs::open` needs no prune: its allocator is fresh, so the map equals the live set.)

- [ ] **Step 3: Run the crate test suite**

```bash
cargo test -p musefs-core
```

Expected: PASS (the facade refresh/rebuild tests exercise both sites; pruning must not perturb inode stability for live paths).

- [ ] **Step 4: Commit**

```bash
git add musefs-core/src/facade.rs
git commit -m "Prune retired allocator paths after facade rebuilds (#126)"
```

### Task 4: PR 1 verification gate and PR

**Files:** none (verification only)

- [ ] **Step 1: Format, lint, full test**

```bash
cargo fmt --all && cargo fmt --all --check
cargo clippy --all-targets
cargo test
```

Expected: all clean. (CI has a hard fmt gate — check the exit status directly.)

- [ ] **Step 2: In-diff mutation gate (CI parity)**

```bash
git diff "$(git merge-base main HEAD)...HEAD" -- '*.rs' > mutants.diff
grep -q '^@@ ' mutants.diff
cargo mutants --in-diff mutants.diff -j2 --exclude 'musefs-latencyfs/**' --output /tmp/mutants-out/in-diff
```

Expected: `grep` exits 0 (non-empty diff — an empty diff is a silent false pass) and no missed mutants. The `prune_retired_waits_for_threshold` test exists to pin the `<=` boundary and the `2 *` factor. If a mutant at the *facade call sites* survives (cargo-mutants mutating `rebuild_full`'s body), extend the facade test that covers refresh rather than weakening the gate.

- [ ] **Step 3: Confirm with the user, then push and open PR 1**

External action — get explicit go-ahead first.

```bash
git push -u origin 126-inode-allocator-prune
gh pr create --title "Bound the InodeAllocator with a threshold-amortized prune" --body "$(cat <<'EOF'
Closes #126.

`InodeAllocator` was insert-only: retired rendered paths were kept forever so
inodes stay stable across rebuilds, growing the map monotonically over a
long-lived mount with tag churn. Now, after any rebuild, once the map exceeds
2x the live node count it is rebuilt from the live tree (each live path keeps
its existing inode). `next` is untouched, so a retired inode is never
reissued and the no-aliasing invariant is unchanged. Trade-off (per spec): a
path that vanishes and returns after a prune gets a fresh inode; the stale
kernel dentry degrades to ENOENT for at most one entry TTL, the existing
documented behavior for vanished paths.

Spec: docs/superpowers/specs/2026-06-04-inode-prune-dbpool-residuals-design.md

🤖 Generated with [Claude Code](https://claude.com/claude-code)
EOF
)"
```

---

## PR 2 — #127: DbPool ReentrantMutex + Drop contract

### Task 5: Branch for PR 2

**Files:** none

- [ ] **Step 1: Create the branch**

```bash
cd /home/cfutro/git/musefs
git checkout main && git checkout -b 127-dbpool-reentrant
```

(If PR 1 already merged, `git fetch origin && git checkout -b 127-dbpool-reentrant origin/main` instead; otherwise the spec commits ride along and this branch is rebased after PR 1 lands.)

### Task 6: Nested-call tests (TDD — these currently deadlock)

**Files:**
- Modify: `musefs-core/src/db_pool.rs` (`mod tests`)

- [ ] **Step 1: Write the failing tests**

Add to `mod tests` in `musefs-core/src/db_pool.rs`:

```rust
    #[test]
    fn nested_with_on_shared_pool() {
        let pool = DbPool::new(Db::open_in_memory().unwrap()).unwrap();
        let r: Result<i64> = pool.with(|_outer| pool.with(|db| Ok(db.data_version()?)));
        assert!(r.is_ok(), "nested with on Shared must not deadlock");
    }

    #[test]
    fn with_poll_inside_with_on_shared_pool() {
        let pool = DbPool::new(Db::open_in_memory().unwrap()).unwrap();
        let r: Result<i64> = pool.with(|_outer| pool.with_poll(|db| Ok(db.data_version()?)));
        assert!(r.is_ok(), "with_poll inside with on Shared must not deadlock");
    }

    #[test]
    fn nested_with_poll_on_per_thread_pool() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("np.db");
        Db::open(&path).unwrap();
        let pool = DbPool::new(Db::open(&path).unwrap()).unwrap();
        let r: Result<i64> =
            pool.with_poll(|_outer| pool.with_poll(|db| Ok(db.data_version()?)));
        assert!(r.is_ok(), "nested with_poll on PerThread must not deadlock");
    }
```

- [ ] **Step 2: Run them to verify they currently hang (bounded by timeout)**

A deadlock hangs rather than fails, so bound the run:

```bash
timeout 30 cargo test -p musefs-core --lib db_pool::tests::nested_with_on_shared_pool -- --test-threads=1
echo "exit: $?"
```

Expected: `exit: 124` (timeout killed the hung test — this is the deadlock signature). Any other non-zero exit (panic, assert) is NOT the expected failure — investigate before proceeding. Do NOT run the other two tests the same way; they hang identically (same root cause).

- [ ] **Step 3: Commit the tests only if the migration follows immediately**

Skip a standalone commit here — a committed deadlocking test would hang any intermediate checkout. The tests land in the same commit as the fix (Task 7 Step 6).

### Task 7: Migrate `db_pool.rs` to `parking_lot::ReentrantMutex`

**Files:**
- Modify: `musefs-core/Cargo.toml:19` (dependencies)
- Modify: `musefs-core/src/db_pool.rs` (imports, enum, `Drop`, `new`, `with_poll`, `with`, one existing test)

- [ ] **Step 1: Add the dependency**

In `musefs-core/Cargo.toml` `[dependencies]` (alphabetical, after `once_cell`):

```toml
parking_lot = "0.12"
```

(Already in `Cargo.lock` at 0.12.5 via fuser — no new compiled code in real builds.)

- [ ] **Step 2: Swap the imports**

In `musefs-core/src/db_pool.rs`, replace:

```rust
use std::sync::{Arc, Mutex};
```

with:

```rust
use std::sync::Arc;

use parking_lot::ReentrantMutex;
```

- [ ] **Step 3: Migrate the enum and document both contracts**

Replace the `DbPool` enum and the `Drop` impl's leading comment:

```rust
/// `with` and `with_poll` may nest freely, on any variant: `PerThread` reads
/// hand out thread-local `Rc` clones, and both mutexes are reentrant.
///
/// Lifetime contract (#127, accepted by design): `PerThread` read connections
/// are owned by the threads that opened them and live until those threads
/// exit. Dropping the pool clears only the dropping thread's connection, so a
/// caller that creates and drops many pools on long-lived shared threads
/// strands connections (one fd + SQLite cache each) until those threads exit.
/// Fine for musefs — a mount's worker pool lives exactly as long as the mount;
/// closing the gap would need a cross-thread registry, deliberately not built
/// (`Rc<Db>` is `!Send`, so another thread's entry is unreachable anyway).
pub enum DbPool {
    PerThread {
        id: u64,
        path: PathBuf,
        poll: ReentrantMutex<Db>,
    },
    Shared(Arc<ReentrantMutex<Db>>),
}

/// Clears only the *dropping thread's* thread-local connection for this pool —
/// see the lifetime contract on [`DbPool`].
impl Drop for DbPool {
```

(The `Drop` body itself is unchanged.)

- [ ] **Step 4: Migrate `new`, `with_poll`, and `with`**

In `DbPool::new`, change `poll: Mutex::new(db)` to `poll: ReentrantMutex::new(db)` and `DbPool::Shared(Arc::new(Mutex::new(db)))` to `DbPool::Shared(Arc::new(ReentrantMutex::new(db)))`.

Replace `with_poll` (the doc comment loses its deadlock warning — nesting is now legal; parking_lot doesn't poison, so the recovery dance goes too):

```rust
    /// Run `f` with the persistent poll connection.
    ///
    /// For `PerThread` pools, `PRAGMA data_version` is connection-relative: a fresh
    /// thread-local connection starts at 0, so it can't detect changes that happened
    /// before it opened. The poll connection is the original writer Db, kept alive
    /// precisely so it can observe incremental changes from other connections.
    /// For `Shared` pools (in-memory), the single shared connection serves both roles.
    pub fn with_poll<R>(&self, f: impl FnOnce(&Db) -> Result<R>) -> Result<R> {
        match self {
            DbPool::PerThread { poll, .. } => f(&poll.lock()),
            DbPool::Shared(m) => f(&m.lock()),
        }
    }
```

And in `with`, replace the `Shared` arm:

```rust
            DbPool::Shared(m) => {
                let db = m.lock();
                f(&db)
            }
```

(`ReentrantMutexGuard` derefs to `&Db`, the exact shape `f` takes. Soundness: `ReentrantMutex<T>: Sync` needs only `T: Send` — `rusqlite::Connection` is `Send` — and same-thread multi-borrow is the single-threaded pattern `Connection`'s internal `RefCell` assumes.)

- [ ] **Step 5: Fix the direct-construction test**

In `with_open_failure_includes_path_in_error` (`db_pool.rs:184`), change `poll: Mutex::new(Db::open_in_memory().unwrap())` to `poll: ReentrantMutex::new(Db::open_in_memory().unwrap())`.

- [ ] **Step 6: Run the tests and commit**

```bash
cargo test -p musefs-core --lib db_pool::
```

Expected: PASS — the three new nested tests plus all five existing `db_pool` tests (`reentrant_with_does_not_panic` unchanged).

```bash
git add musefs-core/Cargo.toml Cargo.lock musefs-core/src/db_pool.rs
git commit -m "Make DbPool nesting legal via ReentrantMutex; document Drop contract (#127)"
```

### Task 8: PR 2 verification gate and PR

**Files:** none (verification only)

- [ ] **Step 1: Format, lint, full test**

```bash
cargo fmt --all && cargo fmt --all --check
cargo clippy --all-targets
cargo test
```

Expected: all clean.

- [ ] **Step 2: In-diff mutation gate (CI parity)**

```bash
git diff "$(git merge-base main HEAD)...HEAD" -- '*.rs' > mutants.diff
grep -q '^@@ ' mutants.diff
cargo mutants --in-diff mutants.diff -j2 --exclude 'musefs-latencyfs/**' --output /tmp/mutants-out/in-diff
```

Expected: non-empty diff, no missed mutants (the `with`/`with_poll` arms are pinned by the existing path/data_version assertions plus the three nested tests).

- [ ] **Step 3: Confirm with the user, then push and open PR 2**

External action — get explicit go-ahead first.

```bash
git push -u origin 127-dbpool-reentrant
gh pr create --title "DbPool: reentrant locks + documented Drop contract" --body "$(cat <<'EOF'
Closes #127.

Two residuals after #94:

- **Nested-call deadlock (fixed):** `with`/`with_poll` on the `Shared` variant
  (and `with_poll` nesting on `PerThread`) deadlocked on a non-reentrant
  `std::sync::Mutex`, while nested `with` already worked on `PerThread` — a
  test-vs-prod asymmetry. Both mutexes are now `parking_lot::ReentrantMutex`
  (already compiled via fuser), so nesting is legal on every variant; the
  poison-recovery dance at these two sites goes away with it.
- **Drop leaving other threads' connections (accepted by design, now
  documented):** cross-thread cleanup is unreachable (`Rc<Db>` is `!Send`) and
  only matters for an embedder that drops many pools on long-lived shared
  threads, which doesn't exist; the lifetime contract is now spelled out on
  the `DbPool` type instead of an internal comment.

Spec: docs/superpowers/specs/2026-06-04-inode-prune-dbpool-residuals-design.md

🤖 Generated with [Claude Code](https://claude.com/claude-code)
EOF
)"
```
