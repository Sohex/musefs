//! FUSE filesystem binding for musefs: translates VFS calls into `musefs-core`
//! operations. fuser dispatches on a single thread; blocking operations are
//! offloaded onto a bounded worker pool and answered via the `Send` reply
//! objects, so a slow backing read cannot stall metadata operations.

use std::ffi::OsStr;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, SystemTime};

use threadpool::ThreadPool;

use crate::convert::{assemble_dir_listing, to_file_attr};
use fuser::{
    BackgroundSession, Config, FileHandle, FileType, Filesystem, FopenFlags, Generation, INodeNo,
    InitFlags, KernelConfig, LockOwner, Notifier, OpenFlags, ReplyAttr, ReplyData, ReplyDirectory,
    ReplyEmpty, ReplyEntry, ReplyOpen, ReplyStatfs, ReplyXattr, Request, Session,
};
use musefs_core::CoreError;
use musefs_core::Fh;
use musefs_core::Musefs;
use musefs_core::convert::usize_from;
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
    /// Keep the kernel page cache across opens (`FOPEN_KEEP_CACHE`). An external
    /// re-tag auto-invalidates the affected inode on refresh (`poll_refresh_notify`
    /// → `inval_inode`), so cached bytes are dropped when content changes.
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
}

impl Default for FuseConfig {
    fn default() -> FuseConfig {
        FuseConfig {
            ttl: Duration::from_secs(1),
            max_readahead: 512 * 1024,
            max_background: 64,
            keep_cache: false,
            uid: rustix::process::getuid().as_raw(),
            gid: rustix::process::getgid().as_raw(),
            file_mode: 0o444,
            dir_mode: 0o555,
            allow_other: false,
            expose_metrics: false,
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
/// underlying errno when present; everything structural collapses to `EIO`.
pub fn errno(err: &CoreError) -> fuser::Errno {
    match err {
        CoreError::NoEntry(_) | CoreError::TrackNotFound(_) => fuser::Errno::ENOENT,
        CoreError::IsDir(_) => fuser::Errno::EISDIR,
        CoreError::NotADir(_) => fuser::Errno::ENOTDIR,
        CoreError::HandleTableFull => fuser::Errno::ENFILE,
        CoreError::Io(e) => fuser::Errno::from_i32(e.raw_os_error().unwrap_or(libc::EIO)),
        CoreError::BackingChanged(_)
        | CoreError::Db(_)
        | CoreError::DbOpen { .. }
        | CoreError::Mp4MetadataTooLarge { .. }
        | CoreError::OrphanedArt { .. }
        | CoreError::ArtTooLarge { .. }
        | CoreError::InvalidPictureType { .. }
        | CoreError::HeaderTooLarge { .. }
        | CoreError::Format(_)
        | CoreError::InvalidTemplate(_) => fuser::Errno::EIO,
    }
}

/// Log a serve-path failure before it collapses to an errno reply, so the
/// cause (e.g. the offending path in `BackingChanged`, or an `Io` error with
/// no raw OS errno) is not lost. Routine tree-shape misses — a stale inode
/// after a refresh, kernel path probing — stay at debug to avoid noise.
fn reply_errno(op: &str, ino: u64, err: &CoreError) -> fuser::Errno {
    match err {
        CoreError::NoEntry(_)
        | CoreError::TrackNotFound(_)
        | CoreError::IsDir(_)
        | CoreError::NotADir(_) => log::debug!("{op}({ino}) failed: {err}"),
        _ => log::warn!("{op}({ino}) failed: {err}"),
    }
    errno(err)
}

/// Run a read's synthesis under a panic boundary so a residual parser panic —
/// one the format-layer alloc guards (`id3v2_alloc_safe` and friends) don't
/// catch — becomes an `EIO` reply instead of unwinding the pool worker. fuser's
/// reply objects send nothing when dropped, so an unwound worker leaves the
/// kernel waiting forever and the read syscall hangs at 0% CPU with no error
/// logged (#359). A `CoreError` maps to its errno via [`reply_errno`]; a caught
/// panic is logged and mapped to `EIO`.
fn read_outcome<F>(ino: u64, work: F) -> Result<(), fuser::Errno>
where
    F: FnOnce() -> Result<(), CoreError> + std::panic::UnwindSafe,
{
    match std::panic::catch_unwind(work) {
        Ok(Ok(())) => Ok(()),
        Ok(Err(e)) => Err(reply_errno("read", ino, &e)),
        Err(payload) => {
            let msg = payload
                .downcast_ref::<&str>()
                .copied()
                .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
                .unwrap_or("<non-string panic>");
            log::error!("read({ino}) worker panicked in synthesis: {msg}; replying EIO");
            Err(fuser::Errno::EIO)
        }
    }
}

/// One directory's readdir snapshot: `(child inode, entry type, name)` rows.
/// Aliased so the handle-map signatures stay readable (and dodge
/// `clippy::type_complexity`).
type DirListing = Vec<(u64, FileType, String)>;

/// Cap on concurrently-open directory handles (#307). Each `opendir` snapshots a
/// full `DirListing`, so an unreleased handle pins memory ~ (entries × name
/// length); the cap bounds the *number* of snapshots, not their inherent size (a
/// single `ls` of the widest directory already allocates one). 1024 sits well
/// above a heavy parallel indexer's concurrent-dir-handle count (~hundreds), so
/// legitimate clients never hit it, while an over-cap `opendir` returns `ENFILE`
/// — the directory-side analogue of the file-handle `HandleTableFull → ENFILE`.
const MAX_DIR_HANDLES: usize = 1024;

/// Cap on concurrently-open `.musefs-metrics/metrics` handles (#394). Each `open`
/// pins one rendered snapshot until `release`; the cap bounds the map the same way
/// `MAX_DIR_HANDLES` bounds dir snapshots, so a client that opens the file without
/// closing cannot grow it without bound. An over-cap `open` returns `ENFILE`.
const MAX_METRICS_HANDLES: usize = 1024;

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

/// Admit a directory handle under the caller's `dir_handles` lock, enforcing
/// `MAX_DIR_HANDLES` (#307). Returns the freshly allocated handle id on admit, or
/// `None` when the table is at `cap` (the caller replies `ENFILE`). The id is
/// drawn from `counter` only on the admit path, and the whole check-then-insert
/// runs under the single lock the caller holds, so concurrent `opendir` closures
/// cannot race the count past the cap and a rejected open burns no id.
fn try_admit_dir_handle(
    handles: &mut std::collections::HashMap<u64, Arc<DirListing>>,
    counter: &AtomicU64,
    cap: usize,
    listing: DirListing,
) -> Option<u64> {
    if handles.len() >= cap {
        return None;
    }
    let fh = counter.fetch_add(1, Ordering::Relaxed);
    handles.insert(fh, Arc::new(listing));
    Some(fh)
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
    pool: ThreadPool,
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
    /// Per-open directory listing snapshots, keyed by the fh handed out by
    /// `opendir`. A paginated `readdir` clones the `Arc` under the lock and
    /// serves it lock-free, so building the listing is O(N) per `ls`, not per
    /// `readdir` call (#176).
    ///
    /// All three lock sites recover a poisoned mutex via `into_inner` rather than
    /// propagating (#194). Every op under the lock is a `HashMap` insert/remove
    /// (plus `Arc::new`, whose only failure mode — allocation — aborts the process
    /// rather than unwinding, so it can't poison), and a `HashMap` mutation cannot
    /// leave a partially-observable map across a single lock acquisition. So even a
    /// poisoning panic can't tear a later `readdir`'s view; recovery is deliberate.
    #[allow(clippy::type_complexity)]
    dir_handles: Arc<Mutex<std::collections::HashMap<u64, Arc<Vec<(u64, FileType, String)>>>>>,
    /// Monotonic dir-handle id (starts at 1; 0 stays the stateless sentinel).
    ///
    /// Unlike the file slab's generation-encoded keys (`facade.rs`, ABA-safe by
    /// construction), this is a bare never-recycled counter — sufficient precisely
    /// *because* it never recycles: an id is handed out once and never reused, so a
    /// stale or duplicate `releasedir` can only `remove` an id that names no live
    /// handle, never evict a different open dir (#192). A 64-bit monotonic counter
    /// cannot wrap within any real process lifetime.
    dir_fh: Arc<AtomicU64>,
    /// In-flight foreground-read counter. `read` reserves a slot before enqueuing;
    /// over `MAX_INFLIGHT_READS` the read is rejected with `EAGAIN`, capping the
    /// otherwise-unbounded pool queue (#308).
    inflight_reads: Arc<AtomicUsize>,
    /// Per-open rendered `.musefs-metrics/metrics` buffers, keyed by the fh handed
    /// out at `open` (#394). Each open snapshots once; reads slice it by absolute
    /// offset; `release` drops it. Empty/untouched unless `expose_metrics` is on.
    metrics_handles: Arc<Mutex<std::collections::HashMap<u64, Arc<Vec<u8>>>>>,
    /// Monotonic fh source for `metrics_handles` (starts at 1; never 0).
    metrics_fh: Arc<AtomicU64>,
}

impl MusefsFs {
    pub fn new(core: Musefs, config: FuseConfig) -> MusefsFs {
        // Work is I/O-bound (especially on NFS), so oversize the pool vs CPUs.
        let workers = std::thread::available_parallelism().map_or(4, std::num::NonZero::get) * 2;
        let structure_only = core.mode() == musefs_core::Mode::StructureOnly;
        MusefsFs {
            core: Arc::new(core),
            // `ThreadPool`'s queue is unbounded, so foreground reads are gated by
            // `inflight_reads`/`MAX_INFLIGHT_READS` before submission (#308) and
            // directory handles are capped at `MAX_DIR_HANDLES` (#307); both reject
            // over-cap work rather than letting it grow process memory.
            // `max_background` (set in `init`) separately caps the kernel's
            // background/readahead requests.
            pool: ThreadPool::new(workers),
            uid: config.uid,
            gid: config.gid,
            mount_time: SystemTime::now(),
            config,
            notifier: Arc::new(OnceLock::new()),
            poll_pending: Arc::new(AtomicBool::new(false)),
            passthrough: platform::passthrough::PassthroughState::new(structure_only),
            dir_handles: Arc::new(Mutex::new(std::collections::HashMap::new())),
            dir_fh: Arc::new(AtomicU64::new(1)),
            inflight_reads: Arc::new(AtomicUsize::new(0)),
            metrics_handles: Arc::new(Mutex::new(std::collections::HashMap::new())),
            metrics_fh: Arc::new(AtomicU64::new(1)),
        }
    }

    fn notifier_cell(&self) -> Arc<OnceLock<Notifier>> {
        Arc::clone(&self.notifier)
    }

    /// Fire `poll_refresh` on the worker pool (off the dispatch thread), but only
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
            self.pool.execute(move || {
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
            self.pool.execute(move || {
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
    fn render_metrics(&self) -> Vec<u8> {
        let core = self.core.telemetry();
        let dir_handles = self
            .dir_handles
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len() as u64;
        let fuse = musefs_core::FuseTelemetry {
            uptime_seconds: self.mount_time.elapsed().map_or(0, |d| d.as_secs()),
            reads_inflight: self.inflight_reads.load(Ordering::Relaxed) as u64,
            reads_inflight_max: MAX_INFLIGHT_READS as u64,
            dir_handles,
            dir_handles_max: MAX_DIR_HANDLES as u64,
            pool_workers: self.pool.max_count() as u64,
            pool_active: self.pool.active_count() as u64,
            pool_queued: self.pool.queued_count() as u64,
            passthrough: self
                .passthrough
                .telemetry()
                .map(|(disabled, active)| musefs_core::PassthroughTelemetry { disabled, active }),
        };
        let alloc = allocator_stats();
        let syscalls = syscall_snapshot();
        musefs_core::render_prometheus(&core, &fuse, alloc.as_ref(), syscalls.as_ref()).into_bytes()
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
        self.pool.execute(move || match core.getattr(child) {
            Ok(attr) => reply.entry(
                &ttl,
                &to_file_attr(&attr, uid, gid, fm, dm, mt),
                Generation(0),
            ),
            Err(e) => reply.error(reply_errno("lookup", child, &e)),
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
        self.pool.execute(move || match core.getattr(ino.0) {
            Ok(attr) => reply.attr(&ttl, &to_file_attr(&attr, uid, gid, fm, dm, mt)),
            Err(e) => reply.error(reply_errno("getattr", ino.0, &e)),
        });
    }

    fn open(&self, _req: &Request, ino: INodeNo, _flags: OpenFlags, reply: ReplyOpen) {
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
        self.pool.execute(move || {
            let fh = match core.open_handle(ino.0) {
                Ok(fh) => fh,
                Err(e) => return reply.error(reply_errno("open", ino.0, &e)),
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
        self.dir_handles
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&fh.0);
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
        // reject with EAGAIN so the unbounded pool queue can't grow (#308).
        let Some(slot) = reserve_read_slot(&self.inflight_reads, MAX_INFLIGHT_READS) else {
            return reply.error(fuser::Errno::EAGAIN);
        };
        let core = Arc::clone(&self.core);
        self.pool.execute(move || {
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
                let outcome = read_outcome(
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
                    Err(e) => reply.error(e),
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
        mut reply: ReplyDirectory,
    ) {
        self.fire_poll_refresh();
        if self.config.expose_metrics && ino.0 == metrics_dir::METRICS_DIR_INO {
            let listing = metrics_dir::dir_listing();
            for (i, (child, kind, name)) in listing.iter().enumerate().skip(usize_from(offset)) {
                if reply.add(INodeNo(*child), (i + 1) as u64, *kind, name) {
                    break;
                }
            }
            return reply.ok();
        }
        let snapshot = self
            .dir_handles
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&fh.0)
            .map(Arc::clone);
        // Lock released; the reply loop below runs without holding it.
        let listing = match snapshot {
            Some(l) => l,
            // Unknown fh (e.g. fh 0): build once inline so we never regress.
            None => match build_dir_listing(&self.core, ino.0, self.config.expose_metrics) {
                Ok(l) => Arc::new(l),
                Err(e) => return reply.error(reply_errno("readdir", ino.0, &e)),
            },
        };
        for (i, (child, kind, name)) in listing.iter().enumerate().skip(usize_from(offset)) {
            // The stored offset is the index of the *next* entry to return.
            if reply.add(INodeNo(*child), (i + 1) as u64, *kind, name) {
                break;
            }
        }
        reply.ok();
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
    let fs = MusefsFs::new(core, config);
    let cell = fs.notifier_cell();
    let session = new_session(fs, mountpoint, fs_name, allow_other)?;
    let _ = cell.set(session.notifier());
    let bg = session.spawn()?;
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
    fn read_outcome_passes_through_ok() {
        let r = read_outcome(7, || Ok(()));
        assert!(r.is_ok());
    }

    #[test]
    fn read_outcome_maps_core_error_to_errno() {
        let r = read_outcome(7, || Err(CoreError::NoEntry(7)));
        assert_eq!(r.unwrap_err().code(), libc::ENOENT);
    }

    #[test]
    fn read_outcome_catches_panic_as_eio() {
        let r = read_outcome(7, || panic!("parser exploded"));
        assert_eq!(r.unwrap_err().code(), libc::EIO);
    }

    #[test]
    fn fuse_config_default_is_conservative() {
        let c = FuseConfig::default();
        assert_eq!(c.ttl, Duration::from_secs(1));
        assert_eq!(c.max_readahead, 512 * 1024);
        assert_eq!(c.max_background, 64);
        assert!(!c.keep_cache);
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
        use musefs_core::{Mode, MountConfig, Musefs};
        let dir = tempfile::tempdir().unwrap();
        let cfg = MountConfig {
            template: "$artist/$title".to_string(),
            fallbacks: std::collections::BTreeMap::new(),
            default_fallback: "Unknown".to_string(),
            mode: Mode::Synthesis,
            // Zero interval => poll_due() is always true, isolating the gate.
            poll_interval: std::time::Duration::ZERO,
            case_insensitive: false,
            read_ahead_budget: 64 * 1024 * 1024,
            read_ahead_prefetch: false,
        };
        let core =
            Musefs::open(musefs_db::Db::open(dir.path().join("m.db")).unwrap(), cfg).unwrap();
        (dir, MusefsFs::new(core, FuseConfig::default()))
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
        let queued = fs.pool.queued_count();
        let active = fs.pool.active_count();
        for _ in 0..50 {
            fs.fire_poll_refresh();
        }
        assert_eq!(fs.pool.queued_count(), queued, "no task should be queued");
        assert_eq!(fs.pool.active_count(), active, "no task should be started");
    }

    #[test]
    fn fire_poll_refresh_clears_gate_after_task() {
        let (_d, fs) = test_fs();
        assert!(!fs.poll_pending.load(Ordering::SeqCst));
        fs.fire_poll_refresh(); // poll_due() true (zero interval): gate taken, task runs
        fs.pool.join(); // block until the poll task completes
        assert!(
            !fs.poll_pending.load(Ordering::SeqCst),
            "guard must clear the gate after the task finishes"
        );
    }

    fn empty_dir_handles() -> std::collections::HashMap<u64, Arc<DirListing>> {
        std::collections::HashMap::new()
    }

    #[test]
    fn try_admit_dir_handle_admits_and_allocates_id_below_cap() {
        let mut handles = empty_dir_handles();
        let counter = AtomicU64::new(1); // matches the live `dir_fh` start
        let fh = try_admit_dir_handle(&mut handles, &counter, 2, Vec::new());
        assert_eq!(
            fh,
            Some(1),
            "first admit uses the pre-increment counter value"
        );
        assert_eq!(handles.len(), 1);
        assert!(handles.contains_key(&1));
        assert_eq!(counter.load(Ordering::Relaxed), 2, "id allocated on admit");
    }

    #[test]
    fn try_admit_dir_handle_rejects_at_cap_without_inserting_or_advancing_id() {
        let mut handles = empty_dir_handles();
        handles.insert(10, Arc::new(Vec::new()));
        handles.insert(11, Arc::new(Vec::new()));
        let counter = AtomicU64::new(12);
        let fh = try_admit_dir_handle(&mut handles, &counter, 2, Vec::new());
        assert_eq!(fh, None, "at cap must reject");
        assert_eq!(handles.len(), 2, "must not insert on reject");
        assert_eq!(
            counter.load(Ordering::Relaxed),
            12,
            "must not burn a dir_fh id on reject"
        );
    }

    #[test]
    fn try_admit_dir_handle_frees_slot_after_removal() {
        let mut handles = empty_dir_handles();
        handles.insert(10, Arc::new(Vec::new()));
        handles.insert(11, Arc::new(Vec::new()));
        let counter = AtomicU64::new(12);
        handles.remove(&10); // releasedir frees a slot
        let fh = try_admit_dir_handle(&mut handles, &counter, 2, Vec::new());
        assert_eq!(fh, Some(12), "a freed slot admits again");
        assert_eq!(handles.len(), 2);
        assert!(!handles.contains_key(&10), "the freed handle stays gone");
        assert!(
            handles.contains_key(&12),
            "the new handle fills the freed slot"
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
    use super::errno;
    use musefs_core::CoreError;

    #[test]
    fn handle_table_full_maps_to_enfile() {
        assert_eq!(errno(&CoreError::HandleTableFull).code(), libc::ENFILE);
    }
}
