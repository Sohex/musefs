# Phase 4 — Concurrency Correctness Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Stop two contention/starvation problems on the musefs serving path: a full tree rebuild holding the `inodes` lock across DB I/O (#90), and every FUSE metadata op flooding the worker pool with no-op poll tasks (#89).

**Architecture:** #90 — split the DB-read+render phase of a full rebuild out of the `inodes`-locked region (mirroring the already-correct incremental path), so `inodes` is held only across the pure-CPU tree build. #89 — gate `fire_poll_refresh` on a cheap synchronous `Musefs::poll_due()` predicate checked on the dispatch thread, plus a `poll_pending: Arc<AtomicBool>` single-flight gate (cleared by an RAII guard) so at most one poll task is ever queued/running.

**Tech Stack:** Rust workspace (`musefs-core`, `musefs-fuse`); SQLite via `musefs-db`; `fuser` + `threadpool`; `tempfile`/`id3` in tests. Conventions in `CLAUDE.md` (thiserror per-crate errors, Serena symbolic tools for reading code).

**Spec:** `docs/superpowers/specs/2026-06-03-phase4-concurrency-correctness-design.md`

---

## File Structure

- **Modify** `musefs-core/src/facade.rs`
  - #90: add private `Musefs::render_entries`; rewrite `build_full` and `rebuild_full`; update the lock-order comment (~lines 387-398).
  - #89: add public `Musefs::poll_due`; add a one-line cross-reference comment on `poll_refresh_notify`'s gate block; add two `*_for_test` hooks; add unit tests in the existing `mod tests`.
- **Modify** `musefs-fuse/src/lib.rs`
  - #89: add `use std::sync::atomic::{AtomicBool, Ordering};`; add `poll_pending: Arc<AtomicBool>` field + init in `MusefsFs::new`; define `PollPendingGuard`; rewrite `fire_poll_refresh`; add unit tests in the existing `mod tests`.

No new files. Both crates already have a `#[cfg(test)] mod tests` (`facade.rs:772`, `lib.rs:385`).

---

## Task 1: #90 — move DB I/O out of the `inodes`-locked region

**Files:**
- Modify: `musefs-core/src/facade.rs` (`build_full` ~205-231, `rebuild_full` ~247-261, lock-order comment ~387-398)
- Test: `musefs-core/src/facade.rs` (`mod tests` ~772)

The fix is a behavior-preserving refactor: extract the DB-read+render phase into an allocator-free `render_entries`, have `build_full` wrap it, and have `rebuild_full` lock `inodes` only around `VirtualTree::build_with`. Existing rebuild/refresh tests prove behavior is unchanged; the new unit test pins the extracted DB→entries boundary.

- [ ] **Step 1: Write the failing test for the extracted `render_entries`**

Add to `mod tests` in `musefs-core/src/facade.rs`:

```rust
    #[test]
    fn render_entries_returns_paths_and_snapshot() {
        use crate::scan::scan_directory;
        use id3::TagLike;

        let dir = tempfile::tempdir().unwrap();
        {
            let mut tag = id3::Tag::new();
            tag.set_artist("Pix");
            tag.set_title("Song");
            let mut bytes = Vec::new();
            tag.write_to(&mut bytes, id3::Version::Id3v24).unwrap();
            bytes.extend_from_slice(&[0xFF, 0xFB, 1, 2, 3, 4]);
            std::fs::write(dir.path().join("a.mp3"), &bytes).unwrap();
        }
        let db = musefs_db::Db::open(dir.path().join("m.db")).unwrap();
        scan_directory(&db, dir.path()).unwrap();

        let cfg = MountConfig {
            template: "$artist/$title".to_string(),
            fallbacks: BTreeMap::new(),
            default_fallback: "Unknown".to_string(),
            mode: Mode::Synthesis,
            poll_interval: std::time::Duration::ZERO,
        };

        let (entries, snapshot) = Musefs::render_entries(&db, &cfg).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].1, "Pix/Song");
        let id = entries[0].0;
        assert_eq!(snapshot[&id].path, "Pix/Song");
        assert!(snapshot[&id].content_version >= 1);
    }
```

