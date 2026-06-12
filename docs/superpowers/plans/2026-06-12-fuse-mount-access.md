# FUSE mount-access (#293 + #294) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make the advertised `--owner`/`--group` cross-user use case work by adding `allow_other` + `default_permissions` (auto-enabled by `--owner`/`--group`, plus an explicit `--allow-other`), with a clear non-root pre-flight error, and document the AppArmor `fusermount3` mountpoint restriction.

**Architecture:** `allow_other`/`default_permissions` are FUSE *mount options* (passed to fusermount3), distinct from the `init`-time `KernelConfig` knobs. They flow `FuseConfig.allow_other` → `mount_with`/`spawn_with` → `new_session` → `mount_config` → `platform::mount::options`. A Linux-only pre-flight reads `/etc/fuse.conf` before the mount handshake and fails fast with an actionable, self-contained error when a non-root mount lacks `user_allow_other`. The CLI auto-enables `allow_other` whenever `--owner`/`--group` is given.

**Tech Stack:** Rust workspace (`musefs-fuse`, `musefs-cli`), `fuser` 0.17 (`MountOption::{AllowOther, DefaultPermissions}`), `rustix::process::geteuid`, `clap`.

---

## File structure

| File | Responsibility | Change |
| ---- | -------------- | ------ |
| `musefs-fuse/src/lib.rs` | `FuseConfig` + mount entry points (`mount_with`/`spawn_with`/`new_session`/`mount_config`) | Add `allow_other` field; thread it to options; call pre-flight in `new_session` |
| `musefs-fuse/src/platform/mount.rs` | Per-OS mount options + the Linux `/etc/fuse.conf` pre-flight | `options()` gains `allow_other`; add parser, decision, `check_allow_other` |
| `musefs-cli/src/lib.rs` | `MountArgs` + `parse_mount_config` | Add `--allow-other`; set `allow_other` (auto-enable from owner/group) |
| `README.md` | User docs | #293 AppArmor note (Mount section); #294 ownership rewrite |

**Pre-flight context (read before Task 2):** `new_session` (`musefs-fuse/src/lib.rs:599`) holds `MOUNT_SETUP` (a `Mutex`) only around the racy fusermount3 handshake. The pre-flight read must run *before* taking that lock so the file I/O does not extend the critical section. The error must be self-contained because `run_mount` wraps mount errors as `mounting at <path>: …` (`musefs-cli/src/lib.rs:297`).

---

## Task 1: Thread `allow_other` into mount options + auto-enable from `--owner`/`--group`

**Files:**
- Modify: `musefs-fuse/src/platform/mount.rs` (`options` signature + body + tests)
- Modify: `musefs-fuse/src/lib.rs` (`FuseConfig` struct + `Default`, `mount_config`, `new_session`, `mount_with`, `spawn_with`)
- Modify: `musefs-cli/src/lib.rs` (`parse_mount_config` struct literal)

- [ ] **Step 1: Write the failing options tests**

In `musefs-fuse/src/platform/mount.rs`, update the existing `tests` module so the two existing calls use the new two-arg signature, and add two new tests:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn options_are_always_read_only_and_named() {
        let opts = options("musefs", false);
        assert!(opts.contains(&MountOption::RO));
        assert!(opts.contains(&MountOption::FSName("musefs".to_string())));
    }

    #[test]
    fn allow_other_adds_allow_other_and_default_permissions() {
        let opts = options("musefs", true);
        assert!(opts.contains(&MountOption::AllowOther));
        assert!(opts.contains(&MountOption::DefaultPermissions));
    }

    #[test]
    fn no_allow_other_omits_allow_other_and_default_permissions() {
        let opts = options("musefs", false);
        assert!(!opts.contains(&MountOption::AllowOther));
        assert!(!opts.contains(&MountOption::DefaultPermissions));
    }
}
```

Also update the macOS test module's call (`musefs-fuse/src/platform/mount.rs`):

```rust
#[cfg(all(test, target_os = "macos"))]
mod macos_tests {
    use super::*;

