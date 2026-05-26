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
    ReplyDirectory, ReplyEntry, Request,
};
use musefs_core::Attr;
use musefs_core::CoreError;
use musefs_core::Musefs;

const TTL: Duration = Duration::from_secs(1);

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
}

impl MusefsFs {
    pub fn new(core: Musefs) -> MusefsFs {
        // Work is I/O-bound (especially on NFS), so oversize the pool vs CPUs.
        let workers = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4)
            * 2;
        MusefsFs {
            core: Arc::new(core),
            // NOTE: ThreadPool's job queue is unbounded; under a read storm against
            // slow backing storage, queued jobs (each holding a reply handle) can
            // accumulate. A bounded queue / back-pressure is a future tuning item.
            pool: ThreadPool::new(workers),
            // SAFETY: getuid/getgid are always-successful libc calls.
            uid: unsafe { libc::getuid() },
            gid: unsafe { libc::getgid() },
            mount_time: SystemTime::now(),
        }
    }
}

impl Filesystem for MusefsFs {
    fn lookup(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEntry) {
        let _ = self.core.poll_refresh();
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
        let (uid, gid, mt) = (self.uid, self.gid, self.mount_time);
        self.pool.execute(move || match core.getattr(child) {
            Ok(attr) => reply.entry(&TTL, &to_file_attr(&attr, uid, gid, mt), 0),
            Err(e) => reply.error(errno(&e)),
        });
    }

    fn getattr(&mut self, _req: &Request<'_>, ino: u64, reply: ReplyAttr) {
        let _ = self.core.poll_refresh();
        let core = Arc::clone(&self.core);
        let (uid, gid, mt) = (self.uid, self.gid, self.mount_time);
        self.pool.execute(move || match core.getattr(ino) {
            Ok(attr) => reply.attr(&TTL, &to_file_attr(&attr, uid, gid, mt)),
            Err(e) => reply.error(errno(&e)),
        });
    }

    fn read(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        _fh: u64,
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
        self.pool
            .execute(move || match core.read(ino, 0, offset as u64, size as u64) {
                Ok(bytes) => reply.data(&bytes),
                Err(e) => reply.error(errno(&e)),
            });
    }

    fn readdir(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        _fh: u64,
        offset: i64,
        mut reply: ReplyDirectory,
    ) {
        let _ = self.core.poll_refresh();
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

/// Mount `core` at `mountpoint` and block until the filesystem is unmounted.
pub fn mount(core: Musefs, mountpoint: &Path, fs_name: &str) -> std::io::Result<()> {
    fuser::mount2(MusefsFs::new(core), mountpoint, &mount_options(fs_name))
}

/// Mount `core` in a background session, returning a handle whose `Drop`
/// unmounts. Used for tests and embedding.
pub fn spawn(core: Musefs, mountpoint: &Path, fs_name: &str) -> std::io::Result<BackgroundSession> {
    fuser::spawn_mount2(MusefsFs::new(core), mountpoint, &mount_options(fs_name))
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
}
