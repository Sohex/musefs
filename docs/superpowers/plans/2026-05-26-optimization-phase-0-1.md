# musefs Optimization — Phase 0 + Phase 1 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build the benchmark/instrumentation harness (Phase 0) and the concurrency foundation (Phase 1) from the spec `docs/superpowers/specs/2026-05-26-optimization-pass-design.md`, so a slow backing read can no longer block unrelated FUSE operations.

**Architecture:** Phase 0 adds zero-cost (feature-gated) syscall/query counters plus optional per-syscall latency injection, and a baseline read benchmark. Phase 1 converts `Musefs` from `&mut self` (single-threaded) to `&self` interior mutability (`ArcSwap` tree, `Mutex` cache, atomic version), introduces a `DbPool` that hands each worker thread its own read-only SQLite connection (WAL makes these contention-free), and reshapes the FUSE adapter to offload blocking ops (`read`, `getattr`, `lookup`'s attr step) onto a bounded worker pool while keeping pure tree ops (`readdir`) inline. fuser's `Reply*` objects are `Send`, so a worker computes and answers the kernel off the dispatch thread.

**Tech Stack:** Rust, `fuser` 0.14, `rusqlite` 0.31 (WAL), new deps `arc-swap`, `threadpool`, dev-dep `criterion`.

**Invariant (non-negotiable):** served audio stays byte-identical. Every task must keep the existing e2e mount tests green (`cargo test -p musefs-fuse -- --ignored`).

**Note on fuser specifics:** Some `Send`-ness/API details of fuser 0.14 (`ReplyData`/`ReplyEntry`/`ReplyAttr` being `Send`) are confirmed by the compiler when Phase 1c builds. Treat compiler/test output as the source of truth; the code below is written to the documented fuser pattern.

---

## File Structure

**Phase 0**
- Create `musefs-core/src/metrics.rs` — feature-gated counters + fault injection. One responsibility: observability hooks.
- Modify `musefs-core/src/lib.rs` — `pub mod metrics;`.
- Modify `musefs-core/Cargo.toml` — `metrics` feature; `criterion` dev-dep; `[[bench]]`.
- Modify `musefs-core/src/reader.rs` — call the metrics hooks at the `open`/`pread`/`art`/`stat` sites.
- Create `musefs-core/tests/metrics.rs` — baseline syscall-count test (own test binary, own process).
- Create `musefs-core/benches/read_throughput.rs` — sequential-read criterion bench.

**Phase 1**
- Modify `musefs-db/src/lib.rs` — add `path` field, `Db::path()`, `Db::open_readonly()`.
- Create `musefs-core/src/db_pool.rs` — `DbPool` (per-thread read connections or shared in-memory).
- Modify `musefs-core/src/lib.rs` — `mod db_pool;` + re-export.
- Modify `musefs-core/src/facade.rs` — `Musefs` interior mutability; `&self` methods; pool-backed DB access.
- Modify `musefs-core/Cargo.toml` — add `arc-swap`.
- Modify `musefs-core/tests/facade.rs` — adjust `let mut fs` bindings (now `&self`).
- Modify `musefs-fuse/src/lib.rs` — `MusefsFs` holds `Arc<Musefs>` + worker pool; offload blocking ops.
- Modify `musefs-fuse/Cargo.toml` — add `threadpool`.
- Create `musefs-fuse/tests/concurrency.rs` — slow read does not block a metadata op.

---

# Phase 0 — Benchmark & instrumentation harness

## Task 0.1: Metrics module (feature-gated counters + fault injection)

**Files:**
- Create: `musefs-core/src/metrics.rs`
- Modify: `musefs-core/src/lib.rs`
- Modify: `musefs-core/Cargo.toml`

- [ ] **Step 1: Add the feature and bench wiring to `musefs-core/Cargo.toml`**

Add under a new `[features]` section and to dev-deps/bench:

```toml
[features]
metrics = []

[dev-dependencies]
# (existing dev-deps stay)
criterion = "0.5"

[[bench]]
name = "read_throughput"
harness = false
```

- [ ] **Step 2: Write the failing unit test (inside the new module)**

Create `musefs-core/src/metrics.rs` with ONLY the test first so it fails to compile (drives the API):

```rust
#[cfg(all(test, feature = "metrics"))]
mod tests {
    use super::*;

    #[test]
    fn counters_accumulate_and_reset() {
        reset();
        on_open();
        on_open();
        on_pread(100);
        on_art_chunk();
        let s = snapshot();
        assert_eq!(s.opens, 2);
        assert_eq!(s.preads, 1);
        assert_eq!(s.pread_bytes, 100);
        assert_eq!(s.art_chunks, 1);
        reset();
        assert_eq!(snapshot(), Snapshot::default());
    }
}
```

- [ ] **Step 3: Run it to confirm it fails**

Run: `cargo test -p musefs-core --features metrics metrics:: 2>&1 | head -30`
Expected: FAIL — `cannot find function on_open` / `Snapshot` not found.

- [ ] **Step 4: Implement the module above the test block**

Prepend to `musefs-core/src/metrics.rs`:

```rust
//! Optional syscall/query counters and per-syscall latency injection for
//! benchmarking. Zero-cost when the `metrics` feature is off: every hook
//! compiles to an empty inline fn, so call sites stay unconditional and clean.

pub use imp::*;

#[cfg(feature = "metrics")]
mod imp {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::OnceLock;
    use std::time::Duration;

    static OPENS: AtomicU64 = AtomicU64::new(0);
    static STATS: AtomicU64 = AtomicU64::new(0);
    static PREADS: AtomicU64 = AtomicU64::new(0);
    static PREAD_BYTES: AtomicU64 = AtomicU64::new(0);
    static ART_CHUNKS: AtomicU64 = AtomicU64::new(0);

    /// Sleep for the duration named by `var` (microseconds), parsed once.
    fn fault(var: &'static str, cell: &OnceLock<Option<Duration>>) {
        let d = cell.get_or_init(|| {
            std::env::var(var)
                .ok()
                .and_then(|s| s.parse::<u64>().ok())
                .filter(|&us| us > 0)
                .map(Duration::from_micros)
        });
        if let Some(d) = d {
            std::thread::sleep(*d);
        }
    }

    pub fn on_open() {
        OPENS.fetch_add(1, Ordering::Relaxed);
        static C: OnceLock<Option<Duration>> = OnceLock::new();
        fault("MUSEFS_FAULT_OPEN_US", &C);
    }

    pub fn on_stat() {
        STATS.fetch_add(1, Ordering::Relaxed);
        static C: OnceLock<Option<Duration>> = OnceLock::new();
        fault("MUSEFS_FAULT_STAT_US", &C);
    }

    pub fn on_pread(bytes: u64) {
        PREADS.fetch_add(1, Ordering::Relaxed);
        PREAD_BYTES.fetch_add(bytes, Ordering::Relaxed);
        static C: OnceLock<Option<Duration>> = OnceLock::new();
        fault("MUSEFS_FAULT_PREAD_US", &C);
    }

    pub fn on_art_chunk() {
        ART_CHUNKS.fetch_add(1, Ordering::Relaxed);
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
    pub struct Snapshot {
        pub opens: u64,
        pub stats: u64,
        pub preads: u64,
        pub pread_bytes: u64,
        pub art_chunks: u64,
    }

    pub fn snapshot() -> Snapshot {
        Snapshot {
            opens: OPENS.load(Ordering::Relaxed),
            stats: STATS.load(Ordering::Relaxed),
            preads: PREADS.load(Ordering::Relaxed),
            pread_bytes: PREAD_BYTES.load(Ordering::Relaxed),
            art_chunks: ART_CHUNKS.load(Ordering::Relaxed),
        }
    }

    pub fn reset() {
        OPENS.store(0, Ordering::Relaxed);
        STATS.store(0, Ordering::Relaxed);
        PREADS.store(0, Ordering::Relaxed);
        PREAD_BYTES.store(0, Ordering::Relaxed);
        ART_CHUNKS.store(0, Ordering::Relaxed);
    }
}

#[cfg(not(feature = "metrics"))]
mod imp {
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
    pub struct Snapshot {
        pub opens: u64,
        pub stats: u64,
        pub preads: u64,
        pub pread_bytes: u64,
        pub art_chunks: u64,
    }

    #[inline(always)]
    pub fn on_open() {}
    #[inline(always)]
    pub fn on_stat() {}
    #[inline(always)]
    pub fn on_pread(_bytes: u64) {}
    #[inline(always)]
    pub fn on_art_chunk() {}
    #[inline(always)]
    pub fn snapshot() -> Snapshot {
        Snapshot::default()
    }
    #[inline(always)]
    pub fn reset() {}
}
```

- [ ] **Step 5: Register the module in `musefs-core/src/lib.rs`**

Add alongside the other `mod` lines (it currently has `mod facade; ... mod reader;` plus re-exports). Add:

```rust
pub mod metrics;
```

- [ ] **Step 6: Run the unit test (both feature states compile)**

Run: `cargo test -p musefs-core --features metrics metrics::`
Expected: PASS (1 test).
Run: `cargo build -p musefs-core`
Expected: builds with no `metrics` feature (no-op hooks), no warnings about unused items.

- [ ] **Step 7: Commit**

```bash
git add musefs-core/src/metrics.rs musefs-core/src/lib.rs musefs-core/Cargo.toml
git commit -m "feat(core): feature-gated metrics counters + fault injection"
```

## Task 0.2: Instrument the read path

**Files:**
- Modify: `musefs-core/src/reader.rs` (`read_at` body; `resolve` body)

- [ ] **Step 1: Add hook calls in `read_at`**

In `musefs-core/src/reader.rs`, inside `read_at`, instrument the three syscall/DB sites. In the `Segment::BackingAudio` arm, the file-open and the positioned read:

```rust
                Segment::BackingAudio { offset: bo, .. } => {
                    if backing.is_none() {
                        crate::metrics::on_open();
                        backing = Some(std::fs::File::open(&resolved.backing_path)?);
                    }
                    let f = backing.as_ref().unwrap();
                    let mut buf = vec![0u8; n];
                    f.read_exact_at(&mut buf, bo + within)?;
                    crate::metrics::on_pread(n as u64);
                    out.extend_from_slice(&buf);
                }
```

In the `Segment::ArtImage` arm, after the chunk read:

```rust
                Segment::ArtImage { art_id, .. } => {
                    let chunk = db.read_art_chunk(*art_id, within, n)?;
                    crate::metrics::on_art_chunk();
                    out.extend_from_slice(&chunk);
                }
```

In the `Segment::OggAudio` arm, where the backing file is opened (mirror the `BackingAudio` open hook):

```rust
                    if backing.is_none() {
                        crate::metrics::on_open();
                        backing = Some(std::fs::File::open(&resolved.backing_path)?);
                    }
```

In the `Segment::OggArtSlice` arm, add `crate::metrics::on_art_chunk();` after each `db.read_art_chunk(...)` call (both the `base64` and raw branches).

- [ ] **Step 2: Add the stat hook in `resolve`**

In `HeaderCache::resolve`, immediately before the `std::fs::metadata` call:

```rust
        crate::metrics::on_stat();
        let meta = std::fs::metadata(&track.backing_path)?;
```

Also add `crate::metrics::on_open();` at the top of the free function `read_front` (before `std::fs::File::open(path)?`), since FLAC/Ogg synthesis opens the front during resolve.

- [ ] **Step 3: Verify it still builds and existing tests pass**

Run: `cargo test -p musefs-core`
Expected: PASS (hooks are no-ops without the feature; behavior unchanged).
Run: `cargo build -p musefs-core --features metrics`
Expected: builds.

- [ ] **Step 4: Commit**

```bash
git add musefs-core/src/reader.rs
git commit -m "feat(core): instrument read/resolve syscall + art sites with metrics hooks"
```

## Task 0.3: Baseline syscall-count test (documents the per-read `open()` problem)

**Files:**
- Create: `musefs-core/tests/metrics.rs`

This is its own test binary (own process), so the global counters are not shared with other test files. It records the current behavior — one `open()` per `read()` call — which Phase 2 (a later plan) will drive down to one per file.

- [ ] **Step 1: Write the test**

Create `musefs-core/tests/metrics.rs`:

```rust
#![cfg(feature = "metrics")]

mod common;
use common::{make_flac, streaminfo_body, vorbis_comment_body};
use musefs_core::{metrics, scan_directory, MountConfig, Musefs, VirtualTree};
use std::collections::BTreeMap;

fn config() -> MountConfig {
    MountConfig {
        template: "$artist/$title".to_string(),
        fallbacks: BTreeMap::new(),
        default_fallback: "Unknown".to_string(),
        mode: musefs_core::Mode::Synthesis,
    }
}

#[test]
fn baseline_one_open_per_read_call() {
    let dir = tempfile::tempdir().unwrap();
    let flac = make_flac(
        &[
            (0, streaminfo_body()),
            (4, vorbis_comment_body("v", &["ARTIST=Alice", "TITLE=Song"])),
        ],
        &vec![0xCD; 64 * 1024],
    );
    std::fs::write(dir.path().join("a.flac"), &flac).unwrap();

    let db = musefs_db::Db::open_in_memory().unwrap();
    scan_directory(&db, dir.path()).unwrap();
    let fs = Musefs::open(db, config()).unwrap();

    let artist = fs.lookup(VirtualTree::ROOT, "Alice").unwrap();
    let (_, file_inode, _) = fs.readdir(artist).unwrap().into_iter().next().unwrap();
    let size = fs.getattr(file_inode).unwrap().size;

    metrics::reset();
    // Read the file in 16 KiB chunks (the access pattern a streaming player produces).
    let chunk = 16 * 1024u64;
    let mut off = 0u64;
    let mut reads = 0u64;
    while off < size {
        let got = fs.read(file_inode, off, chunk).unwrap();
        if got.is_empty() {
            break;
        }
        off += got.len() as u64;
        reads += 1;
    }
    let s = metrics::snapshot();

    // BASELINE (pre-Phase-2): the backing file is reopened on every read() call.
    // Phase 2 (file-handle lifecycle) will reduce this to ~1 open per file.
    assert!(reads >= 2, "expected a multi-chunk read, got {reads}");
    assert_eq!(s.opens, reads, "currently one open() per read() call");
    assert!(s.pread_bytes >= size, "all audio bytes were read");
}
```

This test uses `fs.lookup`/`readdir`/`getattr`/`read` on a non-`mut` `fs`. Those methods are still `&mut self` until Phase 1a, so until then make the binding `let mut fs`. **After Phase 1a lands, change it back to `let fs`.** (Pick whichever matches the current state when you run it; both compile, `&self` just emits an unused-`mut` warning.)

- [ ] **Step 2: Run it**

Run: `cargo test -p musefs-core --features metrics --test metrics`
Expected: PASS, recording the `opens == reads` baseline. (If it fails to compile on the binding, flip `let fs`/`let mut fs` per the note above.)

- [ ] **Step 3: Commit**

```bash
git add musefs-core/tests/metrics.rs
git commit -m "test(core): baseline syscall-count test (one open per read call)"
```

## Task 0.4: Sequential-read throughput benchmark

**Files:**
- Create: `musefs-core/benches/read_throughput.rs`

- [ ] **Step 1: Write the benchmark**

Create `musefs-core/benches/read_throughput.rs`:

```rust
use std::collections::BTreeMap;

use criterion::{criterion_group, criterion_main, Criterion, Throughput};
use musefs_core::{scan_directory, MountConfig, Musefs, VirtualTree};

#[path = "../tests/common/mod.rs"]
mod common;
use common::{make_flac, streaminfo_body, vorbis_comment_body};

fn config() -> MountConfig {
    MountConfig {
        template: "$artist/$title".to_string(),
        fallbacks: BTreeMap::new(),
        default_fallback: "Unknown".to_string(),
        mode: musefs_core::Mode::Synthesis,
    }
}

fn bench_sequential_read(c: &mut Criterion) {
    let audio_len = 4 * 1024 * 1024usize; // 4 MiB
    let dir = tempfile::tempdir().unwrap();
    let flac = make_flac(
        &[
            (0, streaminfo_body()),
            (4, vorbis_comment_body("v", &["ARTIST=Alice", "TITLE=Song"])),
        ],
        &vec![0x7E; audio_len],
    );
    std::fs::write(dir.path().join("a.flac"), &flac).unwrap();

    let db = musefs_db::Db::open_in_memory().unwrap();
    scan_directory(&db, dir.path()).unwrap();
    let fs = Musefs::open(db, config()).unwrap();
    let artist = fs.lookup(VirtualTree::ROOT, "Alice").unwrap();
    let (_, file_inode, _) = fs.readdir(artist).unwrap().into_iter().next().unwrap();
    let size = fs.getattr(file_inode).unwrap().size;

    let mut group = c.benchmark_group("sequential_read");
    group.throughput(Throughput::Bytes(size));
    group.bench_function("flac_128k_chunks", |b| {
        b.iter(|| {
            let chunk = 128 * 1024u64;
            let mut off = 0u64;
            while off < size {
                let got = fs.read(file_inode, off, chunk).unwrap();
                if got.is_empty() {
                    break;
                }
                off += got.len() as u64;
            }
        });
    });
    group.finish();
}

criterion_group!(benches, bench_sequential_read);
criterion_main!(benches);
```

Note: `fs.read`/`getattr` are `&mut self` until Phase 1a. If running this bench before Phase 1a, make `fs` `mut` and wrap reads accordingly; after Phase 1a it works as written with `&self`.

- [ ] **Step 2: Run the benchmark once to confirm it executes**

Run: `cargo bench -p musefs-core --bench read_throughput -- --warm-up-time 1 --measurement-time 3`
Expected: criterion prints a `sequential_read/flac_128k_chunks` time + throughput. Record this number as the pre-optimization baseline in the PR description.

- [ ] **Step 3: Commit**

```bash
git add musefs-core/benches/read_throughput.rs musefs-core/Cargo.toml
git commit -m "bench(core): sequential FLAC read throughput baseline"
```

---

# Phase 1a — Interior-mutability refactor (`&mut self` → `&self`)

This restructures `Musefs` so its methods take `&self` with interior mutability, without changing threading yet (`mount2` still drives it on one thread). This is the prerequisite for offloading work to other threads.

## Task 1a.1: Convert `Musefs` to interior mutability

**Files:**
- Modify: `musefs-core/Cargo.toml` (add `arc-swap`)
- Modify: `musefs-core/src/facade.rs` (struct + all methods)
- Modify: `musefs-core/tests/facade.rs` (binding adjustments)
- Modify: `musefs-fuse/src/lib.rs` (callers compile against `&self`)

- [ ] **Step 1: Add the dependency**

In `musefs-core/Cargo.toml` `[dependencies]`:

```toml
arc-swap = "1"
```

- [ ] **Step 2: Update imports at the top of `musefs-core/src/facade.rs`**

Add:

```rust
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Mutex;

use arc_swap::ArcSwap;
use std::sync::Arc;
```

- [ ] **Step 3: Replace the `Musefs` struct definition**

Use `replace_symbol_body` on `Musefs`:

```rust
/// The composed read-only filesystem: the store, the rendered tree, and the
/// lazy synthesis cache. All methods take `&self`; the tree is swapped
/// atomically on refresh, the cache is mutex-guarded, and the data-version
/// stamp is atomic. This makes `Musefs` `Sync`, so the FUSE layer can share it
/// across a worker pool.
pub struct Musefs {
    db: Db,
    config: MountConfig,
    tree: ArcSwap<VirtualTree>,
    cache: Mutex<HeaderCache>,
    last_data_version: AtomicI64,
}
```

(Phase 1b replaces the `db: Db` field with a `DbPool`; keep it as `Db` for this task and lock nothing — the methods below call `&self.db` directly.)

- [ ] **Step 4: Replace `Musefs::open`**

```rust
    pub fn open(db: Db, config: MountConfig) -> Result<Musefs> {
        let tree = Self::build_tree(&db, &config)?;
        let last_data_version = db.data_version()?;
        Ok(Musefs {
            cache: Mutex::new(HeaderCache::new(config.mode)),
            last_data_version: AtomicI64::new(last_data_version),
            tree: ArcSwap::from_pointee(tree),
            db,
            config,
        })
    }
```

- [ ] **Step 5: Replace `refresh` and `poll_refresh`**

```rust
    /// Rebuild the tree from the current DB contents (used after external edits).
    pub fn refresh(&self) -> Result<()> {
        let tree = Self::build_tree(&self.db, &self.config)?;
        self.tree.store(Arc::new(tree));
        Ok(())
    }

    /// Cheap check for external DB commits via `PRAGMA data_version`. On a change,
    /// rebuild the tree and drop cached resolutions, then return `true`; the new
    /// version stamp is committed only after a successful rebuild.
    pub fn poll_refresh(&self) -> Result<bool> {
        let version = self.db.data_version()?;
        if version == self.last_data_version.load(Ordering::Acquire) {
            return Ok(false);
        }
        self.refresh()?;
        self.last_data_version.store(version, Ordering::Release);
        self.cache.lock().unwrap().clear();
        Ok(true)
    }
```

- [ ] **Step 6: Replace the tree-only read methods to use the loaded snapshot**

```rust
    pub fn lookup(&self, parent: u64, name: &str) -> Option<u64> {
        self.tree.load().lookup(parent, name)
    }

    /// The parent inode of `inode` (root's parent is itself). Forwards to the tree.
    pub fn parent(&self, inode: u64) -> Option<u64> {
        self.tree.load().parent(inode)
    }

    /// Directory entries as `(name, child_inode, is_dir)`.
    pub fn readdir(&self, inode: u64) -> Result<Vec<(String, u64, bool)>> {
        let tree = self.tree.load();
        let children = match tree.children(inode) {
            Some(children) => children,
            None if tree.node(inode).is_some() => return Err(CoreError::NotADir(inode)),
            None => return Err(CoreError::NoEntry(inode)),
        };
        Ok(children
            .iter()
            .map(|(name, &child)| (name.clone(), child, tree.is_dir(child)))
            .collect())
    }
```

- [ ] **Step 7: Replace `getattr` and `read` to `&self` + locked cache**

```rust
    pub fn getattr(&self, inode: u64) -> Result<Attr> {
        let track_id = {
            let tree = self.tree.load();
            match tree.node(inode) {
                None => return Err(CoreError::NoEntry(inode)),
                Some(node) => match &node.kind {
                    NodeKind::Dir => {
                        return Ok(Attr {
                            inode,
                            is_dir: true,
                            size: 0,
                            mtime_secs: 0,
                        })
                    }
                    NodeKind::File { track_id } => *track_id,
                },
            }
        };
        let resolved = self.cache.lock().unwrap().resolve(&self.db, track_id)?;
        Ok(Attr {
            inode,
            is_dir: false,
            size: resolved.total_len,
            mtime_secs: resolved.mtime_secs,
        })
    }

    pub fn read(&self, inode: u64, offset: u64, size: u64) -> Result<Vec<u8>> {
        let track_id = {
            let tree = self.tree.load();
            match tree.node(inode) {
                None => return Err(CoreError::NoEntry(inode)),
                Some(node) => match &node.kind {
                    NodeKind::Dir => return Err(CoreError::IsDir(inode)),
                    NodeKind::File { track_id } => *track_id,
                },
            }
        };
        // Resolve under the lock, then drop it so the backing-file read runs
        // without serializing other operations.
        let resolved = self.cache.lock().unwrap().resolve(&self.db, track_id)?;
        read_at(&resolved, &self.db, offset, size)
    }
```

Note: `build_tree`, `lookup`, `parent` keep their existing signatures (`build_tree` is an associated fn taking `&Db`). `resolve` already returns `Arc<ResolvedFile>`, so the lock is released at the end of the statement and `read_at` runs lock-free.

- [ ] **Step 8: Fix `musefs-fuse/src/lib.rs` callers**

The fuser methods already take `&mut self`; calling `&self` core methods is fine and needs no change there yet. But `poll_refresh()` now takes `&self` — the existing `let _ = self.core.poll_refresh();` lines still compile. Build to confirm.

- [ ] **Step 9: Fix the existing facade test bindings**

In `musefs-core/tests/facade.rs`, change `let mut fs = Musefs::open(...)` to `let fs = Musefs::open(...)` wherever the `mut` is now unused (the compiler will warn). Do the same in any other test that binds `Musefs` as `mut` only for `read`/`getattr`.

- [ ] **Step 10: Run the full workspace test suite**

Run: `cargo test`
Expected: PASS across all crates (no behavior change; only the mutability model moved).
Run: `cargo test -p musefs-core --features metrics --test metrics`
Expected: PASS (flip the binding to `let fs` per Task 0.3's note).
Run: `cargo clippy --all-targets`
Expected: no new warnings.

- [ ] **Step 11: Commit**

```bash
git add musefs-core/Cargo.toml musefs-core/src/facade.rs musefs-core/tests/facade.rs
git commit -m "refactor(core): Musefs interior mutability (ArcSwap tree, Mutex cache, atomic version)"
```

---

# Phase 1b — `DbPool`: per-worker read connections

WAL lets many read connections run concurrently without blocking. This task lets each worker thread get its own read-only connection; in-memory DBs (which can't be reopened by path) fall back to one shared connection behind a mutex.

## Task 1b.1: `Db` learns its path and a read-only constructor

**Files:**
- Modify: `musefs-db/src/lib.rs`

- [ ] **Step 1: Write a failing test (in `musefs-db`'s inline tests)**

Add to the `tests` module in `musefs-db/src/lib.rs`:

```rust
    #[test]
    fn open_readonly_can_read_a_file_db() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("m.db");
        {
            let w = Db::open(&path).unwrap();
            assert!(w.path().is_some());
        }
        let r = Db::open_readonly(&path).unwrap();
        // A read-only connection can run a read pragma without error.
        assert!(r.data_version().is_ok());
        assert_eq!(r.path().unwrap(), path.as_path());
    }

    #[test]
    fn in_memory_has_no_path() {
        let db = Db::open_in_memory().unwrap();
        assert!(db.path().is_none());
    }
```

- [ ] **Step 2: Run to confirm failure**

Run: `cargo test -p musefs-db open_readonly 2>&1 | head -20`
Expected: FAIL — `no method named path` / `no function open_readonly`.

- [ ] **Step 3: Add the `path` field**

Update the struct (`replace_symbol_body` on `Db`):

```rust
pub struct Db {
    conn: Connection,
    path: Option<std::path::PathBuf>,
}
```

- [ ] **Step 4: Update the existing constructors to set `path`**

In `Db::open`:

```rust
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Db> {
        let p = path.as_ref().to_path_buf();
        let mut conn = Connection::open(&p)?;
        Self::configure(&mut conn, true)?;
        Ok(Db { conn, path: Some(p) })
    }
```

In `Db::open_in_memory`:

```rust
    pub fn open_in_memory() -> Result<Db> {
        let mut conn = Connection::open_in_memory()?;
        Self::configure(&mut conn, false)?;
        Ok(Db { conn, path: None })
    }
```

- [ ] **Step 5: Add `path()` and `open_readonly()`**

Insert these methods into `impl Db` (e.g. after `data_version`):

```rust
    /// The backing file path, or `None` for an in-memory database.
    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// Open an additional read-only connection to an existing file-backed DB.
    /// WAL (set by the writer) lets these run concurrently without blocking.
    /// No migration is run — the schema already exists and the connection is RO.
    pub fn open_readonly<P: AsRef<Path>>(path: P) -> Result<Db> {
        let p = path.as_ref().to_path_buf();
        let conn = Connection::open_with_flags(
            &p,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )?;
        conn.busy_timeout(Duration::from_secs(5))?;
        Ok(Db { conn, path: Some(p) })
    }
```

- [ ] **Step 6: Run the tests**

Run: `cargo test -p musefs-db`
Expected: PASS (new tests + existing).

- [ ] **Step 7: Commit**

```bash
git add musefs-db/src/lib.rs
git commit -m "feat(db): Db::path() and Db::open_readonly() for per-worker read connections"
```

## Task 1b.2: `DbPool` and wiring `Musefs` to it

**Files:**
- Create: `musefs-core/src/db_pool.rs`
- Modify: `musefs-core/src/lib.rs`
- Modify: `musefs-core/src/facade.rs`

- [ ] **Step 1: Write a failing test for the pool**

Create `musefs-core/src/db_pool.rs` with the test first:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use musefs_db::Db;

    #[test]
    fn shared_pool_for_in_memory_db() {
        let pool = DbPool::new(Db::open_in_memory().unwrap()).unwrap();
        let v = pool.with(|db| db.data_version()).unwrap();
        // A second call works (shared connection re-locked).
        let v2 = pool.with(|db| db.data_version()).unwrap();
        assert_eq!(v, v2);
    }

    #[test]
    fn per_thread_pool_for_file_db() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("m.db");
        Db::open(&path).unwrap(); // create + migrate
        let pool = DbPool::new(Db::open(&path).unwrap()).unwrap();
        // Used from a different thread: that thread opens its own read connection.
        let r = std::thread::scope(|s| {
            s.spawn(|| pool.with(|db| db.data_version()).unwrap()).join().unwrap()
        });
        assert!(r >= 0);
    }
}
```

- [ ] **Step 2: Run to confirm failure**

Run: `cargo test -p musefs-core db_pool:: 2>&1 | head -20`
Expected: FAIL — `DbPool` not found.

- [ ] **Step 3: Implement `DbPool` above the test**

Prepend to `musefs-core/src/db_pool.rs`:

```rust
//! Hands a read connection to whichever thread needs one.
//!
//! - File-backed DB → each thread lazily opens its own read-only connection
//!   (WAL makes concurrent readers contention-free; the worker pool is bounded,
//!   so the connection count is bounded).
//! - In-memory DB (tests) cannot be reopened by path, so a single connection is
//!   shared behind a mutex.

use std::cell::RefCell;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use musefs_db::Db;

use crate::error::Result;

pub enum DbPool {
    PerThread { path: PathBuf },
    Shared(Arc<Mutex<Db>>),
}

thread_local! {
    static LOCAL: RefCell<Option<Db>> = const { RefCell::new(None) };
}

impl DbPool {
    /// Build a pool from the DB used to construct the mount. File-backed DBs
    /// become per-thread pools (the passed connection is dropped — workers open
    /// their own); in-memory DBs are wrapped in a shared mutex.
    pub fn new(db: Db) -> Result<DbPool> {
        match db.path() {
            Some(p) => Ok(DbPool::PerThread { path: p.to_path_buf() }),
            None => Ok(DbPool::Shared(Arc::new(Mutex::new(db)))),
        }
    }

    /// Run `f` with a read connection.
    pub fn with<R>(&self, f: impl FnOnce(&Db) -> Result<R>) -> Result<R> {
        match self {
            DbPool::PerThread { path } => LOCAL.with(|cell| {
                {
                    let mut slot = cell.borrow_mut();
                    if slot.is_none() {
                        *slot = Some(Db::open_readonly(path)?);
                    }
                }
                let slot = cell.borrow();
                f(slot.as_ref().unwrap())
            }),
            DbPool::Shared(m) => {
                let db = m.lock().unwrap();
                f(&db)
            }
        }
    }
}
```

- [ ] **Step 4: Register the module**

In `musefs-core/src/lib.rs` add `mod db_pool;` and re-export the type:

```rust
mod db_pool;
pub use db_pool::DbPool;
```

- [ ] **Step 5: Run the pool tests**

Run: `cargo test -p musefs-core db_pool::`
Expected: PASS (2 tests).

- [ ] **Step 6: Swap `Musefs`'s `db` field for the pool**

In `musefs-core/src/facade.rs`:

Add import:

```rust
use crate::db_pool::DbPool;
```

Replace the `Musefs` struct field `db: Db` with `pool: DbPool`:

```rust
pub struct Musefs {
    pool: DbPool,
    config: MountConfig,
    tree: ArcSwap<VirtualTree>,
    cache: Mutex<HeaderCache>,
    last_data_version: AtomicI64,
}
```

Update `open` to build the tree first, then move the DB into the pool:

```rust
    pub fn open(db: Db, config: MountConfig) -> Result<Musefs> {
        let tree = Self::build_tree(&db, &config)?;
        let last_data_version = db.data_version()?;
        Ok(Musefs {
            cache: Mutex::new(HeaderCache::new(config.mode)),
            last_data_version: AtomicI64::new(last_data_version),
            tree: ArcSwap::from_pointee(tree),
            pool: DbPool::new(db)?,
            config,
        })
    }
```

Update the DB-touching methods to route through the pool:

```rust
    pub fn refresh(&self) -> Result<()> {
        let tree = self.pool.with(|db| Self::build_tree(db, &self.config))?;
        self.tree.store(Arc::new(tree));
        Ok(())
    }

    pub fn poll_refresh(&self) -> Result<bool> {
        let version = self.pool.with(|db| db.data_version())?;
        if version == self.last_data_version.load(Ordering::Acquire) {
            return Ok(false);
        }
        self.refresh()?;
        self.last_data_version.store(version, Ordering::Release);
        self.cache.lock().unwrap().clear();
        Ok(true)
    }
```

In `getattr`, replace the resolve line:

```rust
        let resolved = self
            .pool
            .with(|db| self.cache.lock().unwrap().resolve(db, track_id))?;
```

In `read`, replace the resolve + read with a single pooled block so `read_at` uses the same connection:

```rust
        self.pool.with(|db| {
            let resolved = self.cache.lock().unwrap().resolve(db, track_id)?;
            read_at(&resolved, db, offset, size)
        })
    }
```

- [ ] **Step 7: Run the full suite**

Run: `cargo test`
Expected: PASS.
Run: `cargo test -p musefs-fuse -- --ignored`
Expected: PASS (real-mount e2e, byte-identical audio).
Run: `cargo clippy --all-targets`
Expected: clean.

- [ ] **Step 8: Commit**

```bash
git add musefs-core/src/db_pool.rs musefs-core/src/lib.rs musefs-core/src/facade.rs
git commit -m "feat(core): DbPool gives each worker its own read connection; Musefs routes DB access through it"
```

---

# Phase 1c — Worker-pool offload in the FUSE adapter

Now the payoff: keep fuser's dispatch thread non-blocking by moving every op that touches disk/DB onto a bounded worker pool, replying to the kernel from the worker. Pure tree ops (`readdir`) stay inline.

## Task 1c.1: Offload blocking FUSE ops to a worker pool

**Files:**
- Modify: `musefs-fuse/Cargo.toml` (add `threadpool`)
- Modify: `musefs-fuse/src/lib.rs`

- [ ] **Step 1: Add the dependency**

In `musefs-fuse/Cargo.toml` `[dependencies]`:

```toml
threadpool = "1"
```

- [ ] **Step 2: Update imports and the `MusefsFs` struct**

At the top of `musefs-fuse/src/lib.rs` add:

```rust
use std::sync::Arc;
use threadpool::ThreadPool;
```

Replace the `MusefsFs` struct (`replace_symbol_body`):

```rust
/// A `fuser::Filesystem` that serves a `musefs_core::Musefs`. fuser dispatches
/// on one thread; blocking ops (read/getattr/lookup-attr) are offloaded to a
/// bounded worker pool and answered via the `Send` reply objects, so a slow
/// backing read never stalls the dispatch thread or unrelated metadata ops.
pub struct MusefsFs {
    core: Arc<Musefs>,
    pool: ThreadPool,
    uid: u32,
    gid: u32,
    mount_time: SystemTime,
}
```

- [ ] **Step 3: Update `MusefsFs::new` to build the pool**

```rust
impl MusefsFs {
    pub fn new(core: Musefs) -> MusefsFs {
        // I/O-bound work (especially NFS), so oversize the pool relative to CPUs.
        let workers = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4)
            * 2;
        MusefsFs {
            core: Arc::new(core),
            pool: ThreadPool::new(workers),
            // SAFETY: getuid/getgid are always-successful libc calls.
            uid: unsafe { libc::getuid() },
            gid: unsafe { libc::getgid() },
            mount_time: SystemTime::now(),
        }
    }
}
```

- [ ] **Step 4: Rewrite the `Filesystem` impl to offload blocking ops**

Replace the bodies of `lookup`, `getattr`, and `read` (leave `readdir` inline — it only reads the in-memory tree). `replace_symbol_body` on each:

```rust
    fn lookup(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEntry) {
        let _ = self.core.poll_refresh();
        let name = match name.to_str() {
            Some(n) => n,
            None => return reply.error(libc::ENOENT),
        };
        // Inode resolution is an in-memory tree read; the attr (which may touch
        // the DB/disk) is computed on the worker pool.
        let child = match self.core.lookup(parent, name) {
            Some(ino) => ino,
            None => return reply.error(libc::ENOENT),
        };
        let core = Arc::clone(&self.core);
        let (uid, gid, mt) = (self.uid, self.gid, self.mount_time);
        self.pool.execute(move || match core.getattr(child) {
            Ok(attr) => reply.entry(&TTL, &to_file_attr(&attr, uid, gid, mt), 0),
            Err(e) => reply.error(errno(&e)),
        });
    }

    fn getattr(&mut self, _req: &Request<'_>, ino: u64, reply: ReplyAttr) {
        let _ = self.core.poll_refresh();
        let core = Arc::clone(&self.core);
        let (uid, gid, mt) = (self.uid, self.gid, self.mount_time);
        self.pool.execute(move || match core.getattr(ino) {
            Ok(attr) => reply.attr(&TTL, &to_file_attr(&attr, uid, gid, mt)),
            Err(e) => reply.error(errno(&e)),
        });
    }

    fn read(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        _fh: u64,
        offset: i64,
        size: u32,
        _flags: i32,
        _lock_owner: Option<u64>,
        reply: ReplyData,
    ) {
        if offset < 0 {
            return reply.error(libc::EINVAL);
        }
        let core = Arc::clone(&self.core);
        self.pool
            .execute(move || match core.read(ino, offset as u64, size as u64) {
                Ok(bytes) => reply.data(&bytes),
                Err(e) => reply.error(errno(&e)),
            });
    }
