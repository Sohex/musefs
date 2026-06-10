# Mount owner & permission flags Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add `--owner`, `--group`, `--file-mode`, `--dir-mode` flags to `musefs mount` so the mount can present a chosen owner and permission bits instead of the hardcoded process identity and `0o555`/`0o444`.

**Architecture:** `FuseConfig` (musefs-fuse) gains four fields carrying the presented attribute identity; `FuseConfig::default()` resolves the process uid/gid and the current default modes, so existing `mount`/`spawn` helpers and tests are unchanged. Name→id resolution and octal parsing live entirely in the CLI layer via clap value_parsers, so musefs-fuse only ever receives numeric ids and stays free of user-database lookups.

**Tech Stack:** Rust, clap (derive + value_parser), the `uzers` crate (Unix passwd/group lookup), `rustix::process` (getuid/getgid), `fuser`.

**Spec:** `docs/superpowers/specs/2026-06-10-mount-owner-perms-flags-design.md`

---

## File Structure

| File | Responsibility | Change |
| ---- | -------------- | ------ |
| `musefs-fuse/src/lib.rs` | `FuseConfig` struct + `Default`; `MusefsFs::new`; `lookup`/`getattr` closures | add fields, resolve defaults, read identity from config, thread modes |
| `musefs-fuse/src/convert.rs` | `to_file_attr` attribute rendering | take file/dir mode params instead of hardcoded constants |
| `musefs-fuse/src/platform/spotlight.rs` | macOS Spotlight marker attrs | `marker_attr` gains a `file_mode` param |
| `musefs-cli/src/lib.rs` | `MountArgs`, value_parsers, `parse_mount_config`, `run_mount` | new flags, parsers, wiring, write-bit warning |
| `musefs-cli/Cargo.toml` | CLI dependencies | add `uzers` |
| `README.md` | CLI flag docs | document the four flags |

