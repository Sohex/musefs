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
- **No `AutoUnmount`.** `allow_other` mounts are a classic motivation for
  `AutoUnmount` (a crashed daemon leaves a mount other users are stuck on), but
  we do not add it here — cleanup stays the existing signal-handler path that
  shells out to `fusermount3 -u` (`musefs-cli/src/signal.rs`). Adding
  `AutoUnmount` is a separate decision.

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
  Defaults stay `444`/`555` (world-readable), so any account can read
  immediately; tightening `--file-mode`/`--dir-mode` now becomes real access
  control rather than cosmetic.

### Permission model under `default_permissions`

With `default_permissions` on, the kernel checks the caller's identity against
the **presented** owner/group/mode of each entry. The **world (other) bits are
the load-bearing mechanism** for the default cross-user case — access is granted
via the world-read bit, not via an owner/group match. The matrix for the default
`444`/`555` (world-readable) entries:

| Caller \ `--owner svc` | not set (owner = mounting user) | set to `svc` |
| ---------------------- | ------------------------------- | ------------ |
| mounting user          | read (owner bit, and world bit) | read (world bit) |
| `svc` service account  | read (world bit)                | read (owner bit, and world bit) |
| any other user         | read (world bit)                | read (world bit) |

Implication to document: making the mount *private* to the presented owner/group
requires dropping the world bits (e.g. `--file-mode 440 --dir-mode 550`); only
then does `--owner`/`--group` restrict access rather than merely label it. Bare
`--allow-other` (no `--owner`) keeps the mounting user as owner and simply lets
other real users browse the world-readable tree — that is its standalone use
case.

- **Pre-flight check (Linux, non-root):** before mounting, if `allow_other` is
  effective, we are not root (`geteuid() != 0`), and `/etc/fuse.conf` lacks an
  active `user_allow_other` directive, fail fast with an actionable, **self-
  contained** error (it must read well even when the CLI wraps it as
  `mounting at <path>: …`, so it states the exact line to add to
  `/etc/fuse.conf` and that root is exempt) instead of letting fusermount3 emit a
  cryptic `Permission denied`. Root is exempt because libfuse only enforces
  `user_allow_other` for non-root callers. On non-Linux the check is a no-op.

  **`user_allow_other` detection semantics.** For each line: strip a trailing
  `#…` comment, then trim surrounding whitespace; the directive is *active* iff
  the remaining token equals exactly `user_allow_other`. Therefore
  `   user_allow_other   ` ⇒ active; `user_allow_other # note` ⇒ active;
  `# user_allow_other` and `#user_allow_other` ⇒ inactive; `mount_max=10` and
  other directives ⇒ ignored. **Any read failure is treated as "not permitted"**
  — missing file, `EACCES`, dangling symlink, etc. — so the actionable error
  fires. A false positive here is harmless because fusermount3 would have failed
  the mount anyway; failing safe gives the user the helpful message instead of
  the cryptic one.

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
  invoked from `new_session` **before acquiring the `MOUNT_SETUP` mutex** — the
  conf-file read is unrelated to the racy fusermount3 handshake the lock guards,
  so it must not extend that critical section. It returns a self-contained
  `io::Error` (see the contract above; the CLI wraps mount errors as
  `mounting at <path>: …`, so the message cannot rely on additional context).
  The `/etc/fuse.conf` parser implements the detection semantics defined above.
- CLI (`musefs-cli/src/lib.rs`): add `--allow-other` to `MountArgs`
  (`env = "MUSEFS_ALLOW_OTHER"`). In `parse_mount_config` (the `(MountConfig,
  FuseConfig)` builder, ~cli.rs:254–275), set the new field in the `FuseConfig`
  struct literal:
  `allow_other: args.allow_other || args.owner.is_some() || args.group.is_some()`.
- **Struct-literal sites to update** (adding a field to `FuseConfig` breaks
  exhaustive literals): `FuseConfig::default` (lib.rs:~70) and the
  `parse_mount_config` literal (cli.rs:~264). The `keep_cache` test site uses
  `..Default::default()` and is unaffected. **Adding a field to `MountArgs` also
  breaks the four exhaustive `MountArgs { … }` literals in the integration test
  `musefs-cli/tests/cli.rs` (lines 121, 153, 181, 230)** — each must gain
  `allow_other: false,`. (The in-`src` CLI tests build `MountArgs` via argv
  parsing and are non-breaking.) The plan should grep `FuseConfig {` and
  `MountArgs {` to confirm no further exhaustive sites exist.

## #293 + #294 — documentation

- **#293** (README **Mount** section): note that an arbitrary mountpoint (e.g.
  `/data/...`) may be rejected by the `fusermount3` AppArmor profile on
  Ubuntu 24.04+/libfuse ≥ 3.17, with the `apparmor="DENIED" operation="mount"`
  audit-log signature, and the two fixes: mount under a permitted prefix (the
  shipped profile allows `$HOME/**`, `/mnt`, `/media`, `/tmp`, `/cvmfs`,
  `$XDG_RUNTIME_DIR`, plus flatpak dirs) or whitelist the desired prefix via
  `/etc/apparmor.d/local/fusermount3` (the shipped profile already ends with
  `include if exists <local/fusermount3>`). The README may show a representative
  subset of this list; keep the two consistent.
- **#294** (README **Ownership and permissions** section): rewrite to document
  `--allow-other`, the auto-enable on `--owner`/`--group`, that
  `default_permissions` makes mode/owner bits enforced (not cosmetic), and the
  `user_allow_other`/`/etc/fuse.conf` requirement for non-root mounts (root
  exempt). Add the `MUSEFS_ALLOW_OTHER` entry to the environment-variable
  section coverage as applicable.

## Testing

- `options()` unit tests: `allow_other=true` adds both `AllowOther` and
  `DefaultPermissions`; `allow_other=false` adds neither.
- `/etc/fuse.conf` parser unit tests covering the detection vectors above:
  `user_allow_other`, leading/trailing whitespace, trailing inline comment (all
  active); `# user_allow_other`, `#user_allow_other`, `mount_max=10`, empty file
  (all inactive); and read failure / missing file ⇒ not permitted.
- Pre-flight unit tests: root-exemption path and the not-permitted error path,
  injecting the conf contents and root-ness rather than touching real `/etc`;
  assert the error message is self-contained (mentions `/etc/fuse.conf` +
  `user_allow_other`) so it survives the CLI's `mounting at …` wrapper.
- CLI tests: `--owner` alone and `--group` alone each auto-enable `allow_other`
  in the built `FuseConfig`; explicit `--allow-other` works; default is off; and
  the env path — `MUSEFS_OWNER` set with `MUSEFS_ALLOW_OTHER=false` still yields
  effective `true` (auto-enable wins).
- Existing FUSE e2e (ignored) remains; a privileged cross-user e2e is out of
  scope (needs two real accounts).
- The README changes (#293, #294) are docs-only and the pre-commit cargo gate
  skips them, so they can land as a separate green commit from the code.

## Risks / trade-offs

- Auto-enabling on `--owner`/`--group` changes behaviour for an existing setup
  that passed `--owner` cosmetically on a box without `user_allow_other`: the
  mount will now fail the pre-flight check instead of mounting. This is
  intentional — the previous behaviour silently failed the advertised use case —
  and the pre-flight error tells the user exactly how to proceed (add the
  directive, or drop `--owner`/`--group`/`--allow-other`).
