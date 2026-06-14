//! Per-handle backing read-ahead: an adaptive window over raw backing-file
//! bytes, a global byte budget with eviction, and the `BackingReader` shim that
//! every backing read flows through. See
//! `docs/superpowers/specs/2026-06-14-read-ahead-overlap-design.md`.

use std::collections::HashMap;
use std::io;
use std::sync::atomic::{AtomicU64 as Epoch, Ordering as O};
use std::sync::{Arc, Mutex};

/// Floor window size: a fresh or just-seeked stream still reads this much ahead.
pub const WINDOW_FLOOR: u64 = 512 * 1024;
/// Absolute per-stream window cap, independent of the global budget.
pub const WINDOW_ABS_CAP: u64 = 8 * 1024 * 1024;

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
    charged: Epoch,
    /// Active streaming handles keyed by slab key. Only sequential streams register.
    streams: Mutex<HashMap<usize, StreamEntry>>,
    /// Monotonic source for `last_served` stamps (LRU ordering).
    clock: Epoch,
}

impl ReadAheadPool {
    pub fn new(budget: u64) -> Self {
        ReadAheadPool {
            budget,
            charged: Epoch::new(0),
            streams: Mutex::new(HashMap::new()),
            clock: Epoch::new(0),
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
        let last_served = self.clock.fetch_add(1, O::Relaxed);
        self.streams
            .lock()
            .unwrap()
            .insert(key, StreamEntry { buf, last_served });
    }

    /// Deregister on release; returns the bytes to uncharge. Drops the `streams`
    /// lock BEFORE locking the buffer: a concurrent read holds its buffer mutex
    /// and then blocking-acquires `streams` (via `permitted_window`), so holding
    /// `streams` while locking the buffer here would invert that order and
    /// deadlock. Keeping `streams` a leaf (released before any buffer mutex)
    /// preserves the pool's deadlock-free invariant.
    pub fn deregister(&self, key: usize) {
        let entry = self.streams.lock().unwrap().remove(&key);
        let freed = match entry {
            Some(e) => e.buf.lock().unwrap().len(),
            None => 0,
        };
        if freed > 0 {
            self.charged.fetch_sub(freed, O::Relaxed);
        }
    }

    /// Mark `key` as most-recently-served (LRU bump). No-op if unregistered.
    pub fn touch(&self, key: usize) {
        let stamp = self.clock.fetch_add(1, O::Relaxed);
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
            let cur = self.charged.load(O::Relaxed);
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
            self.charged.fetch_add(new_len - old_len, O::Relaxed);
        } else if new_len < old_len {
            self.charged.fetch_sub(old_len - new_len, O::Relaxed);
        }
    }

    /// Best-effort check that `need` bytes of *free* (uncharged) budget exist.
    /// Speculative prefetch uses this rather than evicting live streams: under
    /// memory pressure it simply declines to prefetch. Racy by design — the
    /// caller still `reconcile`s the actual stored delta, so the
    /// `charged == Σ bytes` invariant holds regardless; this only bounds overshoot.
    pub fn has_room_for(&self, need: u64) -> bool {
        self.budget > 0 && self.budget.saturating_sub(self.charged.load(O::Relaxed)) >= need
    }

    /// Bytes currently held across all registered read-ahead buffers (telemetry
    /// `musefs_readahead_charged_bytes`; #394).
    pub fn charged(&self) -> u64 {
        self.charged.load(O::Relaxed)
    }

    /// Total read-ahead RAM envelope; `0` when read-ahead is disabled (telemetry
    /// `musefs_readahead_budget_bytes`; #394).
    pub fn budget(&self) -> u64 {
        self.budget
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
                    self.charged.fetch_sub(freed, O::Relaxed);
                    return Some(freed);
                }
            }
        }
        None
    }
}

struct Window {
    start: u64,
    bytes: Vec<u8>,
}

pub struct ReadAhead {
    windows: Vec<Window>,
    next_expected: u64,
    window: u64,
    cap: u64,
    max_windows: usize,
}

impl ReadAhead {
    pub fn new(cap: u64) -> Self {
        ReadAhead {
            windows: Vec::new(),
            next_expected: u64::MAX,
            window: WINDOW_FLOOR,
            cap,
            max_windows: 1,
        }
    }
    pub fn set_max_windows(&mut self, n: usize) {
        self.max_windows = n.max(1);
    }
    pub fn next_expected(&self) -> u64 {
        self.next_expected
    }
    /// The current adaptive window size (grows geometrically on sequential
    /// access, resets to the floor on seek). Drives prefetch depth.
    pub fn window(&self) -> u64 {
        self.window
    }

    #[allow(clippy::len_without_is_empty)]
    pub fn len(&self) -> u64 {
        self.windows.iter().map(|w| w.bytes.len() as u64).sum()
    }
    pub fn clear(&mut self) -> u64 {
        let freed = self.len();
        self.windows.clear();
        self.window = WINDOW_FLOOR.min(self.cap);
        self.next_expected = u64::MAX;
        freed
    }
    pub fn set_cap(&mut self, cap: u64) {
        self.cap = cap;
        if self.window > self.cap {
            self.window = self.cap;
        }
    }

    pub fn covers(&self, off: u64, len: usize) -> bool {
        let end = off.saturating_add(len as u64);
        self.windows
            .iter()
            .any(|w| off >= w.start && end <= w.start + w.bytes.len() as u64)
    }

