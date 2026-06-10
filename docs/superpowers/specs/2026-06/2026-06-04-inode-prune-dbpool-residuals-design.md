# InodeAllocator pruning and DbPool residuals

**Date:** 2026-06-04
**Issues:** #126 — `InodeAllocator` grows without bound over the life of a mount;
#127 — DbPool residuals: `Drop` leaves other threads' connections; Shared
nested-`with` deadlock unguarded
**Status:** Approved

Two independent fixes covered by one spec, shipped as **two separate PRs**
(they touch disjoint modules: `tree.rs` vs `db_pool.rs`).

## Part 1 — #126: bound the InodeAllocator (PR 1)

### Problem

`InodeAllocator` (`musefs-core/src/tree.rs`) keys stable inodes by rendered
path and is insert-only by design: an unchanged path keeps its inode across
rebuilds, and a retired inode is never recycled. The cost is that the
path→inode map grows monotonically with every distinct path ever rendered.
Over a long-lived mount whose tags churn (each retag renders new paths), the
map grows without bound until remount.

### Design: threshold-amortized prune

Add a prune step to `InodeAllocator`, driven by the just-rebuilt tree:

- **Trigger:** after any rebuild, if `paths.len() > 2 × tree.nodes.len()`
  (an O(1) integer compare; both counts include the root). The incremental
  `apply_changes` path therefore stays cheap in the steady state.
- **Prune:** rebuild the `paths` map by iterating the tree's `nodes` and
  rendering each live path with the existing `path_of` helper
  (`tree.rs:301`), copying each live path's **existing** inode into a fresh
  map. `next` is never modified.
- **Cost:** the walk is O(live paths) but only runs after at least
  live-set-size retirements have accumulated — amortized O(1) per retirement.
  Memory is bounded at 2× the live set between prunes.

**Invariants:**

- *No aliasing (unchanged):* `next` is monotone and untouched by pruning, so a
  retired inode is never reissued for any path. A stale FUSE handle can never
  alias a different node.
- *Live-path stability (unchanged):* a path present in consecutive trees keeps
  its inode; pruning copies live entries verbatim.
- *Rebirth (deliberately weakened):* a path that vanishes and later reappears
  may get a fresh inode instead of its old one (it does whenever a prune ran in
  between). Accepted: exact-path rebirth requires a reverted tag edit or a
  delete-and-rescan to identical tags — rare — and the cost is a stale kernel
  dentry resolving ENOENT for at most one entry TTL, which is already the
  documented degradation for vanished paths. Open handles on the old inode were
  already invalidated by the path vanishing.

**Call sites.** Two, both in `facade.rs`, immediately after a rebuild while the
`inodes` lock is held: the full-rebuild path (`rebuild_full`, after
`build_with` at `facade.rs:304`) and the incremental path — on the unified
`tree` value after the `match` over the `apply_changes` result (i.e. covering
both the incremental success and the fallback-`build_with` arm, before
`self.tree.store`, ~`facade.rs:468`). The debug parity check (`facade.rs:447`)
clones the allocator before the prune point and is unaffected.

**Docs.** Rewrite the struct comment (`tree.rs:13-16`): replace "the map grows
monotonically with the universe of distinct paths ever rendered" with the new
bound (≤ 2× live set between prunes) and the rebirth caveat.

### Tests (unit tests in `tree.rs`, existing style)

1. Repeated rebuilds with disjoint path sets keep the map bounded by
   2× live count.
2. Live paths keep their inodes across a prune.
3. A pruned path that reappears gets a fresh inode, never a recycled one.
4. Existing `build_with_keeps_inodes_stable_across_rebuilds` and
   `build_with_does_not_recycle_a_vanished_inode` pass unchanged.

PR 1 `Closes #126`. The in-diff mutation gate covers the new logic.

## Part 2 — #127: DbPool nesting and Drop contract (PR 2)

### Problem

Two residuals after #94 in `musefs-core/src/db_pool.rs`:

