# macOS FUSE end-to-end CI via fuse-t — design

**Date:** 2026-06-17
**Status:** Approved (brainstorming), pending spec review

## Problem

The `macos` CI job (`ci.yml`) is **compile + unit/integration only**: it runs
`cargo clippy --all-targets` and `cargo test --workspace`. The mount-based e2e
suite is `#[ignore]`d and never runs on macOS, so musefs has **zero coverage of
the actual mounted read path on macOS**. The Linux `e2e` and the FreeBSD
`freebsd` jobs are the only ones that mount and read through the filesystem.

The blocker is real, not just a missing workflow file: `musefs-fuse/Cargo.toml`
builds `fuser` with the **`macos-no-mount`** feature on macOS — a compile-only
stub that deliberately avoids requiring macFUSE to build. The mount path
(`establish_session` → `fuser::Session::new`, `musefs-fuse/src/lib.rs:910-926`)
is exercised only on Linux/FreeBSD today.

## Goal

Run the existing mounted FUSE e2e suite on a GitHub-hosted macOS runner, using
**fuse-t** (userspace FUSE↔NFS, no kernel extension) as the mount provider, as a
**best-effort, fuse-gated** CI job — without disturbing the existing dep-free
macOS compile path.

Non-goals: macFUSE/self-hosted runners; kernel-passthrough e2e on macOS (Linux
only, as on FreeBSD); making macOS e2e a required merge gate (deferred until the
job is proven stable).

## Why fuse-t