- [ ] **Step 2: Run the test to verify it fails (does not compile)**

Run: `cargo test -p musefs-core render_entries_returns_paths_and_snapshot`
Expected: FAIL — compile error `no function or associated item named 'render_entries' found for struct 'Musefs'`.

- [ ] **Step 3: Add `render_entries` and rewrite `build_full`**

In `impl Musefs`, replace the existing `build_full` (currently ~205-231) with these two functions:

```rust
    /// DB read + path render with no allocator: the lock-free phase shared by
    /// `build_full` and `rebuild_full`. Confining all `Db` access here is what
    /// lets `rebuild_full` hold `inodes` only across the pure-CPU `build_with`.
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
            snapshot.insert(
                t.id,
                TrackRenderState {
                    content_version: t.content_version,
                    format: t.format,
                    path: path.clone(),
                },
            );
            entries.push((t.id, path));
        }
        Ok((entries, snapshot))
    }

    /// Full rebuild: render every track and build the tree from scratch. Used by
    /// `open`, forced `refresh`, and the Stage B fallback. Returns the tree and the
    /// fresh `track_id -> TrackRenderState` snapshot.
    fn build_full(
        db: &Db,
        config: &MountConfig,
        alloc: &mut InodeAllocator,
    ) -> Result<(VirtualTree, HashMap<i64, TrackRenderState>)> {
        let (entries, snapshot) = Self::render_entries(db, config)?;
        Ok((VirtualTree::build_with(&entries, alloc), snapshot))
    }
```

- [ ] **Step 4: Rewrite `rebuild_full` to lock `inodes` only around `build_with`**

Replace the body of `rebuild_full` (currently ~247-261) with:

```rust
    /// Rebuild + publish the tree via a full render; returns the fresh snapshot
    /// (the caller decides whether/how to diff it). Mirrors `rebuild_incremental`'s
    /// ordering: read + render under the pool connection, then lock `inodes` only
    /// across the pure-CPU `build_with` (#90).
    fn rebuild_full(&self) -> Result<HashMap<i64, TrackRenderState>> {
        if self.force_rebuild_error.load(Ordering::Acquire) {
            return Err(CoreError::BackingChanged(
                "forced refresh failure".to_string(),
            ));
        }
        let (entries, snapshot) = self.pool.with(|db| Self::render_entries(db, &self.config))?;
        let mut alloc = crate::lock::lock_or_flag(&self.inodes, &self.needs_rebuild, "inodes");
        let tree = VirtualTree::build_with(&entries, &mut alloc);
        drop(alloc);
        self.tree.store(Arc::new(tree));
        Ok(snapshot)
    }
```

- [ ] **Step 5: Update the lock-order comment to drop the exception**

In the lock-order comment (~387-398), replace this sentence:

```
// inside `pool.with` during `refresh` — the one intentional exception where a
// pool connection is held around an in-memory lock. `handles` is a lock-free
```

with:

```
// Both rebuild paths (`rebuild_full`, `rebuild_incremental`) release the pool
// connection before locking `inodes`, so the order is uniform: a pool connection
// is never held around an in-memory lock. `handles` is a lock-free
```

- [ ] **Step 6: Run the new test and the rebuild/refresh regression tests**

Run: `cargo test -p musefs-core render_entries_returns_paths_and_snapshot open_handle_reresolves_after_content_version_bump`
Expected: PASS (2 passed). Then run the whole crate to confirm no regression:
Run: `cargo test -p musefs-core`
Expected: PASS (all green).

- [ ] **Step 7: Commit**

```bash
git add musefs-core/src/facade.rs
git commit -m "fix(core): release pool conn before locking inodes in rebuild_full (#90)

Extract the DB-read+render phase into render_entries so rebuild_full
holds the inodes lock only across the pure-CPU build_with, mirroring
rebuild_incremental. Removes the documented lock-order exception."
```

