# SP0b — Passthrough latency-injection FUSE — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A bench-only passthrough FUSE filesystem that mirrors a real backing directory but sleeps a configurable amount per operation, so HDD/NFS latency profiles are reproducible on one machine and SQLite's own fsyncs (not just backing reads) are delayed and counted.

**Architecture:** A new dev-only workspace crate `musefs-latencyfs` implements `fuser::Filesystem` as a stateless-path passthrough over a root directory, with a per-op latency table and an fsync counter. A `LatencyMount` RAII helper mounts it (read-write — the DB lives under it) and unmounts on drop, mirroring `musefs-fuse`'s `Session`/`BackgroundSession` pattern. SP0a's scan/read timing benches gain a variant that mounts this FS over the corpus dir when `MUSEFS_BENCH_LATENCY_PROFILE` is set. Needs `/dev/fuse`; every test/bench using it is `#[ignore]`d like the existing e2e suite.

**Tech Stack:** Rust, `fuser` 0.17 (typed API: `INodeNo`, `FileHandle`, `Errno`, `ReplyEntry`/`Attr`/`Data`/`Open`/`Write`/`Create`/`Empty`/`Directory`/`Statfs`), `libc`, `std::os::unix::fs::{FileExt, MetadataExt}`, `rusqlite` (WAL smoke test), `tempfile`.

**Prerequisite:** SP0a is implemented (the corpus generator + `bench_ingest` live in `musefs-core/tests/common/` and `musefs-core/tests/`). Task 6 extends those.

**Spec:** `docs/superpowers/specs/2026-05-30-optimization-pass/SP0-measurement-foundation.md` (Component 3 + the deferred acceptance criteria).

**Verified fuser 0.17 facts (against the vendored source):**
- `Filesystem` methods take `&self`. Reply objects are `Send`. `Session::new(fs, &Path, &Config) -> io::Result<Session>`; `Session::spawn(self) -> io::Result<BackgroundSession>`; `BackgroundSession::Drop` unmounts. `Config { mount_options: Vec<MountOption>, .. }` (default is read-write).
- `ReplyEntry::entry(&Duration, &FileAttr, Generation)`, `ReplyAttr::attr(&Duration, &FileAttr)`, `ReplyOpen::opened(FileHandle, FopenFlags)`, `ReplyCreate::created(&Duration, &FileAttr, Generation, FileHandle, FopenFlags)`, `ReplyWrite::written(u32)`, `ReplyData::data(&[u8])`, `ReplyEmpty::ok()`, `ReplyStatfs::statfs(blocks,bfree,bavail,files,ffree,bsize,namelen,frsize)`, `ReplyDirectory::add(INodeNo, offset:u64, FileType, name) -> bool` then `.ok()`. All replies have `.error(Errno)`.
- `FileAttr { ino: INodeNo, size:u64, blocks:u64, atime/mtime/ctime/crtime: SystemTime, kind: FileType, perm:u16, nlink:u32, uid:u32, gid:u32, rdev:u32, blksize:u32, flags:u32 }`.

---

## File structure

- Create `musefs-latencyfs/Cargo.toml` — dev-only workspace member (`publish = false`).
- Create `musefs-latencyfs/src/lib.rs` — `Latency`/profiles, `PassthroughFs`, `LatencyMount`, the `metadata → FileAttr` helper.
- Create `musefs-latencyfs/tests/passthrough.rs` — `#[ignore]`d functional tests (stat / read / write+fsync / rename / unlink).
- Create `musefs-latencyfs/tests/sqlite_wal.rs` — `#[ignore]`d gating smoke test: a full SQLite WAL cycle through the mount.
- Create `musefs-latencyfs/tests/latency_effect.rs` — `#[ignore]`d test: a non-zero profile measurably slows I/O and bumps the fsync counter.
- Modify `Cargo.toml` (workspace `members`) — add `musefs-latencyfs`.
- Modify `musefs-core/Cargo.toml` — add `musefs-latencyfs` as a dev-dependency.
- Modify `musefs-core/tests/bench_ingest.rs` — mount the latency FS when `MUSEFS_BENCH_LATENCY_PROFILE` is set; report fsyncs.
- Modify `docs/superpowers/specs/2026-05-30-optimization-pass/README.md` — status + run instructions.

