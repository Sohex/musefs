# riscv64 Support Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Ship `riscv64gc` (glibc + musl) as a first-class musefs release platform — prebuilt tarballs, multi-arch Docker images, and an emulated FUSE smoke test — with no crate source changes.

**Architecture:** All changes live in `.github/workflows/release.yml` and docs. The release `build` job cross-compiles via `cargo-zigbuild`; zig is bumped to 0.14 (0.13 cannot emit riscv64 glibc). A new emulated smoke leg runs the binary under `docker run --platform linux/riscv64` (QEMU user-mode: guest syscalls hit the real host kernel, so FUSE mounts), gated `continue-on-error` so emulation flakiness never blocks the release. The `images` job gains a third staged arch; the Dockerfiles are already `${TARGETARCH}`-generic.

**Tech Stack:** GitHub Actions, `cargo-zigbuild` + zig, `rustup` cross targets, `docker buildx` + QEMU `binfmt`, the existing `scripts/smoke-binary.sh`.

**Spec:** `docs/superpowers/specs/2026-06-14-riscv64-support-design.md`

---

## Notes for the implementer (read first)

- **This is YAML + docs work, not Rust.** There are no unit tests to add — verification is local cross-compilation, `yamllint`, an emulated-mount spike, and structural inspection of the workflow.
- **Pre-commit cost:** the pre-commit hook skips the cargo gate only for *docs-only* commits (every staged path under `docs/` or a `*.md`). The `release.yml` commits in Tasks 1, 2, 4, 5 are **not** docs-only, so each runs the **full workspace test suite** (slow, but expected — let it run; do not `--no-verify`). It also runs `yamllint` over the edited workflow. The Task 6 docs commit skips the cargo gate.
- **No libfuse for cross-builds:** the `build` job does not install `libfuse3-dev`, and musefs links no libfuse on Linux (mount execs `fusermount3` at runtime; passthrough uses `rustix`). So `cargo zigbuild` cross-compiles without any target FUSE libraries.
- **Toolchain prereqs for local verification** (your dedicated machine, per setup): zig **0.14.0** on `PATH` and a `cargo-zigbuild` that supports it. Install zig 0.14.0 from `https://ziglang.org/download/0.14.0/zig-linux-x86_64-0.14.0.tar.xz`; install/confirm cargo-zigbuild with `cargo install cargo-zigbuild` (or use the pinned `0.22.3`). Docker (or podman with the same flags) is required for the Task 3 spike.

---

## File Structure

- **Modify:** `.github/workflows/release.yml` — the only workflow touched. Four jobs change: `build` (zig bump + 2 matrix rows), `smoke` (emulated leg + QEMU step + `continue-on-error`), `images` (third staged arch + platform). `gate`, `publish`, `release-assets`, `benchmarks` are untouched (verified arch-generic).
- **Modify:** `README.md` — "Prebuilt binaries" table only.
- **Modify:** `CHANGELOG.md` — one `### Added` bullet under `## [Unreleased]`.
- **No change:** `docker/Dockerfile.glibc`, `docker/Dockerfile.musl` (already `${TARGETARCH}`), `scripts/smoke-binary.sh`, `scripts/container_tags.py`, all crate source.

---

## Task 1: Bump zig to 0.14 and verify existing targets still build

