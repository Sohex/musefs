# DbPool: pool-owned connections (reopened #127)

**Date:** 2026-06-07
**Issue:** #127 (reopened) — `DbPool::drop` leaves other threads' connections;
the TLS-based design is unreachable from the type that owns the lifetime
**Status:** Approved

## Problem

PR #144 fixed the nested-`with` deadlock half of #127 and accepted the `Drop`
half by design: `DbPool::drop` clears only the dropping thread's entry in the
global `thread_local! PER_PATH` map, so connections opened by other threads
(one fd + SQLite page cache each) persist until those threads exit. The
documented rationale was that cross-thread cleanup is unreachable because
`Rc<Db>` is `!Send`.

That acceptance is reversed. The unreachability is self-imposed by the `Rc`
choice, and the hole is wider than the original issue stated: `DbPool` is
`Send + Sync` — it must be, because `Arc<Musefs>` is moved into
`threadpool::ThreadPool::execute` closures throughout the FUSE adapter
(`musefs-fuse/src/lib.rs`) — so a pool can be dropped on a thread that never
opened a connection, in which case the `Drop` impl removes nothing at all.
Connection lifetime is owned by thread-local storage; the type that is
supposed to govern that lifetime cannot reach it. The "lifetime contract"
doc block patches a type-system hole with prose.

For the current `musefs` binary the leak is bounded (a mount's worker pool
lives exactly as long as the mount), but `musefs-core` is a library: an
embedder that creates and drops pools on long-lived shared threads strands a
connection per pool per thread, unboundedly.

### Alternatives rejected

- **Make `DbPool` `!Send`** (reviewer suggestion): fails to compile the
  serving architecture (`Arc<Musefs>` requires `Musefs: Send + Sync`), and
  even where viable it only prevents the cross-thread-drop variant — drop on
  the origin thread still strands every other thread's connection.
- **Global `DashMap<(ThreadId, PathBuf), Weak>` registry** (reviewer
  suggestion): with `Weak` entries the strong refs still live in TLS and
  remain unreachable, so it doesn't close the hole; with strong entries it
  converges to the chosen design but with global state, composite keys
  (`PathBuf` alone breaks two-pools-same-path), a `Drop` that must scan the
  registry, and a new dependency to optimize a lock that has no contention.
- **No caching, open per `with` call**: a SQLite open per FUSE operation is
  what the pool exists to avoid.

## Design

Invert ownership: the pool owns its connections; threads have affinity to
one. The global TLS map is deleted.

```rust
pub enum DbPool {
    PerThread {
        path: PathBuf,
        poll: ReentrantMutex<Db<ReadOnly>>,
        conns: Mutex<HashMap<ThreadId, Arc<ReentrantMutex<Db<ReadOnly>>>>>,
    },
    Shared(Arc<ReentrantMutex<Db<ReadOnly>>>),
}
```

Deleted outright: the manual `Drop` impl, `NEXT_POOL_ID`, the `id` field, the
`thread_local! PER_PATH` map, the `PerPathMap` alias, and the
"lifetime contract (#127, accepted by design)" doc block. The
compiler-generated drop of `conns` closes every connection, from whatever
thread drops the pool — `Arc<ReentrantMutex<Db>>` is `Send`, so this is sound
by construction. Two-pools-same-path keying falls out for free: each pool has
its own map, so no composite key is needed.

The inner `ReentrantMutex` is never contended — only its owning thread ever
locks it — but it is load-bearing for the type system: `Arc<Db>` is not
`Send` (`Db` is not `Sync`), so the mutex wrapper is what keeps the map
field, and therefore `DbPool`, `Send + Sync`. It is reentrant (not a plain
`Mutex`) so nested `with` on the same thread keeps working. This earns a
comment in the code.

### `with` flow

1. Lock `conns`; `entry(thread::current().id())` — on vacancy,
   `Db::open_readonly(path)` and insert (open failure maps to
   `CoreError::DbOpen` carrying the path, unchanged).
2. Clone the `Arc`; **drop the map guard**.
3. Lock the connection's `ReentrantMutex`; run `f`.

The map lock is never held while `f` runs, so nesting cannot deadlock on it;
lock order is strictly map→conn, and conn locks are single-thread by
construction, so no cross-order deadlock exists. The one-time open happens
under the map lock — that briefly blocks other threads' first access, once
per thread per pool; not worth a double-checked dance.

`with_poll`, the `Shared` variant, and `data_version` semantics are
untouched. Warm per-thread page caches and contention-free WAL reads (#94)
are preserved. Per-call cost drops slightly: one `ThreadId` hash under an
uncontended `parking_lot::Mutex` replaces a `PathBuf` clone plus two
`(PathBuf, u64)` hashes in TLS.

### Inverted residual (documented, no machinery)

The leak bound flips: a thread that dies while the pool lives leaves its
connection in the map until pool drop, instead of a dropped pool leaking
until threads die. This is the strictly better bound — FUSE workers live
exactly as long as the mount, and an embedder's pool drop now reclaims
everything. One sentence in the module doc.

## Tests

- Existing tests adapt mechanically: the `PerThread` literal in
  `with_open_failure_includes_path_in_error` loses `id`, gains an empty
  `conns`.
- **New: drop closes other threads' connections.** Worker threads each run
  `with`, then park on a barrier — still alive — when the pool is dropped;
  assert the process's open-fd count for the DB path (readlink over
  `/proc/self/fd`, Linux-only like the rest of the suite) returns to
  baseline. (Note: `std::thread::scope` can't express this — scoped closures
  borrow the pool for the whole scope — so plain threads with a done-signal
  before the drop, joined at the end.)
- **New: cross-thread drop.** Create and use the pool on one thread, move it
  into another thread and drop it there; assert all DB fds are closed. This
  pins the `Send` hole as a regression test.
- Existing reentrancy tests (`reentrant_with_does_not_panic`,
  `nested_with_poll_on_per_thread_pool`, and the `Shared`-variant nesting
  tests) continue to cover nesting.

## Verification

Workspace tests, `cargo clippy --all-targets`, `cargo fmt --all --check`,
and the in-diff mutation gate (CI parity invocation per CLAUDE.md).