---

## Task 1: Crate skeleton, latency table, inode/handle tables, and metadata→FileAttr

**Files:**
- Modify: `Cargo.toml` (root)
- Create: `musefs-latencyfs/Cargo.toml`
- Create: `musefs-latencyfs/src/lib.rs`

- [ ] **Step 1: Add the workspace member**

In root `Cargo.toml`, change the members line to:

```toml
members = ["musefs-db", "musefs-format", "musefs-core", "musefs-fuse", "musefs-cli", "musefs", "musefs-latencyfs"]
```

- [ ] **Step 2: Create the crate manifest**

Create `musefs-latencyfs/Cargo.toml`:

```toml
[package]
name = "musefs-latencyfs"
description = "Bench-only passthrough FUSE with per-op latency injection for musefs benchmarks."
version.workspace = true
edition.workspace = true
license.workspace = true
repository.workspace = true
publish = false

[dependencies]
fuser = "0.17"
libc = "0.2"

[dev-dependencies]
tempfile = "3"
rusqlite = { version = "0.40", features = ["bundled"] }

[lints]
workspace = true
```

- [ ] **Step 3: Write the latency table + tables + attr helper (no FS impl yet)**

Create `musefs-latencyfs/src/lib.rs`:

```rust
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
            self.rev.insert(to, i);
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
```

- [ ] **Step 4: Verify it compiles**

Run: `cargo build -p musefs-latencyfs`
Expected: builds clean (unused-field warnings are acceptable at this step; the FS impl in Task 2 uses them).

- [ ] **Step 5: Commit**

```bash
git add Cargo.toml musefs-latencyfs/Cargo.toml musefs-latencyfs/src/lib.rs
git commit -m "feat(latencyfs): crate skeleton, latency profiles, inode/handle tables"
```

---

## Task 2: Metadata + read ops (`lookup`/`getattr`/`opendir`/`readdir`/`releasedir`/`open`/`read`/`flush`/`release`/`statfs`/`access`/`forget`)

This is enough to `ls` and read files through the mount. The mount harness is added here too so the test can exercise it.

**Files:**
- Modify: `musefs-latencyfs/src/lib.rs`
- Create: `musefs-latencyfs/tests/passthrough.rs`

- [ ] **Step 1: Write the failing test**

Create `musefs-latencyfs/tests/passthrough.rs`:

```rust
use std::io::Read;

use musefs_latencyfs::LatencyMount;

#[test]
#[ignore = "requires /dev/fuse; run with --ignored"]
fn reads_a_file_through_the_mount() {
    let backing = tempfile::tempdir().unwrap();
    std::fs::create_dir(backing.path().join("sub")).unwrap();
    std::fs::write(backing.path().join("sub/hello.txt"), b"hello world").unwrap();

    let mount = LatencyMount::new(backing.path(), "ssd").unwrap();
    // Stat + read through the FUSE mount.
    let mp = mount.path();
    let meta = std::fs::metadata(mp.join("sub/hello.txt")).unwrap();
    assert_eq!(meta.len(), 11);
    let mut s = String::new();
    std::fs::File::open(mp.join("sub/hello.txt"))
        .unwrap()
        .read_to_string(&mut s)
        .unwrap();
    assert_eq!(s, "hello world");

    // readdir sees the entry.
    let names: Vec<String> = std::fs::read_dir(mp.join("sub"))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert!(names.contains(&"hello.txt".to_string()));
}
```

- [ ] **Step 2: Run it (fails to compile — no `LatencyMount`)**

Run: `cargo test -p musefs-latencyfs --test passthrough -- --ignored --nocapture`
Expected: FAIL — `cannot find … LatencyMount`.

