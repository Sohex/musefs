# DbPool Pool-Owned Connections Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make `DbPool::drop` close every per-thread SQLite read connection from whatever thread drops the pool, by inverting ownership: the pool owns its connections in a `ThreadId`-keyed map instead of a global `thread_local!`.

**Architecture:** Rewrite the `PerThread` variant of `musefs_core::DbPool` (`musefs-core/src/db_pool.rs`) to hold `conns: Mutex<HashMap<ThreadId, Arc<ReentrantMutex<Db<ReadOnly>>>>>`. The manual `Drop` impl, the `thread_local! PER_PATH` map, the `NEXT_POOL_ID` counter, and the `id` field are all deleted — the compiler-generated drop of `conns` closes every connection. Public API (`new`/`with`/`with_poll`) is unchanged; the only consumer is `musefs-core/src/facade.rs` and it needs no edits.

**Tech Stack:** Rust, `parking_lot` (`Mutex`, `ReentrantMutex` — already a dependency), `rusqlite` via `musefs-db`. Tests use `/proc/self/fd` readlink (Linux-only, like the rest of the suite).

**Spec:** `docs/superpowers/specs/2026-06-07-dbpool-owned-connections-design.md` — read it before starting. Key invariants: the map guard is NEVER held while the caller's closure runs (would reintroduce the nested-`with` deadlock PR #144 fixed); the inner `ReentrantMutex` is uncontended but load-bearing for `Send + Sync` (`Db` is `Send + !Sync`); contention-free per-thread WAL reads and `with_poll`'s `data_version` semantics are unchanged.

**Branch:** `main` is protected. Create a working branch first: `git checkout -b dbpool-owned-connections` (or execute in a worktree per superpowers:using-git-worktrees).

---

### Task 1: Failing tests — pool drop closes connections

The two new tests encode the bug: today, `DbPool::drop` only clears the dropping thread's `thread_local!` entry, so both tests FAIL against the current implementation. They must be written and seen red before the rewrite.

**Files:**
- Modify: `musefs-core/src/db_pool.rs` (tests module only, currently lines 124–230)

- [ ] **Step 1: Add the fd-counting helper and the two failing tests**

Add inside `mod tests` in `musefs-core/src/db_pool.rs` (after the existing imports `use super::*; use musefs_db::Db;`):

```rust
    /// Count this process's open fds whose target path starts with `db_path`.
    /// Prefix match deliberately: a WAL reader holds up to three fds
    /// (`db`, `db-wal`, `db-shm`).
    fn db_fd_count(db_path: &std::path::Path) -> usize {
        let prefix = db_path.to_str().unwrap();
        std::fs::read_dir("/proc/self/fd")
            .unwrap()
            .filter_map(|e| std::fs::read_link(e.unwrap().path()).ok())
            .filter(|target| target.to_string_lossy().starts_with(prefix))
            .count()
    }

    #[test]
    fn drop_closes_connections_opened_by_live_threads() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("d.db");
        Db::open(&path).unwrap(); // create + migrate (writer, sets WAL)
        let baseline = db_fd_count(&path);

        let pool = Arc::new(DbPool::new(Db::open(&path).unwrap()).unwrap());
        // 2 workers + the main thread; workers park here until main has asserted.
        let barrier = Arc::new(std::sync::Barrier::new(3));
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let mut handles = Vec::new();
        for _ in 0..2 {
            let pool = Arc::clone(&pool);
            let barrier = Arc::clone(&barrier);
            let done = done_tx.clone();
            handles.push(std::thread::spawn(move || {
                pool.with(|db| Ok(db.data_version()?)).unwrap();
                drop(pool); // this thread's Arc clone; main's is then the last
                done.send(()).unwrap();
                barrier.wait();
            }));
        }
        // Count exactly two done-signals; don't drain-until-Err — the workers
        // still hold their sender clones while parked at the barrier.
        drop(done_tx);
        for _ in 0..2 {
            done_rx.recv().unwrap();
        }

        // Both workers are done using the pool but still alive (parked at, or
        // headed to, the barrier — they cannot pass it until main waits too).
        drop(pool);
        assert_eq!(
            db_fd_count(&path),
            baseline,
            "pool drop must close all threads' connections while those threads are alive"
        );

        barrier.wait();
        for h in handles {
            h.join().unwrap();
        }
    }

    #[test]
    fn drop_on_foreign_thread_closes_all_connections() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.db");
        Db::open(&path).unwrap();
        let baseline = db_fd_count(&path);

        let pool = DbPool::new(Db::open(&path).unwrap()).unwrap();
        pool.with(|db| Ok(db.data_version()?)).unwrap(); // opens this thread's connection

        // DbPool is Send: drop it on a thread that never opened a connection.
        std::thread::spawn(move || drop(pool)).join().unwrap();

        assert_eq!(
            db_fd_count(&path),
            baseline,
            "drop on a foreign thread must still close every connection"
        );
    }
```

