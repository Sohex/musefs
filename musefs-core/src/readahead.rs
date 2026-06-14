//! Per-handle backing read-ahead: an adaptive window over raw backing-file
//! bytes, a global byte budget with eviction, and the `BackingReader` shim that
//! every backing read flows through. See
//! `docs/superpowers/specs/2026-06-14-read-ahead-overlap-design.md`.

use std::io;

/// Floor window size: a fresh or just-seeked stream still reads this much ahead.
pub const WINDOW_FLOOR: u64 = 512 * 1024;
/// Absolute per-stream window cap, independent of the global budget.
pub const WINDOW_ABS_CAP: u64 = 8 * 1024 * 1024;

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
