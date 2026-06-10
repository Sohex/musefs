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

### Keep `libfuse` disabled (it already is) and make that explicit

**Important correction:** `fuser 0.17` has `default = []` (no default features),
and its `build.rs` selects the pure-rust mount path on Linux whenever
`not(feature = "libfuse")` (verified in the vendored
`fuser-0.17.0/build.rs:11-15`). So the current `fuser = "0.17"` dependency
**already** uses the `fusermount3` shell-out path and **does not link libfuse**.
musl static linking already would not pull libfuse. There is no libfuse link to
"remove."

The change is therefore about *intent and future-proofing*, not switching off a
link:

```toml
# musefs-fuse/Cargo.toml and musefs-latencyfs/Cargo.toml
fuser = { version = "0.17", default-features = false }  # pure-rust fusermount3; never enable `libfuse` (static musl can't link it)
```

`default-features = false` is currently a no-op (the default set is empty) but
guards against a future fuser release adding `libfuse` to its defaults. The
macOS target dependency keeps its existing `macos-no-mount` feature.

- **`musefs-latencyfs` also depends on `fuser`** (`musefs-latencyfs/Cargo.toml:11`).
  Cargo unifies features across the workspace, so for the "never link libfuse"
  intent to hold for any `cargo build`/`clippy --all-targets`, both crates carry
  `default-features = false`. (The release artifact itself is built `-p musefs`,
  which does not pull `musefs-latencyfs`, but the explicit flag keeps the whole
  workspace consistent.)

**Why musl works without libfuse:**

- musefs already mounts through `fusermount3` (see the handshake comment at
  `musefs-fuse/src/lib.rs:507`), not a privileged libfuse syscall path.
- musefs uses **no** `MountOption::AutoUnmount` anywhere (only `RO`, `FSName`,
  and macOS-only `volname`/`noappledouble` customs — see
  `musefs-fuse/src/platform/mount.rs`). `AutoUnmount` is the only fuser feature
  that *requires* `libfuse` to be linked — which we deliberately don't enable,
  on any target.

**Consequence:** `AutoUnmount` was never available to musefs (it would require
enabling `libfuse`, which breaks static musl) and is not in use today. The
mechanism that would clean up a stale mount after a hard `SIGKILL`/segfault is
therefore unavailable by design; we cover the realistic lifecycle cases with a
portable signal handler (below). A hard `SIGKILL`/segfault still leaks a mount —
same as today.

**Optional CI cleanup (not required):** `release.yml`/`ci.yml` install
`libfuse3-dev` (dev headers, unused since nothing links libfuse) alongside
`fuse3` (which provides the required `fusermount3` runtime). The `libfuse3-dev`
install could be dropped, keeping only `fuse3`. The plan may do this or leave it;
it is harmless either way.

### Graceful unmount on SIGTERM/SIGINT

Today the blocking mount path (`mount_with` → `new_session` →
`session.spawn()` → `bg.join()` in `musefs-fuse/src/lib.rs`) installs no signal
handler. On SIGINT/SIGTERM the process dies without running
`BackgroundSession::Drop`, leaving a stale mount (`Transport endpoint is not
connected`) until manual `fusermount3 -u`. This is true today even with
libfuse linked.

Add a SIGTERM/SIGINT handler that triggers a graceful unmount before exit. This
covers the realistic lifecycle cases — Ctrl-C, `systemctl stop`, and container /
Lidarr stop all send SIGTERM — leaving only `SIGKILL`/segfault to manual
recovery (same as today). Works identically on glibc and musl; no `libfuse`
needed.

**Mechanism: external `fusermount3 -u`, not a fuser unmounter handle.**

The obvious approach — hold a fuser `SessionUnmounter` and call it from the
handler thread — **does not work** with fuser 0.17's public API (verified
against the vendored source):

- `Session::spawn()` does `std::mem::take` on the shared `Mount`
  (`fuser-0.17.0/src/session.rs:226`), moving it into the returned
  `BackgroundSession`. A `SessionUnmounter` obtained before `spawn()` then reads
  `None` and unmounts nothing.
- `BackgroundSession` exposes no non-consuming unmount handle (only
  `join(self)` / `umount_and_join(self)`, both consuming), and the blocking
  `Session::run()` is `pub(crate)`. The only public blocking-mount shape is the
  `spawn()` + `bg.join()` musefs already uses.