- [ ] **Step 2: Run the new tests and verify they FAIL**

Run: `cargo test -p musefs-core db_pool`

Expected: the two new tests FAIL on their `assert_eq!` (fd count above baseline — the current TLS design leaks the workers'/main's connections), every pre-existing `db_pool` test still PASSES. If a new test passes here, STOP — the test is not observing the leak (most likely the readlink prefix doesn't match; check `db_fd_count` against a hand-opened `Db`).

- [ ] **Step 3: Do NOT commit yet**

The pre-commit hook (`.githooks/pre-commit`) runs `cargo test --workspace`, so
a commit with deliberately-red tests is guaranteed to be rejected. Never
bypass it with `--no-verify`. The red state was verified in Step 2; these
tests are committed together with the implementation in Task 2 Step 5.

---

### Task 2: Rewrite DbPool internals — pool-owned connection map

**Files:**
- Modify: `musefs-core/src/db_pool.rs` (everything above `mod tests`, currently lines 1–122, plus one test literal at ~lines 189–202)

- [ ] **Step 1: Replace the non-test body of the file**

Replace everything above `#[cfg(test)] mod tests` in `musefs-core/src/db_pool.rs` with:

```rust
//! Hands a read connection to whichever thread needs one.
//!
//! - File-backed DB → each thread lazily opens its own read-only connection
//!   (WAL makes concurrent readers contention-free; the worker pool is bounded,
//!   so the connection count is bounded).
//! - In-memory DB (tests) cannot be reopened by path, so a single connection is
//!   shared behind a mutex.
//!
//! The pool owns every connection it opens, keyed by the opening thread, so
//! dropping the pool closes all of them — from any thread (#127). A thread
//! that dies while the pool lives leaves its connection in the map until the
//! pool is dropped; that bound is the pool's lifetime, not the thread's.

use std::collections::hash_map::Entry;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::thread::ThreadId;

use parking_lot::{Mutex, ReentrantMutex};

use musefs_db::{Db, ReadOnly};

use crate::error::{CoreError, Result};

/// `with` and `with_poll` may nest freely, on any variant: per-thread reads
/// hand out `Arc` clones of this thread's own connection, and every mutex a
/// caller's closure can re-enter is reentrant.
///
/// The `poll`/`conns` asymmetry is deliberate. `poll` is a bare
/// `ReentrantMutex` because the pool owns it directly and `with_poll` takes
/// no other lock. `conns` values are `Arc`-wrapped so `with` can clone a
/// handle and release the map guard *before* running the caller's closure —
/// holding the (non-reentrant) map guard across it would deadlock nested
/// `with`. The inner `ReentrantMutex` is never contended (only its owning
/// thread locks it) but is load-bearing for the type system: `Db` is
/// `Send + !Sync`, so the mutex wrapper is what keeps the map field, and
/// therefore `DbPool`, `Send + Sync`.
pub enum DbPool {
    PerThread {
        path: PathBuf,
        poll: ReentrantMutex<Db<ReadOnly>>,
        conns: Mutex<HashMap<ThreadId, Arc<ReentrantMutex<Db<ReadOnly>>>>>,
    },
    Shared(Arc<ReentrantMutex<Db<ReadOnly>>>),
}

impl DbPool {
    /// Build a pool from the DB used to construct the mount. File-backed DBs
    /// become per-thread pools (the passed connection becomes the poll
    /// connection — workers open their own); in-memory DBs are wrapped in a
    /// shared mutex.
    pub fn new(db: Db) -> Result<DbPool> {
        let db = db.into_read_only();
        match db.path() {
            Some(p) => Ok(DbPool::PerThread {
                path: p.to_path_buf(),
                poll: ReentrantMutex::new(db),
                conns: Mutex::new(HashMap::new()),
            }),
            None => Ok(DbPool::Shared(Arc::new(ReentrantMutex::new(db)))),
        }
    }

    /// Run `f` with the persistent poll connection.
    ///
    /// For `PerThread` pools, `PRAGMA data_version` is connection-relative: a fresh
    /// thread-local connection starts at 0, so it can't detect changes that happened
    /// before it opened. The poll connection is the original writer Db, kept alive
    /// precisely so it can observe incremental changes from other connections.
    /// For `Shared` pools (in-memory), the single shared connection serves both roles.
    pub fn with_poll<R>(&self, f: impl FnOnce(&Db<ReadOnly>) -> Result<R>) -> Result<R> {
        match self {
            DbPool::PerThread { poll, .. } => f(&poll.lock()),
            DbPool::Shared(m) => f(&m.lock()),
        }
    }

    /// Run `f` with this thread's read connection, opening it on first use.
    pub fn with<R>(&self, f: impl FnOnce(&Db<ReadOnly>) -> Result<R>) -> Result<R> {
        match self {
            DbPool::PerThread { path, conns, .. } => {
                let conn = {
                    let mut map = conns.lock();
                    match map.entry(std::thread::current().id()) {
                        Entry::Occupied(e) => Arc::clone(e.get()),
                        Entry::Vacant(e) => {
                            let db =
                                Db::open_readonly(path).map_err(|source| CoreError::DbOpen {
                                    path: path.clone(),
                                    source,
                                })?;
                            Arc::clone(e.insert(Arc::new(ReentrantMutex::new(db))))
                        }
                    }
                    // map guard dropped here, before f runs — see the type docs
                };
                f(&conn.lock())
            }
            DbPool::Shared(m) => f(&m.lock()),
        }
    }
}
```

Deleted relative to the old file: the `Drop` impl, `NEXT_POOL_ID`, the `id` field, `thread_local! PER_PATH`, the `PerPathMap` alias, the "lifetime contract (#127, accepted by design)" doc block, and the now-unused imports (`std::cell::RefCell`, `std::rc::Rc`, `std::sync::atomic::{AtomicU64, Ordering}`).

- [ ] **Step 2: Adapt the hand-constructed `PerThread` literal in the error test**

In `mod tests`, `with_open_failure_includes_path_in_error` builds the variant directly. Replace:

```rust
        let pool = DbPool::PerThread {
            id: u64::MAX,
            path: bad.clone(),
            poll: ReentrantMutex::new(Db::open_in_memory().unwrap().into_read_only()),
        };
```

with:

```rust
        let pool = DbPool::PerThread {
            path: bad.clone(),
            poll: ReentrantMutex::new(Db::open_in_memory().unwrap().into_read_only()),
            conns: Mutex::new(HashMap::new()),
        };
```

- [ ] **Step 3: Run the db_pool tests — all green, including Task 1's two**

Run: `cargo test -p musefs-core db_pool`

Expected: all tests PASS — the eight pre-existing ones (reentrancy, nesting, two-pools-same-path, poll, error-path) plus `drop_closes_connections_opened_by_live_threads` and `drop_on_foreign_thread_closes_all_connections`.

- [ ] **Step 4: Run the full workspace suite**

Run: `cargo test`

Expected: PASS across all crates (the FUSE e2e tests stay `#[ignore]`d). The only consumer (`facade.rs`) uses `with`/`with_poll` only, so nothing else should need edits — a failure elsewhere means the rewrite changed observable behavior; stop and investigate rather than patching the caller.

- [ ] **Step 5: Commit (tests from Task 1 + implementation together)**

This is the first commit of the branch — it carries Task 1's tests too, since
the pre-commit hook's workspace test run forbids committing them red.

```bash
git add musefs-core/src/db_pool.rs
git commit -m "fix: DbPool owns its connections; drop closes them from any thread (#127)"
```

---

### Task 3: Verification gates (CI parity)

**Files:** none (verification only; fixes, if any, amend nothing — new commits)

- [ ] **Step 1: Format and lint exactly as CI does**

Run: `cargo fmt --all --check && cargo clippy --all-targets`

Expected: both exit 0. If fmt rejects, run `cargo fmt --all`, re-run the check, and commit the formatting as part of a new commit. (clippy `--all-targets` matters: benches and integration-test dirs hold API consumers a plain `cargo build` misses.)

- [ ] **Step 2: In-diff mutation gate**

Run (verbatim from CLAUDE.md — `/tmp` here is RAM-backed tmpfs and some mutants are allocation bombs, hence the cgroup + on-disk TMPDIR; don't pipe through tail/grep):

```bash
git diff "$(git merge-base main HEAD)...HEAD" -- '*.rs' > mutants.diff
grep -q '^@@ ' mutants.diff
mkdir -p ~/.cache/musefs-mutants-tmp
TMPDIR="$HOME/.cache/musefs-mutants-tmp" systemd-run --user --scope --collect \
    -p MemoryMax=10G -p MemorySwapMax=0 \
    cargo mutants --in-diff mutants.diff -j2 --exclude 'musefs-latencyfs/**' --output /tmp/mutants-out/in-diff
```

Expected: `grep` exits 0 (non-empty diff — an empty diff is a silent false pass), and `cargo mutants` reports 0 missed. Surviving mutants in `with`'s map handling mean a coverage gap — add a targeted test rather than excluding, unless the mutant is genuinely unobservable (then a documented `exclude_re` in `.cargo/mutants.toml`, per the established convention).

- [ ] **Step 3: Confirm the working tree is clean and the branch is pushable**

Run: `git status`

Expected: clean. Hand off per superpowers:finishing-a-development-branch (PR should reference "Closes #127" — the issue is open, reopened 2026-06-07).