Task 1 makes the structural musefs-fuse change (behavior identical — defaults reproduce today's output). Task 2 adds the CLI flags that override those defaults. Task 3 documents them. Each task is a single green commit (the pre-commit hook runs the full workspace test suite, so every commit must compile and pass).

---

## Task 1: FuseConfig carries attribute identity (musefs-fuse)

After this task, behavior is byte-for-byte identical to today, but `FuseConfig` holds uid/gid/file_mode/dir_mode and `to_file_attr`/`marker_attr` take the modes as parameters. No CLI flags yet.

**Files:**
- Modify: `musefs-fuse/src/lib.rs` (`FuseConfig` ~37-67, `MusefsFs::new` ~162-183, `lookup`/`getattr` closures ~266-285, test ~556-562)
- Modify: `musefs-fuse/src/convert.rs` (`to_file_attr` 15-54, test 110-144)
- Modify: `musefs-fuse/src/platform/spotlight.rs` (`marker_attr` 25-43, test 86-98)
- Modify: `musefs-cli/src/lib.rs` (`parse_mount_config` struct literal 199-204) — needed so the workspace still compiles after the field addition

- [ ] **Step 1: Extend the failing tests for the new defaults and mode params**

In `musefs-fuse/src/lib.rs`, extend `fuse_config_default_is_conservative` (currently lines 556-562) to assert the four new fields:

```rust
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
```

In `musefs-fuse/src/convert.rs`, update the existing `converts_dir_and_file_attrs` test calls to pass modes, and add a new swap test that kills the dir/file-mode swap mutant. Replace the test body's two `to_file_attr` calls (lines 120 and 135) so they read:

```rust
        let fa = to_file_attr(&dir, 501, 20, 0o444, 0o555, fallback);
```
```rust
        let fa = to_file_attr(&file, 501, 20, 0o444, 0o555, fallback);
```

Then add this new test immediately after `converts_dir_and_file_attrs` (after line 144):

```rust
    #[test]
    fn to_file_attr_applies_distinct_dir_and_file_modes() {
        let fallback = SystemTime::UNIX_EPOCH + Duration::from_secs(1000);
        let dir = Attr { inode: 1, is_dir: true, size: 0, mtime_secs: 0 };
        let file = Attr { inode: 2, is_dir: false, size: 0, mtime_secs: 0 };
        // Deliberately asymmetric so a dir/file swap is observable.
        let d = to_file_attr(&dir, 0, 0, 0o400, 0o700, fallback);
        let f = to_file_attr(&file, 0, 0, 0o400, 0o700, fallback);
        assert_eq!(d.perm, 0o700, "dir must get dir_mode");
        assert_eq!(f.perm, 0o400, "file must get file_mode");
    }
```

In `musefs-fuse/src/platform/spotlight.rs`, update `marker_attr_is_zero_byte_read_only_file` (lines 86-98) to pass and assert a `file_mode`:

```rust
    #[test]
    fn marker_attr_is_zero_byte_read_only_file() {
        let mt = SystemTime::UNIX_EPOCH + Duration::from_secs(1000);
        let a = marker_attr(501, 20, 0o444, mt);
        assert_eq!(a.ino, INodeNo(u64::MAX));
        assert_eq!(a.kind, FileType::RegularFile);
        assert_eq!(a.perm, 0o444);
        assert_eq!(a.size, 0);
        assert_eq!(a.nlink, 1);
        assert_eq!(a.uid, 501);
        assert_eq!(a.gid, 20);
        assert_eq!(a.mtime, mt);
    }
```

- [ ] **Step 2: Run the tests to verify they fail to compile**

Run: `cargo test -p musefs-fuse 2>&1 | head -30`
Expected: compile error — `FuseConfig` has no field `file_mode`, and `to_file_attr`/`marker_attr` take the wrong number of arguments.

- [ ] **Step 3: Add the fields and thread the modes**

In `musefs-fuse/src/lib.rs`, add four fields to the `FuseConfig` struct (after `keep_cache`, before the closing brace at line 56):

```rust
    /// uid presented for every entry (the marker, synthetic dirs, real files).
    pub uid: u32,
    /// gid presented for every entry.
    pub gid: u32,
    /// Permission bits for regular files (bare mode word, no type bits).
    pub file_mode: u16,
    /// Permission bits for directories (bare mode word, no type bits).
    pub dir_mode: u16,
```

Update the `FuseConfig` doc comment (lines 38-39) to note it now also carries attribute identity:

```rust
/// Fuse-layer mount knobs: kernel tuning, page-cache policy, and the ownership
/// (`uid`/`gid`) and permission bits (`file_mode`/`dir_mode`) presented for
/// every entry. Distinct from `musefs_core::MountConfig`, which governs how the
/// virtual tree is rendered.
```

Update `FuseConfig::default()` (lines 58-67) to resolve the process identity and the current default modes:

```rust
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
```

In `MusefsFs::new`, replace the `getuid`/`getgid` calls (lines 173-174) with reads from `config` (a u32 is `Copy`, so reading `config.uid` before `config` is moved at line 176 is fine):

```rust
            uid: config.uid,
            gid: config.gid,
```

Update the `lookup` closure (lines 266-271) to capture and forward both modes:

```rust
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
            Ok(attr) => {
                reply.entry(&ttl, &to_file_attr(&attr, uid, gid, fm, dm, mt), Generation(0))
            }
            Err(e) => reply.error(reply_errno("lookup", child, &e)),
        });
```

Update the `getattr` closure (lines 280-285) the same way:

```rust
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
```

Update the two `marker_attr` call sites (lines 258 and 277) to pass `self.config.file_mode`:

```rust
            let attr = platform::spotlight::marker_attr(
                self.uid,
                self.gid,
                self.config.file_mode,
                self.mount_time,
            );
```

In `musefs-fuse/src/convert.rs`, change `to_file_attr` (lines 15-54). Update the doc comment and signature, and replace the hardcoded perms:

```rust
/// Translate a core `Attr` into a `fuser::FileAttr`. Permission bits come from
/// `dir_mode`/`file_mode` (the mount is read-only, so these are advertised but
/// inert for writes). A zero `mtime_secs` (e.g. synthetic directories) falls
/// back to `fallback_mtime` so tools don't see a 1970 timestamp.
pub(crate) fn to_file_attr(
    attr: &Attr,
    uid: u32,
    gid: u32,
    file_mode: u16,
    dir_mode: u16,
    fallback_mtime: SystemTime,
) -> FileAttr {
```

and the kind/perm/nlink block (lines 33-37):

```rust
    let (kind, perm, nlink) = if attr.is_dir {
        (FileType::Directory, dir_mode, 2)
    } else {
        (FileType::RegularFile, file_mode, 1)
    };
```

In `musefs-fuse/src/platform/spotlight.rs`, change `marker_attr` (lines 25-43) to take `file_mode`:

```rust
/// The marker's attributes: a zero-byte, read-only regular file owned by the
/// mount, all timestamps set to `mtime` (matching synthetic-node stamping).
pub fn marker_attr(uid: u32, gid: u32, file_mode: u16, mtime: SystemTime) -> FileAttr {
    FileAttr {
        ino: INodeNo(MARKER_INO),
        size: 0,
        blocks: 0,
        atime: mtime,
        mtime,
        ctime: mtime,
        crtime: mtime,
        kind: FileType::RegularFile,
        perm: file_mode,
        nlink: 1,
        uid,
        gid,
        rdev: 0,
        blksize: 512,
        flags: 0,
    }
}
```

In `musefs-cli/src/lib.rs`, update the `parse_mount_config` `FuseConfig` literal (lines 199-204) so the workspace still compiles. Fill the new fields from `FuseConfig::default()` for now (Task 2 wires the flags):

```rust
    let defaults = musefs_fuse::FuseConfig::default();
    let fuse_config = musefs_fuse::FuseConfig {
        ttl: std::time::Duration::from_millis(args.attr_ttl_ms),
        max_readahead: args.max_readahead_kib.saturating_mul(1024),
        max_background: args.max_background,
        keep_cache: args.keep_cache,
        uid: defaults.uid,
        gid: defaults.gid,
        file_mode: defaults.file_mode,
        dir_mode: defaults.dir_mode,
    };
```

- [ ] **Step 4: Run the full workspace tests to verify they pass**

Run: `cargo test --workspace 2>&1 | tail -15`
Expected: all tests pass (the new default-field, swap, and marker assertions included).

- [ ] **Step 5: Verify formatting and lint, then commit**

Run: `cargo fmt --all && cargo clippy --all-targets -- -D warnings 2>&1 | tail -5`
Expected: no warnings.

```bash
git add musefs-fuse/src/lib.rs musefs-fuse/src/convert.rs musefs-fuse/src/platform/spotlight.rs musefs-cli/src/lib.rs
git commit -m "feat(fuse): carry presented uid/gid and perms in FuseConfig

FuseConfig now holds the uid/gid and file/dir permission bits presented for
every entry; FuseConfig::default() resolves the process identity and the
existing 0o555/0o444 defaults, so mount/spawn helpers are unchanged. to_file_attr
and marker_attr take the modes as parameters. Behavior is identical until the
CLI flags land.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 2: CLI flags `--owner`/`--group`/`--file-mode`/`--dir-mode` (musefs-cli)

**Files:**
- Modify: `musefs-cli/Cargo.toml` (add `uzers`)
- Modify: `musefs-cli/src/lib.rs` (`MountArgs` ~40-90, value_parsers near `parse_fallback` ~178, `parse_mount_config` ~190-205, `run_mount` ~207-219, tests ~325-360)

- [ ] **Step 1: Add the `uzers` dependency**

In `musefs-cli/Cargo.toml`, under `[dependencies]`, add:

```toml
uzers = "0.12"
```

Run: `cargo build -p musefs-cli 2>&1 | tail -5`
Expected: the dependency downloads and the crate builds (no source changes yet).

- [ ] **Step 2: Write the failing CLI tests**

Add these tests inside the existing `#[cfg(test)] mod tests` block in `musefs-cli/src/lib.rs` (e.g. after `mount_args_parse_into_configs`, around line 360). They reference `parse_octal_mode`, `write_bit_warning`, and the new `--owner`/`--file-mode` flags that don't exist yet.

```rust
    #[test]
    fn octal_mode_parses_as_octal_not_decimal() {
        assert_eq!(parse_octal_mode("644").unwrap(), 0o644);
        assert_eq!(parse_octal_mode("644").unwrap(), 420); // == decimal 420, proving octal
        assert_eq!(parse_octal_mode("0755").unwrap(), 0o755);
    }

    #[test]
    fn octal_mode_rejects_out_of_range_and_non_octal() {
        assert!(parse_octal_mode("10000").is_err()); // 0o10000 > 0o7777
        assert!(parse_octal_mode("8").is_err()); // not an octal digit
        assert!(parse_octal_mode("xyz").is_err());
    }

    #[test]
    fn write_bit_warning_fires_only_for_write_bits() {
        assert!(write_bit_warning("file-mode", 0o444).is_none());
        assert!(write_bit_warning("dir-mode", 0o555).is_none());
        assert!(write_bit_warning("file-mode", 0o664).is_some());
        assert!(write_bit_warning("dir-mode", 0o775).is_some());
    }

    #[test]
    fn owner_and_modes_flow_into_fuse_config() {
        use clap::Parser;
        let cli = Cli::try_parse_from([
            "musefs", "mount", "/mnt", "--db", "/tmp/x.db",
            "--owner", "0", "--group", "0",
            "--file-mode", "640", "--dir-mode", "750",
        ])
        .unwrap();
        let Command::Mount(args) = cli.command else {
            panic!("expected Mount");
        };
        let (_config, fuse_config) = parse_mount_config(&args);
        assert_eq!(fuse_config.uid, 0);
        assert_eq!(fuse_config.gid, 0);
        assert_eq!(fuse_config.file_mode, 0o640);
        assert_eq!(fuse_config.dir_mode, 0o750);
    }

    #[test]
    fn owner_flags_default_to_process_identity() {
        use clap::Parser;
        let cli = Cli::try_parse_from(["musefs", "mount", "/mnt", "--db", "/tmp/x.db"]).unwrap();
        let Command::Mount(args) = cli.command else {
            panic!("expected Mount");
        };
        let (_config, fuse_config) = parse_mount_config(&args);
        let defaults = musefs_fuse::FuseConfig::default();
        assert_eq!(fuse_config.uid, defaults.uid);
        assert_eq!(fuse_config.gid, defaults.gid);
        assert_eq!(fuse_config.file_mode, 0o444);
        assert_eq!(fuse_config.dir_mode, 0o555);
    }

    #[test]
    fn owner_accepts_numeric_and_rejects_unknown_name() {
        assert_eq!(parse_owner("1234").unwrap(), 1234);
        assert!(parse_owner("").is_err());
        assert!(parse_owner("definitely-no-such-user-xyzzy").is_err());
    }
```

- [ ] **Step 3: Run the tests to verify they fail to compile**

Run: `cargo test -p musefs-cli 2>&1 | head -20`
Expected: compile error — `parse_octal_mode`, `write_bit_warning`, `parse_owner` not found and `--owner` is an unexpected argument.

- [ ] **Step 4: Implement the parsers, flags, wiring, and warning**

In `musefs-cli/src/lib.rs`, add the value_parsers and the warning helper next to `parse_fallback` (after line 186):

```rust
/// Resolve `--owner`: a numeric uid is used directly; anything else is looked
/// up as a username. An all-numeric string is always treated as an id (never a
/// name), matching `chown`.
fn parse_owner(s: &str) -> Result<u32, String> {
    if let Ok(uid) = s.parse::<u32>() {
        return Ok(uid);
    }
    uzers::get_user_by_name(s)
        .map(|u| u.uid())
        .ok_or_else(|| format!("no such user: {s}"))
}

/// Resolve `--group`: a numeric gid is used directly; anything else is looked
/// up as a group name.
fn parse_group(s: &str) -> Result<u32, String> {
    if let Ok(gid) = s.parse::<u32>() {
        return Ok(gid);
    }
    uzers::get_group_by_name(s)
        .map(|g| g.gid())
        .ok_or_else(|| format!("no such group: {s}"))
}

/// Parse a bare octal permission word (e.g. `644`, `0755`) — NOT decimal, and
/// without an `0o` prefix. Range-checked to `0o7777`.
fn parse_octal_mode(s: &str) -> Result<u16, String> {
    let mode = u16::from_str_radix(s, 8).map_err(|_| format!("invalid octal mode: {s}"))?;
    if mode > 0o7777 {
        return Err(format!("octal mode out of range (max 7777): {s}"));
    }
    Ok(mode)
}

/// Warning text when a read-only mount is given a mode with write bits set;
/// the bits are applied as requested, this only informs.
fn write_bit_warning(flag: &str, mode: u16) -> Option<String> {
    (mode & 0o222 != 0).then(|| {
        format!("--{flag} {mode:o} sets write bits, but the mount is read-only; writes will fail with EROFS")
    })
}
```

Add the four flags to `MountArgs`, after the `case_insensitive` field (before the struct's closing brace at line 90):

```rust
    /// Owning user for every entry: a username or numeric uid. Defaults to the
    /// launching process's uid.
    #[arg(long, value_name = "NAME|UID", value_parser = parse_owner)]
    pub owner: Option<u32>,
    /// Owning group for every entry: a group name or numeric gid. Defaults to
    /// the launching process's gid.
    #[arg(long, value_name = "NAME|GID", value_parser = parse_group)]
    pub group: Option<u32>,
    /// Permission bits for regular files, octal (e.g. 444). Defaults to 444.
    /// The mount is read-only, so write bits are advertised but inert.
    #[arg(long, value_name = "OCTAL", value_parser = parse_octal_mode)]
    pub file_mode: Option<u16>,
    /// Permission bits for directories, octal (e.g. 555). Defaults to 555.
    #[arg(long, value_name = "OCTAL", value_parser = parse_octal_mode)]
    pub dir_mode: Option<u16>,
```

In `parse_mount_config`, replace the four default-filled fields (added in Task 1) so supplied flags override the defaults:

```rust
        uid: args.owner.unwrap_or(defaults.uid),
        gid: args.group.unwrap_or(defaults.gid),
        file_mode: args.file_mode.unwrap_or(defaults.file_mode),
        dir_mode: args.dir_mode.unwrap_or(defaults.dir_mode),
```

In `run_mount`, emit the warning(s) after `parse_mount_config` (insert after the `let (config, fuse_config) = parse_mount_config(args);` line, around line 211):

```rust
    for (flag, mode) in [("file-mode", args.file_mode), ("dir-mode", args.dir_mode)] {
        if let Some(w) = mode.and_then(|m| write_bit_warning(flag, m)) {
            eprintln!("warning: {w}");
        }
    }
```

- [ ] **Step 5: Run the full workspace tests to verify they pass**

Run: `cargo test --workspace 2>&1 | tail -15`
Expected: all tests pass, including the six new CLI tests.

- [ ] **Step 6: Verify formatting and lint, then commit**

Run: `cargo fmt --all && cargo clippy --all-targets -- -D warnings 2>&1 | tail -5`
Expected: no warnings.

```bash
git add musefs-cli/Cargo.toml musefs-cli/src/lib.rs Cargo.lock
git commit -m "feat(cli): add --owner/--group/--file-mode/--dir-mode to mount

Resolve owner/group names via uzers (numeric ids pass through), parse modes as
octal, and override the FuseConfig defaults. Warn on stderr when a read-only
mount is given write bits.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 3: Document the flags and verify FreeBSD portability

**Files:**
- Modify: `README.md` (CLI flags / mount options section)

- [ ] **Step 1: Find the mount-flags section in the README**

Run: `grep -n -- '--keep-cache\|--attr-ttl-ms\|--case-insensitive\|## .*[Mm]ount' README.md`
Expected: the line numbers of the existing `mount` flag documentation (the four new flags go alongside these).

- [ ] **Step 2: Add the four flags to the README**

In the `mount` flags list (next to `--case-insensitive` / `--keep-cache`), add entries matching the surrounding style. Use the wording:

```markdown
- `--owner <NAME|UID>` — user presented as the owner of every entry. Accepts a
  username or a numeric uid. Default: the launching process's uid.
- `--group <NAME|GID>` — group presented for every entry. Accepts a group name
  or numeric gid. Default: the launching process's gid.
- `--file-mode <OCTAL>` — permission bits for regular files, in octal (e.g.
  `444`). Default: `444`. The mount is read-only, so write bits are advertised
  but writes still fail with `EROFS`.
- `--dir-mode <OCTAL>` — permission bits for directories, in octal (e.g.
  `555`). Default: `555`.
```

- [ ] **Step 3: Verify the docs build/render and the binary help matches**

Run: `cargo run -p musefs --quiet -- mount --help 2>&1 | grep -A1 -- '--owner\|--group\|--file-mode\|--dir-mode'`
Expected: the four flags appear in the generated help with the documented value names and descriptions, confirming the README matches the CLI.

- [ ] **Step 4: Confirm FreeBSD portability of `uzers` (verification, see spec Portability)**

Run: `cargo build --target x86_64-unknown-freebsd -p musefs-cli 2>&1 | tail -15`
Expected: builds. (`uzers` does not advertise a FreeBSD target on docs.rs but, as a `users` fork, builds via libc.) If it does **not** build, do not work around it here — stop and apply the spec's contingency: drop `uzers` and resolve names with `getpwnam_r`/`getgrnam_r` via `libc` behind a safe wrapper in `musefs-cli`, then re-run Tasks 2-3. If the FreeBSD target is not installed locally, note that the FreeBSD CI job is the gate and proceed.

- [ ] **Step 5: Commit**

```bash
git add README.md
git commit -m "docs(readme): document mount --owner/--group/--file-mode/--dir-mode

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Final verification

- [ ] **Run the full suite once more from a clean state**

Run: `cargo fmt --all --check && cargo clippy --all-targets -- -D warnings && cargo test --workspace 2>&1 | tail -20`
Expected: formatting clean, no clippy warnings, all tests pass.

- [ ] **Smoke-check the fuzz crate is unaffected** (format-layer signatures unchanged here, but the convention is to confirm)

Run: `cargo +nightly fuzz build 2>&1 | tail -5`
Expected: builds. (No format-layer API changed, so this should be unaffected; if nightly is unavailable, skip — CI covers it.)
