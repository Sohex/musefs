# FUSE mount-access behaviour (#293 + #294)

Date: 2026-06-12

## Problem

Two GitHub issues concern FUSE mount-access behaviour the docs don't reflect:

- **#293** — On distros shipping an AppArmor profile for `fusermount3` (Ubuntu
  24.04+ / libfuse ≥ 3.17), unprivileged FUSE mounts are only permitted when the
  mountpoint falls under a whitelisted prefix (`$HOME/**`, `/mnt`, `/media`,
  `/tmp`, `/cvmfs`, `$XDG_RUNTIME_DIR`, plus flatpak dirs). Mounting elsewhere
  (e.g. `/data/...`) fails with `fusermount3: mount failed: Permission denied`.
  AppArmor rejects the `mount()` syscall before the mountpoint's own ownership is
  ever checked, so it reads as a musefs bug rather than an environmental
  restriction. The README's mount instructions don't mention it.

- **#294** — The README advertises `--owner`/`--group` as a way to present a
  service account as the owner "without running musefs as that user", but that
  use case is unreachable. musefs mounts with a fixed option set (`RO` +
  `FSName`); there is no `allow_other`, so a default FUSE mount is accessible
  only by the mounting user (and root) — the service account cannot traverse the
  mount at all. And without `default_permissions` the kernel does not enforce the
  presented mode/owner bits, so `--owner`/`--group` are purely cosmetic in
  `stat`/`ls -l`.

## Goals

- #293: document the AppArmor `fusermount3` mountpoint restriction and the two
  workarounds.
- #294: make the advertised cross-user use case actually work by adding
  `allow_other` + `default_permissions` support, auto-enabled when the user asks
  for a specific owner/group, with a clear pre-flight error when the environment
  can't satisfy it.

## Non-goals

- No change to the cardinal read-only / byte-preserving invariants.
- No change to the default mode bits (`444` files, `555` dirs) — they stay
  world-readable so the cross-user case works out of the box.
- No `allow_root` support (out of scope; `allow_other` covers the use case).

## #294 — user-facing contract

- New flag `--allow-other` (bool, env `MUSEFS_ALLOW_OTHER`, default `false`).
  When effective, the mount adds **both** `allow_other` and
  `default_permissions`.
- **Auto-enable:** the effective value is
  `--allow-other || --owner given || --group given`. Passing an owner or group
  implies the intent to let a different account reach the mount, so it stops
  being cosmetic. `--allow-other false` does **not** override the auto-enable
  when `--owner`/`--group` is present (simplest rule: auto-enable wins).
- `default_permissions` makes the kernel enforce the presented owner/mode bits.
  Defaults stay `444`/`555` (world-readable), so the service account can read
  immediately; tightening `--file-mode`/`--dir-mode` now becomes real access
  control rather than cosmetic.
- **Pre-flight check (Linux, non-root):** before mounting, if `allow_other` is
  effective, we are not root, and `/etc/fuse.conf` lacks an active
  `user_allow_other` directive, fail fast with an actionable error (what line to
  add to `/etc/fuse.conf`, and that running as root is exempt) instead of letting
  fusermount3 emit a cryptic `Permission denied`. On non-Linux the check is a
  no-op.

## #294 — implementation

Layering: `allow_other`/`default_permissions` are **mount options** passed to
fusermount3 (unlike `max_readahead`/`max_background`, which are negotiated at
`init` time via `KernelConfig`). They must therefore flow through
`mount_config`/`options`, which today receive only `fs_name`.

- `FuseConfig` (`musefs-fuse/src/lib.rs`) gains `allow_other: bool`
  (default `false`).
- `mount_with`/`spawn_with` extract `config.allow_other` and pass it into
  `new_session(fs, mountpoint, fs_name, allow_other)` →
  `mount_config(fs_name, allow_other)` → `options(fs_name, allow_other)`.
- `platform::mount::options` pushes `MountOption::AllowOther` and
  `MountOption::DefaultPermissions` when `allow_other` is true (both exist in
  fuser 0.17).
- The pre-flight check lives in `musefs-fuse` `platform::mount` (Linux-gated),
  invoked from `new_session` before `Session::new`. It returns an
  `io::Error` that the CLI surfaces with context. A small `/etc/fuse.conf`
  parser detects an active `user_allow_other` directive, ignoring comment
  (`#`) and whitespace-only lines and treating a missing file as "not
  permitted".
- CLI (`musefs-cli/src/lib.rs`): add `--allow-other` to `MountArgs`; in the
  `FuseConfig` builder set
  `allow_other: args.allow_other || args.owner.is_some() || args.group.is_some()`.

## #293 + #294 — documentation

- **#293** (README **Mount** section): note that an arbitrary mountpoint (e.g.
  `/data/...`) may be rejected by the `fusermount3` AppArmor profile on
  Ubuntu 24.04+/libfuse ≥ 3.17, with the `apparmor="DENIED" operation="mount"`
  audit-log signature, and the two fixes: mount under a permitted prefix
  (`$HOME`, `/mnt`, `/media`, `/tmp`, `$XDG_RUNTIME_DIR`, …) or whitelist the
  desired prefix via `/etc/apparmor.d/local/fusermount3` (the shipped profile
  already ends with `include if exists <local/fusermount3>`).
- **#294** (README **Ownership and permissions** section): rewrite to document
  `--allow-other`, the auto-enable on `--owner`/`--group`, that
  `default_permissions` makes mode/owner bits enforced (not cosmetic), and the
  `user_allow_other`/`/etc/fuse.conf` requirement for non-root mounts (root
  exempt). Add the `MUSEFS_ALLOW_OTHER` entry to the environment-variable
  section coverage as applicable.

## Testing

- `options()` unit tests: `allow_other=true` adds both `AllowOther` and
  `DefaultPermissions`; `allow_other=false` adds neither.
- `/etc/fuse.conf` parser unit tests: detects `user_allow_other`, ignores
  commented and whitespace-only lines, treats a missing file as not permitted.
- Pre-flight unit tests: root-exemption path and the missing-config error path,
  injecting the conf contents and root-ness rather than touching real `/etc`.
- CLI tests: `--owner`/`--group` each auto-enable `allow_other` in the built
  `FuseConfig`; explicit `--allow-other` works; default is off.
- Existing FUSE e2e (ignored) remains; a privileged cross-user e2e is out of
  scope (needs two real accounts).

## Risks / trade-offs

- Auto-enabling on `--owner`/`--group` changes behaviour for an existing setup
  that passed `--owner` cosmetically on a box without `user_allow_other`: the
  mount will now fail the pre-flight check instead of mounting. This is
  intentional — the previous behaviour silently failed the advertised use case —
  and the pre-flight error tells the user exactly how to proceed (add the
  directive, or drop `--owner`/`--group`/`--allow-other`).