    fn window_containing(&self, off: u64, len: usize) -> Option<&Window> {
        let end = off.saturating_add(len as u64);
        self.windows
            .iter()
            .find(|w| off >= w.start && end <= w.start + w.bytes.len() as u64)
    }

    fn insert_window(&mut self, w: Window) {
        if let Some(slot) = self.windows.iter_mut().find(|x| x.start == w.start) {
            *slot = w;
        } else {
            self.windows.push(w);
        }
        self.windows.sort_by_key(|x| x.start);
        while self.windows.len() > self.max_windows {
            let frontier = self.next_expected;
            let idx = self
                .windows
                .iter()
                .enumerate()
                .filter(|(_, w)| w.start + w.bytes.len() as u64 <= frontier)
                .min_by_key(|(_, w)| w.start)
                .map_or(0, |(i, _)| i);
            self.windows.remove(idx);
        }
    }

    pub fn store_window(&mut self, start: u64, bytes: Vec<u8>) -> (u64, u64) {
        let old = self.len();
        self.insert_window(Window { start, bytes });
        (old, self.len())
    }

    pub fn read_into(
        &mut self,
        dst: &mut [u8],
        off: u64,
        backing_len: u64,
        mut fill: impl FnMut(&mut [u8], u64) -> io::Result<()>,
    ) -> io::Result<(u64, u64)> {
        let len = dst.len();
        if len == 0 {
            let n = self.len();
            return Ok((n, n));
        }
        if let Some(w) = self.window_containing(off, len) {
            #[expect(clippy::cast_possible_truncation)]
            let lo = (off - w.start) as usize;
            dst.copy_from_slice(&w.bytes[lo..lo + len]);
            self.next_expected = off + len as u64;
            let n = self.len();
            return Ok((n, n));
        }
        let old = self.len();
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
        self.insert_window(Window {
            start: off,
            bytes: buf,
        });
        self.next_expected = off + len as u64;
        Ok((old, self.len()))
    }
}

/// Store a prefetched window into `buf` iff the handle's epoch is unchanged, and
/// charge the global budget by the resulting size delta so the
/// `charged == Σ(registered buffers' bytes.len())` invariant is preserved. A
/// stale epoch (seek/release/refresh since dispatch) drops the window untouched.
pub fn try_store_prefetch(
    pool: &ReadAheadPool,
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
    let (old, new) = ra.store_window(start, bytes);
    drop(ra);
    pool.reconcile(old, new);
    true
}

/// How many `window`-sized next-windows to keep in flight for a sequential
/// stream: enough that their combined size is about one per-stream budget share
/// (`cap`), clamped to a small thread fan-out. Decreases as the adaptive window
/// grows toward `cap` (fewer, larger chunks) and rises again after a seek resets
/// the window to the floor. Always ≥ 1 and never `cap / cap == 1` regardless of
/// `window`.
pub fn prefetch_depth(cap: u64, window: u64) -> u64 {
    (cap / window.max(1)).clamp(1, 4)
}

/// Plan which next-windows to enqueue, deduplicating against `prefetched_upto`
/// (the offset through which jobs were already dispatched for this stream).
///
/// Anchoring dispatch to the moving `next_expected` made every read re-request
/// the same forward windows at shifted offsets — a pile of overlapping preads.
/// Instead we fetch contiguous `window`-sized blocks from the watermark up to
/// the horizon (`start + depth*window`), so a sequential reader enqueues only
/// the freshly-exposed tail (≈ one window per window consumed). A seek — the
/// reader landing before the watermark, or the watermark sitting beyond the new
/// horizon — restarts dispatch from `start`. Returns the window-aligned start
/// offsets and the new watermark.
pub fn plan_prefetch(
    prefetched_upto: u64,
    start: u64,
    window: u64,
    depth: u64,
    backing_len: u64,
) -> (Vec<u64>, u64) {
    if window == 0 || start >= backing_len {
        return (Vec::new(), prefetched_upto);
    }
    let horizon = start
        .saturating_add(depth.saturating_mul(window))
        .min(backing_len);
    let mut s = prefetched_upto;
    // Outside [start, horizon] means a seek (reader moved before the watermark)
    // or the watermark ran past the horizon: dispatch from the current position.
    if !(start..=horizon).contains(&s) {
        s = start;
    }
    let mut starts = Vec::new();
    while s < horizon {
        starts.push(s);
        s += window;
    }
    (starts, s.min(backing_len))
}

use std::cell::Cell;
use std::sync::mpsc;

pub struct PrefetchJob {
    pub file: Arc<std::fs::File>,
    pub buf: Arc<Mutex<ReadAhead>>,
    pub pool: Arc<ReadAheadPool>,
    pub epoch: Arc<std::sync::atomic::AtomicU64>,
    pub dispatched_epoch: u64,
    pub start: u64,
    pub len: u64,
    pub backing_len: u64,
}

pub struct PrefetchWorkers {
    tx: mpsc::SyncSender<PrefetchJob>,
    #[cfg(test)]
    handles: Vec<std::thread::JoinHandle<()>>,
}

