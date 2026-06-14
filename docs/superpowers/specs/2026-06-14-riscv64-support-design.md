# riscv64 support — design

**Date:** 2026-06-14
**Status:** approved (pending spec review)

## Goal

Ship `riscv64` as a first-class release platform with full parity to the
existing `x86_64`/`aarch64` targets: prebuilt glibc and musl binary tarballs,
multi-arch Docker images, and an end-to-end smoke test. No user-facing feature
change — musefs already runs on riscv64 in principle; this makes it built,
tested, and distributed.

## Why no source changes are needed

The workspace contains no architecture-specific code. All conditional
compilation is OS-gated (`#[cfg(target_os = "linux" | "macos")]`), never
arch-gated. The one width-sensitive helper, `musefs_db::convert::usize_from`,
is already guarded to 64-bit-only targets; `riscv64gc` is 64-bit, so it
compiles unchanged. The workspace bans `unsafe_code`, so there are no raw
syscalls or hand-written ABI that could differ per arch.

Rust targets used (both Tier 2 with `std`):

- `riscv64gc-unknown-linux-gnu`
- `riscv64gc-unknown-linux-musl`

No *Rust* source changes are needed. The one non-source caveat is the vendored
jemalloc C library, which is compiled per target — see Gotcha 3.

## Two toolchain gotchas (the load-bearing details)

### Gotcha 1 — zig must be bumped to 0.14.0

