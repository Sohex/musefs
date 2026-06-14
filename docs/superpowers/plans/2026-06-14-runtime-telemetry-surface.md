# Runtime Telemetry Surface (`.musefs-metrics`) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Expose handle/queue/cache/tree/allocator telemetry on a *live* musefs mount via a flag-gated, `/proc`-style `.musefs-metrics/metrics` file in Prometheus exposition format, without rebuilding with a cargo feature.

**Architecture:** A new `musefs-core::telemetry` module owns the data types and **all** Prometheus rendering (`render_prometheus`). `Musefs::telemetry()` returns the core-owned half (handle count, cache, tree, refresh state). `musefs-fuse` owns a synthetic-inode namespace (mirroring the existing Spotlight marker at `platform/spotlight.rs`) that intercepts FUSE dispatch for two reserved inodes (`u64::MAX-1` dir, `u64::MAX-2` file), gathers the core half + its own counters + an optional allocator/syscall probe, and serves the rendered bytes. The DB-derived virtual tree and the `RegionLayout`/segment audio path are untouched.

**Tech Stack:** Rust, `fuser` 0.17 (FUSE), `quick_cache` 0.6.23, `threadpool` 1.8, `sharded-slab`, `tikv-jemalloc-ctl` 0.7, `clap` 4.

**Spec:** `docs/superpowers/specs/2026-06-14-runtime-telemetry-surface-design.md`

### Deviation from spec (jemalloc probe)

The spec said the binary injects an `Arc<dyn Fn>` allocator probe as a constructor argument. This plan instead gives **`musefs-fuse` a `jemalloc` cargo feature** (mirroring its existing `metrics = ["musefs-core/metrics"]` forwarding pattern): when built with the feature, `musefs-fuse` reads `tikv_jemalloc_ctl` directly. The binary's existing `jemalloc` feature gains `musefs-fuse/jemalloc`. This is strictly cleaner — no closure threaded through `musefs-cli`'s `run`/`run_mount`/`mount_with` public API, and `FuseConfig` stays a `Debug + Clone` POD. The renderer in core is unchanged (still takes `Option<&AllocatorStats>`). **Flag this to the user at execution handoff.**

### Mutant-anchor note (read before touching `musefs-core`)

`.cargo/mutants.toml` pins some mutants by `file:line:col` in `tree.rs` and `metrics.rs`. Adding lines to `tree.rs` (Task 2) shifts those anchors and the pre-commit hook's `scripts/check_mutant_anchors.py` will reject the commit. `facade.rs` and `reader.rs` carry **no** anchors, so edits there are safe. For `tree.rs`, append new methods at the **end of each impl block** and, in the **same commit**, run `python3 scripts/check_mutant_anchors.py` — if it fails, run `python3 scripts/check_mutant_anchors.py --fix`, then `git add .cargo/mutants.toml`. Each Task that touches `tree.rs` includes this step explicitly.

### Everyday commands

```bash
cargo test -p musefs-core telemetry        # core telemetry tests
cargo test -p musefs-fuse                  # fuse tests (excludes #[ignore] e2e)
cargo test -p musefs-fuse --features metrics   # syscall-counter section
cargo test -p musefs-cli                   # CLI flag test
cargo clippy --all-targets --workspace -- -D warnings
cargo fmt --all
```

---

## Task 1: Core telemetry types + Prometheus renderer

Pure data + string formatting. Fully unit-testable in `musefs-core` with no FUSE/DB.

**Files:**
- Create: `musefs-core/src/telemetry.rs`
- Modify: `musefs-core/src/lib.rs` (add `mod telemetry;` + `pub use`)

- [ ] **Step 1: Create the module with types and a failing test.**

Create `musefs-core/src/telemetry.rs`:

