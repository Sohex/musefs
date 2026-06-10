# Container Images Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Publish official multi-arch GHCR container images for musefs — a glibc manifest (`:VERSION`/`:latest`) and a musl manifest (`:VERSION-musl`/`:musl`), each spanning linux/amd64 + linux/arm64 — built by reusing the existing release binaries.

**Architecture:** A new `images` job in `.github/workflows/release.yml` (`needs: smoke`) downloads the four already-built, already-smoke-tested zigbuild binaries, COPYs each into a thin per-libc Dockerfile, smokes the amd64 image, then builds-and-pushes the multi-arch manifest. Tag/ref computation (lowercasing, floating-tag guard) is factored into a unit-tested Python helper. README gains a deployment section.

**Tech Stack:** GitHub Actions, `docker/build-push-action` + buildx (QEMU only for the package-install layer), `debian:bookworm-slim` (glibc) / `alpine:3.20` (musl) bases, Python 3 helper with pytest, GHCR via the built-in `GITHUB_TOKEN`.

**Spec:** `docs/superpowers/specs/2026-06-10-container-images-design.md`

---

## Working context

- All work happens in the worktree at
  `/home/cfutro/git/musefs/.claude/worktrees/container-images-198` on branch
  `worktree-container-images-198`. Run every command from there.
- The pre-commit hook runs `fmt` + `clippy -D warnings` + the **full workspace
  test suite** + `ruff` over `scripts/` and `contrib/`. Every commit must be
  green, so each task ends with a real commit that passes the hook.
- Local tooling present: `podman` (no `docker`/`buildx`), `/dev/fuse`, Python 3,
  `pytest`. **No** `actionlint`/`yamllint` locally.
- The `images` job only triggers on `v*` tag pushes, so it will **not** run on
  the PR for this branch. Its first real exercise is the next release tag; the
  per-step verifications below are how we de-risk it before then.

## File structure

| File | Disposition | Responsibility |
| --- | --- | --- |
| `scripts/container_tags.py` | Create | Pure logic: lowercase the GHCR ref, strip the `v`, apply the floating-tag guard, emit the tag list for a variant. |
| `scripts/test_container_tags.py` | Create | pytest unit tests for the helper. |
| `.github/workflows/ci.yml` | Modify | Wire the helper's pytest into the `python` job. |
| `docker/Dockerfile.glibc` | Create | `debian:bookworm-slim` + `fuse3` + COPY the glibc binary. |
| `docker/Dockerfile.musl` | Create | `alpine:3.20` + `fuse3` + COPY the musl binary. |
| `.github/workflows/release.yml` | Modify | New `images` job: download → stage → smoke → build+push. |
| `README.md` | Modify | New `## Container images` deployment section. |

---

## Task 1: Tag/ref computation helper (`container_tags.py`)

**Files:**
- Create: `scripts/container_tags.py`
- Test: `scripts/test_container_tags.py`
- Modify: `.github/workflows/ci.yml` (add a pytest step)

This isolates the only non-trivial logic in the workflow (GHCR lowercasing +
prerelease floating-tag guard) into something we can test without pushing a tag.

- [ ] **Step 1: Write the failing tests**

Create `scripts/test_container_tags.py`:

```python
import pytest

from container_tags import (
    is_prerelease,
    main,
    registry_ref,
    tags_for,
    version_from_ref,
)


def test_registry_ref_lowercases_owner():
    # GitHub owner is mixed-case "Sohex"; GHCR rejects uppercase refs.
    assert registry_ref("Sohex/musefs") == "ghcr.io/sohex/musefs"


def test_version_from_ref_strips_leading_v():
    assert version_from_ref("v0.2.0") == "0.2.0"
    assert version_from_ref("0.2.0") == "0.2.0"


def test_is_prerelease():
    assert is_prerelease("0.2.0") is False
    assert is_prerelease("0.2.0-rc1") is True


def test_tags_for_glibc_stable():
    assert tags_for("Sohex/musefs", "v0.2.0", "glibc") == [
        "ghcr.io/sohex/musefs:0.2.0",
        "ghcr.io/sohex/musefs:latest",
    ]


def test_tags_for_musl_stable():
    assert tags_for("Sohex/musefs", "v0.2.0", "musl") == [
        "ghcr.io/sohex/musefs:0.2.0-musl",
        "ghcr.io/sohex/musefs:musl",
    ]


def test_tags_for_glibc_prerelease_omits_latest():
    assert tags_for("Sohex/musefs", "v0.2.0-rc1", "glibc") == [
        "ghcr.io/sohex/musefs:0.2.0-rc1",
    ]


def test_tags_for_musl_prerelease_omits_floating():
    assert tags_for("Sohex/musefs", "v0.2.0-rc1", "musl") == [
        "ghcr.io/sohex/musefs:0.2.0-rc1-musl",
    ]


def test_unknown_variant_raises():
    with pytest.raises(ValueError):
        tags_for("Sohex/musefs", "v0.2.0", "windows")


def test_main_prints_newline_separated(capsys):
    rc = main(["--repo", "Sohex/musefs", "--ref", "v0.2.0", "--variant", "glibc"])
    out = capsys.readouterr().out
    assert rc == 0
    assert out == "ghcr.io/sohex/musefs:0.2.0\nghcr.io/sohex/musefs:latest\n"
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cd scripts && python -m pytest test_container_tags.py -v`
Expected: FAIL — `ModuleNotFoundError: No module named 'container_tags'`.

- [ ] **Step 3: Implement the helper**

Create `scripts/container_tags.py`:

```python
"""Compute the GHCR image references for a musefs container release.

The release workflow publishes two multi-arch manifests per tag: glibc
(``:VERSION`` / ``:latest``) and musl (``:VERSION-musl`` / ``:musl``). The
floating tags (``latest`` / ``musl``) move only on stable releases; a prerelease
tag whose version carries a ``-`` segment (e.g. ``v0.2.0-rc1``) publishes only
the immutable version-pinned tags. GHCR rejects uppercase in image references,
so the owner is lowercased here (the GitHub owner is mixed-case ``Sohex``).
"""

from __future__ import annotations

import argparse

# variant -> (version-tag suffix, floating tag)
_VARIANTS = {
    "glibc": ("", "latest"),
    "musl": ("-musl", "musl"),
}


def registry_ref(repo: str) -> str:
    """Return the lowercased GHCR image path for ``owner/name`` ``repo``."""
    return f"ghcr.io/{repo}".lower()


def version_from_ref(ref: str) -> str:
    """Strip a leading ``v`` from a tag ref (``v0.2.0`` -> ``0.2.0``)."""
    return ref[1:] if ref.startswith("v") else ref


def is_prerelease(version: str) -> bool:
    """A version is a prerelease iff it carries a ``-`` pre-release segment."""
    return "-" in version


def tags_for(repo: str, ref: str, variant: str) -> list[str]:
    """Full image refs to publish for ``variant`` at tag ``ref``."""
    if variant not in _VARIANTS:
        raise ValueError(f"unknown variant: {variant!r}")
    suffix, floating = _VARIANTS[variant]
    base = registry_ref(repo)
    version = version_from_ref(ref)
    tags = [f"{base}:{version}{suffix}"]
    if not is_prerelease(version):
        tags.append(f"{base}:{floating}")
    return tags


def main(argv=None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repo", required=True, help="owner/name, e.g. Sohex/musefs")
    parser.add_argument("--ref", required=True, help="tag ref, e.g. v0.2.0")
    parser.add_argument("--variant", required=True, choices=sorted(_VARIANTS))
    args = parser.parse_args(argv)
    for tag in tags_for(args.repo, args.ref, args.variant):
        print(tag)
    return 0


if __name__ == "__main__":  # pragma: no cover
    raise SystemExit(main())
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cd scripts && python -m pytest test_container_tags.py -v`
Expected: PASS — 9 passed.

- [ ] **Step 5: Confirm ruff is clean (the pre-commit hook lints `scripts/`)**

Run: `ruff check scripts/container_tags.py scripts/test_container_tags.py && ruff format --check scripts/container_tags.py scripts/test_container_tags.py`
Expected: `All checks passed!` and no formatting diff. If `ruff format --check`
reports a diff, run `ruff format scripts/container_tags.py scripts/test_container_tags.py` and re-check.

