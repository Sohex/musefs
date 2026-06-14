# Single-stream backing read-ahead Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Overlap/amortize backing-read latency on single sequential synthesis-mode streams so high-RTT NFS throughput stops collapsing to `chunk_count × RTT` (issue #255).

**Architecture:** A per-handle backing read-ahead buffer caches *raw backing-file bytes keyed by absolute backing-file offset* and is consulted by every backing read (PCM `BackingAudio` and Ogg `serve_ogg_window` alike) through a single `BackingReader::read_exact_at`. An adaptive window grows geometrically on sequential access and resets on seek; all buffers draw from one global byte budget (`ReadAheadPool`) with `try_lock` LRU eviction. Phase 1 fills the window synchronously with one large `pread` (read amplification); Phase 2 adds background prefetch workers that fill ahead of the kernel position to fully hide RTT. Serving still flows through the existing per-read `validate_opened_backing`, so the cardinal audio-bytes invariant and retag/refresh semantics are untouched.

**Tech Stack:** Rust, `musefs-core` crate (the integration layer). Synchronous `threadpool::ThreadPool` model (no async). `std::os::unix::fs::FileExt::read_exact_at` for positioned reads. `sharded_slab` (existing handle store), `Mutex`/`AtomicU64` for buffer + budget.

**Source spec:** `docs/superpowers/specs/2026-06-14-read-ahead-overlap-design.md`

**Phase boundary:** Tasks 1–11 are Phase 1 (synchronous amplification) and must land as a coherent, shippable unit. Tasks 12–15 are Phase 2 (parallel prefetch). Tasks 16–17 are rollout (bench + docs) and apply after Phase 1 (re-run after Phase 2). Each task ends in a green commit; the pre-commit hook runs the full workspace test suite.

**Project gotchas every task must respect:**
- Editing `reader.rs` / `facade.rs` / `ogg_index.rs` shifts the `.cargo/mutants.toml` line:col anchors → re-anchor in the SAME commit via each entry's `# guard:` tag (run `python3 scripts/check_mutant_anchors.py --fix` if present, else hand-fix; the pre-commit hook gate is `check_mutant_anchors.py`).
- The CI `metrics` feature is NOT exercised by `cargo test --workspace`. After any read-path change run `cargo test -p musefs-core --features metrics`.
- The workspace denies `unsafe_code` even in tests — use safe `std`/`rustix` wrappers only.
- The out-of-workspace `fuzz/` crate isn't built by `cargo test`; if a `musefs-format` signature changes run `cargo +nightly fuzz build`. (This plan does not change format-layer signatures.)

---

## File Structure

- **Create `musefs-core/src/readahead.rs`** — the entire read-ahead subsystem: `ReadAhead` (per-handle window state + decision table), `ReadAheadPool` (global budget + eviction registry), `BackingReader` (the `read_exact_at` shim wrapping fd + buffer + pool), and the Phase-2 prefetch ring/worker. One module, one responsibility (read-ahead), self-contained and heavily unit-tested.
- **Modify `musefs-core/src/lib.rs`** — `mod readahead;` + re-exports.
- **Modify `musefs-core/src/reader.rs`** — route the `BackingAudio` arm and the `file: Option<&File>` plumbing through `Option<&BackingReader>`.
- **Modify `musefs-core/src/ogg_index.rs`** — `serve_ogg_window` / `read_counted` / `find_page_start` / `page_crc_ok` take `&BackingReader`.
- **Modify `musefs-core/src/facade.rs`** — `Handle` gains the buffer + epoch; `Musefs` gains the `Arc<ReadAheadPool>`; `read_into` builds a `BackingReader`; `open_handle`/`release_handle` register/deregister; `MountConfig` gains the budget; epoch bumps on generation change / seek / release.
- **Modify `musefs-core/src/metrics.rs`** — add read-ahead hit/miss counters.
- **Modify `musefs-cli/src/lib.rs`** — `--read-ahead-budget-mib` flag → `MountConfig`.
- **Modify `BENCHMARKS.md`, `ARCHITECTURE.md`, `docs/OGG.md`, `README.md`, `benches/storage_tunables_bench.sh`** — rollout.

---

## PHASE 1 — synchronous read amplification

### Task 1: `ReadAhead` window core (decision table, no budget)

**Files:**
- Create: `musefs-core/src/readahead.rs`
- Modify: `musefs-core/src/lib.rs`

- [ ] **Step 1: Register the module**

In `musefs-core/src/lib.rs`, add alongside the other `mod` lines:

```rust
mod readahead;
```

- [ ] **Step 2: Write the failing tests**

Create `musefs-core/src/readahead.rs` with ONLY the tests first (so it fails to compile → fails):

```rust
//! Per-handle backing read-ahead: an adaptive window over raw backing-file
//! bytes, a global byte budget with eviction, and the `BackingReader` shim that
//! every backing read flows through. See
//! `docs/superpowers/specs/2026-06-14-read-ahead-overlap-design.md`.

use std::io;

/// Floor window size: a fresh or just-seeked stream still reads this much ahead.
pub const WINDOW_FLOOR: u64 = 512 * 1024;
/// Absolute per-stream window cap, independent of the global budget.
pub const WINDOW_ABS_CAP: u64 = 8 * 1024 * 1024;

#[cfg(test)]
mod window_tests {
    use super::*;

    /// A fake backing file: `data` is the whole file; `fill` copies from it and
    /// records each (offset, len) actually read so tests can assert pread counts.
    struct Fake {
        data: Vec<u8>,
        reads: Vec<(u64, usize)>,
    }
    impl Fake {
        fn new(len: usize) -> Self {
            let data = (0..len).map(|i| (i % 251) as u8).collect();
            Fake { data, reads: Vec::new() }
        }
        fn fill(&mut self, buf: &mut [u8], off: u64) -> io::Result<()> {
            self.reads.push((off, buf.len()));
            let o = off as usize;
            buf.copy_from_slice(&self.data[o..o + buf.len()]);
            Ok(())
        }
    }

    fn serve(ra: &mut ReadAhead, fake: &mut Fake, off: u64, len: usize) -> Vec<u8> {
        let mut dst = vec![0u8; len];
        let backing_len = fake.data.len() as u64;
        ra.read_into(&mut dst, off, backing_len, |b, o| fake.fill(b, o)).unwrap();
        dst
    }

    #[test]
    fn first_read_misses_then_sequential_reads_hit() {
        let mut fake = Fake::new(4 * 1024 * 1024);
        let mut ra = ReadAhead::new(WINDOW_ABS_CAP);
        // First 64 KiB read: a miss, fills a floor-sized window.
        let a = serve(&mut ra, &mut fake, 0, 64 * 1024);
        assert_eq!(a, fake.data[0..64 * 1024]);
        assert_eq!(fake.reads.len(), 1, "first read must fill once");
        // Next sequential 64 KiB: fully inside the window → no new pread.
        let b = serve(&mut ra, &mut fake, 64 * 1024, 64 * 1024);
        assert_eq!(b, fake.data[64 * 1024..128 * 1024]);
        assert_eq!(fake.reads.len(), 1, "sequential hit must not pread");
    }

    #[test]
    fn sequential_miss_grows_window_geometrically() {
        let mut fake = Fake::new(16 * 1024 * 1024);
        let mut ra = ReadAhead::new(WINDOW_ABS_CAP);
        // Read the full floor window, forcing a sequential miss at its end each time.
        let floor = WINDOW_FLOOR as usize;
        serve(&mut ra, &mut fake, 0, floor); // miss, window stays floor (first fill)
        serve(&mut ra, &mut fake, WINDOW_FLOOR, floor); // seq miss → window doubles
        // The second fill must have requested > floor bytes (geometric growth).
        let second_fill_len = fake.reads[1].1 as u64;
        assert!(second_fill_len > WINDOW_FLOOR, "window must grow on sequential miss");
        assert!(second_fill_len <= WINDOW_ABS_CAP, "window capped");
    }

    #[test]
    fn seek_resets_window_to_floor() {
        let mut fake = Fake::new(16 * 1024 * 1024);
        let mut ra = ReadAhead::new(WINDOW_ABS_CAP);
        serve(&mut ra, &mut fake, 0, WINDOW_FLOOR as usize);
        serve(&mut ra, &mut fake, WINDOW_FLOOR, WINDOW_FLOOR as usize); // grow
        // Seek far away → next fill is floor-sized again.
        serve(&mut ra, &mut fake, 12 * 1024 * 1024, 4096);
        let seek_fill_len = fake.reads.last().unwrap().1 as u64;
        assert_eq!(seek_fill_len, WINDOW_FLOOR, "seek resets to floor");
    }

    #[test]
    fn window_clamps_to_backing_len_at_eof() {
        let mut fake = Fake::new(700 * 1024); // smaller than abs cap
        let mut ra = ReadAhead::new(WINDOW_ABS_CAP);
        // Read near EOF: requested range valid, but a full window would overrun.
        let out = serve(&mut ra, &mut fake, 680 * 1024, 20 * 1024);
        assert_eq!(out, fake.data[680 * 1024..700 * 1024]);
        let (off, len) = fake.reads[0];
        assert!(off + len as u64 <= 700 * 1024, "fill must not read past EOF");
    }
}
```

- [ ] **Step 3: Run tests to verify they fail**

Run: `cargo test -p musefs-core readahead::window_tests 2>&1 | tail -20`
Expected: FAIL — `cannot find type ReadAhead` / `ReadAhead::new` undefined.

- [ ] **Step 4: Implement `ReadAhead`**

Insert above the `#[cfg(test)]` block in `readahead.rs`:

```rust
/// A single contiguous read-ahead window over a backing file, keyed by absolute
/// backing-file offset. NOT thread-safe; the caller wraps it in a `Mutex` (one
/// per open handle). Caches raw backing bytes only — never synthesized output —
/// so it is immune to retag and orthogonal to the DB snapshot path.
pub struct ReadAhead {
    /// Absolute backing offset of `bytes[0]`. Meaningless when `bytes` is empty.
    win_start: u64,
    /// Buffered raw backing bytes covering `[win_start, win_start + bytes.len())`.
    bytes: Vec<u8>,
    /// Backing offset just past the last served read — the sequential predictor.
    /// `u64::MAX` until the first read, so the first read is always a (seek) miss.
    next_expected: u64,
    /// Current target window size; grows geometrically on a sequential miss,
    /// resets to the floor on a seek. Bounded by `cap`.
    window: u64,
    /// Per-stream window cap (set from the budget: `min(WINDOW_ABS_CAP, budget/N)`).
    cap: u64,
}

impl ReadAhead {
    pub fn new(cap: u64) -> Self {
        ReadAhead {
            win_start: 0,
            bytes: Vec::new(),
            next_expected: u64::MAX,
            window: WINDOW_FLOOR,
            cap: cap.max(WINDOW_FLOOR),
        }
    }

    /// Bytes currently held (charged against the global budget).
    pub fn len(&self) -> u64 {
        self.bytes.len() as u64
    }

    /// Drop the buffered bytes (eviction / invalidation). Returns bytes freed.
    pub fn clear(&mut self) -> u64 {
        let freed = self.bytes.len() as u64;
        self.bytes = Vec::new();
        self.window = WINDOW_FLOOR.min(self.cap);
        self.next_expected = u64::MAX;
        freed
    }

    /// Update the per-stream cap (when the active-stream count changes the share).
    pub fn set_cap(&mut self, cap: u64) {
        self.cap = cap.max(WINDOW_FLOOR);
        if self.window > self.cap {
            self.window = self.cap;
        }
    }

    fn covers(&self, off: u64, len: usize) -> bool {
        let end = off.saturating_add(len as u64);
        !self.bytes.is_empty()
            && off >= self.win_start
            && end <= self.win_start + self.bytes.len() as u64
    }

    /// Serve `[off, off+dst.len())` into `dst`. On a hit, memcpy from the window.
    /// On a miss, `fill(window_buf, start)` does ONE positioned read of up to
    /// `window` bytes (clamped to `backing_len`) starting at `off`. `backing_len`
    /// is the backing file size; the caller guarantees `off+dst.len() <=
    /// backing_len` (the splice loop already clamps the request).
    ///
    /// Returns `(old_len, new_len)` of `self.bytes` so the caller can reconcile
    /// the global budget. A hit returns `(n, n)` (no change).
    pub fn read_into(
        &mut self,
        dst: &mut [u8],
        off: u64,
        backing_len: u64,
        mut fill: impl FnMut(&mut [u8], u64) -> io::Result<()>,
    ) -> io::Result<(u64, u64)> {
        let len = dst.len();
        if len == 0 {
            return Ok((self.bytes.len() as u64, self.bytes.len() as u64));
        }
        if self.covers(off, len) {
            let lo = (off - self.win_start) as usize;
            dst.copy_from_slice(&self.bytes[lo..lo + len]);
            self.next_expected = off + len as u64;
            let n = self.bytes.len() as u64;
            return Ok((n, n));
        }
        let old_len = self.bytes.len() as u64;
        // Sequential miss grows; a seek resets to floor.
        if off == self.next_expected {
            self.window = self.window.saturating_mul(2).min(self.cap);
        } else {
            self.window = WINDOW_FLOOR.min(self.cap);
        }
        // The window must at least cover the request, and never overrun EOF.
        let want = self.window.max(len as u64).min(backing_len - off);
        let mut buf = vec![0u8; want as usize];
        fill(&mut buf, off)?;
        dst.copy_from_slice(&buf[..len]);
        self.win_start = off;
        self.bytes = buf;
        self.next_expected = off + len as u64;
        Ok((old_len, self.bytes.len() as u64))
    }
}
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test -p musefs-core readahead::window_tests 2>&1 | tail -20`
Expected: PASS (4 tests).

- [ ] **Step 6: Commit**

```bash
git add musefs-core/src/readahead.rs musefs-core/src/lib.rs
git commit -m "feat(core): adaptive read-ahead window core (#255)"
```

---

### Task 2: `ReadAheadPool` budget + per-stream cap

**Files:**
- Modify: `musefs-core/src/readahead.rs`

- [ ] **Step 1: Write the failing tests**

Append to `readahead.rs`:

```rust
#[cfg(test)]
mod pool_budget_tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[test]
    fn disabled_pool_grants_nothing_and_reports_disabled() {
        let pool = ReadAheadPool::new(0);
        assert!(!pool.enabled());
        assert_eq!(pool.per_stream_cap(), 0);
    }

    #[test]
    fn per_stream_cap_is_budget_over_divisor_capped_by_abs() {
        // 16 MiB budget / 4 = 4 MiB, below the 8 MiB abs cap.
        let pool = ReadAheadPool::new(16 * 1024 * 1024);
        assert_eq!(pool.per_stream_cap(), 4 * 1024 * 1024);
        // Huge budget → abs cap wins.
        let big = ReadAheadPool::new(1024 * 1024 * 1024);
        assert_eq!(big.per_stream_cap(), WINDOW_ABS_CAP);
    }

    #[test]
    fn permitted_window_grants_up_to_budget_then_clamps() {
        let pool = ReadAheadPool::new(4 * 1024 * 1024);
        let buf = Arc::new(Mutex::new(ReadAhead::new(pool.per_stream_cap())));
        pool.register(1, Arc::clone(&buf));
        // Grow from 0 → 1 MiB: permitted fully, then charge the actual delta.
        assert_eq!(pool.permitted_window(1, 0, 1024 * 1024), 1024 * 1024);
        pool.reconcile(0, 1024 * 1024);
        // charged is now 1 MiB; the per-stream cap is budget/4 = 1 MiB, so a
        // request for 8 MiB is first capped to 1 MiB (== old_len) → no growth.
        assert_eq!(pool.permitted_window(1, 1024 * 1024, 8 * 1024 * 1024), 1024 * 1024);
    }
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p musefs-core readahead::pool_budget_tests 2>&1 | tail -20`
Expected: FAIL — `ReadAheadPool` undefined.

- [ ] **Step 3: Implement the pool skeleton (budget + cap + register; eviction stubbed)**

Add near the top of `readahead.rs` (after the constants):

```rust
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// Default global read-ahead budget when the operator passes no flag (64 MiB,
/// mirroring `reader::DEFAULT_CACHE_BUDGET`). `0` disables read-ahead entirely.
pub const DEFAULT_READAHEAD_BUDGET: u64 = 64 * 1024 * 1024;
/// No single stream may hold more than `budget / PER_STREAM_DIVISOR`.
const PER_STREAM_DIVISOR: u64 = 4;

struct StreamEntry {
    buf: Arc<Mutex<ReadAhead>>,
    last_served: u64,
}

/// The process-wide read-ahead allocator: one byte budget shared by all active
/// streams, with `try_lock` LRU eviction. Deadlock-free by construction — the
/// budget is a lock-free atomic, the registry lock is a leaf (released before any
/// buffer mutex), and eviction never blocks on a buffer mutex (`try_lock` + skip).
pub struct ReadAheadPool {
    /// Total RAM envelope; `0` means read-ahead is disabled.
    budget: u64,
    /// Currently charged bytes (sum of registered buffers' lengths).
    charged: AtomicU64,
    /// Active streaming handles keyed by slab key. Only sequential streams register.
    streams: Mutex<HashMap<usize, StreamEntry>>,
    /// Monotonic source for `last_served` stamps (LRU ordering).
    clock: AtomicU64,
}

impl ReadAheadPool {
    pub fn new(budget: u64) -> Self {
        ReadAheadPool {
            budget,
            charged: AtomicU64::new(0),
            streams: Mutex::new(HashMap::new()),
            clock: AtomicU64::new(0),
        }
    }

    pub fn enabled(&self) -> bool {
        self.budget > 0
    }

    /// Per-stream window cap derived from the budget.
    pub fn per_stream_cap(&self) -> u64 {
        if self.budget == 0 {
            return 0;
        }
        (self.budget / PER_STREAM_DIVISOR).min(WINDOW_ABS_CAP).max(WINDOW_FLOOR)
    }

    /// Lazily register a handle's buffer once sequential access is detected.
    pub fn register(&self, key: usize, buf: Arc<Mutex<ReadAhead>>) {
        let last_served = self.clock.fetch_add(1, Ordering::Relaxed);
        self.streams
            .lock()
            .unwrap()
            .insert(key, StreamEntry { buf, last_served });
    }

    /// Deregister on release; returns the bytes to uncharge.
    pub fn deregister(&self, key: usize) {
        let freed = {
            let mut g = self.streams.lock().unwrap();
            match g.remove(&key) {
                Some(e) => e.buf.lock().unwrap().len(),
                None => 0,
            }
        };
        if freed > 0 {
            self.charged.fetch_sub(freed, Ordering::Relaxed);
        }
    }

    /// Mark `key` as most-recently-served (LRU bump). No-op if unregistered.
    pub fn touch(&self, key: usize) {
        let stamp = self.clock.fetch_add(1, Ordering::Relaxed);
        if let Some(e) = self.streams.lock().unwrap().get_mut(&key) {
            e.last_served = stamp;
        }
    }