- [ ] **Step 3: Implement the read-side ops + mount harness**

Append to `musefs-latencyfs/src/lib.rs`:

```rust
use fuser::{
    BackgroundSession, Config, FopenFlags, MountOption, OpenFlags, Request, ReplyAttr, ReplyData,
    ReplyDirectory, ReplyEmpty, ReplyEntry, ReplyOpen, ReplyStatfs, Session,
};
use std::ffi::OsStr;

impl fuser::Filesystem for PassthroughFs {
    fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
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

    fn getattr(&self, _req: &Request, ino: INodeNo, _fh: Option<fuser::FileHandle>, reply: ReplyAttr) {
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
        reply.opened(fuser::FileHandle(0), FopenFlags::empty());
    }

    fn readdir(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: fuser::FileHandle,
        offset: u64,
        mut reply: ReplyDirectory,
    ) {
        nap(self.lat.stat);
        let Some(dir) = self.ipath(ino.0) else {
            return reply.error(fuser::Errno::ENOENT);
        };
        let parent_ino = dir
            .parent()
            .map(|p| self.inodes.lock().unwrap().intern(p.to_path_buf()))
            .unwrap_or(ino.0);
        let mut entries: Vec<(u64, FileType, std::ffi::OsString)> = vec![
            (ino.0, FileType::Directory, ".".into()),
            (parent_ino, FileType::Directory, "..".into()),
        ];
        let rd = match std::fs::read_dir(&dir) {
            Ok(rd) => rd,
            Err(e) => return reply.error(fuser::Errno::from_i32(e.raw_os_error().unwrap_or(libc::EIO))),
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
        _fh: fuser::FileHandle,
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
                    return reply.error(fuser::Errno::from_i32(e.raw_os_error().unwrap_or(libc::EIO)))
                }
            },
        };
        let fh = self.next_fh.fetch_add(1, Ordering::Relaxed);
        self.handles.lock().unwrap().insert(fh, file);
        reply.opened(fuser::FileHandle(fh), FopenFlags::empty());
    }

    fn read(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: fuser::FileHandle,
        offset: u64,
        size: u32,
        _flags: OpenFlags,
        _lock_owner: Option<fuser::LockOwner>,
        reply: ReplyData,
    ) {
        nap(self.lat.read);
        let guard = self.handles.lock().unwrap();
        let Some(file) = guard.get(&fh.0) else {
            return reply.error(fuser::Errno::EBADF);
        };
        let mut buf = vec![0u8; size as usize];
        match file.read_at(&mut buf, offset) {
            Ok(n) => reply.data(&buf[..n]),
            Err(e) => reply.error(fuser::Errno::from_i32(e.raw_os_error().unwrap_or(libc::EIO))),
        }
    }

    fn flush(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _fh: fuser::FileHandle,
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
        fh: fuser::FileHandle,
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
            if let Ok(cstr) = std::ffi::CString::new(p.as_os_str().to_string_lossy().as_bytes()) {
                let mut s: libc::statvfs = unsafe { std::mem::zeroed() };
                // SAFETY: cstr is a valid NUL-terminated path; s is a valid out-param.
                if unsafe { libc::statvfs(cstr.as_ptr(), &mut s) } == 0 {
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

    fn access(&self, _req: &Request, _ino: INodeNo, _mask: fuser::AccessFlags, reply: ReplyEmpty) {
        reply.ok();
    }

    fn forget(&self, _req: &Request, _ino: INodeNo, _nlookup: u64) {}
}

/// A mounted passthrough FS. Unmounts on drop. The corpus + DB live under
/// `path()`; point scans and `Db::open` there to measure under injected latency.
pub struct LatencyMount {
    fsyncs: Arc<AtomicU64>,
    // Drop order: the session (unmount) must drop before the mountpoint tempdir.
    _bg: BackgroundSession,
    _mountdir: tempfile::TempDir,
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
        let session = Session::new(fs, mountdir.path(), &cfg)?;
        let bg = session.spawn()?;
        Ok(LatencyMount {
            fsyncs,
            _bg: bg,
            _mountdir: mountdir,
        })
    }

    /// The mountpoint. Use this as the corpus dir / DB parent.
    pub fn path(&self) -> PathBuf {
        self._mountdir.path().to_path_buf()
    }

    /// Total `fsync`/`fsyncdir` operations observed since mount.
    pub fn fsyncs(&self) -> u64 {
        self.fsyncs.load(Ordering::Relaxed)
    }
}
```