    #[test]
    fn macos_adds_volname_and_noappledouble() {
        let opts = options("musefs", false);
        assert!(opts.contains(&MountOption::CUSTOM("volname=musefs".to_string())));
        assert!(opts.contains(&MountOption::CUSTOM("noappledouble".to_string())));
    }
}
```

- [ ] **Step 2: Run the new tests to verify they fail to compile**

Run: `cargo test -p musefs-fuse options`
Expected: FAIL — compile error, `options` takes 1 argument but 2 were supplied (signature not yet changed).

- [ ] **Step 3: Change `options()` to take and apply `allow_other`**

In `musefs-fuse/src/platform/mount.rs`, replace the `options` function:

```rust
/// Read-only mount options for `fs_name`, plus any per-OS additions. With
/// `allow_other`, also mount `allow_other` + `default_permissions` so an account
/// other than the mounting user can reach the mount and the presented owner/mode
/// bits are kernel-enforced.
pub fn options(fs_name: &str, allow_other: bool) -> Vec<MountOption> {
    let mut opts = vec![MountOption::RO, MountOption::FSName(fs_name.to_string())];
    if allow_other {
        opts.push(MountOption::AllowOther);
        opts.push(MountOption::DefaultPermissions);
    }
    extend_os_specific(&mut opts, fs_name);
    opts
}
```

- [ ] **Step 4: Add the `allow_other` field to `FuseConfig` and its `Default`**

In `musefs-fuse/src/lib.rs`, in `struct FuseConfig` add a field after `dir_mode`:

```rust
    /// Permission bits for directories (bare mode word, no type bits).
    pub dir_mode: u16,
    /// Mount with `allow_other` + `default_permissions`: accounts other than the
    /// mounting user can reach the mount and the kernel enforces the presented
    /// owner/mode bits. Non-root mounts also require `user_allow_other` in
    /// `/etc/fuse.conf` (validated at mount time).
    pub allow_other: bool,
```

In `impl Default for FuseConfig`, add the field after `dir_mode: 0o555,`:

```rust
            dir_mode: 0o555,
            allow_other: false,
```

- [ ] **Step 5: Thread `allow_other` through `mount_config`, `new_session`, `mount_with`, `spawn_with`**

In `musefs-fuse/src/lib.rs`, replace `mount_config`:

```rust
fn mount_config(fs_name: &str, allow_other: bool) -> Config {
    let mut cfg = Config::default();
    cfg.mount_options = platform::mount::options(fs_name, allow_other);
    cfg
}
```

Replace `new_session` (add the `allow_other` parameter; the pre-flight call is added in Task 2):

```rust
fn new_session(
    fs: MusefsFs,
    mountpoint: &Path,
    fs_name: &str,
    allow_other: bool,
) -> std::io::Result<Session<MusefsFs>> {
    // Recover from a poisoned lock: it guards only ordering, so a prior panic
    // during a mount leaves no inconsistent state to protect against.
    let _guard = MOUNT_SETUP
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    Session::new(fs, mountpoint, &mount_config(fs_name, allow_other))
}
```

In `mount_with`, capture `allow_other` before `config` is moved into `MusefsFs::new`:

```rust
    let allow_other = config.allow_other;
    let fs = MusefsFs::new(core, config);
    let cell = fs.notifier_cell();
    let session = new_session(fs, mountpoint, fs_name, allow_other)?;
```

Apply the identical change in `spawn_with`:

```rust
    let allow_other = config.allow_other;
    let fs = MusefsFs::new(core, config);
    let cell = fs.notifier_cell();
    let session = new_session(fs, mountpoint, fs_name, allow_other)?;
```

- [ ] **Step 6: Set `allow_other` (auto-enable) in the CLI config builder**

In `musefs-cli/src/lib.rs`, in `parse_mount_config`, add the field to the `FuseConfig` struct literal after `dir_mode`:

```rust
        file_mode: args.file_mode.unwrap_or(defaults.file_mode),
        dir_mode: args.dir_mode.unwrap_or(defaults.dir_mode),
        allow_other: args.owner.is_some() || args.group.is_some(),
    };
```

- [ ] **Step 7: Add CLI auto-enable tests**

In `musefs-cli/src/lib.rs`, inside `mod tests`, add:

```rust
    #[test]
    fn owner_or_group_auto_enables_allow_other() {
        use clap::Parser;
        for arg in [["--owner", "0"], ["--group", "0"]] {
            let cli = Cli::try_parse_from(
                ["musefs", "mount", "/mnt", "--db", "/tmp/x.db", arg[0], arg[1]],
            )
            .unwrap();
            let Command::Mount(args) = cli.command else {
                panic!("expected Mount");
            };
            let (_config, fuse_config) = parse_mount_config(&args);
            assert!(fuse_config.allow_other, "{arg:?} should enable allow_other");
        }
    }

    #[test]
    fn allow_other_defaults_off_without_owner_group() {
        use clap::Parser;
        let cli = Cli::try_parse_from(["musefs", "mount", "/mnt", "--db", "/tmp/x.db"]).unwrap();
        let Command::Mount(args) = cli.command else {
            panic!("expected Mount");
        };
        let (_config, fuse_config) = parse_mount_config(&args);
        assert!(!fuse_config.allow_other);
    }
