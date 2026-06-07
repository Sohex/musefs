# FreeBSD/macOS Support — Plan A: Platform Module, Passthrough Gating, Spotlight Marker, CI

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make musefs build and run on FreeBSD (real e2e) and macOS (best-effort compile + unit) by centralizing all per-OS behavior in a new `musefs-fuse/src/platform/` module, gating Linux-only passthrough, adding macOS mount options and a `.metadata_never_index` Spotlight marker, and wiring FreeBSD/macOS CI.

**Architecture:** A new `platform` submodule owns every `#[cfg(target_os = ...)]` branch. The `Filesystem` handlers in `lib.rs` call OS-agnostic functions whose stubs compile to no-ops/`None` off-target, so handler bodies stay `#[cfg]`-free. This plan is the compile-time platform axis only; the runtime `case_insensitive` flag and tree case-folding are Plan B and are NOT touched here.

**Tech Stack:** Rust, `fuser` 0.17, `libc`, GitHub Actions (`vmactions/freebsd-vm`, `macos-latest`).

**Spec:** `docs/superpowers/specs/2026-06-07-freebsd-macos-support-design.md`

---

## File Structure

- Create: `musefs-fuse/src/platform/mod.rs` — module root; declares + re-exports the three submodules.
- Create: `musefs-fuse/src/platform/mount.rs` — per-OS `MountOption` list builder.
- Create: `musefs-fuse/src/platform/passthrough.rs` — Linux passthrough state/logic; no-op stubs elsewhere. Owns the `CAP_SYS_ADMIN` probe.
- Create: `musefs-fuse/src/platform/spotlight.rs` — macOS `.metadata_never_index` marker; `None`/`false` stubs elsewhere.
- Modify: `musefs-fuse/src/lib.rs` — declare `mod platform`; refactor `init`/`open`/`release`/`read`/`lookup`/`getattr`/`readdir`/`mount_config`/`MusefsFs`/`MusefsFs::new`; trim imports; relocate the cap-parser tests.
- Modify: `musefs-fuse/Cargo.toml` — enable `fuser`'s `macos-no-mount` feature on macOS targets.
- Modify: `.github/workflows/ci.yml` — add `macos` and `freebsd` jobs; add both to `ci-ok`'s `needs:`.
- Modify: `.gitignore` — ignore `/.scratch/`.
- Modify: `CONTRIBUTING.md` — document the local FreeBSD VM workflow.

---

## Task 1: Platform module skeleton + per-OS mount options

**Files:**
- Create: `musefs-fuse/src/platform/mod.rs`
- Create: `musefs-fuse/src/platform/mount.rs`
- Modify: `musefs-fuse/src/lib.rs` (add `mod platform;`, rewrite `mount_config`)

- [ ] **Step 1: Create the module root**

Create `musefs-fuse/src/platform/mod.rs`:

```rust
//! Per-OS behavior for the FUSE adapter. Every `#[cfg(target_os = ...)]` branch
//! in this crate lives under this module, so the `Filesystem` handlers in
//! `lib.rs` stay platform-agnostic: they call functions whose stubs compile to
//! no-ops or `None` on the wrong OS.

pub mod mount;
pub mod passthrough;
pub mod spotlight;
```

- [ ] **Step 2: Write the failing test for mount options**

Create `musefs-fuse/src/platform/mount.rs`:

```rust
//! Per-OS FUSE mount options. The common set (read-only, filesystem name) is
//! shared; macOS adds a volume name and suppresses AppleDouble sidecar noise.

use fuser::MountOption;

/// Read-only mount options for `fs_name`, plus any per-OS additions.
pub fn options(fs_name: &str) -> Vec<MountOption> {
    let mut opts = vec![
        MountOption::RO,
        MountOption::FSName(fs_name.to_string()),
    ];
    extend_os_specific(&mut opts, fs_name);
    opts
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn options_are_always_read_only_and_named() {
        let opts = options("musefs");
        assert!(opts.contains(&MountOption::RO));
        assert!(opts.contains(&MountOption::FSName("musefs".to_string())));
    }
}
```

- [ ] **Step 3: Run the test to verify it fails**

Run: `cargo test -p musefs-fuse --lib platform::mount`
Expected: FAIL — `cannot find function extend_os_specific` (compile error). (`mod platform;` is not declared yet, so this also won't resolve until Step 5; if so, do Steps 4–5 first, then re-run.)

- [ ] **Step 4: Add the OS-specific helpers**

Append to `musefs-fuse/src/platform/mount.rs`, above the `#[cfg(test)]` module:

```rust
#[cfg(target_os = "macos")]
fn extend_os_specific(opts: &mut Vec<MountOption>, fs_name: &str) {
    // fuser 0.17 has no `VolName` variant; macOS-specific options go through
    // CUSTOM. `noappledouble` stops Finder writing ._ sidecar files (which a
    // read-only fs would reject noisily). Best-effort/tunable: FUSE-T's option
    // set differs from macFUSE.
    opts.push(MountOption::CUSTOM(format!("volname={fs_name}")));
    opts.push(MountOption::CUSTOM("noappledouble".to_string()));
}

#[cfg(not(target_os = "macos"))]
fn extend_os_specific(_opts: &mut Vec<MountOption>, _fs_name: &str) {}

#[cfg(all(test, target_os = "macos"))]
mod macos_tests {
    use super::*;

    #[test]
    fn macos_adds_volname_and_noappledouble() {
        let opts = options("musefs");
        assert!(opts.contains(&MountOption::CUSTOM("volname=musefs".to_string())));
        assert!(opts.contains(&MountOption::CUSTOM("noappledouble".to_string())));
    }
}
```

- [ ] **Step 5: Declare the module and rewrite `mount_config`**

In `musefs-fuse/src/lib.rs`, add the module declaration after the imports (after line 25, the `use std::num::NonZeroU64;` line):

```rust
mod platform;
```

Then replace the body of `mount_config` (currently `lib.rs:486-494`):

```rust
/// Read-only mount options tagged with the filesystem name, plus per-OS extras.
fn mount_config(fs_name: &str) -> Config {
    let mut cfg = Config::default();
    cfg.mount_options = platform::mount::options(fs_name);
    cfg
}
```

- [ ] **Step 6: Run tests to verify they pass**

Run: `cargo test -p musefs-fuse --lib platform::mount`
Expected: PASS (`options_are_always_read_only_and_named`).

Run: `cargo clippy -p musefs-fuse --all-targets -- -D warnings`
Expected: clean.

- [ ] **Step 7: Commit**

```bash
git add musefs-fuse/src/platform/mod.rs musefs-fuse/src/platform/mount.rs musefs-fuse/src/lib.rs
git commit -m "feat(fuse): platform module + per-OS mount options"
```

---

## Task 2: Passthrough gating — move Linux-only logic behind `platform::passthrough`

This is a behavior-preserving refactor of tested code. The existing Linux e2e
suite (`musefs-fuse/tests/passthrough.rs`, `--ignored --features metrics`) and
the cap-parser unit tests are the safety net; the unit tests move into the new
module. On Linux nothing observable changes; on non-Linux the passthrough path
becomes a compile-time no-op that always serves through the daemon.

**Files:**
- Create: `musefs-fuse/src/platform/passthrough.rs`
- Modify: `musefs-fuse/src/lib.rs` (`MusefsFs` struct + `new` + `init` + `open` + `release`; trim imports; remove the moved functions and their tests)

- [ ] **Step 1: Write the passthrough module with the relocated cap-parser tests**

Create `musefs-fuse/src/platform/passthrough.rs`:

