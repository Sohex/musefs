//! FUSE filesystem binding for musefs: translates VFS calls into `musefs-core`
//! operations. fuser dispatches on a single thread; blocking operations are
//! offloaded onto a bounded worker pool and answered via the `Send` reply
//! objects, so a slow backing read cannot stall metadata operations.

use std::ffi::OsStr;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::{Duration, SystemTime};

use threadpool::ThreadPool;

use crate::convert::{assemble_dir_listing, make_attr, to_file_attr};
use fuser::{
    AccessFlags, BackgroundSession, Config, FileAttr, FileHandle, FileType, Filesystem, FopenFlags,
    Generation, INodeNo, InitFlags, KernelConfig, LockOwner, Notifier, OpenAccMode, OpenFlags,
    ReplyAttr, ReplyData, ReplyDirectory, ReplyDirectoryPlus, ReplyEmpty, ReplyEntry, ReplyOpen,
    ReplyStatfs, ReplyXattr, Request, Session,
};
use musefs_core::CoreError;
use musefs_core::Fh;
use musefs_core::Musefs;
use musefs_core::TreeSnapshot;
use musefs_core::convert::usize_from;
use musefs_core::serve_warn;
use std::num::NonZeroU64;

mod convert;
mod metrics_dir;
mod platform;

/// Per-worker read scratch buffer: each threadpool worker reuses one Vec across
/// reads (filled by `Musefs::read_into`, sent as fuser's borrowed iovec), so the
/// hot path allocates nothing per read. Capacity is clamped after use so one
/// giant read doesn't pin memory for the worker's lifetime.
const MAX_RETAINED_READ_BUF: usize = 2 * 1024 * 1024;
thread_local! {
    static READ_BUF: std::cell::RefCell<Vec<u8>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// Fuse-layer mount knobs: kernel tuning, page-cache policy, and the ownership
/// (`uid`/`gid`) and permission bits (`file_mode`/`dir_mode`) presented for
/// every entry. Distinct from `musefs_core::MountConfig`, which governs how the
/// virtual tree is rendered.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct FuseConfig {
    /// Entry/attr cache lifetime the kernel may trust before re-validating.
    /// Longer cuts `lookup`/`getattr` traffic but bounds how fast external DB
    /// edits become visible (the existing freshness trade-off).
    pub ttl: Duration,
    /// Kernel read-ahead window in bytes (clamped to the kernel's max).
    pub max_readahead: u32,
    /// Max outstanding background (readahead/async) requests the kernel queues.
    /// Caps that class of work delivered to the pool; foreground reads are
    /// bounded separately by `MAX_INFLIGHT_READS` (#308), not by this.
    pub max_background: u16,
    /// Keep the kernel page cache across opens (`FOPEN_KEEP_CACHE`). On by
    /// default (#432): the one measured storage win (~3× faster repeat-open on
    /// HDD/NFS). An external re-tag auto-invalidates the affected inode on
    /// refresh (`poll_refresh_notify` → `inval_inode`), so cached bytes are
    /// dropped when content changes.
    pub keep_cache: bool,
    /// uid presented for every entry (the marker, synthetic dirs, real files).
    pub uid: u32,
    /// gid presented for every entry.
    pub gid: u32,
    /// Permission bits for regular files (bare mode word, no type bits).
    pub file_mode: u16,
    /// Permission bits for directories (bare mode word, no type bits).
    pub dir_mode: u16,
    /// Mount with `allow_other` + `default_permissions`: accounts other than the
    /// mounting user can reach the mount and the kernel enforces the presented
    /// owner/mode bits. Non-root mounts also require `user_allow_other` in
    /// `/etc/fuse.conf` (validated at mount time).
    pub allow_other: bool,
    /// Expose the `/proc`-style `.musefs-metrics/` telemetry namespace at the
    /// mount root (#394). Default off; named distinctly from the compile-time
    /// `metrics` cargo feature (which gates the syscall counters).
    pub expose_metrics: bool,
    /// Worker-pool size for offloaded ops (reads, metadata synthesis).
    /// `0` = auto: 2× the CPU count, oversized because the work is I/O-bound.
    /// Each worker lazily opens its own read-only DB connection, so this also
    /// bounds the SQLite connection count — steady-state memory scales with it
    /// (#631). Lower it on memory-constrained or many-core hosts.
    pub workers: usize,
    /// Test-only: the worker pool's metadata admission cap, in place of
    /// `MAX_QUEUED_JOBS` (#694), so a mount test can put the pool over it on
    /// demand. `None` keeps the real cap.
    #[cfg(feature = "test-support")]
    #[doc(hidden)]
    pub pool_admission_cap: Option<usize>,
    /// Test-only: the directory-handle cap, in place of `MAX_DIR_HANDLES`
    /// (#616), so a mount test can serve every `opendir` statelessly. `None`
    /// keeps the real cap.
    #[cfg(feature = "test-support")]
    #[doc(hidden)]
    pub dir_handle_cap: Option<usize>,
    /// Test-only: how many listings stateless enumerations keep pinned, in
    /// place of `MAX_STATELESS_LISTINGS` (#695), so a mount test can evict one
    /// on demand. `None` keeps the real cap.
    #[cfg(feature = "test-support")]
    #[doc(hidden)]
    pub stateless_listing_cap: Option<usize>,
    /// Test-only: record every job the mount hands its worker pool, with the
    /// route it took (#694). `None` records nothing.
    #[cfg(feature = "test-support")]
    #[doc(hidden)]
    pub route_trace: Option<RouteTrace>,
}

impl Default for FuseConfig {
    fn default() -> FuseConfig {
        FuseConfig {
            ttl: Duration::from_secs(1),
            max_readahead: 512 * 1024,
            max_background: 64,
            keep_cache: true,
            uid: rustix::process::getuid().as_raw(),
            gid: rustix::process::getgid().as_raw(),
            file_mode: 0o444,
            dir_mode: 0o555,
            allow_other: false,
            expose_metrics: false,
            workers: 0,
            #[cfg(feature = "test-support")]
            pool_admission_cap: None,
            #[cfg(feature = "test-support")]
            dir_handle_cap: None,
            #[cfg(feature = "test-support")]
            stateless_listing_cap: None,
            #[cfg(feature = "test-support")]
            route_trace: None,
        }
    }
}

/// `FOPEN_*` flags for an `open` reply, derived from the cache policy.
fn open_flags(keep_cache: bool) -> FopenFlags {
    if keep_cache {
        FopenFlags::FOPEN_KEEP_CACHE
    } else {
        FopenFlags::empty()
    }
}

pub use musefs_core::AllocatorStats;

/// A process-wide allocator-stats probe, installed by [`set_alloc_probe`].
///
/// The probe lives in the final binary, not here: reading jemalloc stats means
/// linking `tikv-jemalloc-ctl` (and the vendored jemalloc C lib), and only the
/// binary that installs `tikv-jemallocator` as its `#[global_allocator]` can do
/// that coherently. Linking it into this library would, via workspace feature
/// unification, pull a second jemalloc into every `cargo test --workspace`
/// binary — which segfaults on FreeBSD (base libc *is* jemalloc). So the binary
/// injects a probe and this layer stays allocator-agnostic (#394).
pub type AllocProbe = fn() -> Option<AllocatorStats>;

static ALLOC_PROBE: OnceLock<AllocProbe> = OnceLock::new();

/// Install the allocator-stats probe (call once at startup, before mounting).
/// Later calls are ignored. Without it, `.musefs-metrics` omits the alloc block.
pub fn set_alloc_probe(probe: AllocProbe) {
    let _ = ALLOC_PROBE.set(probe);
}

/// Run the installed allocator probe, or `None` if none was installed.
fn allocator_stats() -> Option<AllocatorStats> {
    ALLOC_PROBE.get().and_then(|probe| probe())
}

/// Serve-path syscall counters, present only on a `metrics`-feature build.
#[cfg(feature = "metrics")]
#[allow(clippy::unnecessary_wraps)]
fn syscall_snapshot() -> Option<musefs_core::metrics::Snapshot> {
    Some(musefs_core::metrics::snapshot())
}

#[cfg(not(feature = "metrics"))]
fn syscall_snapshot() -> Option<musefs_core::metrics::Snapshot> {
    None
}

/// Synthetic `statfs` reply values (#368). musefs is a read-only passthrough
/// with no single backing volume to mirror (backing files are per-track and may
/// span devices), so we advertise a large, fully-free synthetic capacity rather
/// than fuser's default all-zero reply — which makes `df` report a 0-byte
/// filesystem and can make capacity-checking importers (Lidarr et al.) refuse to
/// operate. Returns the `ReplyStatfs::statfs` argument tuple:
/// `(blocks, bfree, bavail, files, ffree, bsize, namelen, frsize)`.
fn statfs_params() -> (u64, u64, u64, u64, u64, u32, u32, u32) {
    const BSIZE: u32 = 512;
    const NAMELEN: u32 = 255;
    // 1 TiB advertised capacity, reported entirely free — read-only, so nothing
    // is "used" in a writable sense, and 1 TiB clears typical free-space checks.
    const CAPACITY_BYTES: u64 = 1 << 40;
    const TOTAL_INODES: u64 = 1 << 32;
    let blocks = CAPACITY_BYTES / u64::from(BSIZE);
    (
        blocks,
        blocks,
        blocks,
        TOTAL_INODES,
        TOTAL_INODES,
        BSIZE,
        NAMELEN,
        BSIZE,
    )
}

/// Map a core error onto a POSIX errno for the FUSE reply. `Io` errors carry the
/// underlying errno when present; everything structural collapses to `EIO`, and
/// so does a variant [`placed_errno`] has not placed yet.
pub fn errno(err: &CoreError) -> fuser::Errno {
    placed_errno(err).unwrap_or(fuser::Errno::EIO)
}

/// The errno for a variant this mapping names, or `None` for one it does not.
///
/// `CoreError` is `#[non_exhaustive]` (#708), so the match needs a wildcard,
/// and a variant added to `musefs-core` falls into it without a compile error.
/// Keeping that fallback out of the named arms is what lets a test tell a
/// variant deliberately mapped to `EIO` from one nobody has placed.
fn placed_errno(err: &CoreError) -> Option<fuser::Errno> {
    Some(match err {
        CoreError::NoEntry(_) | CoreError::TrackNotFound(_) => fuser::Errno::ENOENT,
        CoreError::IsDir(_) => fuser::Errno::EISDIR,
        CoreError::NotADir(_) => fuser::Errno::ENOTDIR,
        CoreError::HandleTableFull => fuser::Errno::ENFILE,
        CoreError::Io(e) => fuser::Errno::from_i32(e.raw_os_error().unwrap_or(libc::EIO)),
        CoreError::BackingIo { source, .. } => {
            fuser::Errno::from_i32(source.raw_os_error().unwrap_or(libc::EIO))
        }
        CoreError::BackingChanged(_)
        | CoreError::DerivedStateStale(_)
        | CoreError::Db(_)
        | CoreError::DbOpen { .. }
        | CoreError::Mp4MetadataTooLarge { .. }
        | CoreError::OrphanedArt { .. }
        | CoreError::ArtTooLarge { .. }
        | CoreError::InvalidPictureType { .. }
        | CoreError::HeaderTooLarge { .. }
        // Scan-time refusals (#644). They cannot reach a FUSE reply — nothing
        // over-cap is ever written — but EIO is the right collapse if the
        // scanner's validation is ever reused on a serve path.
        | CoreError::TrackFieldTooLarge { .. }
        | CoreError::TrackMetadataTooLarge { .. }
        | CoreError::Format(_)
        | CoreError::InvalidTemplate(_) => fuser::Errno::EIO,
        // A variant added after this list: `errno` collapses it to `EIO` until
        // it is given a place above.
        _ => return None,
    })
}

/// Log a serve-path failure before it collapses to an errno reply, so the
/// cause (e.g. the offending path in `BackingChanged`, or an `Io` error with
/// no raw OS errno) is not lost. Routine tree-shape misses — a stale inode
/// after a refresh, kernel path probing — stay at debug to avoid noise, and
/// the warn arm goes through the process-wide limiter in `musefs-core` so
/// per-file failures (a walk over missing backing files) can't scale the log
/// with library size. The limiter is shared with core's own synthesis warns
/// (#650): one serve path, one budget.
fn reply_errno(op: &str, ino: u64, err: &CoreError) -> fuser::Errno {
    match err {
        CoreError::NoEntry(_)
        | CoreError::TrackNotFound(_)
        | CoreError::IsDir(_)
        | CoreError::NotADir(_) => log::debug!("{op}({ino}) failed: {err}"),
        _ => serve_warn!("{op}({ino}) failed: {err}"),
    }
    errno(err)
}

/// Best-effort text of a caught panic payload, for the log line that reports it.
fn panic_message(payload: &(dyn std::any::Any + Send)) -> &str {
    payload
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
        .unwrap_or("<non-string panic>")
}

/// Submit `work` to the worker pool behind an outer panic boundary. Every pool
/// submission goes through this; none calls `pool.execute` directly.
///
/// `threadpool` retires any worker that unwinds and spawns a replacement, which
/// gets a fresh `ThreadId`. `DbPool::PerThread` keys its connections on that id
/// and never evicts, so the retired worker's SQLite connection — and up to three
/// file descriptors — is stranded for the life of the mount, while
/// `musefs_pool_workers` keeps reading healthy (#669). Catching here keeps the
/// worker, and therefore its connection, alive.
///
/// This is a backstop, not the reply guarantee. fuser answers a reply dropped
/// unsent with a bare `EIO` and a warning that names only the request id, so a
/// task that panics before replying fails its syscall with the wrong errno and
/// no record of what failed. Reply-bearing tasks guard their synthesis with
/// [`synth_outcome`] and reply *outside* that boundary, which is what gives the
/// caller the real errno and the log its cause (#359, #533).
/// What reaches this boundary is the rest of the task body — the reply call
/// itself, handle bookkeeping — and the poll-refresh tasks, which carry no reply
/// at all. `op` labels the syscall in the log line.
fn execute_guarded(pool: &ThreadPool, op: &'static str, work: impl FnOnce() + Send + 'static) {
    pool.execute(move || run_guarded(op, work));
}

/// Run `work` behind [`execute_guarded`]'s panic boundary on the calling thread,
/// for a job [`Workers::submit`] runs in place because the queue is full.
fn run_guarded(op: &'static str, work: impl FnOnce()) {
    if let Err(payload) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(work)) {
        log::error!(
            "{op} worker task panicked outside the synthesis boundary: {}; worker retained",
            panic_message(&*payload)
        );
    }
}

/// Cap on metadata jobs queued or running on the worker pool at once (#694).
///
/// `ThreadPool`'s queue is unbounded. Reads reserve against their own cap before
/// queueing ([`MAX_INFLIGHT_READS`], #308); every other job used to queue with
/// no bound at all. 4096 is many times what a pool of any sensible size drains
/// between two kernel round trips, so an ordinary workload never meets it; what
/// meets it is a backlog already too deep to be worth growing.
const MAX_QUEUED_JOBS: usize = 4096;

/// The admission cap a mount's pool runs with: [`MAX_QUEUED_JOBS`], unless a
/// test forced another through `FuseConfig::pool_admission_cap`.
#[cfg(feature = "test-support")]
fn admission_cap(config: &FuseConfig) -> usize {
    config.pool_admission_cap.unwrap_or(MAX_QUEUED_JOBS)
}

#[cfg(not(feature = "test-support"))]
fn admission_cap(_config: &FuseConfig) -> usize {
    MAX_QUEUED_JOBS
}

/// Where a job a mount handed its worker pool went (#694), as a test-support
/// [`RouteTrace`] records it.
#[cfg(feature = "test-support")]
#[doc(hidden)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum PoolRoute {
    /// Admitted and queued on the pool.
    Queued,
    /// Over the admission cap, run on the submitting thread.
    InPlace,
    /// Over the admission cap, dropped unrun.
    Dropped,
    /// A read, on its own lane past the metadata gate.
    ReadLane,
}