The release build pins `ZIG_VERSION: "0.13.0"`, which cannot emit riscv64 glibc (zig#20909 shipped in 0.14.0). Bump it first, in isolation, and prove the four *existing* targets still cross-compile — this is the regression guard before adding any new arch.

**Files:**
- Modify: `.github/workflows/release.yml:159` (the `ZIG_VERSION` env in the `build` job's "Install Zig and cargo-zigbuild" step)

- [ ] **Step 1: Edit the zig version pin**

In `.github/workflows/release.yml`, inside the `build` job's "Install Zig and cargo-zigbuild" step `env:` block, change:

```yaml
          ZIG_VERSION: "0.13.0"
```

to:

```yaml
          ZIG_VERSION: "0.14.0"
```

Leave `CARGO_ZIGBUILD_VERSION: "0.22.3"` as-is for now; Step 3 verifies it works with zig 0.14. The URL in the step (`zig-linux-x86_64-${ZIG_VERSION}.tar.xz`) is version-interpolated, so no other edit is needed there.

- [ ] **Step 2: Lint the workflow**

Run: `yamllint .github/workflows/release.yml`
Expected: no errors (clean exit). If `yamllint` is absent, install it or rely on the pre-commit hook's yamllint leg.

- [ ] **Step 3: Regression-build the existing targets locally against zig 0.14**

Confirm zig 0.14 + the pinned cargo-zigbuild can still build all four current release targets. Ensure zig 0.14.0 is on `PATH` (`zig version` → `0.14.0`), then:

```bash
rustup target add x86_64-unknown-linux-gnu aarch64-unknown-linux-gnu \
                  x86_64-unknown-linux-musl aarch64-unknown-linux-musl
cargo zigbuild --release -p musefs --target x86_64-unknown-linux-gnu.2.17
cargo zigbuild --release -p musefs --target aarch64-unknown-linux-gnu.2.17
cargo zigbuild --release -p musefs --target x86_64-unknown-linux-musl
cargo zigbuild --release -p musefs --target aarch64-unknown-linux-musl
```

Expected: all four finish `Compiling … Finished release`. If cargo-zigbuild errors that the zig version is unsupported, bump `CARGO_ZIGBUILD_VERSION` in the workflow to the latest (check `cargo install cargo-zigbuild --version` output / its release notes for the zig-0.14-compatible version) and re-run; record the version you landed on.

- [ ] **Step 4: Commit**

```bash
git add .github/workflows/release.yml
git commit -m "$(cat <<'EOF'
ci(release): bump zig to 0.14 for riscv64 glibc support

zig 0.13 cannot emit glibc for riscv64-linux-gnu (zig#20909 shipped in
0.14.0). Bump the release build toolchain; the four existing targets
were re-verified to still cross-compile against 0.14.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>
EOF
)"
```

(If you also bumped `CARGO_ZIGBUILD_VERSION`, include it in this same commit and mention it in the body.)

---

## Task 2: Add riscv64 to the build matrix

Add the two riscv64 targets to the `build` job and prove both cross-compile locally, including the `.2.27` glibc floor.

**Files:**
- Modify: `.github/workflows/release.yml:140-148` (the `build` job `matrix.include` list)

- [ ] **Step 1: Add the two matrix rows**

In the `build` job's `strategy.matrix.include`, after the `aarch64-unknown-linux-musl` entry (`.github/workflows/release.yml:147-148`), add:

```yaml
          - triple: riscv64gc-unknown-linux-gnu
            zig_target: riscv64gc-unknown-linux-gnu.2.27
          - triple: riscv64gc-unknown-linux-musl
            zig_target: riscv64gc-unknown-linux-musl
```

Keep alignment with the existing entries (two-space list indent under `include:`). No other step in the `build` job changes — `rustup target add ${{ matrix.triple }}`, `cargo zigbuild --target ${{ matrix.zig_target }}`, packaging, and upload are all matrix-generic.

- [ ] **Step 2: Lint the workflow**

Run: `yamllint .github/workflows/release.yml`
Expected: clean exit.

- [ ] **Step 3: Cross-compile both riscv64 targets locally**

```bash
rustup target add riscv64gc-unknown-linux-gnu riscv64gc-unknown-linux-musl
cargo zigbuild --release -p musefs --target riscv64gc-unknown-linux-gnu.2.27
cargo zigbuild --release -p musefs --target riscv64gc-unknown-linux-musl
```

Expected: both finish `Finished release`. The glibc one is the real test of Task 1's zig bump + the `.2.27` floor — if it fails with a glibc-version or "unknown target" error, recheck the zig version (Step 3 of Task 1) and the `.2.27` suffix. Confirm the artifacts exist:

```bash
file target/riscv64gc-unknown-linux-gnu/release/musefs
file target/riscv64gc-unknown-linux-musl/release/musefs
```

Expected: both report `ELF 64-bit LSB ... UCB RISC-V` (the musl one statically linked). **Keep these binaries** — Task 3's spike reuses the glibc one.

- [ ] **Step 4: Commit**

```bash
git add .github/workflows/release.yml
git commit -m "$(cat <<'EOF'
ci(release): build riscv64gc glibc + musl release binaries

Add riscv64gc-unknown-linux-{gnu,musl} to the cargo-zigbuild matrix.
glibc pins .2.27 (riscv64's glibc floor); musl is unsuffixed. Both
cross-compile verified locally.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 3: Spike — prove the emulated FUSE mount works (no commit)

Before wiring the emulated smoke into the workflow, prove `scripts/smoke-binary.sh` actually passes under `docker run --platform linux/riscv64`. This is a throwaway validation that decides Task 4's shape (full smoke vs `--version`-only fallback). **No commit.**

**Files:** none modified. Uses the Task 2 glibc binary and the existing `scripts/smoke-binary.sh`.

- [ ] **Step 1: Register QEMU binfmt handlers locally**

```bash
docker run --rm --privileged tonistiigi/binfmt --install riscv64
```

Expected: JSON output listing `riscv64` among the installed emulators. (Skip if your host already has `qemu-riscv64` binfmt registered; verify with `cat /proc/sys/fs/binfmt_misc/qemu-riscv64` → `enabled`.)

- [ ] **Step 2: Stage the binary the way the smoke job will see it**

The smoke job extracts the tarball into `./bin/musefs`. Mirror that with the Task 2 glibc binary:

```bash
mkdir -p bin && cp target/riscv64gc-unknown-linux-gnu/release/musefs bin/musefs
```

- [ ] **Step 3: Run the real smoke under emulation (glibc)**

```bash
docker run --rm --platform linux/riscv64 \
  --device /dev/fuse --cap-add SYS_ADMIN --security-opt apparmor=unconfined \
  -v "$PWD":/w -w /w \
  debian:bookworm-slim \
  sh -c 'apt-get update >/dev/null && apt-get install -y --no-install-recommends fuse3 ffmpeg >/dev/null && sh scripts/smoke-binary.sh ./bin/musefs'
```

Expected (success): the script prints `smoke: read <N> bytes ... (fLaC OK)` and `smoke: SIGTERM unmounted cleanly — PASS`, exit 0. It will be slow (ffmpeg under emulation — allow several minutes).

- [ ] **Step 4: Record the outcome and decide Task 4's shape**

- **If the smoke PASSES:** Task 4 wires the full `scripts/smoke-binary.sh` run (the default). Proceed.
- **If the mount fails** (e.g. `fusermount3` errors, `/dev/fuse` unusable, or it hangs past the 30s loops): Task 4 uses the **`--version`-only fallback** instead. Note which, and why, in the Task 4 commit body.

Either way the binaries and images still ship; this only decides how much the emulated leg asserts.

- [ ] **Step 5: Clean up the spike artifacts**

```bash
rm -rf bin
```

(`bin/` is throwaway staging; do not commit it.)

---

## Task 4: Wire the emulated smoke leg into the `smoke` job

Add the riscv64 emulated smoke legs, the QEMU setup step they need, and the `continue-on-error` gate so a flaky/failed emulated leg cannot block `images`/`publish`/`release-assets` (all of which `needs: smoke`).

**Files:**
- Modify: `.github/workflows/release.yml:191-242` (the `smoke` job: `strategy`, `matrix.include`, and steps)

- [ ] **Step 1: Add the per-leg `continue-on-error` gate to the job**

In the `smoke` job, add a `continue-on-error` keyed off the matrix mode. Place it alongside `runs-on` (job level), e.g. immediately after `runs-on: ${{ matrix.runner }}` (`.github/workflows/release.yml:209`):

```yaml
    continue-on-error: ${{ matrix.mode == 'emulated' }}
```

This makes only the emulated legs non-blocking; `host`/`alpine` legs stay hard-gating.

- [ ] **Step 2: Add the two emulated matrix rows**

In the `smoke` job's `strategy.matrix.include`, after the `aarch64-unknown-linux-musl` entry (`.github/workflows/release.yml:206-208`), add:

```yaml
          - triple: riscv64gc-unknown-linux-gnu
            runner: ubuntu-latest
            mode: emulated
            platform: linux/riscv64
            image: debian:bookworm-slim
            pkg: apt-get update >/dev/null && apt-get install -y --no-install-recommends fuse3 ffmpeg >/dev/null
          - triple: riscv64gc-unknown-linux-musl
            runner: ubuntu-latest
            mode: emulated
            platform: linux/riscv64
            image: alpine:3.20
            pkg: apk add --no-cache fuse3 ffmpeg >/dev/null
```

(The `platform`/`image`/`pkg` fields exist only on these rows; the host/alpine steps never reference them because they are `if`-gated on their own modes.)

- [ ] **Step 3: Add the QEMU setup step**

The existing "Download artifact" and "Extract binary" steps are mode-agnostic and unchanged (the `./bin/musefs --version || true` in Extract is harmless for a riscv64 binary on the x86 host — it just fails the `|| true`). After the "Extract binary" step (`.github/workflows/release.yml:219-227`) and before the "Smoke (host)" step, add:

```yaml
      - name: Set up QEMU (emulated arch)
        if: matrix.mode == 'emulated'
        uses: docker/setup-qemu-action@06116385d9baf250c9f4dcb4858b16962ea869c3
```

(Same SHA pin as the `images` job at `release.yml:314`, per the repo's SHA-pinning convention.)

- [ ] **Step 4: Add the emulated smoke step**

After the "Smoke (Alpine container)" step (`.github/workflows/release.yml:234-242`), add:

```yaml
      - name: Smoke (emulated container)
        if: matrix.mode == 'emulated'
        run: |
          set -euo pipefail
          docker run --rm --platform ${{ matrix.platform }} \
            --device /dev/fuse --cap-add SYS_ADMIN --security-opt apparmor=unconfined \
            -v "$PWD":/w -w /w \
            ${{ matrix.image }} \
            sh -c '${{ matrix.pkg }} && sh scripts/smoke-binary.sh ./bin/musefs'
```

**If Task 3 chose the `--version`-only fallback**, replace the final `sh -c '...'` line with:

```yaml
            sh -c '${{ matrix.pkg }} && ./bin/musefs --version'
```

- [ ] **Step 5: Lint the workflow**

Run: `yamllint .github/workflows/release.yml`
Expected: clean exit.

- [ ] **Step 6: Sanity-check the matrix structurally**

Confirm the emulated entries parse and carry the expected fields (no live emulation here — that was Task 3):

```bash
python -c "import yaml,sys; d=yaml.safe_load(open('.github/workflows/release.yml')); inc=d['jobs']['smoke']['strategy']['matrix']['include']; em=[e for e in inc if e.get('mode')=='emulated']; assert len(em)==2, em; assert all(e['platform']=='linux/riscv64' for e in em); print('emulated legs OK:', [e['triple'] for e in em])"
```

Expected: `emulated legs OK: ['riscv64gc-unknown-linux-gnu', 'riscv64gc-unknown-linux-musl']`

- [ ] **Step 7: Commit**

```bash
git add .github/workflows/release.yml
git commit -m "$(cat <<'EOF'
ci(release): emulated riscv64 FUSE smoke under QEMU

Run the riscv64 binary through scripts/smoke-binary.sh inside a
docker --platform linux/riscv64 container (qemu-user; mount syscalls
reach the host kernel). continue-on-error on the emulated legs so
emulation flakiness can't block images/publish/release-assets.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>
EOF
)"
```

(If you used the `--version`-only fallback, say so in the body and why the full mount didn't work under emulation.)

---

## Task 5: Add riscv64 to the multi-arch Docker images

The Dockerfiles already `COPY ${TARGETARCH}/musefs`, so they need no change. Stage a third arch and add it to the manifest platform list.

**Files:**
- Modify: `.github/workflows/release.yml:244-360` (the `images` job: `matrix`, a new download step, the stage run step, and the manifest `platforms`)

- [ ] **Step 1: Add `riscv64_triple` to each matrix variant**

In the `images` job's `strategy.matrix.include`, add a `riscv64_triple` to both variants. The glibc entry (`.github/workflows/release.yml:254-258`) gains:

```yaml
            riscv64_triple: riscv64gc-unknown-linux-gnu
```

The musl entry (`.github/workflows/release.yml:259-263`) gains:

```yaml
            riscv64_triple: riscv64gc-unknown-linux-musl
```

- [ ] **Step 2: Add the riscv64 download step**

After the "Download arm64 artifact" step (`.github/workflows/release.yml:276-280`), add:

```yaml
      - name: Download riscv64 artifact
        uses: actions/download-artifact@3e5f45b2cfb9172054b4087a40e8e0b5a5461e7c
        with:
          name: musefs-${{ matrix.riscv64_triple }}
          path: dl/riscv64
```

- [ ] **Step 3: Stage the riscv64 binary**

In the "Stage binaries into the build context" run step, after the `stage arm64 "${{ matrix.arm64_triple }}"` line (`.github/workflows/release.yml:296`), add:

```bash
          stage riscv64 "${{ matrix.riscv64_triple }}"
```

The `stage()` function and `TARGETARCH`-based `COPY` already handle any `arch` name (`dl/riscv64` → `ctx/riscv64/musefs`), so nothing else in that step changes.

- [ ] **Step 4: Add `linux/riscv64` to the manifest platforms**

In the "Build and push multi-arch manifest" step, change the `platforms` line (`.github/workflows/release.yml:347`):

```yaml
          platforms: linux/amd64,linux/arm64
```

to:

```yaml
          platforms: linux/amd64,linux/arm64,linux/riscv64
```

The amd64-only "Build … for smoke" / "Smoke the built image" steps are unchanged — they validate the Dockerfile, not each arch; the extra `ctx/riscv64/musefs` is harmless to them.

- [ ] **Step 5: Lint the workflow**

Run: `yamllint .github/workflows/release.yml`
Expected: clean exit.

- [ ] **Step 6: Sanity-check the images job structurally**

```bash
python -c "import yaml; d=yaml.safe_load(open('.github/workflows/release.yml')); inc=d['jobs']['images']['strategy']['matrix']['include']; assert all('riscv64_triple' in e for e in inc), inc; steps=[s.get('name','') for s in d['jobs']['images']['steps']]; assert 'Download riscv64 artifact' in steps, steps; print('images riscv64 wiring OK')"
```

Expected: `images riscv64 wiring OK`

- [ ] **Step 7: Commit**

```bash
git add .github/workflows/release.yml
git commit -m "$(cat <<'EOF'
ci(release): publish linux/riscv64 Docker images

Stage the riscv64 binary into the build context and add linux/riscv64
to both the glibc and musl multi-arch manifests. Dockerfiles are
already ${TARGETARCH}-generic, so no Dockerfile change.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 6: Documentation

**Files:**
- Modify: `README.md:75-82` (the "Prebuilt binaries" table)
- Modify: `CHANGELOG.md:11-13` (the `## [Unreleased]` → `### Added` list)

- [ ] **Step 1: Update the README prebuilt-binaries table**

In `README.md`, change the lead-in line (`README.md:75`):

```markdown
Each tagged release attaches static/portable Linux binaries for four targets:
```

to:

```markdown
Each tagged release attaches static/portable Linux binaries for six targets:
```

Then add two rows to the table, after the `aarch64-unknown-linux-musl` row (`README.md:82`):

```markdown
| `riscv64gc-unknown-linux-gnu` | glibc | glibc 2.27 floor, RISC-V 64. |
| `riscv64gc-unknown-linux-musl` | musl | Fully static, RISC-V 64. |
```

(The "Platform support" table at `README.md:265-271` is OS-level and needs no change.)

- [ ] **Step 2: Add the CHANGELOG entry**

In `CHANGELOG.md`, under `## [Unreleased]` → `### Added`, add as the first bullet (before the `statfs` entry at `CHANGELOG.md:13`):

```markdown
- **riscv64 release platform:** prebuilt `riscv64gc-unknown-linux-{gnu,musl}`
  binaries and `linux/riscv64` Docker images now ship with each tagged release.
```

- [ ] **Step 3: Verify the docs**

Run: `git diff --stat README.md CHANGELOG.md`
Expected: both files show as modified. Eyeball the table renders with six rows and the CHANGELOG bullet sits under `### Added`.

- [ ] **Step 4: Commit**

This is a docs-only commit (the pre-commit cargo gate is skipped):

```bash
git add README.md CHANGELOG.md
git commit -m "$(cat <<'EOF'
docs: announce riscv64 prebuilt binaries and images

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Final verification

- [ ] **All commits present and green**

```bash
git log --oneline -6
```

Expected: the six task commits (Tasks 1, 2, 4, 5 touch `release.yml`; Task 3 has none; Task 6 docs). Each commit passed the pre-commit hook (full workspace tests for the YAML commits, yamllint, ruff).

- [ ] **Whole-workflow lint**

Run: `yamllint .github/workflows/release.yml`
Expected: clean exit.

- [ ] **End-to-end confidence:** the riscv64 glibc + musl binaries cross-compiled locally (Task 2), and the emulated FUSE mount was proven in the Task 3 spike (or the documented fallback was taken). The remaining end-to-end proof — artifact upload, image push, release attach — only exercises on a real `v*` tag push, which is out of scope for this branch; merging this plan makes the *next* tagged release produce riscv64 outputs.

---

## Self-review notes (author)

- **Spec coverage:** zig bump (§Gotcha 1 → Task 1), `.2.27` floor + build matrix (§Gotcha 2, §1 → Task 2), emulated-mount spike (§0 → Task 3), emulated smoke + `continue-on-error` (§2 → Task 4), images staging/platforms (§3 → Task 5), publish/release-assets no-change (§4 → confirmed in File Structure / Final verification, no task needed), docs (§5 → Task 6). All spec sections map to a task.
- **No placeholders:** every code/YAML step shows the exact text and a line anchor; commands have expected output.
- **Consistency:** matrix field names (`triple`, `zig_target`, `mode`, `platform`, `image`, `pkg`, `riscv64_triple`) are used identically across Tasks 2/4/5; the `stage` helper name matches the existing `release.yml` function.
