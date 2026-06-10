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

Each image contains:

- the matching prebuilt `musefs` binary at `/usr/local/bin/musefs`,
- the `fuse3` package,
- `ENTRYPOINT ["musefs"]`,
- OCI labels (`org.opencontainers.image.source`, `.revision`, `.version`, …) so
  GHCR links the package to the repo and records provenance.

## How it's built (`release.yml`)

A new `images` job:

- `needs: [smoke]` — images publish only after the binaries pass their smoke
  tests, and independently of the GitHub-Release / crates-publish jobs, so a
  registry hiccup does not block binary release.
- `permissions: { contents: read, packages: write }`; log in to GHCR with the
  built-in `GITHUB_TOKEN`.
- Download all four `musefs-<triple>` artifacts, un-tar, and arrange the
  binaries into per-variant build contexts keyed by **Docker** arch
  (`x86_64` → `amd64`, `aarch64` → `arm64`):
  - `ctx/glibc/amd64/musefs`, `ctx/glibc/arm64/musefs`
  - `ctx/musl/amd64/musefs`,  `ctx/musl/arm64/musefs`
- Two thin Dockerfiles, `docker/Dockerfile.glibc` and `docker/Dockerfile.musl`:
  - `ARG TARGETARCH` → `COPY ${TARGETARCH}/musefs /usr/local/bin/musefs`
  - `RUN <pkg-mgr> install fuse3` (`apt-get` for debian-slim, `apk` for alpine)
  - OCI labels, `ENTRYPOINT ["musefs"]`.
- Build with `docker buildx --platform linux/amd64,linux/arm64` (one build
  produces one manifest list). `docker/setup-qemu-action` is required only so
  the small `apt-get`/`apk` `fuse3` install `RUN` can execute under the
  non-native arch — **no compilation under emulation**, because the binary is
  just `COPY`d in.
- **Per-variant smoke before push:** build the amd64 image with `--load`, run
  `scripts/smoke-binary.sh` inside it (native amd64; `/dev/fuse` is available on
  the `ubuntu-latest` runner) to prove `fusermount3` + the binary work
  end-to-end. Only on success build-and-push the multi-arch manifest (the amd64
  layer is reused from buildx cache).
- Matrix over `variant ∈ {glibc, musl}` keeps the two flows identical.

### Tag computation

In the job, derive from `GITHUB_REF_NAME`:

- `VERSION = ${GITHUB_REF_NAME#v}`.
- Immutable tags always pushed: `:VERSION` (glibc), `:VERSION-musl` (musl).
- Floating tags pushed only when `VERSION` contains no `-`:
  `:latest` (glibc), `:musl` (musl).

## Documentation

A new **Container images** section in `README.md`, alongside the existing
install/usage docs:

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
  mounts, reads a synthesized FLAC, and asserts clean SIGTERM unmount inside the
  actual published image.
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