---

## Task 2: #89 (core) — add the synchronous `poll_due()` predicate

**Files:**
- Modify: `musefs-core/src/facade.rs` (add `poll_due` and two test hooks in `impl Musefs`; one-line comment on `poll_refresh_notify`'s gate block ~429)
- Test: `musefs-core/src/facade.rs` (`mod tests`)

`poll_due` mirrors `poll_refresh_notify`'s early-return gates (`needs_rebuild` → interval debounce → failure backoff) with no DB access. The two `*_for_test` hooks let the backoff branch be tested deterministically, companions to the existing `expire_poll_debounce_for_test`.

- [ ] **Step 1: Write the failing `poll_due` unit tests**

Add to `mod tests` in `musefs-core/src/facade.rs`:

```rust
    fn fs_with_poll_interval(interval: std::time::Duration) -> (tempfile::TempDir, Musefs) {
        let dir = tempfile::tempdir().unwrap();
        let cfg = MountConfig {
            template: "$artist/$title".to_string(),
            fallbacks: BTreeMap::new(),
            default_fallback: "Unknown".to_string(),
            mode: Mode::Synthesis,
            poll_interval: interval,
        };
        let fs = Musefs::open(musefs_db::Db::open(dir.path().join("m.db")).unwrap(), cfg).unwrap();
        (dir, fs)
    }

    #[test]
    fn poll_due_false_within_interval_true_after_expiry() {
        let (_d, fs) = fs_with_poll_interval(std::time::Duration::from_secs(3600));
        assert!(!fs.poll_due(), "fresh open is within the debounce window");
        fs.expire_poll_debounce_for_test();
        assert!(fs.poll_due(), "past the debounce window");
    }

    #[test]
    fn poll_due_true_when_needs_rebuild_regardless_of_interval() {
        let (_d, fs) = fs_with_poll_interval(std::time::Duration::from_secs(3600));
        assert!(!fs.poll_due());
        fs.mark_needs_rebuild_for_test();
        assert!(fs.poll_due(), "needs_rebuild bypasses the debounce");
    }

    #[test]
    fn poll_due_true_when_interval_zero() {
        let (_d, fs) = fs_with_poll_interval(std::time::Duration::ZERO);
        assert!(fs.poll_due(), "zero interval disables the debounce");
    }

    #[test]
    fn poll_due_respects_failure_backoff_window() {
        let (_d, fs) = fs_with_poll_interval(std::time::Duration::from_secs(3600));
        fs.expire_poll_debounce_for_test(); // get past the debounce gate first
        fs.fail_refresh_now_for_test();
        assert!(!fs.poll_due(), "inside the retry backoff window");
        fs.expire_refresh_backoff_for_test();
        assert!(fs.poll_due(), "past the retry backoff window");
    }
```

- [ ] **Step 2: Run the tests to verify they fail (do not compile)**

Run: `cargo test -p musefs-core poll_due_`
Expected: FAIL — compile errors: `no method named 'poll_due'`, `no method named 'fail_refresh_now_for_test'`, `no method named 'expire_refresh_backoff_for_test'`.

- [ ] **Step 3: Add `poll_due` and the two test hooks**

In `impl Musefs`, add `poll_due` (place it just above `poll_refresh`, ~before line 400):

```rust
    /// Cheap, synchronous "is a `data_version` poll worth dispatching?" predicate
    /// for the FUSE dispatch thread to gate `fire_poll_refresh` on, so a
    /// metadata-op storm doesn't flood the worker pool with no-op poll tasks (#89).
    /// Mirrors the early-return gates in `poll_refresh_notify` — keep the two in
    /// sync. Advisory only: no DB access, no `data_version` read, no rebuild. A
    /// stale `true` costs at most one task the inner gate short-circuits, and
    /// `needs_rebuild` is checked first so a self-heal is never debounced away.
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

Add the two hooks next to the existing `expire_poll_debounce_for_test` (~571):

```rust
    /// Stamps a failed-refresh time of "now" so the backoff gate is active, for
    /// tests exercising `poll_due`'s backoff branch without a real failure.
    #[doc(hidden)]
    pub fn fail_refresh_now_for_test(&self) {
        *crate::lock::lock_recover(&self.last_failed_refresh, "last_failed_refresh") =
            Some(std::time::Instant::now());
    }

    /// Backdates the failed-refresh stamp past the retry-backoff window so the
    /// backoff gate no longer blocks (companion to `expire_poll_debounce_for_test`).
    #[doc(hidden)]
    pub fn expire_refresh_backoff_for_test(&self) {
        let past = std::time::Instant::now()
            .checked_sub(self.refresh_retry_backoff)
            .expect("refresh_retry_backoff exceeds monotonic clock base");
        *crate::lock::lock_recover(&self.last_failed_refresh, "last_failed_refresh") = Some(past);
    }
```

- [ ] **Step 4: Add the cross-reference comment on `poll_refresh_notify`'s gate**

In `poll_refresh_notify`, immediately above the `if !self.poll_interval.is_zero()` debounce check (~429), add:

```rust
        // These early-return gates are mirrored by the cheap `poll_due` pre-check
        // the FUSE layer runs on the dispatch thread (#89); keep the two in sync.
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p musefs-core poll_due_`
Expected: PASS (4 passed).

- [ ] **Step 6: Commit**

```bash
git add musefs-core/src/facade.rs
git commit -m "feat(core): add synchronous poll_due() debounce predicate (#89)

poll_due mirrors poll_refresh_notify's early-return gates (needs_rebuild,
interval debounce, failure backoff) with no DB access, so the FUSE layer
can gate poll submission on the dispatch thread."
```

---

## Task 3: #89 (fuse) — synchronous gate + single-flight in `fire_poll_refresh`

**Files:**
- Modify: `musefs-fuse/src/lib.rs` (imports ~6-9; `MusefsFs` struct ~116-126; `MusefsFs::new` ~128-146; `fire_poll_refresh` ~151-176; add `PollPendingGuard`)
- Test: `musefs-fuse/src/lib.rs` (`mod tests` ~385)

Gate `fire_poll_refresh` on `core.poll_due()` (submit nothing when not due), then a `poll_pending` single-flight `compare_exchange` so at most one poll task is queued/running; an RAII `PollPendingGuard` clears the flag on every exit path including panic.

- [ ] **Step 1: Write the failing tests**

Add to `mod tests` in `musefs-fuse/src/lib.rs`:

```rust
    fn test_fs() -> (tempfile::TempDir, MusefsFs) {
        use musefs_core::{Mode, MountConfig, Musefs};
        let dir = tempfile::tempdir().unwrap();
        let cfg = MountConfig {
            template: "$artist/$title".to_string(),
            fallbacks: std::collections::BTreeMap::new(),
            default_fallback: "Unknown".to_string(),
            mode: Mode::Synthesis,
            // Zero interval => poll_due() is always true, isolating the gate.
            poll_interval: std::time::Duration::ZERO,
        };
        let core = Musefs::open(musefs_db::Db::open(dir.path().join("m.db")).unwrap(), cfg).unwrap();
        (dir, MusefsFs::new(core, FuseConfig::default()))
    }

    #[test]
    fn poll_pending_guard_clears_flag_on_panic() {
        let flag = Arc::new(AtomicBool::new(true));
        let f = Arc::clone(&flag);
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _g = PollPendingGuard(&f);
            panic!("boom");
        }));
        assert!(r.is_err());
        assert!(!flag.load(Ordering::SeqCst), "guard must clear the flag on unwind");
    }

    #[test]
    fn fire_poll_refresh_single_flights_when_pending() {
        let (_d, fs) = test_fs();
        // Simulate a poll already in flight; the gate must reject new submissions.
        fs.poll_pending.store(true, Ordering::SeqCst);
        let queued = fs.pool.queued_count();
        let active = fs.pool.active_count();
        for _ in 0..50 {
            fs.fire_poll_refresh();
        }
        assert_eq!(fs.pool.queued_count(), queued, "no task should be queued");
        assert_eq!(fs.pool.active_count(), active, "no task should be started");
    }

    #[test]
    fn fire_poll_refresh_clears_gate_after_task() {
        let (_d, fs) = test_fs();
        assert!(!fs.poll_pending.load(Ordering::SeqCst));
        fs.fire_poll_refresh(); // poll_due() true (zero interval): gate taken, task runs
        fs.pool.join(); // block until the poll task completes
        assert!(
            !fs.poll_pending.load(Ordering::SeqCst),
            "guard must clear the gate after the task finishes"
        );
    }