```

- [ ] **Step 8: Run the workspace tests**

Run: `cargo test -p musefs-fuse -p musefs-cli`
Expected: PASS — all `options`/allow_other tests pass; existing tests still pass.

- [ ] **Step 9: Lint and commit**

Run: `cargo clippy --all-targets -- -D warnings && cargo fmt --all --check`
Expected: clean.

```bash
git add musefs-fuse/src/lib.rs musefs-fuse/src/platform/mount.rs musefs-cli/src/lib.rs
git commit -m "feat(fuse): allow_other/default_permissions, auto-enabled by --owner/--group (#294)"
```

---

## Task 2: Pre-flight `/etc/fuse.conf` check for non-root `allow_other` mounts

**Files:**
- Modify: `musefs-fuse/src/platform/mount.rs` (parser, decision, `check_allow_other`, tests)
- Modify: `musefs-fuse/src/lib.rs` (`new_session` calls the pre-flight before the lock)

- [ ] **Step 1: Write the failing pre-flight tests**

In `musefs-fuse/src/platform/mount.rs`, add a Linux-gated test module (place it after the existing `tests` module):

```rust
#[cfg(all(test, target_os = "linux"))]
mod preflight_tests {
    use super::*;

    #[test]
    fn parser_accepts_active_directive_forms() {
        assert!(user_allow_other_active("user_allow_other"));
        assert!(user_allow_other_active("   user_allow_other   "));
        assert!(user_allow_other_active("user_allow_other # enable for media server"));
        assert!(user_allow_other_active("mount_max=1000\nuser_allow_other\n"));
    }

    #[test]
    fn parser_rejects_inactive_or_absent() {
        assert!(!user_allow_other_active("# user_allow_other"));
        assert!(!user_allow_other_active("#user_allow_other"));
        assert!(!user_allow_other_active("mount_max=1000"));
        assert!(!user_allow_other_active(""));
    }

    #[test]
    fn preflight_passes_when_not_requested_or_root() {
        assert!(preflight_decision(false, false, None).is_ok());
        assert!(preflight_decision(true, true, None).is_ok());
    }

    #[test]
    fn preflight_requires_directive_for_nonroot() {
        assert!(preflight_decision(true, false, Some("user_allow_other")).is_ok());
        assert!(preflight_decision(true, false, Some("# nope")).is_err());
        assert!(preflight_decision(true, false, None).is_err());
    }

