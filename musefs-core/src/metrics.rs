//! Optional syscall/query counters and per-syscall latency injection for
//! benchmarking. Zero-cost when the `metrics` feature is off: every hook
//! compiles to an empty inline fn, so call sites stay unconditional and clean.
//!
//! Counting scope: `on_open`/`on_stat` count every backing-file open and
//! metadata syscall on any read path; `on_open` fires on the open *attempt*
//! (a failed open is still a syscall). `on_pread` counts positioned backing
//! reads on the serve path, attempt-based: one pread plus the attempted
//! buffer length, recorded before the read (a failed or short read is still
//! a round-trip, and the `MUSEFS_FAULT_PREAD_US` injection applies to it).
//! For `BackingAudio` segments, bytes attempted equal bytes served; on the
//! Ogg path (page-index scans, CRC probes, header and payload reads) bytes
//! attempted may exceed bytes served, because scan and header bytes are
//! patched or discarded — the counter reports backing I/O performed, not
//! output produced. Art-blob and binary-tag chunks are DB reads, tracked by
//! call count (`on_art_chunk`/`on_binary_tag_chunk`), not byte-counted.
//! `on_scan_open`/`on_scan_read` count backing-file opens and positioned
//! reads on the *scan* path (distinct from the serve path); `on_scan_read`
//! also accumulates bytes read, analogous to `on_pread`.
//! Counters measure *daemon* work, not user traffic: StructureOnly reads
//! served via kernel passthrough never reach userspace and are invisible to
//! `on_pread` — by design (the passthrough e2e test asserts exactly this).

/// Single source of the counter list: `public Snapshot field => backing static`.
/// The `Snapshot` struct below and the metrics-on statics / `snapshot()` /
/// `reset()` are all generated from this one list (the x-macro / callback
/// pattern), so adding a counter is a one-line edit here instead of four edits
/// scattered across both `imp` modules.
macro_rules! for_each_counter {
    ($cb:ident) => {
        $cb! {
            opens => OPENS,
            stats => STATS,
            preads => PREADS,
            pread_bytes => PREAD_BYTES,
            art_chunks => ART_CHUNKS,
            binary_tag_chunks => BINARY_TAG_CHUNKS,
            scan_opens => SCAN_OPENS,
            scan_preads => SCAN_PREADS,
            scan_bytes_read => SCAN_BYTES_READ,
            readahead_hits => READAHEAD_HITS,
            readahead_misses => READAHEAD_MISSES,
        }
    };
}

macro_rules! decl_snapshot {
    ($($field:ident => $stat:ident),* $(,)?) => {
        /// Counter totals sampled by `snapshot`; zeroable by `reset`. Generated
        /// from `for_each_counter!` so the fields stay in lockstep with the
        /// backing statics. Present (and `Default`) even without the `metrics`
        /// feature so consumers compile unconditionally.
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
        pub struct Snapshot {
            $(pub $field: u64,)*
        }
    };
}
for_each_counter!(decl_snapshot);

pub use imp::*;

#[cfg(feature = "metrics")]
mod imp {
    use std::sync::atomic::{AtomicU8, AtomicU64, AtomicUsize, Ordering};
    use std::sync::{Mutex, MutexGuard, OnceLock};
    use std::time::Duration;

    // The counter statics, `snapshot()`, and `reset()` are all generated from the
    // one `for_each_counter!` list (see the top of the file) so they can't drift.
    macro_rules! decl_counters {
        ($($field:ident => $stat:ident),* $(,)?) => {
            $(static $stat: AtomicU64 = AtomicU64::new(0);)*

            pub fn snapshot() -> super::Snapshot {
                super::Snapshot {
                    $($field: $stat.load(Ordering::Relaxed),)*
                }
            }

            pub fn reset() {
                $($stat.store(0, Ordering::Relaxed);)*
            }
        };
    }
    for_each_counter!(decl_counters);

    static PREAD_FAULT: OnceLock<Option<Duration>> = OnceLock::new();

    // Backing-read fault seam (test-only; process-global so it reaches the FUSE
    // worker thread that actually performs the read — a thread-local set on the
    // test thread would not). Kind: 0=none, 1=EIO, 2=short read. Distinct from
    // the latency-only `set_fault_pread` hook above.
    static BACKING_FAULT_KIND: AtomicU8 = AtomicU8::new(0);
    static BACKING_FAULT_PREFIX: AtomicUsize = AtomicUsize::new(0);
    // Serializes fault scopes: the seam is process-global, so two fault tests in
    // the same test binary would otherwise clobber each other's kind when cargo
    // runs them on parallel threads. `set_backing_fault` holds this for the life
    // of its guard; the serve/worker path only loads the atomics, never locks.
    static SEAM_LOCK: Mutex<()> = Mutex::new(());