    /// Decide the largest window (≤ `desired`, ≤ per-stream cap) a stream may grow
    /// to right now, given a current size of `old_len`. Evicts colder OTHER
    /// streams as needed to make room for the `(window - old_len)` delta, but does
    /// NOT charge — charging happens in `reconcile` against the ACTUAL bytes read.
    /// Never blocks on a buffer mutex (`try_lock` + skip). Call only on a miss.
    pub fn permitted_window(&self, key: usize, old_len: u64, desired: u64) -> u64 {
        if self.budget == 0 {
            return 0;
        }
        let desired = desired.min(self.per_stream_cap()).max(old_len);
        let need = desired - old_len;
        loop {
            let cur = self.charged.load(Ordering::Relaxed);
            let room = self.budget.saturating_sub(cur);
            if room >= need {
                return desired;
            }
            // Try to evict the coldest OTHER stream to free room.
            match self.evict_one_coldest(key) {
                Some(_) => continue, // evict_one_coldest already uncharged it
                None => {
                    // Nothing evictable: permit only what fits now.
                    return old_len + room;
                }
            }
        }
    }

    /// Charge the budget by the ACTUAL window-size change `(old_len → new_len)`.
    /// Keeps the invariant `charged == Σ(registered buffers' bytes.len())`.
    /// `new < old` (shrink/clear) uncharges. A no-op when `old == new` (hit).
    pub fn reconcile(&self, old_len: u64, new_len: u64) {
        if new_len > old_len {
            self.charged.fetch_add(new_len - old_len, Ordering::Relaxed);
        } else if new_len < old_len {
            self.charged.fetch_sub(old_len - new_len, Ordering::Relaxed);
        }
        debug_assert!(self.charged.load(Ordering::Relaxed) <= self.budget.max(1) * 2);
    }

    /// Uncharge `bytes` directly (window cleared on invalidation/release).
    pub fn uncharge(&self, bytes: u64) {
        if bytes > 0 {
            self.charged.fetch_sub(bytes, Ordering::Relaxed);
        }
    }

    /// Find and clear the coldest registered buffer other than `except`, using
    /// `try_lock` so eviction never blocks on an in-progress read. Returns the
    /// freed byte count, or `None` if nothing was evictable this pass.
    fn evict_one_coldest(&self, except: usize) -> Option<u64> {
        // Snapshot candidates (key, last_served, buf Arc) under the registry lock,
        // then release it before touching any buffer mutex (leaf-lock rule).
        let candidates: Vec<(usize, u64, Arc<Mutex<ReadAhead>>)> = {
            let g = self.streams.lock().unwrap();
            let mut v: Vec<_> = g
                .iter()
                .filter(|(k, _)| **k != except)
                .map(|(k, e)| (*k, e.last_served, Arc::clone(&e.buf)))
                .collect();
            v.sort_by_key(|(_, ls, _)| *ls); // coldest (smallest stamp) first
            v
        };
        for (_, _, buf) in candidates {
            if let Ok(mut ra) = buf.try_lock() {
                let freed = ra.clear();
                if freed > 0 {
                    self.charged.fetch_sub(freed, Ordering::Relaxed);
                    return Some(freed);
                }
            }
        }
        None
    }
}
```

- [ ] **Step 4: Run to verify pass**

Run: `cargo test -p musefs-core readahead::pool_budget_tests 2>&1 | tail -20`
Expected: PASS (3 tests).

- [ ] **Step 5: Commit**

```bash
git add musefs-core/src/readahead.rs
git commit -m "feat(core): read-ahead global budget pool + per-stream cap (#255)"
```

---

### Task 3: Eviction across streams

**Files:**
- Modify: `musefs-core/src/readahead.rs`

- [ ] **Step 1: Write the failing tests**

Append to `readahead.rs`:

```rust
#[cfg(test)]
mod eviction_tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    /// Build a buffer holding exactly `bytes` real backing bytes and register it
    /// with the pool, charging the pool for those bytes (mirrors what a real miss
    /// does via permitted_window + reconcile). `bytes` must be >= WINDOW_FLOOR for
    /// the stored window to equal `bytes`.
    fn register_filled(pool: &ReadAheadPool, key: usize, bytes: usize) -> Arc<Mutex<ReadAhead>> {
        let arc = Arc::new(Mutex::new(ReadAhead::new(pool.per_stream_cap())));
        let data = vec![7u8; bytes * 2];
        let mut dst = vec![0u8; bytes];
        let (old, new) = arc
            .lock()
            .unwrap()
            .read_into(&mut dst, 0, (bytes * 2) as u64, |b, _| {
                b.copy_from_slice(&data[..b.len()]);
                Ok(())
            })
            .unwrap();
        pool.register(key, Arc::clone(&arc));
        pool.reconcile(old, new);
        arc
    }

    #[test]
    fn permitted_window_evicts_coldest_other_stream_under_pressure() {
        // Budget 4 MiB, per-stream cap 1 MiB. Fill the budget with four 1 MiB
        // streams (registered keys 1..4, so key 1 is coldest), then a fifth stream
        // wants to grow → must evict the coldest (key 1).
        let pool = ReadAheadPool::new(4 * 1024 * 1024);
        let mib = 1024 * 1024;
        let cold = register_filled(&pool, 1, mib);
        register_filled(&pool, 2, mib);
        register_filled(&pool, 3, mib);
        register_filled(&pool, 4, mib);
        // Budget is now full (4 x 1 MiB). A fresh hot stream wants 1 MiB.
        let hot = Arc::new(Mutex::new(ReadAhead::new(pool.per_stream_cap())));
        pool.register(5, Arc::clone(&hot));
        let granted = pool.permitted_window(5, 0, pool.per_stream_cap());
        assert_eq!(granted, mib, "eviction frees room for the full cap");
        assert_eq!(cold.lock().unwrap().len(), 0, "coldest stream was evicted");
    }

    #[test]
    fn locked_victim_is_skipped_not_blocked() {
        let pool = ReadAheadPool::new(4 * 1024 * 1024);
        let mib = 1024 * 1024;
        let victim = register_filled(&pool, 1, mib);
        register_filled(&pool, 2, mib);
        register_filled(&pool, 3, mib);
        register_filled(&pool, 4, mib);
        // Hold the coldest victim's lock: eviction must skip it (try_lock), not hang.
        let _held = victim.lock().unwrap();
        let hot = Arc::new(Mutex::new(ReadAhead::new(pool.per_stream_cap())));
        pool.register(5, Arc::clone(&hot));
        // Returns promptly: evicts the next-coldest unlocked victim instead.
        let granted = pool.permitted_window(5, 0, pool.per_stream_cap());
        assert!(granted > 0 && granted <= pool.per_stream_cap());
    }
}
```

- [ ] **Step 2: Run to verify failure or pass**

Run: `cargo test -p musefs-core readahead::eviction_tests 2>&1 | tail -20`
Expected: PASS — the eviction logic was implemented in Task 2. (If `permitted_window_evicts_coldest_other_stream_under_pressure` fails, the bug is in `evict_one_coldest` ordering or the `permitted_window` loop; fix the `sort_by_key`/`except` filter until green.) This task is the dedicated test gate for the Task-2 eviction code; keep it as a separate commit so a regression here bisects cleanly.

- [ ] **Step 3: Commit**

```bash
git add musefs-core/src/readahead.rs
git commit -m "test(core): read-ahead cross-stream eviction + try_lock skip (#255)"
```

---

### Task 4: `BackingReader` shim (real fd + pool, synchronous fill)

**Files:**
- Modify: `musefs-core/src/readahead.rs`

- [ ] **Step 1: Write the failing tests**

Append to `readahead.rs`:

```rust
#[cfg(test)]
mod backing_reader_tests {
    use super::*;
    use std::io::Write;
    use std::os::unix::fs::FileExt;
    use std::sync::{Arc, Mutex};

    fn temp_file(len: usize) -> (tempfile::TempDir, std::fs::File, Vec<u8>) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("backing.bin");
        let data: Vec<u8> = (0..len).map(|i| (i % 251) as u8).collect();
        std::fs::File::create(&path).unwrap().write_all(&data).unwrap();
        let f = std::fs::File::open(&path).unwrap();
        (dir, f, data)
    }

    #[test]
    fn sequential_reads_collapse_to_one_pread_per_window() {
        let (_d, file, data) = temp_file(4 * 1024 * 1024);
        let pool = ReadAheadPool::new(64 * 1024 * 1024);
        let buf = Arc::new(Mutex::new(ReadAhead::new(pool.per_stream_cap())));
        pool.register(1, Arc::clone(&buf));
        let backing_len = data.len() as u64;
        let br = BackingReader::new(&file, &buf, &pool, 1, backing_len);
        // 16 sequential 64 KiB reads.
        let mut out = vec![0u8; 64 * 1024];
        for chunk in 0..16u64 {
            br.read_exact_at(&mut out, chunk * 64 * 1024).unwrap();
            assert_eq!(out, data[(chunk * 64 * 1024) as usize..][..64 * 1024]);
        }
        // Far fewer than 16 backing preads (window grows past 1 MiB quickly).
        assert!(br.fills() < 16, "read-ahead must collapse preads, got {}", br.fills());
    }

    #[test]
    fn bytes_match_direct_pread_for_random_access() {
        let (_d, file, data) = temp_file(2 * 1024 * 1024);
        let pool = ReadAheadPool::new(64 * 1024 * 1024);
        let buf = Arc::new(Mutex::new(ReadAhead::new(pool.per_stream_cap())));
        pool.register(1, Arc::clone(&buf));
        let br = BackingReader::new(&file, &buf, &pool, 1, data.len() as u64);
        for &(off, len) in &[(0u64, 100usize), (1_000_000, 4096), (5000, 700), (2_097_000, 152)] {
            let mut a = vec![0u8; len];
            br.read_exact_at(&mut a, off).unwrap();
            let mut b = vec![0u8; len];
            file.read_exact_at(&mut b, off).unwrap();
            assert_eq!(a, b, "read-ahead byte mismatch at {off}+{len}");
        }
    }
}
```

Add `tempfile` to `musefs-core/Cargo.toml` dev-dependencies if not already present:

```bash
grep -q '^tempfile' musefs-core/Cargo.toml || echo "check [dev-dependencies] for tempfile"
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p musefs-core readahead::backing_reader_tests 2>&1 | tail -20`
Expected: FAIL — `BackingReader` undefined.

- [ ] **Step 3: Implement `BackingReader`**

Append to `readahead.rs` (before the test modules):

```rust
use std::cell::Cell;

