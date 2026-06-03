# Phase 4 — Concurrency correctness

**Date:** 2026-06-03
**Scope:** Roadmap "Phase 4 — Concurrency correctness" — issues #90, #89. Two
independent fixes in different crates (`musefs-core`, `musefs-fuse`), both about
keeping the dispatch/serving path from being starved under load. Ships as **one
batched PR** (matching how Phases 0–3 each landed).

## Goal

Stop two distinct ways the serving path degrades under concurrent / high-rate
access:

- **#90** — a full tree rebuild holds the `inodes` mutex across DB I/O, blocking
  concurrent first-touch inode allocation for the duration of a (possibly slow,
  e.g. NFS) DB scan.
- **#89** — every FUSE metadata op submits a poll closure to the bounded worker
  pool, so a metadata-op storm (a `find . -type f`, a media-manager library scan)
  floods the pool with no-op tasks that compete with real read/`getattr` I/O.

Neither is a user-visible behavior change for correctly-served files; both remove
contention/starvation under load. The byte-identical-audio invariant is
untouched. The full `#[ignore]` e2e mount suite stays green.

---

## #90 — `rebuild_full` holds the `inodes` lock across DB I/O

**Problem.** In `musefs-core/src/facade.rs`, `rebuild_full` acquires the `inodes`
mutex *inside* the `pool.with(|db| …)` closure (`facade.rs:255-258`) and holds it
across `build_full`'s DB reads (`list_tracks()` + `tags_grouped()`). On a slow
backing store the lock is held for the whole DB scan, blocking concurrent
first-touch inode allocation.

`rebuild_incremental` already does the opposite (`facade.rs:308-340`): it scans
render keys + fetches tags from the DB first, releases the pool connection, and
only *then* locks `inodes` around the pure-CPU `apply_changes` / `build_with`.
The lock-order comment at `facade.rs:387-398` even calls out `rebuild_full` /
`refresh` as "the one intentional exception where a pool connection is held
around an in-memory lock." This fix removes that exception.

**Fix.** Restructure `rebuild_full` to mirror `rebuild_incremental`: do the
DB read + path render first, outside any in-memory lock; then lock `inodes` only
around `VirtualTree::build_with`.

Extract the DB-read + render phase of `build_full` into a small allocator-free
helper that returns `(entries: Vec<(i64, String)>, snapshot: HashMap<i64,
TrackRenderState>)`:

```rust
// allocator-free: pure DB read + path render
fn render_entries(
    db: &Db,
    config: &MountConfig,
) -> Result<(Vec<(i64, String)>, HashMap<i64, TrackRenderState>)> {
    let tracks = db.list_tracks()?;
    let mut tags_by_track = db.tags_grouped()?;
    let mut entries = Vec::with_capacity(tracks.len());
    let mut snapshot = HashMap::with_capacity(tracks.len());
    for t in &tracks {
        let tags = tags_by_track.remove(&t.id).unwrap_or_default();
        let path = Self::render_one(config, t.format, &tags);
        snapshot.insert(t.id, TrackRenderState {
            content_version: t.content_version,
            format: t.format,
            path: path.clone(),
        });
        entries.push((t.id, path));
    }
    Ok((entries, snapshot))
}
```

`rebuild_full` becomes:

```rust
fn rebuild_full(&self) -> Result<HashMap<i64, TrackRenderState>> {
    if self.force_rebuild_error.load(Ordering::Acquire) {
        return Err(CoreError::BackingChanged("forced refresh failure".to_string()));
    }
    let (entries, snapshot) = self.pool.with(|db| Self::render_entries(db, &self.config))?;
    let mut alloc = crate::lock::lock_or_flag(&self.inodes, &self.needs_rebuild, "inodes");
    let tree = VirtualTree::build_with(&entries, &mut alloc);
    drop(alloc);
    self.tree.store(Arc::new(tree));
    Ok(snapshot)
}
```

`build_full` (the other caller, `open()` at `facade.rs:163`) stays as a thin
wrapper over `render_entries` + `build_with`, so its existing signature
(`db, config, alloc`) is preserved. `open()` runs single-threaded at init with a
fresh local `InodeAllocator` — no contention — so it is unaffected either way.
`force_full_rebuild` calls `rebuild_full`, so it inherits the fix for free.

**Lock-order comment.** Update `facade.rs:387-398` to drop the "one intentional
exception" sentence: the order is now uniform — the pool connection
(`pool.with` / `with_poll`) is always released before any in-memory lock
(`inodes`, header-cache shards) is acquired, in *both* rebuild paths.

---

## #89 — `fire_poll_refresh` submits a threadpool task on every metadata op

**Problem.** In `musefs-fuse/src/lib.rs`, `fire_poll_refresh` (`lib.rs:151-176`)
is called from `lookup`, `getattr`, and `readdir`, and *unconditionally*
`pool.execute()`s a closure (`Arc::clone(core)` + boxed closure + channel-send +
worker wakeup). The poll-interval debounce is only checked *inside* the submitted
closure (`poll_refresh_notify`, `facade.rs:429-431`), not before submission. A
metadata-op storm floods the bounded worker pool with tasks that merely acquire
`last_poll`, compare a timestamp, and return — competing with real read/`getattr`
I/O for worker threads.

**Chosen approach (synchronous debounce + single-flight gate).** Two pieces:

1. a cheap synchronous predicate checked on the dispatch thread *before*
   submitting (the "synchronous debounce" the roadmap names), and
2. a single-flight gate so at most **one** poll task is ever queued/running.

Both pieces are kept because each covers a case the other misses:
- The synchronous predicate skips submission entirely in the common debounced
  case (no pool task at all), but on its own a burst right after a real DB change
  — before the poll task stamps `last_poll` — could still submit several
  concurrent tasks that each run a `data_version` pragma.
