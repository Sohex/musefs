# musefs M2 (FUSE + CLI) — End-to-End Read-Only FLAC Mount Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Expose the already-built `musefs-core` read path as a real, mountable FUSE filesystem and a `musefs` CLI (`scan` + `mount`), delivering an end-to-end read-only FLAC mount.

**Architecture:** A thin `musefs-fuse` crate implements `fuser::Filesystem` for a `MusefsFs` wrapper that owns a `musefs_core::Musefs` and translates VFS calls (lookup/getattr/readdir/read) into core operations, mapping `CoreError` to errno and `Attr` to `fuser::FileAttr`. A `musefs-cli` crate provides the `musefs` binary: `scan` walks a backing directory into a SQLite DB via `musefs_core::scan_directory`; `mount` builds a `Musefs` from the DB + a `MountConfig` and hands it to `musefs-fuse`. The mount is dispatched single-threaded (fuser 0.14's session loop), which is exactly what `Musefs`'s `&mut self` methods assume.

**Tech Stack:** Rust workspace; `fuser` 0.14 (libfuse3-backed mount; `libfuse3-dev` 3.18 is installed and `/dev/fuse` is available); `clap` 4 (derive); `anyhow` for CLI error ergonomics; `libc` for uid/gid + errno constants; existing `musefs-db` / `musefs-format` / `musefs-core`.

**Scope (this milestone):** synthesis-mode read-only FLAC mount + `scan` and `mount` commands.
**Explicitly deferred (later milestones, not this plan):** `--mode structure-only` (M5), `musefs refresh` / SIGHUP / `data_version` polling (M5), `--revalidate` (M5), TOML `--config` file loading (M5 polish — this plan uses CLI flags only), per-fd `open()` snapshot pinning (M2-core resolves per-read against the content-version-keyed cache; documented behavior), MP3 (M3), embedded art in the read path (M4).

---

## File Structure

- `musefs-fuse/Cargo.toml` — new crate manifest (deps: `fuser` 0.14, `libc`, `musefs-core`; dev-deps: `tempfile`, `metaflac`, `musefs-db`).
- `musefs-fuse/src/lib.rs` — `MusefsFs`, the `fuser::Filesystem` impl, the pure helpers `errno` / `to_file_attr`, and the `mount` / `spawn` entry functions. One focused file; the crate has a single responsibility (VFS-call translation).
- `musefs-fuse/tests/mount.rs` — gated (`#[ignore]`) end-to-end mount integration test.
- `musefs-cli/Cargo.toml` — new crate manifest (deps: `clap` 4 derive, `anyhow`, `musefs-db`, `musefs-core`, `musefs-fuse`).
- `musefs-cli/src/lib.rs` — `Cli`/`Command` clap types, `run`, `run_scan`, `run_mount`. Library so the scan path is unit-testable without a binary.
- `musefs-cli/src/main.rs` — thin entry point: parse args, dispatch, print errors.
- `musefs-core/src/tree.rs`, `musefs-core/src/facade.rs` — add a `parent()` accessor (Task 1) for FUSE `..` entries.
- `Cargo.toml` (workspace root) — add `musefs-fuse` and `musefs-cli` to `members`.

**Branch:** all work happens on a new branch `musefs-m2-fuse-cli` cut from `main`.

---

## Task 1: `musefs-core` — `parent()` accessor for FUSE `..` entries

The FUSE `readdir` must emit `.` (the directory's own inode) and `..` (its parent). `VirtualTree` stores each node's parent but exposes no accessor; root's parent is itself (`Node { parent: ROOT, .. }`), which is the correct FUSE convention for `..` at the root.

**Files:**
- Modify: `musefs-core/src/tree.rs`
- Modify: `musefs-core/src/facade.rs`
- Test: `musefs-core/tests/tree.rs`, `musefs-core/tests/facade.rs`

- [ ] **Step 1: Write the failing tests**

Append to `musefs-core/tests/tree.rs`:

```rust
#[test]
fn parent_of_root_is_root_and_children_point_back() {
    let tree = VirtualTree::build(&[(1, "Alice/Song.flac".to_string())]);
    assert_eq!(tree.parent(VirtualTree::ROOT), Some(VirtualTree::ROOT));

    let alice = tree.lookup(VirtualTree::ROOT, "Alice").unwrap();
    assert_eq!(tree.parent(alice), Some(VirtualTree::ROOT));

    let song = tree.lookup(alice, "Song.flac").unwrap();
    assert_eq!(tree.parent(song), Some(alice));

    assert_eq!(tree.parent(99999), None);
}
```

Append to `musefs-core/tests/facade.rs` (the existing `config`/`scanned_db`/`make_flac` helpers are already in that file):

```rust
#[test]
fn parent_exposes_the_tree_hierarchy() {
    let dir = tempfile::tempdir().unwrap();
    let db = scanned_db(dir.path());
    let fs = Musefs::open(db, config()).unwrap();

    let artist = fs.lookup(VirtualTree::ROOT, "Alice").unwrap();
    assert_eq!(fs.parent(artist), Some(VirtualTree::ROOT));
    assert_eq!(fs.parent(VirtualTree::ROOT), Some(VirtualTree::ROOT));
    assert_eq!(fs.parent(424242), None);
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p musefs-core --test tree --test facade`
Expected: FAIL — `no method named parent found for ... VirtualTree` / `... Musefs`.

- [ ] **Step 3: Implement the accessors**

In `musefs-core/src/tree.rs`, add this method inside `impl VirtualTree` (e.g. right after `node`):

```rust
    /// The parent inode of `inode` (root's parent is itself), or `None` if `inode`
    /// is unknown. Used by the FUSE layer to emit `..` directory entries.
    pub fn parent(&self, inode: u64) -> Option<u64> {
        self.nodes.get(&inode).map(|n| n.parent)
    }
```

In `musefs-core/src/facade.rs`, add this method inside `impl Musefs` (e.g. right after `lookup`):

```rust
    /// The parent inode of `inode` (root's parent is itself). Forwards to the tree.
    pub fn parent(&self, inode: u64) -> Option<u64> {
        self.tree.parent(inode)
    }
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p musefs-core --test tree --test facade`
Expected: PASS.

- [ ] **Step 5: Confirm zero warnings, then commit**

Run: `cargo clippy -p musefs-core --all-targets 2>&1 | grep -iE "warning|error" || echo clean`
Expected: `clean`.

```bash
git add musefs-core/src/tree.rs musefs-core/src/facade.rs musefs-core/tests/tree.rs musefs-core/tests/facade.rs
git commit -m "$(printf 'feat(core): expose parent() accessor for FUSE .. entries\n\nCo-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>')"
```

---

## Task 2: `musefs-fuse` crate scaffold

**Files:**
- Create: `musefs-fuse/Cargo.toml`
- Create: `musefs-fuse/src/lib.rs`
- Modify: `Cargo.toml` (workspace `members`)

- [ ] **Step 1: Create the crate manifest**

`musefs-fuse/Cargo.toml`:

```toml
[package]
name = "musefs-fuse"
version = "0.1.0"
edition = "2021"

[dependencies]
fuser = "0.14"
libc = "0.2"
musefs-core = { path = "../musefs-core" }

[dev-dependencies]
tempfile = "3"
metaflac = "0.2"
musefs-db = { path = "../musefs-db" }
```

- [ ] **Step 2: Create the lib stub**

`musefs-fuse/src/lib.rs`:

```rust
//! FUSE filesystem binding for musefs: translates VFS calls into `musefs-core`
//! operations. Mounted single-threaded (fuser's session loop), matching the
//! `&mut self` read path in `musefs_core::Musefs`.
```

- [ ] **Step 3: Add the crate to the workspace**

In the root `Cargo.toml`, extend `members` to include the new crate:

```toml
members = ["musefs-db", "musefs-format", "musefs-core", "musefs-fuse"]
```

- [ ] **Step 4: Verify it builds**

Run: `cargo build -p musefs-fuse 2>&1 | tail -5`
Expected: builds (fuser links against the installed libfuse3 via pkg-config). If it fails with a pkg-config / libfuse linkage error, STOP and report — do not switch the dependency to `default-features = false` without confirmation.

- [ ] **Step 5: Commit**

```bash
git add Cargo.toml Cargo.lock musefs-fuse/Cargo.toml musefs-fuse/src/lib.rs
git commit -m "$(printf 'chore(fuse): scaffold musefs-fuse crate\n\nCo-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>')"
```

---

## Task 3: `musefs-fuse` — `CoreError` → errno mapping

**Files:**
- Modify: `musefs-fuse/src/lib.rs`

- [ ] **Step 1: Write the failing test**

Append to `musefs-fuse/src/lib.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use musefs_core::CoreError;

    #[test]
    fn maps_core_errors_to_errno() {
        assert_eq!(errno(&CoreError::NoEntry(7)), libc::ENOENT);
        assert_eq!(errno(&CoreError::TrackNotFound(7)), libc::ENOENT);
        assert_eq!(errno(&CoreError::IsDir(7)), libc::EISDIR);
        assert_eq!(errno(&CoreError::BackingChanged("x".into())), libc::EIO);
        assert_eq!(errno(&CoreError::ArtNotSupported), libc::EIO);

        let io = CoreError::Io(std::io::Error::from_raw_os_error(libc::ENOENT));
        assert_eq!(errno(&io), libc::ENOENT);
        let io_other = CoreError::Io(std::io::Error::new(std::io::ErrorKind::Other, "boom"));
        assert_eq!(errno(&io_other), libc::EIO);
    }
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p musefs-fuse errno`
Expected: FAIL — `cannot find function errno in this scope`.

- [ ] **Step 3: Implement `errno`**

Add to `musefs-fuse/src/lib.rs` (after the module doc comment):

```rust
use musefs_core::CoreError;

/// Map a core error onto a POSIX errno for the FUSE reply. `Io` errors carry the
/// underlying errno when present; everything structural collapses to `EIO`.
pub fn errno(err: &CoreError) -> i32 {
    match err {
        CoreError::NoEntry(_) | CoreError::TrackNotFound(_) => libc::ENOENT,
        CoreError::IsDir(_) => libc::EISDIR,
        CoreError::Io(e) => e.raw_os_error().unwrap_or(libc::EIO),
        CoreError::BackingChanged(_)
        | CoreError::Db(_)
        | CoreError::Format(_)
        | CoreError::ArtNotSupported => libc::EIO,
    }
}
```

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test -p musefs-fuse errno`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add musefs-fuse/src/lib.rs
git commit -m "$(printf 'feat(fuse): map CoreError to errno\n\nCo-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>')"
```

---

## Task 4: `musefs-fuse` — `Attr` → `fuser::FileAttr` conversion

**Files:**
- Modify: `musefs-fuse/src/lib.rs`

- [ ] **Step 1: Write the failing test**

Add these assertions inside the existing `#[cfg(test)] mod tests` in `musefs-fuse/src/lib.rs`:

```rust
    use fuser::FileType;
    use musefs_core::Attr;
    use std::time::{Duration, SystemTime};

    #[test]
    fn converts_dir_and_file_attrs() {
        let fallback = SystemTime::UNIX_EPOCH + Duration::from_secs(1000);

        let dir = Attr { inode: 1, is_dir: true, size: 0, mtime_secs: 0 };
        let fa = to_file_attr(&dir, 501, 20, fallback);
        assert_eq!(fa.ino, 1);
        assert_eq!(fa.kind, FileType::Directory);
        assert_eq!(fa.perm, 0o555);
        assert_eq!(fa.uid, 501);
        assert_eq!(fa.gid, 20);
        // mtime_secs == 0 falls back to the supplied mount time.
        assert_eq!(fa.mtime, fallback);

        let file = Attr { inode: 9, is_dir: false, size: 4096, mtime_secs: 1_700_000_000 };
        let fa = to_file_attr(&file, 501, 20, fallback);
        assert_eq!(fa.kind, FileType::RegularFile);
        assert_eq!(fa.perm, 0o444);
        assert_eq!(fa.size, 4096);
        assert_eq!(fa.blocks, 8); // 4096 / 512
        assert_eq!(fa.mtime, SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000));
    }
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p musefs-fuse converts_dir_and_file_attrs`
Expected: FAIL — `cannot find function to_file_attr`.

- [ ] **Step 3: Implement `to_file_attr`**

Add to `musefs-fuse/src/lib.rs` (near `errno`):

```rust
use fuser::{FileAttr, FileType};
use musefs_core::Attr;
use std::time::{Duration, SystemTime};

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
```

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test -p musefs-fuse converts_dir_and_file_attrs`
Expected: PASS.

- [ ] **Step 5: Confirm zero warnings, then commit**

Run: `cargo clippy -p musefs-fuse --all-targets 2>&1 | grep -iE "warning|error" || echo clean`
Expected: `clean`.

```bash
git add musefs-fuse/src/lib.rs
git commit -m "$(printf 'feat(fuse): convert core Attr to fuser FileAttr\n\nCo-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>')"
```

---

## Task 5: `musefs-fuse` — `MusefsFs` Filesystem impl + mount entry points

This is the heart of the milestone. The pure helpers are already tested (Tasks 3–4); this task wires them into the `fuser::Filesystem` trait and is driven by a gated end-to-end mount test (it needs `/dev/fuse`, which is available here, so it should pass when run with `--ignored`).

**Files:**
- Modify: `musefs-fuse/src/lib.rs`
- Test: `musefs-fuse/tests/mount.rs`

- [ ] **Step 1: Write the failing (gated) integration test**

`musefs-fuse/tests/mount.rs`:

```rust
use std::collections::BTreeMap;

use musefs_core::{scan_directory, MountConfig, Musefs};

// --- minimal proven FLAC fixture (mirrors musefs-core/tests/common) ---

fn flac_block(block_type: u8, body: &[u8], is_last: bool) -> Vec<u8> {
    let mut out = Vec::new();
    out.push((if is_last { 0x80 } else { 0 }) | (block_type & 0x7F));
    let len = body.len();
    out.push(((len >> 16) & 0xFF) as u8);
    out.push(((len >> 8) & 0xFF) as u8);
    out.push((len & 0xFF) as u8);
    out.extend_from_slice(body);
    out
}

fn streaminfo_body() -> Vec<u8> {
    let mut b = vec![
        0x10, 0x00, 0x10, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x0A, 0xC4, 0x42, 0xF0, 0x00,
        0x00, 0x00, 0x00,
    ];
    b.extend_from_slice(&[0u8; 16]);
    b
}

fn vorbis_comment_body(vendor: &str, comments: &[&str]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&(vendor.len() as u32).to_le_bytes());
    out.extend_from_slice(vendor.as_bytes());
    out.extend_from_slice(&(comments.len() as u32).to_le_bytes());
    for c in comments {
        out.extend_from_slice(&(c.len() as u32).to_le_bytes());
        out.extend_from_slice(c.as_bytes());
    }
    out
}

fn make_flac(comments: &[&str], audio: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(b"fLaC");
    out.extend_from_slice(&flac_block(0, &streaminfo_body(), false));
    out.extend_from_slice(&flac_block(4, &vorbis_comment_body("orig", comments), true));
    out.extend_from_slice(audio);
    out
}

fn config() -> MountConfig {
    MountConfig {
        template: "$artist/$title".to_string(),
        fallbacks: BTreeMap::new(),
        default_fallback: "Unknown".to_string(),
    }
}

#[test]
#[ignore = "requires /dev/fuse; run with: cargo test -p musefs-fuse -- --ignored"]
fn end_to_end_read_through_mount() {
    // Build backing dir + scanned DB + Musefs.
    let backing = tempfile::tempdir().unwrap();
    let flac = make_flac(&["ARTIST=Alice", "TITLE=Song"], &[0xAB; 64]);
    std::fs::write(backing.path().join("a.flac"), &flac).unwrap();
    let db = musefs_db::Db::open_in_memory().unwrap();
    scan_directory(&db, backing.path()).unwrap();
    let fs = Musefs::open(db, config()).unwrap();

    // Mount it in the background.
    let mountpoint = tempfile::tempdir().unwrap();
    let session = musefs_fuse::spawn(fs, mountpoint.path(), "musefs-test").unwrap();

    // Read /Alice/Song.flac through the mount and decode it independently.
    let song = mountpoint.path().join("Alice").join("Song.flac");
    let bytes = std::fs::read(&song).unwrap();
    let tag = metaflac::Tag::read_from(&mut std::io::Cursor::new(&bytes)).unwrap();
    assert_eq!(
        tag.vorbis_comments().unwrap().get("TITLE").map(|v| v.as_slice()),
        Some(["Song".to_string()].as_slice())
    );

    // readdir through the mount.
    let mut names: Vec<String> = std::fs::read_dir(mountpoint.path().join("Alice"))
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    assert_eq!(names, vec!["Song.flac".to_string()]);

    drop(session); // unmounts
    drop(backing);
}
```

> **Note for the implementer:** delete the unused `build_fs`/its placeholder body — it was scaffolding in this plan's draft. Keep only `config()`, the fixture helpers, and `end_to_end_read_through_mount`. The real fixture wiring lives inside the test function. (This note exists because the plan must not ship dead helper code; remove `build_fs` entirely.)

- [ ] **Step 2: Run the test to verify it fails to compile**

Run: `cargo test -p musefs-fuse --test mount 2>&1 | tail -15`
Expected: FAIL — `function spawn not found in crate musefs_fuse`. Confirm the failure is the missing `spawn`, not a fixture error.

- [ ] **Step 3: Implement `MusefsFs`, the `Filesystem` impl, and the mount entry points**

Add to `musefs-fuse/src/lib.rs`:

```rust
use std::ffi::OsStr;
use std::path::Path;

use fuser::{
    BackgroundSession, Filesystem, MountOption, ReplyAttr, ReplyData, ReplyDirectory, ReplyEntry,
    Request,
};
use musefs_core::Musefs;

const TTL: Duration = Duration::from_secs(1);

/// A `fuser::Filesystem` that serves a `musefs_core::Musefs`. Owns the core
/// (and thus the DB + header cache); fuser drives it single-threaded, so the
/// `&mut self` core methods are safe.
pub struct MusefsFs {
    core: Musefs,
    uid: u32,
    gid: u32,
    mount_time: SystemTime,
}

impl MusefsFs {
    pub fn new(core: Musefs) -> MusefsFs {
        MusefsFs {
            core,
            // SAFETY: getuid/getgid are always-successful libc calls.
            uid: unsafe { libc::getuid() },
            gid: unsafe { libc::getgid() },
            mount_time: SystemTime::now(),
        }
    }
}

impl Filesystem for MusefsFs {
    fn lookup(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEntry) {
        let name = match name.to_str() {
            Some(n) => n,
            None => return reply.error(libc::ENOENT),
        };
        let child = match self.core.lookup(parent, name) {
            Some(ino) => ino,
            None => return reply.error(libc::ENOENT),
        };
        match self.core.getattr(child) {
            Ok(attr) => reply.entry(&TTL, &to_file_attr(&attr, self.uid, self.gid, self.mount_time), 0),
            Err(e) => reply.error(errno(&e)),
        }
    }

    fn getattr(&mut self, _req: &Request<'_>, ino: u64, reply: ReplyAttr) {
        match self.core.getattr(ino) {
            Ok(attr) => reply.attr(&TTL, &to_file_attr(&attr, self.uid, self.gid, self.mount_time)),
            Err(e) => reply.error(errno(&e)),
        }
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
        match self.core.read(ino, offset as u64, size as u64) {
            Ok(bytes) => reply.data(&bytes),
            Err(e) => reply.error(errno(&e)),
        }
    }

    fn readdir(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        _fh: u64,
        offset: i64,
        mut reply: ReplyDirectory,
    ) {
        let entries = match self.core.readdir(ino) {
            Ok(e) => e,
            Err(e) => return reply.error(errno(&e)),
        };
        let parent = self.core.parent(ino).unwrap_or(ino);

        // `.` and `..` first, then the children. `offset` is the index already
        // consumed by a previous call; `reply.add` returns true when the buffer
        // is full, at which point we stop and reply.
        let mut listing: Vec<(u64, fuser::FileType, String)> = Vec::with_capacity(entries.len() + 2);
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
    vec![
        MountOption::RO,
        MountOption::FSName(fs_name.to_string()),
        MountOption::AutoUnmount,
    ]
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
```

> Note: `Duration` / `SystemTime` are already imported by Task 4's `to_file_attr` block. If the compiler reports an unused or duplicate import after combining, consolidate the `use std::time::{Duration, SystemTime};` line to a single occurrence at the top of the file. Do not add a second import.

- [ ] **Step 4: Run the gated test to verify it passes**

Run: `cargo test -p musefs-fuse --test mount -- --ignored`
Expected: PASS (`end_to_end_read_through_mount`). This actually mounts via libfuse3 and reads through the kernel.

If it fails to *mount* (e.g. `/dev/fuse` permission in a restricted sandbox) rather than failing an assertion, report the exact error. The logic is still covered by the core tests; the mount test is environmental.

- [ ] **Step 5: Confirm the default suite still passes and is warning-free**

The mount test is `#[ignore]`, so the normal suite skips it:
Run: `cargo test -p musefs-fuse 2>&1 | tail -8`
Expected: the unit tests pass; `mount` test shows as ignored.
Run: `cargo clippy -p musefs-fuse --all-targets 2>&1 | grep -iE "warning|error" || echo clean`
Expected: `clean`.

- [ ] **Step 6: Commit**

```bash
git add musefs-fuse/src/lib.rs musefs-fuse/tests/mount.rs
git commit -m "$(printf 'feat(fuse): MusefsFs Filesystem impl and mount entry points\n\nCo-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>')"
```

---

## Task 6: `musefs-cli` crate scaffold

**Files:**
- Create: `musefs-cli/Cargo.toml`
- Create: `musefs-cli/src/lib.rs`
- Create: `musefs-cli/src/main.rs`
- Modify: `Cargo.toml` (workspace `members`)

- [ ] **Step 1: Create the crate manifest**

`musefs-cli/Cargo.toml`:

```toml
[package]
name = "musefs-cli"
version = "0.1.0"
edition = "2021"

[[bin]]
name = "musefs"
path = "src/main.rs"

[dependencies]
clap = { version = "4", features = ["derive"] }
anyhow = "1"
musefs-db = { path = "../musefs-db" }
musefs-core = { path = "../musefs-core" }
musefs-fuse = { path = "../musefs-fuse" }

[dev-dependencies]
tempfile = "3"
```

- [ ] **Step 2: Create the lib stub**

`musefs-cli/src/lib.rs`:

```rust
//! The `musefs` command-line interface: `scan` (ingest a backing directory into a
//! SQLite store) and `mount` (serve a read-only FUSE view of that store).
```

- [ ] **Step 3: Create the binary entry point**

`musefs-cli/src/main.rs`:

```rust
fn main() {
    std::process::exit(0);
}
```

(This is a temporary stub; Task 8 replaces it with real dispatch.)

- [ ] **Step 4: Add the crate to the workspace**

In the root `Cargo.toml`:

```toml
members = ["musefs-db", "musefs-format", "musefs-core", "musefs-fuse", "musefs-cli"]
```

- [ ] **Step 5: Verify it builds**

Run: `cargo build -p musefs-cli 2>&1 | tail -5`
Expected: builds.

- [ ] **Step 6: Commit**

```bash
git add Cargo.toml Cargo.lock musefs-cli/Cargo.toml musefs-cli/src/lib.rs musefs-cli/src/main.rs
git commit -m "$(printf 'chore(cli): scaffold musefs-cli crate\n\nCo-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>')"
```

---

## Task 7: `musefs-cli` — `scan` command (`run_scan`)

**Files:**
- Modify: `musefs-cli/src/lib.rs`
- Test: `musefs-cli/tests/scan.rs`

- [ ] **Step 1: Write the failing test**

`musefs-cli/tests/scan.rs`:

```rust
use musefs_cli::run_scan;

fn flac_block(block_type: u8, body: &[u8], is_last: bool) -> Vec<u8> {
    let mut out = Vec::new();
    out.push((if is_last { 0x80 } else { 0 }) | (block_type & 0x7F));
    let len = body.len();
    out.push(((len >> 16) & 0xFF) as u8);
    out.push(((len >> 8) & 0xFF) as u8);
    out.push((len & 0xFF) as u8);
    out.extend_from_slice(body);
    out
}

fn streaminfo_body() -> Vec<u8> {
    let mut b = vec![
        0x10, 0x00, 0x10, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x0A, 0xC4, 0x42, 0xF0, 0x00,
        0x00, 0x00, 0x00,
    ];
    b.extend_from_slice(&[0u8; 16]);
    b
}

fn vorbis_comment_body(comments: &[&str]) -> Vec<u8> {
    let vendor = "orig";
    let mut out = Vec::new();
    out.extend_from_slice(&(vendor.len() as u32).to_le_bytes());
    out.extend_from_slice(vendor.as_bytes());
    out.extend_from_slice(&(comments.len() as u32).to_le_bytes());
    for c in comments {
        out.extend_from_slice(&(c.len() as u32).to_le_bytes());
        out.extend_from_slice(c.as_bytes());
    }
    out
}

fn make_flac(comments: &[&str], audio: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(b"fLaC");
    out.extend_from_slice(&flac_block(0, &streaminfo_body(), false));
    out.extend_from_slice(&flac_block(4, &vorbis_comment_body(comments), true));
    out.extend_from_slice(audio);
    out
}

#[test]
fn scan_ingests_flacs_into_a_fresh_db() {
    let backing = tempfile::tempdir().unwrap();
    std::fs::write(
        backing.path().join("a.flac"),
        make_flac(&["ARTIST=Alice", "TITLE=Song"], &[0xAB; 32]),
    )
    .unwrap();

    let db_dir = tempfile::tempdir().unwrap();
    let db_path = db_dir.path().join("musefs.db");

    let stats = run_scan(&db_path, backing.path()).unwrap();
    assert_eq!(stats.scanned, 1);

    // The DB file was created and persists the track.
    let db = musefs_db::Db::open(&db_path).unwrap();
    let tracks = db.list_tracks().unwrap();
    assert_eq!(tracks.len(), 1);
    assert!(tracks[0].backing_path.ends_with("a.flac"));
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p musefs-cli --test scan`
Expected: FAIL — `cannot find function run_scan in crate musefs_cli`.

- [ ] **Step 3: Implement `run_scan` (and the clap types it will share with mount)**

Append to `musefs-cli/src/lib.rs`:

```rust
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use musefs_core::ScanStats;
use musefs_db::Db;

#[derive(Parser, Debug)]
#[command(name = "musefs", about = "Read-only re-tagging FUSE view of a music library")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Walk a backing directory, ingesting FLAC files into the SQLite store.
    Scan {
        /// Directory of backing audio files to scan recursively.
        backing_dir: PathBuf,
        /// Path to the SQLite database (created if absent).
        #[arg(long)]
        db: PathBuf,
    },
    /// Mount a read-only FUSE view of the store.
    Mount {
        /// Empty directory to mount at.
        mountpoint: PathBuf,
        /// Path to the SQLite database.
        #[arg(long)]
        db: PathBuf,
        /// Path template, e.g. "$albumartist/$album/$title".
        #[arg(long, default_value = "$artist/$title")]
        template: String,
        /// Fallback value substituted for any missing template field.
        #[arg(long, default_value = "Unknown")]
        default_fallback: String,
    },
}

/// Open (creating/migrating) the DB at `db_path` and scan `backing_dir` into it.
pub fn run_scan(db_path: &Path, backing_dir: &Path) -> Result<ScanStats> {
    let db = Db::open(db_path)
        .with_context(|| format!("opening database at {}", db_path.display()))?;
    let stats = musefs_core::scan_directory(&db, backing_dir)
        .with_context(|| format!("scanning {}", backing_dir.display()))?;
    Ok(stats)
}
```

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test -p musefs-cli --test scan`
Expected: PASS.

- [ ] **Step 5: Confirm zero warnings, then commit**

Run: `cargo clippy -p musefs-cli --all-targets 2>&1 | grep -iE "warning|error" || echo clean`
Expected: `clean`. (The `Mount` variant is unused until Task 8; clap derive constructs it, so there is no dead-code warning. If one appears, it will be resolved in Task 8, not suppressed here — Task 8 lands immediately after.)

```bash
git add musefs-cli/src/lib.rs musefs-cli/tests/scan.rs
git commit -m "$(printf 'feat(cli): scan command ingesting FLACs into the store\n\nCo-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>')"
```

---

## Task 8: `musefs-cli` — `mount` command + dispatch + `main`

**Files:**
- Modify: `musefs-cli/src/lib.rs`
- Modify: `musefs-cli/src/main.rs`
- Test: `musefs-cli/tests/cli.rs`

- [ ] **Step 1: Write the failing test**

`musefs-cli/tests/cli.rs` (verifies arg parsing wires up, without mounting):

```rust
use clap::Parser;
use musefs_cli::{Cli, Command};

#[test]
fn parses_scan_and_mount_invocations() {
    let cli = Cli::parse_from(["musefs", "scan", "/music", "--db", "/tmp/m.db"]);
    match cli.command {
        Command::Scan { backing_dir, db } => {
            assert_eq!(backing_dir.to_str(), Some("/music"));
            assert_eq!(db.to_str(), Some("/tmp/m.db"));
        }
        _ => panic!("expected scan"),
    }

    let cli = Cli::parse_from([
        "musefs", "mount", "/mnt/x", "--db", "/tmp/m.db", "--template", "$album/$title",
    ]);
    match cli.command {
        Command::Mount { mountpoint, db, template, default_fallback } => {
            assert_eq!(mountpoint.to_str(), Some("/mnt/x"));
            assert_eq!(db.to_str(), Some("/tmp/m.db"));
            assert_eq!(template, "$album/$title");
            assert_eq!(default_fallback, "Unknown"); // default applied
        }
        _ => panic!("expected mount"),
    }
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p musefs-cli --test cli`
Expected: FAIL — compile error or panic until `run` + `run_mount` exist and `main` dispatches. (The `Cli`/`Command` types exist from Task 7, so this may compile; the purpose is to lock the parsing contract before `main` is wired.)

- [ ] **Step 3: Implement `run_mount` and `run`**

Append to `musefs-cli/src/lib.rs`:

```rust
use std::collections::BTreeMap;

use musefs_core::{MountConfig, Musefs};

/// Build a `Musefs` from the DB at `db_path` and mount it (blocking) at
/// `mountpoint`.
pub fn run_mount(
    db_path: &Path,
    mountpoint: &Path,
    template: String,
    default_fallback: String,
) -> Result<()> {
    let db = Db::open(db_path)
        .with_context(|| format!("opening database at {}", db_path.display()))?;
    let config = MountConfig {
        template,
        fallbacks: BTreeMap::new(),
        default_fallback,
    };
    let core = Musefs::open(db, config).context("building the virtual filesystem")?;
    musefs_fuse::mount(core, mountpoint, "musefs")
        .with_context(|| format!("mounting at {}", mountpoint.display()))?;
    Ok(())
}

/// Dispatch a parsed CLI invocation.
pub fn run(cli: Cli) -> Result<()> {
    match cli.command {
        Command::Scan { backing_dir, db } => {
            let stats = run_scan(&db, &backing_dir)?;
            println!("scanned {} file(s), skipped {}", stats.scanned, stats.skipped);
            Ok(())
        }
        Command::Mount { mountpoint, db, template, default_fallback } => {
            run_mount(&db, &mountpoint, template, default_fallback)
        }
    }
}
```

- [ ] **Step 4: Replace `main.rs` with real dispatch**

`musefs-cli/src/main.rs`:

```rust
use clap::Parser;
use musefs_cli::{run, Cli};

fn main() {
    if let Err(e) = run(Cli::parse()) {
        eprintln!("musefs: {e:#}");
        std::process::exit(1);
    }
}
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p musefs-cli`
Expected: PASS (`scan` + `cli` tests).

- [ ] **Step 6: Confirm the binary builds and `--help` works, then commit**

Run: `cargo run -p musefs-cli -- --help 2>&1 | tail -15`
Expected: usage text listing `scan` and `mount`.
Run: `cargo clippy -p musefs-cli --all-targets 2>&1 | grep -iE "warning|error" || echo clean`
Expected: `clean`.

```bash
git add musefs-cli/src/lib.rs musefs-cli/src/main.rs musefs-cli/tests/cli.rs
git commit -m "$(printf 'feat(cli): mount command, dispatch, and binary entry point\n\nCo-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>')"
```

---

## Task 9: Whole-workspace verification + manual end-to-end

**Files:** none (verification only).

- [ ] **Step 1: Run the entire workspace test suite (default, fast)**

Run: `cargo test`
Expected: PASS across `musefs-db`, `musefs-format`, `musefs-core`, `musefs-fuse` (mount test ignored), `musefs-cli`.

- [ ] **Step 2: Run the gated mount integration test**

Run: `cargo test -p musefs-fuse -- --ignored`
Expected: PASS (`end_to_end_read_through_mount`). If the environment denies `/dev/fuse`, record the exact error; do not delete or weaken the test.

- [ ] **Step 3: Confirm a clean, warning-free workspace build**

Run: `cargo clippy --workspace --all-targets 2>&1 | grep -iE "warning|error" || echo "clean"`
Expected: `clean`.

- [ ] **Step 4: Manual end-to-end smoke (real binary, real mount)**

```bash
# Build the binary.
cargo build -p musefs-cli

# Prepare a tiny backing library + scan it.
BACK=$(mktemp -d); DB=$(mktemp -u --suffix=.db); MNT=$(mktemp -d)
# (place at least one real .flac in $BACK, e.g. cp a sample file there)
./target/debug/musefs scan "$BACK" --db "$DB"

# Mount in the foreground in one terminal:
./target/debug/musefs mount "$MNT" --db "$DB" --template '$artist/$title'
# In another terminal: ls -R "$MNT"; play/inspect a file; then:
fusermount3 -u "$MNT"
```

Expected: the mounted tree reflects the template; files are readable and decode as valid FLAC; `st_size` matches the bytes read. This step is documentation for the human operator — it is not automated.

- [ ] **Step 5: Commit any cleanup**

```bash
git add -A
git commit -m "$(printf 'chore: M2 fuse+cli cleanup, no warnings\n\nCo-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>')" || echo "nothing to commit"
```

---

## Self-Review Notes

- **Spec coverage (M2 fuse+cli scope):**
  - `musefs-fuse` `fuser::Filesystem` impl — lookup/getattr/readdir/read (spec "FUSE serving layer"): Task 5. `open`/`release` use fuser's defaults (read-only, per-read resolution against the core's content-version-keyed cache); per-fd snapshot pinning is documented as deferred (M2-core's cache already validates the backing file every resolve).
  - `getattr` reports `st_size` from the measured total length and `mtime` from `max(backing_mtime, updated_at)` (computed in `musefs-core`); dirs `0555`, files `0444`: Task 4 + Task 5.
  - `read` walks segments splicing inline framing + positioned backing reads — implemented in `musefs-core::read_at` (M2-core) and surfaced here.
  - CLI `scan` and `mount` with `--db`, `--template` (spec "CLI surface"): Tasks 7–8.
  - End-to-end FLAC read through a real mount (spec "M2 — Read-only FLAC mount", "FUSE integration tests gated"): Task 5 (`#[ignore]`) + Task 9.
- **Correctly deferred (other milestones):** `--mode structure-only` and passthrough reads (M5); `musefs refresh` + SIGHUP + `data_version` polling (M5); `--revalidate` (M5); TOML `--config` loading (M5 polish; flags suffice for MVP); `--foreground`/daemonization (this plan's `mount` always runs in the foreground — `mount2` blocks until unmount; backgrounding is left to the shell for MVP); MP3 (M3); embedded art in reads (M4); multithreaded mount + `arc-swap` tree + SQLite pooling (the spec's concurrency design — M2-core deliberately chose single-threaded `&mut self`, which fuser's default session loop satisfies; revisit if profiling demands it).
- **Type consistency:** uses the M2-core public API verbatim — `Musefs::{open, refresh, lookup, getattr, readdir, read, parent}` (Task 1 adds `parent`), `Attr { inode, is_dir, size, mtime_secs }`, `MountConfig { template, fallbacks, default_fallback }`, `CoreError` (8 variants, matched exhaustively in `errno`), `scan_directory(&Db, &Path) -> Result<ScanStats>`, `ScanStats { scanned, skipped }`, `Db::open(path)`, `Db::list_tracks`. fuser 0.14 signatures verified against docs: `&mut self`, plain `u64` inodes, `offset: i64`, `flags: i32`, `lock_owner: Option<u64>`; `ReplyEntry::entry(&TTL, &FileAttr, gen)`, `ReplyAttr::attr(&TTL, &FileAttr)`, `ReplyData::data(&[u8])`, `ReplyDirectory::add(ino, next_offset, FileType, name) -> bool` then `ok()`, `Reply*::error(i32)`; `mount2`/`spawn_mount2(fs, mountpoint, &[MountOption])`.
- **Placeholder discipline:** the only intentional stubs are the Task 2/Task 6 module docs and the temporary `main.rs` (Task 6 → replaced in Task 8). Every code step ships complete, compilable code.
- **Platform note:** Linux + libfuse3 (3.18 installed); `/dev/fuse` present and world-accessible; `fusermount3` available for unmount. `fuser = "0.14"` with default features links libfuse3 via pkg-config.
- **errno nuance:** `CoreError::Io` forwards the underlying OS errno when present (so a missing backing file surfaces `ENOENT`, not a blanket `EIO`); structural errors collapse to `EIO`, matching the spec's "backing changed → EIO".
```
