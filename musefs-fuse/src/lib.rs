//! FUSE filesystem binding for musefs: translates VFS calls into `musefs-core`
//! operations. fuser dispatches on a single thread; blocking operations are
//! offloaded onto a bounded worker pool and answered via the `Send` reply
//! objects, so a slow backing read cannot stall metadata operations.

use std::ffi::OsStr;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, SystemTime};

use threadpool::ThreadPool;

use crate::convert::{assemble_dir_listing, to_file_attr};
use fuser::{
    BackgroundSession, Config, FileHandle, FileType, Filesystem, FopenFlags, Generation, INodeNo,
    InitFlags, KernelConfig, LockOwner, Notifier, OpenFlags, ReplyAttr, ReplyData, ReplyDirectory,
    ReplyEmpty, ReplyEntry, ReplyOpen, Request, Session,
};
use musefs_core::CoreError;
use musefs_core::Fh;
use musefs_core::Musefs;
use musefs_core::convert::usize_from;
use std::num::NonZeroU64;

mod convert;
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
    /// bounded only by client concurrency, not by this.
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

/// Build a directory's full readdir listing once. Shared by `opendir`
/// (snapshotted per fh) and the `readdir` fallback for an unknown fh.
fn build_dir_listing(core: &Musefs, ino: u64) -> Result<Vec<(u64, FileType, String)>, CoreError> {
    let entries = core.readdir(ino)?;
    let parent = core.parent(ino).unwrap_or(ino);
    let marker = platform::spotlight::marker_dir_entry(ino);
    Ok(assemble_dir_listing(ino, parent, entries, marker))
}

/// Clears the `fire_poll_refresh` single-flight gate when the poll task ends,
/// on every exit path including a panic in `poll_refresh_notify` (#89).
struct PollPendingGuard<'a>(&'a AtomicBool);

impl Drop for PollPendingGuard<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
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
    #[allow(clippy::type_complexity)]
    dir_handles: Arc<Mutex<std::collections::HashMap<u64, Arc<Vec<(u64, FileType, String)>>>>>,
    /// Monotonic dir-handle id (starts at 1; 0 stays the stateless sentinel).
    dir_fh: Arc<AtomicU64>,
}

impl MusefsFs {
    pub fn new(core: Musefs, config: FuseConfig) -> MusefsFs {
        // Work is I/O-bound (especially on NFS), so oversize the pool vs CPUs.
        let workers = std::thread::available_parallelism().map_or(4, std::num::NonZero::get) * 2;
        let structure_only = core.mode() == musefs_core::Mode::StructureOnly;
        MusefsFs {
            core: Arc::new(core),
            // `ThreadPool`'s queue is unbounded. `max_background` (set in `init`)
            // caps the kernel's *background/readahead* requests, bounding that
            // class of work; foreground reads are bounded only by client
            // concurrency, so a wide parallel read storm can still queue jobs.
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
        let core = Arc::clone(&self.core);
        let pending = Arc::clone(&self.poll_pending);
        if self.config.keep_cache {
            let notifier = Arc::clone(&self.notifier);
            self.pool.execute(move || {
                let _guard = PollPendingGuard(&pending);
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
                let _guard = PollPendingGuard(&pending);
                if let Err(e) = core.poll_refresh() {
                    log::warn!("poll_refresh failed: {e}");
                }
            });
        }
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
        let core = Arc::clone(&self.core);
        let flags = open_flags(self.config.keep_cache);
        let passthrough = self.passthrough.clone();
        self.pool.execute(move || {
            let fh = match core.open_handle(ino.0) {
                Ok(fh) => fh,
                Err(e) => return reply.error(reply_errno("open", ino.0, &e)),
            };
            platform::passthrough::reply_open(&passthrough, &core, fh, reply, flags);
        });
    }

    fn opendir(&self, _req: &Request, ino: INodeNo, _flags: OpenFlags, reply: ReplyOpen) {
        self.fire_poll_refresh();
        let core = Arc::clone(&self.core);
        let handles = Arc::clone(&self.dir_handles);
        let counter = Arc::clone(&self.dir_fh);
        self.pool.execute(move || {
            let listing = match build_dir_listing(&core, ino.0) {
                Ok(l) => l,
                Err(e) => return reply.error(reply_errno("opendir", ino.0, &e)),
            };
            let fh = counter.fetch_add(1, Ordering::Relaxed);
            handles
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert(fh, Arc::new(listing));
            reply.opened(FileHandle(fh), FopenFlags::empty());
        });
    }

    fn release(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        // Cheap (a backing-map remove + a slab remove); no need to offload to the pool.
        if let Some(fh) = NonZeroU64::new(fh.0) {
            // Drops the backing registration (fires the close ioctl on Linux);
            // a no-op for plain handles and on non-Linux.
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
        let core = Arc::clone(&self.core);
        self.pool.execute(move || {
            READ_BUF.with(|b| {
                let mut buf = b.borrow_mut();
                match core.read_into(
                    ino.0,
                    NonZeroU64::new(fh.0).map(Fh::from),
                    offset,
                    u64::from(size),
                    &mut buf,
                ) {
                    Ok(()) => reply.data(&buf),
                    Err(e) => reply.error(reply_errno("read", ino.0, &e)),
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
            None => match build_dir_listing(&self.core, ino.0) {
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
fn mount_config(fs_name: &str) -> Config {
    let mut cfg = Config::default();
    cfg.mount_options = platform::mount::options(fs_name);
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
) -> std::io::Result<Session<MusefsFs>> {
    // Recover from a poisoned lock: it guards only ordering, so a prior panic
    // during a mount leaves no inconsistent state to protect against.
    let _guard = MOUNT_SETUP
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    Session::new(fs, mountpoint, &mount_config(fs_name))
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
    let fs = MusefsFs::new(core, config);
    let cell = fs.notifier_cell();
    let session = new_session(fs, mountpoint, fs_name)?;
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
    let fs = MusefsFs::new(core, config);
    let cell = fs.notifier_cell();
    let session = new_session(fs, mountpoint, fs_name)?;
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
        };
        let core =
            Musefs::open(musefs_db::Db::open(dir.path().join("m.db")).unwrap(), cfg).unwrap();
        (dir, MusefsFs::new(core, FuseConfig::default()))
    }

    #[test]
    fn poll_pending_guard_clears_flag_on_panic() {
        let flag = Arc::new(AtomicBool::new(true));
        let f = Arc::clone(&flag);
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _g = PollPendingGuard(&f);
            panic!("boom");
        }));
        assert!(r.is_err());
        assert!(
            !flag.load(Ordering::SeqCst),
            "guard must clear the flag on unwind"
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
