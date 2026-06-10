# Mount owner & permission flags — design

## Summary

Add four optional flags to `musefs mount` that override the ownership and
permission bits the mount presents:

- `--owner <NAME|UID>` — owning user (username or numeric uid)
- `--group <NAME|GID>` — owning group (groupname or numeric gid)
- `--file-mode <OCTAL>` — permission bits for regular files
- `--dir-mode <OCTAL>` — permission bits for directories

All four are optional. Omitting them preserves today's behavior exactly, so the
change is non-breaking.

## Motivation

Today the mount hardcodes ownership to the launching process's uid/gid
(`musefs-fuse/src/lib.rs:173-174`) and permissions to `0o555` for directories
and `0o444` for files (`musefs-fuse/src/convert.rs:33-36`), with the macOS
Spotlight marker also fixed at `0o444` (`musefs-fuse/src/platform/spotlight.rs`).
A mount serving a shared music library often needs to present a specific
owner/group (e.g. a media-server service account) and tighter or looser perms
than the launching process's identity. There is currently no way to set these
without running the daemon as the target user.

## Defaults (backward compatibility)

| Flag | Default | Source of default |
| ---- | ------- | ----------------- |
| `--owner` | launching process uid | `rustix::process::getuid()` |
| `--group` | launching process gid | `rustix::process::getgid()` |
| `--file-mode` | `0o444` | current `to_file_attr` constant |
| `--dir-mode` | `0o555` | current `to_file_attr` constant |

With no flags supplied, the resolved values are byte-for-byte what the mount
produces today.

## Flag parsing & validation (musefs-cli)

Resolution and validation live entirely in the CLI layer, via clap
`value_parser` functions, so `musefs-fuse` only ever receives numeric `u32`s and
stays free of user-database lookups.

### Owner / group

`--owner` and `--group` accept either form:

- If the value parses as a `u32`, it is used directly as the uid/gid. This
  means an all-numeric *username* (legal on Linux) is always interpreted as a
  raw id, never resolved as a name — matching `chown`'s long-standing
  semantics. This is the intended, documented behavior.
