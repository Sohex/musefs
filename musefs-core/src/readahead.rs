//! Per-handle backing read-ahead: an adaptive window over raw backing-file
//! bytes, a global byte budget with eviction, and the `BackingReader` shim that
//! every backing read flows through. See
//! `docs/superpowers/specs/2026-06-14-read-ahead-overlap-design.md`.

use std::collections::HashMap;
use std::io;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// Floor window size: a fresh or just-seeked stream still reads this much ahead.
pub const WINDOW_FLOOR: u64 = 512 * 1024;
/// Absolute per-stream window cap, independent of the global budget.
pub const WINDOW_ABS_CAP: u64 = 8 * 1024 * 1024;

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
        (self.budget / PER_STREAM_DIVISOR).clamp(WINDOW_FLOOR, WINDOW_ABS_CAP)
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
                Some(_) => {}
                None => {
                    // Nothing evictable: permit only what fits now.
                    return old_len + room;
                }
            }
        }
    }

    /// Charge the budget by the ACTUAL window-size change `(old_len → new_len)`.
    /// Keeps the invariant `charged == Σ(registered buffers' bytes.len())`.
    pub fn reconcile(&self, old_len: u64, new_len: u64) {
        if new_len > old_len {
            self.charged.fetch_add(new_len - old_len, Ordering::Relaxed);
        } else if new_len < old_len {
            self.charged.fetch_sub(old_len - new_len, Ordering::Relaxed);
        }
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
            cap,
        }
    }

    /// Bytes currently held (charged against the global budget).
    #[allow(clippy::len_without_is_empty)]
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

    /// Update the per-stream cap (when the budget share changes). No floor — see
    /// `new`: a sub-floor cap must shrink the window, not be silently raised.
    pub fn set_cap(&mut self, cap: u64) {
        self.cap = cap;
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
            #[expect(clippy::cast_possible_truncation)]
            let lo = (off - self.win_start) as usize;
            dst.copy_from_slice(&self.bytes[lo..lo + len]);
            self.next_expected = off + len as u64;
            let n = self.bytes.len() as u64;
            return Ok((n, n));
        }
        let old_len = self.bytes.len() as u64;
        if off == self.next_expected {
            self.window = self.window.saturating_mul(2).min(self.cap);
        } else {
            self.window = WINDOW_FLOOR.min(self.cap);
        }
        debug_assert!(off < backing_len && off + len as u64 <= backing_len);
        let want = self
            .window
            .max(len as u64)
            .min(self.cap.max(len as u64))
            .min(backing_len.saturating_sub(off));
        #[expect(clippy::cast_possible_truncation)]
        let mut buf = vec![0u8; want as usize];
        fill(&mut buf, off)?;
        dst.copy_from_slice(&buf[..len]);
        self.win_start = off;
        self.bytes = buf;
        self.next_expected = off + len as u64;
        Ok((old_len, self.bytes.len() as u64))
    }
}

use std::cell::Cell;

