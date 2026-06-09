# musl + glibc release binaries — design

**Date:** 2026-06-09
**Status:** Approved (pending implementation plan)

## Problem

musefs ships no downloadable binary today — `release.yml` only publishes crates
to crates.io. Users on Alpine / musl-based systems cannot run a glibc binary
(observed during the Lidarr smoke test: the glibc `musefs` binary would not run
on Alpine). We want portable, downloadable binaries for the common Linux
targets, built and verified on every release tag.

## Goal

On every `v*` tag, publish four downloadable, portable binaries as GitHub
Release assets, each verified by a real FUSE mount smoke test:

| libc  | x86_64 | aarch64 |
| ----- | ------ | ------- |
| glibc | ✅     | ✅      |
| musl  | ✅     | ✅      |

Triples: `x86_64-unknown-linux-gnu`, `aarch64-unknown-linux-gnu`,
`x86_64-unknown-linux-musl`, `aarch64-unknown-linux-musl`.

## Non-goals (explicitly deferred)

- Container images / Alpine APK packaging — tracked in a separate follow-up
  issue. Not in this work.
- Windows / FreeBSD / macOS binary artifacts.
- A standalone CI build/test gate for musl outside the release flow. The release
  build is the only musl build for now; between releases musl can break
  silently. (If desired later, the matrix job can be reused in `ci.yml`.)

## Source changes

### Drop the `libfuse` feature

`musefs-fuse/Cargo.toml`:

```toml
fuser = { version = "0.17", default-features = false }
```

The macOS target dependency keeps its existing `macos-no-mount` feature.

**Why this is safe / transparent:**

- musefs already mounts through `fusermount3` (see the handshake comment at
  `musefs-fuse/src/lib.rs:507`), not a privileged libfuse syscall path.
- musefs uses **no** `MountOption::AutoUnmount` anywhere (only `RO`, `FSName`,
  and macOS-only `volname`/`noappledouble` customs — see
  `musefs-fuse/src/platform/mount.rs`). `AutoUnmount` is the only fuser feature
  that *requires* `libfuse`.
- A static musl binary cannot easily link libfuse3 (C). The non-libfuse fuser
  path shells out to the `fusermount3` binary at runtime, giving a fully static
  binary with no functional loss on Linux.

**Consequence:** dropping `libfuse` loses nothing musefs currently has, but
permanently forecloses adding `MountOption::AutoUnmount` (the mechanism that
cleans up a stale mount after a hard `SIGKILL`/segfault, and only on glibc
anyway). We replace that gap with a portable signal handler (below).

### Graceful unmount on SIGTERM/SIGINT

Today the blocking mount path (`mount_with` → `new_session` →
`session.spawn()` → `bg.join()` in `musefs-fuse/src/lib.rs`) installs no signal
handler. On SIGINT/SIGTERM the process dies without running
`BackgroundSession::Drop`, leaving a stale mount (`Transport endpoint is not
connected`) until manual `fusermount3 -u`. This is true today even with
libfuse linked.

Add a SIGTERM/SIGINT handler that triggers a graceful session unmount before
exit. This covers the realistic lifecycle cases — Ctrl-C, `systemctl stop`, and
container / Lidarr stop all send SIGTERM — leaving only `SIGKILL`/segfault to
manual recovery (same as today). Works identically on glibc and musl; no
`libfuse` needed.

**Approach:**

- The workspace denies `unsafe_code`, so raw `libc::sigaction` is out. Use the
  `signal-hook` crate for safe signal handling (new dependency, likely in
  `musefs-fuse`; the exact crate boundary is an implementation detail for the
  plan).
- Obtain an unmount handle from the fuser session (fuser exposes a session
  unmounter / `unmount_callable`-style handle). Restructure the blocking mount
  entry so a signal-handler thread can call the unmounter, causing `bg.join()`
  to return and the process to exit cleanly with the mount removed.
- `musefs-cli` / the `musefs` binary stay thin; cross-cutting mount logic lives
  in `musefs-fuse` per the workspace layering.

## Build: `cargo-zigbuild`

Build all four targets from a single amd64 host using `cargo-zigbuild`, which
uses Zig as the cross-linker and C compiler. Zig compiles the bundled SQLite
(`rusqlite` with `bundled`) for every target without Docker or per-target host
toolchains.

- Install Zig (e.g. `mlugg/setup-zig`) and `cargo-zigbuild`.
- `rustup target add` the four triples.
- Build: `cargo zigbuild --release -p musefs --target <triple>`.
- **glibc portability:** pin glibc with the zigbuild target suffix (e.g.
  `x86_64-unknown-linux-gnu.2.17`) so glibc binaries run on older
  distributions. Pick a conservative, widely-available glibc version in the
  plan.
- **musl:** static by default — no extra flags.
- Strip the release binaries.

### Packaging

Each target produces:

- `musefs-<version>-<triple>.tar.gz` (the stripped binary)
- `musefs-<version>-<triple>.tar.gz.sha256`

`<version>` is the workspace version (already verified to match the tag by the
existing `release.yml` step).

## Smoke test: real FUSE mount, both arches

The smoke exercises the **built binary** (not `cargo test`) and performs a real
FUSE mount, on native runners — no qemu in CI.

- **amd64** smokes run on `ubuntu-latest`.
- **aarch64** smokes run on `ubuntu-24.04-arm` (free native arm64 hosted
  runners for public repos — musefs is public/MIT). Real FUSE mounting already
  works on GitHub Linux runners (the existing `musefs-fuse -- --ignored` e2e job
  relies on it).
- **musl** smokes run **inside an Alpine container** on the matching runner
  (`apk add fuse3`, container needs `--device /dev/fuse --cap-add SYS_ADMIN`,
  and `--security-opt apparmor:unconfined` if required). This proves the static
  binary actually runs on Alpine — the original failure mode.
- **glibc** smokes run directly on the runner (`apt-get install fuse3`).

**Each smoke must, end-to-end:**

1. Install the FUSE runtime (`fuse3` / `fusermount3`).
2. Prepare a minimal backing fixture and scan it into a DB using the built
   `musefs` binary (the exact fixture-generation approach — reuse of existing
   test fixtures or ffmpeg-generated audio — is a plan detail; the existing e2e
   job installs ffmpeg for this purpose).
3. Mount the built binary at a temp mountpoint.
4. Read at least one synthesized file through the mount and verify its bytes
   (assert the central invariant holds for at least one format).
5. Unmount cleanly.

This is four smokes total (one per target) across two physical runner types.

**Asset upload is gated on all four smokes passing.**

There are four binaries but only two runner architectures; if a real mount
proves infeasible inside the Alpine container in CI for a given arch, fall back
to a `--version` smoke for that cell and **log the downgrade explicitly** rather
than silently skipping (no silent coverage gaps).

## Release workflow changes (`release.yml`)

Current state: a single `publish` job installs libfuse3, verifies the tag
matches the workspace version, and publishes crates to crates.io. It creates
**no** GitHub Release.

Add, alongside the unchanged `publish` job:

1. **`build` matrix job** (4 targets) on amd64 — runs `cargo-zigbuild`, packages
   tarballs + checksums, uploads them as workflow artifacts.
2. **`smoke` jobs** — per target, on the matching runner / container, consuming
   the build artifacts and running the real mount smoke above.
3. **`release-assets` job** — depends on all smokes passing; downloads the
   artifacts and creates/updates the GitHub Release for the tag, uploading the
   tarballs + `.sha256` files via the **`gh` CLI** (`gh release create` /
   `gh release upload`). Using `gh` avoids adding a third-party action that
   would need SHA-pinning.

**Permissions:** the `release-assets` job needs `permissions: contents: write`
(scoped to that job; the rest of the workflow keeps `contents: read`). Any new
third-party action that *is* added must be SHA-pinned to its commit (per the
repo's action-pinning convention and the annotated-tag trap previously hit in
CI).

The crates.io `publish` job stays independent and may run in parallel; a binary
smoke failure must not have published broken binaries, but crate publishing is a
separate concern.

## Validation & docs

- **First implementation step:** confirm the workspace builds for
  `x86_64-unknown-linux-musl` locally before wiring CI, to de-risk the
  `fuser`/bundled-SQLite changes. (Local environment can run heavy Rust
  tooling.)
- **README:** add a musl / Alpine + portability note — the static binaries, the
  runtime requirement of `fusermount3` / `fuse3` and `/dev/fuse`, and which
  artifact to pick.
- **Separate follow-up issue:** container images / Alpine APK packaging. Short,
  problem-description-only, per repo issue conventions.

## Risks / open items for the plan

- **fuser without `libfuse` actually mounts:** validate a real mount works with
  `default-features = false` on a normal glibc dev box before trusting it in the
  release flow (the local first-step build/mount check covers this).
- **Signal-handler wiring:** exact crate boundary (`musefs-fuse` vs
  `musefs-cli`) and how the unmounter handle is threaded through the blocking
  mount path. Must keep the pre-commit gate green (full workspace test suite),
  so the change needs accompanying tests where feasible.
- **glibc pin version:** choose a conservative glibc (e.g. 2.17 / 2.28) balancing
  portability against bundled-SQLite/`cc` requirements.
- **Alpine container FUSE in CI:** confirm `--device /dev/fuse --cap-add
  SYS_ADMIN` is sufficient on GitHub runners; document the fallback if a real
  mount can't run for an arch.
- **Pre-commit hook:** runs fmt, clippy `-D warnings`, the full workspace test
  suite, and ruff — each commit must be green; the `fuzz/` crate is outside the
  workspace and a format-layer signature change could break it (not expected
  here, but the fuser change touches `musefs-fuse` only).