- [ ] **Step 6: Wire the test into CI**

In `.github/workflows/ci.yml`, the `python-musefs` job (job key at the top level
of `jobs:`, gated by `if: needs.changes.outputs.src == 'true'` — same gate as the
existing scripts tests, so no change needed there) runs a series of pytest steps.
After the existing block ending at the crates-index step:

```yaml
      - name: Test crates-index probe
        run: python -m pytest scripts/test_crates_index.py -v
```

add immediately after it:

```yaml
      - name: Test container-tags helper
        run: python -m pytest scripts/test_container_tags.py -v
```

- [ ] **Step 7: Verify the CI YAML parses and the step landed in `python-musefs`**

Run:
```bash
python - <<'PY'
import yaml
wf = yaml.safe_load(open(".github/workflows/ci.yml"))
steps = wf["jobs"]["python-musefs"]["steps"]
names = [s.get("name", "") for s in steps]
assert "Test container-tags helper" in names, names
print("ci.yml python-musefs wiring OK")
PY
```
Expected: `ci.yml python-musefs wiring OK`.

- [ ] **Step 8: Commit**

```bash
git add scripts/container_tags.py scripts/test_container_tags.py .github/workflows/ci.yml
git commit -m "feat(release): tag-computation helper for container images (#198)"
```

---

## Task 2: glibc Dockerfile

**Files:**
- Create: `docker/Dockerfile.glibc`

The base must ship the setuid `fusermount3` (rules out scratch/distroless). The
`fuse3` install goes **before** the binary `COPY` so the package layer caches
independently of the binary (helps the smoke-build → push-build cache reuse).

- [ ] **Step 1: Create the Dockerfile**

Create `docker/Dockerfile.glibc`:

```dockerfile
# syntax=docker/dockerfile:1
FROM debian:bookworm-slim

# fuse3 provides the setuid `fusermount3` helper musefs execs at mount/unmount.
RUN apt-get update \
 && apt-get install -y --no-install-recommends fuse3 \
 && rm -rf /var/lib/apt/lists/*

# TARGETARCH is auto-populated by buildx per platform (amd64 / arm64); the build
# context root holds <arch>/musefs for each.
ARG TARGETARCH
COPY ${TARGETARCH}/musefs /usr/local/bin/musefs

ENTRYPOINT ["musefs"]
```

- [ ] **Step 2: Stage a real glibc binary for a local build test**

Build a native (glibc, amd64) binary and lay out a build context the way the
workflow will:

```bash
cargo build --release -p musefs
mkdir -p ctx/amd64
cp target/release/musefs ctx/amd64/musefs
```

Expected: `ctx/amd64/musefs` exists and is executable.

- [ ] **Step 3: Build the image locally with podman**

Run:
```bash
podman build --build-arg TARGETARCH=amd64 -f docker/Dockerfile.glibc -t musefs-glibc-test ctx
```
Expected: build succeeds; final line `Successfully tagged localhost/musefs-glibc-test:latest` (or a digest line).

- [ ] **Step 4: Verify the image contents (binary runs, fusermount3 present)**

Run:
```bash
podman run --rm --entrypoint sh musefs-glibc-test -c 'command -v fusermount3 && /usr/local/bin/musefs --version'
```
Expected: prints `/usr/bin/fusermount3` and a `musefs <version>` line (exit 0).

- [ ] **Step 5: Best-effort end-to-end smoke (same shape the workflow uses)**

Run:
```bash
podman run --rm \
  --device /dev/fuse --cap-add SYS_ADMIN --security-opt apparmor=unconfined \
  -v "$PWD/scripts":/scripts:ro --entrypoint sh musefs-glibc-test \
  -c 'apt-get update >/dev/null && apt-get install -y --no-install-recommends ffmpeg >/dev/null && sh /scripts/smoke-binary.sh /usr/local/bin/musefs'
```
Expected: ends with `smoke: read <N> bytes ... (fLaC OK)` and exit 0.
**If rootless podman cannot FUSE-mount in this environment** (mount/permission
error rather than a musefs failure), Step 4 is the binding verification for this
task; the real in-CI smoke (Task 4) runs under Docker on the runner where this
is known to work. Note which path you took in the commit/PR.

