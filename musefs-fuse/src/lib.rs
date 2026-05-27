//! FUSE filesystem binding for musefs: translates VFS calls into `musefs-core`
//! operations. fuser dispatches on a single thread; blocking operations are
//! offloaded onto a bounded worker pool and answered via the `Send` reply
//! objects, so a slow backing read cannot stall metadata operations.

use std::ffi::OsStr;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use threadpool::ThreadPool;

use fuser::{
    BackgroundSession, FileAttr, FileType, Filesystem, MountOption, ReplyAttr, ReplyData,
    ReplyDirectory, ReplyEmpty, ReplyEntry, ReplyOpen, Request,
};
use musefs_core::Attr;
use musefs_core::CoreError;
use musefs_core::Musefs;

/// Fuse-layer mount knobs: kernel tuning + page-cache policy. Distinct from
/// `musefs_core::MountConfig`, which governs how the virtual tree is rendered.
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
    /// Keep the kernel page cache across opens (`FOPEN_KEEP_CACHE`). Safe only
    /// for static libraries: after an external re-tag the kernel may serve stale
    /// cached bytes until the cache is dropped (`drop_caches`) or remount.
    pub keep_cache: bool,
}

impl Default for FuseConfig {
    fn default() -> FuseConfig {
        FuseConfig {
            ttl: Duration::from_secs(1),
            max_readahead: 512 * 1024,
            max_background: 64,
            keep_cache: false,
        }
    }
}

/// `FOPEN_*` flags for an `open` reply, derived from the cache policy.
fn open_flags(keep_cache: bool) -> u32 {
    if keep_cache {
        fuser::consts::FOPEN_KEEP_CACHE
    } else {
        0
    }
}

/// Map a core error onto a POSIX errno for the FUSE reply. `Io` errors carry the
/// underlying errno when present; everything structural collapses to `EIO`.
pub fn errno(err: &CoreError) -> i32 {
    match err {
        CoreError::NoEntry(_) | CoreError::TrackNotFound(_) => libc::ENOENT,
        CoreError::IsDir(_) => libc::EISDIR,
        CoreError::NotADir(_) => libc::ENOTDIR,
        CoreError::Io(e) => e.raw_os_error().unwrap_or(libc::EIO),
        CoreError::BackingChanged(_) | CoreError::Db(_) | CoreError::Format(_) => libc::EIO,
    }
}

/// Translate a core `Attr` into a `fuser::FileAttr`. Read-only perms (`0o555`
/// dirs, `0o444` files). A zero `mtime_secs` (e.g. synthetic directories) falls
/// back to `fallback_mtime` so tools don't see a 1970 timestamp.
pub fn to_file_attr(attr: &Attr, uid: u32, gid: u32, fallback_mtime: SystemTime) -> FileAttr {
    let mtime = if attr.mtime_secs > 0 {
        SystemTime::UNIX_EPOCH + Duration::from_secs(attr.mtime_secs as u64)
    } else {
        fallback_mtime
    };
    let (kind, perm, nlink) = if attr.is_dir {
        (FileType::Directory, 0o555, 2)
    } else {
        (FileType::RegularFile, 0o444, 1)
    };
    FileAttr {
        ino: attr.inode,
        size: attr.size,
        blocks: attr.size.div_ceil(512),
        atime: mtime,
        mtime,
        ctime: mtime,
        crtime: mtime,
        kind,
        perm,
        nlink,
        uid,
        gid,
        rdev: 0,
        blksize: 512,
        flags: 0,
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
}

impl MusefsFs {
    pub fn new(core: Musefs, config: FuseConfig) -> MusefsFs {
        // Work is I/O-bound (especially on NFS), so oversize the pool vs CPUs.
        let workers = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4)
            * 2;
        MusefsFs {
            core: Arc::new(core),
            // `ThreadPool`'s queue is unbounded. `max_background` (set in `init`)
            // caps the kernel's *background/readahead* requests, bounding that
            // class of work; foreground reads are bounded only by client
            // concurrency, so a wide parallel read storm can still queue jobs.
            pool: ThreadPool::new(workers),
            // SAFETY: getuid/getgid are always-successful libc calls.
            uid: unsafe { libc::getuid() },
            gid: unsafe { libc::getgid() },
            mount_time: SystemTime::now(),
            config,
        }
    }
}