- **(a)** `DbPool::drop` clears only the dropping thread's thread-local
  connection; connections the pool opened on other threads persist until those
  threads exit. (`Rc<Db>` in a `thread_local!` is unreachable cross-thread:
  `Rc` is `!Send`, `Connection` is `!Sync`.)
- **(b)** Re-entrant `with`/`with_poll` on the `Shared` variant deadlocks on
  its non-reentrant `std::sync::Mutex`; the hazard is warned about in a comment
  but a violation surfaces as a hang, not a diagnostic. Latent asymmetry:
  nested `with` *works* on `PerThread` (the `Rc` path — pinned by the
  `reentrant_with_does_not_panic` test) but deadlocks on `Shared`, so the
  in-memory test variant hangs on a pattern that is legal in production.
  `with_poll`-inside-`with_poll` likewise deadlocks on `PerThread`'s poll
  mutex.

### Design (b): make nesting legal with `parking_lot::ReentrantMutex`

Chosen over a detect-and-panic guard: it erases the bug class instead of
diagnosing it, removes the PerThread-vs-Shared asymmetry rather than
enshrining it, and is less code. parking_lot is already compiled into every
real build (fuser depends on it), so no new code reaches the binary.

- **Dependency:** add `parking_lot = "0.12"` to `musefs-core/Cargo.toml`
  (first direct edge; previously only transitive via fuser, one layer up).
- **Types:** `PerThread { poll: Mutex<Db> }` → `ReentrantMutex<Db>`;
  `Shared(Arc<Mutex<Db>>)` → `Shared(Arc<ReentrantMutex<Db>>)`.
  `ReentrantMutex::lock()` yields `&Db`, the exact shape the `with`/`with_poll`
  closures take. Soundness: `ReentrantMutex<T>: Sync` requires only `T: Send`
  (`rusqlite::Connection` is `Send`); multiple same-thread `&Db` borrows are
  the single-threaded access pattern `Connection`'s internal `RefCell` assumes.
- **Poison handling:** parking_lot locks don't poison; the two
  `unwrap_or_else(PoisonError::into_inner)` sites in `with`/`with_poll` are
  removed (other poison sites in the crate are out of scope — see #96).
- **Contract:** delete the deadlock warning comment on `with_poll`; document on
  the type that `with` and `with_poll` may nest freely, on any variant.

### Design (a): document-only, accepted-by-design

No code change. For musefs as shipped the leak is unreachable: the only
`DbPool` creator is `Musefs::open` (one per mount), and the mount's FUSE
worker threads — the only holders of "leaked" connections — exit when the
mount ends. The leak materializes only for a hypothetical embedder that drops
many pools while reusing long-lived shared worker threads; none exists in the
repo or roadmap, and building a sweep for it cuts against the project's YAGNI
conventions. The failure mode if such an embedder appears (fd accumulation) is
loud and easy to diagnose.

Promote the `Drop` impl's internal comment into the `DbPool` doc comment as an
explicit contract: per-thread connections live as long as their thread; a
caller that creates and drops many pools on long-lived shared threads strands
connections until those threads exit, and accepting that is a deliberate
design decision.

### Tests (in `db_pool.rs`, alongside existing)

1. Nested `with` inside `with` on `Shared` (in-memory) returns `Ok` — the case
   that deadlocked before.
2. `with_poll` inside `with` on `Shared` returns `Ok` — same mutex, mixed
   entry points.
3. `with_poll` inside `with_poll` on `PerThread` returns `Ok` — the third
   deadlock shape.
4. Existing tests pass; `with_open_failure_includes_path_in_error`
   (`db_pool.rs:184`) constructs a `PerThread` directly and needs its
   `Mutex::new` swapped for `ReentrantMutex::new` — a mechanical change only.

PR 2 `Closes #127`; the PR body notes the split — (b) fixed by making nesting
legal, (a) accepted-by-design and now documented on the type.

## Out of scope

- A cross-thread connection sweep for #127a (rejected: machinery for a caller
  that doesn't exist).
- Migrating the crate's remaining `std::sync::Mutex` + poison-recovery sites
  to parking_lot (#96 owns that question).
- Inode rebirth preservation (grace windows, retired-entry LRU) for #126.