- The single-flight gate bounds outstanding tasks to one regardless, and is the
  *only* protection when `--poll-interval-ms 0` disables the time-debounce (then
  the predicate is a no-op that returns `true` every op).

### Piece 1 — `Musefs::poll_due()` (musefs-core/src/facade.rs)

Add a pure, cheap predicate that performs exactly the early-return gate checks
`poll_refresh_notify` already does — and nothing else (no DB access, no
`data_version`, no rebuild):

```rust
/// Cheap, synchronous "is a poll worth submitting?" check for the dispatch
/// thread to gate `fire_poll_refresh` on. Mirrors the early-return gates in
/// `poll_refresh_notify`; keep the two in sync. Advisory only — a stale `true`
/// just costs one task that the inner gate short-circuits; `needs_rebuild` is
/// checked first so a self-heal is never skipped.
pub fn poll_due(&self) -> bool {
    if self.needs_rebuild.load(Ordering::Acquire) {
        return true;
    }
    if !self.poll_interval.is_zero()
        && crate::lock::lock_recover(&self.last_poll, "last_poll").elapsed()
            < self.poll_interval
    {
        return false;
    }
    if let Some(last_failed) =
        *crate::lock::lock_recover(&self.last_failed_refresh, "last_failed_refresh")
    {
        if last_failed.elapsed() < self.refresh_retry_backoff {
            return false;
        }
    }
    true
}
```

`poll_refresh_notify` keeps its own internal gates **unchanged** — it is public
API and must stay correct if called directly (the no-callback `poll_refresh`,
tests, a future caller). `poll_due` is purely an advisory pre-filter. A
cross-reference comment is added on both so they are kept in sync.

The timestamp stays `Mutex<Instant>` (not converted to a lock-free `AtomicU64`):
the mutex is uncontended (its only writer is the once-per-interval poll worker,
which holds it for a trivial assignment, never across I/O), so the per-op cost is
~tens of ns — deep in the noise of a microsecond-scale FUSE round-trip, and
strictly cheaper than the `Arc::clone` + boxed closure + channel-send it replaces
in the debounced case. An `AtomicU64`-nanos variant was considered and rejected
as a micro-optimization that, done properly, would also drag in
`last_failed_refresh` (an `Option<Instant>` needing a sentinel encoding) for no
measurable gain.

### Piece 2 — single-flight submission gate (musefs-fuse/src/lib.rs)

Add a shared `poll_pending: Arc<AtomicBool>` to `MusefsFs` (an `Arc` because it
must be cloned into the `'static` worker closure to be cleared on completion).
Rewrite `fire_poll_refresh`:

```rust
fn fire_poll_refresh(&self) {
    if !self.core.poll_due() {
        return; // synchronous debounce on the dispatch thread — submit nothing
    }
    if self
        .poll_pending
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return; // a poll task is already queued/running
    }
    let core = Arc::clone(&self.core);
    let pending = Arc::clone(&self.poll_pending);
    // existing keep_cache vs plain branch, with the body wrapped so the gate is
    // cleared on every exit path (incl. panic) via a small drop guard:
    self.pool.execute(move || {
        let _guard = PollPendingGuard(&pending); // clears poll_pending on drop
        // ... existing poll_refresh_notify (keep_cache) / poll_refresh body ...
    });
}
```

`PollPendingGuard` is a tiny RAII type whose `Drop` does
`pending.store(false, Ordering::Release)`, so a panic inside `poll_refresh_notify`
cannot wedge the gate `true` forever.

**Resulting behavior.** Per metadata op the dispatch thread does only the cheap
`poll_due()` check and, in the common debounced case, submits nothing. When a poll
is due, exactly one task is in flight at a time. A whole-library metadata scan
(read-only against musefs's store, so `data_version` never changes) thus costs at
most ~one trivial `data_version`-pragma task per `poll_interval`, leaving the
worker pool free for the scan's actual `getattr`/`open`/`read` work — and the
bound holds even with `--poll-interval-ms 0`.

---

## Testing

- **#90:** assert the `inodes` lock is *not* held across DB I/O. Preferred seam:
  use a slow/blocking DB injection (or the existing `force_*` test hooks) so a
  rebuild's DB scan is in flight, and assert a concurrent first-touch inode
  allocation is not blocked during it. If no clean blocking seam exists, fall
  back to a structural test asserting `render_entries` performs all DB access and
  `build_with` runs under the lock with no `Db` access in scope. Existing
  rebuild/refresh tests (incremental + full + force-rebuild + self-heal) stay
  green.
- **#89, `poll_due()`:** unit-test it returns `false` inside the interval; `true`
  past it; `true` when `needs_rebuild` is set (regardless of interval); `false`
  inside the failure backoff window and `true` past it; and `true` when
  `poll_interval` is zero.
- **#89, single-flight gate:** a test that a burst of `fire_poll_refresh` calls
  submits at most one task while one is pending — asserted via a task counter on
  the pool, mirroring the existing direct-call metric/test style — and that the
  gate is released after the task completes (and after a panicking task, via the
  guard).
- **Suite:** `cargo test` workspace green; the full `#[ignore]` e2e mount suite
  green on `/dev/fuse`; `cargo clippy --all-targets` clean; `cargo fmt --all
  --check` clean.

---

## Out of scope

- The lock-free `AtomicU64`-nanos timestamp variant (considered, rejected above).
- #69 (`poll_refresh` rebuild O(library) → O(changed)) — a separate Phase 6
  performance SP; it shares the facade rebuild path but is bench-tracked and
  sequenced after this.
- Any change to `poll_refresh_notify`'s internal rebuild logic or single-flight
  (`refreshing`) semantics.