/// The shim every backing read flows through. Borrows an open backing fd, the
/// handle's read-ahead buffer, and the global pool; serves from the window on a
/// hit, else does one large positioned read that refills the window (Phase 1).
/// Constructed per `read_into` call and dropped at its end.
pub struct BackingReader<'a> {
    file: &'a std::fs::File,
    buf: &'a Arc<Mutex<ReadAhead>>,
    pool: &'a ReadAheadPool,
    key: usize,
    backing_len: u64,
    /// Backing preads actually issued (test/metrics visibility).
    fills: Cell<u64>,
}

impl<'a> BackingReader<'a> {
    pub fn new(
        file: &'a std::fs::File,
        buf: &'a Arc<Mutex<ReadAhead>>,
        pool: &'a ReadAheadPool,
        key: usize,
        backing_len: u64,
    ) -> Self {
        BackingReader { file, buf, pool, key, backing_len, fills: Cell::new(0) }
    }

    pub fn fills(&self) -> u64 {
        self.fills.get()
    }

    /// Read exactly `dst.len()` bytes at absolute backing offset `abs_offset`.
    /// The caller (`read_segments_into`) guarantees the range is within the file.
    pub fn read_exact_at(&self, dst: &mut [u8], abs_offset: u64) -> std::io::Result<()> {
        // Read-ahead disabled → plain pread, no window.
        if !self.pool.enabled() {
            self.fills.set(self.fills.get() + 1);
            crate::metrics::on_readahead_miss();
            crate::metrics::on_pread(dst.len() as u64);
            return crate::metrics::backing_read_exact_at(self.file, dst, abs_offset);
        }
        let mut ra = self.buf.lock().unwrap();
        // On a miss (only), ask the pool how large this stream's window may grow —
        // evicting colder streams to make room — BEFORE filling, so the global
        // ceiling holds. A hit changes nothing and skips eviction entirely.
        if !ra.covers(abs_offset, dst.len()) {
            crate::metrics::on_readahead_miss();
            let cap = self.pool.permitted_window(self.key, ra.len(), self.pool.per_stream_cap());
            ra.set_cap(cap.max(WINDOW_FLOOR));
        } else {
            crate::metrics::on_readahead_hit();
        }
        let file = self.file;
        let fills = &self.fills;
        let (old_len, new_len) = ra.read_into(dst, abs_offset, self.backing_len, |b, o| {
            fills.set(fills.get() + 1);
            crate::metrics::on_pread(b.len() as u64);
            crate::metrics::backing_read_exact_at(file, b, o)
        })?;
        // Charge the budget by the ACTUAL window-size change.
        self.pool.reconcile(old_len, new_len);
        drop(ra);
        self.pool.touch(self.key);
        Ok(())
    }
}
```

> Note: `on_readahead_hit` / `on_readahead_miss` are added in Task 8; until then add temporary no-op shims at the top of `readahead.rs`:
> ```rust
> // TEMP until Task 8 wires real metrics; remove these two lines in Task 8.
> ```
> Actually do not stub in two places — implement Task 8's metric functions FIRST if you reach a compile error, or guard the calls. Simplest: do Task 8's metric additions now (they are tiny) so these compile. The plan orders Task 8 later only for test-suite grouping; if you prefer, jump to Task 8 Step 3 (the metric fn definitions) before finishing this step.

- [ ] **Step 4: Add the metric shims needed to compile**

To keep this task self-contained, add the two counter functions now (full versions in Task 8). In `musefs-core/src/metrics.rs`, inside BOTH the `#[cfg(feature="metrics")]` module and the `#[cfg(not(feature="metrics"))]` module, add:

Enabled module (near `on_art_chunk`):
```rust
    static READAHEAD_HITS: AtomicU64 = AtomicU64::new(0);
    static READAHEAD_MISSES: AtomicU64 = AtomicU64::new(0);
    pub fn on_readahead_hit() { READAHEAD_HITS.fetch_add(1, Ordering::Relaxed); }
    pub fn on_readahead_miss() { READAHEAD_MISSES.fetch_add(1, Ordering::Relaxed); }
```
Disabled module:
```rust
    #[inline(always)]
    pub fn on_readahead_hit() {}
    #[inline(always)]
    pub fn on_readahead_miss() {}
```

- [ ] **Step 5: Run to verify pass**

Run: `cargo test -p musefs-core readahead::backing_reader_tests 2>&1 | tail -20`
Expected: PASS (2 tests).

- [ ] **Step 6: Commit**

```bash
git add musefs-core/src/readahead.rs musefs-core/src/metrics.rs musefs-core/Cargo.toml
git commit -m "feat(core): BackingReader read-ahead shim over fd + budget (#255)"
```

---

### Task 5: Route the PCM `BackingAudio` arm through `BackingReader`

**Files:**
- Modify: `musefs-core/src/reader.rs`
- Modify: `musefs-core/src/lib.rs` (re-export `BackingReader`, `ReadAheadPool`)

- [ ] **Step 1: Re-export the types**

In `musefs-core/src/lib.rs` add to the `pub use` of `readahead`:

```rust
pub use readahead::{BackingReader, ReadAheadPool, DEFAULT_READAHEAD_BUDGET};
```

- [ ] **Step 2: Write the failing differential test**

In `reader.rs`, inside the existing test region (near `serve_cap_tests`), add a module:

```rust
#[cfg(test)]
mod readahead_differential_tests {
    use super::*;
    use crate::readahead::{BackingReader, ReadAhead, ReadAheadPool};
    use std::sync::{Arc, Mutex};

    // Serve every byte of a PCM-backed resolved file two ways — directly and
    // through a BackingReader — and assert identical output across odd splits.
    #[test]
    fn pcm_bytes_identical_through_backing_reader() {
        let (db, resolved, file, backing_len) = pcm_fixture(); // see helper below
        let pool = ReadAheadPool::new(64 * 1024 * 1024);
        let buf = Arc::new(Mutex::new(ReadAhead::new(pool.per_stream_cap())));
        pool.register(1, Arc::clone(&buf));
        let br = BackingReader::new(&file, &buf, &pool, 1, backing_len);
        let total = resolved.total_len;
        for &size in &[1u64, 7, 4096, 65536, 262_144] {
            let mut off = 0;
            while off < total {
                let n = size.min(total - off);
                let mut via = Vec::new();
                read_segments_into(&resolved, &db, Some(&br), off, n, &mut via).unwrap();
                let mut direct = Vec::new();
                read_segments_into_direct(&resolved, &db, &file, off, n, &mut direct).unwrap();
                assert_eq!(via, direct, "mismatch at off={off} size={size}");
                off += n;
            }
        }
    }
}
```

> The helpers `pcm_fixture()` and `read_segments_into_direct()` (a copy of the
> pre-change splice using a raw `&File`, kept only in tests as the oracle) must be
> written using the existing fixture builders in `reader.rs` tests. Reuse whatever
> `ResolvedFile` PCM builder the existing `serve_cap_tests` use; if none exposes
> the backing `File` + length, extend it minimally. Keep `read_segments_into_direct`
> as a verbatim pre-change copy so it is an independent oracle.

- [ ] **Step 3: Run to verify failure**

Run: `cargo test -p musefs-core readahead_differential_tests 2>&1 | tail -20`
Expected: FAIL — `read_segments_into` still takes `Option<&std::fs::File>`, not `Option<&BackingReader>`.

- [ ] **Step 4: Change the signatures and the `BackingAudio` arm**

In `reader.rs`:

1. Change `read_segments_into` signature param `file: Option<&std::fs::File>` → `backing: Option<&crate::readahead::BackingReader>`.
2. In the `Segment::BackingAudio { offset: bo, .. }` arm, replace:
```rust
let f = file.expect("backing segment requires an open backing file");
let start = out.len();
out.resize(start + n, 0);
crate::metrics::backing_read_exact_at(f, &mut out[start..], bo + within)?;
crate::metrics::on_pread(n as u64);
```
with (note: `on_pread` moves INTO `BackingReader`'s fill closure — Task 4 — so it
counts *physical* preads; do NOT call it here or it double-counts):
```rust
let br = backing.expect("backing segment requires an open backing reader");
let start = out.len();
out.resize(start + n, 0);
br.read_exact_at(&mut out[start..], bo + within)?;
```
3. In the `Segment::OggAudio { .. }` arm, temporarily keep it compiling by
   obtaining the raw file from the reader for now — Task 6 routes it properly.
   Add an accessor to `BackingReader`:
```rust
// in readahead.rs, impl BackingReader
pub fn file(&self) -> &std::fs::File { self.file }
```
   and in the Ogg arm change `let f = file.expect(...)` → `let f = backing.expect("...").file();` (Task 6 replaces this).
4. Update callers `read_at_with_file_into` and `read_at_into` to build a
   `BackingReader`. For the non-handle `read_at_into` fallback (one-shot open, no
   persistent buffer) construct a throwaway pool-less reader:
```rust
// read_at_into: needs_file branch
let pool = crate::readahead::ReadAheadPool::new(0); // disabled → plain preads
let buf = std::sync::Arc::new(std::sync::Mutex::new(
    crate::readahead::ReadAhead::new(0)));
let br = crate::readahead::BackingReader::new(&file, &buf, &pool, 0, resolved.total_len);
read_segments_into(resolved, db, Some(&br), offset, size, out)
```
   (`backing_len` for the fallback can be `resolved` audio bound; `total_len` is a
   safe upper clamp because the splice never requests past it. If a stricter file
   length is needed, `file.metadata()?.len()`.)
5. Make `read_at_with_file_into` / `read_at_with_file` take `&BackingReader`
   instead of `&std::fs::File` (callers in `facade.rs` updated in Task 7). Keep
   their bodies delegating to `read_segments_into(..., Some(br), ...)`.

Also expose `ReadAhead` from the crate for tests: in `lib.rs` re-export under a
`#[cfg(test)]`-friendly path or make `pub use readahead::ReadAhead;`.

- [ ] **Step 5: Run to verify pass + full crate build**

Run: `cargo test -p musefs-core readahead_differential_tests 2>&1 | tail -20`
Expected: PASS.
Run: `cargo build -p musefs-core 2>&1 | tail -20`
Expected: builds (Ogg arm compiles via the temporary `.file()` accessor).

- [ ] **Step 6: Re-anchor mutants + commit**

```bash
python3 scripts/check_mutant_anchors.py --fix 2>/dev/null || true
git add musefs-core/src/reader.rs musefs-core/src/lib.rs musefs-core/src/readahead.rs .cargo/mutants.toml
git commit -m "feat(core): route PCM backing reads through read-ahead (#255)"
```

---

### Task 6: Route Ogg backing reads through `BackingReader`

**Files:**
- Modify: `musefs-core/src/ogg_index.rs`
- Modify: `musefs-core/src/reader.rs` (the `OggAudio` arm call site)

- [ ] **Step 1: Write the failing differential test**

In `reader.rs` `readahead_differential_tests`, add an Ogg variant mirroring the
PCM test but built from the existing Ogg serve fixture (reuse `ogg_serve_tests`'
builders):

```rust
    #[test]
    fn ogg_bytes_identical_through_backing_reader() {
        let (db, resolved, file, backing_len) = ogg_fixture(); // reuse ogg_serve_tests builder
        let pool = ReadAheadPool::new(64 * 1024 * 1024);
        let buf = Arc::new(Mutex::new(ReadAhead::new(pool.per_stream_cap())));
        pool.register(1, Arc::clone(&buf));
        let br = BackingReader::new(&file, &buf, &pool, 1, backing_len);
        let total = resolved.total_len;
        for &size in &[1u64, 13, 4096, 65536] {
            let mut off = 0;
            while off < total {
                let n = size.min(total - off);
                let mut via = Vec::new();
                read_segments_into(&resolved, &db, Some(&br), off, n, &mut via).unwrap();
                let mut direct = Vec::new();
                read_segments_into_direct(&resolved, &db, &file, off, n, &mut direct).unwrap();
                assert_eq!(via, direct, "ogg mismatch at off={off} size={size}");
                off += n;
            }
        }
    }
```

- [ ] **Step 2: Run to verify it passes via the temporary `.file()` path, then make it route through the buffer**

Run: `cargo test -p musefs-core readahead_differential_tests::ogg_bytes_identical_through_backing_reader 2>&1 | tail -20`
Expected: PASS already (the temp `.file()` accessor serves correct bytes) — but it bypasses the read-ahead window. The point of this task is to route Ogg's preads through the buffer so they also benefit and are covered. Proceed to change the signatures.

- [ ] **Step 3: Thread `&BackingReader` through `ogg_index.rs`**

Change these signatures from `backing: &std::fs::File` / `f: &std::fs::File` to
`backing: &crate::readahead::BackingReader`:
- `serve_ogg_window`
- `find_page_start`
- `read_counted`
- `page_crc_ok` (and any other `read_counted` caller)

Change `read_counted` body to route through the shim and drop its own
`on_pread` (the shim's fill closure counts physical preads — Task 4):
```rust
fn read_counted(
    backing: &crate::readahead::BackingReader,
    buf: &mut [u8],
    offset: u64,
) -> std::io::Result<()> {
    // The BackingReader's fill closure increments `preads`; do not double-count.
    backing.read_exact_at(buf, offset)
}
```
**Counting model (already established in Task 4):** `preads` counts *physical*
backing preads, incremented only inside `BackingReader`'s fill closure (and its
disabled-pool path). Neither the PCM `BackingAudio` arm nor `read_counted` calls
`on_pread`. Verify no stray `crate::metrics::on_pread(` remains in `reader.rs` or
`ogg_index.rs`:
```bash
grep -rn "on_pread" musefs-core/src/reader.rs musefs-core/src/ogg_index.rs
```
Expected: no matches (all counting now lives in `readahead.rs`).

- [ ] **Step 4: Update the `OggAudio` arm call site in `reader.rs`**

```rust
Segment::OggAudio { offset: ao, seq_delta, len } => {
    let br = backing.expect("ogg-audio segment requires an open backing reader");
    serve_ogg_window(br, *ao, *len, *seq_delta, within, within + n as u64, &mut *out, Some(&resolved.last_page))?;
}
```
Remove the temporary `.file()` accessor usage. You may keep `BackingReader::file()`
only if Phase 2 needs it; otherwise delete it to avoid a dead-code warning.

- [ ] **Step 5: Run the full differential suite + crate tests**

Run: `cargo test -p musefs-core readahead 2>&1 | tail -20`
Expected: PASS.
Run: `cargo test -p musefs-core 2>&1 | tail -20`
Expected: PASS (whole crate).

- [ ] **Step 6: Re-anchor mutants + commit**

```bash
python3 scripts/check_mutant_anchors.py --fix 2>/dev/null || true
git add musefs-core/src/ogg_index.rs musefs-core/src/reader.rs musefs-core/src/readahead.rs .cargo/mutants.toml
git commit -m "feat(core): route Ogg backing reads through read-ahead (#255)"
```

---

### Task 7: Wire the buffer onto `Handle` and into `read_into`

**Files:**
- Modify: `musefs-core/src/facade.rs`

- [ ] **Step 1: Write the failing test**

In `facade.rs` tests, add a sequential-read test that asserts read-ahead reduces
physical preads through the real handle path (requires the `metrics` feature, so
gate it):

```rust
#[cfg(all(test, feature = "metrics"))]
mod readahead_handle_tests {
    use super::*;

    #[test]
    fn sequential_handle_reads_amortize_preads() {
        let fs = build_test_fs_with_one_large_pcm_track(); // reuse an existing builder
        crate::metrics::reset();
        let fh = fs.open_handle(file_inode).unwrap();
        let mut buf = Vec::new();
        // 32 sequential 256 KiB reads.
        for i in 0..32u64 {
            fs.read_into(file_inode, Some(fh), i * 262_144, 262_144, &mut buf).unwrap();
        }
        fs.release_handle(fh);
        let s = crate::metrics::snapshot();
        assert!(s.preads < 32, "read-ahead must amortize backing preads, got {}", s.preads);
    }
}
```

