//! Bench-only passthrough FUSE that mirrors a backing directory and sleeps a
//! configurable amount per operation, so HDD/NFS latency profiles are
//! reproducible on one machine. The corpus AND the SQLite DB live under the
//! mount, so backing reads and SQLite fsyncs are both delayed (and fsyncs
//! counted). Not for production; requires /dev/fuse.

// Task 3 will add write ops (create/write/fsync/fsyncdir/setattr/unlink/rename/mkdir/rmdir)
// that consume forget_path, rename, lat.write, lat.fsync, uid, and gid.
#![allow(dead_code)]

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io;
use std::os::unix::fs::{FileExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use fuser::{
    AccessFlags, BackgroundSession, Config, FileAttr, FileHandle, FileType, FopenFlags, Generation,
    INodeNo, MountOption, OpenFlags, ReplyAttr, ReplyData, ReplyDirectory, ReplyEmpty, ReplyEntry,
    ReplyOpen, ReplyStatfs, Request, Session,
};

const TTL: Duration = Duration::from_secs(1);

fn ms(n: u64) -> Duration {
    Duration::from_millis(n)
}
fn us(n: u64) -> Duration {
    Duration::from_micros(n)
}
fn nap(d: Duration) {
    if !d.is_zero() {
        std::thread::sleep(d);
    }
}

/// Per-operation injected latency. `ssd` is all-zero (≈ no injection).
#[derive(Clone, Copy, Default)]
pub struct Latency {
    pub open: Duration,
    pub stat: Duration,
    pub read: Duration,
    pub write: Duration,
    pub fsync: Duration,
    pub other: Duration,
}

impl Latency {
    /// Named profiles. Unknown / "ssd" => zero.
    pub fn profile(name: &str) -> Latency {
        match name {
            "hdd" => Latency {
                open: ms(8),
                stat: ms(8),
                read: ms(8),
                write: ms(8),
                fsync: ms(10),
                other: ms(2),
            },
            "nfs-ssd" => Latency {
                open: us(600),
                stat: us(400),
                read: us(600),
                write: us(600),
                fsync: us(800),
                other: us(300),
            },
            "nfs-hdd" => Latency {
                open: us(8600),
                stat: us(8400),
                read: us(8600),
                write: us(8600),
                fsync: ms(10) + us(800),
                other: us(2300),
            },
            _ => Latency::default(),
        }
    }
}

/// Bidirectional inode<->path map. Never forgets (bench-scale memory is fine).
struct Inodes {
    fwd: HashMap<u64, PathBuf>,
    rev: HashMap<PathBuf, u64>,
    next: u64,
}

impl Inodes {
    fn new(root: PathBuf) -> Inodes {
        let mut fwd = HashMap::new();
        let mut rev = HashMap::new();
        fwd.insert(1, root.clone());
        rev.insert(root, 1);
        Inodes { fwd, rev, next: 2 }
    }
    fn path(&self, ino: u64) -> Option<PathBuf> {
        self.fwd.get(&ino).cloned()
    }
    fn intern(&mut self, path: PathBuf) -> u64 {
        if let Some(&i) = self.rev.get(&path) {
            return i;
        }
        let i = self.next;
        self.next += 1;
        self.fwd.insert(i, path.clone());
        self.rev.insert(path, i);
        i
    }
    fn forget_path(&mut self, path: &Path) {
        if let Some(i) = self.rev.remove(path) {
            self.fwd.remove(&i);
        }
    }
    fn rename(&mut self, from: &Path, to: PathBuf) {
        if let Some(i) = self.rev.remove(from) {
            self.fwd.insert(i, to.clone());
            // An overwrite-rename onto an already-interned path must drop the
            // displaced inode's stale `fwd` entry, or the bidirectional map
            // would report two inodes for one path.
            if let Some(displaced) = self.rev.insert(to, i) {
                self.fwd.remove(&displaced);
            }
        }
    }
}

/// `std::fs::Metadata` -> `fuser::FileAttr`, reporting our interned `ino`.
fn attr_from_meta(ino: u64, m: &std::fs::Metadata) -> FileAttr {
    let kind = if m.is_dir() {
        FileType::Directory
    } else if m.file_type().is_symlink() {
        FileType::Symlink
    } else {
        FileType::RegularFile
    };
    let t = |secs: i64, nsec: i64| {
        if secs >= 0 {
            SystemTime::UNIX_EPOCH + Duration::new(secs as u64, nsec as u32)
        } else {
            SystemTime::UNIX_EPOCH
        }
    };
    FileAttr {
        ino: INodeNo(ino),
        size: m.size(),
        blocks: m.blocks(),
        atime: t(m.atime(), m.atime_nsec()),
        mtime: t(m.mtime(), m.mtime_nsec()),
        ctime: t(m.ctime(), m.ctime_nsec()),
        crtime: SystemTime::UNIX_EPOCH,
        kind,
        perm: (m.mode() & 0o7777) as u16,
        nlink: m.nlink() as u32,
        uid: m.uid(),
        gid: m.gid(),
        rdev: m.rdev() as u32,
        blksize: m.blksize() as u32,
        flags: 0,
    }
}

/// The passthrough filesystem. Root inode (1) maps to the backing root.
///
/// Invariant: mounted with single-threaded FUSE dispatch (`Config::default()`,
/// no `n_threads`), so filesystem ops never run concurrently. The `handles` and
/// `inodes` locks therefore never contend, and ops may hold a lock across a
/// blocking syscall. A multi-threaded bench would need `Arc<File>` handles.
pub struct PassthroughFs {
    inodes: Mutex<Inodes>,
    handles: Mutex<HashMap<u64, File>>,
    next_fh: AtomicU64,
    lat: Latency,
    fsyncs: Arc<AtomicU64>,
    uid: u32,
    gid: u32,
}

impl PassthroughFs {
    fn new(root: PathBuf, lat: Latency, fsyncs: Arc<AtomicU64>) -> PassthroughFs {
        PassthroughFs {
            inodes: Mutex::new(Inodes::new(root)),
            handles: Mutex::new(HashMap::new()),
            next_fh: AtomicU64::new(1),
            lat,
            fsyncs,
            // SAFETY: getuid/getgid never fail.
            uid: unsafe { libc::getuid() },
            gid: unsafe { libc::getgid() },
        }
    }
    fn ipath(&self, ino: u64) -> Option<PathBuf> {
        self.inodes.lock().unwrap().path(ino)
    }
}

impl fuser::Filesystem for PassthroughFs {
    fn lookup(&self, _req: &Request, parent: INodeNo, name: &std::ffi::OsStr, reply: ReplyEntry) {
        nap(self.lat.stat);
        let Some(pp) = self.ipath(parent.0) else {
            return reply.error(fuser::Errno::ENOENT);
        };
        let child = pp.join(name);
        match std::fs::symlink_metadata(&child) {
            Ok(m) => {
                let ino = self.inodes.lock().unwrap().intern(child);
                reply.entry(&TTL, &attr_from_meta(ino, &m), Generation(0));
            }
            Err(_) => reply.error(fuser::Errno::ENOENT),
        }
    }

    fn getattr(&self, _req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
        nap(self.lat.stat);
        let Some(p) = self.ipath(ino.0) else {
            return reply.error(fuser::Errno::ENOENT);
        };
        match std::fs::symlink_metadata(&p) {
            Ok(m) => reply.attr(&TTL, &attr_from_meta(ino.0, &m)),
            Err(_) => reply.error(fuser::Errno::ENOENT),
        }
    }

    fn opendir(&self, _req: &Request, _ino: INodeNo, _flags: OpenFlags, reply: ReplyOpen) {
        nap(self.lat.open);
        reply.opened(FileHandle(0), FopenFlags::empty());
    }

    fn readdir(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        mut reply: ReplyDirectory,
    ) {
        nap(self.lat.stat);
        let Some(dir) = self.ipath(ino.0) else {
            return reply.error(fuser::Errno::ENOENT);
        };
        // Root's `..` is self-referential (conventional FUSE root), so we never
        // intern a path outside the mounted backing tree.
        let parent_ino = if ino.0 == 1 {
            ino.0
        } else {
            dir.parent().map_or(ino.0, |p| {
                self.inodes.lock().unwrap().intern(p.to_path_buf())
            })
        };
        let mut entries: Vec<(u64, FileType, std::ffi::OsString)> = vec![
            (ino.0, FileType::Directory, ".".into()),
            (parent_ino, FileType::Directory, "..".into()),
        ];
        let rd = match std::fs::read_dir(&dir) {
            Ok(rd) => rd,
            Err(e) => {
                return reply.error(fuser::Errno::from_i32(
                    e.raw_os_error().unwrap_or(libc::EIO),
                ))
            }
        };
        for ent in rd.flatten() {
            let p = ent.path();
            let kind = match ent.file_type() {
                Ok(ft) if ft.is_dir() => FileType::Directory,
                Ok(ft) if ft.is_symlink() => FileType::Symlink,
                _ => FileType::RegularFile,
            };
            let cino = self.inodes.lock().unwrap().intern(p);
            entries.push((cino, kind, ent.file_name()));
        }
        for (i, (cino, kind, name)) in entries.into_iter().enumerate().skip(offset as usize) {
            if reply.add(INodeNo(cino), (i + 1) as u64, kind, &name) {
                break;
            }
        }
        reply.ok();
    }

    fn releasedir(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _fh: FileHandle,
        _flags: OpenFlags,
        reply: ReplyEmpty,
    ) {
        reply.ok();
    }

    fn open(&self, _req: &Request, ino: INodeNo, _flags: OpenFlags, reply: ReplyOpen) {
        nap(self.lat.open);
        let Some(p) = self.ipath(ino.0) else {
            return reply.error(fuser::Errno::ENOENT);
        };
        // Try read-write (the DB); fall back to read-only (audio files are 0444).
        let file = match OpenOptions::new().read(true).write(true).open(&p) {
            Ok(f) => f,
            Err(_) => match File::open(&p) {
                Ok(f) => f,
                Err(e) => {
                    return reply.error(fuser::Errno::from_i32(
                        e.raw_os_error().unwrap_or(libc::EIO),
                    ))
                }
            },
        };
        let fh = self.next_fh.fetch_add(1, Ordering::Relaxed);
        self.handles.lock().unwrap().insert(fh, file);
        reply.opened(FileHandle(fh), FopenFlags::empty());
    }

    fn read(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        size: u32,
        _flags: OpenFlags,
        _lock_owner: Option<fuser::LockOwner>,
        reply: ReplyData,
    ) {
        nap(self.lat.read);
        // The `handles` lock is held across `read_at` (and `write_at` in the
        // write ops). This is fine because the mount uses `Config::default()`,
        // i.e. single-threaded FUSE dispatch (see `PassthroughFs` invariant), so
        // ops never run concurrently. If a future bench sets `n_threads`, switch
        // the handle values to `Arc<File>` and drop the guard before the syscall.
        let guard = self.handles.lock().unwrap();
        let Some(file) = guard.get(&fh.0) else {
            return reply.error(fuser::Errno::EBADF);
        };
        let mut buf = vec![0u8; size as usize];
        match file.read_at(&mut buf, offset) {
            Ok(n) => reply.data(&buf[..n]),
            Err(e) => {
                reply.error(fuser::Errno::from_i32(
                    e.raw_os_error().unwrap_or(libc::EIO),
                ));
            }
        }
    }

    fn flush(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _fh: FileHandle,
        _lock_owner: fuser::LockOwner,
        reply: ReplyEmpty,
    ) {
        nap(self.lat.other);
        reply.ok();
    }

    fn release(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        _flags: OpenFlags,
        _lock_owner: Option<fuser::LockOwner>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        self.handles.lock().unwrap().remove(&fh.0);
        reply.ok();
    }

    fn statfs(&self, _req: &Request, ino: INodeNo, reply: ReplyStatfs) {
        nap(self.lat.stat);
        // Pass through real statvfs of the inode's path; fall back to benign values.
        if let Some(p) = self.ipath(ino.0) {
            // Use the raw OS path bytes (not `to_string_lossy`, which would
            // mangle non-UTF-8 names into U+FFFD and statvfs a different path).
            if let Ok(cstr) =
                std::ffi::CString::new(std::os::unix::ffi::OsStrExt::as_bytes(p.as_os_str()))
            {
                let mut s: libc::statvfs = unsafe { std::mem::zeroed() };
                // SAFETY: cstr is a valid NUL-terminated path; s is a valid out-param.
                if unsafe { libc::statvfs(cstr.as_ptr(), &raw mut s) } == 0 {
                    return reply.statfs(
                        s.f_blocks as u64,
                        s.f_bfree as u64,
                        s.f_bavail as u64,
                        s.f_files as u64,
                        s.f_ffree as u64,
                        s.f_bsize as u32,
                        s.f_namemax as u32,
                        s.f_frsize as u32,
                    );
                }
            }
        }
        reply.statfs(0, 0, 0, 0, 0, 512, 255, 0);
    }

    fn access(&self, _req: &Request, _ino: INodeNo, _mask: AccessFlags, reply: ReplyEmpty) {
        reply.ok();
    }

    fn forget(&self, _req: &Request, _ino: INodeNo, _nlookup: u64) {}
}

/// Serializes the racy fusermount3 mount handshake. `Session::new` forks/execs
/// `fusermount3` and passes the `/dev/fuse` fd back over a socket; fork and the
/// fd table are process-global, so two mounts establishing concurrently from one
/// process race the fd table ("file descriptor N is not a socket, can't send fuse
/// fd"). `cargo test -- --ignored` runs a binary's tests in parallel and several
/// test files mount more than once, so guard setup. Mirrors `musefs-fuse`'s
/// `MOUNT_SETUP`. The lock covers only establishment, never the session lifetime,
/// so it never serializes filesystem operations.
static MOUNT_SETUP: Mutex<()> = Mutex::new(());

/// A mounted passthrough FS. Unmounts on drop. The corpus + DB live under
/// `path()`; point scans and `Db::open` there to measure under injected latency.
pub struct LatencyMount {
    fsyncs: Arc<AtomicU64>,
    // Drop order: the session (unmount) must drop before the mountpoint tempdir.
    _bg: BackgroundSession,
    mountdir: tempfile::TempDir,
}

impl LatencyMount {
    /// Mount a passthrough over `backing` using the named latency profile
    /// (`ssd`|`hdd`|`nfs-ssd`|`nfs-hdd`). Returns once the mount is live.
    pub fn new(backing: &Path, profile: &str) -> io::Result<LatencyMount> {
        let mountdir = tempfile::tempdir()?;
        let fsyncs = Arc::new(AtomicU64::new(0));
        let fs = PassthroughFs::new(
            backing.to_path_buf(),
            Latency::profile(profile),
            Arc::clone(&fsyncs),
        );
        let mut cfg = Config::default();
        cfg.mount_options = vec![MountOption::FSName("musefs-latencyfs".to_string())];
        let session = {
            // Recover from a poisoned lock: it guards only ordering, so a prior
            // panic during a mount leaves no inconsistent state to protect.
            let _guard = MOUNT_SETUP
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            Session::new(fs, mountdir.path(), &cfg)?
        };
        let bg = session.spawn()?;
        Ok(LatencyMount {
            fsyncs,
            _bg: bg,
            mountdir,
        })
    }

    /// The mountpoint. Use this as the corpus dir / DB parent.
    pub fn path(&self) -> PathBuf {
        self.mountdir.path().to_path_buf()
    }

    /// Total `fsync`/`fsyncdir` operations observed since mount.
    pub fn fsyncs(&self) -> u64 {
        self.fsyncs.load(Ordering::Relaxed)
    }
}