/// Every job a mount handed its worker pool, as `(op label, route)`, in order.
#[cfg(feature = "test-support")]
#[doc(hidden)]
pub type RouteTrace = Arc<Mutex<Vec<(&'static str, PoolRoute)>>>;

/// The directory-handle cap a mount runs with: [`MAX_DIR_HANDLES`], unless a
/// test forced another through `FuseConfig::dir_handle_cap`.
#[cfg(feature = "test-support")]
fn configured_dir_handle_cap(config: &FuseConfig) -> usize {
    config.dir_handle_cap.unwrap_or(MAX_DIR_HANDLES)
}

#[cfg(not(feature = "test-support"))]
fn configured_dir_handle_cap(_config: &FuseConfig) -> usize {
    MAX_DIR_HANDLES
}

/// How many listings a mount's stateless enumerations keep pinned:
/// [`MAX_STATELESS_LISTINGS`], unless a test forced another through
/// `FuseConfig::stateless_listing_cap`.
#[cfg(feature = "test-support")]
fn configured_stateless_listing_cap(config: &FuseConfig) -> usize {
    config
        .stateless_listing_cap
        .unwrap_or(MAX_STATELESS_LISTINGS)
}

#[cfg(not(feature = "test-support"))]
fn configured_stateless_listing_cap(_config: &FuseConfig) -> usize {
    MAX_STATELESS_LISTINGS
}

/// The worker pool behind one admission gate for everything but reads (#694).
/// Cloning shares the pool, the count and the counter.
#[derive(Clone)]
struct Workers {
    pool: ThreadPool,
    /// Metadata jobs queued or running.
    admitted: Arc<AtomicUsize>,
    /// Jobs that found `admitted` at the cap: run in place, or for a
    /// `readdirplus` entry's attrs, not run at all (`musefs_pool_over_cap_total`).
    over_cap: Arc<AtomicU64>,
    cap: usize,
    /// Test-only: where each job went (#694).
    #[cfg(feature = "test-support")]
    trace: Option<RouteTrace>,
}

/// Gives one [`Workers::admitted`] slot back when dropped: when its job ends,
/// when it panics, or when a dead pool drops it without running it.
struct AdmittedSlot(Arc<AtomicUsize>);

impl Drop for AdmittedSlot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

impl Workers {
    fn new(pool: ThreadPool, cap: usize) -> Workers {
        Workers {
            pool,
            admitted: Arc::new(AtomicUsize::new(0)),
            over_cap: Arc::new(AtomicU64::new(0)),
            cap,
            #[cfg(feature = "test-support")]
            trace: None,
        }
    }

    /// Test-only: record every job's route into `trace` (#694).
    #[cfg(feature = "test-support")]
    fn traced(mut self, trace: Option<RouteTrace>) -> Workers {
        self.trace = trace;
        self
    }

    /// Test-only: record where `op`'s job went, if a test asked for a trace.
    /// Called before the job runs or is dropped, so the entry exists before
    /// any reply the job sends can unblock the test's syscall.
    #[cfg(feature = "test-support")]
    fn record(&self, op: &'static str, route: PoolRoute) {
        if let Some(trace) = &self.trace {
            trace
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push((op, route));
        }
    }

    /// Take a slot if the queue has room, counting the refusal if not.
    fn admit(&self) -> Option<AdmittedSlot> {
        let count = self.admitted.fetch_add(1, Ordering::Relaxed) + 1;
        let slot = AdmittedSlot(Arc::clone(&self.admitted));
        if count > self.cap {
            self.over_cap.fetch_add(1, Ordering::Relaxed);
            None // `slot` drops here, giving the increment back
        } else {
            Some(slot)
        }
    }

    /// Queue a metadata job, or — with the queue at its cap — run it on this
    /// thread instead (#694).
    ///
    /// Running it here is the backpressure. From the dispatch thread it stops
    /// fuser reading the next request until the job is done, so a backlog waits
    /// in the kernel, which bounds it, rather than in an unbounded queue here.
    /// Nothing is refused: a failed `lookup` or `getattr` fails the caller's
    /// syscall outright, and refusing directory work is what #616 walked back.
    fn submit(&self, op: &'static str, work: impl FnOnce() + Send + 'static) {
        if let Some(slot) = self.admit() {
            #[cfg(feature = "test-support")]
            self.record(op, PoolRoute::Queued);
            execute_guarded(&self.pool, op, move || {
                let _slot = slot;
                work();
            });
        } else {
            #[cfg(feature = "test-support")]
            self.record(op, PoolRoute::InPlace);
            run_guarded(op, work);
        }
    }

    /// Queue a job only if the queue has room, dropping it unrun otherwise, and
    /// report which (#694). For work with a cheaper answer than running in place.
    fn try_submit(&self, op: &'static str, work: impl FnOnce() + Send + 'static) -> bool {
        let Some(slot) = self.admit() else {
            #[cfg(feature = "test-support")]
            self.record(op, PoolRoute::Dropped);
            return false;
        };
        #[cfg(feature = "test-support")]
        self.record(op, PoolRoute::Queued);
        execute_guarded(&self.pool, op, move || {
            let _slot = slot;
            work();
        });
        true
    }

    /// Queue a read. Reads are admitted against their own cap before they get
    /// here (#308), so they bypass this gate.
    fn submit_read(&self, work: impl FnOnce() + Send + 'static) {
        #[cfg(feature = "test-support")]
        self.record("read", PoolRoute::ReadLane);
        execute_guarded(&self.pool, "read", work);
    }

    fn max_count(&self) -> usize {
        self.pool.max_count()
    }

    fn active_count(&self) -> usize {
        self.pool.active_count()
    }

    fn queued_count(&self) -> usize {
        self.pool.queued_count()
    }

    #[cfg(test)]
    fn join(&self) {
        self.pool.join();
    }
}

/// Run metadata/handle/read synthesis under a panic boundary so a residual
/// parser panic — one the format-layer alloc guards (`id3v2_alloc_safe` and
/// friends) don't catch — becomes an errno reply instead of unwinding the pool
/// worker. An unwound worker drops its reply unsent, which fuser answers with a
/// bare `EIO` and a warning naming only the request id, so the caller gets no
/// real errno and the log no cause (#359). The same metadata synthesis runs behind `read`,
/// `lookup`, `getattr`, and `open` (all resolve a layout via `cache.resolve`),
/// so every one of them must guard it, not just `read` (#533). The caller makes
/// the reply *outside* this boundary on the returned outcome. A `CoreError` maps
/// to its errno via [`reply_errno`]; a caught panic is logged and mapped to
/// `EIO`. `op` labels the syscall in those messages.
fn synth_outcome<F, T>(op: &str, ino: u64, work: F) -> Result<T, fuser::Errno>
where
    F: FnOnce() -> Result<T, CoreError> + std::panic::UnwindSafe,
{
    match std::panic::catch_unwind(work) {
        Ok(Ok(v)) => Ok(v),
        Ok(Err(e)) => Err(reply_errno(op, ino, &e)),
        Err(payload) => {
            let msg = panic_message(&*payload);
            log::error!("{op}({ino}) worker panicked in synthesis: {msg}; replying EIO");
            Err(fuser::Errno::EIO)
        }
    }
}

/// One directory's readdir snapshot: `(child inode, entry type, name)` rows.
/// Aliased so the handle-map signatures stay readable (and dodge
/// `clippy::type_complexity`).
type DirListing = Vec<(u64, FileType, String)>;

/// What one `DirListing` describes: a directory, as of one virtual-tree
/// generation. `(TreeSnapshot::id(), inode)`.
///
/// Two handles that agree on both are asking for byte-identical listings, which
/// is what lets them share one (#675). The generation half is a heap address and
/// so is unique only among *live* snapshots — see [`DirHandles::shared`] for why
/// that is sound here.
type DirListingKey = (usize, u64);

/// One open directory handle: the listing `readdir` serves, plus which shared
/// entry it is holding.
struct DirHandle {
    /// Identifies `listing` in [`DirHandles::shared`], so `releasedir` can
    /// release its share of the index entry.
    key: DirListingKey,
    listing: Arc<DirListing>,
}

/// One directory's listing as shared by every handle on it.
struct SharedListing {
    /// `Weak`, so the index never keeps a listing alive by itself: a handle
    /// dropping its `Arc` while an in-flight `readdir` still serves a clone
    /// leaves that clone valid and this entry harmless.
    listing: Weak<DirListing>,
    /// The generation this entry's key names, pinned for exactly as long as the
    /// entry lives.
    ///
    /// Load-bearing, not decorative. The key's generation half is a heap
    /// address, unique only among *live* snapshots, so an entry that outlived
    /// its generation could be matched by a later tree allocated at the same
    /// address and serve that new generation a listing of the old one. Pinning
    /// here rather than in [`DirHandle`] is what forecloses it: a handle's
    /// listing can outlive the handle by way of an in-flight `readdir`, but
    /// nothing outlives this entry, because the entry outlives every handle on
    /// it by construction (`handles` below).
    _snapshot: TreeSnapshot,
    /// Open handles on this key. The entry is created with the first and removed
    /// with the last, so `shared` holds an entry exactly while `open` holds a
    /// handle for it — and its `listing` is therefore always upgradeable.
    handles: usize,
}

/// The `opendir` handle table and the index that lets handles share listings.
#[derive(Default)]
struct DirHandles {
    /// Live handles, keyed by the fh handed out by `opendir`.
    open: std::collections::HashMap<u64, DirHandle>,
    /// The listing each open directory currently has, keyed by what it is a
    /// listing of (#675).
    shared: std::collections::HashMap<DirListingKey, SharedListing>,
}

/// Cap on concurrently-open directory handles (#307). Each `opendir` takes a
/// `DirListing`, so an unreleased handle pins memory ~ (entries × name length);
/// the cap bounds the *number* of handles, not the inherent size of one (a
/// single `ls` of the widest directory already allocates it).
///
/// Since #675 the cap no longer bounds a *product*. Handles on one directory at
/// one tree generation share a single listing through [`DirHandles::shared`], so
/// 1,024 opens of a 300,000-entry directory cost one listing and 1,024 refcount
/// bumps rather than 1,024 copies of it — and all but the first skip the tree
/// walk that builds one.
///
/// The cap bounds memory, not correctness: an ordinary parallel walker (`bfs`,
/// the default `find` on some distributions) blows past 1024 concurrent dir
/// handles on a large mount, so over-cap opens must still *work* (#616). They
/// are served statelessly via [`DIR_FH_STATELESS`] and counted, never refused,
/// and a stateless enumeration still pages a single generation's listing
/// through [`StatelessListings`] (#695).
const MAX_DIR_HANDLES: usize = 1024;

/// The `opendir` fh that stores no snapshot: `readdir` pages it through
/// [`StatelessListings`], which pins the listing each enumeration started on
/// (#695), and `releasedir` on it removes nothing. Handed out for the synthetic
/// `.musefs-metrics` directory and for any open over `MAX_DIR_HANDLES` (#616),
/// so a saturated table costs a client at most a rebuild per enumeration rather
/// than the directory itself. `dir_fh` starts at 1, so no real handle can
/// collide with it.
const DIR_FH_STATELESS: u64 = 0;

/// Bits of a directory cookie that hold the index of the next entry (#695). The
/// bits above them hold the generation tag of the listing a stateless
/// enumeration is paging. An admitted handle, whose listing cannot change under
/// it, uses tag 0, so its cookies are exactly the indexes they always were. A
/// cookie is opaque to the kernel and to readers; ext4 hands out 63-bit hash
/// cookies, so a large one is ordinary.
const COOKIE_INDEX_BITS: u32 = 32;

/// The cookie that resumes a listing tagged `tag` at entry `next`.
fn dir_cookie(tag: u32, next: usize) -> u64 {
    let index = u32::try_from(next).expect("a directory listing has fewer than 2^32 entries");
    (u64::from(tag) << COOKIE_INDEX_BITS) | u64::from(index)
}

/// Split a cookie into its generation tag and the index it resumes at.
fn split_dir_cookie(offset: u64) -> (u32, usize) {
    let tag = u32::try_from(offset >> COOKIE_INDEX_BITS).expect("the high half of a u64 fits u32");
    (tag, usize_from(offset & u64::from(u32::MAX)))
}

/// Where a directory page starts: the listing index, and the generation tag its
/// cookies carry (0 for a listing that cannot change under the cursor).
#[derive(Clone, Copy)]
struct PageStart {
    index: usize,
    tag: u32,
}

impl PageStart {
    /// A page of a listing held for the whole enumeration, where the offset the
    /// kernel hands back is the index itself.
    fn untagged(offset: u64) -> PageStart {
        PageStart {
            index: usize_from(offset),
            tag: 0,
        }
    }
}

/// How many listings stateless enumerations keep pinned (#695). Eviction alone
/// costs an enumeration nothing while its generation is still current: the
/// listing is rebuilt identically. Only a listing evicted *and* replaced by a
/// refresh leaves the enumeration unresumable — see [`StatelessPage::Stale`].
const MAX_STATELESS_LISTINGS: usize = 64;

/// Listings pinned for enumerations served without a directory handle (#695).
///
/// A stateless fh cannot tell one enumeration from another, so nothing
/// per-handle can hold the listing it is paging. Rebuilding each page from
/// whatever generation was current let a refresh between two pages shift the
/// entries under an index cookie: one enumeration could return an entry twice,
/// or skip one. Instead the first page of an enumeration tags the current
/// generation and pins its listing here, and every cookie it hands out carries
/// the tag, so each later page reads the same listing.
struct StatelessListings {
    /// The [`TreeSnapshot::generation`] new enumerations are tagged with, and its
    /// tag. It only ever advances: a generation number is never reused, so
    /// nothing needs pinning, and it is ordered, so a worker still holding a
    /// snapshot from before a refresh cannot re-tag the current generation back
    /// to its own.
    current: Option<(u64, u32)>,
    /// The last tag minted. Tags start at 1; 0 means untagged.
    last_tag: u32,
    /// `((directory inode, tag), listing)`, least recently used first.
    pinned: std::collections::VecDeque<((u64, u32), Arc<DirListing>)>,
    /// How many listings `pinned` holds before it evicts:
    /// [`MAX_STATELESS_LISTINGS`] outside a test that forced another.
    cap: usize,
}

impl Default for StatelessListings {
    fn default() -> StatelessListings {
        StatelessListings::with_cap(MAX_STATELESS_LISTINGS)
    }
}

impl StatelessListings {
    /// The tag for `generation`: the current tag if it is the current
    /// generation, a newly minted one if it is newer, and `None` if it is older.
    ///
    /// Older means the caller loaded its snapshot before a refresh that another
    /// enumeration has since tagged. Minting for it would move `current` back,
    /// and the enumeration paging the newer generation would then find its tag
    /// replaced — refused as stale the moment its listing was evicted, though
    /// nothing it was paging had changed.
    fn tag_for(&mut self, generation: u64) -> Option<u32> {
        match self.current {
            Some((held, tag)) if held == generation => Some(tag),
            Some((held, _)) if held > generation => None,
            _ => {
                self.last_tag = self.last_tag.checked_add(1).unwrap_or(1);
                self.current = Some((generation, self.last_tag));
                Some(self.last_tag)
            }
        }
    }

    /// The listing pinned for `(ino, tag)`, marked most recently used.
    fn get(&mut self, ino: u64, tag: u32) -> Option<Arc<DirListing>> {
        let at = self.pinned.iter().position(|(key, _)| *key == (ino, tag))?;
        let entry = self.pinned.remove(at)?;
        let listing = Arc::clone(&entry.1);
        self.pinned.push_back(entry);
        Some(listing)
    }

    /// An empty cache that pins at most `cap` listings.
    fn with_cap(cap: usize) -> StatelessListings {
        StatelessListings {
            current: None,
            last_tag: 0,
            pinned: std::collections::VecDeque::new(),
            cap,
        }
    }

    /// Pin `listing` for `(ino, tag)`, evicting the least recently used past the cap.
    fn insert(&mut self, ino: u64, tag: u32, listing: Arc<DirListing>) {
        self.pinned.retain(|(key, _)| *key != (ino, tag));
        self.pinned.push_back(((ino, tag), listing));
        while self.pinned.len() > self.cap {
            self.pinned.pop_front();
        }
    }
}

/// What a stateless `readdir`/`readdirplus` cookie resolves to (#695).
enum StatelessPage {
    /// The listing to page, and where the page starts.
    Page(Arc<DirListing>, PageStart),
    /// The cookie belongs to an enumeration that can no longer be resumed: its
    /// listing was evicted, and a refresh has since replaced the generation it
    /// was paging. Its index points into a listing that no longer exists, and
    /// applied to the current one it would repeat or skip entries. The reply is
    /// [`stale_enumeration`]'s errno.
    Stale,
}

/// The errno for a [`StatelessPage::Stale`] cookie, logged at the serve-path
/// warn budget.
///
/// `ESTALE` rather than `EIO`: nothing failed to read. The enumeration's view of
/// the directory is gone, which is what "stale file handle" means, and `ls`,
/// `find` and scandir-style walkers report exactly that for the one directory
/// and carry on. An `EIO` reads as a disk fault. Neither is retried by the
/// kernel: the Linux VFS retries `ESTALE` only on path lookups, and `getdents`
/// on an open directory is not one; the macOS and FreeBSD FUSE clients pass a
/// `readdir` errno straight through. A new enumeration of the directory starts
/// on the current generation and succeeds.
fn stale_enumeration(op: &str, ino: u64) -> fuser::Errno {
    serve_warn!(
        "{op}({ino}) rejected: ESTALE — the enumeration's listing was evicted and \
         the tree has changed since, so its cookie cannot be resumed"
    );
    fuser::Errno::ESTALE
}

/// Resolve a stateless `readdir`/`readdirplus` cookie (#695).
///
/// - An untagged offset — the first page of an enumeration — is served from
///   `snapshot`'s generation, which is tagged and pinned so the rest of the
///   enumeration stays on it.
/// - A tagged cookie resumes the listing its tag pinned, whatever has been
///   published since.
/// - A tagged cookie whose listing was evicted, but whose tag is still the
///   current generation's, gets that listing rebuilt and re-pinned: it is the
///   same generation, so the index still means what it meant.
/// - A tagged cookie whose listing was evicted and whose generation has been
///   replaced is [`StatelessPage::Stale`]. Paging the new generation at the old
///   index was exactly the duplicate-or-skip this cache exists to prevent.
///
/// `snapshot` is the one the worker loaded. If a refresh has published a newer
/// generation and another enumeration has already tagged it, `snapshot` is
/// behind the current tag, and `latest` supplies the newer tree instead — which
/// is at least as new, because publication only moves forward.
///
/// `build` supplies a listing from the snapshot the page is on when one is not
/// pinned already, and runs without the cache lock held.
fn stateless_page<E>(
    listings: &Mutex<StatelessListings>,
    ino: u64,
    offset: u64,
    snapshot: &TreeSnapshot,
    latest: impl FnOnce() -> TreeSnapshot,
    build: impl FnOnce(&TreeSnapshot) -> Result<Arc<DirListing>, E>,
) -> Result<StatelessPage, E> {
    let (tag, index) = split_dir_cookie(offset);
    let mut reloaded = None;
    let current = {
        let mut guard = listings
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if tag != 0
            && let Some(listing) = guard.get(ino, tag)
        {
            return Ok(StatelessPage::Page(listing, PageStart { index, tag }));
        }
        let current = if let Some(current) = guard.tag_for(snapshot.generation()) {
            current
        } else {
            let fresh = latest();
            let current = guard
                .tag_for(fresh.generation())
                .expect("a snapshot loaded after the current tag's is at least as new");
            reloaded = Some(fresh);
            current
        };
        if tag != 0 && tag != current {
            return Ok(StatelessPage::Stale);
        }
        if let Some(listing) = guard.get(ino, current) {
            return Ok(StatelessPage::Page(
                listing,
                PageStart {
                    index,
                    tag: current,
                },
            ));
        }
        current
    };
    let listing = build(reloaded.as_ref().unwrap_or(snapshot))?;
    listings
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(ino, current, Arc::clone(&listing));
    Ok(StatelessPage::Page(
        listing,
        PageStart {
            index,
            tag: current,
        },
    ))
}

/// The fh an `opendir` reply carries, given the [`try_admit_dir_handle`]
/// outcome: the admitted snapshot id, or the stateless sentinel when the table
/// was full. Over-cap opens degrade instead of failing (#616) — replying
/// `ENFILE` lost the directory from a parallel walk entirely, and surfaced to
/// the operator as "Too many open files in system", pointing at their kernel
/// rather than at the mount.
fn dir_open_fh(admitted: Option<u64>) -> FileHandle {
    FileHandle(admitted.unwrap_or(DIR_FH_STATELESS))
}

/// Cap on concurrently-open `.musefs-metrics/metrics` handles (#394). Each `open`
/// pins one rendered snapshot until `release`; the cap bounds the map the same way
/// `MAX_DIR_HANDLES` bounds dir snapshots, so a client that opens the file without
/// closing cannot grow it without bound. An over-cap `open` returns `ENFILE`.
const MAX_METRICS_HANDLES: usize = 1024;

/// Build a directory's full readdir listing once, from one pinned tree
/// generation. Shared by `opendir` (held per fh) and the `readdir` fallback for
/// an unknown fh. When `expose_metrics` is on, the synthetic `.musefs-metrics`
/// entry is appended to the root listing (#394), like the Spotlight marker. No
/// dedup is needed: both names are reserved in the virtual-tree namespace
/// (`musefs_core::RESERVED_ROOT_NAMES`), so the tree cannot supply a root entry
/// of the same name for this to duplicate or hide (#681).
///
/// Reading the children and the parent from one [`TreeSnapshot`] rather than
/// two `Musefs` calls is what lets the result be keyed by generation and shared
/// (#675); it also means a refresh landing mid-build can no longer splice a
/// `..` from one tree onto children from another.
fn build_dir_listing(
    tree: &TreeSnapshot,
    ino: u64,
    expose_metrics: bool,
) -> Result<Vec<(u64, FileType, String)>, CoreError> {
    let entries = tree.readdir(ino)?;
    let parent = tree.parent(ino).unwrap_or(ino);
    let marker = platform::spotlight::marker_dir_entry(ino);
    let mut listing = assemble_dir_listing(ino, parent, entries, marker);
    if expose_metrics && let Some(entry) = metrics_dir::root_dir_entry(ino) {
        listing.push(entry);
    }
    Ok(listing)
}

/// Answer a `readdir` with the page of a listing held for the whole enumeration,
/// starting at `offset`.
fn reply_dir_page(reply: ReplyDirectory, listing: &[(u64, FileType, String)], offset: u64) {
    reply_dir_entries(reply, listing, PageStart::untagged(offset));
}

/// Answer a `readdir` with the page of `listing` starting at `page`. Slicing from
/// the start index directly keeps a paginated enumeration O(n) rather than
/// O(n^2): the skipped prefix is never re-walked (#442). The cookie stored with
/// each entry resumes at the *next* entry, under the page's tag.
fn reply_dir_entries(
    mut reply: ReplyDirectory,
    listing: &[(u64, FileType, String)],
    page: PageStart,
) {
    let start = page.index.min(listing.len());
    for (i, (child, kind, name)) in (start..).zip(&listing[start..]) {
        if reply.add(INodeNo(*child), dir_cookie(page.tag, i + 1), *kind, name) {
            break;
        }
    }
    reply.ok();
}

/// The listing already held for `key`, if any handle still holds it (#675). A
/// hit spares the caller the tree walk *and* the allocation; a miss means no
/// open handle describes that directory at that generation.
fn shared_listing(handles: &DirHandles, key: DirListingKey) -> Option<Arc<DirListing>> {
    handles
        .shared
        .get(&key)
        .and_then(|shared| shared.listing.upgrade())
}

/// Entries one `readdirplus` round resolves before it tries to fill the reply
/// (#667).
///
/// fuser does not expose the reply buffer's size, so the handler cannot know
/// where the kernel's page ends until it has filled it: a `DirEntryPlus` is a
/// 152-byte header plus the name, so a 4 KiB buffer holds roughly 25 entries and
/// a 32 KiB one roughly 200. Rounds bound the over-resolution to less than one
/// round per page, and even that is not wasted — a resolved attr lands in the
/// size cache, so the page that does ask for it finds it warm.
const READDIRPLUS_BATCH: usize = 64;

/// A round must be able to fill the smallest reply buffer the kernel will send
/// on its own, or every listing pays extra rounds to reach the end of one page.
const _: () = assert!(READDIRPLUS_BATCH * 160 >= 4096);

/// The mount-wide constants every attr reply is built from.
#[derive(Clone, Copy)]
struct AttrStyle {
    uid: u32,
    gid: u32,
    file_mode: u16,
    dir_mode: u16,
    mount_time: SystemTime,
    ttl: Duration,
}

/// One entry's attrs and how long the kernel may trust them. The TTL is the
/// mount's, except for an entry the kernel is meant to refuse: see
/// [`unlinkable_plus_entry`].
#[derive(Clone, Copy)]
struct PlusEntry {
    attr: FileAttr,
    ttl: Duration,
}

/// What became of one entry's resolution. A slot left unset means it never
/// ran: dropped over the pool's admission cap, or lost with a dropped task.
#[derive(Clone, Copy)]
enum Resolution {
    /// The file's own attrs.
    Resolved(PlusEntry),
    /// The synthesis ran and failed; the error is already logged.
    Failed,
}

/// How one entry of a finished round goes into the reply.
#[derive(Clone, Copy)]
enum PlusEmit {
    /// With the file's own attrs.
    Attrs(PlusEntry),
    /// Listed, with attrs the kernel will refuse to link: see
    /// [`unlinkable_plus_entry`].
    Unlinkable,
}

/// What a finished round sends: `emits`, one per entry from the round's start,
/// and whether the reply page ends after them.
struct RoundPlan {
    emits: Vec<PlusEmit>,
    ends_page: bool,
}

/// Decide what a finished round sends, from its slots in listing order.
/// `opens_page` is whether the round's first entry is the reply page's first.
///
/// Nothing goes out with attrs that are not the file's own. The kernel applies
/// a `readdirplus` entry's attrs to the inode it already holds under that name,
/// whatever the TTL, so a placeholder size truncates the page cache of a file
/// another process has open — and kills one that has it mapped with `SIGBUS`.
/// So the page ends before the first entry without attrs, and the kernel asks
/// again from that entry's cookie: a short page is harmless, where an empty one
/// reads as the end of the directory and an error fails the whole `getdents`.
///
/// The exception is the page's own first entry, which has had its one attempt
/// in place and cannot be deferred again without ending the page empty. If it
/// has no attrs, it is listed [`PlusEmit::Unlinkable`], so the name still
/// appears and the listing still moves on.
fn plan_round(slots: &[Option<Resolution>], opens_page: bool) -> RoundPlan {
    let mut emits = Vec::with_capacity(slots.len());
    for (idx, slot) in slots.iter().enumerate() {
        match slot {
            Some(Resolution::Resolved(entry)) => emits.push(PlusEmit::Attrs(*entry)),
            _ if idx == 0 && opens_page => emits.push(PlusEmit::Unlinkable),
            _ => {
                return RoundPlan {
                    emits,
                    ends_page: true,
                };
            }
        }
    }
    RoundPlan {
        emits,
        ends_page: false,
    }
}

/// A `readdirplus` reply being filled (#667). Rounds run strictly one after
/// another — the round that finishes starts the next — so nothing here is
/// contended except by the resolutions within one round.
struct PlusFill {
    /// The listing being paged, shared with the directory handle (#675).
    listing: Arc<DirListing>,
    /// The reply, until the round that fills or exhausts it sends it. `None`
    /// afterwards, so a stray second finish cannot double-reply.
    reply: Mutex<Option<ReplyDirectoryPlus>>,
    core: Arc<Musefs>,
    pool: Workers,
    style: AttrStyle,
    expose_metrics: bool,
    /// The generation tag every cookie of this fill carries (#695).
    cookie_tag: u32,
    /// Index into `listing` of the reply page's first entry: the one entry
    /// resolved even over the admission cap, so that the page is never empty.
    page_start: usize,
}

/// One round's resolutions: a slice of the listing, a slot per entry, and the
/// countdown that decides who assembles it.
struct PlusRound {
    fill: Arc<PlusFill>,
    /// Index into `fill.listing` of the first entry this round covers; the
    /// round covers `slots.len()` entries from there.
    start: usize,
    /// One slot per entry, each set at most once by the task that owns it, and
    /// left unset if that task never ran.
    slots: Vec<OnceLock<Resolution>>,
    /// Resolutions still to come, plus one held by the dispatcher until every
    /// task is queued.
    outstanding: AtomicUsize,
}

/// Counts one resolution out of its round on every exit path: normal
/// completion, a panic caught by [`execute_guarded`], and a task a dead pool
/// dropped without running (the shape [`PollPendingGuard`] guards against,
/// #369). The last one out assembles the round, so a lost task costs that
/// entry's attrs and never the reply — which, dropped unsent, fuser would
/// answer with a bare `EIO` for the whole listing (#359).
struct PlusSlot(Arc<PlusRound>);

impl Drop for PlusSlot {
    fn drop(&mut self) {
        // AcqRel: the assembling thread must see every slot written by the
        // tasks it is counting out.
        if self.0.outstanding.fetch_sub(1, Ordering::AcqRel) == 1 {
            finish_plus_round(&self.0);
        }
    }
}

/// Attrs for an entry that needs no DB work, or `None` when it has to go to the
/// pool. Directories are free — `Musefs::getattr` returns immediately for one
/// without touching the DB — and the synthetic entries carry static attrs
/// already (#667).
fn inline_plus_entry(
    child: u64,
    kind: FileType,
    expose_metrics: bool,
    style: &AttrStyle,
) -> Option<PlusEntry> {
    let attr = if kind == FileType::Directory {
        // What `getattr` reports for a directory: size 0, and the mount time
        // standing in for its absent mtime.
        make_attr(
            child,
            0,
            (FileType::Directory, style.dir_mode, 2),
            style.uid,
            style.gid,
            style.mount_time,
        )
    } else if platform::spotlight::is_marker(child) {
        platform::spotlight::marker_attr(style.uid, style.gid, style.file_mode, style.mount_time)
    } else if expose_metrics && child == metrics_dir::METRICS_FILE_INO {
        metrics_dir::file_attr(style.uid, style.gid, style.file_mode, style.mount_time)
    } else {
        return None;
    };
    Some(PlusEntry {
        attr,
        ttl: style.ttl,
    })
}

/// A size no file can have: the kernel's `fuse_valid_size` rejects anything
/// above `i64::MAX`.
const UNLINKABLE_SIZE: u64 = u64::MAX;

/// An entry that lists the name and gives the kernel nothing to cache, for a
/// page's first entry whose attrs could not be resolved (see [`plan_round`]).
///
/// The entry has to appear, or the file vanishes from the listing, where plain
/// `readdir` lists it and the client's own `lookup` reports the error. But no
/// attrs may go with it that are not the file's own: the kernel applies them to
/// the inode it already holds for the name, whatever the TTL. The protocol's
/// way of saying "no attrs for this one", a zero `nodeid`, is not reachable
/// through fuser's API, which derives both the nodeid and the dirent's inode
/// from `attr.ino`, and a zero inode makes `readdir` skip the name.
///
/// So the attrs are ones the kernel will not accept. It emits a
/// `readdirplus` dirent before it links the entry, and `fuse_direntplus_link`
/// runs `fuse_invalid_attr` before it touches the dcache or any inode. An
/// out-of-range size fails that check, so the name is listed, nothing is
/// linked, and the kernel sends a `FORGET` for the entry that musefs ignores.
/// The check is in Linux from 5.5. The zero TTL is a second line of defence:
/// were the entry ever linked, the kernel would still revalidate it on the next
/// access.
fn unlinkable_plus_entry(child: u64, kind: FileType, style: &AttrStyle) -> PlusEntry {
    let node = if kind == FileType::Directory {
        (FileType::Directory, style.dir_mode, 2)
    } else {
        (FileType::RegularFile, style.file_mode, 1)
    };
    PlusEntry {
        attr: make_attr(
            child,
            UNLINKABLE_SIZE,
            node,
            style.uid,
            style.gid,
            style.mount_time,
        ),
        ttl: Duration::ZERO,
    }
}

/// Start filling `reply` with `listing` from `page` (#667).
fn start_plus_fill(
    core: &Arc<Musefs>,
    pool: &Workers,
    style: AttrStyle,
    expose_metrics: bool,
    listing: Arc<DirListing>,
    page: PageStart,
    reply: ReplyDirectoryPlus,
) {
    let start = page.index.min(listing.len());
    let fill = Arc::new(PlusFill {
        listing,
        reply: Mutex::new(Some(reply)),
        core: Arc::clone(core),
        pool: pool.clone(),
        style,
        expose_metrics,
        cookie_tag: page.tag,
        page_start: start,
    });
    spawn_plus_round(&fill, start);
}

/// Resolve the round starting at `start`: fill what needs no DB work inline,
/// fan the rest across the pool, and let the last one out assemble the reply.
///
/// Nothing waits here. A worker that blocked on tasks it queued to its own
/// bounded pool could deadlock behind them, so the round is a countdown rather
/// than a join — the reason this op is shaped unlike every other one in this
/// file. Fanning out is the point: concurrent `lookup`s already spread across
/// the pool, so a handler that resolved a page serially would be slower than the
/// round trips it removes for a threaded scanner (#667).
fn spawn_plus_round(fill: &Arc<PlusFill>, start: usize) {
    let end = start
        .saturating_add(READDIRPLUS_BATCH)
        .min(fill.listing.len());
    let round = Arc::new(PlusRound {
        fill: Arc::clone(fill),
        start,
        slots: (start..end).map(|_| OnceLock::new()).collect(),
        // The dispatcher's own count, released below: without it a task that
        // finishes while later ones are still being queued would assemble a
        // half-resolved round.
        outstanding: AtomicUsize::new(1),
    });
    let dispatching = PlusSlot(Arc::clone(&round));
    for (idx, (child, kind, _)) in fill.listing[start..end].iter().enumerate() {
        let (child, kind) = (*child, *kind);
        if let Some(entry) = inline_plus_entry(child, kind, fill.expose_metrics, &fill.style) {
            let _ = round.slots[idx].set(Resolution::Resolved(entry));
            continue;
        }
        round.outstanding.fetch_add(1, Ordering::Relaxed);
        let slot = PlusSlot(Arc::clone(&round));
        let style = fill.style;
        let resolve = move || {
            let round = &slot.0;
            let resolution = match synth_outcome(
                "readdirplus",
                child,
                std::panic::AssertUnwindSafe(|| round.fill.core.getattr(child)),
            ) {
                Ok(attr) => Resolution::Resolved(PlusEntry {
                    attr: to_file_attr(
                        &attr,
                        style.uid,
                        style.gid,
                        style.file_mode,
                        style.dir_mode,
                        style.mount_time,
                    ),
                    ttl: style.ttl,
                }),
                Err(_) => Resolution::Failed,
            };
            let _ = round.slots[idx].set(resolution);
            // `slot` drops here, counting this resolution out and, if it is the
            // last, assembling the round.
        };
        if start + idx == fill.page_start {
            // The page's first entry runs here if the pool is over its
            // admission cap (#694), like any other metadata job: a page has to
            // carry at least one entry, since an empty reply reads as the end of
            // the directory. Only this one, so a very wide directory still
            // cannot chain round after round on one thread.
            fill.pool.submit("readdirplus_attr", resolve);
        } else {
            // Over the cap every other entry is left unrun, and the page ends
            // before it (see `plan_round`); the kernel asks again from there.
            fill.pool.try_submit("readdirplus_attr", resolve);
        }
    }
    drop(dispatching);
}

/// Emit a finished round into the reply, then send it or start the next one.
///
/// Runs exactly once per round, on whichever thread counted the last resolution
/// out, so it needs no lock of its own beyond taking the reply.
fn finish_plus_round(round: &PlusRound) {
    let fill = &round.fill;
    let Some(mut reply) = fill
        .reply
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take()
    else {
        return;
    };
    let next = round.start + round.slots.len();
    let slots: Vec<Option<Resolution>> = round.slots.iter().map(|s| s.get().copied()).collect();
    let plan = plan_round(&slots, round.start == fill.page_start);
    for (i, emit) in (round.start..).zip(&plan.emits) {
        let (child, kind, name) = &fill.listing[i];
        let entry = match emit {
            PlusEmit::Attrs(entry) => *entry,
            PlusEmit::Unlinkable => unlinkable_plus_entry(*child, *kind, &fill.style),
        };
        // The stored cookie resumes at the *next* entry, as in
        // `reply_dir_entries`: the kernel hands it back to resume from here.
        if reply.add(
            INodeNo(*child),
            dir_cookie(fill.cookie_tag, i + 1),
            name,
            &entry.ttl,
            &entry.attr,
            Generation(0),
        ) {
            // Buffer full: what fits is the page, and the kernel asks again
            // from the last accepted offset.
            return reply.ok();
        }
    }
    if plan.ends_page || next >= fill.listing.len() {
        // Ended early, the page stops at the last entry sent, and the kernel's
        // next request resumes at the first one held back.
        return reply.ok();
    }
    *fill
        .reply
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(reply);
    spawn_plus_round(fill, next);
}

/// Admit a directory handle under the caller's `dir_handles` lock, enforcing
/// `MAX_DIR_HANDLES` (#307). Returns the freshly allocated handle id on admit, or
/// `None` when the table is at `cap` and the caller falls back to the stateless
/// fh (see [`dir_open_fh`]). The id is drawn from `counter` only on the admit
/// path, and the whole check-then-insert runs under the single lock the caller
/// holds, so concurrent `opendir` closures cannot race the count past the cap
/// and a rejected open burns no id.
///
/// `listing` is what the caller built or found; the admitted handle may end up
/// holding a *different* `Arc` with the same contents, because a live listing
/// already indexed under `key` wins over the caller's. Two `opendir` workers
/// that both missed [`shared_listing`] before building would otherwise store one
/// copy each, which is the amplification this exists to remove (#675).
///
/// The reject path bumps `rejections`, surfaced as
/// `musefs_dir_handle_rejections_total`. Saturation is bursty — a parallel walk
/// can rack up thousands of rejections between two samples of the
/// `musefs_dir_handles` gauge, which reads healthy the whole time — so the
/// monotonic counter is the only after-the-fact signal that the cap was hit
/// (#626).
fn try_admit_dir_handle(
    handles: &mut DirHandles,
    counter: &AtomicU64,
    rejections: &AtomicU64,
    cap: usize,
    key: DirListingKey,
    snapshot: TreeSnapshot,
    listing: Arc<DirListing>,
) -> Option<u64> {
    if handles.open.len() >= cap {
        rejections.fetch_add(1, Ordering::Relaxed);
        return None;
    }
    let fh = counter.fetch_add(1, Ordering::Relaxed);
    let shared = handles.shared.entry(key).or_insert(SharedListing {
        listing: Arc::downgrade(&listing),
        _snapshot: snapshot,
        handles: 0,
    });
    // An entry that was already there carries the listing every other handle on
    // this key is serving; take that one and drop the caller's.
    let listing = shared.listing.upgrade().unwrap_or(listing);
    shared.handles += 1;
    handles.open.insert(fh, DirHandle { key, listing });
    Some(fh)
}

/// Release one directory handle and, with the last one on its key, the index
/// entry they shared (#675).
///
/// Counting handles rather than testing the `Weak` is what keeps that entry's
/// lifetime exact. An in-flight `readdir` that cloned the `Arc` under the lock
/// and is serving it outside the lock would otherwise keep a released key's
/// entry alive for as long as it ran, and with it the tree generation the entry
/// pins. The clone it is serving stays valid either way: the index hands out
/// listings, it never gates access to one.
///
/// An unknown fh — the stateless sentinel, a duplicate `releasedir` — releases
/// nothing.
fn release_dir_handle(handles: &mut DirHandles, fh: u64) {
    let Some(handle) = handles.open.remove(&fh) else {
        return;
    };
    let key = handle.key;
    drop(handle);
    if let Some(shared) = handles.shared.get_mut(&key) {
        shared.handles -= 1;
        if shared.handles == 0 {
            handles.shared.remove(&key);
        }
    }
}

/// Clears the `fire_poll_refresh` single-flight gate when the poll task ends,
/// on every exit path including a panic in `poll_refresh_notify` (#89). Owns an
/// `Arc<AtomicBool>` so the guard is built before `ThreadPool::execute` and moved
/// into the worker closure: if a dead pool drops the job without running it,
/// dropping the closure still drops the guard and clears the gate (#369).
struct PollPendingGuard(Arc<AtomicBool>);

impl Drop for PollPendingGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

/// Cap on concurrently outstanding foreground reads (#308). Every FUSE `read`
/// reserves a slot on the dispatch thread *before* enqueuing onto the unbounded
/// pool queue; over the cap the read is rejected with `EAGAIN` rather than
/// queued, so the queue cannot grow past the cap. 1024 is far above any
/// legitimate read fan-in (a player reads sequentially; readahead is bounded by
/// `max_background`), so it is an attack-only response, and queued job state is
/// small, keeping the bound cheap.
const MAX_INFLIGHT_READS: usize = 1024;

/// Releases one `inflight_reads` slot when dropped — on worker completion, on the
/// over-cap reject path, and on panic. Owns an `Arc<AtomicUsize>` (like
/// `PollPendingGuard`) so it can move into the `'static` worker closure.
struct ReadSlotGuard(Arc<AtomicUsize>);

impl Drop for ReadSlotGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Reserve one in-flight-read slot (#308). Increments `inflight` and returns a
/// guard if the post-increment count is within `cap`; otherwise the guard drops
/// immediately (undoing the increment) and `None` is returned, so the caller
/// replies `EAGAIN` without enqueuing. The counter is a pure count with no
/// happens-before tie to other data, so `Relaxed` ordering suffices.
fn reserve_read_slot(inflight: &Arc<AtomicUsize>, cap: usize) -> Option<ReadSlotGuard> {
    let count = inflight.fetch_add(1, Ordering::Relaxed) + 1;
    let guard = ReadSlotGuard(Arc::clone(inflight));
    if count > cap {
        None // guard drops here, undoing the increment
    } else {
        Some(guard)
    }
}

/// A `fuser::Filesystem` that serves a `musefs_core::Musefs`. fuser dispatches
/// on one thread; blocking ops (read/getattr/lookup-attr) are offloaded to a
/// bounded worker pool and answered via the `Send` reply objects, so a slow
/// backing read never stalls the dispatch thread or unrelated metadata ops.
pub struct MusefsFs {
    core: Arc<Musefs>,
    /// Offloaded ops, behind the metadata admission gate (#694).
    pool: Workers,
    /// Store-refresh tasks, on a lane of their own (#694). A metadata backlog on
    /// `pool` cannot delay them, and they never run in place on the dispatch
    /// thread, where `poll_refresh_notify`'s `inval_inode` notifications would be
    /// written to the very channel fuser is reading.
    refresh: ThreadPool,
    uid: u32,
    gid: u32,
    mount_time: SystemTime,
    config: FuseConfig,
    // Set once, right after the session is created (the fs is moved into the
    // session, so the notifier can only be obtained afterward via this shared cell).
    notifier: Arc<OnceLock<Notifier>>,
    /// Single-flight gate for `fire_poll_refresh`: at most one poll task is
    /// queued/running at a time, so a metadata-op storm can't flood the pool (#89).
    poll_pending: Arc<AtomicBool>,
    /// Per-OS kernel-passthrough state (live backing registrations + sticky
    /// disable on Linux; a no-op marker elsewhere).
    passthrough: platform::passthrough::PassthroughState,
    /// Per-open directory listings, keyed by the fh handed out by `opendir`,
    /// plus the index that lets handles on the same directory and tree
    /// generation share one listing instead of copying it each (#675). A
    /// paginated `readdir` clones the `Arc` under the lock and serves it
    /// lock-free, so building the listing is O(N) per `ls`, not per `readdir`
    /// call (#176).
    ///
    /// All three lock sites recover a poisoned mutex via `into_inner` rather than
    /// propagating (#194). Every op under the lock is a `HashMap` insert/remove
    /// (plus `Arc::new`, whose only failure mode — allocation — aborts the process
    /// rather than unwinding, so it can't poison), and a `HashMap` mutation cannot
    /// leave a partially-observable map across a single lock acquisition. So even a
    /// poisoning panic can't tear a later `readdir`'s view; recovery is deliberate.
    dir_handles: Arc<Mutex<DirHandles>>,
    /// Listings pinned for enumerations served on the stateless fh (#695).
    stateless_listings: Arc<Mutex<StatelessListings>>,
    /// Monotonic dir-handle id (starts at 1; 0 stays [`DIR_FH_STATELESS`]).
    ///
    /// Unlike the file slab's generation-encoded keys (`facade.rs`, ABA-safe by
    /// construction), this is a bare never-recycled counter — sufficient precisely
    /// *because* it never recycles: an id is handed out once and never reused, so a
    /// stale or duplicate `releasedir` can only `remove` an id that names no live
    /// handle, never evict a different open dir (#192). A 64-bit monotonic counter
    /// cannot wrap within any real process lifetime.
    dir_fh: Arc<AtomicU64>,
    /// `opendir` calls that found `dir_handles` at `MAX_DIR_HANDLES` and fell back
    /// to the stateless listing path. Surfaced as
    /// `musefs_dir_handle_rejections_total`: the `dir_handles` gauge alone cannot
    /// show saturation, because it is bursty enough to sit at 0 in every sample
    /// while thousands of opens are turned away between them (#626). Since #616
    /// this is also the only signal that directories are being re-listed on every
    /// `readdir` — worth knowing before it shows up as CPU.
    dir_handle_rejections: Arc<AtomicU64>,
    /// The directory-handle cap: `MAX_DIR_HANDLES` outside a test that forced
    /// another (#616).
    dir_handle_cap: usize,
    /// `readdirplus` calls served, surfaced as `musefs_readdirplus_total`. The
    /// op is negotiated at mount and `FUSE_READDIRPLUS_AUTO` lets the kernel
    /// choose per listing, so whether a mount is getting the folded-in lookups
    /// at all is otherwise unobservable from the daemon (#667).
    readdirplus_calls: Arc<AtomicU64>,
    /// In-flight foreground-read counter. `read` reserves a slot before enqueuing;
    /// over `MAX_INFLIGHT_READS` the read is rejected with `EAGAIN`, capping the
    /// otherwise-unbounded pool queue (#308).
    inflight_reads: Arc<AtomicUsize>,
    /// Reads that failed rather than served bytes: an `EAGAIN` load-shed under
    /// read-slot saturation, or any error reply from the read worker (EIO and
    /// friends). Surfaced as `musefs_read_errors_total` — read-error rate is one
    /// of the two most-wanted production metrics for a passthrough FS (#523).
    read_errors: Arc<AtomicU64>,
    /// Per-open rendered `.musefs-metrics/metrics` buffers, keyed by the fh handed
    /// out at `open` (#394). Each open snapshots once; reads slice it by absolute
    /// offset; `release` drops it. Empty/untouched unless `expose_metrics` is on.
    metrics_handles: Arc<Mutex<std::collections::HashMap<u64, Arc<Vec<u8>>>>>,
    /// Monotonic fh source for `metrics_handles` (starts at 1; never 0).
    metrics_fh: Arc<AtomicU64>,
}

impl MusefsFs {
    pub fn new(core: Musefs, config: FuseConfig) -> MusefsFs {
        // Work is I/O-bound (especially on NFS), so the auto default oversizes
        // the pool vs CPUs. An explicit `--workers` wins: besides concurrency,
        // the pool size bounds the per-worker DB connections and their memory
        // (#631), so operators may deliberately size it down.
        let workers = match config.workers {
            0 => std::thread::available_parallelism().map_or(4, std::num::NonZero::get) * 2,
            n => n,
        };
        let structure_only = core.mode() == musefs_core::Mode::StructureOnly;
        // Built before the struct literal, which moves `config`.
        let pool = Workers::new(ThreadPool::new(workers), admission_cap(&config));
        #[cfg(feature = "test-support")]
        let pool = pool.traced(config.route_trace.clone());
        let dir_handle_cap = configured_dir_handle_cap(&config);
        let stateless_listings =
            StatelessListings::with_cap(configured_stateless_listing_cap(&config));
        MusefsFs {
            core: Arc::new(core),
            // `ThreadPool`'s queue is unbounded, so nothing reaches it ungated:
            // reads reserve against `MAX_INFLIGHT_READS` and get EAGAIN over it
            // (#308); every other job goes through `Workers`' admission gate and
            // runs in place over it (#694); directory handles are capped at
            // `MAX_DIR_HANDLES` and degrade to the stateless fh over it (#307,
            // #616). `max_background` (set in `init`) separately caps the
            // kernel's background/readahead requests.
            pool,
            refresh: threadpool::Builder::new()
                .num_threads(1)
                .thread_name("musefs-refresh".to_string())
                .build(),
            uid: config.uid,
            gid: config.gid,
            mount_time: SystemTime::now(),
            config,
            notifier: Arc::new(OnceLock::new()),
            poll_pending: Arc::new(AtomicBool::new(false)),
            passthrough: platform::passthrough::PassthroughState::new(structure_only),
            dir_handles: Arc::new(Mutex::new(DirHandles::default())),
            stateless_listings: Arc::new(Mutex::new(stateless_listings)),
            dir_fh: Arc::new(AtomicU64::new(1)),
            dir_handle_rejections: Arc::new(AtomicU64::new(0)),
            dir_handle_cap,
            readdirplus_calls: Arc::new(AtomicU64::new(0)),
            inflight_reads: Arc::new(AtomicUsize::new(0)),
            read_errors: Arc::new(AtomicU64::new(0)),
            metrics_handles: Arc::new(Mutex::new(std::collections::HashMap::new())),
            metrics_fh: Arc::new(AtomicU64::new(1)),
        }
    }

    fn notifier_cell(&self) -> Arc<OnceLock<Notifier>> {
        Arc::clone(&self.notifier)
    }

    /// Fire `poll_refresh` on the refresh lane (off the dispatch thread), but only
    /// when due: a cheap synchronous `poll_due()` check gates submission so a
    /// metadata-op storm doesn't flood the pool, and a `poll_pending` single-flight
    /// gate bounds in-flight poll tasks to one (#89). When keep-cache is enabled,
    /// also drop the kernel page cache for every inode whose content changed.
    fn fire_poll_refresh(&self) {
        if !self.core.poll_due() {
            return;
        }
        if self
            .poll_pending
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return; // a poll task is already queued/running
        }
        // Build the gate guard before enqueuing and move it into the closure: if
        // `execute` ever fails to run the job (a dead worker pool), dropping the
        // un-run closure drops the guard and clears `poll_pending`, so the gate
        // can't stick `true` forever and silently disable refresh (#369).
        let guard = PollPendingGuard(Arc::clone(&self.poll_pending));
        let core = Arc::clone(&self.core);
        if self.config.keep_cache {
            let notifier = Arc::clone(&self.notifier);
            execute_guarded(&self.refresh, "poll_refresh_notify", move || {
                let _guard = guard;
                if let Err(e) = core.poll_refresh_notify(|ino| {
                    if let Some(n) = notifier.get()
                        && let Err(inval_err) = n.inval_inode(INodeNo(ino), 0, 0)
                    {
                        log::warn!("inval_inode({ino}) failed: {inval_err}");
                    }
                }) {
                    log::warn!("poll_refresh_notify failed: {e}");
                }
            });
        } else {
            execute_guarded(&self.refresh, "poll_refresh", move || {
                let _guard = guard;
                if let Err(e) = core.poll_refresh() {
                    log::warn!("poll_refresh failed: {e}");
                }
            });
        }
    }

    /// Assemble and render the `.musefs-metrics/metrics` body (#394). Best-effort:
    /// every source is an atomic load, a brief lock, or a fallible probe mapped to
    /// `None`/0; nothing here can panic the daemon or perturb a read.
    /// The mount-wide constants every attr reply is built from.
    fn attr_style(&self) -> AttrStyle {
        AttrStyle {
            uid: self.uid,
            gid: self.gid,
            file_mode: self.config.file_mode,
            dir_mode: self.config.dir_mode,
            mount_time: self.mount_time,
            ttl: self.config.ttl,
        }
    }

    fn render_metrics(&self) -> Vec<u8> {
        let core = self.core.telemetry();
        let (dir_handles, dir_listings) = {
            let handles = self
                .dir_handles
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            (handles.open.len() as u64, handles.shared.len() as u64)
        };
        let fuse = musefs_core::FuseTelemetry {
            uptime_seconds: self.mount_time.elapsed().map_or(0, |d| d.as_secs()),
            reads_inflight: self.inflight_reads.load(Ordering::Relaxed) as u64,
            reads_inflight_max: MAX_INFLIGHT_READS as u64,
            read_errors: self.read_errors.load(Ordering::Relaxed),
            dir_handles,
            dir_listings,
            dir_handles_max: self.dir_handle_cap as u64,
            dir_handle_rejections: self.dir_handle_rejections.load(Ordering::Relaxed),
            readdirplus_calls: self.readdirplus_calls.load(Ordering::Relaxed),
            pool_workers: self.pool.max_count() as u64,
            pool_active: self.pool.active_count() as u64,
            pool_queued: self.pool.queued_count() as u64,
            pool_over_cap: self.pool.over_cap.load(Ordering::Relaxed),
            passthrough: self
                .passthrough
                .telemetry()
                .map(|(disabled, active)| musefs_core::PassthroughTelemetry { disabled, active }),
        };
        let process = musefs_core::process_stats();
        let alloc = allocator_stats();
        let syscalls = syscall_snapshot();
        musefs_core::render_prometheus(&core, &fuse, &process, alloc.as_ref(), syscalls.as_ref())
            .into_bytes()
    }
}

impl Filesystem for MusefsFs {
    fn init(&mut self, _req: &Request, config: &mut KernelConfig) -> std::io::Result<()> {
        // All tuning is best-effort and must never abort the mount. On Err these
        // setters leave the config unchanged (the nearest legal value comes back
        // as the Err payload, not written) — and for max_readahead the unchanged
        // value is the kernel's advertised max, so an over-large request still
        // yields that max. We discard the results regardless.
        let _ = config.set_max_readahead(self.config.max_readahead);
        let _ = config.set_max_background(self.config.max_background);
        // `add_capabilities` is all-or-nothing — a single unsupported bit drops
        // the rest — so request them individually. ASYNC_READ is already on by
        // default; PARALLEL_DIROPS may be unsupported on older kernels (ignored).
        let _ = config.add_capabilities(InitFlags::FUSE_ASYNC_READ);
        let _ = config.add_capabilities(InitFlags::FUSE_PARALLEL_DIROPS);
        // READDIRPLUS folds the per-entry `lookup` into the directory read, and
        // AUTO lets the kernel drop back to plain `readdir` when the caller is
        // not stat-ing what it lists — an attr-laden reply is a pessimization
        // for a bare `ls`, since each entry carries ~128 bytes of attrs and so
        // fewer of them fit in a page (#667). Requested separately: without the
        // handler below the kernel would never send the op anyway, and without
        // AUTO it would send it for every listing.
        let _ = config.add_capabilities(InitFlags::FUSE_DO_READDIRPLUS);
        let _ = config.add_capabilities(InitFlags::FUSE_READDIRPLUS_AUTO);
        // Kernel passthrough (Linux-only) is requested by the platform module;
        // off Linux this is a no-op and reads are served through the daemon.
        platform::passthrough::request_capabilities(config);
        Ok(())
    }

    fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        self.fire_poll_refresh();
        let Some(name) = name.to_str() else {
            return reply.error(fuser::Errno::ENOENT);
        };
        if platform::spotlight::marker_lookup(parent.0, name).is_some() {
            let attr = platform::spotlight::marker_attr(
                self.uid,
                self.gid,
                self.config.file_mode,
                self.mount_time,
            );
            return reply.entry(&self.config.ttl, &attr, Generation(0));
        }
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
        // Inode resolution is an in-memory tree read; the attr (which may touch
        // the DB/disk) is computed on the worker pool.
        let Some(child) = self.core.lookup(parent.0, name) else {
            return reply.error(fuser::Errno::ENOENT);
        };
        let core = Arc::clone(&self.core);
        let (uid, gid, fm, dm, mt, ttl) = (
            self.uid,
            self.gid,
            self.config.file_mode,
            self.config.dir_mode,
            self.mount_time,
            self.config.ttl,
        );
        // `reply` stays outside the panic boundary so a residual synthesis panic
        // is answered (EIO) instead of unwinding the worker and hanging the
        // syscall (#359, #533).
        self.pool.submit("lookup", move || {
            match synth_outcome(
                "lookup",
                child,
                std::panic::AssertUnwindSafe(|| core.getattr(child)),
            ) {
                Ok(attr) => reply.entry(
                    &ttl,
                    &to_file_attr(&attr, uid, gid, fm, dm, mt),
                    Generation(0),
                ),
                Err(e) => reply.error(e),
            }
        });
    }

    fn getattr(&self, _req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
        self.fire_poll_refresh();
        if platform::spotlight::is_marker(ino.0) {
            let attr = platform::spotlight::marker_attr(
                self.uid,
                self.gid,
                self.config.file_mode,
                self.mount_time,
            );
            return reply.attr(&self.config.ttl, &attr);
        }
        if self.config.expose_metrics && metrics_dir::is_metrics_ino(ino.0) {
            let attr = if ino.0 == metrics_dir::METRICS_DIR_INO {
                metrics_dir::dir_attr(self.uid, self.gid, self.config.dir_mode, self.mount_time)
            } else {
                metrics_dir::file_attr(self.uid, self.gid, self.config.file_mode, self.mount_time)
            };
            return reply.attr(&self.config.ttl, &attr);
        }
        let core = Arc::clone(&self.core);
        let (uid, gid, fm, dm, mt, ttl) = (
            self.uid,
            self.gid,
            self.config.file_mode,
            self.config.dir_mode,
            self.mount_time,
            self.config.ttl,
        );
        // `reply` stays outside the panic boundary so a residual synthesis panic
        // is answered (EIO) instead of unwinding the worker and hanging the
        // syscall (#359, #533).
        self.pool.submit("getattr", move || {
            match synth_outcome(
                "getattr",
                ino.0,
                std::panic::AssertUnwindSafe(|| core.getattr(ino.0)),
            ) {
                Ok(attr) => reply.attr(&ttl, &to_file_attr(&attr, uid, gid, fm, dm, mt)),
                Err(e) => reply.error(e),
            }
        });
    }

    fn open(&self, _req: &Request, ino: INodeNo, flags: OpenFlags, reply: ReplyOpen) {
        // Read-only filesystem: refuse write-intent opens at the daemon. The mount
        // also carries MountOption::RO so the kernel VFS blocks writes, but the
        // daemon should not itself hand back a handle for O_WRONLY/O_RDWR (#527).
        if flags.acc_mode() != OpenAccMode::O_RDONLY {
            return reply.error(fuser::Errno::EROFS);
        }
        if platform::spotlight::is_marker(ino.0) {
            // Stateless empty file: fh 0 means `release` skips it (its
            // NonZeroU64 guard) and `read` short-circuits on `is_marker`.
            return reply.opened(FileHandle(0), open_flags(false));
        }
        if self.config.expose_metrics && ino.0 == metrics_dir::METRICS_FILE_INO {
            let body = Arc::new(self.render_metrics());
            let mut handles = self
                .metrics_handles
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            // Check + id + insert under one lock hold: concurrent opens can't race
            // the count past the cap, and a rejected open burns no id (#394).
            if handles.len() >= MAX_METRICS_HANDLES {
                log::warn!(
                    "open({}) rejected: ENFILE — metrics-handle table full at {MAX_METRICS_HANDLES}",
                    ino.0
                );
                return reply.error(fuser::Errno::ENFILE);
            }
            let fh = self.metrics_fh.fetch_add(1, Ordering::Relaxed);
            handles.insert(fh, body);
            drop(handles);
            // DIRECT_IO (no NONSEEKABLE): size-0 stat means the kernel reads to
            // EOF, and absolute-offset slicing in `read` supports pread/re-reads.
            return reply.opened(FileHandle(fh), FopenFlags::FOPEN_DIRECT_IO);
        }
        let core = Arc::clone(&self.core);
        let flags = open_flags(self.config.keep_cache);
        let passthrough = self.passthrough.clone();
        self.pool.submit("open", move || {
            // `open_handle` runs the same layout synthesis as `read`; guard it so a
            // residual panic replies EIO instead of unwinding the worker and hanging
            // `open` (#359, #533). `reply_open` below stays outside the boundary.
            let fh = match synth_outcome(
                "open",
                ino.0,
                std::panic::AssertUnwindSafe(|| core.open_handle(ino.0)),
            ) {
                Ok(fh) => fh,
                Err(e) => return reply.error(e),
            };
            // Ordering invariant (#193): `open_handle` inserts the handle into the
            // slab and returns `fh` *before* we reply here, and the kernel won't
            // issue `release` for an fh until it has received this open reply — so
            // the handle is always registered before any `release` can find it.
            // Keep the slab insert ahead of this reply: replying first to shave open
            // latency would let a `release` race a not-yet-registered handle.
            platform::passthrough::reply_open(&passthrough, &core, fh, reply, flags);
        });
    }

    fn opendir(&self, _req: &Request, ino: INodeNo, _flags: OpenFlags, reply: ReplyOpen) {
        self.fire_poll_refresh();
        if self.config.expose_metrics && ino.0 == metrics_dir::METRICS_DIR_INO {
            // Stateless: readdir(METRICS_DIR_INO) serves an inline listing and
            // never consults dir_handles, so this fh burns no MAX_DIR_HANDLES slot.
            return reply.opened(FileHandle(DIR_FH_STATELESS), FopenFlags::empty());
        }
        let core = Arc::clone(&self.core);
        let handles = Arc::clone(&self.dir_handles);
        let counter = Arc::clone(&self.dir_fh);
        let rejections = Arc::clone(&self.dir_handle_rejections);
        let dir_handle_cap = self.dir_handle_cap;
        let expose_metrics = self.config.expose_metrics;
        self.pool.submit("opendir", move || {
            // Pin the tree generation first: it names what a listing of this
            // directory would contain, so it is both what the build reads and
            // what the result is keyed by (#675).
            let snapshot = core.tree_snapshot();
            let key = (snapshot.id(), ino.0);
            let cached = shared_listing(
                &handles
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner),
                key,
            );
            // `build_dir_listing` walks the virtual tree and resolves the parent,
            // so guard it like every other synthesis: a residual panic replies EIO
            // instead of unwinding the worker (#359, #533, #669). `reply` stays
            // outside the boundary so the answer is always sent.
            let listing = match cached {
                // Another handle already has this exact listing: no walk, no
                // second copy, just a refcount bump.
                Some(listing) => listing,
                None => match synth_outcome(
                    "opendir",
                    ino.0,
                    std::panic::AssertUnwindSafe(|| {
                        build_dir_listing(&snapshot, ino.0, expose_metrics)
                    }),
                ) {
                    Ok(l) => Arc::new(l),
                    Err(e) => return reply.error(e),
                },
            };
            let admitted = {
                let mut guard = handles
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                try_admit_dir_handle(
                    &mut guard,
                    &counter,
                    &rejections,
                    dir_handle_cap,
                    key,
                    snapshot,
                    listing,
                )
            };
            if admitted.is_none() {
                // Debug, not warn: a parallel walk over a large mount produces
                // thousands of these in seconds, the walk still succeeds, and
                // the listing is simply rebuilt per `readdir` from here on.
                // `musefs_dir_handle_rejections_total` is the operator-facing
                // signal that the degraded path is in use (#626).
                log::debug!(
                    "opendir({ino}) over the {dir_handle_cap}-handle cap: serving it statelessly",
                    ino = ino.0
                );
            }
            reply.opened(dir_open_fh(admitted), FopenFlags::empty());
        });
    }

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

    fn releasedir(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        _flags: OpenFlags,
        reply: ReplyEmpty,
    ) {
        release_dir_handle(
            &mut self
                .dir_handles
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            fh.0,
        );
        reply.ok();
    }

    fn flush(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _fh: FileHandle,
        _lock_owner: LockOwner,
        reply: ReplyEmpty,
    ) {
        // Read-only filesystem: nothing to flush. fuser's default replies
        // ENOSYS and logs a warn on every close(), which would drown the
        // serve-failure log lines.
        reply.ok();
    }

    fn statfs(&self, _req: &Request, _ino: INodeNo, reply: ReplyStatfs) {
        let (blocks, bfree, bavail, files, ffree, bsize, namelen, frsize) = statfs_params();
        reply.statfs(blocks, bfree, bavail, files, ffree, bsize, namelen, frsize);
    }

    fn getxattr(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _name: &OsStr,
        _size: u32,
        reply: ReplyXattr,
    ) {
        // Read-only filesystem with no extended attributes. Reply ENOTSUP
        // explicitly so fuser's default doesn't log a `[Not Implemented]` warn on
        // every probe (#364); callers see the same "Operation not supported"
        // result the default's ENOSYS already collapses to.
        reply.error(fuser::Errno::ENOTSUP);
    }

    fn listxattr(&self, _req: &Request, _ino: INodeNo, _size: u32, reply: ReplyXattr) {
        // See `getxattr`: no xattrs, reply ENOTSUP quietly to suppress the warn.
        reply.error(fuser::Errno::ENOTSUP);
    }

    fn setxattr(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _name: &OsStr,
        _value: &[u8],
        _flags: i32,
        _position: u32,
        reply: ReplyEmpty,
    ) {
        // Read-only: setting an xattr is unsupported. Replied explicitly for
        // symmetry with get/listxattr so no `[Not Implemented]` warn is logged.
        reply.error(fuser::Errno::ENOTSUP);
    }

    fn removexattr(&self, _req: &Request, _ino: INodeNo, _name: &OsStr, reply: ReplyEmpty) {
        // Read-only: removing an xattr is unsupported. See `setxattr`.
        reply.error(fuser::Errno::ENOTSUP);
    }

    fn access(&self, _req: &Request, _ino: INodeNo, _mask: AccessFlags, reply: ReplyEmpty) {
        // Every entry the daemon presents is accessible to whoever can reach the
        // mount, so there is no per-inode check to make here. The mount carries
        // `MountOption::RO`, so the kernel refuses write-intent access before it
        // reaches us; with `allow_other` it also carries `default_permissions`
        // and enforces the presented owner/mode bits itself (in which case this
        // is never called at all). Replied explicitly rather than left to
        // fuser's default, which logs a `[Not Implemented]` warn at the default
        // log floor — the same stream the serve-failure lines use (#364, #624).
        reply.ok();
    }

    fn read(
        &self,
        _req: &Request,
        ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        size: u32,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyData,
    ) {
        if platform::spotlight::is_marker(ino.0) {
            return reply.data(&[]);
        }
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
            let end = start
                .saturating_add(usize_from(u64::from(size)))
                .min(body.len());
            return reply.data(&body[start..end]);
        }
        // Reserve a slot on the dispatch thread before enqueuing; over the cap,
        // reject with EAGAIN so reads cannot grow the pool queue (#308).
        let Some(slot) = reserve_read_slot(&self.inflight_reads, MAX_INFLIGHT_READS) else {
            self.read_errors.fetch_add(1, Ordering::Relaxed);
            // Rate-limited: a saturated client retries rejected reads in a tight
            // loop, so this otherwise warns once per shed read for the whole storm.
            serve_warn!(
                "read({}) rejected: EAGAIN — {MAX_INFLIGHT_READS} reads already in flight (load-shedding)",
                ino.0
            );
            return reply.error(fuser::Errno::EAGAIN);
        };
        let core = Arc::clone(&self.core);
        let read_errors = Arc::clone(&self.read_errors);
        self.pool.submit_read(move || {
            // `_slot` (named) holds the guard until the read completes or the
            // worker panics, then releases it. Do NOT simplify to bare `_`: that
            // drops the guard immediately, releasing the slot before the work
            // runs and neutering the cap.
            let _slot = slot;
            READ_BUF.with(|b| {
                let mut buf = b.borrow_mut();
                // `reply` lives outside the panic boundary so a residual parser
                // panic in `read_into` still gets answered (EIO) instead of
                // unwinding the worker with no reply and hanging the read (#359).
                let outcome = synth_outcome(
                    "read",
                    ino.0,
                    std::panic::AssertUnwindSafe(|| {
                        core.read_into(
                            ino.0,
                            NonZeroU64::new(fh.0).map(Fh::from),
                            offset,
                            u64::from(size),
                            &mut buf,
                        )
                    }),
                );
                match outcome {
                    Ok(()) => reply.data(&buf),
                    Err(e) => {
                        // read_outcome already logged the cause (via reply_errno or
                        // the panic handler); count it for the read-error rate (#523).
                        read_errors.fetch_add(1, Ordering::Relaxed);
                        reply.error(e);
                    }
                }
                if buf.capacity() > MAX_RETAINED_READ_BUF {
                    buf.shrink_to(MAX_RETAINED_READ_BUF);
                }
            });
        });
    }

    fn readdir(
        &self,
        _req: &Request,
        ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        reply: ReplyDirectory,
    ) {
        self.fire_poll_refresh();
        if self.config.expose_metrics && ino.0 == metrics_dir::METRICS_DIR_INO {
            return reply_dir_page(reply, &metrics_dir::dir_listing(), offset);
        }
        let held = self
            .dir_handles
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .open
            .get(&fh.0)
            .map(|handle| Arc::clone(&handle.listing));
        // Lock released; the reply below runs without holding it.
        let Some(listing) = held else {
            // Unknown fh — the stateless sentinel, from a synthetic directory or
            // an over-cap `opendir` (#616). Rebuilding walks the virtual tree and
            // resolves the parent, so it is offloaded like every other blocking
            // op rather than run on the single dispatch thread: since #616 this
            // is the normal path for exactly the wide directories that make it
            // expensive (#623). `ReplyDirectory` is `Send`, so the worker answers.
            let core = Arc::clone(&self.core);
            let handles = Arc::clone(&self.dir_handles);
            let stateless = Arc::clone(&self.stateless_listings);
            let expose_metrics = self.config.expose_metrics;
            return self.pool.submit("readdir", move || {
                let loaded = core.tree_snapshot();
                let latest = || core.tree_snapshot();
                let page = stateless_page(&stateless, ino.0, offset, &loaded, latest, |snapshot| {
                    // An over-cap open is exactly the case where some *other*
                    // handle usually holds this directory's listing already, so
                    // probe the index before walking the tree again (#675). A hit
                    // is the listing the rebuild would produce: same directory,
                    // same generation.
                    if let Some(listing) = shared_listing(
                        &handles
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner),
                        (snapshot.id(), ino.0),
                    ) {
                        return Ok(listing);
                    }
                    synth_outcome(
                        "readdir",
                        ino.0,
                        std::panic::AssertUnwindSafe(|| {
                            build_dir_listing(snapshot, ino.0, expose_metrics)
                        }),
                    )
                    .map(Arc::new)
                });
                match page {
                    Ok(StatelessPage::Page(listing, start)) => {
                        reply_dir_entries(reply, &listing, start);
                    }
                    Ok(StatelessPage::Stale) => reply.error(stale_enumeration("readdir", ino.0)),
                    Err(e) => reply.error(e),
                }
            });
        };
        reply_dir_page(reply, &listing, offset);
    }

    /// `readdir` with each entry's attrs inline, so a client that stats what it
    /// lists — `ls -l`, every media scanner — spends one round trip on the
    /// directory instead of one more per entry (#667).
    ///
    /// The listing is found exactly as `readdir` finds it. What is new is the
    /// attrs: directories and the synthetic entries are filled inline, and the
    /// file entries fan out across the worker pool in rounds, since resolving a
    /// page serially on one worker would be slower for a threaded scanner than
    /// the `lookup`s it replaces. See [`spawn_plus_round`].
    fn readdirplus(
        &self,
        _req: &Request,
        ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        reply: ReplyDirectoryPlus,
    ) {
        self.fire_poll_refresh();
        self.readdirplus_calls.fetch_add(1, Ordering::Relaxed);
        let style = self.attr_style();
        if self.config.expose_metrics && ino.0 == metrics_dir::METRICS_DIR_INO {
            // Every entry here is inline, so this fills and replies without
            // touching the pool at all.
            return start_plus_fill(
                &self.core,
                &self.pool,
                style,
                true,
                Arc::new(metrics_dir::dir_listing()),
                PageStart::untagged(offset),
                reply,
            );
        }
        let held = self
            .dir_handles
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .open
            .get(&fh.0)
            .map(|handle| Arc::clone(&handle.listing));
        // Lock released; the fill below runs without holding it.
        let expose_metrics = self.config.expose_metrics;
        let Some(listing) = held else {
            // Unknown fh — the stateless sentinel, from a synthetic directory or
            // an over-cap `opendir` (#616). Rebuilding walks the tree, so it is
            // offloaded like every other blocking op; the shared index usually
            // spares it even that (#675).
            let core = Arc::clone(&self.core);
            let handles = Arc::clone(&self.dir_handles);
            let stateless = Arc::clone(&self.stateless_listings);
            let pool = self.pool.clone();
            return self.pool.submit("readdirplus", move || {
                let loaded = core.tree_snapshot();
                let latest = || core.tree_snapshot();
                let page = stateless_page(&stateless, ino.0, offset, &loaded, latest, |snapshot| {
                    if let Some(listing) = shared_listing(
                        &handles
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner),
                        (snapshot.id(), ino.0),
                    ) {
                        return Ok(listing);
                    }
                    synth_outcome(
                        "readdirplus",
                        ino.0,
                        std::panic::AssertUnwindSafe(|| {
                            build_dir_listing(snapshot, ino.0, expose_metrics)
                        }),
                    )
                    .map(Arc::new)
                });
                match page {
                    Ok(StatelessPage::Page(listing, start)) => {
                        start_plus_fill(&core, &pool, style, expose_metrics, listing, start, reply);
                    }
                    Ok(StatelessPage::Stale) => {
                        reply.error(stale_enumeration("readdirplus", ino.0));
                    }
                    Err(e) => reply.error(e),
                }
            });
        };
        start_plus_fill(
            &self.core,
            &self.pool,
            style,
            expose_metrics,
            listing,
            PageStart::untagged(offset),
            reply,
        );
    }
}

