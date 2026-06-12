# Bound FUSE-layer Resources Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Bound two unbounded FUSE-adapter tables against a hostile local client — the per-open directory-handle snapshots (#307) and the foreground-read pool queue (#308) — each with a hardcoded cap whose overflow maps to a clean errno.

**Architecture:** Both changes live entirely in `musefs-fuse/src/lib.rs`. Each bound is a pure decision helper (so the cap logic is unit-testable without a live FUSE mount) wired into the existing trait method. Part A admits a directory handle under the existing `dir_handles` mutex (`ENFILE` over cap); Part B reserves an in-flight-read slot synchronously on the dispatch thread before `pool.execute` (`EAGAIN` over cap, never blocking). Both mirror the existing `HandleTableFull → ENFILE` house pattern and the `PollPendingGuard` drop-guard.

**Tech Stack:** Rust workspace (`musefs-fuse` crate), `fuser`, `threadpool`, `std::sync::atomic`, `cargo test`.

**Spec:** `docs/superpowers/specs/2026-06-12-bound-fuse-resources-design.md`

---

## Background the implementer needs

- The single fuser dispatch thread calls every `Filesystem` trait method; blocking work is offloaded to `self.pool` (`threadpool::ThreadPool`, **unbounded** mpsc queue) and answered via the `Send` reply objects.
- `opendir` (`musefs-fuse/src/lib.rs:351-368`) runs its **entire body on the pool thread**: it builds the listing, `dir_fh.fetch_add(1, Relaxed)` to allocate an id, then inserts into `dir_handles` under the mutex and replies. The cap check must move *inside* the existing lock hold, and the `fetch_add` must move *after* the check so a rejected open burns no id.
- `read` (`lib.rs:419-452`) does a synchronous marker check on the dispatch thread, then offloads `core.read_into` to the pool. The slot reservation is added **on the dispatch thread, before `pool.execute`** — that is what caps the queue.
- `dir_fh` starts at 1; `fetch_add` returns the *previous* value as the handle id (first handle = 1, counter becomes 2). `0` is a stateless sentinel and must stay reserved.
- Drop-guard precedent: `PollPendingGuard<'a>(&'a AtomicBool)` (`lib.rs:140-146`). Part B's guard differs — it must *own* an `Arc<AtomicUsize>` so it can move into the `'static` worker closure.
- Imports already present: `AtomicBool, AtomicU64, Ordering` (`lib.rs:8`), `Arc, Mutex` (`lib.rs:9`), `FileType` (`lib.rs:16`). Part B adds `AtomicUsize`.
- Test module `mod tests` (`lib.rs:562`) opens with `use super::*;`, so every crate-level item (the new consts, helpers, guard, alias, and the atomics) is in scope inside the tests.
- **Clippy runs `--all-targets -D warnings` in the pre-commit hook.** A helper that exists in the lib but is only called from `#[cfg(test)]` code triggers `dead_code` in the no-`cfg(test)` lib target. Therefore each task wires its helper into the lib (`opendir`/`read`) in the **same commit** as it introduces it — never commit an unused helper.
- The pre-commit hook runs the full workspace test suite, clippy, and fmt; a red commit is rejected. Each task ends green.

## File Structure

- **Modify only:** `musefs-fuse/src/lib.rs`
  - New crate-level items: `type DirListing`, `const MAX_DIR_HANDLES`, `fn try_admit_dir_handle`, `const MAX_INFLIGHT_READS`, `struct ReadSlotGuard` + `Drop`, `fn reserve_read_slot`.
  - New `MusefsFs` field: `inflight_reads: Arc<AtomicUsize>`.
  - Edited methods: `opendir`, `read`, `MusefsFs::new`; edited doc comment on `FuseConfig::max_background`.
  - New unit tests appended to the existing `mod tests`.

No other crates, no config/CLI surface, no schema changes, no `fuzz/` impact.

---

## Task 1: Part A — bound directory-handle snapshots (#307)

**Files:**
- Modify: `musefs-fuse/src/lib.rs` (add alias/const/helper near `build_dir_listing` ~line 129–136; rewrite `opendir` at 351–368; edit `MusefsFs::new` pool comment at 185–189)
- Test: `musefs-fuse/src/lib.rs` (`mod tests`, appended before its closing `}` at line 694)

- [ ] **Step 1: Write the failing unit tests**