    /// A simulated backing-read failure, set per test via [`set_backing_fault`].
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum BackingFault {
        /// Return `EIO` instead of reading any bytes.
        Eio,
        /// Fill the first `prefix` bytes from the file, then return
        /// `UnexpectedEof` (simulating a truncated/short pread).
        ShortRead { prefix: usize },
    }

    /// Clears the global backing fault when dropped (and releases [`SEAM_LOCK`]),
    /// so a fault never leaks past the test that set it.
    #[must_use = "the fault is cleared when this guard drops; bind it to a name"]
    pub struct BackingFaultGuard(
        // Held for the guard's lifetime to keep [`SEAM_LOCK`] locked; released on
        // drop. Never read directly — the RAII effect is the point.
        #[allow(dead_code)] MutexGuard<'static, ()>,
    );

    impl Drop for BackingFaultGuard {
        fn drop(&mut self) {
            BACKING_FAULT_KIND.store(0, Ordering::SeqCst);
        }
    }

    /// Install a backing-read fault for the current test scope. The seam is
    /// process-global; this serializes on [`SEAM_LOCK`] so concurrent fault tests
    /// in one binary take turns rather than clobbering each other's kind. The
    /// lock is held until the returned guard drops.
    pub fn set_backing_fault(fault: BackingFault) -> BackingFaultGuard {
        // Recover from a poisoned lock: a panicking fault test fails on its own;
        // it must not cascade into every later fault test.
        let lock = SEAM_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match fault {
            BackingFault::Eio => {
                BACKING_FAULT_KIND.store(1, Ordering::SeqCst);
            }
            BackingFault::ShortRead { prefix } => {
                BACKING_FAULT_PREFIX.store(prefix, Ordering::SeqCst);
                BACKING_FAULT_KIND.store(2, Ordering::SeqCst);
            }
        }
        BackingFaultGuard(lock)
    }

    /// Positioned backing read used by the serve path. Honors an injected fault
    /// when one is set; otherwise a plain `read_exact_at`. The no-fault path is a
    /// single relaxed atomic load.
    pub fn backing_read_exact_at(
        f: &std::fs::File,
        buf: &mut [u8],
        offset: u64,
    ) -> std::io::Result<()> {
        use std::os::unix::fs::FileExt;
        match BACKING_FAULT_KIND.load(Ordering::SeqCst) {
            // EIO is 5 on Linux, macOS, and FreeBSD.
            1 => return Err(std::io::Error::from_raw_os_error(5)),
            2 => {
                let p = BACKING_FAULT_PREFIX.load(Ordering::SeqCst).min(buf.len());
                f.read_exact_at(&mut buf[..p], offset)?;
                return Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "injected short backing read",
                ));
            }
            _ => {}
        }
        f.read_exact_at(buf, offset)
    }

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
        fault("MUSEFS_FAULT_PREAD_US", &PREAD_FAULT);
    }

    pub fn set_fault_pread(d: Option<Duration>) {
        let first_set = PREAD_FAULT.set(d).is_ok();
        debug_assert!(
            first_set,
            "set_fault_pread must run before the first on_pread"
        );
    }

    pub fn on_art_chunk() {
        ART_CHUNKS.fetch_add(1, Ordering::Relaxed);
    }

    pub fn on_binary_tag_chunk() {
        BINARY_TAG_CHUNKS.fetch_add(1, Ordering::Relaxed);
    }

    /// One backing-file open on the *scan* path (distinct from serve-path `on_open`).
    pub fn on_scan_open() {
        SCAN_OPENS.fetch_add(1, Ordering::Relaxed);
    }

    /// One positioned scan-path read of `bytes` bytes (prefix, widen, or tail read).
    pub fn on_scan_read(bytes: u64) {
        SCAN_PREADS.fetch_add(1, Ordering::Relaxed);
        SCAN_BYTES_READ.fetch_add(bytes, Ordering::Relaxed);
    }

    pub fn on_readahead_hit() {
        READAHEAD_HITS.fetch_add(1, Ordering::Relaxed);
    }

    pub fn on_readahead_miss() {
        READAHEAD_MISSES.fetch_add(1, Ordering::Relaxed);
    }
}