/// Read-only mount options tagged with the filesystem name, plus per-OS extras.
fn mount_config(fs_name: &str, allow_other: bool) -> Config {
    let mut cfg = Config::default();
    cfg.mount_options = platform::mount::options(fs_name, allow_other);
    cfg
}

/// Serializes the fusermount3 mount handshake (`Session::new`). That handshake
/// forks/execs `fusermount3` and passes the `/dev/fuse` fd back over a socket;
/// fork and the file-descriptor table are process-global, so two mounts running
/// it concurrently from one process race the fd table ("file descriptor N is not
/// a socket, can't send fuse fd"). The CLI mounts once per process, but library
/// embedders — and the parallel mount tests — can mount concurrently, so guard
/// the setup. The lock covers only mount establishment, never the session's
/// lifetime, so it does not serialize filesystem operations.
static MOUNT_SETUP: Mutex<()> = Mutex::new(());

/// Establish a mounted `Session`, serializing the racy fusermount3 handshake.
fn new_session(
    fs: MusefsFs,
    mountpoint: &Path,
    fs_name: &str,
    allow_other: bool,
) -> std::io::Result<Session<MusefsFs>> {
    // Validate the allow_other environment before taking the mount lock: the
    // /etc/fuse.conf read is unrelated to the fusermount3 handshake the lock
    // serializes, so it must not extend that critical section.
    platform::mount::check_allow_other(allow_other)?;
    // Recover from a poisoned lock: it guards only ordering, so a prior panic
    // during a mount leaves no inconsistent state to protect against.
    let _guard = MOUNT_SETUP
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    Session::new(fs, mountpoint, &mount_config(fs_name, allow_other))
}

