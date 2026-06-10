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

pub use imp::*;

#[cfg(feature = "metrics")]
mod imp {
    use std::sync::atomic::{AtomicU8, AtomicU64, AtomicUsize, Ordering};
    use std::sync::{Mutex, MutexGuard, OnceLock};
    use std::time::Duration;

    static OPENS: AtomicU64 = AtomicU64::new(0);
    static STATS: AtomicU64 = AtomicU64::new(0);
    static PREADS: AtomicU64 = AtomicU64::new(0);
    static PREAD_BYTES: AtomicU64 = AtomicU64::new(0);
    static ART_CHUNKS: AtomicU64 = AtomicU64::new(0);
    static BINARY_TAG_CHUNKS: AtomicU64 = AtomicU64::new(0);
    static SCAN_OPENS: AtomicU64 = AtomicU64::new(0);
    static SCAN_PREADS: AtomicU64 = AtomicU64::new(0);
    static SCAN_BYTES_READ: AtomicU64 = AtomicU64::new(0);
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

    #[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
    pub struct Snapshot {
        pub opens: u64,
        pub stats: u64,
        pub preads: u64,
        pub pread_bytes: u64,
        pub art_chunks: u64,
        pub binary_tag_chunks: u64,
        pub scan_opens: u64,
        pub scan_preads: u64,
        pub scan_bytes_read: u64,
    }

    pub fn snapshot() -> Snapshot {
        Snapshot {
            opens: OPENS.load(Ordering::Relaxed),
            stats: STATS.load(Ordering::Relaxed),
            preads: PREADS.load(Ordering::Relaxed),
            pread_bytes: PREAD_BYTES.load(Ordering::Relaxed),
            art_chunks: ART_CHUNKS.load(Ordering::Relaxed),
            binary_tag_chunks: BINARY_TAG_CHUNKS.load(Ordering::Relaxed),
            scan_opens: SCAN_OPENS.load(Ordering::Relaxed),
            scan_preads: SCAN_PREADS.load(Ordering::Relaxed),
            scan_bytes_read: SCAN_BYTES_READ.load(Ordering::Relaxed),
        }
    }

    pub fn reset() {
        OPENS.store(0, Ordering::Relaxed);
        STATS.store(0, Ordering::Relaxed);
        PREADS.store(0, Ordering::Relaxed);
        PREAD_BYTES.store(0, Ordering::Relaxed);
        ART_CHUNKS.store(0, Ordering::Relaxed);
        BINARY_TAG_CHUNKS.store(0, Ordering::Relaxed);
        SCAN_OPENS.store(0, Ordering::Relaxed);
        SCAN_PREADS.store(0, Ordering::Relaxed);
        SCAN_BYTES_READ.store(0, Ordering::Relaxed);
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
        pub binary_tag_chunks: u64,
        pub scan_opens: u64,
        pub scan_preads: u64,
        pub scan_bytes_read: u64,
    }

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
    pub fn snapshot() -> Snapshot {
        Snapshot::default()
    }
    #[inline(always)]
    pub fn reset() {}
}

#[cfg(all(test, feature = "metrics"))]
mod tests {
    use std::sync::Mutex;

    use super::*;

    // The counters and `reset()` are process-global, so these tests serialize on
    // a shared lock rather than clobbering each other when run on parallel
    // threads in this binary.
    static COUNTERS_LOCK: Mutex<()> = Mutex::new(());

    fn lock_counters() -> std::sync::MutexGuard<'static, ()> {
        COUNTERS_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    #[test]
    fn counters_accumulate_and_reset() {
        let _lock = lock_counters();
        reset();
        on_open();
        on_open();
        on_pread(100);
        on_art_chunk();
        on_binary_tag_chunk();
        let s = snapshot();
        assert_eq!(s.opens, 2);
        assert_eq!(s.preads, 1);
        assert_eq!(s.pread_bytes, 100);
        assert_eq!(s.art_chunks, 1);
        assert_eq!(s.binary_tag_chunks, 1);
        reset();
        assert_eq!(snapshot(), Snapshot::default());
    }

    #[test]
    fn scan_counters_accumulate_and_reset() {
        let _lock = lock_counters();
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