- [ ] **Step 4: Run the test**

Run: `cargo test -p musefs-latencyfs --test passthrough reads_a_file -- --ignored --nocapture`
Expected: PASS (stat, read, and readdir succeed through the mount).

- [ ] **Step 5: Commit**

```bash
git add musefs-latencyfs/src/lib.rs musefs-latencyfs/tests/passthrough.rs
git commit -m "feat(latencyfs): metadata + read ops and the LatencyMount harness"
```

---

## Task 3: Write ops (`create`/`write`/`fsync`/`fsyncdir`/`setattr`/`unlink`/`rename`/`mkdir`/`rmdir`)

These let SQLite create and grow its DB/WAL/SHM files under the mount.

**Files:**
- Modify: `musefs-latencyfs/src/lib.rs`
- Modify: `musefs-latencyfs/tests/passthrough.rs`

- [ ] **Step 1: Write the failing test**

Append to `musefs-latencyfs/tests/passthrough.rs`:

```rust
#[test]
#[ignore = "requires /dev/fuse; run with --ignored"]
fn write_fsync_rename_unlink_through_the_mount() {
    let backing = tempfile::tempdir().unwrap();
    let mount = LatencyMount::new(backing.path(), "ssd").unwrap();
    let mp = mount.path();

    // create + write + fsync via normal file ops (the kernel issues create/write/fsync).
    let p = mp.join("data.bin");
    {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&p)
            .unwrap();
        f.write_all(b"abcdef").unwrap();
        f.sync_all().unwrap();
    }
    assert_eq!(std::fs::read(&p).unwrap(), b"abcdef");
    assert!(mount.fsyncs() >= 1, "fsync should have been counted");

    // rename + unlink.
    let q = mp.join("renamed.bin");
    std::fs::rename(&p, &q).unwrap();
    assert!(q.exists() && !p.exists());
    std::fs::remove_file(&q).unwrap();
    assert!(!q.exists());

    // The backing dir reflects the changes (true passthrough).
    assert!(!backing.path().join("data.bin").exists());
}
```

- [ ] **Step 2: Run it (fails — write ops unimplemented, default ENOSYS)**

Run: `cargo test -p musefs-latencyfs --test passthrough write_fsync -- --ignored --nocapture`
Expected: FAIL — the `OpenOptions::create` returns `ENOSYS`/`EROFS`-style error (write ops not implemented yet).

- [ ] **Step 3: Implement the write ops**

Add these methods inside the existing `impl fuser::Filesystem for PassthroughFs` block (alongside the read ops). Extend the `use fuser::{...}` line to also import `ReplyCreate, ReplyWrite, TimeOrNow, BsdFileFlags, RenameFlags, WriteFlags`:

```rust
    fn create(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
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
                return reply.error(fuser::Errno::from_i32(e.raw_os_error().unwrap_or(libc::EIO)))
            }
        };
        let m = match file.metadata() {
            Ok(m) => m,
            Err(e) => {
                return reply.error(fuser::Errno::from_i32(e.raw_os_error().unwrap_or(libc::EIO)))
            }
        };
        let ino = self.inodes.lock().unwrap().intern(child);
        let fh = self.next_fh.fetch_add(1, Ordering::Relaxed);
        self.handles.lock().unwrap().insert(fh, file);
        reply.created(
            &TTL,
            &attr_from_meta(ino, &m),
            Generation(0),
            fuser::FileHandle(fh),
            FopenFlags::empty(),
        );
    }

    fn write(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: fuser::FileHandle,
        offset: u64,
        data: &[u8],
        _write_flags: WriteFlags,
        _flags: OpenFlags,
        _lock_owner: Option<fuser::LockOwner>,
        reply: ReplyWrite,
    ) {
        nap(self.lat.write);
        let guard = self.handles.lock().unwrap();
        let Some(file) = guard.get(&fh.0) else {
            return reply.error(fuser::Errno::EBADF);
        };
        match file.write_at(data, offset) {
            Ok(n) => reply.written(n as u32),
            Err(e) => reply.error(fuser::Errno::from_i32(e.raw_os_error().unwrap_or(libc::EIO))),
        }
    }

    fn fsync(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: fuser::FileHandle,
        datasync: bool,
        reply: ReplyEmpty,
    ) {
        nap(self.lat.fsync);
        self.fsyncs.fetch_add(1, Ordering::Relaxed);
        let guard = self.handles.lock().unwrap();
        let Some(file) = guard.get(&fh.0) else {
            return reply.error(fuser::Errno::EBADF);
        };
        let r = if datasync { file.sync_data() } else { file.sync_all() };
        match r {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(fuser::Errno::from_i32(e.raw_os_error().unwrap_or(libc::EIO))),
        }
    }

    fn fsyncdir(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _fh: fuser::FileHandle,
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
        _fh: Option<fuser::FileHandle>,
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
        // The only attr SQLite needs: truncate/extend the WAL.
        if let Some(sz) = size {
            if let Ok(f) = OpenOptions::new().write(true).open(&p) {
                let _ = f.set_len(sz);
            }
        }
        match std::fs::symlink_metadata(&p) {
            Ok(m) => reply.attr(&TTL, &attr_from_meta(ino.0, &m)),
            Err(e) => reply.error(fuser::Errno::from_i32(e.raw_os_error().unwrap_or(libc::EIO))),
        }
    }

    fn unlink(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
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
            Err(e) => reply.error(fuser::Errno::from_i32(e.raw_os_error().unwrap_or(libc::EIO))),
        }
    }

    fn rename(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        newparent: INodeNo,
        newname: &OsStr,
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
            Err(e) => reply.error(fuser::Errno::from_i32(e.raw_os_error().unwrap_or(libc::EIO))),
        }
    }

    fn mkdir(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
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
            Err(e) => reply.error(fuser::Errno::from_i32(e.raw_os_error().unwrap_or(libc::EIO))),
        }
    }

    fn rmdir(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
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
            Err(e) => reply.error(fuser::Errno::from_i32(e.raw_os_error().unwrap_or(libc::EIO))),
        }
    }
```

- [ ] **Step 4: Run the test**

Run: `cargo test -p musefs-latencyfs --test passthrough write_fsync -- --ignored --nocapture`
Expected: PASS (create/write/fsync/rename/unlink all succeed; `fsyncs() >= 1`).

- [ ] **Step 5: Commit**

```bash
git add musefs-latencyfs/src/lib.rs musefs-latencyfs/tests/passthrough.rs
git commit -m "feat(latencyfs): write/create/fsync/setattr/unlink/rename ops"
```

---

## Task 4: Gating smoke test — full SQLite WAL cycle through the mount

This is the spec's gating acceptance criterion: prove the op surface is complete by running a real SQLite WAL workload (create, write, fsync, truncate, checkpoint, read-back) at the `ssd` profile.

**Files:**
- Create: `musefs-latencyfs/tests/sqlite_wal.rs`

- [ ] **Step 1: Write the smoke test**

Create `musefs-latencyfs/tests/sqlite_wal.rs`:

```rust
use musefs_latencyfs::LatencyMount;
use rusqlite::Connection;

#[test]
#[ignore = "requires /dev/fuse; run with --ignored"]
fn sqlite_wal_cycle_through_the_mount() {
    let backing = tempfile::tempdir().unwrap();
    let mount = LatencyMount::new(backing.path(), "ssd").unwrap();
    let db_path = mount.path().join("test.db");

    {
        let conn = Connection::open(&db_path).unwrap();
        // WAL mode exercises -wal/-shm create, write, fsync, and truncate.
        let mode: String = conn
            .query_row("PRAGMA journal_mode = WAL", [], |r| r.get(0))
            .unwrap();
        assert_eq!(mode.to_lowercase(), "wal");
        conn.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)", [])
            .unwrap();
        for i in 0..200 {
            conn.execute("INSERT INTO t (v) VALUES (?1)", [format!("row-{i}")])
                .unwrap();
        }
        // Force a checkpoint (truncates the WAL).
        conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);").unwrap();
        let n: i64 = conn.query_row("SELECT COUNT(*) FROM t", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 200);
    }

    // Reopen and re-read to confirm durability through the mount.
    let conn = Connection::open(&db_path).unwrap();
    let n: i64 = conn.query_row("SELECT COUNT(*) FROM t", [], |r| r.get(0)).unwrap();
    assert_eq!(n, 200);

    assert!(mount.fsyncs() > 0, "WAL writes must have triggered fsyncs");
}
```

- [ ] **Step 2: Run it**

Run: `cargo test -p musefs-latencyfs --test sqlite_wal -- --ignored --nocapture`
Expected: PASS. If it fails with an `EIO`/`disk I/O error`, an op is missing — the failing operation is the one to implement (the test is the op-completeness guard the spec calls for).

- [ ] **Step 3: Commit**

```bash
git add musefs-latencyfs/tests/sqlite_wal.rs
git commit -m "test(latencyfs): gating SQLite WAL cycle smoke test through the mount"
```

---

## Task 5: Latency-effect + fsync-direction test

Proves the injected latency is measurable (and ≈0 under `ssd`) and that the fsync count moves with write volume — the relative SP1 signal.

**Files:**
- Create: `musefs-latencyfs/tests/latency_effect.rs`

- [ ] **Step 1: Write the test**

Create `musefs-latencyfs/tests/latency_effect.rs`:

```rust
use std::time::Instant;

use musefs_latencyfs::LatencyMount;

fn open_read_close(path: &std::path::Path) {
    use std::io::Read;
    let mut s = Vec::new();
    std::fs::File::open(path).unwrap().read_to_end(&mut s).unwrap();
}

#[test]
#[ignore = "requires /dev/fuse; run with --ignored"]
fn nonzero_profile_is_slower_than_ssd() {
    let backing = tempfile::tempdir().unwrap();
    std::fs::write(backing.path().join("f.bin"), vec![0u8; 64 * 1024]).unwrap();

    // ssd (~0) baseline.
    let fast = LatencyMount::new(backing.path(), "ssd").unwrap();
    let t0 = Instant::now();
    for _ in 0..20 {
        open_read_close(&fast.path().join("f.bin"));
    }
    let fast_ms = t0.elapsed().as_millis();
    drop(fast);

    // hdd profile: each open+read sleeps multiple ms, so 20 iterations are far slower.
    let slow = LatencyMount::new(backing.path(), "hdd").unwrap();
    let t1 = Instant::now();
    for _ in 0..20 {
        open_read_close(&slow.path().join("f.bin"));
    }
    let slow_ms = t1.elapsed().as_millis();

    println!("ssd={fast_ms}ms hdd={slow_ms}ms");
    assert!(
        slow_ms > fast_ms + 50,
        "hdd profile ({slow_ms}ms) should be clearly slower than ssd ({fast_ms}ms)"
    );
}

#[test]
#[ignore = "requires /dev/fuse; run with --ignored"]
fn fsync_count_rises_with_more_commits() {
    use rusqlite::Connection;
    let backing = tempfile::tempdir().unwrap();
    let mount = LatencyMount::new(backing.path(), "ssd").unwrap();

    let conn = Connection::open(mount.path().join("c.db")).unwrap();
    let _: String = conn
        .query_row("PRAGMA journal_mode = WAL", [], |r| r.get(0))
        .unwrap();
    conn.execute("CREATE TABLE t (id INTEGER PRIMARY KEY)", []).unwrap();

    let before = mount.fsyncs();
    // Each checkpoint forces fsyncs; more checkpoints => strictly more fsyncs.
    for _ in 0..10 {
        conn.execute("INSERT INTO t DEFAULT VALUES", []).unwrap();
        conn.execute_batch("PRAGMA wal_checkpoint(FULL);").unwrap();
    }
    let after = mount.fsyncs();
    assert!(after > before, "fsync count must rise with commits/checkpoints");
}
```

