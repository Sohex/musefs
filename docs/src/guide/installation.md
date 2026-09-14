# Installation

Three ways to get musefs: a [prebuilt binary](#prebuilt-binaries) (no
toolchain needed), [building from source](#building-from-source), or a
[container image](containers.md#container-images).
Whichever you pick, mounting needs a 64-bit FUSE-capable OS (Linux, FreeBSD, macOS) — see
[Platform support](#platform-support).

> **Upgrading from 1.x:** 2.0.0 changes the store's schema, and every command
> other than `musefs migrate` refuses a 1.x store until it has been upgraded.
> Stop any running mount, then run `musefs migrate --db <store>`; the
> [release notes](../release-notes.md#upgrading-from-v130) walk through it.

> **Important:** Linux and FreeBSD are E2E tested. I don't have anything running macOS to test on, if you run this on one let me know if it works, or especially if it doesn't!
>
> At present AMD64, AARCH64, and RISC-V 64 are supported. If you'd like 32-bit support please open an issue.

## Prebuilt binaries

Each tagged release attaches static/portable Linux binaries for six targets:

| Target | libc | Notes |
| --- | --- | --- |
| `x86_64-unknown-linux-gnu`  | glibc | Pinned to glibc 2.17 — runs on essentially any current distro. |
| `aarch64-unknown-linux-gnu` | glibc | glibc 2.17 floor, ARM64. |
| `x86_64-unknown-linux-musl`  | musl | Fully static — runs on Alpine / scratch containers. |
| `aarch64-unknown-linux-musl` | musl | Fully static, ARM64. |
| `riscv64gc-unknown-linux-gnu` | glibc | glibc 2.27 floor, RISC-V 64. |
| `riscv64gc-unknown-linux-musl` | musl | Fully static, RISC-V 64. |

The `*-musl` build is statically linked, so it runs on **any** Linux host of
that architecture regardless of libc — glibc distros (Debian/Ubuntu/Fedora)
included, not just Alpine/musl. For mixed or containerized deployments it is the
simplest choice: one binary you can drop onto a glibc host and an Alpine image
alike.

Download the tarball for your target from the
[latest release](https://github.com/Sohex/musefs/releases/latest), verify it,
and extract:

```bash
sha256sum -c musefs-<version>-<target>.tar.gz.sha256
tar -xzf musefs-<version>-<target>.tar.gz   # yields ./musefs
```

**Runtime requirements:** the binaries mount via FUSE's `fusermount3` helper, so
the target needs the FUSE userspace tools and `/dev/fuse`:

- Debian/Ubuntu: `apt-get install fuse3`
- Alpine: `apk add fuse3`

No glibc/libfuse install is needed for the musl binaries beyond `fuse3`.

**Backing storage:** FAT (FAT12, FAT16 and FAT32) and exFAT are not
recommended for the music library musefs serves from. musefs is fully
functional on them, but its check that a backing file has not changed on disk
is weaker there. That check normally
compares a file's size, modification time, change time and inode number. FAT
stores the modification time in two-second steps and exFAT in 10 ms steps, both
report the change time as the modification time, and neither keeps inode
numbers stable, so musefs records no inode for their files. What is left is size
plus a coarse modification time: a same-size replacement or rewrite inside that
window goes undetected, and the file is served with metadata laid out for its
old content.

musefs records an inode only on filesystems it knows keep their inode numbers
across a remount, such as ext4, btrfs, XFS, ZFS, APFS and NFS. Network shares
over SMB, FUSE mounts (sshfs, rclone, s3fs) and overlayfs record none either,
because their numbers can change with a remount: recording them would make every
file on the mount fail to open after each remount until a revalidate. Those keep
their real timestamps, so they lose only the inode's protection against a
same-size replacement inside the timestamp resolution. Filesystems with real
timestamps and stable inode numbers get the full check; see
[freshness](../architecture/tree-scanning.md) for how it works and the full
list.

> **Note:** On Ubuntu 24.04+ (libfuse ≥ 3.17) the `fusermount3` AppArmor
> profile only permits unprivileged mounts under whitelisted prefixes
> (`$HOME/**`, `/mnt`, `/media`, `/tmp`, …). Mounting elsewhere fails with
> `fusermount3: mount failed: Permission denied` — see
> [Mounting](mounting.md#mount) for the whitelist and the fix.

## Building from source

`cargo install musefs` compiles the latest release; building needs Rust 1.95
or newer (2024 edition) plus a C compiler, for the bundled SQLite, and `make`,
for jemalloc. No FUSE headers or `pkg-config` are needed: musefs does not link
libfuse (on Linux it mounts through `fusermount3`). To install the latest
development version instead:

```bash
cargo install --git https://github.com/Sohex/musefs musefs
```

On Linux the same `fuse3` runtime requirement as the prebuilt binaries applies;
FreeBSD and macOS use their own FUSE implementations, listed under
[Platform support](#platform-support).

The binary uses **jemalloc** as its global allocator by default (it bounds
resident memory for the long-lived mount daemon under heavy concurrent reads).
Distribution packagers or anyone debugging memory with valgrind/heaptrack can
build against the system allocator instead with
`cargo build -p musefs --no-default-features` (or `cargo install musefs
--no-default-features`).

## Platform support

| Platform | FUSE | Kernel passthrough (StructureOnly) | Notes |
| --- | --- | --- | --- |
| Linux | Yes (`/dev/fuse` + `fusermount3`, from the `fuse3` package) | Yes (6.9+, falls back to daemon serving otherwise) | Full support. |
| FreeBSD | Yes (pure-rust `/dev/fuse` backend; `fusefs` kernel module, no libfuse) | No | Full FUSE support. |
| macOS (FUSE-T) | Best-effort | No | Compiles and runs unit tests with `macos-no-mount`; mounted e2e is not yet validated. |

On platforms without kernel passthrough, `--mode structure-only` still serves
the original bytes, just through the daemon instead of the kernel.

Filename **case-folding** is platform-aware: `--case-insensitive <true|false>`
defaults to `true` on macOS and `false` on Linux/FreeBSD. When enabled,
filenames are compared case-insensitively — case-variant directories merge into
one (first-seen casing wins) and case-variant files get a numeric suffix (e.g.
`Song (2)`); case-insensitive mounts refresh via a full rebuild rather than the
incremental fast path.