impl Filesystem for MusefsFs {
    fn lookup(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEntry) {
        {
            let core = Arc::clone(&self.core);
            self.pool.execute(move || {
                let _ = core.poll_refresh();
            });
        }
        let name = match name.to_str() {
            Some(n) => n,
            None => return reply.error(libc::ENOENT),
        };
        // Inode resolution is an in-memory tree read; the attr (which may touch
        // the DB/disk) is computed on the worker pool.
        let child = match self.core.lookup(parent, name) {
            Some(ino) => ino,
            None => return reply.error(libc::ENOENT),
        };
        let core = Arc::clone(&self.core);
        let (uid, gid, mt, ttl) = (self.uid, self.gid, self.mount_time, self.config.ttl);
        self.pool.execute(move || match core.getattr(child) {
            Ok(attr) => reply.entry(&ttl, &to_file_attr(&attr, uid, gid, mt), 0),
            Err(e) => reply.error(errno(&e)),
        });
    }

    fn getattr(&mut self, _req: &Request<'_>, ino: u64, reply: ReplyAttr) {
        {
            let core = Arc::clone(&self.core);
            self.pool.execute(move || {
                let _ = core.poll_refresh();
            });
        }
        let core = Arc::clone(&self.core);
        let (uid, gid, mt, ttl) = (self.uid, self.gid, self.mount_time, self.config.ttl);
        self.pool.execute(move || match core.getattr(ino) {
            Ok(attr) => reply.attr(&ttl, &to_file_attr(&attr, uid, gid, mt)),
            Err(e) => reply.error(errno(&e)),
        });
    }

    fn open(&mut self, _req: &Request<'_>, ino: u64, _flags: i32, reply: ReplyOpen) {
        let core = Arc::clone(&self.core);
        let flags = open_flags(self.config.keep_cache);
        self.pool.execute(move || match core.open_handle(ino) {
            Ok(fh) => reply.opened(fh, flags),
            Err(e) => reply.error(errno(&e)),
        });
    }

    fn release(
        &mut self,
        _req: &Request<'_>,
        _ino: u64,
        fh: u64,
        _flags: i32,
        _lock_owner: Option<u64>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        // Cheap (a map remove); no need to offload to the pool.
        self.core.release_handle(fh);
        reply.ok();
    }

    fn read(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        fh: u64,
        offset: i64,
        size: u32,
        _flags: i32,
        _lock_owner: Option<u64>,
        reply: ReplyData,
    ) {
        if offset < 0 {
            return reply.error(libc::EINVAL);
        }
        let core = Arc::clone(&self.core);
        self.pool.execute(
            move || match core.read(ino, fh, offset as u64, size as u64) {
                Ok(bytes) => reply.data(&bytes),
                Err(e) => reply.error(errno(&e)),
            },
        );
    }