Append these three tests inside `mod tests` (just before its closing brace at `lib.rs:694`):

```rust
    fn empty_dir_handles() -> std::collections::HashMap<u64, Arc<DirListing>> {
        std::collections::HashMap::new()
    }

    #[test]
    fn try_admit_dir_handle_admits_and_allocates_id_below_cap() {
        let mut handles = empty_dir_handles();
        let counter = AtomicU64::new(1); // matches the live `dir_fh` start
        let fh = try_admit_dir_handle(&mut handles, &counter, 2, Vec::new());
        assert_eq!(fh, Some(1), "first admit uses the pre-increment counter value");
        assert_eq!(handles.len(), 1);
        assert!(handles.contains_key(&1));
        assert_eq!(counter.load(Ordering::Relaxed), 2, "id allocated on admit");
    }

    #[test]
    fn try_admit_dir_handle_rejects_at_cap_without_inserting_or_advancing_id() {
        let mut handles = empty_dir_handles();
        handles.insert(10, Arc::new(Vec::new()));
        handles.insert(11, Arc::new(Vec::new()));
        let counter = AtomicU64::new(12);
        let fh = try_admit_dir_handle(&mut handles, &counter, 2, Vec::new());
        assert_eq!(fh, None, "at cap must reject");
        assert_eq!(handles.len(), 2, "must not insert on reject");
        assert_eq!(
            counter.load(Ordering::Relaxed),
            12,
            "must not burn a dir_fh id on reject"
        );
    }

    #[test]
    fn try_admit_dir_handle_frees_slot_after_removal() {
        let mut handles = empty_dir_handles();
        handles.insert(10, Arc::new(Vec::new()));
        handles.insert(11, Arc::new(Vec::new()));
        let counter = AtomicU64::new(12);
        handles.remove(&10); // releasedir frees a slot
        let fh = try_admit_dir_handle(&mut handles, &counter, 2, Vec::new());
        assert_eq!(fh, Some(12), "a freed slot admits again");
        assert_eq!(handles.len(), 2);
        assert!(!handles.contains_key(&10), "the freed handle stays gone");
        assert!(handles.contains_key(&12), "the new handle fills the freed slot");
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p musefs-fuse try_admit_dir_handle`
Expected: FAIL — compile error `E0412 cannot find type DirListing` / `E0425 cannot find function try_admit_dir_handle`.

- [ ] **Step 3: Add the alias, the cap constant, and the helper**

Insert immediately **before** the `build_dir_listing` doc comment (`lib.rs:129`):

```rust
/// One directory's readdir snapshot: `(child inode, entry type, name)` rows.
/// Aliased so the handle-map signatures stay readable (and dodge
/// `clippy::type_complexity`).
type DirListing = Vec<(u64, FileType, String)>;

/// Cap on concurrently-open directory handles (#307). Each `opendir` snapshots a
/// full `DirListing`, so an unreleased handle pins memory ~ (entries × name
/// length); the cap bounds the *number* of snapshots, not their inherent size (a
/// single `ls` of the widest directory already allocates one). 1024 sits well
/// above a heavy parallel indexer's concurrent-dir-handle count (~hundreds), so
/// legitimate clients never hit it, while an over-cap `opendir` returns `ENFILE`
/// — the directory-side analogue of the file-handle `HandleTableFull → ENFILE`.
const MAX_DIR_HANDLES: usize = 1024;
```

Insert immediately **after** the `build_dir_listing` function (after its closing brace at `lib.rs:136`):

```rust
/// Admit a directory handle under the caller's `dir_handles` lock, enforcing
/// `MAX_DIR_HANDLES` (#307). Returns the freshly allocated handle id on admit, or
/// `None` when the table is at `cap` (the caller replies `ENFILE`). The id is
/// drawn from `counter` only on the admit path, and the whole check-then-insert
/// runs under the single lock the caller holds, so concurrent `opendir` closures
/// cannot race the count past the cap and a rejected open burns no id.
fn try_admit_dir_handle(
    handles: &mut std::collections::HashMap<u64, Arc<DirListing>>,
    counter: &AtomicU64,
    cap: usize,
    listing: DirListing,
) -> Option<u64> {
    if handles.len() >= cap {
        return None;
    }
    let fh = counter.fetch_add(1, Ordering::Relaxed);
    handles.insert(fh, Arc::new(listing));
    Some(fh)
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p musefs-fuse try_admit_dir_handle`
Expected: PASS (3 tests).