```

Leave `readdir` exactly as it is (inline, tree-only).

- [ ] **Step 5: Update the module doc comment**

The file header (lines 1-3) says "Mounted single-threaded ... matching the `&mut self` read path." Replace with:

```rust
//! FUSE filesystem binding for musefs: translates VFS calls into `musefs-core`
//! operations. fuser dispatches on a single thread; blocking operations are
//! offloaded onto a bounded worker pool and answered via the `Send` reply
//! objects, so a slow backing read cannot stall metadata operations.
```

- [ ] **Step 6: Build and run all existing tests**

Run: `cargo build -p musefs-fuse`
Expected: compiles. If the compiler rejects moving a `Reply*` into the closure (a `Send` bound error), that is the signal to confirm the fuser version's reply types — fuser 0.14's replies are `Send`; check the dependency resolved correctly.
Run: `cargo test -p musefs-fuse`
Expected: PASS (unit tests).
Run: `cargo test -p musefs-fuse -- --ignored`
Expected: PASS — the real-mount e2e read-through tests still serve byte-identical audio, now through the worker pool.
Run: `cargo test`
Expected: PASS workspace-wide.

- [ ] **Step 7: Commit**

```bash
git add musefs-fuse/Cargo.toml musefs-fuse/src/lib.rs
git commit -m "feat(fuse): offload blocking ops to a worker pool; keep readdir inline"
```

## Task 1c.2: Concurrency test — a slow read does not block a metadata op

**Files:**
- Create: `musefs-fuse/tests/concurrency.rs`

This proves the core goal: with a deliberately slow read in flight, an unrelated metadata op (`stat`) on the mount still returns promptly. It uses the Phase 0 fault-injection env var, so it requires building `musefs-core` with the `metrics` feature.

- [ ] **Step 1: Write the test**

Create `musefs-fuse/tests/concurrency.rs`. Model the mount setup on the existing `musefs-fuse/tests/mount.rs` / `ogg_read_through.rs` (`musefs_fuse::spawn`, a tempdir mountpoint, a scanned DB). Replace `<setup>` with that established helper pattern from those test files:

```rust
#![cfg(feature = "metrics")]
//! Requires: cargo test -p musefs-fuse --features musefs-core/metrics -- --ignored