- [ ] **Step 6: Clean up the scratch context (do not commit binaries)**

Run: `rm -rf ctx target/release/musefs.d 2>/dev/null`
Expected: `ctx/` gone. Confirm `git status --short` shows only `docker/Dockerfile.glibc` as new.

- [ ] **Step 7: Commit**

```bash
git add docker/Dockerfile.glibc
git commit -m "feat(images): glibc (debian-slim) container Dockerfile (#198)"
```

---

## Task 3: musl Dockerfile

**Files:**
- Create: `docker/Dockerfile.musl`

Local runtime-smoking the musl image needs a static musl binary, which requires
cross-toolchain setup we deliberately skip here — the existing Alpine
binary-smoke in `release.yml` already proves the musl binary runs on Alpine, and
Task 4's in-CI image smoke covers the published musl image. Local verification
here is structural: the Dockerfile builds and ships `fusermount3`.

- [ ] **Step 1: Create the Dockerfile**

Create `docker/Dockerfile.musl`:

```dockerfile
# syntax=docker/dockerfile:1
FROM alpine:3.20

# fuse3 provides the setuid `fusermount3` helper musefs execs at mount/unmount.
RUN apk add --no-cache fuse3

# TARGETARCH is auto-populated by buildx per platform (amd64 / arm64); the build
# context root holds <arch>/musefs for each.
ARG TARGETARCH
COPY ${TARGETARCH}/musefs /usr/local/bin/musefs

ENTRYPOINT ["musefs"]
```

- [ ] **Step 2: Stage a placeholder binary for a structural build test**

The musl image needs a musl binary at run time, but to verify the Dockerfile's
COPY path / fuse3 layer / entrypoint we only need *a* file at `amd64/musefs`.
Reuse the native binary purely as a stand-in (we will not run it on Alpine):

```bash
cargo build --release -p musefs
mkdir -p ctx/amd64
cp target/release/musefs ctx/amd64/musefs
```

Expected: `ctx/amd64/musefs` exists.

- [ ] **Step 3: Build the image locally with podman**

Run:
```bash
podman build --build-arg TARGETARCH=amd64 -f docker/Dockerfile.musl -t musefs-musl-test ctx
```
Expected: build succeeds (apk fetches `fuse3`; `COPY` resolves `amd64/musefs`).

- [ ] **Step 4: Verify fusermount3 is present in the image**

Run:
```bash
podman run --rm --entrypoint sh musefs-musl-test -c 'command -v fusermount3 && test -x /usr/local/bin/musefs && echo layout-ok'
```
Expected: prints `/usr/bin/fusermount3` and `layout-ok` (exit 0).
(Do **not** run `musefs --version` here — a glibc stand-in binary will not exec
on musl; the real musl binary is validated in CI.)

- [ ] **Step 5: Clean up the scratch context**

Run: `rm -rf ctx`
Expected: `git status --short` shows only `docker/Dockerfile.musl` as new.

- [ ] **Step 6: Commit**

```bash
git add docker/Dockerfile.musl
git commit -m "feat(images): musl (alpine) container Dockerfile (#198)"
```

---

## Task 4: `images` job in `release.yml`

**Files:**
- Modify: `.github/workflows/release.yml` (add a new job after `smoke`)

- [ ] **Step 1: Resolve commit SHAs for the new actions**

The repo SHA-pins every action. Resolve each new action's tag to a **commit**
SHA (use the commits endpoint — an annotated tag's object SHA fails only at
run time):

```bash
for r in docker/setup-qemu-action:v3 docker/setup-buildx-action:v3 docker/login-action:v3 docker/build-push-action:v6; do
  name="${r%:*}"; tag="${r#*:}"
  printf '%s -> ' "$r"
  gh api "repos/${name}/commits/${tag}" -q .sha
done
```
Expected: four 40-char SHAs. Record them; substitute each for `<SHA-...>` below.
(`actions/checkout`, `actions/setup-python`, and `actions/download-artifact` reuse
the SHAs already pinned elsewhere in `release.yml`.)

- [ ] **Step 2: Add the `images` job**