/// Mount `core` at `mountpoint` with default fuse tuning, blocking until unmounted.
pub fn mount(core: Musefs, mountpoint: &Path, fs_name: &str) -> std::io::Result<()> {
    mount_with(core, mountpoint, fs_name, FuseConfig::default())
}

/// Mount `core` at `mountpoint` with explicit fuse tuning, blocking until unmounted.
pub fn mount_with(
    core: Musefs,
    mountpoint: &Path,
    fs_name: &str,
    config: FuseConfig,
) -> std::io::Result<()> {
    let allow_other = config.allow_other;
    let (files, dirs) = core.entry_counts();
    let mode = core.mode();
    let fs = MusefsFs::new(core, config);
    let cell = fs.notifier_cell();
    let session = new_session(fs, mountpoint, fs_name, allow_other)?;
    let _ = cell.set(session.notifier());
    let bg = session.spawn()?;
    // Mount succeeded and the session is serving: surface what was mounted so an
    // empty or wrong DB is distinguishable from a correctly serving one (#522).
    log::info!(
        "musefs mounted at {}: {files} files in {dirs} directories ({mode:?})",
        mountpoint.display()
    );
    bg.join()
}

/// Background-session mount with default tuning; the handle's `Drop` unmounts.
pub fn spawn(core: Musefs, mountpoint: &Path, fs_name: &str) -> std::io::Result<BackgroundSession> {
    spawn_with(core, mountpoint, fs_name, FuseConfig::default())
}