```rust
//! Runtime telemetry surface: plain-data snapshot types and Prometheus
//! exposition-format rendering for the `.musefs-metrics/metrics` virtual file
//! (#394). All rendering lives here (most of the data is core-owned and this is
//! unit-testable without a mount); `musefs-fuse` gathers the fuse-side half and
//! the optional allocator/syscall probes, then calls [`render_prometheus`].

use std::fmt::Write;

/// Core-owned telemetry: the file-handle slab count, header/size caches, the
/// virtual-tree footprint, and refresh health. Produced by `Musefs::telemetry`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CoreTelemetry {
    pub handles_open: u64,
    pub cache_header_entries: u64,
    pub cache_header_bytes: u64,
    pub cache_header_bytes_max: u64,
    pub cache_header_hits: u64,
    pub cache_header_misses: u64,
    pub cache_size_entries: u64,
    pub tree_nodes: u64,
    pub inode_paths: u64,
    pub refresh_generation: u64,
    pub refresh_gap_fallbacks: u64,
    pub refresh_needs_rebuild: bool,
}

/// Passthrough sub-telemetry; `None` (in [`FuseTelemetry`]) off Linux.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PassthroughTelemetry {
    pub disabled: bool,
    pub active: u64,
}

/// Fuse-owned telemetry: uptime, the read/dir-handle gates and their caps, the
/// worker pool, and (Linux only) passthrough state.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FuseTelemetry {
    pub uptime_seconds: u64,
    pub reads_inflight: u64,
    pub reads_inflight_max: u64,
    pub dir_handles: u64,
    pub dir_handles_max: u64,
    pub pool_workers: u64,
    pub pool_active: u64,
    pub pool_queued: u64,
    pub passthrough: Option<PassthroughTelemetry>,
}

/// jemalloc allocator stats (present only on a `jemalloc`-feature build).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AllocatorStats {
    pub allocated: u64,
    pub resident: u64,
    pub active: u64,
    pub retained: u64,
}

fn gauge(out: &mut String, name: &str, help: &str, val: u64) {
    let _ = write!(out, "# HELP {name} {help}\n# TYPE {name} gauge\n{name} {val}\n");
}

fn counter(out: &mut String, name: &str, help: &str, val: u64) {
    let _ = write!(out, "# HELP {name} {help}\n# TYPE {name} counter\n{name} {val}\n");
}

/// Render a full Prometheus exposition-format document. Feature-gated blocks
/// (`alloc`, `syscalls`) are omitted entirely when their `Option` is `None`.
pub fn render_prometheus(
    core: &CoreTelemetry,
    fuse: &FuseTelemetry,
    alloc: Option<&AllocatorStats>,
    syscalls: Option<&crate::metrics::Snapshot>,
) -> String {
    let mut out = String::with_capacity(4096);

    gauge(&mut out, "musefs_uptime_seconds", "Seconds since the mount started.", fuse.uptime_seconds);
    gauge(&mut out, "musefs_handles_open", "Open file handles in the core slab.", core.handles_open);

    gauge(&mut out, "musefs_reads_inflight", "Foreground reads queued/in-flight.", fuse.reads_inflight);
    gauge(&mut out, "musefs_reads_inflight_max", "Cap before reads are rejected with EAGAIN.", fuse.reads_inflight_max);
    gauge(&mut out, "musefs_dir_handles", "Open directory-listing snapshots.", fuse.dir_handles);
    gauge(&mut out, "musefs_dir_handles_max", "Cap before opendir is rejected with ENFILE.", fuse.dir_handles_max);

    gauge(&mut out, "musefs_pool_workers", "Worker-pool size.", fuse.pool_workers);
    gauge(&mut out, "musefs_pool_active", "Workers currently running a job.", fuse.pool_active);
    gauge(&mut out, "musefs_pool_queued", "Jobs waiting in the worker-pool queue.", fuse.pool_queued);

    gauge(&mut out, "musefs_cache_header_entries", "Resolved-file entries in the header cache.", core.cache_header_entries);
    gauge(&mut out, "musefs_cache_header_bytes", "Resident inline bytes in the header cache.", core.cache_header_bytes);
    gauge(&mut out, "musefs_cache_header_bytes_max", "Header-cache byte budget.", core.cache_header_bytes_max);
    counter(&mut out, "musefs_cache_header_hits_total", "Raw header-cache key hits; a hit may still trigger a content-version rebuild.", core.cache_header_hits);
    counter(&mut out, "musefs_cache_header_misses_total", "Raw header-cache key misses.", core.cache_header_misses);
    gauge(&mut out, "musefs_cache_size_entries", "Entries in the getattr size cache.", core.cache_size_entries);

    gauge(&mut out, "musefs_tree_nodes", "Live virtual-tree inodes.", core.tree_nodes);
    gauge(&mut out, "musefs_inode_paths", "Interned paths in the inode allocator.", core.inode_paths);

    gauge(&mut out, "musefs_refresh_generation", "Refresh generation (bumped on each non-empty refresh).", core.refresh_generation);
    counter(&mut out, "musefs_refresh_gap_fallbacks_total", "Polls that took the changelog-gap full-rebuild path.", core.refresh_gap_fallbacks);
    gauge(&mut out, "musefs_refresh_needs_rebuild", "1 if a poisoned-lock recovery left a full rebuild pending.", u64::from(core.refresh_needs_rebuild));

    if let Some(pt) = fuse.passthrough {
        gauge(&mut out, "musefs_passthrough_disabled", "1 if kernel passthrough is sticky-disabled.", u64::from(pt.disabled));
        gauge(&mut out, "musefs_passthrough_active", "Live kernel-passthrough backing registrations.", pt.active);
    }

    if let Some(a) = alloc {
        gauge(&mut out, "musefs_alloc_allocated_bytes", "jemalloc bytes allocated and in use.", a.allocated);
        gauge(&mut out, "musefs_alloc_resident_bytes", "jemalloc resident bytes (RSS proxy).", a.resident);
        gauge(&mut out, "musefs_alloc_active_bytes", "jemalloc bytes in active pages.", a.active);
        gauge(&mut out, "musefs_alloc_retained_bytes", "jemalloc retained (lazily-purgeable) bytes.", a.retained);
    }

    if let Some(s) = syscalls {
        counter(&mut out, "musefs_backing_opens_total", "Serve-path backing-file opens.", s.opens);
        counter(&mut out, "musefs_backing_stats_total", "Serve-path metadata syscalls.", s.stats);
        counter(&mut out, "musefs_backing_preads_total", "Serve-path positioned backing reads.", s.preads);
        counter(&mut out, "musefs_backing_pread_bytes_total", "Serve-path backing bytes attempted.", s.pread_bytes);
        counter(&mut out, "musefs_art_chunks_total", "Art-blob chunks streamed from the DB.", s.art_chunks);
        counter(&mut out, "musefs_binary_tag_chunks_total", "Binary-tag chunks streamed from the DB.", s.binary_tag_chunks);
        counter(&mut out, "musefs_scan_opens_total", "Scan-path backing-file opens.", s.scan_opens);
        counter(&mut out, "musefs_scan_preads_total", "Scan-path positioned reads.", s.scan_preads);
        counter(&mut out, "musefs_scan_bytes_total", "Scan-path bytes read.", s.scan_bytes_read);
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_core() -> CoreTelemetry {
        CoreTelemetry {
            handles_open: 3,
            cache_header_entries: 7,
            cache_header_bytes: 4096,
            cache_header_bytes_max: 64 * 1024 * 1024,
            cache_header_hits: 100,
            cache_header_misses: 5,
            cache_size_entries: 9,
            tree_nodes: 42,
            inode_paths: 50,
            refresh_generation: 2,
            refresh_gap_fallbacks: 1,
            refresh_needs_rebuild: false,
        }
    }

    fn sample_fuse() -> FuseTelemetry {
        FuseTelemetry {
            uptime_seconds: 60,
            reads_inflight: 1,
            reads_inflight_max: 1024,
            dir_handles: 2,
            dir_handles_max: 1024,
            pool_workers: 8,
            pool_active: 1,
            pool_queued: 0,
            passthrough: Some(PassthroughTelemetry { disabled: false, active: 4 }),
        }
    }

    #[test]
    fn renders_core_and_fuse_gauges() {
        let out = render_prometheus(&sample_core(), &sample_fuse(), None, None);
        assert!(out.contains("# TYPE musefs_handles_open gauge\nmusefs_handles_open 3\n"));
        assert!(out.contains("musefs_reads_inflight 1\n"));
        assert!(out.contains("musefs_reads_inflight_max 1024\n"));
        assert!(out.contains("musefs_pool_queued 0\n"));
        assert!(out.contains("musefs_tree_nodes 42\n"));
        // counter type for hit/miss
        assert!(out.contains("# TYPE musefs_cache_header_hits_total counter\nmusefs_cache_header_hits_total 100\n"));
    }

    #[test]
    fn passthrough_block_present_when_some_absent_when_none() {
        let with = render_prometheus(&sample_core(), &sample_fuse(), None, None);
        assert!(with.contains("musefs_passthrough_active 4\n"));
        assert!(with.contains("musefs_passthrough_disabled 0\n"));

        let mut f = sample_fuse();
        f.passthrough = None;
        let without = render_prometheus(&sample_core(), &f, None, None);
        assert!(!without.contains("musefs_passthrough"));
    }

    #[test]
    fn alloc_and_syscall_blocks_are_omitted_when_none() {
        let out = render_prometheus(&sample_core(), &sample_fuse(), None, None);
        assert!(!out.contains("musefs_alloc_"));
        assert!(!out.contains("musefs_backing_"));
    }

    #[test]
    fn alloc_block_present_when_some() {
        let a = AllocatorStats { allocated: 1, resident: 2, active: 3, retained: 4 };
        let out = render_prometheus(&sample_core(), &sample_fuse(), Some(&a), None);
        assert!(out.contains("musefs_alloc_resident_bytes 2\n"));
        assert!(out.contains("musefs_alloc_retained_bytes 4\n"));
    }

    #[test]
    fn syscall_block_present_when_some() {
        let s = crate::metrics::Snapshot { opens: 11, preads: 22, ..crate::metrics::Snapshot::default() };
        let out = render_prometheus(&sample_core(), &sample_fuse(), None, Some(&s));
        assert!(out.contains("# TYPE musefs_backing_opens_total counter\nmusefs_backing_opens_total 11\n"));
        assert!(out.contains("musefs_backing_preads_total 22\n"));
    }
}
```

- [ ] **Step 2: Register the module.** Edit `musefs-core/src/lib.rs`: add `mod telemetry;` in the module list (alphabetically, after `mod template;` / before `mod tree;`) and append to the `pub use` block:

```rust
pub use telemetry::{
    AllocatorStats, CoreTelemetry, FuseTelemetry, PassthroughTelemetry, render_prometheus,
};
```

- [ ] **Step 3: Run the tests — expect FAIL first, then PASS.**

Run: `cargo test -p musefs-core telemetry`
Expected: the 5 tests compile and PASS. (If `Snapshot` field access fails, confirm `crate::metrics::Snapshot` is `pub` — it is, in both cfg arms.)