So the handler instead runs the external **`fusermount3 -u <mountpoint>`** (the
same command fuser's own pure-rust unmount path uses), falling back to
`fusermount -u` then `umount`. Unmounting EOFs the `/dev/fuse` channel, the
background session's worker returns, and `bg.join()` unblocks — clean exit,
mount removed.

The external-command design has a second, more durable justification than "the
fuser API forces it": the handler thread **never touches core state** — not the
slab, DB connections, or session locks. Pool workers and `poll_refresh` hold
those guards; a handler that tried to manipulate session state from the signal
thread could deadlock against an in-flight read. Shelling out and letting the
kernel drive the unmount sidesteps that entirely.

**Bounded exit (the unmount can fail or stall — don't hang past it):**

A plain `fusermount3 -u` returns `EBUSY` when the mount is busy, and for a
wedged backing store (dead NFS, a spun-down disk) an in-flight read may never
drain — so the unmount does not free the channel and `bg.join()` would block
indefinitely. Because installing the handler *replaces* SIGTERM's default
"terminate immediately" disposition, an unbounded handler would be **worse than
none** in that case: the process lingers until the init system's SIGKILL
(systemd's default `TimeoutStopSec` is 90s). So the handler is bounded:

1. First signal: attempt the unmount chain (errors are logged and **swallowed** —
   the goal is exit, not unmount success; the mount may already be gone via a
   manual `fusermount3 -u` or an `ENOTCONN` dead endpoint).
2. If the blocking mount hasn't returned within a grace window (`GRACE`, 5s),
   escalate to a lazy detach (`fusermount3 -u -z`, `MNT_DETACH`) and
   `std::process::exit` — never wait on a join that may never come.
3. **Second** SIGTERM/SIGINT while step 1–2 is in flight: hard-exit immediately
   (`exit(130)`) — the operator/init system wants out now; "Ctrl-C twice does
   nothing" is a real daemon footgun.

On the happy path the unmount EOFs `/dev/fuse`, `bg.join()` returns, and `main`
exits on its own well before `GRACE` (the handler thread is torn down with the
process), so the escalation is invisible.

**Placement and constraints:**

- The handler is installed in the **CLI's blocking mount path**
  (`musefs-cli::run_mount` / the `musefs` binary), **not** in the
  `musefs-fuse` library functions (`mount_with` / `spawn_with`). A library
  function must not install a process-global signal handler — it would hijack
  signals for the in-process FUSE e2e harness and any embedder. `musefs-fuse`
  stays a pure library; `run_mount` already has the `mountpoint` the handler
  needs.
- The workspace denies `unsafe_code`, so raw `libc::sigaction` is out. Use the
  `signal-hook` crate (e.g. `signal_hook::iterator::Signals` on a dedicated
  thread) for safe SIGTERM/SIGINT handling. New dependency in `musefs-cli`.

**Test strategy (must land as a single green commit — the pre-commit hook runs
the full workspace suite):**

- An `--ignored` e2e test (gated on `/dev/fuse` like the existing FUSE e2e):
  mount the real binary as a child, send `SIGTERM`, then assert it exited
  cleanly and the mountpoint is no longer a FUSE mount. This exercises the real
  `fusermount3 -u` path.
- A second `--ignored` e2e for the **bounded-exit** guarantee: hold an open fd
  on a file inside the mount so a non-lazy unmount fails `EBUSY` (a
  deterministic stand-in for the wedged-backing-store case), send `SIGTERM`, and
  assert the daemon still exits within a bound (≈`GRACE`) via the lazy-detach +
  forced-exit escalation — rather than hanging until SIGKILL.
- Non-ignored unit coverage where feasible (e.g. the handler wiring / unmount
  command construction is unit-testable without an actual mount).
- The default (non-ignored) suite must stay green without `/dev/fuse`, so the
  signal-path e2e is `--ignored`, consistent with the rest of the FUSE e2e.

## Build: `cargo-zigbuild`

Build all four targets from a single amd64 host using `cargo-zigbuild`, which
uses Zig as the cross-linker and C compiler. Zig compiles the bundled SQLite
(`rusqlite` with `bundled`) for every target without Docker or per-target host
toolchains.

- Install Zig (e.g. `mlugg/setup-zig`) and `cargo-zigbuild`.
- `rustup target add` the four triples.
- Build: `cargo zigbuild --release -p musefs --target <triple>`.
- **glibc portability:** pin glibc to **2.17** via the zigbuild target suffix
  (`x86_64-unknown-linux-gnu.2.17`, `aarch64-unknown-linux-gnu.2.17`). 2.17 is
  the manylinux2014 floor (CentOS 7-era) and zigbuild's well-trodden default, so
  glibc binaries run on essentially any current distro. The local de-risking
  build (below) must confirm bundled SQLite compiles against this floor.
- **musl:** static by default — no extra flags.
- **Strip:** add `strip = "debuginfo"` to `[profile.release]` in the workspace
  `Cargo.toml`. Use `"debuginfo"` rather than `true`/`"symbols"`: it drops the
  bulk of the size (debug info) but **keeps the symbol table**, so field panic
  backtraces stay symbolicated. This profile also governs local `cargo build
  --release` debugging builds, so a bus-factor-one maintainer keeps actionable
  backtraces. (Panic *messages* print regardless of stripping; only backtrace
  symbolication is at stake.)

### Packaging

Each target produces:

- `musefs-<version>-<triple>.tar.gz` (the stripped binary)
- `musefs-<version>-<triple>.tar.gz.sha256`

`<version>` is the workspace version (already verified to match the tag by the
existing `release.yml` step). The `.sha256` is written in `sha256sum`
two-column format (`<hash>␠␠<filename>`) so users can verify with
`sha256sum -c <file>.sha256`; the README instructions must match.

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
The aarch64 binaries are **cross-built on the amd64 host** and the **exact
artifact** is downloaded and smoked on the arm64 runner — no rebuild on arm.
That is the whole point of cross-building; the plan must not re-run `cargo
zigbuild` on the arm runner.

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

- **First implementation milestone (de-risk before any CI wiring):** locally,
  via `cargo-zigbuild`, build + strip + *run* the binary for **all four
  targets**, with special attention to `aarch64-unknown-linux-musl` (bundled
  SQLite C cross-compile is the highest build risk) and the glibc-2.17 pin.
  Confirm a real `fusermount3` mount works with the pure-rust fuser path on a
  normal glibc dev box. Only proceed to CI once all four build and the mount
  works. (Local environment can run heavy Rust tooling.)
- **README:** add a musl / Alpine + portability note — the static binaries, the
  runtime requirement of `fusermount3` / `fuse3` and `/dev/fuse`, and which
  artifact to pick.
- **Separate follow-up issue:** container images / Alpine APK packaging. Short,
  problem-description-only, per repo issue conventions.

## Risks / open items for the plan

Highest first. The top two are the reasons the first milestone is "build all four
locally before any CI."

- **`aarch64-unknown-linux-musl` + bundled SQLite via zigbuild** is the single
  highest build risk (cross-compiled C). De-risked by the local all-four build
  milestone; if it proves intractable, that one cell is the candidate to drop
  with an explicit logged note (not a silent skip).
- **Alpine container FUSE in CI:** the existing e2e mounts directly on the
  runner, not in a container — mounting inside Alpine (`--device /dev/fuse
  --cap-add SYS_ADMIN`, possibly `--security-opt apparmor:unconfined`) is a new,
  untested capability here. Fallback is the logged `--version`-only smoke per the
  smoke-test section.
- **Native arm64 runner label:** `ubuntu-24.04-arm` is free for public repos as
  of the cutoff and musefs is public/MIT, but treat the label as a value to
  confirm at implementation time, not a guarantee.
- **Signal-handler e2e as a single green commit:** the unmount-on-SIGTERM test
  is `--ignored` (needs `/dev/fuse`), so the default suite the pre-commit hook
  runs stays green; verify the handler wiring lands with its tests in one commit
  (red-test intermediate commits are rejected by the hook).
- **Pre-commit hook** runs fmt, clippy `-D warnings`, the full workspace test
  suite, and ruff — each commit must be green. The `fuser` dependency change
  touches `musefs-fuse` and `musefs-latencyfs` only; the `fuzz/` crate is outside
  the workspace and unaffected (no format-layer signature change here).

**Decided (previously open):** build tool (`cargo-zigbuild`); glibc floor
(2.17); strip (`[profile.release] strip = true`); checksum format (`sha256sum`
two-column); signal-handler mechanism (external `fusermount3 -u`, installed in
the CLI path, not the library).
