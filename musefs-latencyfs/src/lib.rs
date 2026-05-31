//! Bench-only passthrough FUSE that mirrors a backing directory and sleeps a
//! configurable amount per operation, so HDD/NFS latency profiles are
//! reproducible on one machine. The corpus AND the SQLite DB live under the
//! mount, so backing reads and SQLite fsyncs are both delayed (and fsyncs
//! counted). Not for production; requires /dev/fuse.

// Task 1 skeleton: all items below are consumed by the Filesystem impl in Task 2.
// Remove this attribute once Task 2 lands.
#![allow(dead_code, unused_imports)]

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io;
use std::os::unix::fs::{FileExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use fuser::{FileAttr, FileType, Generation, INodeNo};

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
