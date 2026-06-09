# musl + glibc release binaries — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Publish four portable binaries on every `v*` tag — `{glibc, musl} × {x86_64, aarch64}` — each verified by a real FUSE mount smoke, with a graceful SIGTERM/SIGINT unmount in the CLI.

**Architecture:** A local de-risk milestone proves all four targets build via `cargo-zigbuild` (Zig as cross-linker/C-compiler for the bundled SQLite). Two small source changes land first (make the no-libfuse choice explicit; add a `fusermount3 -u`-based signal handler in the CLI mount path). A reusable in-tree smoke script exercises a built binary end-to-end. Finally `release.yml` gains build → smoke → release-asset jobs that cross-build on amd64, smoke each artifact on its native runner (musl inside an Alpine container via `docker run`), and upload to the GitHub Release with `gh`.

**Tech Stack:** Rust (2024 edition), `cargo-zigbuild` + Zig, `signal-hook`, `rustix` (test-only), GitHub Actions, `gh` CLI, Alpine/Docker, ffmpeg (smoke fixture), `fusermount3`/`fuse3`.

**Spec:** `docs/superpowers/specs/2026-06-09-musl-glibc-release-binaries-design.md`

---

## The four targets (referenced throughout)

| # | Rust triple (artifact name) | zigbuild `--target` | Smoke runner | Smoke mode |
| - | --------------------------- | ------------------- | ------------ | ---------- |
| 1 | `x86_64-unknown-linux-gnu`   | `x86_64-unknown-linux-gnu.2.17`  | `ubuntu-latest`   | host (apt `fuse3`) |
| 2 | `aarch64-unknown-linux-gnu`  | `aarch64-unknown-linux-gnu.2.17` | `ubuntu-24.04-arm`| host (apt `fuse3`) |
| 3 | `x86_64-unknown-linux-musl`  | `x86_64-unknown-linux-musl`      | `ubuntu-latest`   | Alpine `docker run` |
| 4 | `aarch64-unknown-linux-musl` | `aarch64-unknown-linux-musl`     | `ubuntu-24.04-arm`| Alpine `docker run` |

Artifact form throughout (build output, CI artifact, release asset): `musefs-<version>-<triple>.tar.gz` containing a single stripped, executable `musefs` at the archive root, plus a `musefs-<version>-<triple>.tar.gz.sha256` in `sha256sum` two-column format.

**Pinned action SHAs already used in this repo (reuse verbatim):**
- `actions/checkout@df4cb1c069e1874edd31b4311f1884172cec0e10`
- `dtolnay/rust-toolchain@29eef336d9b2848a0b548edc03f92a220660cdb8`
- `Swatinem/rust-cache@e18b497796c12c097a38f9edb9d0641fb99eee32`

---

## Task 0: Local de-risk milestone (no commit)

**Purpose:** Prove all four targets build + run before writing any CI, and capture the exact `zig` and `cargo-zigbuild` versions the workflow will pin. This is the spec's first milestone. The highest risk is `aarch64-unknown-linux-musl` + bundled SQLite and the glibc-2.17 floor.

**Files:** none (local only).

- [ ] **Step 1: Install the toolchain locally**

```bash
# Zig (pin a known-good version; this exact version goes into the workflow).
ZIG_VERSION=0.13.0
curl -fsSL "https://ziglang.org/download/${ZIG_VERSION}/zig-linux-x86_64-${ZIG_VERSION}.tar.xz" | tar -xJ
export PATH="$PWD/zig-linux-x86_64-${ZIG_VERSION}:$PATH"
zig version

# cargo-zigbuild (prefer the prebuilt release binary; pin the version).
cargo install --locked cargo-zigbuild
cargo zigbuild --version

rustup target add x86_64-unknown-linux-gnu aarch64-unknown-linux-gnu \
                 x86_64-unknown-linux-musl aarch64-unknown-linux-musl
```

- [ ] **Step 2: Build all four targets and run each that can run locally**

```bash
for T in x86_64-unknown-linux-gnu.2.17 aarch64-unknown-linux-gnu.2.17 \
         x86_64-unknown-linux-musl aarch64-unknown-linux-musl; do
  echo "=== $T ==="
  cargo zigbuild --release -p musefs --target "$T"
done
# amd64 ones run on this host:
./target/x86_64-unknown-linux-gnu/release/musefs --version
./target/x86_64-unknown-linux-musl/release/musefs --version
file ./target/x86_64-unknown-linux-musl/release/musefs   # expect: statically linked
# aarch64 ones: confirm they built; optionally run under qemu-user if installed:
#   qemu-aarch64-static ./target/aarch64-unknown-linux-musl/release/musefs --version
```

Expected: all four compile (bundled SQLite cross-compiles); the musl binaries report "statically linked".

- [ ] **Step 3: Confirm a real FUSE mount works with the pure-rust fuser path**

```bash
cargo test -p musefs-fuse -- --ignored
```