- Otherwise it is treated as a name and resolved against the system
  user/group database using the [`uzers`](https://crates.io/crates/uzers) crate
  (`get_user_by_name` → `uid()`, `get_group_by_name` → `gid()`). `uzers` is a
  maintained, *nix-only fork of `users` with a safe API, chosen over hand-rolled
  `getpwnam_r`/`getgrnam_r` FFI to avoid unsafe code. All supported platforms
  (Linux, macOS, FreeBSD) are *nix, so the *nix-only constraint is acceptable
  in principle — but FreeBSD support is a verification item, not a settled
  fact (see [Portability](#portability)).
- A name that does not resolve (including an empty string) produces a clean
  startup error from clap
  (e.g. `error: invalid value 'nobody2' for '--owner <NAME|UID>': no such user`).

### File / dir modes

`--file-mode` and `--dir-mode` parse as **octal** integers via a custom
`parse_octal_mode` value_parser using `u16::from_str_radix(s, 8)` — **not**
clap's default value_parser, which parses decimal and would silently read
`--file-mode 644` as decimal `644` instead of `0o644`. The parser returns
`u16` (matching `fuser::FileAttr::perm` and the `FuseConfig` field type) and is
range-checked to `0o7777`; out-of-range or non-octal values are rejected.

The value masked and stored is the bare permission word (no `S_IFDIR`/`S_IFREG`
type bits OR'd in) — consistent with how `to_file_attr` stores bare `0o555`/
`0o444` in `perm` today, and the same bare value the write-bit check inspects.

The mount is mounted `MountOption::RO`, so write bits in a mode are advertised
but inert — the kernel rejects writes at the read-only-mount level regardless of
the bits. To avoid surprising the user, if a supplied mode contains any write
bit (`0o222` mask), the CLI prints a warning to stderr at startup
(e.g. `warning: --file-mode 0664 sets write bits, but the mount is read-only;
writes will fail with EROFS`). The bits are still applied as requested — the
warning informs, it does not clamp.

## Threading (musefs-cli → musefs-fuse)

`FuseConfig` (`musefs-fuse/src/lib.rs`) gains four fields:

```rust
pub uid: u32,
pub gid: u32,
pub file_mode: u16,
pub dir_mode: u16,
```

(`u16` matches `fuser::FileAttr::perm`.)

**`FuseConfig::default()` (`musefs-fuse/src/lib.rs:58-67`) is the single source
of the defaults.** It resolves `uid`/`gid` via `rustix::process::getuid()`/
`getgid()` and sets `file_mode = 0o444`, `dir_mode = 0o555`. This keeps the
existing `mount`/`spawn` convenience helpers (`lib.rs:475-497`, which pass
`FuseConfig::default()`) and all `default()`-based tests presenting the current
process identity — they would otherwise present uid/gid `0` (root). Putting the
process-id default in `default()` rather than in `MusefsFs::new` or the CLI
centralizes it in one place.

`MusefsFs::new` (`musefs-fuse/src/lib.rs:162`) **drops its own
`getuid`/`getgid` calls** (`lib.rs:173-174`) and reads `uid`/`gid` from the
passed `config`. The modes are read from `self.config` at the call sites (below),
not promoted to top-level `MusefsFs` fields — consistent with how `ttl`/
`keep_cache` are already accessed via `self.config`.

`parse_mount_config` (`musefs-cli/src/lib.rs`) builds the `FuseConfig` starting
from `FuseConfig::default()` and overrides only the fields whose flags were
supplied: `uid` from a resolved `--owner` else the default, `gid` from
`--group` else the default, `file_mode`/`dir_mode` from their flags else the
defaults. The CLI therefore needs **no** `rustix`/`getuid` dependency of its
own — it inherits the process-id default through `FuseConfig::default()`.

`FuseConfig`'s doc comment, currently scoped to "kernel tuning + page-cache
policy", is updated to note it also carries the presented attribute identity
(owner and permission bits). A separate `AttrConfig` struct was considered but
rejected as over-plumbing for four fields with a single consumer.

## Attribute rendering (musefs-fuse)

`to_file_attr` (`musefs-fuse/src/convert.rs`) takes the two modes instead of the
hardcoded `0o555`/`0o444`:

```rust
let (kind, perm, nlink) = if attr.is_dir {
    (FileType::Directory, dir_mode, 2)
} else {
    (FileType::RegularFile, file_mode, 1)
};
```

uid/gid continue to be passed in as they are today. The two closures that call
`to_file_attr` — `lookup` (`lib.rs:266-271`) and `getattr` (`lib.rs:280-285`) —
currently capture `let (uid, gid, mt, ttl) = (self.uid, self.gid, ...)`; they
must additionally capture `self.config.file_mode` and `self.config.dir_mode`
and forward both to `to_file_attr`.

The macOS Spotlight marker `marker_attr`
(`musefs-fuse/src/platform/spotlight.rs:25`) — today
`marker_attr(uid, gid, mtime)` — gains a `file_mode` parameter, becoming
`marker_attr(uid, gid, file_mode, mtime)`. Its two call sites (`lib.rs:258`,
`lib.rs:277`) pass `self.config.file_mode`, and the existing non-macOS test
(`spotlight.rs:87-98`) is updated for the new signature. So every entry the
mount presents — real files, synthetic directories, and the marker — is
uniform.

## Components touched

| Unit | Change |
| ---- | ------ |
| `musefs-cli` `MountArgs` | four new `#[arg]` fields + value_parsers |
| `musefs-cli` value_parsers | `parse_owner`, `parse_group`, `parse_octal_mode` (with write-bit warning) |
| `musefs-cli` `parse_mount_config` | populate new `FuseConfig` fields |
| `musefs-cli` Cargo.toml | add `uzers` dependency |
| `musefs-fuse` `FuseConfig` | four new fields + doc update |
| `musefs-fuse` `FuseConfig::default()` | resolve process uid/gid + default modes |
| `musefs-fuse` `MusefsFs::new` | drop `getuid`/`getgid`; read uid/gid from config |
| `musefs-fuse` `to_file_attr` | take file/dir mode params |
| `musefs-fuse` `lookup`/`getattr` closures | capture + forward both modes (`lib.rs:266-285`) |
| `musefs-fuse` `marker_attr` + 2 call sites | new `file_mode` param (`spotlight.rs:25`, `lib.rs:258`,`277`) |
| `README.md` | document the four flags |

## Portability

Name resolution runs at clap-parse time, as the launching user, before any
mount or session work — so it reads the passwd/group DB with the invoking
identity, which is exactly the intent (present an owner *without* running as
that user).

`uzers` advertises build targets for Linux and macOS (Darwin) but **not**
FreeBSD on docs.rs. As a `users` fork it historically builds on FreeBSD via
libc and will most likely work, but this is an explicit verification task, not
an assumption: confirm `uzers` compiles and resolves names under the FreeBSD CI
job (which must exercise `musefs-cli` for the dep to be covered). **Contingency
if it does not build on FreeBSD:** drop `uzers` and hand-roll the lookup with
`getpwnam_r`/`getgrnam_r` via `libc` (already a `musefs-fuse` dependency),
guarded behind a small safe wrapper in `musefs-cli` — the same alternative
weighed and deferred during design.

## Error handling

- Unresolvable owner/group name → clap parse error, non-zero exit, no mount.
- Out-of-range octal mode → clap parse error, non-zero exit, no mount.
- Write bits in a mode → stderr warning, mount proceeds.

## Testing

- **CLI unit tests** (`musefs-cli`):
  - `--owner`/`--group` accept numeric and name forms; an all-numeric value
    resolves as an id, not a name; an unresolvable name (and an empty string)
    errors.
  - Octal parsing: `--file-mode 644` yields `0o644` (== decimal 420), proving
    octal-not-decimal; valid modes accepted; out-of-range (`> 0o7777`) and
    non-octal rejected.
  - The write-bit warning **fires** for a mode with `0o222` bits and is
    **silent** for `0o444`/`0o555` (pins the mask boundary).
  - End-to-end: extend the existing `mount_args_parse_into_configs` test
    (`musefs-cli/src/lib.rs:326-360`) to assert that supplied `--owner`/
    `--group`/`--file-mode`/`--dir-mode` land in the resulting `FuseConfig`
    fields, and that with no flags the `FuseConfig` matches
    `FuseConfig::default()` (catches broken wiring through `parse_mount_config`).
- **`convert.rs`**: `to_file_attr` honors custom file and dir modes — the
  existing default-asserting tests (`convert.rs:110-144`) are updated to pass
  modes explicitly, **plus** a test with `dir_mode != file_mode` asserting the
  dir gets `dir_mode` and the file gets `file_mode`. This last test kills the
  dir/file-mode **swap mutant**: `to_file_attr` is the only mutation-tested
  logic in `musefs-fuse` (gated by `.cargo/mutants.toml`, per the header
  comment at `convert.rs:6-9`), and parameterizing the modes adds new mutable
  branches, so the swap case must be covered or the gate surfaces a survivor.
- **`FuseConfig::default()`**: extend `fuse_config_default_is_conservative`
  (`musefs-fuse/src/lib.rs:556-562`) to assert `file_mode == 0o444`,
  `dir_mode == 0o555`, and `uid`/`gid` equal `getuid()`/`getgid()`.
- **Spotlight** (`spotlight.rs`): `marker_attr` test confirms configured uid/gid
  and file_mode under the new `(uid, gid, file_mode, mtime)` signature.
- **README**: CLI-flags section lists the four flags with defaults.

## Out of scope

- Symbolic mode syntax (`u+rwx`) — octal only.
- Per-entry or per-template ownership/perms — a single uniform owner/mode pair
  for the whole mount.
- Windows/non-*nix name resolution — all supported platforms are *nix.