```

- [ ] **Step 2: Run the tests to verify they fail (do not compile)**

Run: `cargo test -p musefs-fuse poll_pending fire_poll_refresh`
Expected: FAIL — compile errors: `cannot find type 'PollPendingGuard'`, `no field 'poll_pending'`, unresolved `AtomicBool`/`Ordering`.

- [ ] **Step 3: Add imports and the `PollPendingGuard` type**

Change the `std::sync` import line (~8) to add the atomics:

```rust
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
```

Add this type just above `pub struct MusefsFs` (~115):

```rust
/// Clears the `fire_poll_refresh` single-flight gate when the poll task ends,
/// on every exit path including a panic in `poll_refresh_notify` (#89).
struct PollPendingGuard<'a>(&'a AtomicBool);

impl Drop for PollPendingGuard<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}
```

- [ ] **Step 4: Add the `poll_pending` field and initialize it**

Add to the `MusefsFs` struct (after the `notifier` field, ~125):

```rust
    /// Single-flight gate for `fire_poll_refresh`: at most one poll task is
    /// queued/running at a time, so a metadata-op storm can't flood the pool (#89).
    poll_pending: Arc<AtomicBool>,
```

Add to the `MusefsFs { … }` literal in `MusefsFs::new` (after `notifier: Arc::new(OnceLock::new()),`, ~144):

```rust
            poll_pending: Arc::new(AtomicBool::new(false)),