/// Background-session mount with explicit tuning; the handle's `Drop` unmounts.
pub fn spawn_with(
    core: Musefs,
    mountpoint: &Path,
    fs_name: &str,
    config: FuseConfig,
) -> std::io::Result<BackgroundSession> {
    let allow_other = config.allow_other;
    let fs = MusefsFs::new(core, config);
    let cell = fs.notifier_cell();
    let session = new_session(fs, mountpoint, fs_name, allow_other)?;
    // Set the notifier BEFORE `spawn()` starts the dispatch thread, so the first
    // request can't observe an empty cell. `session.notifier()` and the spawned
    // session's notifier clone the same channel sender, so they're equivalent.
    let _ = cell.set(session.notifier());
    session.spawn()
}

#[cfg(test)]
mod tests {
    use super::*;
    use musefs_core::CoreError;
    use std::time::Duration;

    #[test]
    fn maps_core_errors_to_errno() {
        assert_eq!(errno(&CoreError::NoEntry(7)).code(), libc::ENOENT);
        assert_eq!(errno(&CoreError::TrackNotFound(7)).code(), libc::ENOENT);
        assert_eq!(errno(&CoreError::IsDir(7)).code(), libc::EISDIR);
        assert_eq!(errno(&CoreError::NotADir(7)).code(), libc::ENOTDIR);
        assert_eq!(
            errno(&CoreError::BackingChanged("x".into())).code(),
            libc::EIO
        );
        let io = CoreError::Io(std::io::Error::from_raw_os_error(libc::ENOENT));
        assert_eq!(errno(&io).code(), libc::ENOENT);
        let io_other = CoreError::Io(std::io::Error::other("boom"));
        assert_eq!(errno(&io_other).code(), libc::EIO);
        assert_eq!(
            errno(&CoreError::OrphanedArt {
                track_id: 1,
                art_id: 2
            })
            .code(),
            libc::EIO
        );
        assert_eq!(
            errno(&CoreError::InvalidPictureType {
                track_id: 1,
                art_id: 2,
                value: 99,
            })
            .code(),
            libc::EIO
        );
        assert_eq!(
            errno(&CoreError::ArtTooLarge {
                track_id: 1,
                art_id: 2,
                byte_len: 16_711_681,
                cap: 16_711_680,
            })
            .code(),
            libc::EIO
        );
        assert_eq!(
            errno(&CoreError::HeaderTooLarge {
                requested: 67_108_865,
                cap: 67_108_864,
            })
            .code(),
            libc::EIO
        );
    }