```rust
//! Kernel FUSE passthrough is Linux-only (Linux 6.9+). On Linux this registers
//! the backing fd with the kernel so reads bypass the daemon; on every other OS
//! the entire path is a no-op and reads are served through the daemon.
//!
//! Each `imp` module carries its own `use` lines; the public surface is the
//! `pub use` re-export at the bottom, so there are no top-level imports here.

#[cfg(target_os = "linux")]
mod imp {
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex, PoisonError};

    use fuser::{BackingId, FileHandle, FopenFlags, InitFlags, KernelConfig, ReplyOpen};
    use musefs_core::{Fh, Musefs};

    /// Live passthrough state: kernel-registered backing fds keyed by wire fh,
    /// plus a sticky disable flag flipped on the first `open_backing` failure
    /// (kernel < 6.9 / ioctl unsupported) so later opens skip the doomed ioctl.
    #[derive(Clone)]
    pub struct PassthroughState {
        backing: Arc<Mutex<HashMap<u64, BackingId>>>,
        disabled: Arc<AtomicBool>,
    }

    impl PassthroughState {
        pub fn new(structure_only: bool) -> PassthroughState {
            let disabled = structure_only && definitely_lacks_cap_sys_admin();
            if disabled {
                log::info!(
                    "StructureOnly mount without CAP_SYS_ADMIN: kernel passthrough unavailable; reads will be served by the daemon"
                );
            }
            PassthroughState {
                backing: Arc::new(Mutex::new(HashMap::new())),
                disabled: Arc::new(AtomicBool::new(disabled)),
            }
        }

        /// Drop the backing registration for `fh` (fires the backing-close ioctl
        /// via `BackingId`'s Drop). A no-op for plain handles not in the map.
        pub fn remove(&self, fh: u64) {
            self.backing
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .remove(&fh);
        }
    }

    /// Reply to `open`: try kernel passthrough, else serve through the daemon.
    pub fn reply_open(
        pt: &PassthroughState,
        core: &Musefs,
        fh: Fh,
        reply: ReplyOpen,
        plain_flags: FopenFlags,
    ) {
        if !pt.disabled.load(Ordering::Relaxed) {
            if let Some(pfd) = core.passthrough_fd(fh) {
                match reply.open_backing(&pfd) {
                    Ok(id) => {
                        // Insert before the reply: the kernel cannot release an
                        // fh it has not yet seen. FOPEN_KEEP_CACHE is dropped —
                        // page-cache ownership belongs to the backing inode here.
                        let mut map = pt
                            .backing
                            .lock()
                            .unwrap_or_else(PoisonError::into_inner);
                        let id = map.entry(fh.get()).insert_entry(id).into_mut();
                        return reply.opened_passthrough(
                            FileHandle(fh.get()),
                            FopenFlags::empty(),
                            id,
                        );
                    }
                    Err(e) => {
                        pt.disabled.store(true, Ordering::Relaxed);
                        log::info!(
                            "FUSE passthrough unavailable; serving reads through the daemon: {e}"
                        );
                    }
                }
            }
        }
        reply.opened(FileHandle(fh.get()), plain_flags);
    }

    /// Request the passthrough capability + stack depth during `init`.
    pub fn request_capabilities(config: &mut KernelConfig) {
        // Both calls are required: fuser only copies max_stack_depth into the
        // init reply when FUSE_PASSTHROUGH negotiated; depth 0 disables it.
        // Depth 2 (kernel max) lets backing files live on a stacked fs.
        let _ = config.add_capabilities(InitFlags::FUSE_PASSTHROUGH);
        let _ = config.set_max_stack_depth(2);
    }

    /// True only when /proc/self/status definitively shows CAP_SYS_ADMIN absent.
    /// Unreadable/unparseable -> false (stay neutral; the first open decides).
    fn definitely_lacks_cap_sys_admin() -> bool {
        std::fs::read_to_string("/proc/self/status")
            .ok()
            .and_then(|s| cap_eff_has_sys_admin(&s))
            .is_some_and(|has| !has)
    }

    /// Parse the `CapEff:` line; None when absent or malformed.
    fn cap_eff_has_sys_admin(status: &str) -> Option<bool> {
        const CAP_SYS_ADMIN_BIT: u32 = 21;
        let hex = status
            .lines()
            .find_map(|l| l.strip_prefix("CapEff:"))?
            .trim();
        let mask = u64::from_str_radix(hex, 16).ok()?;
        Some(mask & (1 << CAP_SYS_ADMIN_BIT) != 0)
    }

    #[cfg(test)]
    mod tests {
        use super::cap_eff_has_sys_admin;

        #[test]
        fn cap_eff_parser_root_mask_has_sys_admin() {
            assert_eq!(
                cap_eff_has_sys_admin("CapPrm:\t0000003fffffffff\nCapEff:\t0000003fffffffff\n"),
                Some(true)
            );
        }

        #[test]
        fn cap_eff_parser_zero_mask_lacks_sys_admin() {
            assert_eq!(
                cap_eff_has_sys_admin("CapEff:\t0000000000000000\n"),
                Some(false)
            );
        }

        #[test]
        fn cap_eff_parser_missing_line_returns_none() {
            assert_eq!(cap_eff_has_sys_admin("Name:\tfoo\nUid:\t1000\n"), None);
        }

        #[test]
        fn cap_eff_parser_garbage_hex_returns_none() {
            assert_eq!(cap_eff_has_sys_admin("CapEff:\tnothex\n"), None);
        }
    }
}

#[cfg(not(target_os = "linux"))]
mod imp {
    use fuser::{FileHandle, FopenFlags, KernelConfig, ReplyOpen};
    use musefs_core::{Fh, Musefs};

    /// Off Linux there is no kernel passthrough; this carries no state.
    #[derive(Clone)]
    pub struct PassthroughState;

    impl PassthroughState {
        pub fn new(structure_only: bool) -> PassthroughState {
            if structure_only {
                log::info!(
                    "StructureOnly mount: kernel passthrough is Linux-only; reads will be served by the daemon"
                );
            }
            PassthroughState
        }

        pub fn remove(&self, _fh: u64) {}
    }

    /// Always serve through the daemon — no passthrough on this OS.
    pub fn reply_open(
        _pt: &PassthroughState,
        _core: &Musefs,
        fh: Fh,
        reply: ReplyOpen,
        plain_flags: FopenFlags,
    ) {
        reply.opened(FileHandle(fh.get()), plain_flags);
    }

    /// No passthrough capability to request off Linux.
    pub fn request_capabilities(_config: &mut KernelConfig) {}
}

pub use imp::{request_capabilities, reply_open, PassthroughState};
```