> If `metrics::reset()` does not exist, add it under the `metrics` feature (a
> helper that zeroes the static counters) — it is needed for deterministic count
> assertions and mirrors the existing test usage pattern.

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p musefs-core --features metrics readahead_handle_tests 2>&1 | tail -20`
Expected: FAIL — handle has no read-ahead yet (preads == 32), or build error (fields missing).

- [ ] **Step 3: Extend `Handle` and `Musefs`**

In `facade.rs`:

1. `Handle` gains fields:
```rust
struct Handle {
    track_id: i64,
    resolved: arc_swap::ArcSwap<ResolvedFile>,
    generation: AtomicU64,
    file: std::fs::File,
    /// Per-handle read-ahead window (raw backing bytes). Empty until sequential
    /// access is detected; drawn from the shared `ReadAheadPool`.
    readahead: Arc<Mutex<crate::readahead::ReadAhead>>,
    /// Whether this handle is registered in the pool's active-stream registry.
    registered: AtomicBool,
    /// Bumped on a refresh-generation change, a seek, or release; an in-flight
    /// Phase-2 prefetch checks it before storing (see spec §"Phase 2").
    epoch: AtomicU64,
}
```

2. `Musefs` gains a field:
```rust
readahead_pool: Arc<crate::readahead::ReadAheadPool>,
```
   Initialize it in `Musefs::open` from the config (Task 9 adds the config field;
   for now seed with `crate::readahead::DEFAULT_READAHEAD_BUDGET`):
```rust
readahead_pool: Arc::new(crate::readahead::ReadAheadPool::new(
    crate::readahead::DEFAULT_READAHEAD_BUDGET,
)),
```

3. `open_handle` constructs the buffer:
```rust
fh_from_key(self.handles.insert(Arc::new(Handle {
    track_id,
    resolved: arc_swap::ArcSwap::from(resolved),
    generation: AtomicU64::new(generation),
    file,
    readahead: Arc::new(Mutex::new(crate::readahead::ReadAhead::new(
        self.readahead_pool.per_stream_cap(),
    ))),
    registered: AtomicBool::new(false),
    epoch: AtomicU64::new(0),
})))
```

4. In `read_into`, the per-handle served branch currently calls
   `read_at_with_file_into(r, db, &h.file, offset, size, out)`. Replace the
   `&h.file` argument with a freshly built `BackingReader`, and register the
   stream lazily on first use:
```rust
// Build the per-call backing reader. Lazily register this handle as an active
// stream the first time it is read so the pool can account/evict it.
let key = fh.slab_key();
if !h.registered.swap(true, Ordering::AcqRel) {
    self.readahead_pool.register(key, Arc::clone(&h.readahead));
}
let backing_len = r.stamp.size; // backing file size from the validated stamp (u64 field)
let br = crate::readahead::BackingReader::new(
    &h.file, &h.readahead, &self.readahead_pool, key, backing_len,
);
read_at_with_file_into(r, db, &br, offset, size, out)?;
```
   (Adjust `read_at_with_file_into`'s signature use: it now takes `&BackingReader`
   per Task 5/6.) Determine the correct `backing_len` accessor on `BackingStamp`;
   if it stores size as a field use `r.stamp.size` / add a `size()` getter.

5. On the generation-bump path in `read_into` (where `h.generation.store(cur, ...)`
   happens after a re-resolve), drop the buffer and bump the epoch:
```rust
h.epoch.fetch_add(1, Ordering::AcqRel);
let freed = h.readahead.lock().unwrap().clear();
self.readahead_pool.uncharge(freed);
```

6. `release_handle` deregisters before removing:
```rust
pub fn release_handle(&self, fh: Fh) {
    let key = fh.slab_key();
    self.readahead_pool.deregister(key);
    self.handles.remove(key);
}
```

7. Seek detection lives inside `ReadAhead` already (offset != next_expected →
   reset); no extra facade logic needed for Phase 1. The epoch's seek bump is only
   needed for Phase 2 (Task 12).

- [ ] **Step 4: Run to verify pass**

Run: `cargo test -p musefs-core --features metrics readahead_handle_tests 2>&1 | tail -20`
Expected: PASS.
Run: `cargo test -p musefs-core 2>&1 | tail -20`
Expected: PASS (whole crate, default features).

- [ ] **Step 5: Re-anchor mutants + commit**

```bash
python3 scripts/check_mutant_anchors.py --fix 2>/dev/null || true
git add musefs-core/src/facade.rs .cargo/mutants.toml
git commit -m "feat(core): per-handle read-ahead buffer wired into read_into (#255)"
```

---

### Task 8: Read-ahead metrics + update exact-count tests

**Files:**
- Modify: `musefs-core/src/metrics.rs`
- Modify: existing `metrics`-feature count assertions wherever they live

- [ ] **Step 1: Finalize the metric surface**

Ensure `metrics.rs` has (enabled module):
```rust
static READAHEAD_HITS: AtomicU64 = AtomicU64::new(0);
static READAHEAD_MISSES: AtomicU64 = AtomicU64::new(0);
pub fn on_readahead_hit() { READAHEAD_HITS.fetch_add(1, Ordering::Relaxed); }
pub fn on_readahead_miss() { READAHEAD_MISSES.fetch_add(1, Ordering::Relaxed); }
```
Add `readahead_hits` / `readahead_misses` to BOTH `Snapshot` structs and both
`snapshot()` bodies, and to `reset()` if present.

- [ ] **Step 2: Write the failing test for the new semantics**

Add (enabled module tests):
```rust
#[test]
fn readahead_counts_physical_preads_not_logical_reads() {
    reset();
    // Drive a known sequential pattern through a BackingReader (reuse the
    // readahead::backing_reader_tests helper or replicate a tiny temp file).
    // 16 sequential 64 KiB reads over a 4 MiB file.
    // Assert: preads < 16 and readahead_hits + readahead_misses == 16.
    // (Fill in with the same temp-file harness as readahead::backing_reader_tests.)
}
```

- [ ] **Step 3: Fix the pre-existing exact-count assertions**

Find every test that asserts `s.preads == N` on a read path:
```bash
grep -rn "preads, " musefs-core/src | grep assert
```
Re-derive the expected counts under read-ahead semantics (`preads` is now
*physical* backing preads; a sequential second read is a hit, not a pread). For
single-read tests the count is unchanged (first read always misses → 1 pread).
For multi-sequential-read tests, lower the expected count and add a
`readahead_hits`/`readahead_misses` assertion. Update each.

- [ ] **Step 4: Run the metrics-feature suite**

Run: `cargo test -p musefs-core --features metrics 2>&1 | tail -30`
Expected: PASS (this is the CI `check`-job gate the local default run skips).

- [ ] **Step 5: Commit**

```bash
git add musefs-core/src/metrics.rs musefs-core/src/reader.rs musefs-core/src/readahead.rs
git commit -m "feat(core): read-ahead hit/miss metrics; preads now physical (#255)"
```

---

### Task 9: CLI flag `--read-ahead-budget-mib`

**Files:**
- Modify: `musefs-core/src/facade.rs` (`MountConfig` + `Musefs::open`)
- Modify: `musefs-cli/src/lib.rs`

- [ ] **Step 1: Write the failing CLI test**

In `musefs-cli/src/lib.rs` tests, mirror the existing `--max-readahead-kib`
assertion:
```rust
#[test]
fn read_ahead_budget_flag_maps_to_mount_config() {
    let args = parse_args(&["musefs", "/src", "/mnt", "--read-ahead-budget-mib", "128"]);
    let cfg = mount_config(&args);
    assert_eq!(cfg.read_ahead_budget, 128 * 1024 * 1024);
}

#[test]
fn read_ahead_budget_zero_disables() {
    let args = parse_args(&["musefs", "/src", "/mnt", "--read-ahead-budget-mib", "0"]);
    let cfg = mount_config(&args);
    assert_eq!(cfg.read_ahead_budget, 0);
}
```
(Match the actual arg-parser/test helpers used near `max_readahead_kib`.)

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p musefs-cli read_ahead_budget 2>&1 | tail -20`
Expected: FAIL — no field / flag.

- [ ] **Step 3: Add the config field and the flag**

In `facade.rs` `MountConfig`:
```rust
/// Global read-ahead RAM envelope in bytes. `0` disables read-ahead. The
/// operator sets the envelope; per-stream windows self-tune within it.
pub read_ahead_budget: u64,
```
Set every existing `MountConfig { .. }` literal (search the crate + tests) to
include `read_ahead_budget: crate::readahead::DEFAULT_READAHEAD_BUDGET,`.

In `Musefs::open`, build the pool from it:
```rust
readahead_pool: Arc::new(crate::readahead::ReadAheadPool::new(config.read_ahead_budget)),
```
(Place this before `config` is moved into the struct, or read the field first.)

In `musefs-cli/src/lib.rs`, add the clap arg next to `max_readahead_kib`:
```rust
/// Global read-ahead RAM budget (MiB) shared across all active streams. The
/// operator sizes this to their concurrent-stream count; 0 disables read-ahead.
#[arg(long, default_value_t = 64)]
pub read_ahead_budget_mib: u32,
```
and in `mount_config`:
```rust
read_ahead_budget: u64::from(args.read_ahead_budget_mib).saturating_mul(1024 * 1024),
```

- [ ] **Step 4: Run to verify pass + workspace build**

Run: `cargo test -p musefs-cli read_ahead_budget 2>&1 | tail -20`
Expected: PASS.
Run: `cargo build 2>&1 | tail -20`
Expected: builds (all `MountConfig` literals updated).

- [ ] **Step 5: Commit**

```bash
git add musefs-core/src/facade.rs musefs-cli/src/lib.rs
git commit -m "feat(cli): --read-ahead-budget-mib sets the read-ahead envelope (#255)"
```

---

### Task 10: Forced-eviction + partial-seek differential coverage

**Files:**
- Modify: `musefs-core/src/reader.rs` (test module)

- [ ] **Step 1: Write the tests**

Add to `readahead_differential_tests`:
```rust
    // Tiny budget forces mid-stream eviction; bytes must still be correct.
    #[test]
    fn pcm_bytes_identical_under_forced_eviction() {
        let (db, resolved, file, backing_len) = pcm_fixture();
        let pool = ReadAheadPool::new(WINDOW_FLOOR * 2); // tiny: forces churn
        let buf = Arc::new(Mutex::new(ReadAhead::new(pool.per_stream_cap())));
        pool.register(1, Arc::clone(&buf));
        let br = BackingReader::new(&file, &buf, &pool, 1, backing_len);
        let total = resolved.total_len;
        let mut off = 0;
        while off < total {
            let n = 65536u64.min(total - off);
            let mut via = Vec::new();
            read_segments_into(&resolved, &db, Some(&br), off, n, &mut via).unwrap();
            let mut direct = Vec::new();
            read_segments_into_direct(&resolved, &db, &file, off, n, &mut direct).unwrap();
            assert_eq!(via, direct, "eviction mismatch at {off}");
            off += n;
        }
    }

    // A seek landing partially back inside a just-shrunk window: exercises the
    // covers()/refill offset math.
    #[test]
    fn partial_overlap_seek_serves_correct_bytes() {
        let (db, resolved, file, backing_len) = pcm_fixture();
        let pool = ReadAheadPool::new(64 * 1024 * 1024);
        let buf = Arc::new(Mutex::new(ReadAhead::new(pool.per_stream_cap())));
        pool.register(1, Arc::clone(&buf));
        let br = BackingReader::new(&file, &buf, &pool, 1, backing_len);
        // A large read grows the window, then a seek lands partially back inside
        // the just-shrunk region, then a near-adjacent read — exercising covers()
        // and the refill offset math across the discontinuity.
        let seq = [(0u64, 600_000u64), (590_000, 50_000), (10_000, 4096), (12_000, 4096)];
        for &(off, n) in &seq {
            let n = n.min(resolved.total_len.saturating_sub(off));
            if n == 0 {
                continue;
            }
            let mut via = Vec::new();
            read_segments_into(&resolved, &db, Some(&br), off, n, &mut via).unwrap();
            let mut direct = Vec::new();
            read_segments_into_direct(&resolved, &db, &file, off, n, &mut direct).unwrap();
            assert_eq!(via, direct, "partial-seek mismatch at {off}+{n}");
        }
    }
```
(Trim the obvious copy/paste; the essential assertions are the two `assert_eq!`s.)