use std::time::{Duration, Instant};

#[test]
#[ignore = "real mount; needs /dev/fuse"]
fn slow_read_does_not_block_stat() {
    // Inject 50ms of latency per backing pread so a streaming read is clearly slow.
    std::env::set_var("MUSEFS_FAULT_PREAD_US", "50000");

    // <setup>: build a scanned DB with at least two tracks, mount via
    // musefs_fuse::spawn at `mnt`, and discover two file paths `big` and `other`
    // under the mountpoint (mirror tests/mount.rs).
    let (mnt, big, other, _session) = setup_two_track_mount();

    // Start a large sequential read of `big` on a background thread.
    let reader = std::thread::spawn(move || {
        let _ = std::fs::read(&big); // slow due to injected pread latency
    });

    // Give the reader a moment to be mid-read.
    std::thread::sleep(Duration::from_millis(20));

    // A metadata op on the *other* file must return promptly despite the slow read.
    let t = Instant::now();
    let md = std::fs::metadata(&other).unwrap();
    let elapsed = t.elapsed();
    assert!(md.len() > 0);
    assert!(
        elapsed < Duration::from_millis(40),
        "stat blocked for {elapsed:?} behind a slow read"
    );

    reader.join().unwrap();
}
```

Implement `setup_two_track_mount()` in the test file using the same primitives as `musefs-fuse/tests/mount.rs` (create two FLACs in a tempdir, `scan_directory` into an **on-disk** DB so the pool is `PerThread` and reads truly run in parallel, `musefs_fuse::spawn(...)`, return the mountpoint + two file paths + the `BackgroundSession` guard). Use an on-disk DB path (`Db::open(dir.join("m.db"))`) — an in-memory DB would use the `Shared` mutex pool and serialize DB access (though not the backing pread, which is where the latency is injected).

- [ ] **Step 2: Run the test**

Run: `cargo test -p musefs-fuse --features musefs-core/metrics --test concurrency -- --ignored --nocapture`
Expected: PASS — `stat` returns in well under 40ms while a 50ms-per-chunk read is in flight. (Before Phase 1c this would block, because the read held the single dispatch thread.)

- [ ] **Step 3: Commit**

```bash
git add musefs-fuse/tests/concurrency.rs
git commit -m "test(fuse): slow read does not block a concurrent metadata op"
```

---

## Self-Review notes (addressed)

- **Spec coverage:** This plan covers spec Phase 0 (harness: counters, fault injection, bench, baseline test) and Phase 1 (concurrency foundation: interior mutability, `ArcSwap` tree, mutex cache, atomic version, `DbPool` with per-worker read connections, worker-pool offload). Spec Phases 2–6 (handle lifecycle, two-tier caching, incremental refresh/inode stability, kernel tuning, M4A bounded read) are intentionally **out of scope** for this plan and will be planned after Phase 1 lands, per the agreed sequencing.
- **Known foundation limitations (resolved by later phases, not bugs here):** the cache `Mutex` still serializes the *resolve* step across concurrent opens (Phase 3 shards it + adds the size cache); `poll_refresh` still runs the tree rebuild on the calling thread and clears the cache wholesale (Phase 4 debounces, single-flights, moves the rebuild off-thread, and switches to lazy invalidation); the per-read `open()`/`stat()` still happen (Phase 2 adds the `open`/`release` handle lifecycle). These are called out so the executor does not "fix" them ahead of their phase.
- **Type consistency:** `DbPool::new`/`with`, `Db::path`/`open_readonly`, `metrics::{on_open,on_stat,on_pread,on_art_chunk,snapshot,reset,Snapshot}`, and the `&self` signatures of `Musefs::{lookup,parent,readdir,getattr,read,refresh,poll_refresh}` are used consistently across tasks.
- **Binding caveat:** Tasks 0.3/0.4 are written for the post-1a `&self` API; if executed before Task 1a.1, flip `let fs` → `let mut fs`. Both compile.