impl PrefetchWorkers {
    pub fn new(threads: usize) -> Self {
        let (tx, rx) = mpsc::sync_channel::<PrefetchJob>(threads * 4);
        let rx = Arc::new(Mutex::new(rx));
        let mut handles = Vec::new();
        for _ in 0..threads {
            let rx = Arc::clone(&rx);
            let h = std::thread::spawn(move || {
                while let Ok(job) = {
                    let g = rx.lock().unwrap();
                    g.recv()
                } {
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

    #[expect(clippy::needless_pass_by_value)]
    pub fn run_job(job: PrefetchJob) {
        use std::os::unix::fs::FileExt;
        if job.epoch.load(std::sync::atomic::Ordering::Acquire) != job.dispatched_epoch {
            return;
        }
        let want = job.len.min(job.backing_len.saturating_sub(job.start));
        if want == 0 {
            return;
        }
        // Speculative: only prefetch into free budget, never evicting a live
        // stream. Under pressure we skip rather than thrash real reads.
        if !job.pool.has_room_for(want) {
            return;
        }
        #[expect(clippy::cast_possible_truncation)]
        let mut bytes = vec![0u8; want as usize];
        if job.file.read_exact_at(&mut bytes, job.start).is_err() {
            return;
        }
        let _ = try_store_prefetch(
            &job.pool,
            &job.buf,
            &job.epoch,
            job.dispatched_epoch,
            job.start,
            bytes,
        );
    }

    pub fn request(&self, job: PrefetchJob) {
        let _ = self.tx.try_send(job);
    }
}

pub struct BackingReader<'a> {
    file: &'a std::fs::File,
    buf: &'a Arc<Mutex<ReadAhead>>,
    pool: &'a ReadAheadPool,
    key: usize,
    backing_len: u64,
    fills: Cell<u64>,
    epoch: &'a std::sync::atomic::AtomicU64,
}

impl<'a> BackingReader<'a> {
    pub fn new(
        file: &'a std::fs::File,
        buf: &'a Arc<Mutex<ReadAhead>>,
        pool: &'a ReadAheadPool,
        key: usize,
        backing_len: u64,
        epoch: &'a std::sync::atomic::AtomicU64,
    ) -> Self {
        BackingReader {
            file,
            buf,
            pool,
            key,
            backing_len,
            fills: Cell::new(0),
            epoch,
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
            if abs_offset != ra.next_expected() {
                self.epoch.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
            }
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
            let epoch = std::sync::atomic::AtomicU64::new(0);
            let br = BackingReader::new(&file, &buf, &pool, 1, backing_len, &epoch);
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
            let epoch = std::sync::atomic::AtomicU64::new(0);
            let br = BackingReader::new(&file, &buf, &pool, 1, data.len() as u64, &epoch);
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

#[cfg(test)]
mod ring_tests {
    use super::*;

    #[test]
    fn default_ring_holds_one_window_like_phase1() {
        let mut ra = ReadAhead::new(WINDOW_ABS_CAP);
        let data = (0..2 * 1024 * 1024u64)
            .map(|i| (i % 251) as u8)
            .collect::<Vec<u8>>();
        let blen = data.len() as u64;
        let mut dst = vec![0u8; 4096];
        ra.read_into(&mut dst, 0, blen, |b, o| {
            #[expect(clippy::cast_possible_truncation)]
            let o = o as usize;
            b.copy_from_slice(&data[o..][..b.len()]);
            Ok(())
        })
        .unwrap();
        ra.read_into(&mut dst, 1_000_000, blen, |b, o| {
            #[expect(clippy::cast_possible_truncation)]
            let o = o as usize;
            b.copy_from_slice(&data[o..][..b.len()]);
            Ok(())
        })
        .unwrap();
        assert!(
            !ra.covers(0, 4096),
            "single-window ring evicts the old window"
        );
        assert!(ra.covers(1_000_000, 4096));
    }

    #[test]
    fn ring_of_two_keeps_current_and_prefetched_window() {
        let mut ra = ReadAhead::new(WINDOW_ABS_CAP);
        ra.set_max_windows(2);
        ra.store_window(1024 * 1024, vec![9u8; 512 * 1024]);
        let mut dst = vec![0u8; 4096];
        ra.read_into(&mut dst, 0, 4 * 1024 * 1024, |b, _| {
            b.fill(1u8);
            Ok(())
        })
        .unwrap();
        assert!(ra.covers(0, 4096), "current window present");
        assert!(
            ra.covers(1024 * 1024, 4096),
            "prefetched window NOT clobbered"
        );
    }

    /// Regression for the `cap / cap == 1` tautology: depth must track the live
    /// adaptive window, fanning out when the window is small and collapsing to 1
    /// as it grows to `cap` — and the in-flight prefetch (`depth * window`) must
    /// stay within one per-stream budget share.
    #[test]
    fn prefetch_depth_tracks_window_and_bounds_inflight() {
        let cap = 8 * 1024 * 1024;
        // Fresh / just-seeked: floor-sized window → fan out (16 → clamp 4),
        // NOT 1 (which the old cap/cap formula always produced).
        assert_eq!(prefetch_depth(cap, WINDOW_FLOOR), 4);
        assert_eq!(prefetch_depth(cap, 2 * 1024 * 1024), 4);
        assert_eq!(prefetch_depth(cap, 4 * 1024 * 1024), 2);
        // Window grown to the cap → a single next-window suffices.
        assert_eq!(prefetch_depth(cap, cap), 1);
        // Degenerate inputs never panic or return 0.
        assert_eq!(prefetch_depth(cap, 0), 4);
        assert_eq!(prefetch_depth(cap, cap * 2), 1);
        // In-flight bytes stay within one budget share across the growth curve.
        for w in [WINDOW_FLOOR, 1 << 20, 2 << 20, 4 << 20, cap] {
            assert!(prefetch_depth(cap, w) * w <= cap, "overshoot at window {w}");
        }
    }

    /// A sequential stream must not re-request windows it already dispatched:
    /// after the initial fan-out, each subsequent read enqueues only the newly
    /// exposed tail (no overlapping preads).
    #[test]
    fn plan_prefetch_dedups_sequential_dispatch() {
        let win = WINDOW_FLOOR;
        let cap = 8 * 1024 * 1024;
        let depth = prefetch_depth(cap, win); // 4
        let blen = 100 * 1024 * 1024;
        // First read ends at `win`; fan out `depth` windows ahead.
        let (s1, w1) = plan_prefetch(0, win, win, depth, blen);
        assert_eq!(s1, vec![win, 2 * win, 3 * win, 4 * win]);
        assert_eq!(w1, 5 * win);
        // Reader advanced ~half a window. The already-requested windows are
        // skipped — only the single newly-exposed window is enqueued.
        let start2 = win + win / 2;
        let (s2, w2) = plan_prefetch(w1, start2, win, depth, blen);
        assert_eq!(s2, vec![5 * win], "must not re-dispatch buffered windows");
        assert_eq!(w2, 6 * win);
    }

    #[test]
    fn plan_prefetch_seek_resets_watermark() {
        let win = WINDOW_FLOOR;
        let depth = 4;
        let blen = 100 * 1024 * 1024;
        let (_s, w) = plan_prefetch(0, 10 * win, win, depth, blen);
        // Seek backward, well before the watermark: dispatch restarts at `start`.
        let (s2, w2) = plan_prefetch(w, 2 * win, win, depth, blen);
        assert_eq!(s2.first(), Some(&(2 * win)));
        assert_eq!(w2, 6 * win);
    }

    #[test]
    fn plan_prefetch_clamps_to_backing_len() {
        let win = WINDOW_FLOOR;
        let blen = 3 * win + 100;
        let (s, w) = plan_prefetch(0, win, win, 4, blen);
        assert!(s.iter().all(|&x| x < blen), "no job starts past EOF");
        assert!(w <= blen, "watermark clamped to EOF");
    }

    #[test]
    fn len_sums_all_windows_and_clear_drops_all() {
        let mut ra = ReadAhead::new(WINDOW_ABS_CAP);
        ra.set_max_windows(3);
        ra.store_window(0, vec![0u8; 100]);
        ra.store_window(1000, vec![0u8; 200]);
        assert_eq!(ra.len(), 300);
        assert_eq!(ra.clear(), 300);
        assert_eq!(ra.len(), 0);
    }
}

#[cfg(test)]
mod prefetch_store_tests {
    use super::*;
    use std::sync::atomic::AtomicU64;

    #[test]
    fn store_with_stale_epoch_is_discarded() {
        let pool = ReadAheadPool::new(64 * 1024 * 1024);
        let ra = Arc::new(Mutex::new(ReadAhead::new(WINDOW_ABS_CAP)));
        let epoch = AtomicU64::new(0);
        epoch.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        assert!(!try_store_prefetch(&pool, &ra, &epoch, 0, 0, vec![1, 2, 3]));
        assert_eq!(ra.lock().unwrap().len(), 0);
    }

    #[test]
    fn store_with_current_epoch_is_accepted_and_charges_budget() {
        let pool = ReadAheadPool::new(64 * 1024 * 1024);
        let ra = Arc::new(Mutex::new(ReadAhead::new(WINDOW_ABS_CAP)));
        ra.lock().unwrap().set_max_windows(2);
        let epoch = AtomicU64::new(5);
        assert!(try_store_prefetch(
            &pool,
            &ra,
            &epoch,
            5,
            1000,
            vec![0u8; 4096]
        ));
        assert!(ra.lock().unwrap().covers(1000, 4096));
        // The stored window is charged against the global budget.
        assert_eq!(pool.charged(), 4096);
    }

    #[test]
    fn pool_reports_its_budget() {
        assert_eq!(ReadAheadPool::new(64 * 1024).budget(), 64 * 1024);
        assert_eq!(ReadAheadPool::new(0).budget(), 0);
        assert_eq!(ReadAheadPool::new(0).charged(), 0);
    }

    /// Regression: a prefetched window must charge the budget so that the
    /// subsequent uncharge on eviction/release cannot drive `charged` negative
    /// (an underflow that silently disabled read-ahead process-wide).
    #[test]
    #[expect(clippy::cast_possible_truncation)]
    fn prefetch_charge_survives_release_without_underflow() {
        let pool = ReadAheadPool::new(2 * 1024 * 1024); // per-stream cap 512K
        let cap = pool.per_stream_cap();
        let buf = Arc::new(Mutex::new(ReadAhead::new(cap)));
        buf.lock().unwrap().set_max_windows(2);
        pool.register(1, Arc::clone(&buf));
        // Sync read fills + charges one cap-sized window at offset 0.
        let mut dst = vec![0u8; cap as usize];
        let (o, n) = buf
            .lock()
            .unwrap()
            .read_into(&mut dst, 0, 8 * 1024 * 1024, |b, _| {
                b.fill(1);
                Ok(())
            })
            .unwrap();
        pool.reconcile(o, n);
        // Prefetch a second cap-sized window — now charged via try_store_prefetch.
        let epoch = Epoch::new(0);
        assert!(try_store_prefetch(
            &pool,
            &buf,
            &epoch,
            0,
            cap,
            vec![2u8; cap as usize]
        ));
        assert_eq!(pool.charged(), 2 * cap);
        pool.deregister(1);
        assert_eq!(pool.charged(), 0, "release must not underflow");
        // A fresh stream still gets its full grant — read-ahead is not disabled.
        let hot = Arc::new(Mutex::new(ReadAhead::new(cap)));
        pool.register(2, Arc::clone(&hot));
        assert_eq!(pool.permitted_window(2, 0, cap), cap);
    }

    /// Under budget pressure, prefetch declines (no free room) rather than
    /// evicting a live stream or overshooting the budget.
    #[test]
    fn prefetch_declines_when_budget_full() {
        let pool = ReadAheadPool::new(1024 * 1024);
        assert!(pool.has_room_for(512 * 1024));
        pool.reconcile(0, 1024 * 1024); // fill the budget
        assert!(!pool.has_room_for(1));
    }
}

#[cfg(test)]
mod prefetch_worker_tests {
    use super::*;
    use std::io::Write;
    use std::sync::{Arc, Mutex};

    #[test]
    fn prefetch_fills_next_window_for_a_sequential_stream() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("b.bin");
        let data: Vec<u8> = (0u64..8 * 1024 * 1024).map(|i| (i % 251) as u8).collect();
        std::fs::File::create(&path)
            .unwrap()
            .write_all(&data)
            .unwrap();
        let file = Arc::new(std::fs::File::open(&path).unwrap());

        let pool = Arc::new(ReadAheadPool::new(64 * 1024 * 1024));
        let buf = Arc::new(Mutex::new(ReadAhead::new(pool.per_stream_cap())));
        let epoch = Arc::new(std::sync::atomic::AtomicU64::new(0));
        pool.register(1, Arc::clone(&buf));

        PrefetchWorkers::run_job(PrefetchJob {
            file: Arc::clone(&file),
            buf: Arc::clone(&buf),
            pool: Arc::clone(&pool),
            epoch: Arc::clone(&epoch),
            dispatched_epoch: 0,
            start: 1024 * 1024,
            len: 1024 * 1024,
            backing_len: data.len() as u64,
        });
        let mut out = vec![0u8; 4096];
        let mut ra = buf.lock().unwrap();
        let mut fills = 0;
        ra.read_into(&mut out, 1024 * 1024, data.len() as u64, |_, _| {
            fills += 1;
            Ok(())
        })
        .unwrap();
        assert_eq!(fills, 0, "prefetched window should serve without a pread");
        assert_eq!(out, data[1024 * 1024..1024 * 1024 + 4096]);
    }
}

#[cfg(test)]
mod concurrency_tests {
    use super::*;
    use std::io::Write;
    use std::os::unix::fs::FileExt;
    use std::sync::Arc;

    #[test]
    fn concurrent_reads_same_handle_match_oracle() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("concurrent.bin");
        #[expect(clippy::cast_sign_loss)]
        let data: Vec<u8> = (0..4 * 1024 * 1024).map(|i| (i % 251) as u8).collect();
        std::fs::File::create(&path)
            .unwrap()
            .write_all(&data)
            .unwrap();
        let file = Arc::new(std::fs::File::open(&path).unwrap());

        let pool = Arc::new(ReadAheadPool::new(64 * 1024 * 1024));
        let buf = Arc::new(Mutex::new(ReadAhead::new(pool.per_stream_cap())));
        let epoch = Arc::new(std::sync::atomic::AtomicU64::new(0));
        pool.register(1, Arc::clone(&buf));

        let backing_len = data.len() as u64;
        let num_threads: u64 = 8;
        let reads_per_thread = 200;

        std::thread::scope(|s| {
            for tid in 0..num_threads {
                let file = Arc::clone(&file);
                let buf = Arc::clone(&buf);
                let pool = Arc::clone(&pool);
                let epoch = Arc::clone(&epoch);
                s.spawn(move || {
                    let br = BackingReader::new(&file, &buf, &pool, 1, backing_len, &epoch);
                    let mut rng_state: u64 = tid * 7919;
                    for _ in 0..reads_per_thread {
                        rng_state = rng_state
                            .wrapping_mul(6_364_136_223_846_793_005)
                            .wrapping_add(1);
                        let off = rng_state % (backing_len.saturating_sub(4096).max(1));
                        #[expect(clippy::cast_possible_truncation)]
                        let len = 4096usize.min((backing_len - off) as usize);
                        let mut got = vec![0u8; len];
                        br.read_exact_at(&mut got, off).unwrap();
                        let mut expected = vec![0u8; len];
                        file.read_exact_at(&mut expected, off).unwrap();
                        assert_eq!(
                            got, expected,
                            "mismatch at off={off} len={len} from thread {tid}"
                        );
                    }
                });
            }
        });
    }

    /// Stress the Phase-2 surface single-stream tests miss: worker-side
    /// `run_job` (store + budget `reconcile`) racing reads, while a second
    /// contended stream forces cross-buffer `try_lock` eviction — all hammering
    /// the shared `charged` budget. Asserts byte-correctness; run under TSan for
    /// race detection.
    #[test]
    #[expect(clippy::cast_possible_truncation)]
    fn workers_and_eviction_race_reads_without_corruption() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("race.bin");
        let data: Vec<u8> = (0u64..4 * 1024 * 1024).map(|i| (i % 251) as u8).collect();
        std::fs::File::create(&path)
            .unwrap()
            .write_all(&data)
            .unwrap();
        let file = Arc::new(std::fs::File::open(&path).unwrap());
        let backing_len = data.len() as u64;

        // Small budget: the worker's has_room_for gate bites and stream 2's
        // misses must evict stream 1 to make room.
        let pool = Arc::new(ReadAheadPool::new(2 * 1024 * 1024));
        let window = pool.per_stream_cap();
        let buf1 = Arc::new(Mutex::new(ReadAhead::new(window)));
        buf1.lock().unwrap().set_max_windows(4);
        let buf2 = Arc::new(Mutex::new(ReadAhead::new(window)));
        let epoch1 = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let epoch2 = Arc::new(std::sync::atomic::AtomicU64::new(0));
        pool.register(1, Arc::clone(&buf1));
        pool.register(2, Arc::clone(&buf2));

        std::thread::scope(|s| {
            // Stream 1: one sequential reader (epoch stays stable, so the
            // prefetched windows actually store + reconcile).
            {
                let (file, buf1, pool, epoch1, data) = (&file, &buf1, &pool, &epoch1, &data);
                s.spawn(move || {
                    let br = BackingReader::new(file, buf1, pool, 1, backing_len, epoch1);
                    let mut off = 0u64;
                    while off < backing_len {
                        let len = (64 * 1024).min((backing_len - off) as usize);
                        let mut got = vec![0u8; len];
                        br.read_exact_at(&mut got, off).unwrap();
                        assert_eq!(got, data[off as usize..off as usize + len]);
                        off += len as u64;
                    }
                });
            }
            // Two prefetchers driving real `run_job` (store + reconcile) for
            // stream 1, concurrent with its reader.
            for _ in 0..2 {
                let (file, buf1, pool, epoch1) = (&file, &buf1, &pool, &epoch1);
                s.spawn(move || {
                    for _ in 0..4 {
                        let mut start = 0u64;
                        while start < backing_len {
                            PrefetchWorkers::run_job(PrefetchJob {
                                file: Arc::clone(file),
                                buf: Arc::clone(buf1),
                                pool: Arc::clone(pool),
                                epoch: Arc::clone(epoch1),
                                dispatched_epoch: epoch1.load(std::sync::atomic::Ordering::Acquire),
                                start,
                                len: window,
                                backing_len,
                            });
                            start += window;
                        }
                    }
                });
            }
            // Stream 2: random-offset readers → misses → permitted_window →
            // cross-buffer eviction of stream 1 (try_lock), churning `charged`.
            for tid in 0..4u64 {
                let (file, buf2, pool, epoch2, data) = (&file, &buf2, &pool, &epoch2, &data);
                s.spawn(move || {
                    let br = BackingReader::new(file, buf2, pool, 2, backing_len, epoch2);
                    let mut rng: u64 = tid * 7919 + 1;
                    for _ in 0..300 {
                        rng = rng.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
                        let off = rng % backing_len.saturating_sub(4096).max(1);
                        let len = 4096usize.min((backing_len - off) as usize);
                        let mut got = vec![0u8; len];
                        br.read_exact_at(&mut got, off).unwrap();
                        assert_eq!(got, data[off as usize..off as usize + len]);
                    }
                });
            }
        });
    }

    /// Regression for the deregister deadlock: `deregister` takes `streams` then
    /// the buffer mutex, while a read holds the buffer mutex then blocking-locks
    /// `streams` via `permitted_window` → eviction. Racing them on the same key
    /// would deadlock if `deregister` held `streams` across the buffer lock.
    /// Asserts bytes stay correct and the run completes; the tsan job's
    /// deadlock detector flags a regression even without an actual hang.
    #[test]
    #[expect(clippy::cast_possible_truncation)]
    fn deregister_races_reads_without_deadlock() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("dereg.bin");
        let data: Vec<u8> = (0u64..2 * 1024 * 1024).map(|i| (i % 251) as u8).collect();
        std::fs::File::create(&path)
            .unwrap()
            .write_all(&data)
            .unwrap();
        let file = Arc::new(std::fs::File::open(&path).unwrap());
        let backing_len = data.len() as u64;

        // Small budget so reads miss and run permitted_window → eviction, which
        // blocking-locks `streams` while the read holds the buffer mutex.
        let pool = Arc::new(ReadAheadPool::new(1024 * 1024));
        let buf = Arc::new(Mutex::new(ReadAhead::new(pool.per_stream_cap())));
        let epoch = Arc::new(std::sync::atomic::AtomicU64::new(0));
        pool.register(1, Arc::clone(&buf));
        // A second registered stream gives eviction a candidate to walk.
        let buf2 = Arc::new(Mutex::new(ReadAhead::new(pool.per_stream_cap())));
        pool.register(2, Arc::clone(&buf2));

        std::thread::scope(|s| {
            for tid in 0..4u64 {
                let (file, buf, pool, epoch, data) = (&file, &buf, &pool, &epoch, &data);
                s.spawn(move || {
                    let br = BackingReader::new(file, buf, pool, 1, backing_len, epoch);
                    let mut rng = tid * 7919 + 1;
                    for _ in 0..300 {
                        rng = rng.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
                        let off = rng % backing_len.saturating_sub(4096).max(1);
                        let len = 4096usize.min((backing_len - off) as usize);
                        let mut got = vec![0u8; len];
                        br.read_exact_at(&mut got, off).unwrap();
                        assert_eq!(got, data[off as usize..off as usize + len]);
                    }
                });
            }
            // Churn registration of the SAME key the readers use — the path that
            // takes `streams` and then the buffer mutex.
            {
                let (pool, buf) = (&pool, &buf);
                s.spawn(move || {
                    for _ in 0..2000 {
                        pool.deregister(1);
                        pool.register(1, Arc::clone(buf));
                    }
                });
            }
        });
    }
}

/// Focused unit tests that pin the read-ahead pool/buffer arithmetic and
/// accounting against the mutation gate (#255): each asserts an EXACT observable
/// (charged bytes, grant size, window size, fill/epoch counts) rather than a
/// loose range, so an operator/return mutation flips the assertion.
#[cfg(test)]
mod mutation_guard_tests {
    use super::*;
    use std::sync::atomic::Ordering as AO;
    use std::sync::{Arc, Mutex};

    #[expect(clippy::unnecessary_wraps)]
    fn fillb(b: &mut [u8], _o: u64) -> io::Result<()> {
        b.fill(7);
        Ok(())
    }

    #[expect(clippy::cast_possible_truncation)]
    fn bk_temp(len: usize) -> (tempfile::TempDir, std::fs::File) {
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bk.bin");
        let data: Vec<u8> = (0..len).map(|i| (i % 251) as u8).collect();
        std::fs::File::create(&path)
            .unwrap()
            .write_all(&data)
            .unwrap();
        (dir, std::fs::File::open(&path).unwrap())
    }

    #[test]
    fn next_expected_equals_consumed_tail() {
        let mut ra = ReadAhead::new(WINDOW_ABS_CAP);
        let mut dst = vec![0u8; 4096];
        ra.read_into(&mut dst, 1000, 8 << 20, fillb).unwrap();
        assert_eq!(ra.next_expected(), 1000 + 4096); // miss path: off + len
        let mut d2 = vec![0u8; 100];
        ra.read_into(&mut d2, 5096, 8 << 20, fillb).unwrap();
        assert_eq!(ra.next_expected(), 5096 + 100); // hit path: off + len
    }

    #[test]
    fn window_doubles_on_sequential_miss_and_floors_on_seek() {
        let mut ra = ReadAhead::new(WINDOW_ABS_CAP);
        let blen = 16 << 20;
        #[expect(clippy::cast_possible_truncation)]
        let mut dst = vec![0u8; WINDOW_FLOOR as usize];
        ra.read_into(&mut dst, 0, blen, fillb).unwrap();
        assert_eq!(ra.window(), WINDOW_FLOOR); // first miss
        ra.read_into(&mut dst, WINDOW_FLOOR, blen, fillb).unwrap();
        assert_eq!(ra.window(), WINDOW_FLOOR * 2); // sequential miss doubles
        ra.read_into(&mut dst, 12 << 20, blen, fillb).unwrap();
        assert_eq!(ra.window(), WINDOW_FLOOR); // seek resets to floor
    }

    #[test]
    fn set_cap_clamps_window_down_only() {
        let mut ra = ReadAhead::new(WINDOW_ABS_CAP);
        ra.set_cap(WINDOW_FLOOR / 2);
        assert_eq!(ra.window(), WINDOW_FLOOR / 2); // window > cap → clamped
        ra.set_cap(WINDOW_ABS_CAP);
        assert_eq!(ra.window(), WINDOW_FLOOR / 2); // window < cap → untouched
    }

    #[test]
    fn reconcile_charges_growth_and_uncharges_shrink() {
        let pool = ReadAheadPool::new(64 << 20);
        pool.reconcile(0, 1000);
        assert_eq!(pool.charged(), 1000);
        pool.reconcile(1000, 250); // shrink must uncharge by old-new
        assert_eq!(pool.charged(), 250);
        pool.reconcile(250, 250); // equal: no change
        assert_eq!(pool.charged(), 250);
    }

    #[test]
    fn has_room_for_zero_need_is_false_when_disabled() {
        assert!(!ReadAheadPool::new(0).has_room_for(0)); // budget==0 guard
        let p = ReadAheadPool::new(1 << 20);
        p.reconcile(0, 1 << 20);
        assert!(!p.has_room_for(1));
    }

    #[test]
    fn permitted_window_need_is_relative_to_old_len() {
        // budget 2 MiB → cap 512K. Charge all but one cap (nothing registered, so
        // nothing is evictable), leaving room == cap. A stream at old_len = cap/4
        // asks for cap: need = cap − cap/4 = 384K ≤ room → full grant. If `need`
        // were `cap + cap/4` (640K > room) it would fall through to the clamp and
        // return old_len + room (640K) instead.
        let pool = ReadAheadPool::new(2 << 20);
        let cap = pool.per_stream_cap();
        pool.reconcile(0, (2 << 20) - cap); // room == cap
        assert_eq!(pool.permitted_window(2, cap / 4, cap), cap);
    }

    #[test]
    fn permitted_window_clamps_to_room_when_nothing_evictable() {
        // budget 2 MiB → cap 512K. Charge to leave room == 256K. A stream at
        // old_len = 128K asks for cap: need = 384K > room, nothing evictable, so
        // the grant clamps to old_len + room = 384K. `old_len − room` underflows.
        let pool = ReadAheadPool::new(2 << 20);
        let cap = pool.per_stream_cap();
        pool.reconcile(0, (2 << 20) - 256 * 1024); // room == 256K
        assert_eq!(
            pool.permitted_window(2, 128 * 1024, cap),
            128 * 1024 + 256 * 1024
        );
    }

    #[test]
    #[expect(clippy::cast_possible_truncation)]
    fn touch_keeps_a_stream_off_the_eviction_block() {
        let pool = ReadAheadPool::new(2 << 20); // cap 512K, holds 4 streams
        let cap = pool.per_stream_cap();
        let mk = |key: usize| {
            let arc = Arc::new(Mutex::new(ReadAhead::new(cap)));
            let mut d = vec![0u8; cap as usize];
            let (o, n) = arc
                .lock()
                .unwrap()
                .read_into(&mut d, 0, cap * 4, fillb)
                .unwrap();
            pool.register(key, Arc::clone(&arc));
            pool.reconcile(o, n);
            arc
        };
        let s1 = mk(1);
        let s2 = mk(2);
        let _s3 = mk(3);
        let _s4 = mk(4); // budget full (4×cap)
        pool.touch(1); // stream 1 now most-recent → stream 2 is coldest
        let hot = Arc::new(Mutex::new(ReadAhead::new(cap)));
        pool.register(5, Arc::clone(&hot));
        pool.permitted_window(5, 0, cap); // must evict the coldest OTHER (stream 2)
        assert_eq!(s2.lock().unwrap().len(), 0, "coldest stream evicted");
        assert!(s1.lock().unwrap().len() > 0, "touched stream survives");
    }

    #[test]
    fn fills_count_is_exact() {
        let (_d, file) = bk_temp(256 * 1024);
        let mut d = vec![0u8; 4096];
        // Disabled pool: every read is one physical fill.
        let pool0 = ReadAheadPool::new(0);
        let buf0 = Arc::new(Mutex::new(ReadAhead::new(0)));
        let ep0 = std::sync::atomic::AtomicU64::new(0);
        let br0 = BackingReader::new(&file, &buf0, &pool0, 0, 256 * 1024, &ep0);
        br0.read_exact_at(&mut d, 0).unwrap();
        br0.read_exact_at(&mut d, 100_000).unwrap();
        assert_eq!(br0.fills(), 2);
        // Enabled pool: a cold read is one amplified fill; the next sequential
        // read hits the window and adds no fill.
        let pool = ReadAheadPool::new(64 << 20);
        let buf = Arc::new(Mutex::new(ReadAhead::new(pool.per_stream_cap())));
        pool.register(1, Arc::clone(&buf));
        let ep = std::sync::atomic::AtomicU64::new(0);
        let br = BackingReader::new(&file, &buf, &pool, 1, 256 * 1024, &ep);
        br.read_exact_at(&mut d, 0).unwrap();
        assert_eq!(br.fills(), 1);
        br.read_exact_at(&mut d, 4096).unwrap();
        assert_eq!(br.fills(), 1);
    }

    #[test]
    fn epoch_bumps_on_seek_not_on_sequential() {
        let (_d, file) = bk_temp(1 << 20);
        let pool = ReadAheadPool::new(64 << 20);
        let buf = Arc::new(Mutex::new(ReadAhead::new(pool.per_stream_cap())));
        pool.register(1, Arc::clone(&buf));
        let ep = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let br = BackingReader::new(&file, &buf, &pool, 1, 1 << 20, &ep);
        let mut d = vec![0u8; 4096];
        br.read_exact_at(&mut d, 0).unwrap(); // first read: a "seek" off MAX
        let base = ep.load(AO::Relaxed);
        br.read_exact_at(&mut d, 4096).unwrap(); // sequential hit
        assert_eq!(ep.load(AO::Relaxed), base, "sequential must not bump epoch");
        br.read_exact_at(&mut d, 900_000).unwrap(); // genuine seek (miss, off != next)
        assert_eq!(ep.load(AO::Relaxed), base + 1, "seek bumps epoch");
    }

    #[test]
    fn ring_trim_evicts_window_fully_behind_the_reader() {
        let mut ra = ReadAhead::new(WINDOW_ABS_CAP);
        ra.set_max_windows(2);
        ra.store_window(0, vec![0u8; 1000]); // [0, 1000)
        ra.store_window(1000, vec![0u8; 1000]); // [1000, 2000)
        // Advance the reader to 2000 so both windows are fully behind it.
        let mut dst = vec![0u8; 10];
        ra.read_into(&mut dst, 1990, 1 << 20, fillb).unwrap(); // hit → next_expected = 2000
        // A third window forces a trim: the victim is the fully-behind window with
        // the smallest start ([0,1000)) — NOT the just-stored ahead window. The
        // `<=` filter (`start+len <= frontier`) selects which windows are behind.
        ra.store_window(2000, vec![0u8; 1000]); // [2000, 3000)
        assert!(!ra.covers(0, 10), "fully-behind window evicted");
        assert!(ra.covers(1000, 10), "nearer behind window kept");
        assert!(ra.covers(2000, 10), "just-stored ahead window kept");
    }

    #[test]
    fn plan_prefetch_no_jobs_when_watermark_at_horizon() {
        // Watermark already at the horizon (start + depth*window): nothing new to
        // dispatch. The `s > horizon` reset guard must NOT fire at s == horizon
        // (`>=` would reset to start and re-queue the whole horizon).
        let win = WINDOW_FLOOR;
        let depth = 4;
        let start = 10 * win;
        let horizon = start + depth * win;
        let (starts, upto) = plan_prefetch(horizon, start, win, depth, 100 << 20);
        assert!(starts.is_empty(), "caught up to horizon → no jobs");
        assert_eq!(upto, horizon);
    }

    #[test]
    fn plan_prefetch_window_zero_leaves_watermark_unchanged() {
        // window == 0 short-circuits via the `||` early-out, returning the
        // watermark UNCHANGED. With `&&` it would fall through and return `start`.
        let (starts, upto) = plan_prefetch(500, 1000, 0, 4, 1 << 20);
        assert!(starts.is_empty());
        assert_eq!(upto, 500);
    }
}