In `.github/workflows/release.yml`, insert this job after the `smoke` job and
before `release-assets` (sibling indentation under `jobs:`). Substitute the four
resolved SHAs from Step 1:

```yaml
  images:
    needs: smoke
    runs-on: ubuntu-latest
    permissions:
      contents: read
      packages: write
    strategy:
      fail-fast: false
      matrix:
        include:
          - variant: glibc
            dockerfile: docker/Dockerfile.glibc
            amd64_triple: x86_64-unknown-linux-gnu
            arm64_triple: aarch64-unknown-linux-gnu
            ffmpeg_install: apt-get update >/dev/null && apt-get install -y --no-install-recommends ffmpeg >/dev/null
          - variant: musl
            dockerfile: docker/Dockerfile.musl
            amd64_triple: x86_64-unknown-linux-musl
            arm64_triple: aarch64-unknown-linux-musl
            ffmpeg_install: apk add --no-cache ffmpeg >/dev/null
    steps:
      - uses: actions/checkout@df4cb1c069e1874edd31b4311f1884172cec0e10
        with:
          persist-credentials: false
      - uses: actions/setup-python@a309ff8b426b58ec0e2a45f0f869d46889d02405
        with:
          python-version: '3.x'
      - name: Download amd64 artifact
        uses: actions/download-artifact@3e5f45b2cfb9172054b4087a40e8e0b5a5461e7c
        with:
          name: musefs-${{ matrix.amd64_triple }}
          path: dl/amd64
      - name: Download arm64 artifact
        uses: actions/download-artifact@3e5f45b2cfb9172054b4087a40e8e0b5a5461e7c
        with:
          name: musefs-${{ matrix.arm64_triple }}
          path: dl/arm64
      - name: Stage binaries into the build context
        env:
          REF: ${{ github.ref_name }}
        run: |
          set -euo pipefail
          VERSION="${REF#v}"
          stage() {  # $1=docker_arch  $2=rust_triple
            arch="$1"; triple="$2"; dir="dl/${arch}"
            tarball="musefs-${VERSION}-${triple}.tar.gz"
            ( cd "$dir" && sha256sum -c "${tarball}.sha256" )
            mkdir -p "ctx/${arch}"
            tar -xzf "${dir}/${tarball}" -C "ctx/${arch}"
            test -x "ctx/${arch}/musefs"
          }
          stage amd64 "${{ matrix.amd64_triple }}"
          stage arm64 "${{ matrix.arm64_triple }}"
          ls -lR ctx
      - name: Compute image tags
        id: tags
        env:
          REPO: ${{ github.repository }}
          REF: ${{ github.ref_name }}
        run: |
          set -euo pipefail
          TAGS="$(python scripts/container_tags.py --repo "$REPO" --ref "$REF" --variant "${{ matrix.variant }}")"
          test -n "$TAGS"   # fail fast rather than hand build-push-action an empty tag list
          echo "computed tags:"; printf '%s\n' "$TAGS"
          echo "version=${REF#v}" >> "$GITHUB_OUTPUT"
          {
            echo "list<<EOF"
            printf '%s\n' "$TAGS"
            echo "EOF"
          } >> "$GITHUB_OUTPUT"
      - uses: docker/setup-qemu-action@<SHA-setup-qemu>
      - uses: docker/setup-buildx-action@<SHA-setup-buildx>
      - name: Log in to GHCR
        uses: docker/login-action@<SHA-login>
        with:
          registry: ghcr.io
          username: ${{ github.actor }}
          password: ${{ secrets.GITHUB_TOKEN }}
      - name: Build amd64 image for smoke
        uses: docker/build-push-action@<SHA-build-push>
        with:
          context: ctx
          file: ${{ matrix.dockerfile }}
          platforms: linux/amd64
          load: true
          tags: musefs-smoke:${{ matrix.variant }}
      - name: Smoke the built image (amd64, native)
        run: |
          set -euo pipefail
          docker run --rm \
            --device /dev/fuse --cap-add SYS_ADMIN --security-opt apparmor=unconfined \
            -v "$PWD/scripts":/scripts:ro --entrypoint sh \
            "musefs-smoke:${{ matrix.variant }}" \
            -c '${{ matrix.ffmpeg_install }} && sh /scripts/smoke-binary.sh /usr/local/bin/musefs'
      - name: Build and push multi-arch manifest
        uses: docker/build-push-action@<SHA-build-push>
        with:
          context: ctx
          file: ${{ matrix.dockerfile }}
          platforms: linux/amd64,linux/arm64
          push: true
          provenance: false
          tags: ${{ steps.tags.outputs.list }}
          labels: |
            org.opencontainers.image.source=https://github.com/${{ github.repository }}
            org.opencontainers.image.revision=${{ github.sha }}
            org.opencontainers.image.version=${{ steps.tags.outputs.version }}
            org.opencontainers.image.licenses=MIT
            org.opencontainers.image.title=musefs
            org.opencontainers.image.description=Read-only passthrough FUSE filesystem presenting a re-tagged view of a music library
```