- [ ] **Step 5: Wire `opendir` to use the helper**

Replace the body of `opendir` (`lib.rs:351-368`) with:

```rust
    fn opendir(&self, _req: &Request, ino: INodeNo, _flags: OpenFlags, reply: ReplyOpen) {
        self.fire_poll_refresh();
        let core = Arc::clone(&self.core);
        let handles = Arc::clone(&self.dir_handles);
        let counter = Arc::clone(&self.dir_fh);
        self.pool.execute(move || {
            let listing = match build_dir_listing(&core, ino.0) {
                Ok(l) => l,
                Err(e) => return reply.error(reply_errno("opendir", ino.0, &e)),
            };
            // Check + id allocation + insert under one lock hold, so concurrent
            // opendir closures can't race the count past MAX_DIR_HANDLES (#307).
            let admitted = {
                let mut guard = handles
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                try_admit_dir_handle(&mut guard, &counter, MAX_DIR_HANDLES, listing)
            };
            match admitted {
                Some(fh) => reply.opened(FileHandle(fh), FopenFlags::empty()),
                None => reply.error(fuser::Errno::ENFILE),
            }
        });
    }
```

- [ ] **Step 6: Update the stale `MusefsFs::new` pool comment**

Replace the comment above `pool: ThreadPool::new(workers),` (`lib.rs:185-188`):

```rust
            // `ThreadPool`'s queue is unbounded, so foreground reads are gated by
            // `inflight_reads`/`MAX_INFLIGHT_READS` before submission (#308) and
            // directory handles are capped at `MAX_DIR_HANDLES` (#307); both reject
            // over-cap work rather than letting it grow process memory.
            // `max_background` (set in `init`) separately caps the kernel's
            // background/readahead requests.
            pool: ThreadPool::new(workers),
```

(The `inflight_reads` field referenced here lands in Task 2; this comment is written now to avoid a second edit of the same lines. It compiles regardless — comments reference nothing.)

- [ ] **Step 7: Format, lint, and test the crate**

Run: `cargo fmt --all && cargo clippy -p musefs-fuse --all-targets -- -D warnings && cargo test -p musefs-fuse`
Expected: fmt applies cleanly, clippy clean (the helper is now used in `opendir`), all tests PASS. (The pre-commit hook re-checks fmt/clippy/full suite; running fmt now keeps the commit from being rejected on formatting.)

- [ ] **Step 8: Commit**