- [ ] **Step 4: Lint.**

Run: `cargo clippy -p musefs-core --all-targets -- -D warnings`
Expected: clean.

- [ ] **Step 5: Commit.**

```bash
git add musefs-core/src/telemetry.rs musefs-core/src/lib.rs
git commit -m "feat(core): telemetry types + Prometheus renderer (#394)"
```

---

## Task 2: Core accessors + `handles_open` counter + `Musefs::telemetry()`

Add the always-on handle counter, the cache/tree/inode accessors the renderer needs, and `Musefs::telemetry()` that assembles a `CoreTelemetry`.

**Files:**
- Modify: `musefs-core/src/reader.rs` (HeaderCache accessors — **no mutant anchors here**)
- Modify: `musefs-core/src/tree.rs` (VirtualTree + InodeAllocator accessors — **has anchors, see Step 6**)
- Modify: `musefs-core/src/facade.rs` (counter field + bumps + `telemetry()` — **no anchors here**)

- [ ] **Step 1: Add HeaderCache accessors with a failing test.** In `musefs-core/src/reader.rs`, append these methods inside `impl HeaderCache` (after `remove`, before `resolve`, or at the end of the impl):

```rust
    /// Current number of cached resolved-file entries.
    pub fn entry_count(&self) -> u64 {
        self.cache.len() as u64
    }
    /// Current resident inline-byte weight.
    pub fn weight_bytes(&self) -> u64 {
        self.cache.weight()
    }
    /// Configured resident-byte budget.
    pub fn budget_bytes(&self) -> u64 {
        self.cache.capacity()
    }
    /// Raw key-hit count (NOT content-version-validated hits — see telemetry docs).
    pub fn raw_hits(&self) -> u64 {
        self.cache.hits()
    }
    /// Raw key-miss count.
    pub fn raw_misses(&self) -> u64 {
        self.cache.misses()
    }
```

Add this test to the existing `#[cfg(test)] mod tests` in `reader.rs` (or create one if absent):

```rust
    #[test]
    fn header_cache_exposes_budget_and_starts_empty() {
        let c = HeaderCache::with_budget(Mode::Synthesis, 1234);
        assert_eq!(c.entry_count(), 0);
        assert_eq!(c.weight_bytes(), 0);
        assert_eq!(c.budget_bytes(), 1234);
    }
```

Run: `cargo test -p musefs-core header_cache_exposes_budget`
Expected: PASS. (If `quick_cache::sync::Cache` lacks `hits()`/`misses()`/`weight()`/`capacity()`, STOP — re-verify the 0.6.23 API; these were confirmed present during spec review.)

- [ ] **Step 2: Add VirtualTree + InodeAllocator accessors.** In `musefs-core/src/tree.rs`, append at the **end of the `impl VirtualTree` block**:

```rust
    /// Number of live inodes (telemetry: virtual-tree footprint, #394).
    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }
```

And append at the **end of the `impl InodeAllocator` block**:

```rust
    /// Number of interned paths (telemetry: inode-allocator footprint, #394).
    pub fn interned_path_count(&self) -> usize {
        self.paths.len()
    }
```

- [ ] **Step 3: Add the `handles_open` counter field to `Musefs`.** In `musefs-core/src/facade.rs`, add a field to the `struct Musefs` (e.g. immediately after `handles: sharded_slab::Slab<Arc<Handle>>,`):

```rust
    /// Live count of entries in `handles` (telemetry: `sharded_slab` has no O(1)
    /// `len()`). Incremented only on a successful slab insert, decremented only on
    /// a successful remove, so it tracks slab occupancy exactly (#394).
    handles_open: std::sync::atomic::AtomicUsize,
```

In `Musefs::open`, add to the struct initializer (after `handles: sharded_slab::Slab::new(),`):

```rust
            handles_open: std::sync::atomic::AtomicUsize::new(0),
```

- [ ] **Step 4: Bump the counter on insert/remove.** In `open_handle`, replace the final `fh_from_key(...)` expression:

```rust
        let key = self.handles.insert(Arc::new(Handle {
            track_id,
            resolved: arc_swap::ArcSwap::from(resolved),
            generation: AtomicU64::new(generation),
            file,
        }));
        if key.is_some() {
            self.handles_open.fetch_add(1, Ordering::Relaxed);
        }
        fh_from_key(key)
```

In `release_handle`, replace the body:

```rust
    pub fn release_handle(&self, fh: Fh) {
        if self.handles.remove(fh.slab_key()) {
            self.handles_open.fetch_sub(1, Ordering::Relaxed);
        }
    }
```

(`Ordering` is already imported in `facade.rs`; `sharded_slab::Slab::remove` returns `bool`.)

- [ ] **Step 5: Add `Musefs::telemetry()`.** Append inside `impl Musefs` (e.g. after `mode()`):

```rust
    /// Snapshot the core-owned telemetry for the `.musefs-metrics` surface (#394).
    /// Cheap: atomic loads plus three length reads (the `inodes` mutex is taken
    /// briefly; poison is recovered like every other lock site).
    pub fn telemetry(&self) -> crate::telemetry::CoreTelemetry {
        let tree_nodes = self.tree.load().node_count() as u64;
        let inode_paths = self
            .inodes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .interned_path_count() as u64;
        crate::telemetry::CoreTelemetry {
            handles_open: self.handles_open.load(Ordering::Relaxed) as u64,
            cache_header_entries: self.cache.entry_count(),
            cache_header_bytes: self.cache.weight_bytes(),
            cache_header_bytes_max: self.cache.budget_bytes(),
            cache_header_hits: self.cache.raw_hits(),
            cache_header_misses: self.cache.raw_misses(),
            cache_size_entries: self.size_cache.len() as u64,
            tree_nodes,
            inode_paths,
            refresh_generation: self.refresh_gen.load(Ordering::Acquire),
            refresh_gap_fallbacks: self.gap_fallbacks.load(Ordering::Relaxed),
            refresh_needs_rebuild: self.needs_rebuild.load(Ordering::Relaxed),
        }
    }
```

Add a test in the `facade.rs` test module:

```rust
    #[test]
    fn telemetry_counts_open_handles() {
        let (fs, file_inode) = telemetry_test_fixture(); // see note below
        let base = fs.telemetry().handles_open;
        let fh = fs.open_handle(file_inode).unwrap();
        assert_eq!(fs.telemetry().handles_open, base + 1);
        fs.release_handle(fh);
        assert_eq!(fs.telemetry().handles_open, base);
    }
```

Note: reuse whatever existing helper builds a `Musefs` + a file inode in `facade.rs` tests (e.g. the fixture used by `getattr_size_cache_hit_detects_backing_change` near `facade.rs:1942`). Name the test's fixture call to match the existing helper; do **not** introduce a new fixture if one exists.

- [ ] **Step 6: Re-anchor mutants (tree.rs was edited).**

Run: `python3 scripts/check_mutant_anchors.py`
- If it PASSES: nothing to do.
- If it FAILS: run `python3 scripts/check_mutant_anchors.py --fix`, then re-run the checker to confirm PASS. If `--fix` cannot re-anchor (it only handles covering-set clusters), manually update the failing entry's `:line:col` in `.cargo/mutants.toml` using that entry's `# guard:` tag to locate the new position. Stage `.cargo/mutants.toml`.