- [ ] **Step 3: Verify the workflow YAML parses and the job is wired correctly**

Run:
```bash
python - <<'PY'
import yaml
wf = yaml.safe_load(open(".github/workflows/release.yml"))
jobs = wf["jobs"]
assert "images" in jobs, "images job missing"
assert jobs["images"]["needs"] == "smoke", jobs["images"]["needs"]
assert jobs["images"]["permissions"]["packages"] == "write"
variants = {m["variant"] for m in jobs["images"]["strategy"]["matrix"]["include"]}
assert variants == {"glibc", "musl"}, variants
print("release.yml images job OK")
PY
```
Expected: `release.yml images job OK`.

- [ ] **Step 4: Verify no unresolved action placeholders remain**

Run: `! grep -n '<SHA-' .github/workflows/release.yml`
Expected: no output, exit 0 (every `<SHA-...>` was replaced in Step 2).

- [ ] **Step 5: Commit**

```bash
git add .github/workflows/release.yml
git commit -m "feat(release): publish multi-arch GHCR container images (#198)"
```

---

## Task 5: README deployment docs

**Files:**
- Modify: `README.md` (insert a new section after `## Prebuilt binaries`)

- [ ] **Step 1: Insert the Container images section**

In `README.md`, the `## Prebuilt binaries` section ends with the line:

```
No glibc/libfuse install is needed for the musl binaries beyond `fuse3`.
```

immediately followed by `## Requirements`. Insert this new section **between**
them:

```markdown
## Container images

Each tagged release also publishes multi-arch images to the GitHub Container
Registry:

| Image | libc | Platforms |
| --- | --- | --- |
| `ghcr.io/sohex/musefs:<version>`, `ghcr.io/sohex/musefs:latest` | glibc | amd64, arm64 |
| `ghcr.io/sohex/musefs:<version>-musl`, `ghcr.io/sohex/musefs:musl` | musl | amd64, arm64 |

`docker pull` selects the CPU architecture automatically. Use the `-musl` /
`:musl` tags when slotting musefs into an Alpine-based stack; the default
(glibc) tags suit everything else. Floating `:latest` / `:musl` track the most
recent stable release only — prereleases publish version-pinned tags only.

**Running musefs on the host is the simplest, best-supported option** — it is an
ordinary FUSE daemon and the image exists mainly to colocate musefs with
containerized media managers (e.g. Lidarr). If you do containerize, mind the
gotchas below.

### Required flags

musefs mounts via FUSE, so the container needs `/dev/fuse` and the matching
capability:

```bash
docker run --rm \
  --device /dev/fuse --cap-add SYS_ADMIN --security-opt apparmor=unconfined \
  -v /path/to/library:/library:ro \
  -v /path/to/store:/store \
  ghcr.io/sohex/musefs:latest scan /library --db /store/musefs.db