- [ ] **Step 2: Run it**

Run: `cargo test -p musefs-latencyfs --test latency_effect -- --ignored --nocapture`
Expected: PASS; prints e.g. `ssd=2ms hdd=350ms`.

- [ ] **Step 3: Commit**

```bash
git add musefs-latencyfs/tests/latency_effect.rs
git commit -m "test(latencyfs): latency-effect and fsync-direction guards"
```

---

## Task 6: Wire latency injection into the SP0a scan bench

Add an opt-in path to `bench_ingest`: when `MUSEFS_BENCH_LATENCY_PROFILE` is set, mount the latency FS over the corpus dir and run the scan (and DB) through it, reporting real fsync counts.

**Files:**
- Modify: `musefs-core/Cargo.toml` (dev-dependency)
- Modify: `musefs-core/tests/bench_ingest.rs`

- [ ] **Step 1: Add the dev-dependency**

In `musefs-core/Cargo.toml`, under `[dev-dependencies]`, add:

```toml
musefs-latencyfs = { path = "../musefs-latencyfs" }
```

- [ ] **Step 2: Add a latency-mounted scan variant**

Append to `musefs-core/tests/bench_ingest.rs`:

```rust
#[test]
#[ignore = "needs /dev/fuse + MUSEFS_BENCH_LATENCY_PROFILE; run with --ignored --nocapture"]
fn bench_scan_under_latency() {
    use musefs_latencyfs::LatencyMount;

    let profile = match std::env::var("MUSEFS_BENCH_LATENCY_PROFILE") {
        Ok(p) => p,
        Err(_) => {
            println!("set MUSEFS_BENCH_LATENCY_PROFILE=ssd|hdd|nfs-ssd|nfs-hdd to run");
            return;
        }
    };
    let params = CorpusParams::from_env();
    let tier = std::env::var("MUSEFS_BENCH_TIER").unwrap_or_else(|_| "ci".into());

    // Generate the corpus on a real backing dir, then mount the latency FS over
    // it so scan + DB I/O go through the injected-latency layer.
    let backing = tempfile::tempdir().unwrap();
    common::corpus::generate(backing.path(), &params);
    let mount = LatencyMount::new(backing.path(), &profile).unwrap();

    let db = Db::open(&mount.path().join("musefs-bench.db")).unwrap();
    metrics::reset();
    let t0 = std::time::Instant::now();
    let stats = scan_directory(&db, &mount.path()).unwrap();
    let scan_ms = t0.elapsed().as_millis();
    let s = metrics::snapshot();

    println!("\n{}", RunReport::header());
    println!(
        "{}",
        RunReport {
            label: "scan".into(),
            tier,
            storage: profile,
            wall_ms: scan_ms,
            opens: s.opens,
            preads: s.preads,
            fsyncs: Some(mount.fsyncs()),
            peak_rss_kib: None, // FS runs in-process here, but RSS attribution is mixed; omit.
        }
        .row()
    );
    println!("scanned={} skipped={}\n", stats.scanned, stats.skipped);
    assert!(stats.scanned > 0);
}
```