- [ ] **Step 7: Test + lint.**

Run: `cargo test -p musefs-core` then `cargo clippy -p musefs-core --all-targets -- -D warnings`
Expected: PASS / clean.

- [ ] **Step 8: Commit (include mutants.toml if it changed).**

```bash
git add musefs-core/src/reader.rs musefs-core/src/tree.rs musefs-core/src/facade.rs
git add .cargo/mutants.toml 2>/dev/null || true
git commit -m "feat(core): handles_open counter + telemetry() accessor (#394)"
```

---

## Task 3: Fuse synthetic-inode module (`metrics_dir`)

Mirrors `platform/spotlight.rs`: reserved inodes, `lookup`, attr builders, dir entries — but all-platform and for a dir-containing-a-file. Pure functions, unit-tested like the marker.

**Files:**
- Create: `musefs-fuse/src/metrics_dir.rs`
- Modify: `musefs-fuse/src/lib.rs` (add `mod metrics_dir;`)

- [ ] **Step 1: Create the module.** Create `musefs-fuse/src/metrics_dir.rs`:

```rust
//! Synthetic `/proc`-style telemetry namespace: a `.musefs-metrics/` directory
//! at the mount root containing a single `metrics` file (#394). Mirrors the
//! Spotlight marker (`platform/spotlight.rs`) but is all-platform and gated at
//! the call sites by the runtime `expose_metrics` flag rather than `#[cfg]`.
//!
//! Reserved inodes sit at the very top of the u64 space, following the marker's
//! rationale: `InodeAllocator` starts at 2 and only increments with no ceiling,
//! so the top band is unreachable in practice (a fixed mid-range constant would
//! NOT be safe). They are disjoint from the macOS marker (`u64::MAX`).

use std::time::SystemTime;

use fuser::{FileAttr, FileType, INodeNo};

/// Mount root inode (fuser's FUSE root id).
const ROOT_INO: u64 = 1;

/// The synthetic directory's name at the mount root.
pub const METRICS_DIR_NAME: &str = ".musefs-metrics";
/// The single file inside it.
pub const METRICS_FILE_NAME: &str = "metrics";

/// Reserved sentinel inodes (top of the u64 space; disjoint from the macOS
/// Spotlight marker at `u64::MAX`).
pub const METRICS_DIR_INO: u64 = u64::MAX - 1;
pub const METRICS_FILE_INO: u64 = u64::MAX - 2;

/// True if `ino` is one of the two reserved metrics inodes.
pub fn is_metrics_ino(ino: u64) -> bool {
    ino == METRICS_DIR_INO || ino == METRICS_FILE_INO
}

/// Resolve `(parent, name)` to a metrics inode, or `None`. Callers gate this on
/// the `expose_metrics` flag.
pub fn metrics_lookup(parent: u64, name: &str) -> Option<u64> {
    if parent == ROOT_INO && name == METRICS_DIR_NAME {
        Some(METRICS_DIR_INO)
    } else if parent == METRICS_DIR_INO && name == METRICS_FILE_NAME {
        Some(METRICS_FILE_INO)
    } else {
        None
    }
}

/// Attributes for the synthetic directory (read-only, size 0, nlink 2).
pub fn dir_attr(uid: u32, gid: u32, dir_mode: u16, mtime: SystemTime) -> FileAttr {
    FileAttr {
        ino: INodeNo(METRICS_DIR_INO),
        size: 0,
        blocks: 0,
        atime: mtime,
        mtime,
        ctime: mtime,
        crtime: mtime,
        kind: FileType::Directory,
        perm: dir_mode,
        nlink: 2,
        uid,
        gid,
        rdev: 0,
        blksize: 512,
        flags: 0,
    }
}

/// Attributes for the synthetic `metrics` file. Size 0 (`/proc`-style): the
/// content is served at read time via `FOPEN_DIRECT_IO`, so the kernel reads to
/// EOF rather than trusting `st_size`.
pub fn file_attr(uid: u32, gid: u32, file_mode: u16, mtime: SystemTime) -> FileAttr {
    FileAttr {
        ino: INodeNo(METRICS_FILE_INO),
        size: 0,
        blocks: 0,
        atime: mtime,
        mtime,
        ctime: mtime,
        crtime: mtime,
        kind: FileType::RegularFile,
        perm: file_mode,
        nlink: 1,
        uid,
        gid,
        rdev: 0,
        blksize: 512,
        flags: 0,
    }
}

/// The readdir entry to append when listing the root (only the root).
pub fn root_dir_entry(dir_ino: u64) -> Option<(u64, FileType, String)> {
    (dir_ino == ROOT_INO)
        .then(|| (METRICS_DIR_INO, FileType::Directory, METRICS_DIR_NAME.to_string()))
}