    #[test]
    fn preflight_error_is_self_contained() {
        let err = preflight_decision(true, false, None).unwrap_err();
        assert!(err.contains("/etc/fuse.conf"));
        assert!(err.contains("user_allow_other"));
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p musefs-fuse preflight`
Expected: FAIL — compile error, `user_allow_other_active` / `preflight_decision` not found.

- [ ] **Step 3: Implement the parser, decision, and public check**

In `musefs-fuse/src/platform/mount.rs`, add at the end of the module (before the test modules):

```rust
/// Mount-time guard for `allow_other`: libfuse refuses an `allow_other` mount for
/// a non-root user unless `/etc/fuse.conf` enables `user_allow_other`. Check it
/// up front to replace fusermount3's cryptic "Permission denied" with actionable
/// guidance. Non-Linux platforms don't gate on `/etc/fuse.conf`, so it's a no-op.
pub fn check_allow_other(allow_other: bool) -> std::io::Result<()> {
    #[cfg(target_os = "linux")]
    {
        let is_root = rustix::process::geteuid().as_raw() == 0;
        // `.ok()` collapses any read failure (missing, EACCES, dangling symlink)
        // to `None`, which the decision treats as "not permitted" — fail-safe.
        let conf = std::fs::read_to_string("/etc/fuse.conf").ok();
        preflight_decision(allow_other, is_root, conf.as_deref())
            .map_err(|msg| std::io::Error::new(std::io::ErrorKind::PermissionDenied, msg))
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = allow_other;
        Ok(())
    }
}

#[cfg(target_os = "linux")]
const ALLOW_OTHER_HELP: &str = "allow_other is enabled (via --allow-other, or implied by --owner/--group) \
but '/etc/fuse.conf' does not enable 'user_allow_other'; libfuse refuses a non-root allow_other mount without it. \
Add a line 'user_allow_other' to /etc/fuse.conf, or run musefs as root.";

/// True if `contents` has an active `user_allow_other` directive: a line whose
/// text before any `#` comment trims to exactly `user_allow_other`.
#[cfg(target_os = "linux")]
fn user_allow_other_active(contents: &str) -> bool {
    contents
        .lines()
        .any(|line| line.split('#').next().unwrap_or("").trim() == "user_allow_other")
}

/// Pure pre-flight decision. `conf` is `None` when `/etc/fuse.conf` could not be
/// read; treated as "not permitted" so the actionable error fires (a false
/// positive is harmless — fusermount3 would have failed the mount anyway). Root
/// and the no-`allow_other` case always pass.
#[cfg(target_os = "linux")]
fn preflight_decision(allow_other: bool, is_root: bool, conf: Option<&str>) -> Result<(), String> {
    if !allow_other || is_root {
        return Ok(());
    }
    if conf.is_some_and(user_allow_other_active) {
        return Ok(());
    }
    Err(ALLOW_OTHER_HELP.to_string())
}
```

- [ ] **Step 4: Call the pre-flight in `new_session` before the mount lock**

In `musefs-fuse/src/lib.rs`, insert the check as the first statement of `new_session`, before `MOUNT_SETUP.lock()`:

```rust
fn new_session(
    fs: MusefsFs,
    mountpoint: &Path,
    fs_name: &str,
    allow_other: bool,
) -> std::io::Result<Session<MusefsFs>> {
    // Validate the allow_other environment before taking the mount lock: the
    // /etc/fuse.conf read is unrelated to the fusermount3 handshake the lock
    // serializes, so it must not extend that critical section.
    platform::mount::check_allow_other(allow_other)?;
    // Recover from a poisoned lock: it guards only ordering, so a prior panic
    // during a mount leaves no inconsistent state to protect against.
    let _guard = MOUNT_SETUP
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    Session::new(fs, mountpoint, &mount_config(fs_name, allow_other))
}
```

- [ ] **Step 5: Run the tests**

Run: `cargo test -p musefs-fuse preflight`
Expected: PASS.

- [ ] **Step 6: Lint and commit**

Run: `cargo clippy --all-targets -- -D warnings && cargo fmt --all --check`
Expected: clean. (Note: the `--target x86_64-unknown-freebsd` clippy gate — see CONTRIBUTING — will compile the non-Linux branch; the Linux-only helpers and their tests are `cfg(target_os = "linux")`-gated so they are absent there and produce no dead-code warnings.)

```bash
git add musefs-fuse/src/lib.rs musefs-fuse/src/platform/mount.rs
git commit -m "feat(fuse): pre-flight /etc/fuse.conf user_allow_other check for non-root allow_other mounts (#294)"
```

---

## Task 3: Explicit `--allow-other` flag + effective-value helper

**Files:**
- Modify: `musefs-cli/src/lib.rs` (`MountArgs`, `effective_allow_other`, `parse_mount_config`, tests)

- [ ] **Step 1: Write the failing helper + flag tests**

In `musefs-cli/src/lib.rs`, inside `mod tests`, add:

```rust
    #[test]
    fn effective_allow_other_combines_flag_and_owner_group() {
        assert!(!effective_allow_other(false, None, None));
        assert!(effective_allow_other(true, None, None));
        // Auto-enable wins even when the flag is explicitly false (env path).
        assert!(effective_allow_other(false, Some(0), None));
        assert!(effective_allow_other(false, None, Some(0)));
    }

    #[test]
    fn explicit_allow_other_flag_enables_it() {
        use clap::Parser;
        let cli = Cli::try_parse_from(
            ["musefs", "mount", "/mnt", "--db", "/tmp/x.db", "--allow-other"],
        )
        .unwrap();
        let Command::Mount(args) = cli.command else {
            panic!("expected Mount");
        };
        let (_config, fuse_config) = parse_mount_config(&args);
        assert!(fuse_config.allow_other);
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p musefs-cli allow_other`
Expected: FAIL — `effective_allow_other` not found and `--allow-other` is an unknown argument.

- [ ] **Step 3: Add the `--allow-other` flag to `MountArgs`**

In `musefs-cli/src/lib.rs`, in `struct MountArgs`, add after the `dir_mode` field:

```rust
    /// Mount with `allow_other` + `default_permissions` so accounts other than
    /// the mounting user can reach the mount and the presented owner/mode bits
    /// are kernel-enforced. Implied by `--owner`/`--group`. Non-root mounts also
    /// require `user_allow_other` in `/etc/fuse.conf`.
    #[arg(long, env = "MUSEFS_ALLOW_OTHER")]
    pub allow_other: bool,
```

Adding this field breaks the four **exhaustive** `MountArgs { … }` literals in the integration test file `musefs-cli/tests/cli.rs` (at lines 121, 153, 181, 230 — these build the struct field-by-field, not via argv). Each ends with `dir_mode: None,`; add `allow_other: false,` immediately after that line in all four. For example, the literal at ~line 121:

```rust
        file_mode: None,
        dir_mode: None,
        allow_other: false,
    };
```

Apply the identical one-line addition to the literals at ~153, ~181, and ~230. (Without this the crate fails to compile and the pre-commit full-suite gate rejects the commit.)

- [ ] **Step 4: Add the `effective_allow_other` helper and use it**

In `musefs-cli/src/lib.rs`, add the helper near `parse_mount_config`:

```rust
/// Effective `allow_other`: the explicit flag, or implied by a presented
/// owner/group (the cross-user use case is unreachable without it). Auto-enable
/// wins over an explicit `--allow-other false` (only reachable via the env var).
fn effective_allow_other(flag: bool, owner: Option<u32>, group: Option<u32>) -> bool {
    flag || owner.is_some() || group.is_some()
}
```

In `parse_mount_config`, replace the `allow_other` line in the `FuseConfig` literal:

```rust
        allow_other: effective_allow_other(args.allow_other, args.owner, args.group),
```

- [ ] **Step 5: Run the tests**

Run: `cargo test -p musefs-cli allow_other`
Expected: PASS. Also re-run the Task 1 auto-enable tests: `cargo test -p musefs-cli` → PASS.

- [ ] **Step 6: Lint and commit**

Run: `cargo clippy --all-targets -- -D warnings && cargo fmt --all --check`
Expected: clean.

```bash
git add musefs-cli/src/lib.rs musefs-cli/tests/cli.rs
git commit -m "feat(cli): explicit --allow-other flag with owner/group auto-enable (#294)"
```

---

## Task 4: Documentation (#293 AppArmor note + #294 ownership rewrite)

**Files:**
- Modify: `README.md`

This is a docs-only commit; the pre-commit cargo gate skips it. No tests.

- [ ] **Step 1: Add the AppArmor mountpoint note (#293) to the Mount section**

In `README.md`, find the paragraph (currently ~line 200):

```
`mount` blocks until the filesystem is unmounted (`fusermount -u`, or
Ctrl-C).
```

Insert immediately after it:

```markdown

> **Mounting at an arbitrary path may be denied by AppArmor.** On distros that
> ship an AppArmor profile for `fusermount3` (Ubuntu 24.04+ / libfuse ≥ 3.17),
> unprivileged FUSE mounts are only allowed when the mountpoint is under a
> whitelisted prefix — the shipped profile permits `$HOME/**`, `/mnt`, `/media`,
> `/tmp`, `/cvmfs`, `$XDG_RUNTIME_DIR`, plus flatpak dirs. Mounting elsewhere
> (e.g. a data volume at `/data/...`) fails with `fusermount3: mount failed:
> Permission denied`, and the kernel audit log shows
> `apparmor="DENIED" operation="mount" … profile="fusermount3"`. The mountpoint's
> own ownership is irrelevant — AppArmor rejects the `mount()` syscall first. Fix
> it by mounting under a permitted prefix, or by whitelisting your prefix in
> `/etc/apparmor.d/local/fusermount3` (the shipped profile ends with
> `include if exists <local/fusermount3>`).
```

- [ ] **Step 2: Rewrite the "Ownership and permissions" section (#294)**

In `README.md`, replace the entire current section (intro paragraph + table, currently lines ~259–271):

```markdown
### Ownership and permissions

By default the mount presents the launching process's uid/gid and read-only
permission bits (`555` dirs, `444` files), and is reachable only by the user who
performed the mount (and root).

To present a different owner — e.g. a media-server service account — and let that
account actually reach the mount, pass `--owner`/`--group` (or `--allow-other`).
Either makes musefs mount with `allow_other` and `default_permissions`: other
users can traverse the mount, and the kernel enforces the presented owner/mode
bits instead of ignoring them.

| Flag | Default | What it does |
| ---- | ------- | ------------ |
| `--owner <NAME\|UID>` | process uid | User presented as the owner of every entry. Accepts a username or a numeric uid. Implies `--allow-other`. |
| `--group <NAME\|GID>` | process gid | Group presented for every entry. Accepts a group name or a numeric gid. Implies `--allow-other`. |
| `--allow-other` | off | Mount with `allow_other` + `default_permissions` so accounts other than the mounting user can reach the mount and the owner/mode bits are enforced. Implied by `--owner`/`--group`. |
| `--file-mode <OCTAL>` | `444` | Permission bits for regular files, in octal. The mount is read-only, so write bits are advertised but writes still fail with `EROFS`. |
| `--dir-mode <OCTAL>` | `555` | Permission bits for directories, in octal. |

The default `444`/`555` bits are world-readable, so any account can read once
`allow_other` is on. To restrict the mount to the presented owner/group, drop the
world bits (e.g. `--file-mode 440 --dir-mode 550`) — only then does
`--owner`/`--group` gate access rather than merely label it.

**Non-root mounts need `user_allow_other`.** When you are not root, libfuse
refuses an `allow_other` mount unless `/etc/fuse.conf` contains a line
`user_allow_other`. musefs checks this before mounting and fails with an
explanatory error if it is missing; add the line to `/etc/fuse.conf`, or run
musefs as root. (This is libfuse/system policy, not a musefs restriction.)
```

- [ ] **Step 3: Add `MUSEFS_ALLOW_OTHER` to the systemd conf example**

`contrib/systemd/musefs.conf.example` is the canonical env-var list the README points at. In its "Mount: ownership & permissions" block, after the `#MUSEFS_DIR_MODE=555` entry (currently ~line 59), add:

```ini
# Mount with allow_other + default_permissions so accounts other than the
# mounting user can reach the mount and the owner/mode bits are enforced.
# Implied by MUSEFS_OWNER/MUSEFS_GROUP. Non-root mounts also need
# 'user_allow_other' in /etc/fuse.conf. Default: false.
#MUSEFS_ALLOW_OTHER=true
```

- [ ] **Step 4: Verify the rendered changes read correctly**

Run: `git diff README.md contrib/systemd/musefs.conf.example`
Expected: the AppArmor blockquote appears under **Mount**; the **Ownership and permissions** section shows the new intro, the 5-row table including `--allow-other`, the world-bits note, and the `user_allow_other` note (no stray duplicate of the old table); the systemd example gains the `#MUSEFS_ALLOW_OTHER=true` entry.

- [ ] **Step 5: Commit**

```bash
git add README.md contrib/systemd/musefs.conf.example
git commit -m "docs: document AppArmor mountpoint restriction (#293) and allow_other ownership behaviour (#294)"
```

---

## Final verification

- [ ] **Run the full workspace suite and lints (the pre-commit gate):**

Run: `cargo test && cargo clippy --all-targets -- -D warnings && cargo fmt --all --check`
Expected: all green.

- [ ] **Cross-platform compile check (non-Linux branch of the pre-flight):**

Run: `cargo clippy -p musefs-fuse --target x86_64-unknown-freebsd -- -D warnings`
Expected: clean — the non-Linux `check_allow_other` branch compiles and the Linux-only helpers/tests are absent (no dead-code warnings).

---

## Self-review notes (coverage map)

- Spec "one `--allow-other` flag enabling both" → Task 1 (`options` pushes both `AllowOther` + `DefaultPermissions`) + Task 3 (flag).
- Spec "auto-enable on `--owner`/`--group`, auto-enable wins" → Task 1 (auto-enable) + Task 3 (`effective_allow_other`, tested with `(false, Some, None) == true`).
- Spec "pre-flight, Linux-only, before `MOUNT_SETUP`, self-contained error, read-error ⇒ not permitted, root exempt" → Task 2 (all covered; tests assert root-exempt, not-permitted, self-contained message).
- Spec "parser semantics: strip trailing `#…`, trim, exact token" → Task 2 `user_allow_other_active` + `parser_*` tests.
- Spec "struct-literal sites: `FuseConfig::default` + `parse_mount_config`" → Task 1 Steps 4 & 6.
- Spec "permission matrix / world-bits load-bearing" + "AppArmor prefix list" → Task 4 docs.
- Spec "AutoUnmount unchanged" → no task touches it (confirmed zero usage); intentionally untouched.