#[cfg(not(feature = "metrics"))]
mod imp {
    #[inline(always)]
    pub fn on_open() {}
    #[inline(always)]
    pub fn on_stat() {}
    #[inline(always)]
    pub fn on_pread(_bytes: u64) {}
    #[inline(always)]
    pub fn set_fault_pread(_d: Option<std::time::Duration>) {}
    #[inline(always)]
    pub fn backing_read_exact_at(
        f: &std::fs::File,
        buf: &mut [u8],
        offset: u64,
    ) -> std::io::Result<()> {
        use std::os::unix::fs::FileExt;
        f.read_exact_at(buf, offset)
    }
    #[inline(always)]
    pub fn on_art_chunk() {}
    #[inline(always)]
    pub fn on_binary_tag_chunk() {}
    #[inline(always)]
    pub fn on_scan_open() {}
    #[inline(always)]
    pub fn on_scan_read(_bytes: u64) {}
    #[inline(always)]
    pub fn on_readahead_hit() {}
    #[inline(always)]
    pub fn on_readahead_miss() {}
    #[inline(always)]
    pub fn snapshot() -> super::Snapshot {
        super::Snapshot::default()
    }
    #[inline(always)]
    pub fn reset() {}
}

#[cfg(all(test, feature = "metrics"))]
mod tests {
    use std::sync::Mutex;

    use super::*;

    // The counters/`reset()` and the backing-fault seam are process-global, so
    // every test that touches them serializes on this shared lock rather than
    // clobbering each other when cargo runs them on parallel threads in this
    // binary. The fault tests must hold it for their WHOLE body (not just the
    // `set_backing_fault` window) because they also do un-guarded baseline reads
    // through the seam. It is distinct from `SEAM_LOCK` (which `set_backing_fault`
    // takes internally) so holding it here can't deadlock against that.
    static GLOBAL_STATE_LOCK: Mutex<()> = Mutex::new(());

    fn lock_global_state() -> std::sync::MutexGuard<'static, ()> {
        GLOBAL_STATE_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    #[test]
    fn counters_accumulate_and_reset() {
        let _lock = lock_global_state();
        reset();
        on_open();
        on_open();
        on_pread(100);
        on_art_chunk();
        on_binary_tag_chunk();
        on_readahead_miss();
        let s = snapshot();
        assert_eq!(s.opens, 2);
        assert_eq!(s.preads, 1);
        assert_eq!(s.pread_bytes, 100);
        assert_eq!(s.art_chunks, 1);
        assert_eq!(s.binary_tag_chunks, 1);
        assert_eq!(s.readahead_misses, 1);
        assert_eq!(s.readahead_hits, 0);
        reset();
        assert_eq!(snapshot(), Snapshot::default());
    }

    #[test]
    fn scan_counters_accumulate_and_reset() {
        let _lock = lock_global_state();
        reset();
        on_scan_open();
        on_scan_read(4096);
        on_scan_read(128);
        let s = snapshot();
        assert_eq!(s.scan_opens, 1);
        assert_eq!(s.scan_preads, 2);
        assert_eq!(s.scan_bytes_read, 4096 + 128);
        reset();
        assert_eq!(snapshot(), Snapshot::default());
    }

    #[test]
    fn backing_fault_injects_eio_then_clears_on_drop() {
        use std::io::Write;
        use std::os::unix::fs::FileExt;

        // Held for the whole test: the baseline (no-fault) reads below run
        // outside the `set_backing_fault` guard, so without this a concurrent
        // fault test could inject a fault into them.
        let _lock = lock_global_state();

        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        tmp.write_all(b"hello world").unwrap();
        let f = std::fs::File::open(tmp.path()).unwrap();

        // No fault: real read succeeds.
        let mut buf = [0u8; 5];
        backing_read_exact_at(&f, &mut buf, 0).unwrap();
        assert_eq!(&buf, b"hello");

        {
            let _guard = set_backing_fault(BackingFault::Eio);
            let err = backing_read_exact_at(&f, &mut buf, 0).unwrap_err();
            assert_eq!(err.raw_os_error(), Some(5), "EIO == 5");
        }

        // Guard dropped: fault cleared, real read works again.
        let mut buf2 = [0u8; 5];
        backing_read_exact_at(&f, &mut buf2, 6).unwrap();
        assert_eq!(&buf2, b"world");

        // Sanity: the std read path still fills the same bytes.
        let mut direct = [0u8; 5];
        f.read_exact_at(&mut direct, 0).unwrap();
        assert_eq!(&direct, b"hello");
    }

    #[test]
    fn backing_fault_short_read_fills_prefix_then_errors() {
        use std::io::Write;
        let _lock = lock_global_state();
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        tmp.write_all(b"abcdefgh").unwrap();
        let f = std::fs::File::open(tmp.path()).unwrap();

        let mut buf = [0u8; 8];
        let _guard = set_backing_fault(BackingFault::ShortRead { prefix: 3 });
        let err = backing_read_exact_at(&f, &mut buf, 0).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::UnexpectedEof);
        assert_eq!(
            &buf[..3],
            b"abc",
            "prefix bytes were filled before the fault"
        );
    }
}