```

Without `--device /dev/fuse --cap-add SYS_ADMIN --security-opt apparmor=unconfined`
the mount cannot be established.

### The mount-visibility gotcha (read this before sharing the mount)

A FUSE mount made **inside** a container lives in that container's mount
namespace. By default neither the host nor other containers can see it, so
pointing a second container (your media manager) at musefs's output does not
work out of the box. Two ways to share it:

- **Prefer Podman in a shared pod** — put musefs and the consumer in the same
  pod so they share namespaces and the mount is directly reachable:

  ```bash
  podman pod create --name media
  podman run -d --pod media \
    --device /dev/fuse --cap-add SYS_ADMIN --security-opt apparmor=unconfined \
    -v /path/to/library:/library:ro -v /path/to/store:/store \
    ghcr.io/sohex/musefs:latest mount /mnt/musefs --db /store/musefs.db
  # the consumer container, in the same pod, reads /mnt/musefs
  ```

- Or bind-mount the mount point with **`rshared` propagation** so the mount
  escapes the container's namespace to the host (`--mount type=bind,...,bind-propagation=rshared`).
  This is fiddlier than a shared pod, which is why the pod approach is
  recommended.

Both the glibc and musl images carry the `fuse3` userspace tools; pick `:musl`
if your other containers are Alpine-based, otherwise the default tags are fine.
```

- [ ] **Step 2: Verify the section landed in the right place**

Run: `grep -n '^## ' README.md | grep -A1 'Prebuilt binaries'`
Expected: shows `## Prebuilt binaries` followed by `## Container images` (i.e.
the new section sits between Prebuilt binaries and Requirements).

- [ ] **Step 3: Commit**

```bash
git add README.md
git commit -m "docs(readme): document container images and FUSE-in-container gotchas (#198)"
```

---

## Task 6: Finalize the branch

- [ ] **Step 1: Confirm the full tree is clean and every commit is green**

Run: `git status --short && git log --oneline origin/main..HEAD`
Expected: clean working tree; five feature commits (helper, glibc Dockerfile,
musl Dockerfile, release job, README) plus the two spec commits.

- [ ] **Step 2: Sanity-check no scratch artifacts were committed**

Run: `git ls-files | grep -E '^ctx/|/musefs$' || echo "no stray binaries committed"`
Expected: `no stray binaries committed`.

- [ ] **Step 3: Hand off**

Use `superpowers:finishing-a-development-branch` to open the PR. In the PR body,
flag the one out-of-band step from the spec's Open Risks: **GHCR packages
default to private** — after the first release publishes the images, the
package visibility must be flipped to public once in the repo/org package
settings (cannot be done from the workflow on first run).

---

## Notes carried from the spec / review

- **Why reuse artifacts, not compile-in-Docker:** image bytes stay identical to
  the published tarballs (the binary is sha256-verified before COPY), and there
  is no emulated compilation. QEMU runs only the tiny `fuse3` package-install
  layer on the arm64 leg — the single emulation point and the only
  emulation-related failure surface to watch.
- **Why two buildx invocations:** `buildx --load` cannot load a multi-arch
  manifest, so the amd64 smoke build and the multi-arch push build are separate.
  `setup-buildx-action`'s docker-container builder persists for the whole job,
  so its local layer cache lets the push build reuse the smoked amd64 layers
  with no explicit cache wiring. The smoked and pushed amd64 images are not
  asserted bit-identical — the load-bearing artifact (the binary) is byte-equal;
  layer reproducibility is a non-goal.
- **Why ffmpeg is installed in the smoke container:** the image deliberately
  ships no ffmpeg (not a runtime dep), but `smoke-binary.sh` needs it to
  generate the FLAC fixture; it is added transiently to the throwaway smoke
  container only — exactly the compromise the existing Alpine binary-smoke makes.
- **Failure isolation:** if `images` fails, the release still ships binaries
  (GitHub Release + crates.io publish are independent jobs) and the floating
  tags simply stay put. Intended tradeoff, not an oversight.
- **Context-root layout:** the spec describes a per-variant context root
  (`ctx/glibc/`, `ctx/musl/`). Because the `images` job runs one matrix leg per
  variant, each leg has its own checkout and stages into a single `ctx/`
  (`ctx/amd64/musefs`, `ctx/arm64/musefs`), built with `context: ctx`. That
  per-job `ctx/` *is* the per-variant root — `COPY ${TARGETARCH}/musefs` resolves
  identically; only the directory name differs from the spec's prose.
- **Out of scope:** out-of-order stable tags moving `:latest` backwards
  (releases assumed monotonic); bit-for-bit reproducible image layers; arm64
  in-CI runtime smoke (manifest is built/pushed but only amd64 is run, matching
  the existing binary-smoke's amd64-only image path).