    #[test]
    fn synth_outcome_passes_through_ok_value() {
        let r: Result<u32, _> = synth_outcome("getattr", 7, || Ok(42));
        assert_eq!(r.unwrap(), 42);
    }

    #[test]
    fn synth_outcome_maps_core_error_to_errno() {
        let r: Result<(), _> = synth_outcome("lookup", 7, || Err(CoreError::NoEntry(7)));
        assert_eq!(r.unwrap_err().code(), libc::ENOENT);
    }

    #[test]
    fn synth_outcome_catches_panic_as_eio() {
        let r: Result<(), _> = synth_outcome("open", 7, || panic!("parser exploded"));
        assert_eq!(r.unwrap_err().code(), libc::EIO);
    }

    /// Pins the upstream `threadpool` behavior the outer boundary defends
    /// against: a task that unwinds retires its worker, and the replacement gets
    /// a fresh `ThreadId` — the id `DbPool::PerThread` keys connections on
    /// (#669). If this ever stops holding, `execute_guarded` is free to shrink.
    #[test]
    fn an_unguarded_pool_task_retires_its_worker() {
        let pool = ThreadPool::new(1);
        let (tx, rx) = std::sync::mpsc::channel();
        let t = tx.clone();
        pool.execute(move || {
            t.send(std::thread::current().id()).unwrap();
            panic!("unguarded");
        });
        pool.join();
        pool.execute(move || tx.send(std::thread::current().id()).unwrap());
        pool.join();
        let (first, second) = (rx.recv().unwrap(), rx.recv().unwrap());
        assert_ne!(first, second, "an unwinding task must retire its worker");
        assert_eq!(pool.panic_count(), 1);
    }

    #[test]
    fn execute_guarded_keeps_the_worker_across_a_panic() {
        let pool = ThreadPool::new(1);
        let (tx, rx) = std::sync::mpsc::channel();
        let t = tx.clone();
        execute_guarded(&pool, "read", move || {
            t.send(std::thread::current().id()).unwrap();
            panic!("guarded");
        });
        pool.join();
        execute_guarded(&pool, "read", move || {
            tx.send(std::thread::current().id()).unwrap();
        });
        pool.join();
        let (first, second) = (rx.recv().unwrap(), rx.recv().unwrap());
        assert_eq!(first, second, "a caught panic must not retire the worker");
        assert_eq!(pool.panic_count(), 0);
    }

    /// Count this process's open fds pointing at `db_path` or its WAL sidecars
    /// (prefix match: a WAL reader holds up to three). Linux-only — it reads
    /// `/proc/self/fd`, which FreeBSD has no default equivalent for.
    #[cfg(target_os = "linux")]
    fn db_fd_count(db_path: &Path) -> usize {
        let prefix = db_path.to_str().unwrap();
        std::fs::read_dir("/proc/self/fd")
            .unwrap()
            .filter_map(|e| std::fs::read_link(e.unwrap().path()).ok())
            .filter(|target| target.to_string_lossy().starts_with(prefix))
            .count()
    }

    /// The end of #669: a panicking pool task used to retire its worker, and the
    /// replacement opened a second `DbPool` connection while the dead thread's
    /// entry stayed in the map forever. Guarded, the worker survives and the one
    /// connection is reused.
    // Linux-only: asserts fd counts via `db_fd_count` (/proc/self/fd).
    #[cfg(target_os = "linux")]
    #[test]
    fn a_panicking_task_strands_no_db_connection() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("guard.db");
        musefs_db::Db::open(&path).unwrap(); // create + migrate (writer, sets WAL)
        let db = Arc::new(musefs_core::DbPool::new(musefs_db::Db::open(&path).unwrap()).unwrap());
        let pool = ThreadPool::new(1);

        // Opens this worker's connection, then panics inside the boundary.
        let d = Arc::clone(&db);
        execute_guarded(&pool, "read", move || {
            d.with(|c| Ok(c.data_version()?)).unwrap();
            panic!("synthesis exploded");
        });
        pool.join();
        let after_panic = db_fd_count(&path);
        assert!(after_panic > 0, "the worker must have opened a connection");