Note: the corpus is generated on `backing` (no latency), then read through `mount.path()` (read latency) while the DB is created/written through the same mount (write + fsync latency). `mount.fsyncs()` is the real kernel fsync count for the scan's DB writes — the SP1-batching signal. RSS is omitted here because the passthrough FS shares this process, so `VmHWM` no longer isolates the scan's own footprint (use the SP0a tempfs `bench_cold_scan_and_revalidate` for the RSS signal).

- [ ] **Step 3: Run it (ci tier, ssd profile)**

Run: `MUSEFS_BENCH_LATENCY_PROFILE=ssd cargo test -p musefs-core --features metrics --test bench_ingest bench_scan_under_latency -- --ignored --nocapture`
Expected: PASS; prints a row with a non-`n/a` `fsyncs` value and `scanned=200`.

- [ ] **Step 4: Confirm it no-ops without the env var**

Run: `cargo test -p musefs-core --test bench_ingest bench_scan_under_latency -- --ignored --nocapture`
Expected: prints the "set MUSEFS_BENCH_LATENCY_PROFILE…" hint and returns (no panic).

- [ ] **Step 5: Commit**

```bash
git add musefs-core/Cargo.toml musefs-core/tests/bench_ingest.rs
git commit -m "bench(core): scan under injected latency via musefs-latencyfs"
```

---

## Task 7: Update tracking doc + spec acceptance status

**Files:**
- Modify: `docs/superpowers/specs/2026-05-30-optimization-pass/README.md`

- [ ] **Step 1: Update status + add run instructions**

In the Status table, change the SP0b row state to `Implemented`. Append to the "Running the SP0a harness" section:

```markdown
### Latency-injected runs (SP0b — needs /dev/fuse)

```bash
# Functional + gating tests for the passthrough FS:
cargo test -p musefs-latencyfs -- --ignored --nocapture

# Scan a generated corpus through an injected-latency mount (real fsync counts):
MUSEFS_BENCH_LATENCY_PROFILE=nfs-hdd MUSEFS_BENCH_TIER=large-compute \
  cargo test -p musefs-core --features metrics \
  --test bench_ingest bench_scan_under_latency -- --ignored --nocapture
```

Profiles: `ssd` (≈0), `hdd`, `nfs-ssd`, `nfs-hdd`. The corpus is generated on a
real backing dir; only the scan + DB I/O traverse the latency layer.
```

- [ ] **Step 2: Commit**

```bash
git add docs/superpowers/specs/2026-05-30-optimization-pass/README.md
git commit -m "docs: record SP0b latency-injection harness and how to run it"
```

---

## Self-review notes (for the executor)

- **Spec coverage (the deferred SP0b items):** passthrough-FUSE latency injection (Tasks 1–3), `ssd` gating smoke test incl. WAL (Task 4), latency-effect + fsync-direction (Task 5), fsync counter/column wired into reporting (Task 6). All FUSE tests are `#[ignore]`d, matching the existing e2e convention; the default `cargo test` is unchanged.
- **Op completeness risk:** Task 4's SQLite WAL cycle is the guard. If it fails, the error names the missing op; implement it following the Task-3 patterns (e.g. `getxattr`/`lseek` are tolerated as ENOSYS by SQLite, but if a build surfaces one, add a `reply.error(Errno::ENOSYS)` override is already the default — no action — whereas a *data* op like `copy_file_range` would need adding).
- **Drop-order correctness:** `LatencyMount` declares `_bg` before `_mountdir` so the session unmounts before the mountpoint tempdir is removed.
- **No production code touched:** `musefs-latencyfs` is a new `publish = false` crate; the only edit to a shipping crate is a `[dev-dependencies]` line in `musefs-core`.
- **Type consistency:** `LatencyMount::{new, path, fsyncs}`, `Latency::profile`, `PassthroughFs` used identically across tasks; reply/trait signatures match the verified fuser 0.17 source.
- **Known approximation:** profile latencies are representative, not calibrated to specific hardware; they model *relative* HDD/NFS behavior (the spec's stated non-goal is faithful bandwidth, which real-mount runs cover).
```
