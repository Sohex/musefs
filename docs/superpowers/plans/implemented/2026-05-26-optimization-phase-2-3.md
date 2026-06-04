# musefs Optimization — Phase 2 + Phase 3 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Eliminate the per-read `open()`/`stat()` on the streaming path (Phase 2: a per-handle cached backing fd + resolved layout) and make the cache concurrency-friendly and metadata-walk-cheap (Phase 3: a sharded O(1) byte-bounded layout cache whose `resolve` does stat/synthesis off-lock, a size/attr cache for `getattr`, and lazy cross-refresh invalidation).

**Architecture:** `open()`/`release()` are implemented in the FUSE adapter and backed by a handle table in `Musefs` (`fh -> Handle { Arc<ResolvedFile>, Arc<File> }`); `read` serves from the handle's pre-opened fd via a new `read_at_with_file`, so a streamed file is opened/validated once. The header cache becomes `&self` with internal sharded mutexes (16 shards), each a hand-rolled O(1) doubly-linked-list byte-bounded LRU; `resolve` validates + synthesizes **outside** the shard lock and locks only for the get/insert. A separate unbounded size/attr cache serves `getattr`/`lookup` without a backing `stat` or full synthesis. `poll_refresh` stops wholesale-clearing — entries self-invalidate on a `content_version` mismatch, and vanished tracks are pruned against the live tree.

**Tech Stack:** Rust, `fuser` 0.14, `rusqlite` (WAL), existing `arc-swap`/`threadpool`. No new dependencies (the LRU is hand-rolled).

**Builds on (already on `main`):** Phase 1 — `Musefs` is `&self` interior-mutable (`ArcSwap<VirtualTree>` tree, `Mutex<HeaderCache>` cache via a `cache()` poison-recovery helper, `AtomicI64` version); `DbPool` (`with`/`with_poll`); FUSE ops offload to a `threadpool`; `metrics` feature with `on_open`/`on_stat`/`on_pread`/`on_art_chunk` + `MUSEFS_FAULT_*_US` fault injection.

**Invariant (non-negotiable):** served audio stays byte-identical. Every task keeps the e2e mount tests green: `cargo test -p musefs-fuse -- --ignored --test-threads=1` (real mount; `/dev/fuse` is available).

**Behavior change introduced (Phase 3, intentional, documented):** `getattr` on a size-cache hit no longer `stat`s the backing file. Backing-file changes are detected at scan time (via `data_version` → `poll_refresh`) and at file `open` (handle validation), not at `getattr`. This is acceptable for a read-only mount and matches stale-within-TTL attr semantics; it is what removes the per-file `stat` (NFS round-trip) from a library walk.

---

## File Structure

- `musefs-core/src/reader.rs` — (P2.1) split `read_at` into a thin opening wrapper + `read_at_with_file`. (P3.1) replace `HeaderCache` internals with a sharded O(1) LRU; `resolve` becomes `&self` and locks only for get/insert.
- `musefs-core/src/facade.rs` — (P2.2) add the handle table + `open_handle`/`release_handle` + `read(ino, fh, …)`. (P3.1) `cache: HeaderCache` (no outer `Mutex`); drop the `cache()` helper. (P3.2) add the size/attr cache + use it in `getattr`. (P3.3) lazy invalidation + live-track pruning in `poll_refresh`.
- `musefs-fuse/src/lib.rs` — (P2.3) implement `open`/`release`; pass `fh` through `read`.
- `musefs-core/tests/metrics.rs` — (P2.4) handle reuses one open + zero per-read stat; (P3.4) size-cache hit skips stat.
- `musefs-core/tests/reader.rs` / `tests/read_at.rs` — (P3.1) update `let mut cache` → `let cache`; rework the byte-budget test for sharding.
- `musefs-core/tests/facade.rs` — (P2.2/P3.3) handle round-trip + lazy-invalidation tests; update `fs.read(...)` call sites for the new `fh` arg.

---

# Phase 2 — File-handle lifecycle

## Task 2.1: Extract `read_at_with_file` (keep `read_at` signature stable)

**Files:** Modify `musefs-core/src/reader.rs` (`read_at`).

The goal: a function that serves bytes from an **already-open** backing file, so the handle path doesn't reopen per read. `read_at` keeps its current signature (opens once, delegates) so the ~13 existing callers/tests are untouched.

- [ ] **Step 1: Add a test driving the new function (reader.rs inline tests)**

