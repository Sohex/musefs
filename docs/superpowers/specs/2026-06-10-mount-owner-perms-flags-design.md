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

- If the value parses as a `u32`, it is used directly as the uid/gid.
- Otherwise it is treated as a name and resolved against the system
  user/group database using the [`uzers`](https://crates.io/crates/uzers) crate
  (`get_user_by_name` → `uid()`, `get_group_by_name` → `gid()`). `uzers` is a
  maintained, *nix-only fork of `users` with a safe API, chosen over hand-rolled
  `getpwnam_r`/`getgrnam_r` FFI to avoid unsafe code. All supported platforms
  (Linux, macOS, FreeBSD) are *nix, so the *nix-only constraint is acceptable.
- A name that does not resolve produces a clean startup error from clap
  (e.g. `error: invalid value 'nobody2' for '--owner <NAME|UID>': no such user`).

### File / dir modes

`--file-mode` and `--dir-mode` parse as **octal** integers, range-checked to
`0o7777`; out-of-range values are rejected by the parser.

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

`parse_mount_config` (`musefs-cli/src/lib.rs`) populates them: uid/gid from the
resolved `--owner`/`--group` or the process ids when unset; modes from
`--file-mode`/`--dir-mode` or the defaults.

`MusefsFs::new` (`musefs-fuse/src/lib.rs:162`) reads `uid`/`gid` from the passed
`config` instead of calling `getuid`/`getgid` directly. The
process-id fallback moves up to the CLI, which is the only caller that should
decide the default identity.

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

uid/gid continue to be passed in as they are today.

The macOS Spotlight marker `marker_attr`
(`musefs-fuse/src/platform/spotlight.rs`) receives the same uid/gid and
`file_mode`, so every entry the mount presents — real files, synthetic
directories, and the marker — is uniform.

## Components touched

| Unit | Change |
| ---- | ------ |
| `musefs-cli` `MountArgs` | four new `#[arg]` fields + value_parsers |
| `musefs-cli` value_parsers | `parse_owner`, `parse_group`, `parse_octal_mode` (with write-bit warning) |
| `musefs-cli` `parse_mount_config` | populate new `FuseConfig` fields |
| `musefs-cli` Cargo.toml | add `uzers` dependency |
| `musefs-fuse` `FuseConfig` | four new fields + doc update |
| `musefs-fuse` `MusefsFs::new` | read uid/gid from config |
| `musefs-fuse` `to_file_attr` | take file/dir mode params |
| `musefs-fuse` `marker_attr` callers | pass configured file_mode |
| `README.md` | document the four flags |

## Error handling

- Unresolvable owner/group name → clap parse error, non-zero exit, no mount.
- Out-of-range octal mode → clap parse error, non-zero exit, no mount.
- Write bits in a mode → stderr warning, mount proceeds.

## Testing

- **CLI unit tests** (`musefs-cli`): `--owner`/`--group` accept numeric and name
  forms; an unresolvable name errors; octal parsing accepts valid modes and
  rejects out-of-range; the write-bit warning fires for modes with `0o222` bits;
  defaults with no flags reproduce the current `FuseConfig` values (extend the
  existing `parse_mount_config` tests).
- **`convert.rs`**: `to_file_attr` honors custom file and dir modes (the
  existing tests at `convert.rs:123-137` assert the defaults and are updated to
  pass modes explicitly).
- **Spotlight** (`spotlight.rs`): `marker_attr` test confirms configured uid/gid
  and file_mode.
- **README**: CLI-flags section lists the four flags with defaults.

## Out of scope

- Symbolic mode syntax (`u+rwx`) — octal only.
- Per-entry or per-template ownership/perms — a single uniform owner/mode pair
  for the whole mount.
- Windows/non-*nix name resolution — all supported platforms are *nix.