```

- [ ] **Step 5: Rewrite `fire_poll_refresh`**

Replace the whole `fire_poll_refresh` method (~151-176) with:

```rust
    /// Fire `poll_refresh` on the worker pool (off the dispatch thread), but only
    /// when due: a cheap synchronous `poll_due()` check gates submission so a
    /// metadata-op storm doesn't flood the pool, and a `poll_pending` single-flight
    /// gate bounds in-flight poll tasks to one (#89). When keep-cache is enabled,
    /// also drop the kernel page cache for every inode whose content changed.
    fn fire_poll_refresh(&self) {
        if !self.core.poll_due() {
            return;
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
        if self.config.keep_cache {
            let notifier = Arc::clone(&self.notifier);
            self.pool.execute(move || {
                let _guard = PollPendingGuard(&pending);
                if let Err(e) = core.poll_refresh_notify(|ino| {
                    if let Some(n) = notifier.get() {
                        if let Err(inval_err) = n.inval_inode(INodeNo(ino), 0, 0) {
                            log::warn!("inval_inode({ino}) failed: {inval_err}");
                        }
                    }
                }) {
                    log::warn!("poll_refresh_notify failed: {e}");
                }
            });
        } else {
            self.pool.execute(move || {
                let _guard = PollPendingGuard(&pending);
                if let Err(e) = core.poll_refresh() {
                    log::warn!("poll_refresh failed: {e}");
                }
            });
        }
    }
```

- [ ] **Step 6: Run the tests to verify they pass**

Run: `cargo test -p musefs-fuse poll_pending fire_poll_refresh`
Expected: PASS (3 passed).

- [ ] **Step 7: Commit**

```bash
git add musefs-fuse/src/lib.rs
git commit -m "fix(fuse): gate fire_poll_refresh with poll_due + single-flight (#89)

Check the cheap synchronous poll_due() on the dispatch thread before
submitting, and bound in-flight poll tasks to one via a poll_pending
gate cleared by an RAII guard, so a metadata-op storm no longer floods
the worker pool."
```

---

## Task 4: Full verification + roadmap

**Files:**
- Modify: `docs/ROADMAP.md` (Phase 4 section ~170-172)

- [ ] **Step 1: Format and lint the whole workspace**

Run: `cargo fmt --all --check`
Expected: clean (no diff). If it reports a diff, run `cargo fmt --all` and re-stage.
Run: `cargo clippy --all-targets -- -D warnings`
Expected: clean (no warnings).

- [ ] **Step 2: Run the full workspace test suite**

Run: `cargo test --workspace`
Expected: PASS (all crates green, including the new Task 1–3 tests).

- [ ] **Step 3: Run the FUSE end-to-end mount suite**

These are `#[ignore]`d real mounts; the dev server has `/dev/fuse`.

Run: `cargo test -p musefs-fuse -- --ignored`
Expected: PASS — including `keep_cache_mount_reflects_retag_after_refresh` and the concurrency mount tests that exercise `fire_poll_refresh` end to end.

- [ ] **Step 4: Mark Phase 4 done in the roadmap**

In `docs/ROADMAP.md`, replace the Phase 4 block:

```
**Phase 4 — Concurrency correctness**
- #90 — `rebuild_full` holds `inodes` across DB I/O (mirror the incremental path).
- #89 — `fire_poll_refresh` floods the threadpool (synchronous debounce).
```

with:

```
**Phase 4 — Concurrency correctness** — *done*
- ~~#90 — `rebuild_full` holds `inodes` across DB I/O~~ — done: `render_entries`
  does the DB read + render under the pool connection; `rebuild_full` locks
  `inodes` only across the pure-CPU `build_with` (mirrors the incremental path),
  removing the documented lock-order exception.
- ~~#89 — `fire_poll_refresh` floods the threadpool~~ — done: a synchronous
  `poll_due()` gate on the dispatch thread skips submission within the debounce
  window, and a `poll_pending` single-flight gate bounds in-flight poll tasks to
  one (robust even with `--poll-interval-ms 0`).
```

- [ ] **Step 5: Commit**

```bash
git add docs/ROADMAP.md
git commit -m "docs: mark Phase 4 (#89, #90) done in roadmap"
```

---

## Self-Review Notes

- **Spec coverage:** #90 fix (render_entries extraction + rebuild_full rewrite + lock-order comment) → Task 1. #89 `poll_due` (core, with the listed gate cases incl. interval-zero and backoff) → Task 2. #89 `poll_pending` field + `MusefsFs::new` init + `PollPendingGuard` + `fire_poll_refresh` rewrite → Task 3. Spec's testing section: #90 structural boundary covered by `render_entries` unit test + existing regression tests (no DB-injection/blocking seam exists, per spec — no runtime "not blocked" assertion); #89 `poll_due` deterministic core tests; #89 gate single-flight + guard-on-panic deterministic fuse tests; existing `#[ignore]` mount tests as end-to-end → Task 4 Step 3.
- **Type consistency:** `render_entries(&Db, &MountConfig) -> Result<(Vec<(i64, String)>, HashMap<i64, TrackRenderState>)>` used identically in Task 1 test and `build_full`/`rebuild_full`. `PollPendingGuard(&AtomicBool)` defined in Task 3 Step 3, constructed in the rewrite (Step 5) and the panic test (Step 1). `poll_pending: Arc<AtomicBool>` field name consistent across struct, init, rewrite, and tests. `poll_due` signature `pub fn poll_due(&self) -> bool` consistent between Task 2 (def) and Task 3 (call `self.core.poll_due()`).
- **No placeholders:** every code/command step contains full content and exact commands with expected output.
