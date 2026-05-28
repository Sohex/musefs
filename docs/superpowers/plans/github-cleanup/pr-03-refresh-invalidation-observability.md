# PR 3 Refresh Invalidation Observability Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make refresh retry semantics correct, invalidate old keep-cache inodes for path-changing retags, and log refresh/invalidation failures.

**Architecture:** Split refresh timing into successful poll checks and failed rebuild retry backoff. Snapshot the old tree before rebuild and compare old/new track-to-inode mappings. Keep FUSE changes limited to logging failures from the existing core callback.

**Tech Stack:** Rust, musefs-core refresh path, arc-swap snapshots, fuser notifier, log crate.

---

### Task 1: Fix Refresh Debounce And Failure Backoff

**Files:**
- Modify: `musefs-core/src/facade.rs`
- Test: `musefs-core/tests/facade.rs`

- [ ] **Step 1: Add timing tests**

Add tests that prove both requirements:

```rust
#[test]
fn unchanged_refresh_poll_consumes_debounce_window() {
    let fs = fixture_with_poll_interval(Duration::from_secs(60));
    assert_eq!(fs.poll_refresh().unwrap(), false);
    assert_eq!(fs.poll_refresh().unwrap(), false);
    assert_eq!(fixture_poll_count(&fs), 1);
}

#[test]
fn failed_refresh_retries_after_backoff_not_every_call() {
    let fs = fixture_with_poll_interval_and_bad_changed_track(
        Duration::from_secs(60),
        Duration::from_millis(250),
    );
    assert!(fs.poll_refresh().is_err());
    assert_eq!(fs.poll_refresh().unwrap(), false);
    std::thread::sleep(Duration::from_millis(260));
    assert!(fs.poll_refresh().is_err());
}
```

Use existing test helpers where possible. If no poll counter exists, expose a
small `#[cfg(test)]` helper in `Musefs` to inspect timing state or build the test
around observable retry attempts.

- [ ] **Step 2: Implement separate timing state**

In `Musefs`, track:

```rust
last_poll: Mutex<std::time::Instant>,
last_failed_refresh: Mutex<Option<std::time::Instant>>,
refresh_retry_backoff: std::time::Duration,
```

Use a small fixed backoff such as `poll_interval.min(Duration::from_secs(1))`
with a floor of `Duration::from_millis(100)` when `poll_interval` is nonzero.

`poll_refresh_notify` rules:
- If `last_poll.elapsed() < poll_interval`, return `Ok(false)`.
- If the last refresh failed and `last_failed_refresh.elapsed() < refresh_retry_backoff`, return `Ok(false)`.
- After a successful `data_version` check with no changes, update `last_poll`.
- After a successful rebuild, update `last_poll`, clear `last_failed_refresh`, and store `last_data_version`.
- After a failed rebuild, set `last_failed_refresh` and return the error without storing `last_data_version`.

- [ ] **Step 3: Verify timing tests**

Run:

```bash
cargo test -p musefs-core unchanged_refresh_poll_consumes_debounce_window failed_refresh_retries_after_backoff_not_every_call -- --nocapture
```

Expected: both pass.

### Task 2: Invalidate Old Inodes For Path-Changing Retags

**Files:**
- Modify: `musefs-core/src/facade.rs`
- Test: `musefs-core/tests/facade.rs`

- [ ] **Step 1: Add path-change invalidation test**

Write a test that:
- creates a track whose template path includes `$title`;
- records the original file inode;
- changes the title through the DB so `content_version` changes and path changes;
- calls `poll_refresh_notify`;
- asserts the callback includes the old inode and the new inode if bytes changed.

- [ ] **Step 2: Implement old/new inode comparison**

In `poll_refresh_notify`:

```rust
let old_tree = self.tree.load_full();
let old_versions = self.versions.lock().unwrap_or_else(...).clone();
let new_versions = self.rebuild()?;
let new_tree = self.tree.load();

for (tid, new_ver) in &new_versions {
    if old_versions.get(tid).is_some_and(|old| old != new_ver) {
        if let Some(ino) = new_tree.inode_of_track(*tid) {
            on_changed(ino);
        }
        let old_ino = old_tree.inode_of_track(*tid);
        let new_ino = new_tree.inode_of_track(*tid);
        if old_ino != new_ino {
            if let Some(ino) = old_ino {
                on_changed(ino);
            }
        }
    }
}
for tid in old_versions.keys().filter(|tid| !new_versions.contains_key(tid)) {
    if let Some(ino) = old_tree.inode_of_track(*tid) {
        on_changed(ino);
    }
}
```

Deduplicate callback inodes if a test exposes duplicates.

### Task 3: Log FUSE Refresh And Invalidation Failures

**Files:**
- Modify: `musefs-fuse/Cargo.toml`
- Modify: `musefs-fuse/src/lib.rs`

- [ ] **Step 1: Add dependency**

Add to `musefs-fuse/Cargo.toml`:

```toml
log = "0.4"
```

- [ ] **Step 2: Log errors in `fire_poll_refresh`**

Replace discarded results with:

```rust
if let Err(e) = core.poll_refresh_notify(|ino| {
    if let Some(n) = notifier.get() {
        if let Err(inval_err) = n.inval_inode(ino, 0, 0) {
            log::warn!("inval_inode({ino}) failed: {inval_err}");
        }
    }
}) {
    log::warn!("poll_refresh_notify failed: {e}");
}
```

Use the analogous `log::warn!("poll_refresh failed: {e}")` for non-keep-cache.

- [ ] **Step 3: Verify**

Run:

```bash
cargo test -p musefs-core
cargo test -p musefs-fuse
```

- [ ] **Step 4: Commit**

```bash
git add musefs-core/src/facade.rs musefs-core/tests/facade.rs musefs-fuse/Cargo.toml musefs-fuse/src/lib.rs Cargo.lock
git commit -m "fix(core): bound refresh retries and invalidate moved inodes

Closes #6
Closes #7
Closes #8"
```
