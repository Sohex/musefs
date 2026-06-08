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
    use std::sync::OnceLock;
    use std::sync::atomic::{AtomicU64, Ordering};
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
    use super::*;

    #[test]
    fn counters_accumulate_and_reset() {
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
}