Expected: the existing FUSE e2e tests pass (this is today's behavior; it confirms the mount path before any change).

- [ ] **Step 4: Record the working versions**

Note the exact `ZIG_VERSION` and `cargo-zigbuild` version that worked — Tasks 4 pins these. If `aarch64-unknown-linux-musl` cannot be made to build after reasonable effort, STOP and report back: that cell is the spec's documented drop candidate and the matrix below must be trimmed with a logged note (do not silently skip).

**No commit** — this milestone gates the rest.

---

## Task 1: Make the no-libfuse choice explicit + strip release binaries

**Files:**
- Modify: `musefs-fuse/Cargo.toml:13`
- Modify: `musefs-latencyfs/Cargo.toml:11`
- Modify: `Cargo.toml` (workspace root — add `[profile.release]`)

Background (verified against vendored `fuser-0.17.0/build.rs:11-15`): `fuser` has `default = []`, so on Linux the pure-rust `fusermount3` path is already used and **no libfuse is linked today**. `default-features = false` is a no-op now but documents intent and guards a future fuser default change. This task is functionally a no-op; the existing suite is the test.

- [ ] **Step 1: Pin `default-features = false` on both fuser deps**

In `musefs-fuse/Cargo.toml`, change line 13 from:

```toml
fuser = "0.17"
```

to:

```toml
# Pure-rust fusermount3 mount path; never enable `libfuse` (static musl can't link it).
fuser = { version = "0.17", default-features = false }
```

In `musefs-latencyfs/Cargo.toml`, change line 11 from:

```toml
fuser = "0.17"
```

to:

```toml
# Pure-rust fusermount3 mount path; never enable `libfuse` (keeps workspace feature unification consistent).
fuser = { version = "0.17", default-features = false }
```

Leave the macOS-only `fuser = { version = "0.17", features = ["macos-no-mount"] }` target dependency in `musefs-fuse/Cargo.toml` unchanged.

- [ ] **Step 2: Add a release strip profile to the workspace root**

Append to `/home/cfutro/git/musefs/Cargo.toml` (after the existing `[workspace.lints.*]` blocks):

```toml
[profile.release]
strip = true
```

- [ ] **Step 3: Build to confirm nothing broke**

Run: `cargo build --workspace`
Expected: clean build (no new warnings/errors).

- [ ] **Step 4: Confirm the mount still works (manual, not in pre-commit)**

Run: `cargo test -p musefs-fuse -- --ignored`
Expected: PASS — mounting via the pure-rust path is unaffected.

- [ ] **Step 5: Commit**

```bash
git add musefs-fuse/Cargo.toml musefs-latencyfs/Cargo.toml Cargo.toml
git commit -m "build: pin fuser to no-libfuse explicitly; strip release binaries

fuser 0.17 already uses the pure-rust fusermount3 path on Linux (default
features are empty); make that explicit on both fuser-dependent crates so a
future fuser default change can't pull libfuse and break static musl builds.
Add [profile.release] strip = true for smaller release artifacts."
```

(The pre-commit hook runs fmt, clippy `-D warnings`, the full workspace suite, and ruff. The `Cargo.lock` may update; if `git status` shows it, `git add Cargo.lock` and amend into this commit before it lands — the hook will have regenerated it.)

---

## Task 2: SIGTERM/SIGINT graceful-unmount handler in the CLI

**Files:**
- Modify: `musefs-cli/Cargo.toml` (add `signal-hook` dependency)
- Create: `musefs-cli/src/signal.rs`
- Modify: `musefs-cli/src/lib.rs` (declare `mod signal`; call the installer in `run_mount`)
- Modify: `musefs/Cargo.toml` (add `[dev-dependencies]` for the subprocess e2e)
- Create: `musefs/tests/sigterm_unmount.rs`

The handler lives in the **CLI binary path only** — never in the `musefs-fuse` library — so it can't hijack signals for the in-process e2e harness or any embedder. On SIGTERM/SIGINT it runs `fusermount3 -u <mountpoint>` (fallbacks `fusermount -u`, then `umount`), which EOFs `/dev/fuse` and lets the blocking `mount_with` return cleanly. (A `SessionUnmounter` handle does not work: `Session::spawn()` `mem::take`s the `Mount` and `Session::run()` is `pub(crate)`.)

- [ ] **Step 1: Write the failing unit test for the unmount command list**

Create `musefs-cli/src/signal.rs` with only this test module to start:

```rust
//! CLI-only graceful unmount on stop signals. Installed by `run_mount`; never
//! in the `musefs-fuse` library (which must not hijack process signals).

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;
    use std::path::Path;

    #[test]
    fn unmount_commands_try_fusermount3_then_fallbacks() {
        let cmds = unmount_commands(Path::new("/mnt/x"));
        let progs: Vec<&str> = cmds.iter().map(|(p, _)| *p).collect();
        assert_eq!(progs, ["fusermount3", "fusermount", "umount"]);
        // fusermount variants pass `-u <mp>`; umount passes just `<mp>`.
        assert_eq!(
            cmds[0].1,
            vec![OsString::from("-u"), OsString::from("/mnt/x")]
        );
        assert_eq!(cmds[2].1, vec![OsString::from("/mnt/x")]);
    }
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p musefs-cli unmount_commands_try_fusermount3_then_fallbacks`
Expected: FAIL — `unmount_commands` not found (compile error).

- [ ] **Step 3: Implement the signal module**

Prepend to `musefs-cli/src/signal.rs` (above the test module):

```rust
use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// Unmount commands tried in order, most-preferred first, as `(program, args)`.
/// `fusermount3`/`fusermount` are the unprivileged FUSE unmount tools; `umount`
/// is the last resort.
fn unmount_commands(mountpoint: &Path) -> Vec<(&'static str, Vec<OsString>)> {
    let mp = mountpoint.as_os_str().to_owned();
    vec![
        ("fusermount3", vec!["-u".into(), mp.clone()]),
        ("fusermount", vec!["-u".into(), mp.clone()]),
        ("umount", vec![mp]),
    ]
}

/// Try each unmount command until one succeeds. Best-effort: a stop signal must
/// never panic the process.
fn run_unmount(mountpoint: &Path) {
    for (prog, args) in unmount_commands(mountpoint) {
        if let Ok(status) = std::process::Command::new(prog).args(&args).status() {
            if status.success() {
                return;
            }
        }
    }
    log::warn!(
        "could not unmount {} after stop signal; run `fusermount3 -u {}` manually",
        mountpoint.display(),
        mountpoint.display()
    );
}

/// Spawn a thread that unmounts `mountpoint` on the first SIGTERM/SIGINT, so a
/// `Ctrl-C` / `systemctl stop` / container stop unwinds the blocking mount
/// cleanly instead of leaving a stale FUSE endpoint. CLI-only.
pub fn install_unmount_on_signal(mountpoint: PathBuf) -> std::io::Result<()> {
    use signal_hook::consts::{SIGINT, SIGTERM};
    use signal_hook::iterator::Signals;

    let mut signals = Signals::new([SIGINT, SIGTERM])?;
    std::thread::Builder::new()
        .name("musefs-unmount-on-signal".into())
        .spawn(move || {
            if signals.forever().next().is_some() {
                run_unmount(&mountpoint);
            }
        })?;
    Ok(())
}
```

- [ ] **Step 4: Add the `signal-hook` dependency**

In `musefs-cli/Cargo.toml`, under `[dependencies]`, add:

```toml
signal-hook = "0.3"
log = "0.4"
```

(`log` is needed for the `log::warn!` in `run_unmount`; add it if not already present.)

- [ ] **Step 5: Wire the module and installer into the CLI**

In `musefs-cli/src/lib.rs`, add the module declaration near the top (with the other items):

```rust
mod signal;
```

Then change `run_mount` (currently `musefs-cli/src/lib.rs:183-193`) to install the handler before the blocking mount:

```rust
/// Build a `Musefs` from the DB at `args.db` and mount it (blocking) at
/// `args.mountpoint`.
pub fn run_mount(args: &MountArgs) -> Result<()> {
    let db =
        Db::open(&args.db).with_context(|| format!("opening database at {}", args.db.display()))?;
    let (config, fuse_config) = parse_mount_config(args);
    let core = Musefs::open(db, config).context("building the virtual filesystem")?;
    signal::install_unmount_on_signal(args.mountpoint.clone())
        .context("installing the stop-signal unmount handler")?;
    musefs_fuse::mount_with(core, &args.mountpoint, "musefs", fuse_config)
        .with_context(|| format!("mounting at {}", args.mountpoint.display()))?;
    Ok(())
}
```

- [ ] **Step 6: Run the unit test to verify it passes**

Run: `cargo test -p musefs-cli unmount_commands_try_fusermount3_then_fallbacks`
Expected: PASS.

- [ ] **Step 7: Add the subprocess e2e test (real SIGTERM unmount)**

In `musefs/Cargo.toml`, add (the binary crate currently has no test deps):

```toml
[dev-dependencies]
tempfile = "3"
rustix = { version = "1", features = ["process"] }
```

Create `musefs/tests/sigterm_unmount.rs`:

```rust
//! End-to-end: the `musefs` binary unmounts cleanly when sent SIGTERM, via the
//! CLI's fusermount3-based stop-signal handler. Ignored by default (needs
//! /dev/fuse + fusermount3), like the other FUSE e2e tests.

use std::process::{Child, Command};
use std::time::{Duration, Instant};

// --- minimal proven FLAC fixture (mirrors musefs-fuse/tests/mount.rs) ---

fn flac_block(block_type: u8, body: &[u8], is_last: bool) -> Vec<u8> {
    let mut out = Vec::new();
    out.push((if is_last { 0x80 } else { 0 }) | (block_type & 0x7F));
    let len = body.len();
    out.push(u8::try_from((len >> 16) & 0xFF).unwrap());
    out.push(u8::try_from((len >> 8) & 0xFF).unwrap());
    out.push(u8::try_from(len & 0xFF).unwrap());
    out.extend_from_slice(body);
    out
}

fn streaminfo_body() -> Vec<u8> {
    let mut b = vec![
        0x10, 0x00, 0x10, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x0A, 0xC4, 0x42, 0xF0, 0x00,
        0x00, 0x00, 0x00,
    ];
    b.extend_from_slice(&[0u8; 16]);
    b
}

fn vorbis_comment_body(vendor: &str, comments: &[&str]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&u32::try_from(vendor.len()).unwrap().to_le_bytes());
    out.extend_from_slice(vendor.as_bytes());
    out.extend_from_slice(&u32::try_from(comments.len()).unwrap().to_le_bytes());
    for c in comments {
        out.extend_from_slice(&u32::try_from(c.len()).unwrap().to_le_bytes());
        out.extend_from_slice(c.as_bytes());
    }
    out
}

fn make_flac(comments: &[&str], audio: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(b"fLaC");
    out.extend_from_slice(&flac_block(0, &streaminfo_body(), false));
    out.extend_from_slice(&flac_block(4, &vorbis_comment_body("orig", comments), true));
    out.extend_from_slice(audio);
    out
}

fn wait_until(mut cond: impl FnMut() -> bool, timeout: Duration) -> bool {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if cond() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    cond()
}

fn wait_exit(child: &mut Child, timeout: Duration) -> Option<std::process::ExitStatus> {
    let start = Instant::now();
    while start.elapsed() < timeout {
        match child.try_wait().unwrap() {
            Some(status) => return Some(status),
            None => std::thread::sleep(Duration::from_millis(100)),
        }
    }
    None
}

#[test]
#[ignore = "requires /dev/fuse + fusermount3; run with: cargo test -p musefs -- --ignored"]
fn sigterm_unmounts_cleanly() {
    let bin = env!("CARGO_BIN_EXE_musefs");

    // Backing dir + on-disk DB scanned via the real binary.
    let backing = tempfile::tempdir().unwrap();
    std::fs::write(
        backing.path().join("a.flac"),
        make_flac(&["ARTIST=Alice", "TITLE=Song"], &[0xAB; 64]),
    )
    .unwrap();
    let dbfile = tempfile::NamedTempFile::new().unwrap();
    let db = dbfile.path().to_str().unwrap();
    let scan = Command::new(bin)
        .args(["scan", backing.path().to_str().unwrap(), "--db", db])
        .status()
        .unwrap();
    assert!(scan.success(), "scan failed");

    // Mount as a child process.
    let mp = tempfile::tempdir().unwrap();
    let mut child = Command::new(bin)
        .args(["mount", mp.path().to_str().unwrap(), "--db", db])
        .spawn()
        .unwrap();

    let song = mp.path().join("Alice").join("Song.flac");
    assert!(
        wait_until(|| song.exists(), Duration::from_secs(15)),
        "mount did not come up"
    );

    // Send SIGTERM and assert a clean exit + unmounted mountpoint.
    let pid = rustix::process::Pid::from_child(&child);
    rustix::process::kill_process(pid, rustix::process::Signal::TERM).unwrap();

    let status = wait_exit(&mut child, Duration::from_secs(15))
        .unwrap_or_else(|| panic!("daemon did not exit after SIGTERM"));
    assert!(status.success(), "daemon exited non-zero: {status:?}");
    assert!(
        !song.exists(),
        "mount still present after SIGTERM (stale endpoint)"
    );
}
```

- [ ] **Step 8: Verify the e2e test compiles and passes**

Run (compile-check, ignored, runs by default in pre-commit only as a compile):
```bash
cargo test -p musefs --no-run
cargo test -p musefs -- --ignored sigterm_unmounts_cleanly
```
Expected: first compiles clean; second PASSES (mounts, SIGTERM, clean unmount). If `rustix::process::Signal::TERM` or `Pid::from_child` differ in the installed rustix `1.x`, adjust to the actual API (`rustix::process::test_kill_process`/`Signal` variants) — confirm via `cargo doc -p rustix --open` and keep it `unsafe`-free.

- [ ] **Step 9: Full gate + commit**

```bash
cargo clippy --all-targets -- -D warnings
cargo test --workspace        # ignored e2e is skipped here; must be green
git add musefs-cli/Cargo.toml musefs-cli/src/signal.rs musefs-cli/src/lib.rs \
        musefs/Cargo.toml musefs/tests/sigterm_unmount.rs Cargo.lock
git commit -m "feat(cli): unmount cleanly on SIGTERM/SIGINT

Install a CLI-only stop-signal handler that runs fusermount3 -u on the
mountpoint, so Ctrl-C / systemctl stop / container stop unwind the blocking
mount instead of leaving a stale FUSE endpoint. Kept out of the musefs-fuse
library so it never hijacks signals for the e2e harness or embedders. Covered
by a unit test for the command list and an --ignored subprocess e2e."
```

Expected: pre-commit passes (full suite green; ignored e2e compiled but not run).

---

## Task 3: Reusable in-tree smoke script

**Files:**
- Create: `scripts/smoke-binary.sh`

POSIX `sh` so it runs under both bash (host) and busybox ash (Alpine). It scans an ffmpeg-generated FLAC, mounts the binary, reads the synthesized file, then SIGTERMs the daemon and asserts a clean unmount — exercising the real artifact and the Task 2 handler.

- [ ] **Step 1: Write the script**

Create `scripts/smoke-binary.sh`:

```sh
#!/bin/sh
# Smoke-test a built musefs binary end-to-end: generate a tagged FLAC, scan it,
# mount the binary, read the synthesized file through the mount, then SIGTERM the
# daemon and assert the mount unmounts cleanly.
#
# POSIX sh (runs under bash and busybox ash). Requires on PATH: ffmpeg,
# fusermount3 (fuse3 package), and /dev/fuse present.
#
# Usage: scripts/smoke-binary.sh /path/to/musefs
set -eu

MUSEFS="$1"
WORK="$(mktemp -d)"
cleanup() { fusermount3 -u "$WORK/mnt" 2>/dev/null || true; rm -rf "$WORK"; }
trap cleanup EXIT

echo "smoke: musefs = $MUSEFS"
"$MUSEFS" --version

mkdir -p "$WORK/backing" "$WORK/mnt"

# 1s tagged FLAC fixture.
ffmpeg -hide_banner -loglevel error -f lavfi -i "sine=frequency=440:duration=1" \
  -metadata artist=Alice -metadata title=Song "$WORK/backing/a.flac"

"$MUSEFS" scan "$WORK/backing" --db "$WORK/smoke.db"

"$MUSEFS" mount "$WORK/mnt" --db "$WORK/smoke.db" &
PID=$!

SONG="$WORK/mnt/Alice/Song.flac"
i=0
while [ ! -f "$SONG" ]; do
  i=$((i + 1))
  if [ "$i" -gt 30 ]; then echo "FAIL: mount did not come up"; exit 1; fi
  sleep 1
done

# Served file must be a real, non-empty FLAC (magic 'fLaC').
MAGIC="$(head -c 4 "$SONG")"
if [ "$MAGIC" != "fLaC" ]; then echo "FAIL: served file is not FLAC (magic='$MAGIC')"; exit 1; fi
BYTES="$(wc -c < "$SONG")"
if [ "$BYTES" -le 0 ]; then echo "FAIL: served file is empty"; exit 1; fi
echo "smoke: read $BYTES bytes from $SONG (fLaC OK)"

# Exercise the SIGTERM graceful-unmount handler on the real binary.
kill -TERM "$PID"
i=0
while kill -0 "$PID" 2>/dev/null; do
  i=$((i + 1))
  if [ "$i" -gt 30 ]; then echo "FAIL: daemon did not exit after SIGTERM"; exit 1; fi
  sleep 1
done
wait "$PID" 2>/dev/null || true

if [ -f "$SONG" ]; then echo "FAIL: mount still present after SIGTERM"; exit 1; fi
echo "smoke: SIGTERM unmounted cleanly — PASS"
```

- [ ] **Step 2: Make it executable and run it against a local build**

```bash
chmod +x scripts/smoke-binary.sh
# Needs ffmpeg + fuse3 locally; uses the binary from Task 0/Task 1.
./scripts/smoke-binary.sh ./target/x86_64-unknown-linux-gnu/release/musefs
```

Expected output ends with `smoke: SIGTERM unmounted cleanly — PASS`. If `sleep 1` granularity feels slow locally that's fine — CI mounts come up in <1s.

- [ ] **Step 3: Lint the script (best-effort) and commit**

```bash
command -v shellcheck >/dev/null && shellcheck scripts/smoke-binary.sh || echo "shellcheck not installed; skipping"
git add scripts/smoke-binary.sh
git commit -m "test: add reusable musefs binary smoke script

POSIX-sh end-to-end smoke (generate FLAC, scan, mount, read, SIGTERM,
assert clean unmount) reused by the release smoke jobs for all four targets."
```

---

## Task 4: Release workflow — cross-build job

**Files:**
- Modify: `.github/workflows/release.yml`

Add a `build` matrix job that cross-builds all four targets on amd64 via `cargo-zigbuild`, packages each as a `.tar.gz` (+ `.sha256`), and uploads them as workflow artifacts. The existing `publish` (crates.io) job is left untouched.

- [ ] **Step 1: Resolve and pin the artifact-action SHAs**

`upload-artifact`/`download-artifact` are new to this repo. Pin them to commit SHAs (the repo convention; the annotated-tag object SHA would fail at run time — use the commits endpoint):

```bash
gh api repos/actions/upload-artifact/commits/v4 --jq .sha
gh api repos/actions/download-artifact/commits/v4 --jq .sha
```

Use the returned 40-char SHAs in place of `<UPLOAD_ARTIFACT_SHA>` / `<DOWNLOAD_ARTIFACT_SHA>` below (and in Task 5).

- [ ] **Step 2: Add the `build` job**

In `.github/workflows/release.yml`, keep `permissions: contents: read` at the top level. Add this job (sibling of `publish`). Replace `0.13.0` / `0.19.8` with the versions recorded in Task 0 Step 4:

```yaml
  build:
    runs-on: ubuntu-latest
    strategy:
      fail-fast: false
      matrix:
        include:
          - triple: x86_64-unknown-linux-gnu
            zig_target: x86_64-unknown-linux-gnu.2.17
          - triple: aarch64-unknown-linux-gnu
            zig_target: aarch64-unknown-linux-gnu.2.17
          - triple: x86_64-unknown-linux-musl
            zig_target: x86_64-unknown-linux-musl
          - triple: aarch64-unknown-linux-musl
            zig_target: aarch64-unknown-linux-musl
    steps:
      - uses: actions/checkout@df4cb1c069e1874edd31b4311f1884172cec0e10
        with:
          persist-credentials: false
      - uses: dtolnay/rust-toolchain@29eef336d9b2848a0b548edc03f92a220660cdb8
      - uses: Swatinem/rust-cache@e18b497796c12c097a38f9edb9d0641fb99eee32
        with:
          key: ${{ matrix.triple }}
      - name: Install Zig and cargo-zigbuild
        env:
          ZIG_VERSION: "0.13.0"
          CARGO_ZIGBUILD_VERSION: "0.19.8"
        run: |
          set -euo pipefail
          curl -fsSL "https://ziglang.org/download/${ZIG_VERSION}/zig-linux-x86_64-${ZIG_VERSION}.tar.xz" | tar -xJ
          echo "$PWD/zig-linux-x86_64-${ZIG_VERSION}" >> "$GITHUB_PATH"
          curl -fsSL "https://github.com/rust-cross/cargo-zigbuild/releases/download/v${CARGO_ZIGBUILD_VERSION}/cargo-zigbuild-v${CARGO_ZIGBUILD_VERSION}.x86_64-unknown-linux-musl.tar.gz" \
            | tar -xz -C "$HOME/.cargo/bin" cargo-zigbuild
      - name: Add Rust target
        run: rustup target add ${{ matrix.triple }}
      - name: Build
        run: cargo zigbuild --release -p musefs --target ${{ matrix.zig_target }}
      - name: Package
        run: |
          set -euo pipefail
          VERSION="${GITHUB_REF_NAME#v}"
          NAME="musefs-${VERSION}-${{ matrix.triple }}"
          mkdir -p dist
          tar -C "target/${{ matrix.triple }}/release" -czf "dist/${NAME}.tar.gz" musefs
          ( cd dist && sha256sum "${NAME}.tar.gz" > "${NAME}.tar.gz.sha256" )
          ls -l dist
      - name: Upload artifact
        uses: actions/upload-artifact@<UPLOAD_ARTIFACT_SHA>
        with:
          name: musefs-${{ matrix.triple }}
          path: dist/*
          if-no-files-found: error
          retention-days: 7
```

Note: `cargo zigbuild --target x86_64-unknown-linux-gnu.2.17` still emits to `target/x86_64-unknown-linux-gnu/release/musefs` (the glibc suffix doesn't change the output path), so the `Package` step uses the base `matrix.triple`.

- [ ] **Step 3: Validate the workflow YAML**

```bash
command -v actionlint >/dev/null && actionlint .github/workflows/release.yml \
  || python3 -c "import yaml,sys; yaml.safe_load(open('.github/workflows/release.yml')); print('yaml ok')"
```

Expected: no errors. (Full execution is only exercisable on a real `v*` tag — see Task 6 Step 4.)

- [ ] **Step 4: Commit**

```bash
git add .github/workflows/release.yml
git commit -m "ci(release): cross-build four target binaries with cargo-zigbuild

Build {glibc,musl} x {x86_64,aarch64} static/portable musefs binaries on the
amd64 runner via cargo-zigbuild (Zig cross-links the bundled SQLite). glibc
pinned to 2.17 for portability. Each target is packaged as a tar.gz + sha256
and uploaded as a workflow artifact."
```

---

## Task 5: Release workflow — smoke jobs

**Files:**
- Modify: `.github/workflows/release.yml`

Add a `smoke` matrix job. Each target downloads its own artifact, extracts the `.tar.gz` (preserving the executable bit), and runs `scripts/smoke-binary.sh`. glibc targets run on the host; musl targets run inside an Alpine container via `docker run` (the binary executes under musl; checkout/download happen on the glibc host, avoiding the Node-on-Alpine problem).

- [ ] **Step 1: Add the `smoke` job**

Append to `.github/workflows/release.yml`:

```yaml
  smoke:
    needs: build
    strategy:
      fail-fast: false
      matrix:
        include:
          - triple: x86_64-unknown-linux-gnu
            runner: ubuntu-latest
            mode: host
          - triple: aarch64-unknown-linux-gnu
            runner: ubuntu-24.04-arm
            mode: host
          - triple: x86_64-unknown-linux-musl
            runner: ubuntu-latest
            mode: alpine
          - triple: aarch64-unknown-linux-musl
            runner: ubuntu-24.04-arm
            mode: alpine
    runs-on: ${{ matrix.runner }}
    steps:
      - uses: actions/checkout@df4cb1c069e1874edd31b4311f1884172cec0e10
        with:
          persist-credentials: false
      - name: Download artifact
        uses: actions/download-artifact@<DOWNLOAD_ARTIFACT_SHA>
        with:
          name: musefs-${{ matrix.triple }}
          path: dist
      - name: Extract binary
        run: |
          set -euo pipefail
          tar -xzf dist/musefs-*-${{ matrix.triple }}.tar.gz -C .
          ./musefs --version || true   # glibc/musl host may differ; real check is the smoke
      - name: Smoke (host)
        if: matrix.mode == 'host'
        run: |
          set -euo pipefail
          sudo apt-get update && sudo apt-get install -y fuse3 ffmpeg
          ./scripts/smoke-binary.sh ./musefs
      - name: Smoke (Alpine container)
        if: matrix.mode == 'alpine'
        run: |
          set -euo pipefail
          docker run --rm \
            --device /dev/fuse --cap-add SYS_ADMIN --security-opt apparmor=unconfined \
            -v "$PWD":/w -w /w \
            alpine:3.20 \
            sh -c 'apk add --no-cache fuse3 ffmpeg >/dev/null && sh scripts/smoke-binary.sh ./musefs'
```

- [ ] **Step 2: Validate the workflow YAML**

```bash
command -v actionlint >/dev/null && actionlint .github/workflows/release.yml \
  || python3 -c "import yaml,sys; yaml.safe_load(open('.github/workflows/release.yml')); print('yaml ok')"
```

Expected: no errors.

- [ ] **Step 3: Commit**

```bash
git add .github/workflows/release.yml
git commit -m "ci(release): real FUSE mount smoke for each target binary

Each built artifact is smoke-tested on its native runner (amd64 on
ubuntu-latest, aarch64 on ubuntu-24.04-arm). musl binaries run inside an Alpine
container via docker run to prove they execute on musl; glibc binaries run on
the host. The smoke generates a FLAC, mounts the binary, reads the synthesized
file, and asserts SIGTERM unmounts cleanly."
```

---

## Task 6: Release workflow — upload assets to the GitHub Release

**Files:**
- Modify: `.github/workflows/release.yml`

Add a `release-assets` job gated on all four smokes; it downloads every artifact and uploads the tarballs + checksums to the tag's GitHub Release via `gh`. This job (and only this job) gets `contents: write`.

- [ ] **Step 1: Add the `release-assets` job**

Append to `.github/workflows/release.yml`:

```yaml
  release-assets:
    needs: smoke
    runs-on: ubuntu-latest
    permissions:
      contents: write
    steps:
      - name: Download all artifacts
        uses: actions/download-artifact@<DOWNLOAD_ARTIFACT_SHA>
        with:
          path: dist
          merge-multiple: true
      - name: Upload to GitHub Release
        env:
          GH_TOKEN: ${{ github.token }}
          GH_REPO: ${{ github.repository }}
        run: |
          set -euo pipefail
          ls -l dist
          # Create the release for this tag if it doesn't exist yet, then upload
          # (idempotent on re-run via --clobber).
          if ! gh release view "$GITHUB_REF_NAME" >/dev/null 2>&1; then
            gh release create "$GITHUB_REF_NAME" --verify-tag \
              --title "$GITHUB_REF_NAME" \
              --notes "Prebuilt binaries for ${GITHUB_REF_NAME}. Verify with: sha256sum -c <file>.sha256"
          fi
          gh release upload "$GITHUB_REF_NAME" dist/*.tar.gz dist/*.sha256 --clobber
```

- [ ] **Step 2: Validate the workflow YAML**

```bash
command -v actionlint >/dev/null && actionlint .github/workflows/release.yml \
  || python3 -c "import yaml,sys; yaml.safe_load(open('.github/workflows/release.yml')); print('yaml ok')"
```

Expected: no errors.

- [ ] **Step 3: Commit**

```bash
git add .github/workflows/release.yml
git commit -m "ci(release): publish target binaries as GitHub Release assets

Gated on all four smokes passing, a release-assets job (scoped contents:write)
creates/updates the tag's GitHub Release and uploads the tarballs + sha256
checksums via gh. The crates.io publish job is unchanged and independent."
```

- [ ] **Step 4: Document how to validate end-to-end (manual, externally visible — do NOT run without user sign-off)**

The full pipeline only runs on a real `v*` tag. To validate safely without a real release, the maintainer can push a throwaway pre-release tag (e.g. `v0.0.0-rc-musl`) on a branch, watch `gh run watch`, confirm four green smokes + four uploaded assets, then delete the tag/release. Note this in the handoff; it is an externally visible action requiring user sign-off.

---

## Task 7: Document the prebuilt binaries in the README

**Files:**
- Modify: `README.md` (add a section before `## Requirements` at line 170; lightly correct the Requirements note)

- [ ] **Step 1: Add a "Prebuilt binaries" section**

In `README.md`, insert immediately before the `## Requirements` heading (currently line 170):

```markdown
## Prebuilt binaries

Each tagged release attaches static/portable Linux binaries for four targets:

| Target | libc | Notes |
| --- | --- | --- |
| `x86_64-unknown-linux-gnu`  | glibc | Pinned to glibc 2.17 — runs on essentially any current distro. |
| `aarch64-unknown-linux-gnu` | glibc | glibc 2.17 floor, ARM64. |
| `x86_64-unknown-linux-musl`  | musl | Fully static — runs on Alpine / scratch containers. |
| `aarch64-unknown-linux-musl` | musl | Fully static, ARM64. |

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
```

- [ ] **Step 2: Correct the Requirements wording**

In `README.md`, change the Linux requirement bullet (currently line 173) from:

```markdown
- A supported OS with FUSE to mount — Linux (`/dev/fuse` + libfuse) or FreeBSD
```

to:

```markdown
- A supported OS with FUSE to mount — Linux (`/dev/fuse` + `fusermount3`, from
  the `fuse3` package) or FreeBSD
```

- [ ] **Step 3: Verify and commit**

```bash
# Sanity-check the markdown renders (no broken fences):
grep -n "Prebuilt binaries" README.md
git add README.md
git commit -m "docs: document prebuilt release binaries and the fuse3 runtime requirement"
```

---

## Task 8: Open the container-packaging follow-up issue (externally visible — confirm first)

**Files:** none (creates a GitHub issue).

Per the spec, container images / Alpine APK packaging are deferred. Open a short, problem-description-only issue (matching the repo's issue style: no proposed implementation, no schema, no open questions).

- [ ] **Step 1: Confirm with the user, then create the issue**

This is externally visible — get explicit sign-off before running:

```bash
gh issue create \
  --title "Publish container images for musefs" \
  --body "The release pipeline now ships standalone glibc and musl binaries for x86_64 and aarch64, but there is no official container image. Users running musefs alongside containerized media managers (e.g. Lidarr on Alpine) currently have to build their own image or install the binary into one by hand. Official multi-arch container images (glibc and musl based) would let those users pull and run musefs directly."
```

Expected: prints the new issue URL. Record it in the handoff.

---

## Self-Review

**Spec coverage:**
- Drop/keep libfuse explicit on both fuser crates → Task 1. ✅
- `[profile.release] strip` → Task 1. ✅
- SIGTERM/SIGINT graceful unmount via external `fusermount3 -u`, CLI-only, `signal-hook`, `--ignored` e2e → Task 2. ✅
- `cargo-zigbuild`, four targets, glibc 2.17 pin, packaging + sha256 two-column → Tasks 0, 4. ✅
- All-four local de-risk milestone first → Task 0. ✅
- Real mount smoke on native amd64/arm64 runners; musl inside Alpine; cross-built artifact is the smoked artifact → Tasks 3, 5. ✅
- GitHub Release upload via `gh`, `contents: write` scoped to one job, no new third-party action unpinned → Task 6 (artifact actions pinned via Task 4 Step 1). ✅
- README musl/Alpine + portability + fuse3 runtime note → Task 7. ✅
- Container packaging follow-up issue → Task 8. ✅
- Optional CI `libfuse3-dev` cleanup → intentionally left out (spec marked it optional/harmless).

**Placeholder scan:** The only intentionally-unresolved tokens are `<UPLOAD_ARTIFACT_SHA>` / `<DOWNLOAD_ARTIFACT_SHA>` (resolved by the exact `gh api` commands in Task 4 Step 1) and the `ZIG_VERSION` / `CARGO_ZIGBUILD_VERSION` values (resolved by Task 0 and carried forward). These are concrete resolve-then-fill steps, not vague placeholders.

**Type/name consistency:** `unmount_commands` / `run_unmount` / `install_unmount_on_signal` are defined in Task 2 Step 3 and used in Steps 1/5/6; the smoke script name `scripts/smoke-binary.sh` and the artifact name pattern `musefs-${triple}` / `musefs-<version>-<triple>.tar.gz` are consistent across Tasks 3–7. Matrix `triple`/`zig_target`/`runner`/`mode` keys match between Tasks 4 and 5.

**Residual risks (carried from spec):** `aarch64-unknown-linux-musl` bundled-SQLite cross-compile (gated by Task 0); Alpine `docker run --device /dev/fuse` mount capability on GH runners (Task 5 — if a cell can't mount, downgrade that cell to `--version` only with a logged note, never a silent skip); `ubuntu-24.04-arm` runner-label availability (confirm at Task 5 implementation).