```bash
git add musefs-fuse/src/lib.rs
git commit -m "fix(fuse): cap concurrent directory handles, ENFILE over the limit (#307)

opendir snapshots a full directory listing per open with no quota; an
adversarial client holding many directory fds open grows process memory
unbounded. Cap the dir_handles table at MAX_DIR_HANDLES (1024) under the
existing lock hold (id allocated only on admit), returning ENFILE over cap —
the directory analogue of the file-handle HandleTableFull -> ENFILE path.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 2: Part B — bound foreground read work (#308)

> **Anchor on the quoted text, not the line numbers.** Task 1 inserts ~30 lines, so every line number below is pre-Task-1 and will have shifted. Each step gives an unambiguous textual anchor — use it.

**Files:**
- Modify: `musefs-fuse/src/lib.rs` (add `AtomicUsize` import at line 8; add const + guard + helper after the `PollPendingGuard` Drop impl ~line 146; add field to `MusefsFs` ~line 175; init in `new` ~line 198; rewrite `read` at 419–452; edit `FuseConfig::max_background` doc)
- Test: `musefs-fuse/src/lib.rs` (`mod tests`)

- [ ] **Step 1: Write the failing unit tests**

Append inside `mod tests` (before its closing brace):

```rust
    #[test]
    fn reserve_read_slot_admits_up_to_cap() {
        let inflight = Arc::new(AtomicUsize::new(0));
        let g1 = reserve_read_slot(&inflight, 2);
        let g2 = reserve_read_slot(&inflight, 2);
        assert!(g1.is_some() && g2.is_some(), "two reservations fit cap 2");
        assert_eq!(inflight.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn reserve_read_slot_rejects_over_cap_and_releases() {
        let inflight = Arc::new(AtomicUsize::new(0));
        let _g1 = reserve_read_slot(&inflight, 2);
        let _g2 = reserve_read_slot(&inflight, 2);
        let g3 = reserve_read_slot(&inflight, 2);
        assert!(g3.is_none(), "third reservation exceeds cap 2");
        assert_eq!(
            inflight.load(Ordering::Relaxed),
            2,
            "a rejected reservation must release its own increment"
        );
    }

    #[test]
    fn read_slot_guard_releases_on_drop_and_panic() {
        let inflight = Arc::new(AtomicUsize::new(0));
        {
            let _g = reserve_read_slot(&inflight, 4).expect("under cap");
            assert_eq!(inflight.load(Ordering::Relaxed), 1);
        }
        assert_eq!(inflight.load(Ordering::Relaxed), 0, "guard releases on drop");

        let inflight2 = Arc::new(AtomicUsize::new(0));
        let i2 = Arc::clone(&inflight2);
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _g = reserve_read_slot(&i2, 4).expect("under cap");
            panic!("boom");
        }));
        assert!(r.is_err());
        assert_eq!(
            inflight2.load(Ordering::Relaxed),
            0,
            "guard releases its slot on unwind"
        );
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p musefs-fuse reserve_read_slot read_slot_guard`
Expected: FAIL — compile error `E0425 cannot find function reserve_read_slot` / `E0433 ... AtomicUsize`.

- [ ] **Step 3: Add the `AtomicUsize` import**

Change `lib.rs:8` from:

```rust
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
```

to:

```rust
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
```

- [ ] **Step 4: Add the cap constant, the guard, and the reserve helper**

Insert immediately **after** the `PollPendingGuard` `Drop` impl (after its closing brace at `lib.rs:146`):

```rust
/// Cap on concurrently outstanding foreground reads (#308). Every FUSE `read`
/// reserves a slot on the dispatch thread *before* enqueuing onto the unbounded
/// pool queue; over the cap the read is rejected with `EAGAIN` rather than
/// queued, so the queue cannot grow past the cap. 1024 is far above any
/// legitimate read fan-in (a player reads sequentially; readahead is bounded by
/// `max_background`), so it is an attack-only response, and queued job state is
/// small, keeping the bound cheap.
const MAX_INFLIGHT_READS: usize = 1024;

/// Releases one `inflight_reads` slot when dropped — on worker completion, on the
/// over-cap reject path, and on panic. Owns an `Arc<AtomicUsize>` (unlike the
/// borrow-based `PollPendingGuard`) so it can move into the `'static` worker
/// closure.
struct ReadSlotGuard(Arc<AtomicUsize>);