Only the active OS's `imp` module compiles, so exactly one set of `use` lines is live and there are no unused-import warnings. The `pub use` re-export gives `lib.rs` the same three names (`PassthroughState`, `reply_open`, `request_capabilities`) on every OS.

- [ ] **Step 2: Run the relocated cap-parser tests (Linux)**

Run: `cargo test -p musefs-fuse --lib platform::passthrough`
Expected: PASS — 4 cap-parser tests. (On a non-Linux host these are cfg'd out; the module still compiles.)

- [ ] **Step 3: Refactor `MusefsFs` struct to hold `PassthroughState`**

In `musefs-fuse/src/lib.rs`, replace the `backing` and `passthrough_disabled` fields (currently `lib.rs:191-199`) of `struct MusefsFs` with a single field:

```rust
    /// Per-OS kernel-passthrough state (live backing registrations + sticky
    /// disable on Linux; a no-op marker elsewhere).
    passthrough: platform::passthrough::PassthroughState,
```

So the struct's tail now reads:

```rust
    poll_pending: Arc<AtomicBool>,
    passthrough: platform::passthrough::PassthroughState,
}
```

- [ ] **Step 4: Refactor `MusefsFs::new`**

Replace `MusefsFs::new` (currently `lib.rs:203-230`) with:

```rust
    pub fn new(core: Musefs, config: FuseConfig) -> MusefsFs {
        // Work is I/O-bound (especially on NFS), so oversize the pool vs CPUs.
        let workers = std::thread::available_parallelism().map_or(4, std::num::NonZero::get) * 2;
        let passthrough = platform::passthrough::PassthroughState::new(
            core.mode() == musefs_core::Mode::StructureOnly,
        );
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
            notifier: Arc::new(OnceLock::new()),
            poll_pending: Arc::new(AtomicBool::new(false)),
            passthrough,
        }
    }
```

- [ ] **Step 5: Refactor `init` to delegate capability requests**

Replace the passthrough lines in `init` (currently `lib.rs:293-300`, the comment block plus the two `add_capabilities(InitFlags::FUSE_PASSTHROUGH)` / `set_max_stack_depth` calls) with a single call. The full `init` becomes:

```rust
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
```

NOTE: `InitFlags` is still used here (`FUSE_ASYNC_READ`, `FUSE_PARALLEL_DIROPS`), so keep it imported.

- [ ] **Step 6: Refactor `open` to delegate the passthrough decision**

Replace `open` (currently `lib.rs:332-374`) with:

```rust
    fn open(&self, _req: &Request, ino: INodeNo, _flags: OpenFlags, reply: ReplyOpen) {
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
```

- [ ] **Step 7: Refactor `release` to use `passthrough.remove`**

Replace `release` (currently `lib.rs:376-397`) with:

```rust
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
        // Cheap (a backing-map remove + a slab remove); no need to offload.
        if let Some(fh) = NonZeroU64::new(fh.0) {
            // Drops the backing registration (fires the close ioctl on Linux);
            // a no-op for plain handles and on non-Linux.
            self.passthrough.remove(fh.get());
            self.core.release_handle(Fh::from(fh));
        }
        reply.ok();
    }
```

- [ ] **Step 8: Remove the moved functions and tests from `lib.rs`**

Delete from `musefs-fuse/src/lib.rs`:
- `definitely_lacks_cap_sys_admin` (currently `lib.rs:76-84`) and `cap_eff_has_sys_admin` (currently `lib.rs:86-95`) — both moved to `platform::passthrough`.
- In the `#[cfg(test)] mod tests` block, delete the four `cap_eff_parser_*` tests (currently `lib.rs:682-707`) — moved to `platform::passthrough`.

- [ ] **Step 9: Trim `lib.rs` imports**

In the `use fuser::{...}` block (currently `lib.rs:15-19`) remove `BackingId` and `InitFlags`... wait: `InitFlags` is still used by `init`. Remove only `BackingId`. The block becomes:

```rust
use fuser::{
    BackgroundSession, Config, FileAttr, FileHandle, FileType, Filesystem, FopenFlags,
    Generation, INodeNo, InitFlags, KernelConfig, LockOwner, Notifier, OpenFlags, ReplyAttr,
    ReplyData, ReplyDirectory, ReplyEmpty, ReplyEntry, ReplyOpen, Request, Session,
};
```

Also remove `use std::collections::HashMap;` (currently `lib.rs:6`) — the map now lives in `platform::passthrough`.

- [ ] **Step 10: Build, lint, and run the full suite**

Run: `cargo clippy -p musefs-fuse --all-targets -- -D warnings`
Expected: clean. (If unused-import warnings point at the top of `passthrough.rs`, apply the cleanup from Task 2 Step 1's NOTE.)

Run: `cargo test --workspace`
Expected: PASS — all existing tests green; the relocated cap-parser tests now run under `platform::passthrough`.

- [ ] **Step 11: Verify Linux passthrough e2e still works**

Run (needs `/dev/fuse` + libfuse; requires sudo for the backing ioctl per the project's passthrough e2e):
`cargo test -p musefs-fuse --features metrics --test passthrough -- --ignored --nocapture --test-threads=1`
Expected: PASS — the StructureOnly zero-pread assertion still holds (proves the refactor preserved kernel passthrough). If you cannot run sudo e2e in this environment, note it and rely on Step 10.

- [ ] **Step 12: Commit**

```bash
git add musefs-fuse/src/platform/passthrough.rs musefs-fuse/src/lib.rs
git commit -m "refactor(fuse): move Linux-only passthrough behind platform::passthrough"
```

---

## Task 3: macOS `.metadata_never_index` Spotlight marker

A zero-byte read-only regular file at the mount root, present only on macOS, so
Spotlight does not try to index the mount. Handlers consult `platform::spotlight`
helpers that return `None`/`false` off macOS, keeping handler bodies `#[cfg]`-free.

**Files:**
- Create: `musefs-fuse/src/platform/spotlight.rs`
- Modify: `musefs-fuse/src/lib.rs` (`lookup`, `getattr`, `readdir`, `open`, `read`)

- [ ] **Step 1: Write the spotlight module with cross-OS tests**

Create `musefs-fuse/src/platform/spotlight.rs`:

```rust
//! macOS Spotlight hygiene: present a zero-byte `.metadata_never_index` file at
//! the mount root so `mds`/Spotlight skips the volume. macOS-only; on every
//! other OS the marker does not exist and these helpers report absence.

use std::time::SystemTime;

use fuser::{FileAttr, FileType, INodeNo};

/// Mount root inode (fuser's FUSE root id). The marker is a child of the root.
const ROOT_INO: u64 = 1;

/// Marker filename Spotlight recognizes.
pub const MARKER_NAME: &str = ".metadata_never_index";

/// Reserved sentinel inode for the marker. `InodeAllocator` starts at 2 and only
/// ever increments with no upper bound, so `u64::MAX` is unreachable in practice
/// and cannot collide with a real node. (A fixed "high" constant would NOT be
/// safe — there is no allocator ceiling to sit above.)
pub const MARKER_INO: u64 = u64::MAX;

/// The marker's attributes: a zero-byte, read-only regular file owned by the
/// mount, all timestamps set to `mtime` (matching synthetic-node stamping).
pub fn marker_attr(uid: u32, gid: u32, mtime: SystemTime) -> FileAttr {
    FileAttr {
        ino: INodeNo(MARKER_INO),
        size: 0,
        blocks: 0,
        atime: mtime,
        mtime,
        ctime: mtime,
        crtime: mtime,
        kind: FileType::RegularFile,
        perm: 0o444,
        nlink: 1,
        uid,
        gid,
        rdev: 0,
        blksize: 512,
        flags: 0,
    }
}

/// Marker inode if `(parent, name)` addresses it; `None` otherwise (always `None`
/// off macOS).
#[cfg(target_os = "macos")]
pub fn marker_lookup(parent: u64, name: &str) -> Option<u64> {
    (parent == ROOT_INO && name == MARKER_NAME).then_some(MARKER_INO)
}

/// True if `ino` is the marker (always `false` off macOS).
#[cfg(target_os = "macos")]
pub fn is_marker(ino: u64) -> bool {
    ino == MARKER_INO
}

/// The readdir entry to append when listing `dir_ino` (only the root, only on
/// macOS); `None` otherwise.
#[cfg(target_os = "macos")]
pub fn marker_dir_entry(dir_ino: u64) -> Option<(u64, FileType, String)> {
    (dir_ino == ROOT_INO).then(|| (MARKER_INO, FileType::RegularFile, MARKER_NAME.to_string()))
}

#[cfg(not(target_os = "macos"))]
pub fn marker_lookup(_parent: u64, _name: &str) -> Option<u64> {
    None
}

#[cfg(not(target_os = "macos"))]
pub fn is_marker(_ino: u64) -> bool {
    false
}

#[cfg(not(target_os = "macos"))]
pub fn marker_dir_entry(_dir_ino: u64) -> Option<(u64, FileType, String)> {
    None
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[test]
    fn marker_attr_is_zero_byte_read_only_file() {
        let mt = SystemTime::UNIX_EPOCH + Duration::from_secs(1000);
        let a = marker_attr(501, 20, mt);
        assert_eq!(a.ino, INodeNo(u64::MAX));
        assert_eq!(a.kind, FileType::RegularFile);
        assert_eq!(a.perm, 0o444);
        assert_eq!(a.size, 0);
        assert_eq!(a.nlink, 1);
        assert_eq!(a.uid, 501);
        assert_eq!(a.gid, 20);
        assert_eq!(a.mtime, mt);
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn marker_is_absent_off_macos() {
        assert_eq!(marker_lookup(1, MARKER_NAME), None);
        assert!(!is_marker(MARKER_INO));
        assert_eq!(marker_dir_entry(1), None);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn marker_is_present_on_macos() {
        assert_eq!(marker_lookup(1, MARKER_NAME), Some(MARKER_INO));
        assert_eq!(marker_lookup(2, MARKER_NAME), None); // not under non-root
        assert_eq!(marker_lookup(1, "other"), None);
        assert!(is_marker(MARKER_INO));
        assert_eq!(
            marker_dir_entry(1),
            Some((MARKER_INO, FileType::RegularFile, MARKER_NAME.to_string()))
        );
        assert_eq!(marker_dir_entry(2), None);
    }
}
```

- [ ] **Step 2: Run the spotlight tests**

Run: `cargo test -p musefs-fuse --lib platform::spotlight`
Expected: PASS — `marker_attr_is_zero_byte_read_only_file` and (on Linux) `marker_is_absent_off_macos`.

- [ ] **Step 3: Intercept the marker in `lookup`**

In `musefs-fuse/src/lib.rs`, in `lookup`, after the `name.to_str()` guard and before `self.core.lookup(...)` (currently inserting after `lib.rs:308`), add:

```rust
        if platform::spotlight::marker_lookup(parent.0, name).is_some() {
            let attr =
                platform::spotlight::marker_attr(self.uid, self.gid, self.mount_time);
            return reply.entry(&self.config.ttl, &attr, Generation(0));
        }
```

- [ ] **Step 4: Intercept the marker in `getattr`**

In `getattr`, after `self.fire_poll_refresh();` (currently `lib.rs:323`), add:

```rust
        if platform::spotlight::is_marker(ino.0) {
            let attr =
                platform::spotlight::marker_attr(self.uid, self.gid, self.mount_time);
            return reply.attr(&self.config.ttl, &attr);
        }
```

- [ ] **Step 5: Intercept the marker in `open`**

In `open`, as the very first statement (before cloning `core`), add:

```rust
        if platform::spotlight::is_marker(ino.0) {
            // Stateless empty file: fh 0 means `release` skips it (its
            // NonZeroU64 guard) and `read` short-circuits on `is_marker`.
            return reply.opened(FileHandle(0), open_flags(false));
        }
```

- [ ] **Step 6: Intercept the marker in `read`**

In `read`, as the very first statement (before cloning `core`), add:

```rust
        if platform::spotlight::is_marker(ino.0) {
            return reply.data(&[]);
        }
```

- [ ] **Step 7: Append the marker entry in `readdir`**

In `readdir`, after the `for (name, child, is_dir) in entries { ... }` loop that fills `listing` (currently ends at `lib.rs:474`) and before the `for (i, (child, kind, name)) ...` emit loop, add:

```rust
        if let Some(entry) = platform::spotlight::marker_dir_entry(ino.0) {
            listing.push(entry);
        }
```

- [ ] **Step 8: Build, lint, and test**

Run: `cargo clippy -p musefs-fuse --all-targets -- -D warnings`
Expected: clean.

Run: `cargo test --workspace`
Expected: PASS. (On Linux, marker helpers are inert, so existing e2e/unit behavior is unchanged.)

- [ ] **Step 9: Verify the marker is absent on Linux through a real mount (optional, Linux)**

Add this `#[ignore]` e2e test to `musefs-fuse/tests/mount.rs` (it documents that the marker does NOT leak onto Linux; on macOS the inverse would be asserted, but we cannot mount there in CI):

```rust
#[test]
#[ignore = "requires /dev/fuse; run with: cargo test -p musefs-fuse -- --ignored"]
fn metadata_never_index_marker_absent_on_linux() {
    let backing = tempfile::tempdir().unwrap();
    let flac = make_flac(&["ARTIST=Alice", "TITLE=Song"], &[0xAB; 64]);
    std::fs::write(backing.path().join("a.flac"), &flac).unwrap();
    let db = musefs_db::Db::open_in_memory().unwrap();
    scan_directory(&db, backing.path()).unwrap();
    let fs = Musefs::open(db, config()).unwrap();

    let mountpoint = tempfile::tempdir().unwrap();
    let session = musefs_fuse::spawn(fs, mountpoint.path(), "musefs-marker").unwrap();

    // The marker must NOT exist on Linux.
    assert!(!mountpoint.path().join(".metadata_never_index").exists());
    let root: Vec<String> = std::fs::read_dir(mountpoint.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    assert!(!root.contains(&".metadata_never_index".to_string()));

    drop(session);
    drop(backing);
}
```

Run (if `/dev/fuse` available): `cargo test -p musefs-fuse --test mount -- --ignored metadata_never_index_marker_absent_on_linux`
Expected: PASS.

- [ ] **Step 10: Commit**

```bash
git add musefs-fuse/src/platform/spotlight.rs musefs-fuse/src/lib.rs musefs-fuse/tests/mount.rs
git commit -m "feat(fuse): macOS .metadata_never_index Spotlight marker"
```

---

## Task 4: macOS build feature + macOS CI job

`fuser` 0.17's `build.rs` `pkg-config`-probes for macFUSE on macOS and fails the
build unless the `macos-no-mount` feature is enabled. We enable it only for macOS
targets, giving a compile-and-unit-test build with mounting compiled out.

**Files:**
- Modify: `musefs-fuse/Cargo.toml`
- Modify: `.github/workflows/ci.yml`

- [ ] **Step 1: Enable `macos-no-mount` for macOS targets**

In `musefs-fuse/Cargo.toml`, after the `[dependencies]` block (which keeps
`fuser = "0.17"` as-is for Linux/FreeBSD), add a target-specific dependency that
turns on the feature only when building for macOS:

```toml
[target.'cfg(target_os = "macos")'.dependencies]
# A stock macOS build fails in fuser's build.rs (it pkg-config-probes for
# macFUSE). `macos-no-mount` compiles mounting out for a best-effort
# compile + unit-test build; real mounting on macOS (FUSE-T) is future work.
fuser = { version = "0.17", features = ["macos-no-mount"] }
```

- [ ] **Step 2: Verify the manifest parses**

Run: `cargo metadata --format-version 1 >/dev/null`
Expected: exit 0 (manifest is valid TOML and resolves). If `cargo` reports that
`fuser` has no feature `macos-no-mount`, STOP and consult the spec's open
question — confirm the exact feature name in fuser 0.17's `Cargo.toml`
(`cargo info fuser@0.17` or the docs.rs feature list) and use that name.

- [ ] **Step 3: Add the macOS CI job**

In `.github/workflows/ci.yml`, after the `e2e` job (ends at line 199) and before
the `ci-ok` job, add:

```yaml
  macos:
    # Best-effort macOS build: compiles with fuser's macos-no-mount feature
    # (enabled via a target-specific dependency); no mount step — macFUSE/FUSE-T
    # are not CI-friendly. The #[ignore]d mount e2e tests are skipped here.
    needs: changes
    if: needs.changes.outputs.src == 'true'
    runs-on: macos-latest
    steps:
      - uses: actions/checkout@df4cb1c069e1874edd31b4311f1884172cec0e10
        with:
          persist-credentials: false
      - uses: dtolnay/rust-toolchain@29eef336d9b2848a0b548edc03f92a220660cdb8
        with:
          components: rustfmt, clippy
      - uses: Swatinem/rust-cache@e18b497796c12c097a38f9edb9d0641fb99eee32
      - name: Clippy
        run: cargo clippy --all-targets -- -D warnings
      - name: Test (no mount; ignored mount tests are skipped)
        run: cargo test --workspace
```

- [ ] **Step 4: Verify the workflow is valid YAML**

Run: `python3 -c "import yaml,sys; yaml.safe_load(open('.github/workflows/ci.yml')); print('ok')"`
Expected: `ok`.

- [ ] **Step 5: Commit**

```bash
git add musefs-fuse/Cargo.toml .github/workflows/ci.yml
git commit -m "ci: best-effort macOS build (fuser macos-no-mount) + macOS job"
```

---

## Task 5: FreeBSD CI job + `ci-ok` wiring + `.gitignore`

**Files:**
- Modify: `.github/workflows/ci.yml`
- Modify: `.gitignore`

- [ ] **Step 1: Ignore the local FreeBSD VM scratch dir**

In `.gitignore`, add under the existing entries (e.g. after the `/.claude/` line):

```
# Local scratch (e.g. FreeBSD VM image for e2e); never committed
/.scratch/
```

- [ ] **Step 2: Add the FreeBSD CI job**

In `.github/workflows/ci.yml`, after the `macos` job (added in Task 4) and before
`ci-ok`, add:

```yaml
  freebsd:
    # Real FreeBSD e2e in a VM. FreeBSD uses fuser's pure-rust mount backend
    # (talks to /dev/fuse directly; no libfuse link), so we only need the
    # fusefs kernel module loaded. Confirm package/module names against the
    # local .scratch VM before relying on this.
    needs: changes
    if: needs.changes.outputs.src == 'true'
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@df4cb1c069e1874edd31b4311f1884172cec0e10
        with:
          persist-credentials: false
      - name: Build + test in a FreeBSD VM
        uses: vmactions/freebsd-vm@966989c456d41351f095a421f60e71342d3bce41
        with:
          usesh: true
          prepare: |
            pkg install -y rust git
            kldload fusefs || true
          run: |
            cargo test --workspace
            cargo test -p musefs-fuse -- --ignored
```

NOTE: pin `vmactions/freebsd-vm` to the SHA of the version you intend to use
(the SHA above is a placeholder — replace it with the current release's commit
SHA, matching the project's convention of pinning every action to a full SHA).
If `cargo test -- --ignored` fails to mount, verify `fusefs` is loaded
(`kldstat | grep fusefs`) and that `mount_fusefs` (FreeBSD base system) is
present; adjust `prepare` accordingly.

- [ ] **Step 3: Wire both new jobs into the required-status aggregator**

In `.github/workflows/ci.yml`, update the `ci-ok` job's `needs:` (currently line
207) to include `macos` and `freebsd`:

```yaml
    needs: [changes, check, interop, python-musefs, beets, picard, e2e, macos, freebsd]
```

- [ ] **Step 4: Verify the workflow is valid YAML**

Run: `python3 -c "import yaml,sys; yaml.safe_load(open('.github/workflows/ci.yml')); print('ok')"`
Expected: `ok`.

- [ ] **Step 5: Commit**

```bash
git add .github/workflows/ci.yml .gitignore
git commit -m "ci: FreeBSD VM e2e job; wire macos+freebsd into ci-ok"
```

---

## Task 6: Document the local FreeBSD VM workflow

**Files:**
- Modify: `CONTRIBUTING.md`

- [ ] **Step 1: Find the test-tiers section**

Run: `grep -n -i "tier\|e2e\|/dev/fuse\|FUSE end-to-end\|## .*[Tt]est" CONTRIBUTING.md | head -30`
Expected: locate the section that documents the FUSE e2e tier (the `--ignored` tests).

- [ ] **Step 2: Add a short FreeBSD subsection**

Append a subsection to that area of `CONTRIBUTING.md` (adjust the heading level to
match the surrounding document):

```markdown
### FreeBSD e2e (local)

The FUSE e2e suite also runs on FreeBSD (CI uses a VM; see the `freebsd` job in
`.github/workflows/ci.yml`). For local runs, keep a small FreeBSD VM image under
the gitignored `/.scratch/` directory. Inside the VM, with the `fusefs` kernel
module loaded (`kldload fusefs`), run:

    cargo test --workspace
    cargo test -p musefs-fuse -- --ignored

macOS support is best-effort: it compiles (CI builds with fuser's
`macos-no-mount` feature) and its platform-specific logic is unit-tested, but
mounted e2e on macOS/FUSE-T is not yet validated.
```

- [ ] **Step 3: Verify markdown lints (ruff is for Python only; just sanity-check)**

Run: `git diff --stat CONTRIBUTING.md`
Expected: shows the addition. (No markdown linter is enforced by the pre-commit hook.)

- [ ] **Step 4: Commit**

```bash
git add CONTRIBUTING.md
git commit -m "docs: document local FreeBSD VM e2e workflow"
```

---

## Final verification

- [ ] **Step 1: Full workspace gate (mirrors the pre-commit hook)**

Run: `cargo fmt --all -- --check && cargo clippy --all-targets -- -D warnings && cargo test --workspace`
Expected: all green.

- [ ] **Step 2: Confirm no `#[cfg(target_os` leaked into handler bodies**

Run: `grep -n "cfg(target_os" musefs-fuse/src/lib.rs`
Expected: NO matches (all OS branching lives under `musefs-fuse/src/platform/`).

- [ ] **Step 3: Confirm the platform module owns the OS branching**

Run: `grep -rn "cfg(target_os" musefs-fuse/src/platform/`
Expected: matches in `mount.rs`, `passthrough.rs`, `spotlight.rs` only.