- [ ] **Step 2: Run to verify pass**

Run: `cargo test -p musefs-core readahead_differential_tests 2>&1 | tail -20`
Expected: PASS. If `pcm_bytes_identical_under_forced_eviction` fails, the bug is
in budget reconciliation (`acquire`/`uncharge`) or `covers()` after a `clear()`;
fix in `readahead.rs` until green.

- [ ] **Step 3: Commit**

```bash
git add musefs-core/src/reader.rs
git commit -m "test(core): read-ahead forced-eviction + partial-seek differentials (#255)"
```

---

### Task 11: Phase-1 verification gate (clippy, fmt, fuzz, full suite)

**Files:** none (verification only)

- [ ] **Step 1: Full workspace test**

Run: `cargo test 2>&1 | tail -20`
Expected: PASS.

- [ ] **Step 2: Metrics feature**

Run: `cargo test -p musefs-core --features metrics 2>&1 | tail -20`
Expected: PASS.

- [ ] **Step 3: Lint + format**

Run: `cargo clippy --all-targets 2>&1 | tail -20`
Expected: no warnings (workspace denies warnings in CI).
Run: `cargo fmt --all --check`
Expected: clean.

- [ ] **Step 4: Fuzz smoke (signatures unchanged at format layer, but verify)**

Run: `cargo +nightly fuzz build 2>&1 | tail -10`
Expected: builds.

- [ ] **Step 5: Local in-diff mutation gate (optional but recommended)**

Run: `cargo mutants --in-place -j2 2>&1 | tail -20`
Expected: no surviving mutants in the new `readahead.rs` logic; add tests for any survivor.

- [ ] **Step 6: Commit any fixups**

```bash
git add -A
git commit -m "chore(core): Phase 1 read-ahead verification fixups (#255)"
```

---

## PHASE 2 — parallel prefetch

### Task 12: Generalize the window to a prefetch ring + epoch-checked store

**Files:**
- Modify: `musefs-core/src/readahead.rs`

- [ ] **Step 1: Write the failing tests**

Add a `prefetch_tests` module:
```rust
#[cfg(test)]
mod prefetch_tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[test]
    fn store_with_stale_epoch_is_discarded() {
        let ra = Arc::new(Mutex::new(ReadAhead::new(WINDOW_ABS_CAP)));
        let epoch = Arc::new(std::sync::atomic::AtomicU64::new(0));
        // Dispatch a prefetch at epoch 0.
        let dispatched = epoch.load(std::sync::atomic::Ordering::Acquire);
        // A seek/refresh bumps the epoch.
        epoch.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        // Storing the prefetched window must be rejected.
        let stored = try_store_prefetch(&ra, &epoch, dispatched, 0, vec![1, 2, 3]);
        assert!(!stored, "stale-epoch prefetch must be discarded");
        assert_eq!(ra.lock().unwrap().len(), 0);
    }

    #[test]
    fn store_with_current_epoch_is_accepted() {
        let ra = Arc::new(Mutex::new(ReadAhead::new(WINDOW_ABS_CAP)));
        let epoch = Arc::new(std::sync::atomic::AtomicU64::new(5));
        let stored = try_store_prefetch(&ra, &epoch, 5, 1000, vec![0u8; 4096]);
        assert!(stored);
    }
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p musefs-core readahead::prefetch_tests 2>&1 | tail -20`
Expected: FAIL — `try_store_prefetch` and the prefetch-store path undefined.

- [ ] **Step 3: Implement the prefetch-store seam + a `store_window` on `ReadAhead`**

Add to `ReadAhead`:
```rust
/// Adopt an externally prefetched window (Phase 2). Replaces the current window
/// with `[start, start+bytes.len())`. Does not touch `next_expected` (the
/// foreground reader owns the sequential predictor). Returns the byte delta
/// `(old_len, new_len)` for budget reconciliation.
pub fn store_window(&mut self, start: u64, bytes: Vec<u8>) -> (u64, u64) {
    let old = self.bytes.len() as u64;
    self.win_start = start;
    self.bytes = bytes;
    (old, self.bytes.len() as u64)
}
```
Add a free function:
```rust
use std::sync::atomic::{AtomicU64 as Epoch, Ordering as O};

/// Store a prefetched window only if `dispatched_epoch` still matches `epoch`
/// (no seek/refresh/release happened since dispatch). Returns whether stored.
pub fn try_store_prefetch(
    buf: &Arc<Mutex<ReadAhead>>,
    epoch: &Epoch,
    dispatched_epoch: u64,
    start: u64,
    bytes: Vec<u8>,
) -> bool {
    let mut ra = buf.lock().unwrap();
    if epoch.load(O::Acquire) != dispatched_epoch {
        return false;
    }
    ra.store_window(start, bytes);
    true
}
```

- [ ] **Step 4: Run to verify pass**

Run: `cargo test -p musefs-core readahead::prefetch_tests 2>&1 | tail -20`
Expected: PASS.

- [ ] **Step 5: Re-anchor + commit**

```bash
python3 scripts/check_mutant_anchors.py --fix 2>/dev/null || true
git add musefs-core/src/readahead.rs .cargo/mutants.toml
git commit -m "feat(core): epoch-checked prefetch window store seam (#255)"
```

---

### Task 13: Prefetch worker pool (bounded, Db-free, best-effort)

**Files:**
- Modify: `musefs-core/src/readahead.rs`
- Modify: `musefs-core/src/facade.rs`

- [ ] **Step 1: Write the failing integration test**

In `readahead.rs`, add a test that a prefetch fills the next window ahead of a
foreground read (using a real temp file + a small worker pool):
```rust
#[cfg(test)]
mod prefetch_worker_tests {
    use super::*;
    use std::io::Write;
    use std::sync::{Arc, Mutex};

    #[test]
    fn prefetch_fills_next_window_for_a_sequential_stream() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("b.bin");
        let data: Vec<u8> = (0..8 * 1024 * 1024).map(|i| (i % 251) as u8).collect();
        std::fs::File::create(&path).unwrap().write_all(&data).unwrap();
        let file = Arc::new(std::fs::File::open(&path).unwrap());

        let pool = Arc::new(ReadAheadPool::new(64 * 1024 * 1024));
        let prefetch = PrefetchWorkers::new(2);
        let buf = Arc::new(Mutex::new(ReadAhead::new(pool.per_stream_cap())));
        let epoch = Arc::new(std::sync::atomic::AtomicU64::new(0));
        pool.register(1, Arc::clone(&buf));

        // Foreground reads window 0; request a prefetch of window 1.
        prefetch.request(PrefetchJob {
            file: Arc::clone(&file),
            buf: Arc::clone(&buf),
            epoch: Arc::clone(&epoch),
            dispatched_epoch: 0,
            start: 1024 * 1024,
            len: 1024 * 1024,
            backing_len: data.len() as u64,
        });
        prefetch.drain_for_test();
        // The window now holds [1 MiB, 2 MiB).
        let mut out = vec![0u8; 4096];
        let mut ra = buf.lock().unwrap();
        // covers() true → served without a fill closure call.
        let mut fills = 0;
        ra.read_into(&mut out, 1024 * 1024, data.len() as u64, |_, _| { fills += 1; Ok(()) }).unwrap();
        assert_eq!(fills, 0, "prefetched window should serve without a pread");
        assert_eq!(out, data[1024 * 1024..1024 * 1024 + 4096]);
    }
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p musefs-core readahead::prefetch_worker_tests 2>&1 | tail -20`
Expected: FAIL — `PrefetchWorkers` / `PrefetchJob` undefined.

- [ ] **Step 3: Implement the worker pool**

```rust
use std::sync::mpsc;

/// A prefetch job: read `[start, start+len)` (clamped to `backing_len`) from
/// `file` and store it into `buf` iff `epoch` still equals `dispatched_epoch`.
/// Carries ONLY the backing fd — never a `Db` handle (spec §4 invariant).
pub struct PrefetchJob {
    pub file: Arc<std::fs::File>,
    pub buf: Arc<Mutex<ReadAhead>>,
    pub epoch: Arc<std::sync::atomic::AtomicU64>,
    pub dispatched_epoch: u64,
    pub start: u64,
    pub len: u64,
    pub backing_len: u64,
}

/// A small, bounded pool of background prefetch threads, separate from the FUSE
/// dispatch pool so prefetch can never consume `MAX_INFLIGHT_READS` slots or
/// starve foreground reads. Best-effort: on a full queue or a read error the job
/// is dropped, leaving the window empty so the foreground read re-misses.
pub struct PrefetchWorkers {
    tx: mpsc::SyncSender<PrefetchJob>,
    #[cfg(test)]
    handles: Vec<std::thread::JoinHandle<()>>,
}

impl PrefetchWorkers {
    pub fn new(threads: usize) -> Self {
        // Bounded queue: a backlog means the consumer is slower than the stream,
        // so dropping excess jobs is correct (the window just stays cold).
        let (tx, rx) = mpsc::sync_channel::<PrefetchJob>(threads * 4);
        let rx = Arc::new(Mutex::new(rx));
        let mut handles = Vec::new();
        for _ in 0..threads {
            let rx = Arc::clone(&rx);
            let h = std::thread::spawn(move || {
                while let Ok(job) = { let g = rx.lock().unwrap(); g.recv() } {
                    Self::run_job(job);
                }
            });
            handles.push(h);
        }
        PrefetchWorkers {
            tx,
            #[cfg(test)]
            handles,
        }
    }

    fn run_job(job: PrefetchJob) {
        use std::os::unix::fs::FileExt;
        // Early epoch check: skip work already invalidated.
        if job.epoch.load(std::sync::atomic::Ordering::Acquire) != job.dispatched_epoch {
            return;
        }
        let want = job.len.min(job.backing_len.saturating_sub(job.start));
        if want == 0 {
            return;
        }
        let mut bytes = vec![0u8; want as usize];
        // Best-effort: swallow I/O errors (NFS ESTALE etc.) — leave window empty.
        if job.file.read_exact_at(&mut bytes, job.start).is_err() {
            return;
        }
        let _ = try_store_prefetch(&job.buf, &job.epoch, job.dispatched_epoch, job.start, bytes);
    }

    /// Enqueue a prefetch. Non-blocking: a full queue drops the job (best-effort).
    pub fn request(&self, job: PrefetchJob) {
        let _ = self.tx.try_send(job);
    }

    #[cfg(test)]
    pub fn drain_for_test(&self) {
        // Give workers a moment; jobs are tiny. For determinism, run synchronously
        // instead in the test build:
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}
```