        // The same worker serves the next task, so it reuses that connection.
        let d = Arc::clone(&db);
        execute_guarded(&pool, "read", move || {
            d.with(|c| Ok(c.data_version()?)).unwrap();
        });
        pool.join();
        assert_eq!(
            db_fd_count(&path),
            after_panic,
            "a caught panic must not cost a second connection"
        );
    }

    #[test]
    fn fuse_config_default_is_conservative() {
        let c = FuseConfig::default();
        assert_eq!(c.ttl, Duration::from_secs(1));
        assert_eq!(c.max_readahead, 512 * 1024);
        assert_eq!(c.max_background, 64);
        // #432: keep-cache is the one measured storage win and is now on by default.
        assert!(c.keep_cache);
        assert_eq!(c.file_mode, 0o444);
        assert_eq!(c.dir_mode, 0o555);
        assert_eq!(c.uid, rustix::process::getuid().as_raw());
        assert_eq!(c.gid, rustix::process::getgid().as_raw());
    }

    #[test]
    fn open_flags_sets_keep_cache_bit_only_when_enabled() {
        assert_eq!(open_flags(false), FopenFlags::empty());
        assert_eq!(open_flags(true), FopenFlags::FOPEN_KEEP_CACHE);
    }

    #[test]
    fn statfs_params_reports_nonzero_capacity_with_ample_free() {
        let (blocks, bfree, bavail, files, ffree, bsize, namelen, frsize) = statfs_params();
        // The whole point of #368: a non-zero total so capacity-checking
        // clients (Lidarr et al.) don't read the mount as a full/empty 0-byte fs.
        assert!(blocks > 0, "blocks must be non-zero");
        assert!(bavail > 0 && bfree > 0, "must advertise free space");
        assert!(
            bavail <= blocks && bfree <= blocks,
            "free cannot exceed total"
        );
        assert!(ffree <= files, "free inodes cannot exceed total");
        // bsize/namelen were already fine in fuser's default; keep them.
        assert_eq!(bsize, 512);
        assert_eq!(frsize, 512);
        assert_eq!(namelen, 255);
    }

    fn test_fs() -> (tempfile::TempDir, MusefsFs) {
        use musefs_core::{MountConfig, Musefs};
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = MountConfig::default();
        cfg.template = "$artist/$title".to_string();
        // Zero interval => poll_due() is always true, isolating the gate.
        cfg.poll_interval = std::time::Duration::ZERO;
        cfg.case_insensitive = false;
        let core =
            Musefs::open(musefs_db::Db::open(dir.path().join("m.db")).unwrap(), cfg).unwrap();
        (dir, MusefsFs::new(core, FuseConfig::default()))
    }

    #[test]
    fn explicit_workers_sets_pool_size() {
        use musefs_core::{MountConfig, Musefs};
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = MountConfig::default();
        cfg.template = "$artist/$title".to_string();
        cfg.poll_interval = std::time::Duration::ZERO;
        cfg.case_insensitive = false;
        let core =
            Musefs::open(musefs_db::Db::open(dir.path().join("w.db")).unwrap(), cfg).unwrap();
        let fs = MusefsFs::new(
            core,
            FuseConfig {
                workers: 3,
                ..FuseConfig::default()
            },
        );
        assert_eq!(fs.pool.max_count(), 3);
    }

    #[test]
    fn workers_zero_means_auto_oversized_pool() {
        let (_dir, fs) = test_fs(); // default config: workers == 0
        let auto = std::thread::available_parallelism().map_or(4, std::num::NonZero::get) * 2;
        assert_eq!(fs.pool.max_count(), auto);
    }

    /// #694: under the cap a job runs on a pool thread and holds its slot until
    /// it ends; at the cap the next job runs on the caller, and `try_submit`
    /// drops its job unrun — both counted.
    #[test]
    fn workers_queue_under_the_cap_and_run_in_place_over_it() {
        let workers = Workers::new(ThreadPool::new(1), 1);
        let (ids_tx, ids) = std::sync::mpsc::channel();
        let (release, parked) = std::sync::mpsc::channel::<()>();
        let tx = ids_tx.clone();
        workers.submit("test", move || {
            tx.send(std::thread::current().id()).unwrap();
            parked.recv().unwrap();
        });
        assert_ne!(ids.recv().unwrap(), std::thread::current().id(), "queued");
        assert_eq!(workers.admitted.load(Ordering::Relaxed), 1);

        let tx = ids_tx.clone();
        workers.submit("test", move || {
            tx.send(std::thread::current().id()).unwrap();
        });
        assert_eq!(
            ids.recv().unwrap(),
            std::thread::current().id(),
            "at the cap, run on the caller"
        );
        assert_eq!(workers.over_cap.load(Ordering::Relaxed), 1);

        let ran = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&ran);
        assert!(!workers.try_submit("test", move || flag.store(true, Ordering::SeqCst)));
        assert!(
            !ran.load(Ordering::SeqCst),
            "try_submit drops the job unrun"
        );
        assert_eq!(workers.over_cap.load(Ordering::Relaxed), 2);

        release.send(()).unwrap();
        workers.join();
        assert_eq!(
            workers.admitted.load(Ordering::Relaxed),
            0,
            "the slot comes back when its job ends"
        );
    }

    #[test]
    fn an_admitted_slot_comes_back_when_its_job_panics() {
        let workers = Workers::new(ThreadPool::new(1), 4);
        workers.submit("test", || panic!("boom"));
        workers.join();
        assert_eq!(workers.admitted.load(Ordering::Relaxed), 0);
        assert!(
            workers.try_submit("test", || {}),
            "and the gate admits again"
        );
        workers.join();
    }

    #[test]
    fn poll_pending_guard_clears_flag_on_panic() {
        let flag = Arc::new(AtomicBool::new(true));
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _g = PollPendingGuard(Arc::clone(&flag));
            panic!("boom");
        }));
        assert!(r.is_err());
        assert!(
            !flag.load(Ordering::SeqCst),
            "guard must clear the flag on unwind"
        );
    }

    #[test]
    fn poll_pending_guard_clears_flag_when_dropped_unrun() {
        // Models a dead pool dropping the poll job without running it: the guard
        // is built before `execute`, so dropping the un-run closure still clears
        // the gate rather than sticking it `true` forever (#369).
        let flag = Arc::new(AtomicBool::new(true));
        drop(PollPendingGuard(Arc::clone(&flag)));
        assert!(
            !flag.load(Ordering::SeqCst),
            "guard must clear the flag when dropped without running"
        );
    }

    #[test]
    fn fire_poll_refresh_single_flights_when_pending() {
        let (_d, fs) = test_fs();
        // Simulate a poll already in flight; the gate must reject new submissions.
        fs.poll_pending.store(true, Ordering::SeqCst);
        let queued = fs.refresh.queued_count();
        let active = fs.refresh.active_count();
        for _ in 0..50 {
            fs.fire_poll_refresh();
        }
        assert_eq!(
            fs.refresh.queued_count(),
            queued,
            "no task should be queued"
        );
        assert_eq!(
            fs.refresh.active_count(),
            active,
            "no task should be started"
        );
    }

    #[test]
    fn fire_poll_refresh_clears_gate_after_task() {
        let (_d, fs) = test_fs();
        assert!(!fs.poll_pending.load(Ordering::SeqCst));
        fs.fire_poll_refresh(); // poll_due() true (zero interval): gate taken, task runs
        fs.refresh.join(); // block until the poll task completes
        assert!(
            !fs.poll_pending.load(Ordering::SeqCst),
            "guard must clear the gate after the task finishes"
        );
    }

    /// A real tree snapshot to key handles by. Cheap — the tree behind it is
    /// the empty one `test_fs` mounts — and real, so [`TreeSnapshot::id`] means
    /// what it means in production: an address that stays unique while the
    /// snapshot is held.
    fn test_snapshot() -> (tempfile::TempDir, MusefsFs, TreeSnapshot) {
        let (dir, fs) = test_fs();
        let snapshot = fs.core.tree_snapshot();
        (dir, fs, snapshot)
    }

    fn listing(names: &[&str]) -> Arc<DirListing> {
        Arc::new(
            names
                .iter()
                .enumerate()
                .map(|(i, name)| (i as u64 + 2, FileType::RegularFile, (*name).to_string()))
                .collect(),
        )
    }

    /// #695: tag 0 is the plain index a held listing always used, and a tag
    /// rides above the index bits without disturbing it.
    #[test]
    fn an_untagged_cookie_is_the_index_it_always_was() {
        assert_eq!(dir_cookie(0, 7), 7);
        assert_eq!(split_dir_cookie(7), (0, 7));
        let tagged = dir_cookie(5, 3);
        assert_eq!(split_dir_cookie(tagged), (5, 3));
        assert!(
            tagged > u64::from(u32::MAX),
            "the tag lives above the index"
        );
    }

    #[test]
    fn stateless_listings_tag_per_generation_and_evict_the_least_recently_used() {
        let (_d, fs, snapshot) = test_snapshot();
        let mut listings = StatelessListings::default();
        let tag = listings.tag_for(snapshot.generation()).unwrap();
        assert_ne!(tag, 0, "0 is reserved for untagged cookies");
        assert_eq!(
            listings.tag_for(fs.core.tree_snapshot().generation()),
            Some(tag),
            "the same generation keeps its tag"
        );
        let cap = u64::try_from(MAX_STATELESS_LISTINGS).unwrap();
        for ino in 0..cap {
            listings.insert(ino, tag, listing(&["a"]));
        }
        assert!(
            listings.get(0, tag).is_some(),
            "touching 0 makes 1 the oldest"
        );
        listings.insert(cap, tag, listing(&["a"]));
        assert!(
            listings.get(1, tag).is_none(),
            "the least recently used goes"
        );
        assert!(listings.get(0, tag).is_some());
        assert!(listings.get(cap, tag).is_some());
    }

    /// #695: a stateless enumeration that a refresh interrupts keeps paging the
    /// listing it started on, so an entry inserted ahead of its cursor cannot
    /// shift what the next page returns — no entry twice, none skipped. Paging
    /// the new generation at the same index, as before, returned "b" again.
    #[test]
    fn a_refresh_between_stateless_pages_cannot_shift_the_enumeration() {
        let (dir, fs) = test_fs();
        let db = musefs_db::Db::open(dir.path().join("m.db")).unwrap();
        let add = |title: &str| {
            let id = db
                .upsert_track(&musefs_db::NewTrack {
                    backing_path: dir.path().join(format!("{title}.flac")),
                    format: musefs_db::Format::Flac,
                    audio_offset: 0,
                    audio_length: 1,
                    backing_size: 1,
                    backing_mtime_ns: 0,
                    backing_ctime_ns: 0,
                    backing_ino: None,
                })
                .unwrap();
            db.replace_tags(
                id,
                &[
                    musefs_db::Tag::new("artist", "Art", 0),
                    musefs_db::Tag::new("title", title, 0),
                ],
            )
            .unwrap();
        };
        let names = |listing: &DirListing, from: usize| -> Vec<String> {
            listing[from..]
                .iter()
                .map(|(_, _, name)| name.clone())
                .collect()
        };
        add("b");
        add("c");
        assert!(fs.core.poll_refresh().unwrap());
        let artist = fs
            .core
            .lookup(musefs_core::VirtualTree::ROOT, "Art")
            .unwrap();
        let listings = Mutex::new(StatelessListings::default());

        let first = fs.core.tree_snapshot();
        let (listing, page) = page_of(page_on(&fs, &listings, artist, 0, &first));
        assert_eq!(page.index, 0);
        let all = names(&listing, 0);
        assert_eq!(all.len(), 4, "{all:?}");
        let (b, c) = (all[2].clone(), all[3].clone());
        assert!(b.starts_with('b') && c.starts_with('c'), "{all:?}");
        // The kernel took ".", "..", b, and hands back the cookie after b.
        let resume = dir_cookie(page.tag, 3);

        add("a");
        assert!(fs.core.poll_refresh().unwrap());
        let second = fs.core.tree_snapshot();
        let (listing, resumed) = page_of(page_on(&fs, &listings, artist, resume, &second));
        assert_eq!(
            resumed.tag, page.tag,
            "the enumeration stays on its generation"
        );
        assert_eq!(
            names(&listing, resumed.index),
            [c],
            "no b twice, no c skipped"
        );

        let (listing, fresh) = page_of(page_on(&fs, &listings, artist, 0, &second));
        assert_ne!(
            fresh.tag, page.tag,
            "a new enumeration takes the new generation"
        );
        let now = names(&listing, 0);
        assert_eq!(now.len(), 5, "{now:?}");
        assert!(now[2].starts_with('a'), "{now:?}");
    }

    /// [`stateless_page`] as the handlers call it, over `fs`'s tree, for a
    /// worker that loaded `loaded`.
    fn page_on(
        fs: &MusefsFs,
        listings: &Mutex<StatelessListings>,
        ino: u64,
        offset: u64,
        loaded: &TreeSnapshot,
    ) -> StatelessPage {
        stateless_page(
            listings,
            ino,
            offset,
            loaded,
            || fs.core.tree_snapshot(),
            |snapshot| build_dir_listing(snapshot, ino, false).map(Arc::new),
        )
        .unwrap()
    }

    /// #695: the current generation's tag only ever moves forward. A generation
    /// older than the current one gets no tag at all, and leaves the current one
    /// where it was.
    #[test]
    fn stateless_tags_only_ever_advance() {
        let mut listings = StatelessListings::default();
        let five = listings.tag_for(5).expect("the first generation is tagged");
        assert_eq!(listings.tag_for(5), Some(five), "the same generation");
        assert_eq!(
            listings.tag_for(4),
            None,
            "an older generation must not take the tag"
        );
        assert_eq!(listings.tag_for(5), Some(five), "nor move it");
        let six = listings.tag_for(6).expect("a newer generation is tagged");
        assert_ne!(six, five, "a newer generation is a new tag");
        assert_eq!(listings.tag_for(5), None, "and the old one stays behind it");
    }

    /// The page a [`stateless_page`] resolved to, failing the test on a stale one.
    fn page_of(page: StatelessPage) -> (Arc<DirListing>, PageStart) {
        match page {
            StatelessPage::Page(listing, start) => (listing, start),
            StatelessPage::Stale => panic!("expected a page, got a stale enumeration"),
        }
    }

    /// Pin a cap's worth of other directories under `tag`, evicting every
    /// listing pinned before them.
    fn pin_a_cap_of_others(listings: &Mutex<StatelessListings>, tag: u32) {
        let mut guard = listings.lock().unwrap();
        for i in 0..u64::try_from(MAX_STATELESS_LISTINGS).unwrap() {
            guard.insert(u64::MAX / 2 + i, tag, listing(&["other"]));
        }
    }

    /// A track under artist "Art" titled `title`, in `dir`'s store.
    fn add_art_track(dir: &std::path::Path, db: &musefs_db::Db, title: &str) {
        let id = db
            .upsert_track(&musefs_db::NewTrack {
                backing_path: dir.join(format!("{title}.flac")),
                format: musefs_db::Format::Flac,
                audio_offset: 0,
                audio_length: 1,
                backing_size: 1,
                backing_mtime_ns: 0,
                backing_ctime_ns: 0,
                backing_ino: None,
            })
            .unwrap();
        db.replace_tags(
            id,
            &[
                musefs_db::Tag::new("artist", "Art", 0),
                musefs_db::Tag::new("title", title, 0),
            ],
        )
        .unwrap();
    }

    /// #695: eviction alone does not cost an enumeration its place. A cookie
    /// whose listing was evicted, with no refresh since, rebuilds that same
    /// generation's listing and resumes at its index.
    #[test]
    fn an_evicted_listing_resumes_while_its_generation_is_current() {
        let (dir, fs) = test_fs();
        let db = musefs_db::Db::open(dir.path().join("m.db")).unwrap();
        add_art_track(dir.path(), &db, "b");
        add_art_track(dir.path(), &db, "c");
        assert!(fs.core.poll_refresh().unwrap());
        let artist = fs
            .core
            .lookup(musefs_core::VirtualTree::ROOT, "Art")
            .unwrap();
        let listings = Mutex::new(StatelessListings::default());

        let snapshot = fs.core.tree_snapshot();
        let (listing, page) = page_of(page_on(&fs, &listings, artist, 0, &snapshot));
        let resume = dir_cookie(page.tag, 3);
        pin_a_cap_of_others(&listings, page.tag);
        assert!(
            listings.lock().unwrap().get(artist, page.tag).is_none(),
            "precondition: the enumeration's listing is evicted"
        );

        let same = fs.core.tree_snapshot();
        let (rebuilt, resumed) = page_of(page_on(&fs, &listings, artist, resume, &same));
        assert_eq!((resumed.tag, resumed.index), (page.tag, 3));
        assert_eq!(*rebuilt, *listing, "the same generation's listing, rebuilt");
    }

    /// #695: a cookie whose listing was evicted *and* whose generation a refresh
    /// has since replaced is refused as stale. Resuming the new generation at
    /// the old index, as eviction used to, returned "b" a second time.
    #[test]
    fn an_evicted_listing_from_a_replaced_generation_is_refused_as_stale() {
        let (dir, fs) = test_fs();
        let db = musefs_db::Db::open(dir.path().join("m.db")).unwrap();
        add_art_track(dir.path(), &db, "b");
        add_art_track(dir.path(), &db, "c");
        assert!(fs.core.poll_refresh().unwrap());
        let artist = fs
            .core
            .lookup(musefs_core::VirtualTree::ROOT, "Art")
            .unwrap();
        let listings = Mutex::new(StatelessListings::default());

        let first = fs.core.tree_snapshot();
        let (_, page) = page_of(page_on(&fs, &listings, artist, 0, &first));
        // The kernel took ".", "..", b, and hands back the cookie after b.
        let resume = dir_cookie(page.tag, 3);
        pin_a_cap_of_others(&listings, page.tag);

        add_art_track(dir.path(), &db, "a");
        assert!(fs.core.poll_refresh().unwrap());
        let second = fs.core.tree_snapshot();
        let outcome = page_on(&fs, &listings, artist, resume, &second);
        assert!(
            matches!(outcome, StatelessPage::Stale),
            "an unresumable cookie must not be paged against the new generation"
        );

        let (_, fresh) = page_of(page_on(&fs, &listings, artist, 0, &second));
        assert_ne!(fresh.tag, page.tag, "a new enumeration is unaffected");
    }

    /// A worker can start a fresh enumeration on a snapshot it loaded before a
    /// refresh that another enumeration has already tagged. That must not
    /// re-tag the current generation back to the old one: the enumeration on the
    /// new generation would then find its tag replaced, and once its listing
    /// was evicted its next page would be refused as stale although nothing it
    /// was paging has changed.
    #[test]
    fn a_worker_on_an_older_snapshot_cannot_move_the_tag_backwards() {
        let (dir, fs) = test_fs();
        let db = musefs_db::Db::open(dir.path().join("m.db")).unwrap();
        add_art_track(dir.path(), &db, "b");
        add_art_track(dir.path(), &db, "c");
        assert!(fs.core.poll_refresh().unwrap());
        let artist = fs
            .core
            .lookup(musefs_core::VirtualTree::ROOT, "Art")
            .unwrap();
        let older = fs.core.tree_snapshot();
        add_art_track(dir.path(), &db, "a");
        assert!(fs.core.poll_refresh().unwrap());
        let newer = fs.core.tree_snapshot();
        let listings = Mutex::new(StatelessListings::default());

        let (_, page) = page_of(page_on(&fs, &listings, artist, 0, &newer));
        let resume = dir_cookie(page.tag, 3);
        pin_a_cap_of_others(&listings, page.tag);

        // The late worker: a fresh enumeration of another directory, on the
        // pre-refresh snapshot it loaded. It is served the newer generation.
        let root = musefs_core::VirtualTree::ROOT;
        let (_, late) = page_of(page_on(&fs, &listings, root, 0, &older));
        assert_eq!(
            late.tag, page.tag,
            "the late worker reloads onto the newer tree"
        );

        let (_, resumed) = page_of(page_on(&fs, &listings, artist, resume, &newer));
        assert_eq!(
            (resumed.tag, resumed.index),
            (page.tag, 3),
            "the current generation keeps its tag, so its enumeration resumes"
        );
    }

    #[test]
    fn a_stale_enumeration_replies_estale() {
        assert_eq!(stale_enumeration("readdir", 7), fuser::Errno::ESTALE);
    }

    fn empty_dir_handles() -> DirHandles {
        DirHandles::default()
    }

    /// Fill `handles` with `n` handles on distinct directories, so a cap test
    /// starts at the occupancy it means to test.
    fn fill_dir_handles(handles: &mut DirHandles, snapshot: &TreeSnapshot, n: u64) {
        let counter = AtomicU64::new(1);
        let rejections = AtomicU64::new(0);
        for ino in 0..n {
            try_admit_dir_handle(
                handles,
                &counter,
                &rejections,
                usize_from(n),
                (snapshot.id(), ino),
                snapshot.clone(),
                listing(&["a"]),
            )
            .expect("fixture admits below its own cap");
        }
    }

    #[test]
    fn try_admit_dir_handle_admits_and_allocates_id_below_cap() {
        let (_d, _fs, snapshot) = test_snapshot();
        let mut handles = empty_dir_handles();
        let counter = AtomicU64::new(1); // matches the live `dir_fh` start
        let rejections = AtomicU64::new(0);
        let fh = try_admit_dir_handle(
            &mut handles,
            &counter,
            &rejections,
            2,
            (snapshot.id(), 1),
            snapshot.clone(),
            listing(&["a"]),
        );
        assert_eq!(
            fh,
            Some(1),
            "first admit uses the pre-increment counter value"
        );
        assert_eq!(handles.open.len(), 1);
        assert!(handles.open.contains_key(&1));
        assert_eq!(counter.load(Ordering::Relaxed), 2, "id allocated on admit");
        assert_eq!(
            rejections.load(Ordering::Relaxed),
            0,
            "an admit must not count as a rejection"
        );
    }

    #[test]
    fn try_admit_dir_handle_rejects_at_cap_without_inserting_or_advancing_id() {
        let (_d, _fs, snapshot) = test_snapshot();
        let mut handles = empty_dir_handles();
        fill_dir_handles(&mut handles, &snapshot, 2);
        let counter = AtomicU64::new(12);
        let rejections = AtomicU64::new(0);
        let fh = try_admit_dir_handle(
            &mut handles,
            &counter,
            &rejections,
            2,
            (snapshot.id(), 9),
            snapshot.clone(),
            listing(&["a"]),
        );
        assert_eq!(fh, None, "at cap must reject");
        assert_eq!(handles.open.len(), 2, "must not insert on reject");
        assert_eq!(
            counter.load(Ordering::Relaxed),
            12,
            "must not burn a dir_fh id on reject"
        );
        assert_eq!(
            rejections.load(Ordering::Relaxed),
            1,
            "the reject must be metered (#626)"
        );
    }

    /// The point of #675: the cap bounds handles, and handles on one directory
    /// at one generation cost one listing between them. Before this, 1,024
    /// opens of a 300,000-entry directory pinned 1,024 copies of it.
    #[test]
    fn handles_on_one_directory_and_generation_share_one_listing() {
        let (_d, _fs, snapshot) = test_snapshot();
        let mut handles = empty_dir_handles();
        let counter = AtomicU64::new(1);
        let rejections = AtomicU64::new(0);
        let key = (snapshot.id(), 1);
        let mut fhs = Vec::new();
        for _ in 0..8 {
            // Each open builds its own listing, as two racing `opendir` workers
            // that both missed the probe would.
            fhs.push(
                try_admit_dir_handle(
                    &mut handles,
                    &counter,
                    &rejections,
                    16,
                    key,
                    snapshot.clone(),
                    listing(&["a", "b"]),
                )
                .expect("below cap"),
            );
        }
        let first = Arc::clone(&handles.open[&fhs[0]].listing);
        for fh in &fhs {
            assert!(
                Arc::ptr_eq(&first, &handles.open[fh].listing),
                "every handle on the same key must hold the same allocation"
            );
        }
        assert_eq!(
            handles.shared.len(),
            1,
            "one directory at one generation indexes one listing"
        );
        assert_eq!(Arc::strong_count(&first), 9, "8 handles plus this clone");
    }

    /// A different directory, or the same directory at a different tree
    /// generation, is a different listing — sharing must not outlive a refresh.
    #[test]
    fn a_different_directory_or_generation_does_not_share() {
        let (_d, fs, snapshot) = test_snapshot();
        let mut handles = empty_dir_handles();
        let counter = AtomicU64::new(1);
        let rejections = AtomicU64::new(0);
        let admit = |handles: &mut DirHandles, key: DirListingKey, snap: TreeSnapshot| {
            try_admit_dir_handle(
                handles,
                &counter,
                &rejections,
                16,
                key,
                snap,
                listing(&["a"]),
            )
            .expect("below cap")
        };
        let a = admit(&mut handles, (snapshot.id(), 1), snapshot.clone());
        let b = admit(&mut handles, (snapshot.id(), 2), snapshot.clone());
        assert!(
            !Arc::ptr_eq(&handles.open[&a].listing, &handles.open[&b].listing),
            "two directories are two listings"
        );

        // A snapshot taken later is a distinct generation even when the tree
        // behind it has not changed, because it is a distinct allocation.
        let later = fs.core.tree_snapshot();
        let c = admit(&mut handles, (later.id(), 1), later.clone());
        if later.id() != snapshot.id() {
            assert!(
                !Arc::ptr_eq(&handles.open[&a].listing, &handles.open[&c].listing),
                "a new generation must not serve the old generation's listing"
            );
        }
    }

    #[test]
    fn releasedir_drops_the_index_entry_with_the_last_handle() {
        let (_d, _fs, snapshot) = test_snapshot();
        let mut handles = empty_dir_handles();
        let counter = AtomicU64::new(1);
        let rejections = AtomicU64::new(0);
        let key = (snapshot.id(), 1);
        let first = try_admit_dir_handle(
            &mut handles,
            &counter,
            &rejections,
            16,
            key,
            snapshot.clone(),
            listing(&["a"]),
        )
        .unwrap();
        let second = try_admit_dir_handle(
            &mut handles,
            &counter,
            &rejections,
            16,
            key,
            snapshot.clone(),
            listing(&["a"]),
        )
        .unwrap();

        release_dir_handle(&mut handles, first);
        assert_eq!(handles.open.len(), 1, "the other handle stays open");
        assert!(
            shared_listing(&handles, key).is_some(),
            "the listing outlives the first release: a handle still serves it"
        );

        release_dir_handle(&mut handles, second);
        assert!(handles.open.is_empty());
        assert!(
            handles.shared.is_empty(),
            "the index must not outlive the last handle on the key"
        );

        release_dir_handle(&mut handles, DIR_FH_STATELESS);
        release_dir_handle(&mut handles, second);
        assert!(
            handles.open.is_empty() && handles.shared.is_empty(),
            "an unknown or repeated releasedir removes nothing"
        );
    }

    /// The index entry must live exactly as long as the handles on it, which is
    /// why it counts them rather than testing the `Weak`. A listing outlives its
    /// handle whenever an in-flight `readdir` is serving a clone of it, and an
    /// entry kept alive by that clone would pin the tree generation it names —
    /// leaving the key matchable by a later tree allocated at the same address,
    /// which would then be served a listing of the old generation.
    #[test]
    fn shared_index_entry_lives_exactly_as_long_as_its_handles() {
        let (_d, _fs, snapshot) = test_snapshot();
        let mut handles = empty_dir_handles();
        let counter = AtomicU64::new(1);
        let rejections = AtomicU64::new(0);
        let key = (snapshot.id(), 1);
        let admit = |handles: &mut DirHandles| {
            try_admit_dir_handle(
                handles,
                &counter,
                &rejections,
                16,
                key,
                snapshot.clone(),
                listing(&["a", "b"]),
            )
            .expect("below cap")
        };
        let first = admit(&mut handles);
        let second = admit(&mut handles);

        // What an in-flight `readdir` holds: a clone taken under the lock and
        // served outside it.
        let in_flight = Arc::clone(&handles.open[&first].listing);

        release_dir_handle(&mut handles, first);
        assert_eq!(
            handles.shared.len(),
            1,
            "the entry stays while another handle is open on it"
        );

        release_dir_handle(&mut handles, second);
        assert!(
            handles.shared.is_empty(),
            "the last handle takes the entry with it, even though a readdir is \
             still serving the listing"
        );
        assert_eq!(
            in_flight.len(),
            2,
            "and that readdir's listing stays valid: the index hands listings \
             out, it does not gate access to them"
        );
    }

    #[test]
    fn over_cap_opendir_falls_back_to_the_stateless_fh() {
        assert_eq!(
            dir_open_fh(Some(7)),
            FileHandle(7),
            "an admitted handle keeps its snapshot id"
        );
        assert_eq!(
            dir_open_fh(None),
            FileHandle(DIR_FH_STATELESS),
            "over cap must degrade to the stateless fh, not reply ENFILE (#616)"
        );
        assert_eq!(
            DIR_FH_STATELESS, 0,
            "readdir rebuilds for an unknown fh and releasedir(0) removes nothing"
        );
    }

    #[test]
    fn try_admit_dir_handle_counts_every_rejection_once() {
        // The gauge cannot substitute for this: occupancy sits pinned at the cap
        // while the counter climbs, so only the counter records how many opens
        // were turned away (#626).
        let (_d, _fs, snapshot) = test_snapshot();
        let mut handles = empty_dir_handles();
        let counter = AtomicU64::new(1);
        let rejections = AtomicU64::new(0);
        let admit = |handles: &mut DirHandles, ino: u64| {
            try_admit_dir_handle(
                handles,
                &counter,
                &rejections,
                1,
                (snapshot.id(), ino),
                snapshot.clone(),
                listing(&["a"]),
            )
        };
        assert!(
            admit(&mut handles, 1).is_some(),
            "the first open fits cap 1"
        );
        for ino in 2..5 {
            assert!(admit(&mut handles, ino).is_none(), "the table is full");
        }
        assert_eq!(
            rejections.load(Ordering::Relaxed),
            3,
            "one increment per rejected opendir, not per burst"
        );
    }

    #[test]
    fn try_admit_dir_handle_frees_slot_after_removal() {
        let (_d, _fs, snapshot) = test_snapshot();
        let mut handles = empty_dir_handles();
        fill_dir_handles(&mut handles, &snapshot, 2);
        let freed = *handles.open.keys().min().expect("fixture admitted two");
        let counter = AtomicU64::new(12);
        let rejections = AtomicU64::new(0);
        release_dir_handle(&mut handles, freed); // releasedir frees a slot
        let fh = try_admit_dir_handle(
            &mut handles,
            &counter,
            &rejections,
            2,
            (snapshot.id(), 9),
            snapshot.clone(),
            listing(&["a"]),
        );
        assert_eq!(fh, Some(12), "a freed slot admits again");
        assert_eq!(handles.open.len(), 2);
        assert!(
            !handles.open.contains_key(&freed),
            "the freed handle stays gone"
        );
        assert!(
            handles.open.contains_key(&12),
            "the new handle fills the freed slot"
        );
        assert_eq!(
            rejections.load(Ordering::Relaxed),
            0,
            "re-admitting into a freed slot is not a rejection"
        );
    }

    fn test_style() -> AttrStyle {
        AttrStyle {
            uid: 501,
            gid: 20,
            file_mode: 0o444,
            dir_mode: 0o555,
            mount_time: SystemTime::UNIX_EPOCH + Duration::from_secs(1000),
            ttl: Duration::from_secs(1),
        }
    }

    /// What `readdirplus` must not send to the pool: directories (free —
    /// `Musefs::getattr` answers them without touching the DB) and the
    /// synthetic entries, whose attrs are static (#667).
    #[test]
    fn inline_plus_entry_covers_dirs_and_synthetic_entries() {
        let style = test_style();

        let dir = inline_plus_entry(7, FileType::Directory, false, &style).expect("dirs are free");
        assert_eq!(dir.attr.ino, INodeNo(7));
        assert_eq!(dir.attr.kind, FileType::Directory);
        assert_eq!(dir.attr.perm, style.dir_mode);
        assert_eq!(dir.attr.nlink, 2);
        assert_eq!(dir.attr.size, 0);
        assert_eq!(dir.attr.mtime, style.mount_time, "no mtime: the mount's");
        assert_eq!(dir.ttl, style.ttl);

        let metrics = inline_plus_entry(
            metrics_dir::METRICS_FILE_INO,
            FileType::RegularFile,
            true,
            &style,
        )
        .expect("the metrics file has static attrs");
        assert_eq!(metrics.attr.ino, INodeNo(metrics_dir::METRICS_FILE_INO));
        assert_eq!(metrics.ttl, style.ttl);

        assert!(
            inline_plus_entry(9, FileType::RegularFile, true, &style).is_none(),
            "a real file needs the DB, so it belongs on the pool"
        );
        assert!(
            inline_plus_entry(
                metrics_dir::METRICS_FILE_INO,
                FileType::RegularFile,
                false,
                &style
            )
            .is_none(),
            "without --expose-metrics that inode is not ours to answer for"
        );
    }

    /// A page's first entry that cannot be resolved still has to be listed, or
    /// the file drops out of the listing (#667) — but with attrs the kernel
    /// refuses to link, never ones it would apply to an inode it holds (#694).
    /// `fuse_valid_size` rejects any size above `i64::MAX`; a size of 0, the old
    /// placeholder, truncated a mapped file's page cache.
    #[test]
    fn an_unlinkable_entry_lists_the_name_with_attrs_the_kernel_rejects() {
        let style = test_style();
        let file = unlinkable_plus_entry(9, FileType::RegularFile, &style);
        assert!(
            file.attr.size > u64::try_from(i64::MAX).unwrap(),
            "the size must fail the kernel's own validation, not pass as a real one"
        );
        assert_eq!(file.ttl, Duration::ZERO, "nothing for the kernel to cache");
        assert_eq!(
            file.attr.ino,
            INodeNo(9),
            "a zero inode would hide the name"
        );
        assert_eq!(file.attr.kind, FileType::RegularFile);

        let dir = unlinkable_plus_entry(7, FileType::Directory, &style);
        assert_eq!(
            dir.attr.kind,
            FileType::Directory,
            "type comes from readdir"
        );
        assert!(dir.attr.size > u64::try_from(i64::MAX).unwrap());
        assert_eq!(dir.ttl, Duration::ZERO);
    }

    /// A resolved entry for `plan_round`, told apart by its inode.
    fn resolved(ino: u64) -> Resolution {
        let style = test_style();
        Resolution::Resolved(PlusEntry {
            attr: make_attr(
                ino,
                4096,
                (FileType::RegularFile, style.file_mode, 1),
                style.uid,
                style.gid,
                style.mount_time,
            ),
            ttl: style.ttl,
        })
    }

    /// A plan as `(inode per emitted entry, None for unlinkable; ends_page)`.
    fn shape(plan: &RoundPlan) -> (Vec<Option<u64>>, bool) {
        let emits = plan
            .emits
            .iter()
            .map(|emit| match emit {
                PlusEmit::Attrs(entry) => Some(entry.attr.ino.0),
                PlusEmit::Unlinkable => None,
            })
            .collect();
        (emits, plan.ends_page)
    }

    /// #694: a round whose every entry resolved goes out whole, and the fill
    /// carries on to the next round.
    #[test]
    fn a_resolved_round_is_sent_whole_and_the_page_goes_on() {
        let plan = plan_round(
            &[Some(resolved(2)), Some(resolved(3)), Some(resolved(4))],
            true,
        );
        assert_eq!(shape(&plan), (vec![Some(2), Some(3), Some(4)], false));
        let later = plan_round(&[Some(resolved(5))], false);
        assert_eq!(shape(&later), (vec![Some(5)], false));
    }

    /// #694: the page ends before the first entry with no attrs of its own,
    /// whether its resolution never ran (over the admission cap) or ran and
    /// failed. Nothing after it is sent, even entries that did resolve: the
    /// kernel resumes from the cookie of the last entry sent.
    #[test]
    fn a_round_ends_the_page_before_its_first_entry_without_attrs() {
        let unrun = plan_round(&[Some(resolved(2)), None, Some(resolved(4))], true);
        assert_eq!(shape(&unrun), (vec![Some(2)], true), "never ran");

        let failed = plan_round(
            &[
                Some(resolved(2)),
                Some(Resolution::Failed),
                Some(resolved(4)),
            ],
            false,
        );
        assert_eq!(shape(&failed), (vec![Some(2)], true), "ran and failed");

        let at_round_start = plan_round(&[None, Some(resolved(3))], false);
        assert_eq!(
            shape(&at_round_start),
            (vec![], true),
            "a later round can end the page at its start: earlier rounds filled it"
        );
    }

    /// #694: a page's first entry has had its attempt, so deferring it again
    /// would leave the page empty, which the kernel reads as the end of the
    /// directory. It is listed unlinkable, and the round goes on from there.
    #[test]
    fn a_page_first_entry_without_attrs_is_listed_unlinkable() {
        let failed = plan_round(&[Some(Resolution::Failed), Some(resolved(3))], true);
        assert_eq!(shape(&failed), (vec![None, Some(3)], false));

        let lost = plan_round(&[None, Some(resolved(3)), None], true);
        assert_eq!(
            shape(&lost),
            (vec![None, Some(3)], true),
            "a lost first task is listed too, and later gaps still end the page"
        );
    }

    #[test]
    fn reserve_read_slot_admits_up_to_cap() {
        let inflight = Arc::new(AtomicUsize::new(0));
        let g1 = reserve_read_slot(&inflight, 2);
        let g2 = reserve_read_slot(&inflight, 2);
        assert!(g1.is_some() && g2.is_some(), "two reservations fit cap 2");
        assert_eq!(inflight.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn reserve_read_slot_rejects_over_cap_and_releases() {
        let inflight = Arc::new(AtomicUsize::new(0));
        let _g1 = reserve_read_slot(&inflight, 2);
        let _g2 = reserve_read_slot(&inflight, 2);
        let g3 = reserve_read_slot(&inflight, 2);
        assert!(g3.is_none(), "third reservation exceeds cap 2");
        assert_eq!(
            inflight.load(Ordering::Relaxed),
            2,
            "a rejected reservation must release its own increment"
        );
    }

    #[test]
    fn read_slot_guard_releases_on_drop_and_panic() {
        let inflight = Arc::new(AtomicUsize::new(0));
        {
            let _g = reserve_read_slot(&inflight, 4).expect("under cap");
            assert_eq!(inflight.load(Ordering::Relaxed), 1);
        }
        assert_eq!(
            inflight.load(Ordering::Relaxed),
            0,
            "guard releases on drop"
        );

        let inflight2 = Arc::new(AtomicUsize::new(0));
        let i2 = Arc::clone(&inflight2);
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _g = reserve_read_slot(&i2, 4).expect("under cap");
            panic!("boom");
        }));
        assert!(r.is_err());
        assert_eq!(
            inflight2.load(Ordering::Relaxed),
            0,
            "guard releases its slot on unwind"
        );
    }
}