pub struct BackingReader<'a> {
    file: &'a std::fs::File,
    buf: &'a Arc<Mutex<ReadAhead>>,
    pool: &'a ReadAheadPool,
    key: usize,
    backing_len: u64,
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
        BackingReader {
            file,
            buf,
            pool,
            key,
            backing_len,
            fills: Cell::new(0),
        }
    }

    pub fn fills(&self) -> u64 {
        self.fills.get()
    }

    pub fn file(&self) -> &std::fs::File {
        self.file
    }

    pub fn read_exact_at(&self, dst: &mut [u8], abs_offset: u64) -> std::io::Result<()> {
        if !self.pool.enabled() {
            self.fills.set(self.fills.get() + 1);
            crate::metrics::on_readahead_miss();
            crate::metrics::on_pread(dst.len() as u64);
            return crate::metrics::backing_read_exact_at(self.file, dst, abs_offset);
        }
        let mut ra = self.buf.lock().unwrap();
        if ra.covers(abs_offset, dst.len()) {
            crate::metrics::on_readahead_hit();
        } else {
            crate::metrics::on_readahead_miss();
            let cap = self
                .pool
                .permitted_window(self.key, ra.len(), self.pool.per_stream_cap());
            ra.set_cap(cap);
        }
        let file = self.file;
        let fills = &self.fills;
        let (old_len, new_len) = ra.read_into(dst, abs_offset, self.backing_len, |b, o| {
            fills.set(fills.get() + 1);
            crate::metrics::on_pread(b.len() as u64);
            crate::metrics::backing_read_exact_at(file, b, o)
        })?;
        self.pool.reconcile(old_len, new_len);
        drop(ra);
        self.pool.touch(self.key);
        Ok(())
    }
}

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
            #[expect(clippy::cast_possible_truncation)]
            let data = (0..len).map(|i| (i % 251) as u8).collect();
            Fake {
                data,
                reads: Vec::new(),
            }
        }
        #[expect(clippy::unnecessary_wraps)]
        fn fill(&mut self, buf: &mut [u8], off: u64) -> io::Result<()> {
            self.reads.push((off, buf.len()));
            #[expect(clippy::cast_possible_truncation)]
            let o = off as usize;
            buf.copy_from_slice(&self.data[o..o + buf.len()]);
            Ok(())
        }
    }

    fn serve(ra: &mut ReadAhead, fake: &mut Fake, off: u64, len: usize) -> Vec<u8> {
        let mut dst = vec![0u8; len];
        let backing_len = fake.data.len() as u64;
        ra.read_into(&mut dst, off, backing_len, |b, o| fake.fill(b, o))
            .unwrap();
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
        #[expect(clippy::cast_possible_truncation)]
        let floor = WINDOW_FLOOR as usize;
        serve(&mut ra, &mut fake, 0, floor); // miss, window stays floor (first fill)
        serve(&mut ra, &mut fake, WINDOW_FLOOR, floor); // seq miss → window doubles
        // The second fill must have requested > floor bytes (geometric growth).
        let second_fill_len = fake.reads[1].1 as u64;
        assert!(
            second_fill_len > WINDOW_FLOOR,
            "window must grow on sequential miss"
        );
        assert!(second_fill_len <= WINDOW_ABS_CAP, "window capped");
    }

    #[test]
    fn seek_resets_window_to_floor() {
        let mut fake = Fake::new(16 * 1024 * 1024);
        let mut ra = ReadAhead::new(WINDOW_ABS_CAP);
        #[expect(clippy::cast_possible_truncation)]
        serve(&mut ra, &mut fake, 0, WINDOW_FLOOR as usize);
        #[expect(clippy::cast_possible_truncation)]
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
        assert!(
            off + len as u64 <= 700 * 1024,
            "fill must not read past EOF"
        );
    }
}

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
        assert_eq!(
            pool.permitted_window(1, 1024 * 1024, 8 * 1024 * 1024),
            1024 * 1024
        );
    }
}

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
        let mib = 1024 * 1024usize;
        let cold = register_filled(&pool, 1, mib);
        register_filled(&pool, 2, mib);
        register_filled(&pool, 3, mib);
        register_filled(&pool, 4, mib);
        // Budget is now full (4 x 1 MiB). A fresh hot stream wants 1 MiB.
        let hot = Arc::new(Mutex::new(ReadAhead::new(pool.per_stream_cap())));
        pool.register(5, Arc::clone(&hot));
        let granted = pool.permitted_window(5, 0, pool.per_stream_cap());
        assert_eq!(granted, mib as u64, "eviction frees room for the full cap");
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

    #[cfg(test)]
    mod backing_reader_tests {
        use super::*;
        use std::io::Write;
        use std::os::unix::fs::FileExt;
        use std::sync::{Arc, Mutex};

        #[expect(clippy::cast_possible_truncation)]
        fn temp_file(len: usize) -> (tempfile::TempDir, std::fs::File, Vec<u8>) {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("backing.bin");
            let data: Vec<u8> = (0..len).map(|i| (i % 251) as u8).collect();
            std::fs::File::create(&path)
                .unwrap()
                .write_all(&data)
                .unwrap();
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
            let mut out = vec![0u8; 64 * 1024];
            #[expect(clippy::cast_possible_truncation)]
            for chunk in 0..16u64 {
                br.read_exact_at(&mut out, chunk * 64 * 1024).unwrap();
                assert_eq!(out, data[(chunk * 64 * 1024) as usize..][..64 * 1024]);
            }
            assert!(
                br.fills() < 16,
                "read-ahead must collapse preads, got {}",
                br.fills()
            );
        }

        #[test]
        fn bytes_match_direct_pread_for_random_access() {
            let (_d, file, data) = temp_file(2 * 1024 * 1024);
            let pool = ReadAheadPool::new(64 * 1024 * 1024);
            let buf = Arc::new(Mutex::new(ReadAhead::new(pool.per_stream_cap())));
            pool.register(1, Arc::clone(&buf));
            let br = BackingReader::new(&file, &buf, &pool, 1, data.len() as u64);
            for &(off, len) in &[
                (0u64, 100usize),
                (1_000_000, 4096),
                (5000, 700),
                (2_097_000, 152),
            ] {
                let mut a = vec![0u8; len];
                br.read_exact_at(&mut a, off).unwrap();
                let mut b = vec![0u8; len];
                file.read_exact_at(&mut b, off).unwrap();
                assert_eq!(a, b, "read-ahead byte mismatch at {off}+{len}");
            }
        }
    }
}