fuse-t is a **drop-in macFUSE replacement at the libfuse API level**: it
implements a userspace server that converts the FUSE protocol to NFS and lets
macOS mount a normal NFS volume — **no kext, no reboot, no SIP approval**. The
libfuse API headers are unchanged, so filesystem code that already targets
libfuse needs no source changes. This is the only FUSE route that works on
GitHub-hosted `macos-latest` runners (macFUSE's kext load is blocked there).

`fuser` (0.17, 24 features, none default) mounts on macOS via its **`libfuse`**
feature, which links the system libfuse and shells out to its mount helper. With
fuse-t installed, that libfuse *is* fuse-t's drop-in, so `fuser` mounts through
fuse-t transitively.

## Structural approach (chosen: A — opt-in feature)

Add a `musefs-fuse` cargo feature that turns on `fuser`'s libfuse path on macOS,
**off by default** so the existing compile job and local dev keep building with
zero new dependencies (preserving the deliberate `macos-no-mount` decision):

```toml
# musefs-fuse/Cargo.toml
[features]
metrics = ["musefs-core/metrics"]
macos-mount = ["fuser/libfuse"]   # opt-in: link fuse-t's libfuse on macOS

[target.'cfg(target_os = "macos")'.dependencies]
fuser = { version = "0.17", features = ["macos-no-mount"] }   # unchanged default
```

When `--features macos-mount` is set, `fuser` is built with both
`macos-no-mount` and `libfuse`. **Phase 0 spike (below) must confirm these two
features coexist.** If they conflict, fall back to **Approach B**: drop
`macos-no-mount`, make the macOS dep `default-features = false` with
`macos-mount = ["fuser/libfuse"]`, and add a fuse-t install step to the existing
`macos` compile job (every macOS build then requires fuse-t). The spec keeps A
as primary and documents B as the only fallback.

## Phases

### Phase 0 — feasibility spike (de-risk before any CI wiring)

On a hosted `macos-latest` runner (throwaway branch / `workflow_dispatch`):

1. `brew install` fuse-t (cask; confirm exact tap/formula and whether
   `--no-quarantine` is needed). Record where it installs libfuse + its
   pkg-config (`fuse.pc`/`osxfuse.pc`) so `fuser`'s build script finds it
   (`PKG_CONFIG_PATH` / linker path as needed).
2. Build `musefs-fuse` with `--features macos-mount`. **Confirm `fuser`'s
   `libfuse` + `macos-no-mount` compile together.** If not → Approach B.
3. Run one mount test (`cargo test -p musefs-fuse --features macos-mount --test
   mount -- --ignored`). Confirm a real fuse-t mount + read-through succeeds.

Spike output: the working install/env recipe, the confirmed feature
combination, and a list of any tests that fail under fuse-t's NFS semantics.

### Phase 1 — code + manifest

- Add the `macos-mount` feature (Approach A) or the B fallback per spike.
- Adjust any `#[cfg(target_os = "linux")]`/Linux-only assumptions the spike
  surfaced in the e2e path, following the existing FreeBSD precedent in
  `read_consistency.rs` (`:242`, `:380`) where Linux-only assertions are already
  guarded.
- Keep `cargo test --workspace` (no feature) green on macOS, unchanged.

### Phase 2 — CI job

Add a `macos-e2e` job to `ci.yml`, modeled on `freebsd`:

```yaml
  macos-e2e:
    needs: changes
    if: >-
      startsWith(github.ref, 'refs/tags/') ||
      needs.changes.outputs.fuse == 'true'
    runs-on: macos-latest
    continue-on-error: true       # best-effort; NOT in ci-ok yet
    timeout-minutes: 30
    steps:
      - uses: actions/checkout@<pinned>
        with: { persist-credentials: false }
      - name: Install fuse-t + ffmpeg
        run: |
          brew install <fuse-t tap/formula>   # exact recipe from Phase 0
          brew install ffmpeg
      - uses: dtolnay/rust-toolchain@<pinned>
      - uses: Swatinem/rust-cache@<pinned>
      - name: FUSE end-to-end tests (fuse-t)
        run: cargo test -p musefs-fuse --features macos-mount -- --ignored
```

- **Gating:** runs only when the fuse surface changes or on `v*` tags — same
  predicate as `freebsd` (the `fuse` filter already includes
  `.github/workflows/ci.yml`).
- **Reporting:** `continue-on-error: true` and **not** added to the `ci-ok`
  aggregator `needs` list — a flaky fuse-t run never blocks merges. Mirrors the
  `tsan` best-effort precedent.
- **ffmpeg loud-fail:** like `scripts/freebsd-vm/run-e2e.sh`, the run must fail
  if ffmpeg is absent rather than let `playback_pcm`/`ogg_read_through` skip
  silently into a vacuous green. (Either a guard step or rely on the tests'
  existing behavior — confirm during Phase 1.)

### Test subset

`cargo test -p musefs-fuse -- --ignored` (plus `--features macos-mount`) runs
`mount`, `keep_cache`, `playback_pcm`, `ogg_read_through`, `read_consistency`,
`concurrency`, `concurrent_reads`, `fault_injection`. The `metrics`-gated
`passthrough.rs` and `metrics_e2e.rs` are `#![cfg(feature = "metrics")]` and are
**automatically excluded** (metrics not enabled) — exactly the subset FreeBSD
runs, and the correct exclusion since fuse-t's NFS layer changes the
syscall→FUSE-op mapping and would break the exact getattr/read-count assertions.

If the Phase 0 spike finds specific tests that fuse-t's NFS semantics break
(caching, xattr, statfs), start with a documented narrower subset (mount +
read-through + playback fidelity) and expand in follow-ups, **logging what is
excluded** so a narrowed run never reads as full coverage.

## Risks

| Risk | Mitigation |
| --- | --- |
| `fuser` rejects `libfuse` + `macos-no-mount` together | Phase 0 spike gate; Approach B fallback documented |
| fuse-t's NFS semantics break cache/xattr/statfs tests | Spike enumerates failures; start with a narrower documented subset |
| fuse-t brew install flaky / needs quarantine flag | Pin recipe in Phase 0; job is `continue-on-error` so it never blocks |
| Hosted macOS runner minutes (~10× multiplier) | Fuse-gated (not every PR), 30-min timeout, best-effort |

## Success criteria

- A green `macos-e2e` run mounts via fuse-t and passes the non-metrics e2e
  subset on a hosted `macos-latest` runner.
- The existing `macos` compile job and local `cargo build`/`cargo test
  --workspace` on macOS still work with **no new required dependency**.
- The job is fuse-gated and best-effort (not in `ci-ok`); promotion to required
  is a separate, later decision once it has a stable track record.
