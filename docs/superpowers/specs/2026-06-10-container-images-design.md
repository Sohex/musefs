# Design: Official container images for musefs

**Issue:** [#198](https://github.com/Sohex/musefs/issues/198) — Publish container images for musefs
**Date:** 2026-06-10
**Status:** Approved (design); pending implementation plan

## Problem

The release pipeline ships standalone glibc and musl binaries for x86_64 and
aarch64, but there is no official container image. Users running musefs
alongside containerized media managers (e.g. Lidarr on Alpine) must build their
own image or hand-install the binary. Official multi-arch images — one set
matching each of the four build artifacts — let those users pull and run musefs
directly.

## Goals

- Publish official container images that match the four release build artifacts
  (glibc/musl × amd64/aarch64).
- Reuse the already-built, already-smoke-tested release binaries — image bytes
  identical to the published tarballs, no recompilation.
- Document deployment, with the gotchas specific to running a FUSE filesystem
  in a container.

## Non-goals

- Building musefs from source inside the image (the binaries already exist).
- Publishing to registries other than GHCR.
- Distroless/scratch bases (the image needs the setuid `fusermount3` helper).
- Changes to the binary, the mount path, or any runtime behavior.

## Runtime facts that constrain the design

- musefs uses `fuser` with `default-features = false` — the pure-Rust
  fusermount3 mount path. The binary does **not** dynamically link libfuse.
- At mount and unmount time it shells out to **`fusermount3`**
  (`musefs-cli/src/signal.rs`), so the image must contain the `fuse3` package
  (which provides the setuid `fusermount3` helper). This rules out
  scratch/distroless bases.
- **ffmpeg is not a runtime dependency** — it appears only in test/smoke
  fixtures. The image does not need it.
- At run time the container needs `/dev/fuse`, `CAP_SYS_ADMIN`, and
  `apparmor=unconfined`, exactly like the existing Alpine smoke job in
  `release.yml`.

## What gets published

Two multi-arch manifests on GHCR, built from the existing release binaries:

| Tag(s)                                            | libc  | Platforms                | Base                  |
| ------------------------------------------------- | ----- | ------------------------ | --------------------- |
| `ghcr.io/sohex/musefs:VERSION`, `:latest`         | glibc | linux/amd64, linux/arm64 | `debian:bookworm-slim`|
| `ghcr.io/sohex/musefs:VERSION-musl`, `:musl`      | musl  | linux/amd64, linux/arm64 | `alpine:3.20`         |

- `docker pull` auto-selects the CPU arch from the manifest list. The four
  release artifacts map 1:1 onto the four underlying images.
- glibc-vs-musl cannot be auto-selected by Docker (both are `linux/amd64`), so
  it is expressed as a tag suffix (`-musl` / `:musl`).
- **Floating-tag guard:** `:latest` and `:musl` update only on **stable** tags.
  A prerelease tag whose version contains a hyphen (e.g. `v0.2.0-rc1`) pushes
  only the immutable version tags (`:0.2.0-rc1`, `:0.2.0-rc1-musl`).
- **Image-ref casing:** the GitHub owner is mixed-case (`Sohex`), but GHCR
  rejects uppercase in image references. The image path must be **lowercased**
  before it reaches `docker push` — do not interpolate `${{ github.repository }}`
  raw. Lowercase it explicitly (`tr '[:upper:]' '[:lower:]'`) or let
  `docker/metadata-action` (which lowercases automatically) compute the ref.

Each image contains:

- the matching prebuilt `musefs` binary at `/usr/local/bin/musefs`,
- the `fuse3` package,
- `ENTRYPOINT ["musefs"]`,
- OCI labels so GHCR links the package to the repo and records provenance. The
  exact set and sources:
  - `org.opencontainers.image.source` = the repo URL (drives GHCR repo-linking),
  - `org.opencontainers.image.revision` = `${{ github.sha }}`,
  - `org.opencontainers.image.version` = `VERSION` (the `v`-stripped tag),
  - `org.opencontainers.image.licenses` = `MIT` (matches the repo license),
  - `org.opencontainers.image.title` / `.description` = `musefs` / one-liner.

## How it's built (`release.yml`)

A new `images` job:

- `needs: [smoke]` — images publish only after the binaries pass their smoke
  tests, and independently of the GitHub-Release / crates-publish jobs, so a
  registry hiccup does not block binary release.
- `permissions: { contents: read, packages: write }`; log in to GHCR with the
  built-in `GITHUB_TOKEN`.
- **Two-layer artifact unwrap.** The `build` job uploads an artifact *named*
  `musefs-<triple>` whose *contents* are `musefs-<VERSION>-<triple>.tar.gz` plus
  a `.sha256` — not a bare binary (`release.yml:160-166`). So per triple:
  `download-artifact name=musefs-<triple>` → verify
  `sha256sum -c musefs-<VERSION>-<triple>.tar.gz.sha256` → `tar -xzf` that inner
  tarball → bare `./musefs`. (`VERSION` is embedded in the inner filename; glob
  it rather than hardcoding.)
- Arrange the extracted binaries into per-variant build contexts keyed by
  **Docker** arch (`x86_64` → `amd64`, `aarch64` → `arm64`). The build-context
  **root is the per-variant directory** so `${TARGETARCH}` resolves directly:
  - context root `ctx/glibc/` → `amd64/musefs`, `arm64/musefs`
  - context root `ctx/musl/`  → `amd64/musefs`, `arm64/musefs`
- Two thin Dockerfiles, `docker/Dockerfile.glibc` and `docker/Dockerfile.musl`:
  - `ARG TARGETARCH` (declared so buildx auto-populates it per platform) →
    `COPY ${TARGETARCH}/musefs /usr/local/bin/musefs` (relative to the
    per-variant context root above),
  - `RUN <pkg-mgr> install fuse3` (`apt-get` for debian-slim, `apk` for alpine),
  - OCI labels, `ENTRYPOINT ["musefs"]`.
- `docker/setup-qemu-action` + `docker/setup-buildx-action` (docker-container
  driver, so a build cache persists between the two invocations below). QEMU is
  required only so the `apt-get`/`apk` `fuse3` install `RUN` can execute under
  the non-native (arm64) leg — **no compilation under emulation**, because the
  binary is just `COPY`d in. This package-install `RUN` is the *single* emulated
  step and the only emulation-related failure surface; pin the QEMU action
  version.
- **Per-variant build flow — two buildx invocations** (`buildx --load` cannot
  load a multi-arch manifest, so the smoke and the push are separate builds):
  1. **Smoke build:** `buildx build --platform linux/amd64 --load` (single arch,
     loadable into the local docker daemon), then run the smoke (below).
  2. **Publish build:** on smoke success, `buildx build --platform
     linux/amd64,linux/arm64 --push` — one invocation produces and pushes the
     manifest list. With the shared buildx cache, the amd64 layers are reused
     from invocation 1.
  - **Identity note:** the smoked and pushed amd64 images are not asserted
    bit-identical; the load-bearing artifact — the `musefs` binary — *is*
    byte-identical (it is the release tarball's binary, sha256-verified above),
    and the rest is the deterministic base + `fuse3` install. Bit-for-bit
    reproducibility of the image layers is a non-goal.
- **Smoke step (amd64, native).** The image deliberately ships **no ffmpeg**, but
  `scripts/smoke-binary.sh` needs ffmpeg to generate its FLAC fixture. Mirror the
  existing Alpine binary-smoke (`release.yml:207-215`): run the built image with
  the required FUSE flags, install ffmpeg into the *throwaway* container at run
  time, mount the smoke script in, and invoke it against the image's own binary —
  overriding the entrypoint:

  ```
  docker run --rm \
    --device /dev/fuse --cap-add SYS_ADMIN --security-opt apparmor=unconfined \
    -v "$PWD/scripts":/scripts:ro --entrypoint sh <local-image-tag> \
    -c '<pkg-mgr> add/install ffmpeg && sh /scripts/smoke-binary.sh /usr/local/bin/musefs'
  ```

  This validates the image's `musefs` + `fusermount3` end-to-end (mount, read a
  synthesized FLAC, clean SIGTERM unmount). ffmpeg is added only transiently for
  fixture generation and is never part of the published image — same compromise
  the existing binary-smoke already makes.
- Matrix over `variant ∈ {glibc, musl}` keeps the two flows identical.
- **Failure isolation.** If the `images` job fails, the release still completes
  with binaries (the GitHub Release and crates.io publish are independent), and
  `:latest`/`:musl` simply stay at the previous version. This is the intended
  tradeoff — a registry hiccup must not block binary release — not an oversight.

### Tag computation

In the job, derive from `GITHUB_REF_NAME`:

- `VERSION = ${GITHUB_REF_NAME#v}`.
- Immutable tags always pushed: `:VERSION` (glibc), `:VERSION-musl` (musl).
- Floating tags pushed only when `VERSION` contains no `-`:
  `:latest` (glibc), `:musl` (musl).

Out of scope: guarding against an *out-of-order* stable tag (re-tagging an older
patch after a newer release would move `:latest` backwards). Releases are assumed
monotonic; this is a deliberate non-goal, not a handled case.

## Documentation

A new **Container images** section in `README.md`, placed immediately after the
existing `## Prebuilt binaries` section (which already tells the musl-on-Alpine
story the container section builds on):

- **Host-first recommendation.** Running the binary directly on the host is the
  simplest, best-supported path. The container exists for users who specifically
  want to colocate musefs with containerized media managers.
- **If you containerize, prefer Podman in a shared pod** so musefs and the
  consumer (e.g. Lidarr) share namespaces and the FUSE mount is reachable,
  rather than hand-wiring cross-container mount propagation.
- **Gotchas, called out explicitly:**
  - Requires `--device /dev/fuse --cap-add SYS_ADMIN
    --security-opt apparmor=unconfined`; FUSE cannot mount without these.
  - A FUSE mount made inside a container lives in that container's mount
    namespace — by default the host and *other* containers do not see it. To
    share it, either put both containers in one **podman pod**, or bind-mount
    the mount point with **`rshared` propagation**. This is the central reason
    for the podman-pod recommendation.
  - glibc-vs-musl tag guidance (use `:musl` when slotting into an Alpine-based
    stack).
  - Pull/run examples for both `docker` and `podman`.

No changes to `ARCHITECTURE.md` / `CONTRIBUTING.md`, which document internals,
not deployment.

## Testing / verification

- The in-pipeline per-variant amd64 smoke (above) is the functional gate: it
  runs the built image's own `musefs` + `fusermount3` to mount, read a
  synthesized FLAC, and assert clean SIGTERM unmount. ffmpeg is added to the
  throwaway smoke container only to generate the fixture; it is not part of the
  published image.
- `yamllint` / existing CI lint applies to the new workflow job.
- aarch64 images are validated structurally (the manifest is built and pushed)
  but not run in CI; this matches the existing binary smoke matrix, which runs
  aarch64 on `ubuntu-24.04-arm` for binaries but the image smoke is amd64-only
  to avoid emulated-FUSE complexity. This limitation is recorded here rather
  than silently dropped.

## Open risks

- GHCR package visibility defaults to private; the first publish may need a
  one-time manual flip to public (or an org setting). Note in the implementation
  plan; cannot be done from the workflow alone on first run.