Add to the `ogg_serve_tests` or a fitting inline `mod` — but simplest, add a focused test near the existing read tests in `musefs-core/src/reader.rs`. First find an existing inline test that builds a `resolved` for a FLAC/MP3 (e.g. in `resolve_ogg_tests` there's `build_opus_file`). Add this test to the inline `#[cfg(test)] mod` that already has `use` for `Db`, `read_at`, etc. (mirror the existing `resolves_and_reads_opus_with_identical_audio` setup to get a `resolved` + `db`):

```rust
    #[test]
    fn read_at_with_file_matches_read_at() {
        // Build any resolved file + db (reuse the opus helper used elsewhere).
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("track.opus");
        let (audio_offset, audio_length) = build_opus_file(&path);
        let db = Db::open_in_memory().unwrap();
        let meta = std::fs::metadata(&path).unwrap();
        let track_id = db
            .upsert_track(&NewTrack {
                backing_path: path.to_string_lossy().to_string(),
                format: Format::Opus,
                audio_offset: audio_offset as i64,
                audio_length: audio_length as i64,
                backing_size: meta.len() as i64,
                backing_mtime: mtime_secs(&meta),
            })
            .unwrap();
        let cache = HeaderCache::new(Mode::Synthesis);
        let resolved = cache.resolve(&db, track_id).unwrap();

        let via_open = read_at(&resolved, &db, 0, resolved.total_len).unwrap();
        let file = std::fs::File::open(&resolved.backing_path).unwrap();
        let via_file = read_at_with_file(&resolved, &db, &file, 0, resolved.total_len).unwrap();
        assert_eq!(via_open, via_file);
    }
```
NOTE: this test assumes `cache.resolve` is callable on a non-`mut` binding. It is `&mut self` *today*; for THIS task keep `let mut cache` and `cache.resolve(...)`. (Task 3.1 makes it `&self`.) Adjust the binding to `let mut cache` when running before Task 3.1.

- [ ] **Step 2: Run it to confirm it fails**

Run: `cargo test -p musefs-core read_at_with_file_matches_read_at 2>&1 | head -20`
Expected: FAIL — `cannot find function read_at_with_file`.

- [ ] **Step 3: Refactor `read_at` into a wrapper + `read_at_with_file`**

Replace the body of `read_at` (use `replace_symbol_body` on `read_at`). The new `read_at` opens the file once and delegates; `read_at_with_file` holds the segment loop and uses the provided `&File` (no opening):

```rust
/// Read `size` bytes starting at virtual `offset` from a resolved file, opening
/// the backing file once for this call. Prefer `read_at_with_file` when a backing
/// fd is already held (the per-handle read path) to avoid reopening.
pub fn read_at(resolved: &ResolvedFile, db: &Db, offset: u64, size: u64) -> Result<Vec<u8>> {
    if offset >= resolved.total_len || size == 0 {
        return Ok(Vec::new());
    }
    crate::metrics::on_open();
    let file = std::fs::File::open(&resolved.backing_path)?;
    read_at_with_file(resolved, db, &file, offset, size)
}

/// Serve a byte range from a resolved file using an already-open backing `file`.
/// Splices inline framing, positioned backing reads, art-blob reads, and Ogg page
/// serving. Returns fewer bytes (possibly empty) near EOF.
pub fn read_at_with_file(
    resolved: &ResolvedFile,
    db: &Db,
    file: &std::fs::File,
    offset: u64,
    size: u64,
) -> Result<Vec<u8>> {
    use std::os::unix::fs::FileExt;

    if offset >= resolved.total_len || size == 0 {
        return Ok(Vec::new());
    }
    let end = offset.saturating_add(size).min(resolved.total_len);
    let mut out = Vec::with_capacity((end - offset) as usize);

    let mut seg_start = 0u64;
    for seg in &resolved.layout.segments {
        let seg_len = seg.len();
        let seg_end = seg_start + seg_len;
        let ov_start = offset.max(seg_start);
        let ov_end = end.min(seg_end);
        if ov_start < ov_end {
            let within = ov_start - seg_start;
            let n = (ov_end - ov_start) as usize;
            match seg {
                Segment::Inline(bytes) => {
                    let w = within as usize;
                    out.extend_from_slice(&bytes[w..w + n]);
                }
                Segment::BackingAudio { offset: bo, .. } => {
                    let mut buf = vec![0u8; n];
                    file.read_exact_at(&mut buf, bo + within)?;
                    crate::metrics::on_pread(n as u64);
                    out.extend_from_slice(&buf);
                }
                Segment::ArtImage { art_id, .. } => {
                    let chunk = db.read_art_chunk(*art_id, within, n)?;
                    crate::metrics::on_art_chunk();
                    out.extend_from_slice(&chunk);
                }
                Segment::OggAudio { offset: ao, seq_delta, len } => {
                    let index = resolved
                        .ogg_index
                        .get_or_try_init(|| {
                            build_index(&resolved.backing_path, *ao, *len, *seq_delta).map(Arc::new)
                        })?
                        .clone();
                    serve(&index, file, *ao, within, within + n as u64, &mut out)?;
                }
                Segment::OggArtSlice { art_id, offset, base64, art_total, .. } => {
                    if *base64 {
                        let w =
                            musefs_format::ogg::b64_window(*offset + within, n as u64, *art_total);
                        let raw = db.read_art_chunk(*art_id, w.in_start, w.in_len as usize)?;
                        crate::metrics::on_art_chunk();
                        out.extend_from_slice(&musefs_format::ogg::encode_b64_slice(&raw, w.skip, n));
                    } else {
                        let chunk = db.read_art_chunk(*art_id, *offset + within, n)?;
                        crate::metrics::on_art_chunk();
                        out.extend_from_slice(&chunk);
                    }
                }
            }
        }
        seg_start = seg_end;
        if seg_start >= end {
            break;
        }
    }
    Ok(out)
}
```
Note: `read_at` now opens unconditionally (when the range is non-empty) rather than lazily-on-first-backing-segment. No existing test asserts open-count for an inline-only range, so content results are unchanged; the metrics baseline reads whole files (touch backing), so `opens == reads` still holds there.

- [ ] **Step 4: Run the new test + the existing read tests**

Run: `cargo test -p musefs-core read_at`
Expected: PASS (the new test + the existing `read_at`/`reader` tests are unaffected by the signature-stable wrapper).
Run: `cargo test -p musefs-core`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add musefs-core/src/reader.rs
git commit -m "$(cat <<'EOF'
refactor(core): split read_at into an opening wrapper + read_at_with_file

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

## Task 2.2: Handle table + `open_handle`/`release_handle` + `read(ino, fh, …)` in `Musefs`

**Files:** Modify `musefs-core/src/facade.rs`; update `read` call sites in `musefs-core/tests/facade.rs`, `tests/metrics.rs`, `benches/read_throughput.rs`.

- [ ] **Step 1: Write the handle round-trip test (facade integration test)**

Add to `musefs-core/tests/facade.rs` (it already has `config()`, `scanned_db`, and imports `Musefs`, `VirtualTree`):

```rust
#[test]
fn open_handle_read_and_release_roundtrip() {
    let dir = tempfile::tempdir().unwrap();
    let db = scanned_db(dir.path());
    let fs = Musefs::open(db, config()).unwrap();

    let artist = fs.lookup(VirtualTree::ROOT, "Alice").unwrap();
    let (_, file_inode, _) = fs.readdir(artist).unwrap().into_iter().next().unwrap();
    let size = fs.getattr(file_inode).unwrap().size;

    // Open returns a non-zero handle; reads via the handle match a fh==0 read.
    let fh = fs.open_handle(file_inode).unwrap();
    assert!(fh != 0);
    let via_handle = fs.read(file_inode, fh, 0, size).unwrap();
    let via_fallback = fs.read(file_inode, 0, 0, size).unwrap();
    assert_eq!(via_handle, via_fallback);
    assert_eq!(via_handle.len() as u64, size);

    fs.release_handle(fh);
    // After release, a handle read falls back (still serves correctly by inode).
    let after = fs.read(file_inode, fh, 0, size).unwrap();
    assert_eq!(after, via_fallback);
}
```

- [ ] **Step 2: Run it to confirm it fails**

Run: `cargo test -p musefs-core --test facade open_handle_read_and_release_roundtrip 2>&1 | head -20`
Expected: FAIL — `no method named open_handle` / `read` takes 3 args not 4.

- [ ] **Step 3: Add imports + handle types to `facade.rs`**

Add near the top:
```rust
use std::collections::HashMap;
use std::sync::atomic::AtomicU64;
```
Add the handle struct (above `impl Musefs` or near the struct):
```rust
/// An open file handle: the resolved layout and a backing fd opened (and
/// validated) once at `open`, reused for every `read` on this handle.
struct Handle {
    resolved: Arc<crate::reader::ResolvedFile>,
    file: Arc<std::fs::File>,
}
```
(If `ResolvedFile` is already imported via `use crate::reader::...`, use the short path. Check the existing `use` lines and match them.)

- [ ] **Step 4: Add the two fields to `Musefs`**

Update the struct (`replace_symbol_body` on `Musefs`) to add `handles` + `next_fh` (keep existing fields):
```rust
pub struct Musefs {
    pool: DbPool,
    config: MountConfig,
    tree: ArcSwap<VirtualTree>,
    cache: Mutex<HeaderCache>,
    last_data_version: AtomicI64,
    handles: Mutex<HashMap<u64, Arc<Handle>>>,
    next_fh: AtomicU64,
}
```
And in `Musefs::open`, initialize them:
```rust
            handles: Mutex::new(HashMap::new()),
            next_fh: AtomicU64::new(0),
```
(Add these two lines to the `Ok(Musefs { ... })` initializer.)

- [ ] **Step 5: Add `open_handle` / `release_handle` and change `read` to take `fh`**

Add a small lock helper next to `cache()` for poison recovery:
```rust
    fn handles(&self) -> std::sync::MutexGuard<'_, HashMap<u64, Arc<Handle>>> {
        self.handles.lock().unwrap_or_else(|p| p.into_inner())
    }
```
Add the methods (place after `read`):
```rust
    /// Open a file handle: resolve + validate the layout and open the backing fd
    /// once, store it, and return a non-zero handle id. Subsequent `read`s with
    /// this handle reuse the fd (no per-read open/stat).
    pub fn open_handle(&self, inode: u64) -> Result<u64> {
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
        // resolve() validates size+mtime and builds/caches the layout.
        let resolved = self.pool.with(|db| self.cache().resolve(db, track_id))?;
        crate::metrics::on_open();
        let file = std::fs::File::open(&resolved.backing_path)?;
        let fh = self.next_fh.fetch_add(1, Ordering::Relaxed) + 1; // never 0
        self.handles().insert(
            fh,
            Arc::new(Handle { resolved, file: Arc::new(file) }),
        );
        Ok(fh)
    }

    /// Drop an open handle (closes its backing fd when the last reference goes).
    pub fn release_handle(&self, fh: u64) {
        self.handles().remove(&fh);
    }
```
Replace `read` to take `fh` and serve from the handle when present:
```rust
    pub fn read(&self, inode: u64, fh: u64, offset: u64, size: u64) -> Result<Vec<u8>> {
        // Fast path: serve from the per-handle fd + cached layout (no open/stat).
        if fh != 0 {
            let handle = self.handles().get(&fh).cloned();
            if let Some(h) = handle {
                return self
                    .pool
                    .with(|db| read_at_with_file(&h.resolved, db, &h.file, offset, size));
            }
        }
        // Fallback (no prior open, or unknown handle): resolve by inode and open.
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
        self.pool.with(|db| {
            let resolved = self.cache().resolve(db, track_id)?;
            read_at(&resolved, db, offset, size)
        })
    }
```
Update the `use` line for `read_at` to also import `read_at_with_file`:
```rust
use crate::reader::{read_at, read_at_with_file, HeaderCache};
```
(Match the existing import path/style for `read_at`/`HeaderCache` in this file.)

- [ ] **Step 6: Update the `read` call sites (new `fh` arg = 0)**

- `musefs-core/tests/facade.rs`: the existing `fs.read(file_inode, 0, fattr.size)` becomes `fs.read(file_inode, 0, 0, fattr.size)` (inode, fh=0, offset=0, size). Update every `fs.read(` in this file.
- `musefs-core/tests/metrics.rs`: `fs.read(file_inode, off, chunk)` → `fs.read(file_inode, 0, off, chunk)`.
- `musefs-core/benches/read_throughput.rs`: `fs.read(file_inode, off, chunk)` → `fs.read(file_inode, 0, off, chunk)`.

- [ ] **Step 7: Run tests**

Run: `cargo test -p musefs-core --test facade open_handle_read_and_release_roundtrip`
Expected: PASS.
Run: `cargo test -p musefs-core`
Expected: PASS (all, including updated call sites).
Run: `cargo build -p musefs-core --features metrics --tests` and `cargo clippy --all-targets`
Expected: clean.

- [ ] **Step 8: Commit**

```bash
git add musefs-core/src/facade.rs musefs-core/tests/facade.rs musefs-core/tests/metrics.rs musefs-core/benches/read_throughput.rs
git commit -m "$(cat <<'EOF'
feat(core): per-handle backing fd + layout (open_handle/release_handle, read by fh)

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

## Task 2.3: FUSE `open`/`release` + use `fh` in `read`

**Files:** Modify `musefs-fuse/src/lib.rs`.

- [ ] **Step 1: Add the `ReplyOpen`/`ReplyEmpty` imports**

In the `use fuser::{...}` line add `ReplyOpen, ReplyEmpty`.

- [ ] **Step 2: Implement `open`/`release` and thread `fh` through `read`**

Inside `impl Filesystem for MusefsFs`, add `open` and `release`, and change `read` to use `fh`:
```rust
    fn open(&mut self, _req: &Request<'_>, ino: u64, _flags: i32, reply: ReplyOpen) {
        let core = Arc::clone(&self.core);
        self.pool.execute(move || match core.open_handle(ino) {
            Ok(fh) => reply.opened(fh, 0),
            Err(e) => reply.error(errno(&e)),
        });
    }

    fn release(
        &mut self,
        _req: &Request<'_>,
        _ino: u64,
        fh: u64,
        _flags: i32,
        _lock_owner: Option<u64>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        self.core.release_handle(fh);
        reply.ok();
    }
```
Change `read`'s body to pass `fh` (it currently ignores `_fh`; rename to `fh` and pass it):
```rust
    fn read(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        fh: u64,
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
            .execute(move || match core.read(ino, fh, offset as u64, size as u64) {
                Ok(bytes) => reply.data(&bytes),
                Err(e) => reply.error(errno(&e)),
            });
    }
```
(`release` is cheap — just a map remove — so it can stay inline on the dispatch thread; don't offload it.)

- [ ] **Step 3: Build + run the e2e mount tests (the real gate)**

Run: `cargo build -p musefs-fuse`
Expected: compiles (confirm `ReplyOpen`/`ReplyEmpty` exist in fuser 0.14; if `release`'s signature differs, match the trait's actual signature — do not change behavior).
Run: `cargo test -p musefs-fuse -- --ignored --test-threads=1`
Expected: 5/5 e2e mount tests pass — reads now flow through `open`→`read(fh)`→`release`, byte-identical.
Run: `cargo test` and `cargo clippy --all-targets`
Expected: pass / clean.

- [ ] **Step 4: Commit**

```bash
git add musefs-fuse/src/lib.rs
git commit -m "$(cat <<'EOF'
feat(fuse): implement open/release; serve reads from the per-handle fd

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

## Task 2.4: Metrics test — handle reuses one open, zero per-read stat

**Files:** Modify `musefs-core/tests/metrics.rs`.

- [ ] **Step 1: Add the test**

Append to `musefs-core/tests/metrics.rs` (same file as the baseline test; it's `#![cfg(feature = "metrics")]`, its own binary):
```rust
#[test]
fn handle_reuses_one_open_and_no_per_read_stat() {
    let dir = tempfile::tempdir().unwrap();
    let flac = make_flac(
        &[
            (0, streaminfo_body()),
            (4, vorbis_comment_body("v", &["ARTIST=Alice", "TITLE=Song"])),
        ],
        &[0xCD; 64 * 1024],
    );
    std::fs::write(dir.path().join("a.flac"), &flac).unwrap();

    let db = musefs_db::Db::open_in_memory().unwrap();
    scan_directory(&db, dir.path()).unwrap();
    let fs = Musefs::open(db, config()).unwrap();

    let artist = fs.lookup(VirtualTree::ROOT, "Alice").unwrap();
    let (_, file_inode, _) = fs.readdir(artist).unwrap().into_iter().next().unwrap();
    let size = fs.getattr(file_inode).unwrap().size;

    let fh = fs.open_handle(file_inode).unwrap();
    metrics::reset(); // measure only the reads, not open_handle's resolve+open
    let chunk = 16 * 1024u64;
    let mut off = 0u64;
    let mut reads = 0u64;
    while off < size {
        let got = fs.read(file_inode, fh, off, chunk).unwrap();
        if got.is_empty() {
            break;
        }
        off += got.len() as u64;
        reads += 1;
    }
    let s = metrics::snapshot();
    fs.release_handle(fh);

    assert!(reads >= 2, "expected a multi-chunk read, got {reads}");
    // The whole point of Phase 2: reads reuse the handle's fd and never stat.
    assert_eq!(s.opens, 0, "no per-read open() on the handle path");
    assert_eq!(s.stats, 0, "no per-read stat() on the handle path");
    assert_eq!(s.pread_bytes, 64 * 1024, "audio body read exactly once");
}
```

- [ ] **Step 2: Run it (compare to the fh==0 baseline test)**

Run: `cargo test -p musefs-core --features metrics --test metrics`
Expected: BOTH tests pass — `baseline_one_open_per_read_call` (fh==0 fallback: `opens == reads`) and `handle_reuses_one_open_and_no_per_read_stat` (handle path: `opens == 0`, `stats == 0` during reads).

- [ ] **Step 3: Commit**

```bash
git add musefs-core/tests/metrics.rs
git commit -m "$(cat <<'EOF'
test(core): handle path reuses one fd and does no per-read stat

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

# Phase 3 — Two-tier caching, sharding, lazy invalidation

## Task 3.1: Sharded O(1) byte-bounded layout cache; `resolve` becomes `&self` and off-lock

**Files:** Modify `musefs-core/src/reader.rs` (`HeaderCache` + new `Shard`); `musefs-core/src/facade.rs` (cache field type, drop `cache()` helper, call sites); update `let mut cache` → `let cache` in `tests/reader.rs`, `tests/read_at.rs`, and reader inline tests; rework `cache_bound_tests`.

- [ ] **Step 1: Write the new eviction + concurrency tests (reader inline tests)**

Replace the existing `cache_bound_tests` module body (it currently tests the `Vec`-order LRU) with sharded-aware tests. Find `mod cache_bound_tests` in `reader.rs` and replace its contents:
```rust
#[cfg(test)]
mod cache_bound_tests {
    use super::*;

    // Build a resolved entry of a known cache_bytes cost without touching disk.
    fn entry(content_version: i64, inline_len: usize) -> Arc<ResolvedFile> {
        Arc::new(ResolvedFile {
            layout: RegionLayout::new(vec![Segment::Inline(vec![0u8; inline_len])]),
            total_len: inline_len as u64,
            content_version,
            backing_path: std::path::PathBuf::from("/nonexistent"),
            mtime_secs: 0,
            ogg_index: once_cell::sync::OnceCell::new(),
            cache_bytes: inline_len as u64,
        })
    }

    #[test]
    fn shard_evicts_least_recently_used_over_byte_budget() {
        // A single shard with a tiny budget evicts the LRU entry.
        let mut shard = Shard::new(100);
        shard.insert(1, entry(0, 60));
        shard.insert(2, entry(0, 60)); // 120 > 100 → evict key 1 (LRU)
        assert!(shard.get(1).is_none());
        assert!(shard.get(2).is_some());

        // Touch 2, insert 3 → 2 is recent, 3 newest; with budget 100 one is evicted.
        shard.insert(3, entry(0, 60));
        assert!(shard.get(3).is_some());
    }

    #[test]
    fn header_cache_resolve_caches_by_content_version() {
        // Uses the public resolve path against an in-memory FLAC.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.flac");
        let (audio_offset, audio_length) = crate::reader::cache_bound_tests::write_flac_local(&path);
        let db = Db::open_in_memory().unwrap();
        let meta = std::fs::metadata(&path).unwrap();
        let id = db
            .upsert_track(&NewTrack {
                backing_path: path.to_string_lossy().to_string(),
                format: Format::Flac,
                audio_offset,
                audio_length,
                backing_size: meta.len() as i64,
                backing_mtime: mtime_secs(&meta),
            })
            .unwrap();
        let cache = HeaderCache::new(Mode::Synthesis);
        let a = cache.resolve(&db, id).unwrap();
        let b = cache.resolve(&db, id).unwrap();
        assert!(Arc::ptr_eq(&a, &b)); // cache hit returns the same Arc
    }

    // Minimal local FLAC writer (STREAMINFO + comment + audio) to avoid the
    // tests/common helper (not visible from inline tests).
    pub(super) fn write_flac_local(path: &std::path::Path) -> (i64, i64) {
        fn block(bt: u8, body: &[u8], last: bool) -> Vec<u8> {
            let mut v = vec![(if last { 0x80 } else { 0 }) | (bt & 0x7F)];
            let n = body.len();
            v.extend_from_slice(&[(n >> 16) as u8, (n >> 8) as u8, n as u8]);
            v.extend_from_slice(body);
            v
        }
        let mut si = vec![
            0x10, 0x00, 0x10, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x0A, 0xC4, 0x42, 0xF0,
            0x00, 0x00, 0x00, 0x00,
        ];
        si.extend_from_slice(&[0u8; 16]);
        let mut vc = Vec::new();
        let vendor = b"x";
        vc.extend_from_slice(&(vendor.len() as u32).to_le_bytes());
        vc.extend_from_slice(vendor);
        vc.extend_from_slice(&0u32.to_le_bytes()); // 0 comments
        let mut out = b"fLaC".to_vec();
        out.extend(block(0, &si, false));
        out.extend(block(4, &vc, true));
        let audio = [0xABu8; 256];
        let audio_offset = out.len() as i64;
        out.extend_from_slice(&audio);
        std::fs::write(path, &out).unwrap();
        (audio_offset, audio.len() as i64)
    }
}
```
(If `Db`, `NewTrack`, `Format`, `mtime_secs`, `RegionLayout`, `Segment` aren't already in scope for this module, add the needed `use super::*;`/imports mirroring the other inline test modules in `reader.rs`.)

- [ ] **Step 2: Run to confirm failure**

Run: `cargo test -p musefs-core cache_bound_tests 2>&1 | head -30`
Expected: FAIL — `Shard` not found; `resolve` not callable on `&self`.

- [ ] **Step 3: Replace `HeaderCache` with the sharded design + add `Shard`**

In `reader.rs`, replace the `HeaderCache` struct and add `Shard`. Use `replace_symbol_body` on `HeaderCache` (struct), then `replace_symbol_body` on the whole `impl HeaderCache`. Add the `Shard` type just above `HeaderCache`.

```rust
const CACHE_SHARDS: usize = 16;

/// One LRU shard: a hand-rolled O(1) doubly-linked-list keyed by track id, with a
/// byte budget. `head` is most-recently-used, `tail` least.
struct LruNode {
    value: Arc<ResolvedFile>,
    prev: Option<i64>,
    next: Option<i64>,
}

pub(crate) struct Shard {
    map: HashMap<i64, LruNode>,
    head: Option<i64>,
    tail: Option<i64>,
    bytes: u64,
    budget: u64,
}

impl Shard {
    pub(crate) fn new(budget: u64) -> Shard {
        Shard { map: HashMap::new(), head: None, tail: None, bytes: 0, budget }
    }

    fn unlink(&mut self, key: i64) {
        let (prev, next) = {
            let n = &self.map[&key];
            (n.prev, n.next)
        };
        match prev {
            Some(p) => self.map.get_mut(&p).unwrap().next = next,
            None => self.head = next,
        }
        match next {
            Some(nx) => self.map.get_mut(&nx).unwrap().prev = prev,
            None => self.tail = prev,
        }
        let n = self.map.get_mut(&key).unwrap();
        n.prev = None;
        n.next = None;
    }

    fn push_front(&mut self, key: i64) {
        let old = self.head;
        self.map.get_mut(&key).unwrap().next = old;
        if let Some(h) = old {
            self.map.get_mut(&h).unwrap().prev = Some(key);
        }
        self.head = Some(key);
        if self.tail.is_none() {
            self.tail = Some(key);
        }
    }

    pub(crate) fn get(&mut self, key: i64) -> Option<Arc<ResolvedFile>> {
        if !self.map.contains_key(&key) {
            return None;
        }
        self.unlink(key);
        self.push_front(key);
        Some(self.map[&key].value.clone())
    }

    pub(crate) fn insert(&mut self, key: i64, value: Arc<ResolvedFile>) {
        let add = value.cache_bytes;
        if self.map.contains_key(&key) {
            self.unlink(key);
            let node = self.map.get_mut(&key).unwrap();
            self.bytes -= node.value.cache_bytes;
            node.value = value;
        } else {
            self.map.insert(key, LruNode { value, prev: None, next: None });
        }
        self.bytes += add;
        self.push_front(key);
        while self.bytes > self.budget && self.map.len() > 1 {
            let lru = self.tail.unwrap();
            self.unlink(lru);
            let n = self.map.remove(&lru).unwrap();
            self.bytes -= n.value.cache_bytes;
        }
    }

    fn clear(&mut self) {
        self.map.clear();
        self.head = None;
        self.tail = None;
        self.bytes = 0;
    }

    fn retain_keys(&mut self, live: &std::collections::HashSet<i64>) {
        let dead: Vec<i64> = self.map.keys().copied().filter(|k| !live.contains(k)).collect();
        for k in dead {
            self.unlink(k);
            if let Some(n) = self.map.remove(&k) {
                self.bytes -= n.value.cache_bytes;
            }
        }
    }
}

/// A per-mount cache of resolved files, sharded for concurrency and keyed by track
/// id; an entry self-invalidates when the track's `content_version` changes.
pub struct HeaderCache {
    shards: Vec<Mutex<Shard>>,
    mode: Mode,
}
```
Add the `use` for `Mutex` and `HashSet` at the top of `reader.rs` if not present:
```rust
use std::collections::{HashMap, HashSet};
use std::sync::Mutex;
```
(`HashMap` is likely already imported; add `HashSet` and `Mutex`.)

Now replace `impl HeaderCache` (the whole block) so methods take `&self` and `resolve` validates/synthesizes off-lock:
```rust
impl HeaderCache {
    pub fn new(mode: Mode) -> HeaderCache {
        HeaderCache::with_budget(mode, DEFAULT_CACHE_BUDGET)
    }

    pub fn with_budget(mode: Mode, budget: u64) -> HeaderCache {
        let per_shard = (budget / CACHE_SHARDS as u64).max(1);
        let shards = (0..CACHE_SHARDS).map(|_| Mutex::new(Shard::new(per_shard))).collect();
        HeaderCache { shards, mode }
    }

    fn shard(&self, track_id: i64) -> std::sync::MutexGuard<'_, Shard> {
        let idx = (track_id as u64 % CACHE_SHARDS as u64) as usize;
        self.shards[idx].lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Drop all cached resolutions (used when the DB changed underneath the mount).
    pub fn clear(&self) {
        for s in &self.shards {
            s.lock().unwrap_or_else(|p| p.into_inner()).clear();
        }
    }

    /// Drop cached resolutions for tracks no longer present (`live` = current ids).
    pub fn retain(&self, live: &HashSet<i64>) {
        for s in &self.shards {
            s.lock().unwrap_or_else(|p| p.into_inner()).retain_keys(live);
        }
    }

    /// Resolve a track to its layout, caching on a content-version miss. Validation
    /// (`stat`) and synthesis (front-read/parse) run WITHOUT holding the shard lock;
    /// the lock is taken only briefly for the cache get and insert.
    pub fn resolve(&self, db: &Db, track_id: i64) -> Result<Arc<ResolvedFile>> {
        let track = db
            .get_track(track_id)?
            .ok_or(CoreError::TrackNotFound(track_id))?;

        // Validate the backing file (no lock held).
        crate::metrics::on_stat();
        let meta = std::fs::metadata(&track.backing_path)?;
        if meta.len() != track.backing_size as u64 || mtime_secs(&meta) != track.backing_mtime {
            return Err(CoreError::BackingChanged(track.backing_path.clone()));
        }

        // Fast path: brief lock for the cache get.
        if let Some(hit) = self.shard(track_id).get(track_id) {
            if hit.content_version == track.content_version {
                return Ok(hit);
            }
        }

        // Build the layout (no lock held).
        let resolved = self.build(db, &track, &meta)?;

        // Insert (brief lock). Concurrent misses may both build; last insert wins,
        // both callers get a valid Arc.
        self.shard(track_id).insert(track_id, resolved.clone());
        Ok(resolved)
    }

    /// Build a `ResolvedFile` for `track` (synthesis or passthrough). No lock held.
    fn build(
        &self,
        db: &Db,
        track: &musefs_db::Track,
        meta: &std::fs::Metadata,
    ) -> Result<Arc<ResolvedFile>> {
        let (layout, total_len, mtime_secs_val) = match self.mode {
            Mode::StructureOnly => {
                let layout = RegionLayout::new(vec![Segment::BackingAudio {
                    offset: 0,
                    len: meta.len(),
                }]);
                (layout, meta.len(), track.backing_mtime)
            }
            Mode::Synthesis => {
                if track.audio_offset < 0
                    || track.audio_length < 0
                    || (track.audio_offset as u64).saturating_add(track.audio_length as u64)
                        > meta.len()
                {
                    return Err(CoreError::BackingChanged(track.backing_path.clone()));
                }
                let tags = db.get_tags(track.id)?;
                let inputs = tags_to_inputs(&tags);
                let art_inputs = track_art_to_inputs(db, track.id)?;
                let layout = match track.format {
                    Format::Flac => {
                        let front =
                            read_front(Path::new(&track.backing_path), track.audio_offset as u64)?;
                        let fmeta = flac::read_metadata(&front)?;
                        let scan = FlacScan {
                            audio_offset: track.audio_offset as u64,
                            audio_length: track.audio_length as u64,
                            preserved: fmeta.preserved,
                        };
                        flac::synthesize_layout(&scan, &inputs, &art_inputs)?
                    }
                    Format::Mp3 => mp3::synthesize_layout(
                        track.audio_offset as u64,
                        track.audio_length as u64,
                        &inputs,
                        &art_inputs,
                    )?,
                    Format::M4a => {
                        let bytes = std::fs::read(&track.backing_path)?;
                        let scan = mp4::read_structure(&bytes)?;
                        mp4::synthesize_layout(&scan, &inputs, &art_inputs)?
                    }
                    Format::Opus | Format::Vorbis | Format::OggFlac => {
                        let front =
                            read_front(Path::new(&track.backing_path), track.audio_offset as u64)?;
                        let header = musefs_format::ogg::read_metadata(&front)?;
                        let art_images = crate::mapping::track_art_images(db, &art_inputs)?;
                        let arts: Vec<musefs_format::ogg::OggArt> = art_inputs
                            .iter()
                            .zip(art_images.iter())
                            .map(|(meta, image)| musefs_format::ogg::OggArt {
                                meta,
                                image: image.as_slice(),
                            })
                            .collect();
                        musefs_format::ogg::synthesize_layout(
                            &header,
                            track.audio_offset as u64,
                            track.audio_length as u64,
                            &inputs,
                            &arts,
                        )?
                    }
                };
                let total = layout.total_len();
                (layout, total, track.backing_mtime.max(track.updated_at))
            }
        };
        let cache_bytes = layout
            .segments()
            .iter()
            .map(|s| match s {
                Segment::Inline(b) => b.len() as u64,
                _ => 0,
            })
            .sum();
        Ok(Arc::new(ResolvedFile {
            layout,
            total_len,
            content_version: track.content_version,
            backing_path: PathBuf::from(&track.backing_path),
            mtime_secs: mtime_secs_val,
            ogg_index: OnceCell::new(),
            cache_bytes,
        }))
    }
}
```
(Ensure `musefs_db::Track` is importable — add `use musefs_db::Track;` if the file doesn't already import it, or use the full path as shown.)

- [ ] **Step 4: Update `facade.rs` to the lock-free cache**

The cache is now internally synchronized, so drop the outer `Mutex` and the `cache()` helper.
- Struct field: `cache: HeaderCache` (was `cache: Mutex<HeaderCache>`).
- In `Musefs::open`: `cache: HeaderCache::new(config.mode)` (drop `Mutex::new(...)`).
- Delete the `cache()` helper method.
- Replace `self.cache().resolve(db, id)` → `self.cache.resolve(db, id)` (in `getattr`, `read`, `open_handle`).
- Replace `self.cache().clear()` → `self.cache.clear()` (in `poll_refresh`).
- Update the lock-order comment to read:
```rust
    // Lock order: acquire a DbPool connection (`pool.with`/`with_poll`) before any
    // cache shard lock. The header cache is internally sharded; `resolve` does its
    // stat/synthesis off-lock and locks a shard only for the get/insert.
```

- [ ] **Step 5: Update remaining `let mut cache` bindings**

Search `musefs-core` for `let mut cache = HeaderCache` and `cache.resolve` on a `mut` binding (in `tests/reader.rs`, `tests/read_at.rs`, and reader inline test modules) and change `let mut cache` → `let cache` (resolve is now `&self`). Run `grep -rn "mut cache" musefs-core` to find them all.

- [ ] **Step 6: Build + run everything**

Run: `cargo test -p musefs-core`
Expected: PASS (sharded eviction test, content-version cache test, all existing reader/read_at tests).
Run: `cargo test -p musefs-fuse -- --ignored --test-threads=1`
Expected: 5/5 e2e pass.
Run: `cargo build -p musefs-core --features metrics --tests` and `cargo clippy --all-targets`
Expected: clean.

- [ ] **Step 7: Commit**

```bash
git add musefs-core/src/reader.rs musefs-core/src/facade.rs musefs-core/tests/reader.rs musefs-core/tests/read_at.rs
git commit -m "$(cat <<'EOF'
perf(core): sharded O(1) header cache; resolve validates+synthesizes off-lock

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

## Task 3.2: Size/attr cache — cheap `getattr` (skip stat + synthesis on hit)

**Files:** Modify `musefs-core/src/facade.rs`; add a metrics test to `musefs-core/tests/metrics.rs`.

- [ ] **Step 1: Write the failing metrics test (getattr hit does no stat)**

Append to `musefs-core/tests/metrics.rs`:
```rust
#[test]
fn getattr_size_cache_hit_skips_stat() {
    let dir = tempfile::tempdir().unwrap();
    let flac = make_flac(
        &[
            (0, streaminfo_body()),
            (4, vorbis_comment_body("v", &["ARTIST=Alice", "TITLE=Song"])),
        ],
        &[0xCD; 4096],
    );
    std::fs::write(dir.path().join("a.flac"), &flac).unwrap();
    let db = musefs_db::Db::open_in_memory().unwrap();
    scan_directory(&db, dir.path()).unwrap();
    let fs = Musefs::open(db, config()).unwrap();
    let artist = fs.lookup(VirtualTree::ROOT, "Alice").unwrap();
    let (_, file_inode, _) = fs.readdir(artist).unwrap().into_iter().next().unwrap();

    let first = fs.getattr(file_inode).unwrap(); // miss → resolve → stat
    metrics::reset();
    let second = fs.getattr(file_inode).unwrap(); // hit → size cache, no stat
    let s = metrics::snapshot();

    assert_eq!(first.size, second.size);
    assert_eq!(s.stats, 0, "a warm getattr must not stat the backing file");
}
```

- [ ] **Step 2: Run to confirm failure**

Run: `cargo test -p musefs-core --features metrics --test metrics getattr_size_cache_hit_skips_stat 2>&1 | head -20`
Expected: FAIL — `s.stats` is 1 (current `getattr` always resolves → stats).

- [ ] **Step 3: Add the size cache + use it in `getattr`**

In `facade.rs`, add a field to `Musefs`:
```rust
    /// (content_version, total_len, mtime_secs) keyed by track id. Tiny entries,
    /// effectively unbounded; serves getattr/lookup without a backing stat or
    /// full synthesis. Self-invalidates on a content_version change.
    size_cache: Mutex<HashMap<i64, (i64, u64, i64)>>,
```
Initialize in `open`: `size_cache: Mutex::new(HashMap::new()),`.
Add a poison-recovery helper next to `handles()`:
```rust
    fn size_cache(&self) -> std::sync::MutexGuard<'_, HashMap<i64, (i64, u64, i64)>> {
        self.size_cache.lock().unwrap_or_else(|p| p.into_inner())
    }
```
Replace `getattr` to consult the size cache:
```rust
    pub fn getattr(&self, inode: u64) -> Result<Attr> {
        let track_id = {
            let tree = self.tree.load();
            match tree.node(inode) {
                None => return Err(CoreError::NoEntry(inode)),
                Some(node) => match &node.kind {
                    NodeKind::Dir => {
                        return Ok(Attr { inode, is_dir: true, size: 0, mtime_secs: 0 })
                    }
                    NodeKind::File { track_id } => *track_id,
                },
            }
        };
        let (size, mtime_secs) = self.pool.with(|db| {
            // Cheap, indexed: current content_version drives lazy invalidation.
            let track = db
                .get_track(track_id)?
                .ok_or(CoreError::TrackNotFound(track_id))?;
            if let Some(&(cv, len, mt)) = self.size_cache().get(&track_id) {
                if cv == track.content_version {
                    return Ok((len, mt)); // hit: no backing stat, no synthesis
                }
            }
            // Miss: full resolve (validates via stat, builds + caches the layout).
            let resolved = self.cache.resolve(db, track_id)?;
            self.size_cache()
                .insert(track_id, (track.content_version, resolved.total_len, resolved.mtime_secs));
            Ok((resolved.total_len, resolved.mtime_secs))
        })?;
        Ok(Attr { inode, is_dir: false, size, mtime_secs })
    }
```

- [ ] **Step 4: Run the test + suite**

Run: `cargo test -p musefs-core --features metrics --test metrics`
Expected: PASS — `getattr_size_cache_hit_skips_stat` (warm getattr → `stats == 0`) plus the earlier metrics tests.
Run: `cargo test` and `cargo test -p musefs-fuse -- --ignored --test-threads=1` and `cargo clippy --all-targets`
Expected: pass / 5 e2e pass / clean.

- [ ] **Step 5: Commit**

```bash
git add musefs-core/src/facade.rs musefs-core/tests/metrics.rs
git commit -m "$(cat <<'EOF'
perf(core): size/attr cache makes warm getattr skip the backing stat + synthesis

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

## Task 3.3: Lazy invalidation — keep caches warm across refresh, prune vanished tracks

**Files:** Modify `musefs-core/src/facade.rs`; add a lazy-invalidation test to `musefs-core/tests/facade.rs`.

- [ ] **Step 1: Add the test (cache stays warm; vanished track pruned)**

Add to `musefs-core/tests/facade.rs` (file-backed DB so `data_version` polling works, mirroring `poll_refresh_picks_up_external_db_edits`):
```rust
#[test]
fn poll_refresh_keeps_unchanged_entries_and_prunes_vanished() {
    use musefs_db::{Format, NewTrack, Tag};
    let dir = tempfile::tempdir().unwrap();
    let backing = dir.path().join("a.flac");
    // Real FLAC so resolve() succeeds.
    let bytes = make_flac(
        &[
            (0, streaminfo_body()),
            (4, vorbis_comment_body("v", &["ARTIST=Alice", "TITLE=Song"])),
        ],
        &[0xAB; 64],
    );
    std::fs::write(&backing, &bytes).unwrap();
    let db_path = dir.path().join("m.db");
    {
        let db = musefs_db::Db::open(&db_path).unwrap();
        scan_directory(&db, dir.path()).unwrap();
    }
    let fs = Musefs::open(musefs_db::Db::open(&db_path).unwrap(), config()).unwrap();
    let artist = fs.lookup(VirtualTree::ROOT, "Alice").unwrap();
    let (_, inode, _) = fs.readdir(artist).unwrap().into_iter().next().unwrap();
    let size_before = fs.getattr(inode).unwrap().size;

    // An unrelated external commit bumps data_version without changing this track.
    {
        let db2 = musefs_db::Db::open(&db_path).unwrap();
        let id = db2
            .upsert_track(&NewTrack {
                backing_path: "/x/ghost.mp3".to_string(),
                format: Format::Mp3,
                audio_offset: 0,
                audio_length: 0,
                backing_size: 0,
                backing_mtime: 0,
            })
            .unwrap();
        db2.replace_tags(id, &[Tag::new("artist", "Ghost", 0), Tag::new("title", "G", 0)])
            .unwrap();
    }
    assert!(fs.poll_refresh().unwrap()); // notices the commit, rebuilds tree

    // The unchanged track's layout/size are still served correctly after refresh.
    let size_after = fs.getattr(inode).unwrap().size;
    assert_eq!(size_before, size_after);
    assert!(fs.lookup(VirtualTree::ROOT, "Ghost").is_some());
}
```

- [ ] **Step 2: Run it (passes today via wholesale clear, but we want lazy + pruning)**

Run: `cargo test -p musefs-core --test facade poll_refresh_keeps_unchanged_entries_and_prunes_vanished`
Expected: PASS even before the change (correctness is preserved either way). This test guards that the *lazy* version (next step) keeps correctness — it must still pass after Step 3. (It documents behavior; the warm-cache benefit is verified via metrics in Task 3.4.)

- [ ] **Step 3: Make `poll_refresh` lazy + prune by live track set**

Replace `poll_refresh` so it no longer wholesale-clears; instead it prunes both caches to the live track-id set from the rebuilt tree. Add a helper on `VirtualTree` access — but the tree already exposes nodes; collect live ids from the new tree via a small core helper. Implement inside `poll_refresh`:
```rust
    pub fn poll_refresh(&self) -> Result<bool> {
        let version = self.pool.with_poll(|db| Ok(db.data_version()?))?;
        if version == self.last_data_version.load(Ordering::Acquire) {
            return Ok(false);
        }
        // Rebuild the tree, then prune caches to the live track set (entries that
        // remain are kept warm; a changed track self-invalidates on next resolve
        // via its content_version). Commit the stamp only after a successful rebuild.
        self.refresh()?;
        let live = self.live_track_ids();
        self.cache.retain(&live);
        self.size_cache().retain(|k, _| live.contains(k));
        self.last_data_version.store(version, Ordering::Release);
        Ok(true)
    }

    /// The set of track ids currently present in the tree (file nodes).
    fn live_track_ids(&self) -> std::collections::HashSet<i64> {
        self.tree.load().track_ids()
    }
```
This needs `VirtualTree::track_ids()`. Add it to `tree.rs`:
```rust
    /// All track ids referenced by file nodes (used to prune stale cache entries).
    pub fn track_ids(&self) -> std::collections::HashSet<i64> {
        self.nodes
            .values()
            .filter_map(|n| match &n.kind {
                NodeKind::File { track_id } => Some(*track_id),
                NodeKind::Dir => None,
            })
            .collect()
    }
```
(Match `tree.rs`'s actual field name for the node map — inspect `VirtualTree` with `get_symbols_overview`/`find_symbol` first; it may be `nodes`, `by_inode`, etc. Use the real field and `NodeKind` path.)

Also import `HashSet` in `facade.rs` if needed (`use std::collections::{HashMap, HashSet};`).

- [ ] **Step 4: Run tests**

Run: `cargo test -p musefs-core`
Expected: PASS, including `poll_refresh_keeps_unchanged_entries_and_prunes_vanished` and the existing `poll_refresh_picks_up_external_db_edits`.
Run: `cargo test -p musefs-fuse -- --ignored --test-threads=1` and `cargo clippy --all-targets`
Expected: 5 e2e pass / clean.

- [ ] **Step 5: Commit**

```bash
git add musefs-core/src/facade.rs musefs-core/src/tree.rs musefs-core/tests/facade.rs
git commit -m "$(cat <<'EOF'
perf(core): lazy cache invalidation across refresh; prune vanished tracks only

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

## Task 3.4: Warm-cache metrics test across refresh

**Files:** Modify `musefs-core/tests/metrics.rs`.

- [ ] **Step 1: Add the test (an unchanged track's layout survives a refresh)**

Append to `musefs-core/tests/metrics.rs`:
```rust
#[test]
fn layout_cache_survives_unrelated_refresh() {
    use musefs_db::{Format, NewTrack, Tag};
    let dir = tempfile::tempdir().unwrap();
    let backing = dir.path().join("a.flac");
    let bytes = make_flac(
        &[
            (0, streaminfo_body()),
            (4, vorbis_comment_body("v", &["ARTIST=Alice", "TITLE=Song"])),
        ],
        &[0xCD; 8192],
    );
    std::fs::write(&backing, &bytes).unwrap();
    let db_path = dir.path().join("m.db");
    {
        let db = musefs_db::Db::open(&db_path).unwrap();
        scan_directory(&db, dir.path()).unwrap();
    }
    let fs = Musefs::open(musefs_db::Db::open(&db_path).unwrap(), config()).unwrap();
    let artist = fs.lookup(VirtualTree::ROOT, "Alice").unwrap();
    let (_, inode, _) = fs.readdir(artist).unwrap().into_iter().next().unwrap();

    let fh = fs.open_handle(inode).unwrap(); // warms the layout cache
    fs.release_handle(fh);

    // Unrelated external commit + refresh (does NOT touch Alice's track).
    {
        let db2 = musefs_db::Db::open(&db_path).unwrap();
        let id = db2
            .upsert_track(&NewTrack {
                backing_path: "/x/ghost.mp3".to_string(),
                format: Format::Mp3,
                audio_offset: 0,
                audio_length: 0,
                backing_size: 0,
                backing_mtime: 0,
            })
            .unwrap();
        db2.replace_tags(id, &[Tag::new("artist", "Ghost", 0), Tag::new("title", "G", 0)])
            .unwrap();
    }
    assert!(fs.poll_refresh().unwrap());

    // Re-open Alice's handle: the layout cache entry survived (content_version
    // unchanged), so resolve hits the cache — only the single open_handle fd open,
    // and the front-read synthesis does NOT recur (no extra opens from read_front).
    metrics::reset();
    let fh2 = fs.open_handle(inode).unwrap();
    let s = metrics::snapshot();
    fs.release_handle(fh2);
    // open_handle does exactly one backing open; a cache hit means no front-read
    // open from re-synthesis (FLAC synthesis would open the front again on a miss).
    assert_eq!(s.opens, 1, "warm cache: only the handle fd open, no re-synthesis open");
}
```

- [ ] **Step 2: Run it**

Run: `cargo test -p musefs-core --features metrics --test metrics layout_cache_survives_unrelated_refresh`
Expected: PASS — `opens == 1` proves the FLAC front-read (re-synthesis) did not recur after the refresh, i.e. the layout cache stayed warm. (With wholesale-clear, this would be `2`: the front-read open + the handle open.)

- [ ] **Step 3: Run the whole metrics suite + clippy**

Run: `cargo test -p musefs-core --features metrics --test metrics`
Expected: all metrics tests pass.
Run: `cargo clippy --all-targets --features metrics`
Expected: no warnings.

- [ ] **Step 4: Commit**

```bash
git add musefs-core/tests/metrics.rs
git commit -m "$(cat <<'EOF'
test(core): layout cache stays warm across an unrelated refresh

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Self-Review notes (addressed)

- **Spec coverage:** Phase 2 (handle lifecycle: `open`/`release`, per-handle fd+layout, no per-read open/stat) → Tasks 2.1–2.4. Phase 3 (size/attr cache → 3.2; O(1) LRU + sharding → 3.1; lazy invalidation + prune-vanished → 3.3) → Tasks 3.1–3.4. The Phase-1 final-review carry-forward ("move stat/front-read out from under the cache lock") is Task 3.1's `resolve` restructure.
- **Out of scope (later phases, per spec):** Phase 4 (incremental refresh, debounce/single-flight `poll_refresh`, stable inodes), Phase 5 (kernel/mount tuning, worker-queue back-pressure), Phase 6 (M4A bounded `moov` read — still `std::fs::read` here). The unbounded worker queue noted in Phase 1 remains a Phase-5 item.
- **Behavior change (documented):** `getattr` size-cache hit skips the backing `stat`; backing changes are caught at scan (`data_version`) and at `open` (handle validation). Stated at the top and in `getattr`.
- **Type consistency:** `read_at_with_file(resolved, db, file, offset, size)`; `Handle { resolved, file }`; `Musefs::{open_handle, release_handle, read(ino, fh, offset, size)}`; `HeaderCache::{resolve(&self), clear(&self), retain(&self, &HashSet<i64>)}`, `Shard::{new, get, insert}`; `Musefs.size_cache: Mutex<HashMap<i64,(i64,u64,i64)>>`; `VirtualTree::track_ids() -> HashSet<i64>` are used consistently across tasks.
- **Caller-impact:** `read_at` keeps its signature (wrapper), so existing read_at callers are untouched; `Musefs::read` gains an `fh` arg, so its call sites (facade test, metrics test, bench) are updated in Task 2.2; `HeaderCache::resolve` becomes `&self`, so `let mut cache` bindings are updated in Task 3.1.
- **Verify-at-implementation:** the exact `VirtualTree` node-map field name + `NodeKind` path (Task 3.3) and fuser 0.14's `release` signature (Task 2.3) must be matched against the real code — both are called out in the steps.