> If the 50 ms sleep is too flaky for CI, make `run_job` callable directly and
> have the test call `PrefetchWorkers::run_job(job)` synchronously instead of
> `request` + `drain_for_test`. Prefer the synchronous form for the unit test.

- [ ] **Step 4: Trigger prefetch from `BackingReader` on a serving advance**

In `facade.rs` `read_into`, after a successful `read_at_with_file_into`, if the
pool is enabled and the read was sequential, enqueue a prefetch of the next
window. Add a `Musefs` field `prefetch: crate::readahead::PrefetchWorkers`
(initialize in `open` with e.g. `PrefetchWorkers::new(2)` only when budget > 0;
when disabled, skip requests). Build the job from `h.file` (wrap the fd in an
`Arc` once at `open` — change `Handle.file` to `Arc<std::fs::File>`), `h.readahead`,
`h.epoch`, the current epoch value, the next window `start = next_expected`, and
`len = per_stream_cap`. Gate the request on the buffer reporting a recent
sequential serve (expose `ReadAhead::next_expected()` and only prefetch when the
just-served read ended at the window tail).

> Keep this trigger minimal: a single next-window prefetch per serving read is the
> depth-1 pipeline. Task 14 makes the depth adaptive.

- [ ] **Step 5: Bump epoch on seek + release (Phase-2 cancellation)**

In `read_into`, detect a seek at the facade level too (compare the incoming
`offset`'s backing offset to the buffer's `next_expected`); on a seek and on
`release_handle`, `h.epoch.fetch_add(1, ...)` so any in-flight prefetch is
discarded. (The generation-bump epoch bump from Task 7 already covers refresh.)

- [ ] **Step 6: Run the suite**

Run: `cargo test -p musefs-core 2>&1 | tail -20`
Expected: PASS.
Run: `cargo test -p musefs-core --features metrics 2>&1 | tail -20`
Expected: PASS.

- [ ] **Step 7: Re-anchor + commit**

```bash
python3 scripts/check_mutant_anchors.py --fix 2>/dev/null || true
git add musefs-core/src/readahead.rs musefs-core/src/facade.rs .cargo/mutants.toml
git commit -m "feat(core): background prefetch workers (Db-free, best-effort) (#255)"
```

---

### Task 14: Adaptive prefetch depth

**Files:**
- Modify: `musefs-core/src/facade.rs`
- Modify: `musefs-core/src/readahead.rs`

- [ ] **Step 1: Write the test**

Assert that as a stream stays sequential, the number of windows prefetched ahead
grows (depth ramps), bounded by the per-stream budget share. Use a counter on a
fake/instrumented prefetch sink:
```rust
// In facade.rs tests (metrics feature), drive 64 sequential reads on a slow
// fake backing and assert prefetch depth increased over the run (e.g. later
// reads enqueue more next-windows than the first). Keep it tolerance-based.
```

- [ ] **Step 2: Implement adaptive depth**

Track a per-handle `depth` (in `ReadAhead` or a sibling atomic) that increments
while sequential and resets on seek, capped so `depth * window <=
per_stream_cap`. On each serving advance, enqueue up to `depth` next-window
prefetch jobs (offsets `next_expected, next_expected+window, ...`), each with the
current epoch.

- [ ] **Step 3: Run + commit**

Run: `cargo test -p musefs-core --features metrics 2>&1 | tail -20`
Expected: PASS.
```bash
python3 scripts/check_mutant_anchors.py --fix 2>/dev/null || true
git add musefs-core/src/facade.rs musefs-core/src/readahead.rs .cargo/mutants.toml
git commit -m "feat(core): adaptive prefetch depth bounded by budget share (#255)"
```

---

### Task 15: Phase-2 concurrency + TSan verification

**Files:**
- Modify: `musefs-core/src/readahead.rs` (test) or `musefs-core/tests/`

- [ ] **Step 1: Write a concurrency stress test**

Spawn N threads reading the SAME handle (random + sequential offsets) while
prefetch runs, asserting every read matches a direct pread oracle. This is the
TSan target.
```rust
#[test]
fn concurrent_reads_same_handle_match_oracle() {
    // Build a handle-equivalent (file + buf + pool + epoch); spawn 8 threads,
    // each doing 200 reads at varied offsets through a BackingReader; compare to
    // file.read_exact_at. Any data race surfaces under the TSan job.
}
```

- [ ] **Step 2: Run normally**

Run: `cargo test -p musefs-core concurrent_reads_same_handle 2>&1 | tail -20`
Expected: PASS.

- [ ] **Step 3: Run under TSan (matches CI)**

Run: `RUSTFLAGS="-Zsanitizer=thread" cargo +nightly test -Zbuild-std -p musefs-core --target x86_64-unknown-linux-gnu concurrent_reads_same_handle 2>&1 | tail -30`
Expected: PASS, no TSan reports. (See the `tsan-requires-build-std` project note.)

- [ ] **Step 4: Commit**

```bash
git add -A
git commit -m "test(core): read-ahead concurrent-handle TSan stress (#255)"
```

---

## ROLLOUT

### Task 16: Benchmark + BENCHMARKS.md

**Files:**
- Modify: `benches/storage_tunables_bench.sh`
- Modify: `BENCHMARKS.md`

- [ ] **Step 1: Extend the bench harness**

Add a read-ahead on/off single-stream throughput sweep to
`benches/storage_tunables_bench.sh` (mount with `--read-ahead-budget-mib 0` vs the
default; measure cold sequential MB/s). Keep it runnable in CI per the in-tree
harness convention.

- [ ] **Step 2: Run it (needs /dev/fuse + root; use the live-mount harness)**

Run the bench per its header instructions; capture single-stream MB/s for HDD and
(if available) the NFS-fault model.

- [ ] **Step 3: Record results in BENCHMARKS.md**

Replace the `#storage-tunables` "Latent finding (future work)" note with a new
subsection documenting the measured read-ahead win (on/off table), and link it
from the issue. Keep numbers backed by tmpfs-resident corpora per project policy.

- [ ] **Step 4: Commit**

```bash
git add benches/storage_tunables_bench.sh BENCHMARKS.md
git commit -m "docs(bench): single-stream read-ahead throughput results (#255)"
```

---

### Task 17: Docs

**Files:**
- Modify: `ARCHITECTURE.md`, `docs/OGG.md`, `README.md`

- [ ] **Step 1: ARCHITECTURE.md**

In the segment-model / reader section, add a short subsection: the per-handle
backing read-ahead buffer (raw backing bytes keyed by absolute offset, adaptive
window, global budget, `BackingReader` as the single backing-read path, Phase-2
prefetch). Note it preserves the audio invariant and the per-read validation.

- [ ] **Step 2: docs/OGG.md**

Note that Ogg backing reads (`serve_ogg_window` page walk) now flow through the
same read-ahead buffer; header patching is unchanged and orthogonal to the raw
byte cache.

- [ ] **Step 3: README.md**

Document `--read-ahead-budget-mib` (default 64, 0 disables) in the flags section,
framed as the RAM envelope for slow/high-RTT backing.

- [ ] **Step 4: Commit (docs-only — cargo gate skipped)**

```bash
git add ARCHITECTURE.md docs/OGG.md README.md
git commit -m "docs: backing read-ahead architecture, Ogg, and CLI flag (#255)"
```

---

## Self-Review Notes (for the implementer)

- **Backing length source:** several tasks need the backing file size. Use
  `r.stamp.size` — `BackingStamp` (`musefs-core/src/freshness.rs`) exposes
  `size: u64` as a public field, already captured and validated at resolve time.
  No extra fstat needed.
- **Budget accounting is the subtle part.** The invariant: `pool.charged` ==
  Σ(registered buffers' `bytes.len()`). Every path that changes a buffer's length
  (`acquire` grow, `clear` on eviction/invalidation, `store_window` in Phase 2,
  seek-shrink) must reconcile via `acquire`/`uncharge`. Task 10's forced-eviction
  differential and a debug assertion (`charged <= budget`) are the safety net —
  consider adding `debug_assert!(self.charged.load(Relaxed) <= self.budget)` after
  each `acquire` return.
- **`Handle.file` becomes `Arc<std::fs::File>` in Task 13** (prefetch jobs need an
  owned fd reference). Make that change in Task 13 and update `open_handle`,
  `validate_opened_backing` call sites (`&h.file` → `&*h.file`), and
  `passthrough_fd`'s `AsFd` impl accordingly.
- **`on_pread` semantics changed in Task 6** (now physical preads). Re-audit any
  `scan_*` metrics — they are a separate code path (scanning, not serving) and
  must NOT route through `BackingReader`; leave them untouched.
- **Fallback path (`read_at_into`) stays plain** (disabled pool). Confirm no
  normal player read reaches it (FUSE always supplies the `open` fh).