impl Drop for ReadSlotGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Reserve one in-flight-read slot (#308). Increments `inflight` and returns a
/// guard if the post-increment count is within `cap`; otherwise the guard drops
/// immediately (undoing the increment) and `None` is returned, so the caller
/// replies `EAGAIN` without enqueuing. The counter is a pure count with no
/// happens-before tie to other data, so `Relaxed` ordering suffices.
fn reserve_read_slot(inflight: &Arc<AtomicUsize>, cap: usize) -> Option<ReadSlotGuard> {
    let count = inflight.fetch_add(1, Ordering::Relaxed) + 1;
    let guard = ReadSlotGuard(Arc::clone(inflight));
    if count > cap {
        None // guard drops here, undoing the increment
    } else {
        Some(guard)
    }
}
```

- [ ] **Step 5: Add the `inflight_reads` field to `MusefsFs`**

Insert after the `dir_fh` field (`lib.rs:173-175`, inside the struct):

```rust
    /// In-flight foreground-read counter. `read` reserves a slot before enqueuing;
    /// over `MAX_INFLIGHT_READS` the read is rejected with `EAGAIN`, capping the
    /// otherwise-unbounded pool queue (#308).
    inflight_reads: Arc<AtomicUsize>,
```

- [ ] **Step 6: Initialise the field in `MusefsFs::new`**

Insert after `dir_fh: Arc::new(AtomicU64::new(1)),` (`lib.rs:198`):

```rust
            inflight_reads: Arc::new(AtomicUsize::new(0)),
```

- [ ] **Step 7: Run the helper tests to verify they pass**

Run: `cargo test -p musefs-fuse reserve_read_slot read_slot_guard`
Expected: PASS (3 tests).

- [ ] **Step 8: Wire `read` to reserve a slot before offloading**

Replace the body of `read` (`lib.rs:419-452`) with:

```rust
    fn read(
        &self,
        _req: &Request,
        ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        size: u32,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyData,
    ) {
        if platform::spotlight::is_marker(ino.0) {
            return reply.data(&[]);
        }
        // Reserve a slot on the dispatch thread before enqueuing; over the cap,
        // reject with EAGAIN so the unbounded pool queue can't grow (#308).
        let Some(slot) = reserve_read_slot(&self.inflight_reads, MAX_INFLIGHT_READS) else {
            return reply.error(fuser::Errno::EAGAIN);
        };
        let core = Arc::clone(&self.core);
        self.pool.execute(move || {
            // `_slot` (named) holds the guard until the read completes or the
            // worker panics, then releases it. Do NOT simplify to bare `_`: that
            // drops the guard immediately, releasing the slot before the work
            // runs and neutering the cap.
            let _slot = slot;
            READ_BUF.with(|b| {
                let mut buf = b.borrow_mut();
                match core.read_into(
                    ino.0,
                    NonZeroU64::new(fh.0).map(Fh::from),
                    offset,
                    u64::from(size),
                    &mut buf,
                ) {
                    Ok(()) => reply.data(&buf),
                    Err(e) => reply.error(reply_errno("read", ino.0, &e)),
                }
                if buf.capacity() > MAX_RETAINED_READ_BUF {
                    buf.shrink_to(MAX_RETAINED_READ_BUF);
                }
            });
        });
    }
```

- [ ] **Step 9: Update the `FuseConfig::max_background` doc comment**

Find this exact doc comment on the `max_background` field in `FuseConfig`:

```rust
    /// Max outstanding background (readahead/async) requests the kernel queues.
    /// Caps that class of work delivered to the pool; foreground reads are
    /// bounded only by client concurrency, not by this.
    pub max_background: u16,
```

Replace it with (only the final clause changes):

```rust
    /// Max outstanding background (readahead/async) requests the kernel queues.
    /// Caps that class of work delivered to the pool; foreground reads are
    /// bounded separately by `MAX_INFLIGHT_READS` (#308), not by this.
    pub max_background: u16,
```

- [ ] **Step 10: Format, lint, and test the crate**

Run: `cargo fmt --all && cargo clippy -p musefs-fuse --all-targets -- -D warnings && cargo test -p musefs-fuse`
Expected: fmt applies cleanly, clippy clean (`reserve_read_slot`, `ReadSlotGuard`, and `inflight_reads` are all used in `read`), all tests PASS.

- [ ] **Step 11: Guard against the metrics-feature CI gap**

The read path feeds `musefs-core`'s metrics counters (CI's `check` job asserts exact read/getattr counts). This change doesn't alter how often `core.read_into` runs per admitted FUSE read, but verify:

Run: `cargo test -p musefs-core --features metrics`
Expected: PASS (no count regressions).

- [ ] **Step 12: Commit**

```bash
git add musefs-fuse/src/lib.rs
git commit -m "fix(fuse): cap outstanding foreground reads, EAGAIN over the limit (#308)

The threadpool queue is unbounded and foreground reads were bounded only by
client concurrency, so a wide read storm grew the process queue (each queued
job owns its reply and keeps request state alive). Reserve an in-flight-read
slot on the dispatch thread before pool.execute; over MAX_INFLIGHT_READS (1024)
reject with EAGAIN instead of enqueuing. A drop-guard releases the slot on
completion, reject, and panic. Nothing blocking is added to the read path.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Final verification

- [ ] **Run the full workspace gate** (what the pre-commit hook enforces):

Run: `cargo fmt --all --check && cargo clippy --all-targets -- -D warnings && cargo test`
Expected: all green.

## Optional follow-up (not required for completion)

An `--ignored` end-to-end test for Part A — open `MAX_DIR_HANDLES + 1` directories on a live mount and assert the final `opendir` returns `ENFILE` — would exercise the wiring the unit tests can't reach (the trait methods need a real kernel reply channel). It requires `/dev/fuse` + libfuse and must follow the existing `musefs-fuse` e2e harness conventions (`cargo test -p musefs-fuse -- --ignored`). The decision logic for both bounds is already covered by the helper unit tests; this is integration-coverage polish, deliberately left out of the required path.