The release `build` job pins `ZIG_VERSION: "0.13.0"`. **Zig 0.13.0 (2024-06-05)
cannot build glibc for `riscv64-linux-gnu`** — that capability was added by
[ziglang/zig#20909](https://github.com/ziglang/zig/pull/20909) (merged
2024-08-07, closing [#3340](https://github.com/ziglang/zig/issues/3340)) and
first shipped in **zig 0.14.0**. So pinning the glibc version alone is not
enough; the toolchain itself must be bumped.

Required change: bump `ZIG_VERSION` to `0.14.0` (or later). This affects **all
four existing targets**, not just riscv64, so the local pre-merge validation
(below) must rebuild x86_64/aarch64 glibc+musl against the bumped zig as a
regression guard. `CARGO_ZIGBUILD_VERSION: "0.22.3"` must be confirmed
compatible with zig 0.14 before merge; if not, bump it to a version that
supports zig 0.14 in the same change (both version pins move together).

The musl riscv64 leg is unaffected by this gotcha (musl needs no glibc build),
but it still rides the same bumped zig.

### Gotcha 2 — glibc 2.27 baseline

The existing glibc targets pin `.2.17` (`x86_64-unknown-linux-gnu.2.17`,
`aarch64-unknown-linux-gnu.2.17`). **riscv64 Linux support did not land in glibc
until 2.27**, so reusing `.2.17` would fail. The riscv64 glibc target must pin
`.2.27`:

```
riscv64gc-unknown-linux-gnu.2.27
```

`.2.27` is intentionally *below* zig 0.14's default glibc for the target
(~2.28+) — it is the lowest floor riscv64 supports, chosen for maximum distro
reach. Keep the explicit `.2.27` suffix; do not "tidy" it away. The musl target
carries no version suffix, matching the existing musl entries.

### Gotcha 3 — jemalloc is a default-on C dependency, compiled per target

The `musefs` binary enables `jemalloc` as a **default feature**
(`default = ["jemalloc"]`), installing `tikv-jemallocator` as its
`#[global_allocator]`. The release build runs `cargo zigbuild --release -p
musefs` with no `--no-default-features`, so **the riscv64 binaries link
jemalloc**. `tikv-jemalloc-sys 0.7.1+5.3.1` vendors jemalloc 5.3.1 and compiles
it from C source at build time (autotools `./configure && make`) — under
cargo-zigbuild that means `zig cc` cross-compiling jemalloc for `riscv64gc`.

- **Runtime is not a concern.** jemalloc 5.3.x supports riscv64 Linux, which
  uses 4 KB pages (`LG_PAGE=12`, same as x86_64) — it avoids the variable
  page-size issue that complicates aarch64.
- **The build is the risk**, and it is the single most likely failure point of
  this whole effort. It is de-risked by the fact that the existing musl release
  targets already link jemalloc via the same cargo-zigbuild toolchain — so the
  jemalloc + musl + zigbuild combination is already proven; riscv64 is the only
  new variable. The local cross-compile (Task 2 / Testing) exercises it with
  default features, so a failure surfaces before any tag.
- **Fallbacks, in order:** (1) if jemalloc's `configure` misdetects the page
  size during cross-compile, set `JEMALLOC_SYS_WITH_LG_PAGE=12` for the riscv64
  build; (2) worst case, build the riscv64 target with `--no-default-features`,
  dropping jemalloc for that arch only — the binary falls back to the system
  allocator and loses the jemalloc allocator-stats telemetry on riscv64, with no
  other behavioral change. Mount support is unaffected: the `jemalloc` feature's
  `dep:musefs-fuse` is a *direct* dep only so `main.rs` can install the allocator
  probe; `musefs-fuse` stays linked transitively via `musefs-cli`'s non-optional
  dependency, so a `--no-default-features` binary still mounts (verified with
  `cargo tree -p musefs --no-default-features`).

## Changes

All changes are in CI and docs; no crate source is touched.

### 0. Validate the emulated FUSE mount with a spike first

The emulated-smoke design (§2) rests on the claim that a FUSE mount works under
QEMU user-mode emulation. That is the expected behaviour — qemu-user translates
guest syscalls to host syscalls, so the mount/ioctl path reaches the real host
kernel — but it is **not proven for this project's setuid-`fusermount3` mount
path**, and FUSE mounting is environmentally fragile even natively
(AppArmor/`CAP_SYS_ADMIN`). Before wiring the emulated smoke into the release
matrix, prove it in a throwaway branch: `docker run --platform linux/riscv64`
the debian/alpine image, install `fuse3 ffmpeg`, run `scripts/smoke-binary.sh`
against a riscv64 musefs binary. If the emulated mount does not work, fall back
to a `--version`-only smoke (see §2) rather than blocking the binaries/images.

### 1. `.github/workflows/release.yml` — `build` job matrix + zig bump

- Bump `ZIG_VERSION` to `0.14.0` (Gotcha 1), and `CARGO_ZIGBUILD_VERSION` if
  required for zig-0.14 compatibility.
- Add two `include` entries mirroring the existing pattern:

| `triple` (Rust / artifact name)      | `zig_target`                          |
| ------------------------------------ | ------------------------------------- |
| `riscv64gc-unknown-linux-gnu`        | `riscv64gc-unknown-linux-gnu.2.27`    |
| `riscv64gc-unknown-linux-musl`       | `riscv64gc-unknown-linux-musl`        |

`cargo zigbuild --target <zig_target>`, packaging, and artifact upload steps are
already matrix-generic and need no change. The build runs on the existing
`ubuntu-latest` runner (cargo-zigbuild cross-compiles; no native riscv64 host
required).

### 2. `.github/workflows/release.yml` — `smoke` job (emulated)

No native riscv64 GitHub runner exists, so smoke runs under QEMU user-mode
emulation: qemu-user translates guest syscalls to host syscalls, so the FUSE
mount/ioctl path reaches the real host kernel. **Spike-confirmed** (§0): the
musl binary passes the full FUSE smoke under emulation, and the glibc binary
loads/runs in a `debian:trixie-slim` riscv64 rootfs. Note both *installing*
`ffmpeg` into the emulated container (apt under qemu) and *running* it are slow —
the emulated leg can take 10+ minutes — which is why it is `continue-on-error`
(it must not gate the release). The smoke script's existing 30×1s wait loops
absorb the per-operation latency.

Concrete matrix shape (the current matrix has modes `host`/`alpine` and **no**
QEMU step). Add a single new `mode: emulated` with two `include` rows carrying
`image` + `pkg`-install fields, both on `ubuntu-latest`:

| triple | mode | platform | image | pkg install |
| --- | --- | --- | --- | --- |
| `riscv64gc-unknown-linux-gnu` | `emulated` | `linux/riscv64` | `debian:trixie-slim` | `apt-get update && apt-get install -y fuse3 ffmpeg` |
| `riscv64gc-unknown-linux-musl` | `emulated` | `linux/riscv64` | `alpine:3.20` | `apk add --no-cache fuse3 ffmpeg` |

Concrete edits:

- Add a `docker/setup-qemu-action` step (pinned to the **same SHA already used
  in the `images` job**, release.yml:314), gated `if: matrix.mode ==
  'emulated'` — the native host/arm legs do not need it.
- Add a new step `if: matrix.mode == 'emulated'` that runs
  `scripts/smoke-binary.sh` (unchanged) inside `docker run --platform ${{
  matrix.platform }} --device /dev/fuse --cap-add SYS_ADMIN --security-opt
  apparmor=unconfined -v "$PWD":/w -w /w ${{ matrix.image }} sh -c '<pkg
  install> && sh scripts/smoke-binary.sh ./bin/musefs'`. This is a **new** step,
  not a reuse of the existing alpine step (that step takes no `--platform`).
- The existing `host`/`alpine` steps and their `if:` guards are unchanged.
- Mark the emulated legs **`continue-on-error`**, scoped to those legs only via
  a matrix-keyed expression at the job level (`continue-on-error: ${{
  matrix.mode == 'emulated' }}`) — each matrix leg is a separate job instance,
  so this is per-leg in effect. Do **not** make the whole job unconditionally
  `continue-on-error`. Rationale: `images`, `publish`, and `release-assets` all
  `needs: smoke`, so a
  *hard* emulated-smoke failure would otherwise block the image push, the
  crates.io publish, and the GitHub release upload. `fail-fast: false` only
  stops sibling legs from cancelling — it does not stop a failed leg from
  failing the job. `continue-on-error` keeps the emulated smoke as a **visible
  signal** (red leg, surfaced in the run) without letting emulation flakiness
  hold the release hostage. The native legs stay hard-gating.

**Fallback** (if the §0 spike shows the emulated mount does not work): replace
the full `smoke-binary.sh` run with a `--version`-only check inside the same
emulated container. Binaries and images still ship.

### 3. `.github/workflows/release.yml` — `images` job

The Dockerfiles copy `${TARGETARCH}/musefs` with `TARGETARCH` auto-populated by
buildx (which maps `linux/riscv64` to `TARGETARCH=riscv64`), so the `COPY` logic
is arch-generic. **But the glibc base image must change:** `docker/Dockerfile.glibc`
was `FROM debian:bookworm-slim`, and **bookworm (Debian 12) publishes no riscv64
manifest** — riscv64 became an official Debian architecture only in Debian 13
(trixie). The riscv64 glibc image therefore cannot build on bookworm. Resolution
(decided 2026-06-14): bump the glibc base to **`debian:trixie-slim` for all
arches** — trixie is current Debian stable, so this is a routine base bump that
keeps a single shared base across amd64/arm64/riscv64. `docker/Dockerfile.musl`
(`alpine:3.20`) is **unchanged** — Alpine publishes riscv64 from 3.20 on.

Spike-verified: a riscv64 glibc binary mounts FUSE and serves a valid FLAC under
`debian:trixie-slim` emulation; the riscv64 musl binary does the same on
`alpine:3.20`.

The current `images` job stages exactly two arches by literal calls. Four
concrete edits:

- Bump `docker/Dockerfile.glibc` from `FROM debian:bookworm-slim` to `FROM
  debian:trixie-slim` (the riscv64-availability fix above; the only Dockerfile
  change).
- Add a `riscv64_triple` value to each matrix variant (glibc:
  `riscv64gc-unknown-linux-gnu`, musl: `riscv64gc-unknown-linux-musl`).
- Add a third `download-artifact` step (mirroring the amd64/arm64 ones,
  release.yml:271-280) keyed on `${{ matrix.riscv64_triple }}` into
  `dl/riscv64`, and add a third `stage riscv64 "${{ matrix.riscv64_triple }}"`
  call in the "Stage binaries" run step (release.yml:295-296).
- Append `linux/riscv64` to the `platforms:` list of the multi-arch
  "Build and push" step (release.yml:347 →
  `linux/amd64,linux/arm64,linux/riscv64`).

`scripts/container_tags.py` is arch-agnostic and is not touched. The native
image smoke (release.yml:322-341) builds and tests **`linux/amd64` only** and is
unchanged — it validates the Dockerfile, not each arch; the extra riscv64 binary
staged into `ctx/` is harmless to it. The QEMU binfmt needed by the riscv64
manifest build is already registered in this job by its existing
`setup-qemu-action` step (release.yml:314).

### 4. `publish` and `release-assets` — verified arch-generic, no change

- `release-assets` (release.yml:362-387) globs `dist/*.tar.gz` /
  `dist/*.sha256` after `download-artifact --merge-multiple`, so the two new
  riscv64 tarballs flow through and attach to the GitHub Release automatically.
- `publish` (release.yml:92-132) publishes crates to crates.io and is entirely
  arch-independent.

Both are listed here so the reviewer knows they were considered. See the Risk
section for the dependency-chain blast radius these create.

### 5. Documentation

- **README.md** — the "Prebuilt binaries" table (README.md:75-82) lists "four
  targets". Change to "six", add two rows:
  `riscv64gc-unknown-linux-gnu` (glibc, "glibc 2.27 floor, RISC-V 64") and
  `riscv64gc-unknown-linux-musl` (musl, "Fully static, RISC-V 64"). The
  "Platform support" table (README.md:265-271) is OS-level (Linux/FreeBSD/macOS)
  and needs **no** arch change.
- **CHANGELOG.md** — add an entry under the unreleased section: "Added: riscv64
  (glibc + musl) prebuilt binaries and Docker images."

No `fuzz/` or schema regeneration is involved (no format-API or `musefs-db`
schema change) — N/A, noted to close the loop.

## Testing & verification

- **Local cross-compile check** (pre-merge, no riscv64 hardware), against the
  **bumped zig 0.14** toolchain:
  `rustup target add riscv64gc-unknown-linux-gnu riscv64gc-unknown-linux-musl`
  then `cargo zigbuild --release -p musefs --target
  riscv64gc-unknown-linux-gnu.2.27` (and the musl target) — confirms the glibc
  baseline and zig target strings are correct before they hit a tag-triggered
  release. Also rebuild the four **existing** targets against zig 0.14 as a
  regression guard for the version bump (Gotcha 1).
- **§0 emulated-mount spike** — prove `scripts/smoke-binary.sh` passes under
  `docker run --platform linux/riscv64` before wiring the emulated smoke leg
  into the matrix.
- **CI smoke** validates the released binary actually mounts FUSE, serves a
  synthesized FLAC (magic `fLaC`, non-empty), and unmounts cleanly on SIGTERM —
  under riscv64 emulation, end to end.
- The full workspace test suite already exercises all logic arch-independently;
  no new unit tests are warranted (there is no new arch-specific code path).

## Out of scope / non-goals

- No native riscv64 CI runner (none offered by GitHub; emulation is the
  deliberate substitute).
- No riscv64 entry in the non-release CI (`ci.yml`) test matrix — emulated full
  test runs would be prohibitively slow for no added coverage over the existing
  arch-independent suite.
- No changes to crate source, schema, or the FUSE/format/core logic.

## Risk

**Dependency-chain blast radius.** `images`, `publish`, and `release-assets`
all `needs: smoke`. A *hard* smoke failure therefore blocks the image push, the
crates.io publish, and the GitHub release upload — not just the smoke. This
design defuses that for the emulated legs by marking them `continue-on-error:
true` (§2): they surface signal as a red leg but cannot fail the `smoke` job, so
emulation flakiness/slowness cannot hold the release hostage. The containment is
real only because of `continue-on-error`; without it, the "one CI leg" framing
would be wrong.

**Zig bump regression.** Bumping `ZIG_VERSION` to 0.14 touches all four existing
targets. Mitigated by the local pre-merge rebuild of the existing targets
against zig 0.14 (Testing section) before the change lands.

**Unproven emulated FUSE mount.** The emulated-smoke value depends on FUSE
mounting working under qemu-user (§0). Mitigated by proving it in a spike first
and by the `--version`-only fallback, which keeps binaries and images shipping
regardless.

**jemalloc cross-compile (highest-likelihood failure).** jemalloc ships by
default and is compiled from C per target (Gotcha 3); `zig cc` cross-compiling
it for riscv64gc is the most probable break. Mitigated by the existing musl
targets already proving jemalloc + zigbuild, by the local cross-compile catching
it pre-tag, and by the `JEMALLOC_SYS_WITH_LG_PAGE=12` → `--no-default-features`
fallback ladder.
