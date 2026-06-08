//! Bench-only passthrough FUSE that mirrors a backing directory and sleeps a
//! configurable amount per operation, so HDD/NFS latency profiles are
//! reproducible on one machine. The corpus AND the SQLite DB live under the
//! mount, so backing reads and SQLite fsyncs are both delayed (and fsyncs
//! counted). Not for production; requires /dev/fuse.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io;
use std::os::unix::fs::{FileExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use fuser::{
    AccessFlags, BackgroundSession, BsdFileFlags, Config, FileAttr, FileHandle, FileType,
    FopenFlags, Generation, INodeNo, MountOption, OpenFlags, RenameFlags, ReplyAttr, ReplyCreate,
    ReplyData, ReplyDirectory, ReplyEmpty, ReplyEntry, ReplyOpen, ReplyStatfs, ReplyWrite, Request,
    Session, TimeOrNow, WriteFlags,
};

// latencyfs is 64-bit-only, like the rest of the workspace (musefs-db's
// convert module declares the same bound for the dependent crates).
const _: () = assert!(
    std::mem::size_of::<usize>() == 8,
    "musefs-latencyfs supports 64-bit targets only"
);

/// The crate's only sanctioned `u64 -> usize` cast (see the guard above).
#[expect(
    clippy::cast_possible_truncation,
    reason = "u64 -> usize is lossless on 64-bit targets; guarded by the const assert above"
)]
#[inline]
fn usize_from(v: u64) -> usize {
    v as usize
}

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
        let Some(i) = self.rev.remove(from) else {
            return;
        };
        // Renaming a directory must re-point its already-interned descendants
        // (`from/<suffix>` -> `to/<suffix>`); otherwise their inodes would be
        // stranded on stale paths and later inode->path lookups would fail.
        // `from` is already removed above, so no descendant matches it exactly.
        let descendants: Vec<(u64, PathBuf, PathBuf)> = self
            .rev
            .iter()
            .filter_map(|(path, &ino)| {
                path.strip_prefix(from)
                    .ok()
                    .map(|suffix| (ino, path.clone(), to.join(suffix)))
            })
            .collect();
        for (ino, old, new) in descendants {
            self.rev.remove(&old);
            if let Some(displaced) = self.rev.insert(new.clone(), ino) {
                self.fwd.remove(&displaced);
            }
            self.fwd.insert(ino, new);
        }
        self.fwd.insert(i, to.clone());
        // An overwrite-rename onto an already-interned path must drop the
        // displaced inode's stale `fwd` entry, or the bidirectional map
        // would report two inodes for one path.
        if let Some(displaced) = self.rev.insert(to, i) {
            self.fwd.remove(&displaced);
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
            SystemTime::UNIX_EPOCH
                + Duration::new(
                    u64::try_from(secs).unwrap_or(0),
                    u32::try_from(nsec).unwrap_or(0),
                )
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
        nlink: u32::try_from(m.nlink()).unwrap_or(u32::MAX),
        uid: m.uid(),
        gid: m.gid(),
        rdev: u32::try_from(m.rdev()).unwrap_or(u32::MAX),
        blksize: u32::try_from(m.blksize()).unwrap_or(u32::MAX),
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
    // Stored for future use (e.g. chown passthrough); attrs are read from disk
    // metadata via attr_from_meta, so these are not read by any current op.
    #[allow(dead_code)]
    uid: u32,
    #[allow(dead_code)]
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
            uid: rustix::process::getuid().as_raw(),
            gid: rustix::process::getgid().as_raw(),
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
        for (i, (cino, kind, name)) in entries.into_iter().enumerate().skip(usize_from(offset)) {
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
        let mut buf = vec![0u8; usize_from(u64::from(size))];
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
            if let Ok(s) = rustix::fs::statvfs(&p) {
                return reply.statfs(
                    s.f_blocks,
                    s.f_bfree,
                    s.f_bavail,
                    s.f_files,
                    s.f_ffree,
                    u32::try_from(s.f_bsize).unwrap_or(u32::MAX),
                    u32::try_from(s.f_namemax).unwrap_or(u32::MAX),
                    u32::try_from(s.f_frsize).unwrap_or(u32::MAX),
                );
            }
        }
        reply.statfs(0, 0, 0, 0, 0, 512, 255, 0);
    }

    fn access(&self, _req: &Request, _ino: INodeNo, _mask: AccessFlags, reply: ReplyEmpty) {
        reply.ok();
    }

    fn forget(&self, _req: &Request, _ino: INodeNo, _nlookup: u64) {}

    fn create(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &std::ffi::OsStr,
        _mode: u32,
        _umask: u32,
        _flags: i32,
        reply: ReplyCreate,
    ) {
        nap(self.lat.open);
        let Some(pp) = self.ipath(parent.0) else {
            return reply.error(fuser::Errno::ENOENT);
        };
        let child = pp.join(name);
        let file = match OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(true)
            .open(&child)
        {
            Ok(f) => f,
            Err(e) => {
                return reply.error(fuser::Errno::from_i32(
                    e.raw_os_error().unwrap_or(libc::EIO),
                ))
            }
        };
        let m = match file.metadata() {
            Ok(m) => m,
            Err(e) => {
                return reply.error(fuser::Errno::from_i32(
                    e.raw_os_error().unwrap_or(libc::EIO),
                ))
            }
        };
        let ino = self.inodes.lock().unwrap().intern(child);
        let fh = self.next_fh.fetch_add(1, Ordering::Relaxed);
        self.handles.lock().unwrap().insert(fh, file);
        reply.created(
            &TTL,
            &attr_from_meta(ino, &m),
            Generation(0),
            FileHandle(fh),
            FopenFlags::empty(),
        );
    }

    fn write(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        data: &[u8],
        _write_flags: WriteFlags,
        _flags: OpenFlags,
        _lock_owner: Option<fuser::LockOwner>,
        reply: ReplyWrite,
    ) {
        nap(self.lat.write);
        // The `handles` lock is held across `write_at` — safe under single-threaded
        // dispatch (see `PassthroughFs` invariant and the `read` op comment above).
        let guard = self.handles.lock().unwrap();
        let Some(file) = guard.get(&fh.0) else {
            return reply.error(fuser::Errno::EBADF);
        };
        match file.write_at(data, offset) {
            Ok(n) => reply.written(u32::try_from(n).unwrap_or(u32::MAX)),
            Err(e) => {
                reply.error(fuser::Errno::from_i32(
                    e.raw_os_error().unwrap_or(libc::EIO),
                ));
            }
        }
    }

    fn fsync(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        datasync: bool,
        reply: ReplyEmpty,
    ) {
        nap(self.lat.fsync);
        self.fsyncs.fetch_add(1, Ordering::Relaxed);
        let guard = self.handles.lock().unwrap();
        let Some(file) = guard.get(&fh.0) else {
            return reply.error(fuser::Errno::EBADF);
        };
        let r = if datasync {
            file.sync_data()
        } else {
            file.sync_all()
        };
        match r {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(fuser::Errno::from_i32(
                e.raw_os_error().unwrap_or(libc::EIO),
            )),
        }
    }

    fn fsyncdir(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _fh: FileHandle,
        _datasync: bool,
        reply: ReplyEmpty,
    ) {
        nap(self.lat.fsync);
        self.fsyncs.fetch_add(1, Ordering::Relaxed);
        reply.ok();
    }

    fn setattr(
        &self,
        _req: &Request,
        ino: INodeNo,
        _mode: Option<u32>,
        _uid: Option<u32>,
        _gid: Option<u32>,
        size: Option<u64>,
        _atime: Option<TimeOrNow>,
        _mtime: Option<TimeOrNow>,
        _ctime: Option<SystemTime>,
        _fh: Option<FileHandle>,
        _crtime: Option<SystemTime>,
        _chgtime: Option<SystemTime>,
        _bkuptime: Option<SystemTime>,
        _flags: Option<BsdFileFlags>,
        reply: ReplyAttr,
    ) {
        nap(self.lat.other);
        let Some(p) = self.ipath(ino.0) else {
            return reply.error(fuser::Errno::ENOENT);
        };
        // The only attr SQLite needs: truncate/extend the WAL. Propagate any
        // failure rather than replying ok with the stale (un-truncated) size,
        // which would lie to the kernel and desync the WAL on disk.
        if let Some(sz) = size {
            if let Err(e) = OpenOptions::new()
                .write(true)
                .open(&p)
                .and_then(|f| f.set_len(sz))
            {
                return reply.error(fuser::Errno::from_i32(
                    e.raw_os_error().unwrap_or(libc::EIO),
                ));
            }
        }
        match std::fs::symlink_metadata(&p) {
            Ok(m) => reply.attr(&TTL, &attr_from_meta(ino.0, &m)),
            Err(e) => reply.error(fuser::Errno::from_i32(
                e.raw_os_error().unwrap_or(libc::EIO),
            )),
        }
    }

    fn unlink(&self, _req: &Request, parent: INodeNo, name: &std::ffi::OsStr, reply: ReplyEmpty) {
        nap(self.lat.other);
        let Some(pp) = self.ipath(parent.0) else {
            return reply.error(fuser::Errno::ENOENT);
        };
        let child = pp.join(name);
        match std::fs::remove_file(&child) {
            Ok(()) => {
                self.inodes.lock().unwrap().forget_path(&child);
                reply.ok();
            }
            Err(e) => reply.error(fuser::Errno::from_i32(
                e.raw_os_error().unwrap_or(libc::EIO),
            )),
        }
    }

    fn rename(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &std::ffi::OsStr,
        newparent: INodeNo,
        newname: &std::ffi::OsStr,
        _flags: RenameFlags,
        reply: ReplyEmpty,
    ) {
        nap(self.lat.other);
        let (Some(pp), Some(np)) = (self.ipath(parent.0), self.ipath(newparent.0)) else {
            return reply.error(fuser::Errno::ENOENT);
        };
        let from = pp.join(name);
        let to = np.join(newname);
        match std::fs::rename(&from, &to) {
            Ok(()) => {
                self.inodes.lock().unwrap().rename(&from, to);
                reply.ok();
            }
            Err(e) => reply.error(fuser::Errno::from_i32(
                e.raw_os_error().unwrap_or(libc::EIO),
            )),
        }
    }

    fn mkdir(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &std::ffi::OsStr,
        _mode: u32,
        _umask: u32,
        reply: ReplyEntry,
    ) {
        nap(self.lat.other);
        let Some(pp) = self.ipath(parent.0) else {
            return reply.error(fuser::Errno::ENOENT);
        };
        let child = pp.join(name);
        match std::fs::create_dir(&child).and_then(|()| std::fs::symlink_metadata(&child)) {
            Ok(m) => {
                let ino = self.inodes.lock().unwrap().intern(child);
                reply.entry(&TTL, &attr_from_meta(ino, &m), Generation(0));
            }
            Err(e) => reply.error(fuser::Errno::from_i32(
                e.raw_os_error().unwrap_or(libc::EIO),
            )),
        }
    }

    fn rmdir(&self, _req: &Request, parent: INodeNo, name: &std::ffi::OsStr, reply: ReplyEmpty) {
        nap(self.lat.other);
        let Some(pp) = self.ipath(parent.0) else {
            return reply.error(fuser::Errno::ENOENT);
        };
        let child = pp.join(name);
        match std::fs::remove_dir(&child) {
            Ok(()) => {
                self.inodes.lock().unwrap().forget_path(&child);
                reply.ok();
            }
            Err(e) => reply.error(fuser::Errno::from_i32(
                e.raw_os_error().unwrap_or(libc::EIO),
            )),
        }
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn p(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    #[test]
    fn profile_ssd_and_unknown_are_zero() {
        for name in ["ssd", "", "garbage"] {
            let l = Latency::profile(name);
            assert_eq!(l.open, Duration::ZERO);
            assert_eq!(l.stat, Duration::ZERO);
            assert_eq!(l.read, Duration::ZERO);
            assert_eq!(l.write, Duration::ZERO);
            assert_eq!(l.fsync, Duration::ZERO);
            assert_eq!(l.other, Duration::ZERO);
        }
    }

    // Assert against literal `Duration`s rather than ms()/us(), so a mutation
    // of those helpers can't hide by also corrupting the expected value.
    fn millis(n: u64) -> Duration {
        Duration::from_millis(n)
    }
    fn micros(n: u64) -> Duration {
        Duration::from_micros(n)
    }

    #[test]
    fn profile_hdd_values() {
        let l = Latency::profile("hdd");
        assert_eq!(l.open, millis(8));
        assert_eq!(l.stat, millis(8));
        assert_eq!(l.read, millis(8));
        assert_eq!(l.write, millis(8));
        assert_eq!(l.fsync, millis(10));
        assert_eq!(l.other, millis(2));
    }

    #[test]
    fn profile_nfs_ssd_values() {
        let l = Latency::profile("nfs-ssd");
        assert_eq!(l.open, micros(600));
        assert_eq!(l.stat, micros(400));
        assert_eq!(l.read, micros(600));
        assert_eq!(l.write, micros(600));
        assert_eq!(l.fsync, micros(800));
        assert_eq!(l.other, micros(300));
    }

    #[test]
    fn profile_nfs_hdd_values() {
        let l = Latency::profile("nfs-hdd");
        assert_eq!(l.open, micros(8600));
        assert_eq!(l.stat, micros(8400));
        assert_eq!(l.read, micros(8600));
        assert_eq!(l.write, micros(8600));
        // 10ms + 800us; also pins the `+` in the profile table.
        assert_eq!(l.fsync, micros(10_800));
        assert_eq!(l.other, micros(2300));
    }

    #[test]
    fn nap_sleeps_for_nonzero_and_skips_zero() {
        use std::time::Instant;
        // A non-zero nap sleeps at least its duration (sleep never under-sleeps),
        // so this pins both the no-op mutation and the `!is_zero` guard direction.
        let t = Instant::now();
        nap(millis(30));
        assert!(t.elapsed() >= millis(25), "nonzero nap should sleep");
        // A zero nap returns promptly (no sleep).
        let t = Instant::now();
        nap(Duration::ZERO);
        assert!(t.elapsed() < millis(20), "zero nap should not sleep");
    }

    #[test]
    fn inodes_root_is_one_and_next_starts_at_two() {
        let m = Inodes::new(p("/root"));
        assert_eq!(m.path(1), Some(p("/root")));
        assert_eq!(m.next, 2);
        assert_eq!(m.path(2), None);
    }

    #[test]
    fn intern_is_idempotent_and_increments() {
        let mut m = Inodes::new(p("/root"));
        let a = m.intern(p("/root/a"));
        let b = m.intern(p("/root/b"));
        assert_eq!(a, 2);
        assert_eq!(b, 3);
        // Re-interning the same path returns the same inode, no increment.
        assert_eq!(m.intern(p("/root/a")), a);
        assert_eq!(m.next, 4);
        assert_eq!(m.path(a), Some(p("/root/a")));
    }

    #[test]
    fn forget_path_clears_both_directions() {
        let mut m = Inodes::new(p("/root"));
        let a = m.intern(p("/root/a"));
        m.forget_path(&p("/root/a"));
        assert_eq!(m.path(a), None);
        // A fresh intern of the same path gets a new inode (never recycled).
        let a2 = m.intern(p("/root/a"));
        assert_ne!(a2, a);
    }

    #[test]
    fn rename_moves_inode_and_keeps_map_consistent() {
        let mut m = Inodes::new(p("/root"));
        let a = m.intern(p("/root/a"));
        m.rename(&p("/root/a"), p("/root/b"));
        assert_eq!(m.path(a), Some(p("/root/b")));
        assert_eq!(m.intern(p("/root/b")), a);
        // The old path no longer resolves to the moved inode.
        assert_ne!(m.intern(p("/root/a")), a);
    }

    #[test]
    fn rename_onto_existing_path_drops_displaced_inode() {
        let mut m = Inodes::new(p("/root"));
        let a = m.intern(p("/root/a"));
        let b = m.intern(p("/root/b"));
        // Overwrite-rename a -> b: b's path is now owned by inode `a`, and the
        // displaced inode `b` must have no stale forward entry.
        m.rename(&p("/root/a"), p("/root/b"));
        assert_eq!(m.intern(p("/root/b")), a);
        assert_eq!(m.path(b), None);
    }

    #[test]
    fn rename_directory_repoints_interned_descendants() {
        let mut m = Inodes::new(p("/root"));
        let dir = m.intern(p("/root/d"));
        let child = m.intern(p("/root/d/c"));
        let grandchild = m.intern(p("/root/d/sub/g"));
        m.rename(&p("/root/d"), p("/root/e"));
        // The directory and every interned descendant follow the new prefix,
        // keeping their inodes (a held descriptor stays valid).
        assert_eq!(m.path(dir), Some(p("/root/e")));
        assert_eq!(m.path(child), Some(p("/root/e/c")));
        assert_eq!(m.path(grandchild), Some(p("/root/e/sub/g")));
        // The stale paths no longer resolve to the moved inodes.
        assert_ne!(m.intern(p("/root/d/c")), child);
    }

    #[test]
    fn attr_from_meta_maps_file_and_dir_kinds() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("f"), b"hello").unwrap();
        let fmeta = std::fs::symlink_metadata(dir.path().join("f")).unwrap();
        let fa = attr_from_meta(42, &fmeta);
        assert_eq!(fa.ino, INodeNo(42));
        assert_eq!(fa.size, 5);
        assert_eq!(fa.kind, FileType::RegularFile);
        // perm carries only the 12 permission bits, never the file-type bits.
        assert_eq!(fa.perm & !0o7777, 0);
        // mtime is reconstructed from the raw secs/nsecs as UNIX_EPOCH + delta;
        // it must equal what std read from the same inode (pins the epoch math).
        assert_eq!(fa.mtime, fmeta.modified().unwrap());

        let dmeta = std::fs::symlink_metadata(dir.path()).unwrap();
        let da = attr_from_meta(1, &dmeta);
        assert_eq!(da.kind, FileType::Directory);
        assert_eq!(da.ino, INodeNo(1));
    }
}