#[cfg(test)]
mod errno_tests {
    use super::{errno, placed_errno};
    use musefs_core::CoreError;

    #[test]
    fn handle_table_full_maps_to_enfile() {
        assert_eq!(errno(&CoreError::HandleTableFull).code(), libc::ENFILE);
    }

    /// #708: every `CoreError` variant has a place in the mapping, and the
    /// place it is meant to have.
    ///
    /// `errno` needs a wildcard, so a variant added to `musefs-core` compiles
    /// here without one. It fails this instead: core's sample list cannot leave
    /// a variant out (its own test sees to that), and an unplaced one has no
    /// `placed_errno`. The table below has no wildcard either, so a variant
    /// moved out of the `EIO` arm, or a new one given an arm, has to be
    /// recorded here as a decision.
    #[test]
    fn every_core_error_variant_is_placed_and_maps_as_intended() {
        for err in CoreError::every_variant_for_test() {
            let expected = match &err {
                CoreError::NoEntry(_) | CoreError::TrackNotFound(_) => libc::ENOENT,
                CoreError::IsDir(_) => libc::EISDIR,
                CoreError::NotADir(_) => libc::ENOTDIR,
                CoreError::HandleTableFull => libc::ENFILE,
                // The OS errno passes through.
                CoreError::Io(source) | CoreError::BackingIo { source, .. } => source
                    .raw_os_error()
                    .expect("the samples carry a real OS errno"),
                CoreError::BackingChanged(_)
                | CoreError::DerivedStateStale(_)
                | CoreError::Db(_)
                | CoreError::DbOpen { .. }
                | CoreError::Mp4MetadataTooLarge { .. }
                | CoreError::OrphanedArt { .. }
                | CoreError::ArtTooLarge { .. }
                | CoreError::InvalidPictureType { .. }
                | CoreError::HeaderTooLarge { .. }
                | CoreError::TrackFieldTooLarge { .. }
                | CoreError::TrackMetadataTooLarge { .. }
                | CoreError::Format(_)
                | CoreError::InvalidTemplate(_) => libc::EIO,
                other => panic!(
                    "{other:?} is a CoreError variant this table does not know: decide \
                     its errno, give it an arm in placed_errno, and record it here"
                ),
            };
            let placed = placed_errno(&err).unwrap_or_else(|| {
                panic!("{err:?} has no arm in placed_errno, so errno collapses it to EIO unplaced")
            });
            assert_eq!(placed.code(), expected, "{err:?}");
            assert_eq!(errno(&err).code(), expected, "{err:?}");
        }
    }
}

/// The serve-path warn limiter lives in `musefs-core`, but `reply_errno`'s warn
/// must still be *attributed* to this crate: the troubleshooting guide documents
/// per-crate filtering (`RUST_LOG=warn,musefs_fuse=debug`), and that only works
/// while the record's target follows the call site. This is what makes
/// `musefs_core::serve_warn!` a macro rather than a shared function (#650).
#[cfg(test)]
mod warn_target_tests {
    use std::sync::Mutex;

    use super::reply_errno;
    use musefs_core::CoreError;

    static CAPTURED: Mutex<Vec<(String, String)>> = Mutex::new(Vec::new());
    static CAPTURE_LOGGER: CaptureLogger = CaptureLogger;

    /// Global logger keeping each record's target and rendered message.
    struct CaptureLogger;

    impl log::Log for CaptureLogger {
        fn enabled(&self, _: &log::Metadata<'_>) -> bool {
            true
        }
        fn log(&self, record: &log::Record<'_>) {
            CAPTURED
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push((record.target().to_string(), record.args().to_string()));
        }
        fn flush(&self) {}
    }

    #[test]
    fn reply_errno_warn_is_attributed_to_musefs_fuse() {
        log::set_logger(&CAPTURE_LOGGER).expect("only this test installs a logger");
        // Over-budget warns drop to debug and must carry the same target.
        log::set_max_level(log::LevelFilter::Debug);

        let err = CoreError::BackingChanged("warn-target-probe.flac".into());
        reply_errno("read", 4242, &err);

        let captured = CAPTURED
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let hits: Vec<&(String, String)> = captured
            .iter()
            .filter(|(_, m)| m.contains("warn-target-probe.flac"))
            .collect();
        assert_eq!(hits.len(), 1, "expected exactly one record, got {hits:?}");
        assert_eq!(
            hits[0].0, "musefs_fuse",
            "reply_errno must log under this crate, not the limiter's module"
        );
    }
}