/// The full inline listing for `readdir(METRICS_DIR_INO)`: `.`, `..`, `metrics`.
pub fn dir_listing() -> Vec<(u64, FileType, String)> {
    vec![
        (METRICS_DIR_INO, FileType::Directory, ".".to_string()),
        (ROOT_INO, FileType::Directory, "..".to_string()),
        (METRICS_FILE_INO, FileType::RegularFile, METRICS_FILE_NAME.to_string()),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn reserved_inodes_are_top_of_space_and_disjoint() {
        assert_eq!(METRICS_DIR_INO, u64::MAX - 1);
        assert_eq!(METRICS_FILE_INO, u64::MAX - 2);
        assert_ne!(METRICS_DIR_INO, u64::MAX); // != macOS marker
        assert_ne!(METRICS_FILE_INO, u64::MAX);
        assert!(is_metrics_ino(METRICS_DIR_INO));
        assert!(is_metrics_ino(METRICS_FILE_INO));
        assert!(!is_metrics_ino(1));
        assert!(!is_metrics_ino(u64::MAX));
    }

    #[test]
    fn lookup_resolves_dir_then_file() {
        assert_eq!(metrics_lookup(1, METRICS_DIR_NAME), Some(METRICS_DIR_INO));
        assert_eq!(metrics_lookup(METRICS_DIR_INO, METRICS_FILE_NAME), Some(METRICS_FILE_INO));
        assert_eq!(metrics_lookup(1, "metrics"), None);
        assert_eq!(metrics_lookup(METRICS_DIR_INO, ".musefs-metrics"), None);
        assert_eq!(metrics_lookup(2, METRICS_DIR_NAME), None);
    }

    #[test]
    fn root_dir_entry_only_at_root() {
        let mt = SystemTime::UNIX_EPOCH + Duration::from_secs(1);
        assert!(root_dir_entry(1).is_some());
        assert!(root_dir_entry(2).is_none());
        assert_eq!(dir_attr(0, 0, 0o555, mt).kind, FileType::Directory);
        assert_eq!(file_attr(0, 0, 0o444, mt).size, 0);
        assert_eq!(dir_listing().len(), 3);
    }
}
```

- [ ] **Step 2: Register the module.** In `musefs-fuse/src/lib.rs`, add near the other `mod` declarations (`mod convert; mod platform;`):

```rust
mod metrics_dir;
```

- [ ] **Step 3: Test + lint.**

Run: `cargo test -p musefs-fuse metrics_dir` then `cargo clippy -p musefs-fuse --all-targets -- -D warnings`
Expected: PASS / clean. (clippy may warn unused functions until Task 7 wires them; if so, the commit at Step 4 is still fine because the test references each — but if clippy `-D warnings` flags `dead_code`, add `#[allow(dead_code)]` on the module temporarily and REMOVE it in Task 7. Prefer to land Tasks 3+7 close together.)

- [ ] **Step 4: Commit.**

```bash
git add musefs-fuse/src/metrics_dir.rs musefs-fuse/src/lib.rs
git commit -m "feat(fuse): synthetic metrics-dir inode helpers (#394)"
```

---

## Task 4: `FuseConfig.expose_metrics` field

Add the runtime gate flag to the config POD. Default off. Update every `FuseConfig` literal in the same commit so the workspace stays green.

**Files:**
- Modify: `musefs-fuse/src/lib.rs` (`FuseConfig` struct + `Default`)
- Modify: `musefs-cli/src/lib.rs` (`parse_mount_config` literal)

- [ ] **Step 1: Add the field.** In `musefs-fuse/src/lib.rs`, add to `struct FuseConfig` (after `allow_other: bool,`):

```rust
    /// Expose the `/proc`-style `.musefs-metrics/` telemetry namespace at the
    /// mount root (#394). Default off; named distinctly from the compile-time
    /// `metrics` cargo feature (which gates the syscall counters).
    pub expose_metrics: bool,
```

In `impl Default for FuseConfig`, add (after `allow_other: false,`):

```rust
            expose_metrics: false,
```

- [ ] **Step 2: Update the CLI literal.** In `musefs-cli/src/lib.rs`, in `parse_mount_config`, add to the `FuseConfig { ... }` literal (after `allow_other: ...,`):

```rust
        expose_metrics: args.expose_metrics,
```

(`args.expose_metrics` is added in Task 9; for now this won't compile. To keep this task self-contained and green, set it to a literal `false` here and switch to `args.expose_metrics` in Task 9. Use `expose_metrics: false,` for this commit.)

- [ ] **Step 3: Build + test.**

Run: `cargo test -p musefs-fuse -p musefs-cli`
Expected: PASS (the existing `mount_args_parse_into_configs` test does not assert `expose_metrics`, so it stays green).

- [ ] **Step 4: Commit.**

```bash
git add musefs-fuse/src/lib.rs musefs-cli/src/lib.rs
git commit -m "feat(fuse): FuseConfig.expose_metrics gate field (#394)"
```

---

## Task 5: `PassthroughState::telemetry()` accessor

One method on both cfg arms: `Some((disabled, active))` on Linux, `None` on the stub.

**Files:**
- Modify: `musefs-fuse/src/platform/passthrough.rs`

- [ ] **Step 1: Linux arm.** In the `#[cfg(target_os = "linux")] mod imp`, add to `impl PassthroughState` (after `remove`):

```rust
        /// Telemetry: `(sticky-disabled, live backing registrations)`. The map
        /// length is read under its lock (scrapes are rare). `#394`.
        pub fn telemetry(&self) -> Option<(bool, u64)> {
            let active = self
                .backing
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .len() as u64;
            Some((self.disabled.load(Ordering::Relaxed), active))
        }
```

- [ ] **Step 2: Non-Linux stub arm.** In the `#[cfg(not(target_os = "linux"))] mod imp`, add to `impl PassthroughState` (after `remove`):

```rust
        /// No passthrough off Linux; telemetry is absent.
        #[allow(clippy::unused_self)]
        pub fn telemetry(&self) -> Option<(bool, u64)> {
            None
        }
```

- [ ] **Step 3: Export it.** The bottom `pub use imp::{PassthroughState, reply_open, request_capabilities};` already re-exports `PassthroughState`, so the method is reachable. No change needed.

- [ ] **Step 4: Build + lint.**

Run: `cargo build -p musefs-fuse` then `cargo clippy -p musefs-fuse --all-targets -- -D warnings`
Expected: clean (the method is used in Task 6/7; if `dead_code` fires under `-D warnings`, proceed — it's consumed two tasks later; land close together, or temporarily `#[allow(dead_code)]` and remove in Task 7).

- [ ] **Step 5: Commit.**

```bash
git add musefs-fuse/src/platform/passthrough.rs
git commit -m "feat(fuse): PassthroughState telemetry accessor (#394)"
```

---

## Task 6: Fuse-side gather + render method, jemalloc/metrics probes

A method on `MusefsFs` that assembles `FuseTelemetry`, pulls `CoreTelemetry`, computes the optional allocator/syscall probes, and returns the rendered bytes. Plus the `jemalloc` cargo feature.

**Files:**
- Modify: `musefs-fuse/Cargo.toml` (add `jemalloc` feature + optional dep)
- Modify: `musefs-fuse/src/lib.rs` (gather method + probe helpers)

- [ ] **Step 1: Add the jemalloc feature + dep.** In `musefs-fuse/Cargo.toml`, change `[features]` to:

```toml
[features]
metrics = ["musefs-core/metrics"]
jemalloc = ["dep:tikv-jemalloc-ctl", "tikv-jemalloc-ctl/stats"]
```

And add to `[dependencies]`:

```toml
tikv-jemalloc-ctl = { version = "0.7", optional = true }
```

- [ ] **Step 2: Add the probe helpers + gather method.** In `musefs-fuse/src/lib.rs`, add these free functions near the top-level helpers (e.g. after `open_flags`):

```rust
/// jemalloc allocator stats, or `None` when not built with the `jemalloc`
/// feature (or when the ctls fail — best-effort, never panics). #394.
#[cfg(feature = "jemalloc")]
fn allocator_stats() -> Option<musefs_core::AllocatorStats> {
    use tikv_jemalloc_ctl::{epoch, stats};
    epoch::advance().ok()?;
    Some(musefs_core::AllocatorStats {
        allocated: stats::allocated::read().ok()? as u64,
        resident: stats::resident::read().ok()? as u64,
        active: stats::active::read().ok()? as u64,
        retained: stats::retained::read().ok()? as u64,
    })
}

#[cfg(not(feature = "jemalloc"))]
fn allocator_stats() -> Option<musefs_core::AllocatorStats> {
    None
}

/// Serve-path syscall counters, present only on a `metrics`-feature build.
#[cfg(feature = "metrics")]
fn syscall_snapshot() -> Option<musefs_core::metrics::Snapshot> {
    Some(musefs_core::metrics::snapshot())
}

#[cfg(not(feature = "metrics"))]
fn syscall_snapshot() -> Option<musefs_core::metrics::Snapshot> {
    None
}
```

Add the gather method inside `impl MusefsFs` (near `fire_poll_refresh`):

```rust
    /// Assemble and render the `.musefs-metrics/metrics` body (#394). Best-effort:
    /// every source is an atomic load, a brief lock, or a fallible probe mapped to
    /// `None`/0; nothing here can panic the daemon or perturb a read.
    fn render_metrics(&self) -> Vec<u8> {
        let core = self.core.telemetry();
        let dir_handles = self
            .dir_handles
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len() as u64;
        let fuse = musefs_core::FuseTelemetry {
            uptime_seconds: self.mount_time.elapsed().map(|d| d.as_secs()).unwrap_or(0),
            reads_inflight: self.inflight_reads.load(Ordering::Relaxed) as u64,
            reads_inflight_max: MAX_INFLIGHT_READS as u64,
            dir_handles,
            dir_handles_max: MAX_DIR_HANDLES as u64,
            pool_workers: self.pool.max_count() as u64,
            pool_active: self.pool.active_count() as u64,
            pool_queued: self.pool.queued_count() as u64,
            passthrough: self.passthrough.telemetry().map(|(disabled, active)| {
                musefs_core::PassthroughTelemetry { disabled, active }
            }),
        };
        let alloc = allocator_stats();
        let syscalls = syscall_snapshot();
        musefs_core::render_prometheus(&core, &fuse, alloc.as_ref(), syscalls.as_ref())
            .into_bytes()
    }
```

- [ ] **Step 3: Build all feature combos.**

Run:
```bash
cargo build -p musefs-fuse
cargo build -p musefs-fuse --features metrics
cargo build -p musefs-fuse --features jemalloc
```
Expected: all clean. (The `jemalloc` build compiles the ctl calls; they're only *meaningful* when jemalloc is the global allocator, i.e. in the binary or e2e — see Task 11.)

- [ ] **Step 4: Lint.**

Run: `cargo clippy -p musefs-fuse --all-targets -- -D warnings`
Expected: clean. (`render_metrics` is unused until Task 7; if `-D warnings` flags it, land Task 7 in the same session — do not add a permanent `#[allow]`.)

- [ ] **Step 5: Commit.**

```bash
git add musefs-fuse/Cargo.toml musefs-fuse/src/lib.rs
git commit -m "feat(fuse): metrics gather/render + jemalloc & metrics probes (#394)"
```

---

## Task 7: Wire the synthetic namespace into FUSE dispatch

Intercept the two reserved inodes (and the root listing) in `lookup`, `getattr`, `opendir`, `readdir`, `open`, `read`, `release` — all guarded by `self.config.expose_metrics`. Add the per-open buffer map.

**Files:**
- Modify: `musefs-fuse/src/lib.rs`

- [ ] **Step 1: Add the buffer-map fields to `MusefsFs`.** Add to the `struct MusefsFs` (after `inflight_reads: Arc<AtomicUsize>,`):

```rust
    /// Per-open rendered `.musefs-metrics/metrics` buffers, keyed by the fh handed
    /// out at `open` (#394). Each open snapshots once; reads slice it by absolute
    /// offset; `release` drops it. Empty/untouched unless `expose_metrics` is on.
    metrics_handles: Arc<Mutex<std::collections::HashMap<u64, Arc<Vec<u8>>>>>,
    /// Monotonic fh source for `metrics_handles` (starts at 1; never 0).
    metrics_fh: Arc<AtomicU64>,
```

In `MusefsFs::new`, add to the initializer (after `inflight_reads: Arc::new(AtomicUsize::new(0)),`):

```rust
            metrics_handles: Arc::new(Mutex::new(std::collections::HashMap::new())),
            metrics_fh: Arc::new(AtomicU64::new(1)),
```

- [ ] **Step 2: `lookup` — intercept before core.** In `lookup`, immediately after the marker block (`if platform::spotlight::marker_lookup(...) { ... }`), insert:

```rust
        if self.config.expose_metrics
            && let Some(mino) = metrics_dir::metrics_lookup(parent.0, name)
        {
            let attr = if mino == metrics_dir::METRICS_DIR_INO {
                metrics_dir::dir_attr(self.uid, self.gid, self.config.dir_mode, self.mount_time)
            } else {
                metrics_dir::file_attr(self.uid, self.gid, self.config.file_mode, self.mount_time)
            };
            return reply.entry(&self.config.ttl, &attr, Generation(0));
        }
```

- [ ] **Step 3: `getattr` — intercept the two inodes.** In `getattr`, after the marker block, insert:

```rust
        if self.config.expose_metrics && metrics_dir::is_metrics_ino(ino.0) {
            let attr = if ino.0 == metrics_dir::METRICS_DIR_INO {
                metrics_dir::dir_attr(self.uid, self.gid, self.config.dir_mode, self.mount_time)
            } else {
                metrics_dir::file_attr(self.uid, self.gid, self.config.file_mode, self.mount_time)
            };
            return reply.attr(&self.config.ttl, &attr);
        }
```

- [ ] **Step 4: `opendir` — stateless fh for the metrics dir; append to root.** Replace the `opendir` body with:

```rust
    fn opendir(&self, _req: &Request, ino: INodeNo, _flags: OpenFlags, reply: ReplyOpen) {
        self.fire_poll_refresh();
        if self.config.expose_metrics && ino.0 == metrics_dir::METRICS_DIR_INO {
            // Stateless: readdir(METRICS_DIR_INO) serves an inline listing and
            // never consults dir_handles, so this fh burns no MAX_DIR_HANDLES slot.
            return reply.opened(FileHandle(0), FopenFlags::empty());
        }
        let core = Arc::clone(&self.core);
        let handles = Arc::clone(&self.dir_handles);
        let counter = Arc::clone(&self.dir_fh);
        let expose_metrics = self.config.expose_metrics;
        self.pool.execute(move || {
            let listing = match build_dir_listing(&core, ino.0, expose_metrics) {
                Ok(l) => l,
                Err(e) => return reply.error(reply_errno("opendir", ino.0, &e)),
            };
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

- [ ] **Step 5: Extend `build_dir_listing` to append the root entry.** Replace `build_dir_listing` with:

```rust
/// Build a directory's full readdir listing once. Shared by `opendir`
/// (snapshotted per fh) and the `readdir` fallback for an unknown fh. When
/// `expose_metrics` is on, the synthetic `.musefs-metrics` entry is appended to
/// the root listing (append-without-dedup, matching the Spotlight marker; #394).
fn build_dir_listing(
    core: &Musefs,
    ino: u64,
    expose_metrics: bool,
) -> Result<Vec<(u64, FileType, String)>, CoreError> {
    let entries = core.readdir(ino)?;
    let parent = core.parent(ino).unwrap_or(ino);
    let marker = platform::spotlight::marker_dir_entry(ino);
    let mut listing = assemble_dir_listing(ino, parent, entries, marker);
    if expose_metrics && let Some(entry) = metrics_dir::root_dir_entry(ino) {
        listing.push(entry);
    }
    Ok(listing)
}
```

- [ ] **Step 6: `readdir` — serve the metrics dir inline.** In `readdir`, immediately after `self.fire_poll_refresh();`, insert the inline-serve branch, and update the unknown-fh fallback call to pass the flag:

```rust
        if self.config.expose_metrics && ino.0 == metrics_dir::METRICS_DIR_INO {
            let listing = metrics_dir::dir_listing();
            for (i, (child, kind, name)) in listing.iter().enumerate().skip(usize_from(offset)) {
                if reply.add(INodeNo(*child), (i + 1) as u64, *kind, name) {
                    break;
                }
            }
            return reply.ok();
        }
```

And change the fallback line inside `readdir`:

```rust
            None => match build_dir_listing(&self.core, ino.0, self.config.expose_metrics) {
```

- [ ] **Step 7: `open` — render once into a buffer; DIRECT_IO.** In `open`, after the marker block (`if platform::spotlight::is_marker(ino.0) { ... }`), insert:

```rust
        if self.config.expose_metrics && ino.0 == metrics_dir::METRICS_FILE_INO {
            let body = Arc::new(self.render_metrics());
            let fh = self.metrics_fh.fetch_add(1, Ordering::Relaxed);
            self.metrics_handles
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert(fh, body);
            // DIRECT_IO (no NONSEEKABLE): size-0 stat means the kernel reads to
            // EOF, and absolute-offset slicing in `read` supports pread/re-reads.
            return reply.opened(FileHandle(fh), FopenFlags::FOPEN_DIRECT_IO);
        }
```

- [ ] **Step 8: `read` — slice the buffer by absolute offset.** In `read`, after the marker block (`if platform::spotlight::is_marker(ino.0) { return reply.data(&[]); }`), insert:

```rust
        if self.config.expose_metrics && ino.0 == metrics_dir::METRICS_FILE_INO {
            let body = self
                .metrics_handles
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .get(&fh.0)
                .map(Arc::clone);
            let Some(body) = body else {
                return reply.data(&[]); // unknown fh → EOF
            };
            let start = usize_from(offset).min(body.len());
            let end = start.saturating_add(usize_from(u64::from(size))).min(body.len());
            return reply.data(&body[start..end]);
        }
```

- [ ] **Step 9: `release` — drop the metrics buffer.** In `release`, replace the body so it dispatches on the (now-used) ino:

```rust
    fn release(
        &self,
        _req: &Request,
        ino: INodeNo,
        fh: FileHandle,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        if self.config.expose_metrics && ino.0 == metrics_dir::METRICS_FILE_INO {
            self.metrics_handles
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(&fh.0);
            return reply.ok();
        }
        // Cheap (a backing-map remove + a slab remove); no need to offload to the pool.
        if let Some(fh) = NonZeroU64::new(fh.0) {
            self.passthrough.remove(fh.get());
            self.core.release_handle(Fh::from(fh));
        }
        reply.ok();
    }
```

(Note: the `_ino` parameter is renamed to `ino`.)

- [ ] **Step 10: Fix the other `build_dir_listing` caller.** Search for any remaining 2-arg `build_dir_listing(` calls and add the flag. The only call sites are `opendir` (Step 4, updated) and the `readdir` fallback (Step 6, updated). Confirm with:

Run: `grep -n "build_dir_listing(" musefs-fuse/src/lib.rs`
Expected: every call passes three args.

- [ ] **Step 11: Add a dispatch unit test (no real mount needed for `render_metrics` shape).** In the `lib.rs` test module, add:

```rust
    #[test]
    fn build_dir_listing_appends_metrics_entry_only_when_exposed() {
        // Uses the existing test core builder in this module; if the helper is
        // named differently, match it. Builds a Musefs over a tiny fixture DB.
        let (_dir, core) = test_core(); // <-- match existing helper name
        let without = build_dir_listing(&core, 1, false).unwrap();
        assert!(!without.iter().any(|(_, _, n)| n == metrics_dir::METRICS_DIR_NAME));
        let with = build_dir_listing(&core, 1, true).unwrap();
        assert!(with.iter().any(|(ino, _, n)| {
            *ino == metrics_dir::METRICS_DIR_INO && n == metrics_dir::METRICS_DIR_NAME
        }));
    }
```

If no in-module `Musefs` builder exists, skip this test and rely on the e2e (Task 10) — do not invent a heavyweight fixture. Remove any temporary `#[allow(dead_code)]` added in Tasks 3/5/6.

- [ ] **Step 12: Test + lint (all combos).**

Run:
```bash
cargo test -p musefs-fuse
cargo test -p musefs-fuse --features metrics
cargo clippy -p musefs-fuse --all-targets --features metrics -- -D warnings
cargo clippy -p musefs-fuse --all-targets --features jemalloc -- -D warnings
```
Expected: PASS / clean.

- [ ] **Step 13: Commit.**

```bash
git add musefs-fuse/src/lib.rs
git commit -m "feat(fuse): wire .musefs-metrics into FUSE dispatch (#394)"
```

---

## Task 8: Binary forwards the jemalloc feature

So a default release build (which already enables `jemalloc`) populates the allocator block.

**Files:**
- Modify: `musefs/Cargo.toml`

- [ ] **Step 1: Forward the feature.** In `musefs/Cargo.toml`, change the `jemalloc` feature line to also enable the fuse feature. The dependency on `musefs-fuse` is transitive (via `musefs-cli`), so enable it through `musefs-cli`. First add a passthrough feature to `musefs-cli`.

In `musefs-cli/Cargo.toml`, add a `[features]` section (it has none today):

```toml
[features]
jemalloc = ["musefs-fuse/jemalloc"]
metrics = ["musefs-fuse/metrics"]
```

In `musefs/Cargo.toml`, change:

```toml
jemalloc = ["dep:tikv-jemallocator", "dep:tikv-jemalloc-ctl", "tikv-jemalloc-ctl/stats", "dep:log", "musefs-cli/jemalloc"]
```

- [ ] **Step 2: Build the binary (default features include jemalloc).**

Run: `cargo build -p musefs`
Expected: clean; the binary now compiles `musefs-fuse` with `jemalloc`.

- [ ] **Step 3: Commit.**

```bash
git add musefs/Cargo.toml musefs-cli/Cargo.toml
git commit -m "build: forward jemalloc feature to musefs-fuse for the allocator block (#394)"
```

---

## Task 9: CLI `--expose-metrics` flag

**Files:**
- Modify: `musefs-cli/src/lib.rs` (`MountArgs` field + `parse_mount_config` + test)

- [ ] **Step 1: Add the flag.** In `MountArgs`, add (after the `allow_other` field):

```rust
    /// Expose a `/proc`-style `.musefs-metrics/metrics` file at the mount root
    /// for live observability (handles, read/dir-handle queues, caches, tree,
    /// allocator). Off by default. Distinct from the compile-time `metrics`
    /// cargo feature, which adds the syscall counters.
    #[arg(long, env = "MUSEFS_EXPOSE_METRICS", value_parser = clap::builder::BoolishValueParser::new())]
    pub expose_metrics: bool,
```

- [ ] **Step 2: Wire it into the config.** In `parse_mount_config`, change the `expose_metrics` line in the `FuseConfig` literal from the placeholder `false` (Task 4) to:

```rust
        expose_metrics: args.expose_metrics,
```

- [ ] **Step 3: Extend the CLI test.** In `mount_args_parse_into_configs`, add `"--expose-metrics", "true",` to the args array and assert:

```rust
        assert!(fuse_config.expose_metrics);
```

Also add a second assertion that it defaults off — a minimal extra test:

```rust
    #[test]
    fn expose_metrics_defaults_off() {
        use clap::Parser;
        let cli = Cli::try_parse_from(["musefs", "mount", "/mnt", "--db", "/tmp/x.db"]).unwrap();
        let Command::Mount(args) = cli.command else { panic!("expected Mount") };
        let (_c, fuse_config) = parse_mount_config(&args);
        assert!(!fuse_config.expose_metrics);
    }
```

- [ ] **Step 4: Test.**

Run: `cargo test -p musefs-cli`
Expected: PASS.

- [ ] **Step 5: Commit.**

```bash
git add musefs-cli/src/lib.rs
git commit -m "feat(cli): --expose-metrics / MUSEFS_EXPOSE_METRICS flag (#394)"
```

---

## Task 10: End-to-end mount test + runnable script

A real-mount `#[ignore]` test asserting the read model, counter movement, listing visibility, and the audio invariant. Plus an in-tree script CI can invoke.

**Files:**
- Create: `musefs-fuse/tests/metrics_e2e.rs` (or extend the existing e2e test file if one exists — check `musefs-fuse/tests/`)
- Create: `scripts/metrics-e2e.sh`

- [ ] **Step 1: Inspect existing e2e harness.** Run `ls musefs-fuse/tests/ && grep -rln "#\[ignore\]" musefs-fuse/tests/` to find the existing mount-harness helpers (a fixture DB builder + `spawn`/`spawn_with` mount). Reuse them; match their setup exactly. Note from memory: passthrough e2e needs `/dev/fuse` + libfuse and may need sudo for real passthrough — but this test only reads the metrics file and a small track, so it runs unprivileged under a normal FUSE mount.

- [ ] **Step 2: Write the e2e test.** Create `musefs-fuse/tests/metrics_e2e.rs` modeled on the existing harness. It must:

```rust
// Mount with FuseConfig { expose_metrics: true, ..default }, then:
// 1. assert `cat <mnt>/.musefs-metrics/metrics` returns non-empty Prometheus text
//    containing "musefs_handles_open" and "musefs_reads_inflight_max 1024".
// 2. open a real track file, hold it open, and assert the metrics text now shows
//    musefs_handles_open >= 1 (re-read the file → fresh snapshot per open).
// 3. pread the metrics file at a non-zero offset (e.g. offset 10, len 20) and
//    assert the returned bytes equal body[10..30] of a full read (absolute-offset,
//    NOT consume-once).
// 4. assert a full read (read-to-EOF loop, ignoring st_size==0) returns the whole
//    body — i.e. reading in 8-byte chunks until empty reconstructs the file.
// 5. assert `.musefs-metrics` appears in a readdir of the root (ls -a equivalent).
// 6. md5 a normal track served through the mount and assert it equals the md5 of
//    the backing file's audio (cardinal invariant: the namespace doesn't perturb
//    audio reads). Reuse the existing invariant-check helper if the harness has one.
#[test]
#[ignore = "requires /dev/fuse + libfuse; run with --ignored"]
fn metrics_surface_e2e() {
    // ... using the existing fixture+spawn helpers ...
}
```

Write it out fully against the real harness API discovered in Step 1 (the plan cannot guess the helper names; the executor fills them from the sibling tests, matching their exact signatures). Keep each assertion above as a labeled block.

- [ ] **Step 3: Write the runnable script.** Create `scripts/metrics-e2e.sh` (and `chmod +x`):

```bash
#!/usr/bin/env bash
# Runs the .musefs-metrics e2e mount test (#394). Requires /dev/fuse + libfuse.
set -euo pipefail
cd "$(dirname "$0")/.."
cargo test -p musefs-fuse --test metrics_e2e -- --ignored --nocapture
```

- [ ] **Step 4: Run it (locally, where /dev/fuse exists).**

Run: `bash scripts/metrics-e2e.sh`
Expected: the test mounts under `$HOME` (AppArmor allows FUSE there; `/data` is denied — see project memory), reads the metrics file, and PASSES all six assertions.

- [ ] **Step 5: Commit.**

```bash
git add musefs-fuse/tests/metrics_e2e.rs scripts/metrics-e2e.sh
git commit -m "test(fuse): e2e for .musefs-metrics read model + audio invariant (#394)"
```

---

## Task 11: Documentation

**Files:**
- Modify: `README.md`
- Modify: `ARCHITECTURE.md`

- [ ] **Step 1: README.** Add a short subsection under the CLI flags / usage area documenting:
  - `--expose-metrics` / `MUSEFS_EXPOSE_METRICS` (default off): exposes `.musefs-metrics/metrics`.
  - A sample `cat <mnt>/.musefs-metrics/metrics` excerpt (3–4 lines of Prometheus output).
  - One sentence disambiguating the runtime `--expose-metrics` flag from the compile-time `metrics` cargo feature (which adds the syscall counters), and noting the jemalloc block needs a `jemalloc`-feature build (the default).
  - One sentence on the `/proc`-style `st_size == 0` behavior: use EOF-aware readers (`cat`, Prometheus textfile collector), not `read`-by-`st_size`.

- [ ] **Step 2: ARCHITECTURE.md.** Add a short "Synthetic telemetry namespace" subsection: reserved inodes at `u64::MAX-1/-2` (mirroring the Spotlight marker), dynamic per-read generation rendered at `open`, and that it deliberately bypasses the virtual tree and the `RegionLayout`/segment model so the cardinal audio path is untouched. Cross-link the metric list to `musefs-core/src/telemetry.rs`.

- [ ] **Step 3: Commit (docs-only — pre-commit skips the cargo gate).**

```bash
git add README.md ARCHITECTURE.md
git commit -m "docs: document --expose-metrics and the telemetry namespace (#394)"
```

---

## Task 12: Final verification

- [ ] **Step 1: Full workspace gate (what pre-commit runs).**

```bash
cargo fmt --all --check
cargo clippy --all-targets --workspace -- -D warnings
cargo test --workspace
```
Expected: all clean/green.

- [ ] **Step 2: Feature-matrix tests (CI's `check` job; local `--workspace` skips these — see memory).**

```bash
cargo test -p musefs-core --features metrics
cargo test -p musefs-fuse --features metrics
cargo clippy -p musefs-fuse --all-targets --features jemalloc -- -D warnings
cargo test -p musefs   # default features include jemalloc; exercises the real allocator block path
```
Expected: green/clean.

- [ ] **Step 3: Fuzz crate still builds (out-of-workspace; format-layer signatures unchanged here, but verify per memory).**

```bash
cargo +nightly fuzz build 2>/dev/null || echo "skip if nightly/fuzz unavailable"
```
Expected: builds, or skipped.

- [ ] **Step 4: Mutant anchors clean.**

```bash
python3 scripts/check_mutant_anchors.py
```
Expected: PASS (Task 2 already re-anchored if needed).

- [ ] **Step 5: e2e (manual, where /dev/fuse exists).**

```bash
bash scripts/metrics-e2e.sh
```
Expected: PASS.

- [ ] **Step 6: Confirm default-off is truly inert.** Build the binary and mount WITHOUT `--expose-metrics`; confirm `.musefs-metrics` does not appear (`ls -a` of the root) and a `cat` of that path returns ENOENT. (Covered conceptually by `expose_metrics_defaults_off`; this is the live check.)

---

## Spec coverage check

| Spec requirement | Task |
| ---------------- | ---- |
| `--expose-metrics` flag, default off, `MUSEFS_EXPOSE_METRICS` | 4, 9 |
| Single `.musefs-metrics/metrics`, Prometheus format | 1, 7 |
| Reserved inodes `u64::MAX-1/-2`, no allocator change | 3 |
| Render-once-at-open, absolute-offset, DIRECT_IO no NONSEEKABLE, size 0 | 3, 7, 10 |
| Root readdir appends (not shadows), inline metrics-dir readdir, no dir-handle-cap use | 7 |
| Core renders; fuse plumbs; jemalloc via feature (deviation noted) | 1, 6, 8 |
| `handles_open` AtomicUsize, increment-on-success only | 2 |
| Cache raw hit/miss documented; weight/capacity mappings | 1, 2 |
| `passthrough_active` = locked `map.len()`, absent off-Linux | 5, 6 |
| Thread-pool / cache / size-cache / tree / inode / refresh metrics | 1, 2, 6 |
| Syscall counters only with `metrics` feature | 6 |
| Best-effort error handling (no panics) | 1, 2, 5, 6 |
| Unit + e2e tests incl. audio invariant + read-model edges | 1, 2, 3, 9, 10 |
| README + ARCHITECTURE docs | 11 |
| Feature-matrix + mutant-anchor + fuzz verification | 2, 12 |
