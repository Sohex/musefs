# PR 2 Core DB Read Safety Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Fix same-thread multi-DB `DbPool` reuse and reject backing-file replacement before caching an open handle.

**Architecture:** Keep `musefs-core` responsible for pooling and open-handle safety. Key per-thread read connections by database path and validate the opened file descriptor with descriptor metadata (`File::metadata()`/`fstat`), never path metadata after open.

**Tech Stack:** Rust, musefs-core, musefs-db, tempfile tests.

---

### Task 1: Key Thread-Local DB Connections By Path

**Files:**
- Modify: `musefs-core/src/db_pool.rs`

- [ ] **Step 1: Write the regression test**

Add a unit test in `musefs-core/src/db_pool.rs`:

```rust
#[test]
fn same_thread_two_pools_keyed_by_path() {
    let dir = tempfile::tempdir().unwrap();
    let path_a = dir.path().join("a.db");
    let path_b = dir.path().join("b.db");
    Db::open(&path_a).unwrap();
    Db::open(&path_b).unwrap();

    let pool_a = DbPool::new(Db::open(&path_a).unwrap()).unwrap();
    let pool_b = DbPool::new(Db::open(&path_b).unwrap()).unwrap();

    pool_a
        .with(|db| {
            assert_eq!(db.path().unwrap(), path_a);
            Ok(())
        })
        .unwrap();
    pool_b
        .with(|db| {
            assert_eq!(db.path().unwrap(), path_b);
            Ok(())
        })
        .unwrap();
}
```

- [ ] **Step 2: Verify the test fails before implementation**

Run:

```bash
cargo test -p musefs-core same_thread_two_pools_keyed_by_path -- --nocapture
```

Expected before the fix: failure because both pools reuse the first thread-local
connection.

- [ ] **Step 3: Implement keyed thread-local connections**

In `musefs-core/src/db_pool.rs`, replace the single `Option<Db>` thread local
with:

```rust
use std::collections::HashMap;

thread_local! {
    static PER_PATH: RefCell<HashMap<PathBuf, Db>> = RefCell::new(HashMap::new());
}
```

Change `DbPool::with` for `PerThread` to:

```rust
DbPool::PerThread { path, .. } => PER_PATH.with(|cell| {
    let mut map = cell.borrow_mut();
    if !map.contains_key(path) {
        let db = Db::open_readonly(path)?;
        map.insert(path.clone(), db);
    }
    f(map.get(path).expect("connection inserted for path"))
}),
```

Do not use `expect` for `Db::open_readonly`; open errors must propagate as
`Result` values.

- [ ] **Step 4: Verify the regression test passes**

Run:

```bash
cargo test -p musefs-core same_thread_two_pools_keyed_by_path -- --nocapture
```

Expected: pass.

### Task 2: Validate Opened Descriptor Metadata

**Files:**
- Modify: `musefs-core/src/reader.rs`
- Modify: `musefs-core/src/facade.rs`
- Test: `musefs-core/tests/facade.rs` or `musefs-core/tests/read_at.rs`

- [ ] **Step 1: Add a failing descriptor-mismatch test**

Add an integration test that creates a scanned track, resolves its inode, replaces
the backing file with a different size or mtime, then calls `open_handle`.

The assertion must be:

```rust
let err = fs.open_handle(file_inode).unwrap_err();
assert!(matches!(err, musefs_core::CoreError::BackingChanged(_)));
```

The test must fail before implementation by successfully opening the replaced
file.

- [ ] **Step 2: Carry the expected backing size in `ResolvedFile`**

Add `pub backing_size: u64` to `ResolvedFile` in `musefs-core/src/reader.rs` and
initialize it from `track.backing_size as u64` in `HeaderCache::build`. Update
all test `ResolvedFile` literals.

- [ ] **Step 3: Validate the opened descriptor**

In `Musefs::open_handle` in `musefs-core/src/facade.rs`, open the file, then
validate descriptor metadata:

```rust
let file = std::fs::File::open(&resolved.backing_path)?;
let meta = file.metadata()?;
if meta.len() != resolved.backing_size {
    return Err(CoreError::BackingChanged(
        resolved.backing_path.to_string_lossy().to_string(),
    ));
}
let mtime = meta
    .modified()
    .ok()
    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
    .map_or(0, |d| d.as_secs() as i64);
if mtime != resolved.mtime_secs {
    return Err(CoreError::BackingChanged(
        resolved.backing_path.to_string_lossy().to_string(),
    ));
}
```

This must use `file.metadata()`, not `std::fs::metadata(&resolved.backing_path)`.

- [ ] **Step 4: Verify core tests**

Run:

```bash
cargo test -p musefs-core same_thread_two_pools_keyed_by_path open_handle -- --nocapture
cargo test -p musefs-core
```

Expected: all pass.

- [ ] **Step 5: Commit**

```bash
git add musefs-core/src/db_pool.rs musefs-core/src/reader.rs musefs-core/src/facade.rs musefs-core/tests
git commit -m "fix(core): key DbPool by path and validate opened descriptor

Closes #4
Closes #5"
```