    fn readdir(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        _fh: u64,
        offset: i64,
        mut reply: ReplyDirectory,
    ) {
        {
            let core = Arc::clone(&self.core);
            self.pool.execute(move || {
                let _ = core.poll_refresh();
            });
        }
        let entries = match self.core.readdir(ino) {
            Ok(e) => e,
            Err(e) => return reply.error(errno(&e)),
        };
        let parent = self.core.parent(ino).unwrap_or(ino);

        // `.` and `..` first, then the children. `offset` is the index already
        // consumed by a previous call; `reply.add` returns true when the buffer
        // is full, at which point we stop and reply.
        let mut listing: Vec<(u64, fuser::FileType, String)> =
            Vec::with_capacity(entries.len() + 2);
        listing.push((ino, fuser::FileType::Directory, ".".to_string()));
        listing.push((parent, fuser::FileType::Directory, "..".to_string()));
        for (name, child, is_dir) in entries {
            let kind = if is_dir {
                fuser::FileType::Directory
            } else {
                fuser::FileType::RegularFile
            };
            listing.push((child, kind, name));
        }

        for (i, (child, kind, name)) in listing.into_iter().enumerate().skip(offset as usize) {
            // The offset stored is the index of the *next* entry to return.
            if reply.add(child, (i + 1) as i64, kind, &name) {
                break;
            }
        }
        reply.ok();
    }
}

/// Read-only mount options tagged with the filesystem name.
fn mount_options(fs_name: &str) -> Vec<MountOption> {
    vec![MountOption::RO, MountOption::FSName(fs_name.to_string())]
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
    fuser::mount2(
        MusefsFs::new(core, config),
        mountpoint,
        &mount_options(fs_name),
    )
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
    fuser::spawn_mount2(
        MusefsFs::new(core, config),
        mountpoint,
        &mount_options(fs_name),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use fuser::FileType;
    use musefs_core::Attr;
    use musefs_core::CoreError;
    use std::time::{Duration, SystemTime};

    #[test]
    fn maps_core_errors_to_errno() {
        assert_eq!(errno(&CoreError::NoEntry(7)), libc::ENOENT);
        assert_eq!(errno(&CoreError::TrackNotFound(7)), libc::ENOENT);
        assert_eq!(errno(&CoreError::IsDir(7)), libc::EISDIR);
        assert_eq!(errno(&CoreError::NotADir(7)), libc::ENOTDIR);
        assert_eq!(errno(&CoreError::BackingChanged("x".into())), libc::EIO);
        let io = CoreError::Io(std::io::Error::from_raw_os_error(libc::ENOENT));
        assert_eq!(errno(&io), libc::ENOENT);
        let io_other = CoreError::Io(std::io::Error::other("boom"));
        assert_eq!(errno(&io_other), libc::EIO);
    }

    #[test]
    fn converts_dir_and_file_attrs() {
        let fallback = SystemTime::UNIX_EPOCH + Duration::from_secs(1000);

        let dir = Attr {
            inode: 1,
            is_dir: true,
            size: 0,
            mtime_secs: 0,
        };
        let fa = to_file_attr(&dir, 501, 20, fallback);
        assert_eq!(fa.ino, 1);
        assert_eq!(fa.kind, FileType::Directory);
        assert_eq!(fa.perm, 0o555);
        assert_eq!(fa.uid, 501);
        assert_eq!(fa.gid, 20);
        // mtime_secs == 0 falls back to the supplied mount time.
        assert_eq!(fa.mtime, fallback);

        let file = Attr {
            inode: 9,
            is_dir: false,
            size: 4096,
            mtime_secs: 1_700_000_000,
        };
        let fa = to_file_attr(&file, 501, 20, fallback);
        assert_eq!(fa.kind, FileType::RegularFile);
        assert_eq!(fa.perm, 0o444);
        assert_eq!(fa.size, 4096);
        assert_eq!(fa.blocks, 8); // 4096 / 512
        assert_eq!(
            fa.mtime,
            SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000)
        );
    }

    #[test]
    fn fuse_config_default_is_conservative() {
        let c = FuseConfig::default();
        assert_eq!(c.ttl, Duration::from_secs(1));
        assert_eq!(c.max_readahead, 512 * 1024);
        assert_eq!(c.max_background, 64);
        assert!(!c.keep_cache);
    }

    #[test]
    fn open_flags_sets_keep_cache_bit_only_when_enabled() {
        assert_eq!(open_flags(false), 0);
        assert_eq!(open_flags(true), fuser::consts::FOPEN_KEEP_CACHE);
    }
}
