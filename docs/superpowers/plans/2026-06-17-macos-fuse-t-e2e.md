# macOS FUSE e2e via fuse-t — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Run musefs's mounted FUSE e2e suite on a GitHub-hosted macOS runner via fuse-t, as a best-effort, fuse-gated CI job, without disturbing the existing dep-free macOS compile path.

**Architecture:** Add an opt-in `musefs-fuse` cargo feature `macos-mount = ["fuser/libfuse"]` that enables fuser's libfuse mount path on macOS; on a `macos-latest` runner that path links fuse-t's drop-in libfuse (userspace FUSE↔NFS, no kernel extension). A new `macos-e2e` CI job — modeled on the existing `freebsd` job and `tsan`'s best-effort reporting — installs fuse-t + ffmpeg and runs the non-metrics `--ignored` suite. The default macOS build keeps `macos-no-mount` and gains no new dependency.

**Tech Stack:** Rust, `fuser` 0.17, fuse-t (Homebrew), GitHub Actions (`macos-latest`).

## Global Constraints

- **Audio invariant:** original audio bytes are never copied or modified. (No task here touches the read/synthesis path, but never relax it.)
- **`macos-mount` is macOS-only.** It maps to `fuser/libfuse`, which cargo treats as platform-agnostic; enabling it on Linux would try to link libfuse and violate the "never enable libfuse on Linux (static musl can't link it)" invariant in `musefs-fuse/Cargo.toml:14`. Only the macOS CI job ever passes `--features macos-mount`.
- **Default macOS build stays dep-free:** `cargo build` / `cargo test --workspace` on macOS must keep working with `macos-no-mount` and require no fuse-t install.
- **Best-effort gate:** the new job is `continue-on-error: true` and is NOT added to the `ci-ok` aggregator `needs` list (`.github/workflows/ci.yml:506`). A flaky fuse-t run must never block merges.
- **Fuse-gated:** the job runs only on `needs.changes.outputs.fuse == 'true'` or a `v*` tag — identical predicate to the `freebsd` job.
- **No silent skips:** ffmpeg must be asserted present (the playback/ogg tests skip silently without it — `musefs-fuse/tests/playback_pcm.rs:153`).
- **Pre-commit:** docs-only commits skip the cargo gate; any commit touching `musefs-fuse/src/*.rs` or `Cargo.toml` runs the full workspace test suite + clippy `-D warnings` + fmt, and trips `check_mutant_anchors.py` if line:col anchors shift (re-anchor in the same commit). Cargo.toml/feature edits don't move src anchors, but `mount.rs` edits (Task 3) might.
- **Spec:** `docs/superpowers/specs/2026-06/2026-06-17-macos-fuse-t-e2e-design.md`.

**Verification reality:** the mount path is exercised only on macOS. Linux (the dev host) can confirm "the workspace still builds/tests green" but cannot run the fuse-t mount. The real verification loop is: push the branch → the fuse-gated `macos-e2e` job (or the Task 1 spike job) runs → observe via `gh run watch` / `gh run view`. Each task states its Linux-local check and its macOS-CI check separately.

---

## Task 1: Feasibility spike (temporary CI job)

This task is **investigation, not TDD.** It de-risks three unknowns the rest of the plan branches on, by running a throwaway job on a real `macos-latest` runner. Its deliverable is a recorded **FINDINGS** block appended to this plan, plus the chosen Approach (A or B).

**Files:**
- Create (temporary, removed in Task 4): `.github/workflows/macos-spike.yml`
- Modify (append findings): `docs/superpowers/plans/2026-06-17-macos-fuse-t-e2e.md`

**Unknowns to resolve:**
1. Does `fuser`'s `libfuse` feature compile/link **together with** `macos-no-mount` (→ Approach A), or must `macos-no-mount` be dropped (→ Approach B)?
2. Does fuse-t accept (or silently ignore) the macFUSE mount options `volname=…` / `noappledouble` (`musefs-fuse/src/platform/mount.rs:74-79`), or does it abort the mount? (→ whether Task 3 is needed)
3. Does dropping the `BackgroundSession` cleanly unmount the fuse-t NFS volume, leaving no stale mount? (→ whether Task 4 needs an explicit cleanup step)

- [ ] **Step 1: Add the temporary spike workflow**

Create `.github/workflows/macos-spike.yml`:

```yaml
name: macos-spike
on: workflow_dispatch
permissions:
  contents: read
jobs:
  spike:
    runs-on: macos-latest
    timeout-minutes: 30
    steps:
      - uses: actions/checkout@df4cb1c069e1874edd31b4311f1884172cec0e10
        with:
          persist-credentials: false
      - name: Install fuse-t + ffmpeg
        run: |
          # fuse-t ships as a cask in its own tap. Confirm the exact name from
          # https://github.com/macos-fuse-t/fuse-t (Homebrew install section).
          brew tap macos-fuse-t/homebrew-cask
          brew install --cask fuse-t
          brew install ffmpeg
      - name: Record fuse-t install layout
        run: |
          echo "== libfuse-t dylib =="; find /usr/local /opt/homebrew -name 'libfuse*' 2>/dev/null || true
          echo "== pkg-config =="; find /usr/local /opt/homebrew -name 'fuse*.pc' 2>/dev/null || true
          pkg-config --exists fuse && echo "pkg-config sees 'fuse'" || echo "pkg-config does NOT see 'fuse'"
      - uses: dtolnay/rust-toolchain@29eef336d9b2848a0b548edc03f92a220660cdb8
      - uses: Swatinem/rust-cache@e18b497796c12c097a38f9edb9d0641fb99eee32
      - name: (A) Build with macos-no-mount + libfuse together
        id: approach_a
        continue-on-error: true
        run: |
          # Temporarily add the feature inline so the spike needs no manifest commit.
          # If this fails to compile/link, Approach B is required.
          PKG_CONFIG_PATH="$(brew --prefix)/lib/pkgconfig:${PKG_CONFIG_PATH:-}" \
            cargo build -p musefs-fuse --features fuser/libfuse 2>&1 | tail -40
      - name: Run one mount test through fuse-t
        if: steps.approach_a.outcome == 'success'
        run: |
          PKG_CONFIG_PATH="$(brew --prefix)/lib/pkgconfig:${PKG_CONFIG_PATH:-}" \
            cargo test -p musefs-fuse --features fuser/libfuse --test mount -- --ignored --nocapture 2>&1 | tail -80
      - name: Inspect for stale mounts after tests
        if: always()
        run: |
          echo "== mounts =="; mount | grep -i fuse || echo "no fuse mounts left (good)"
```

- [ ] **Step 2: Commit and push the spike workflow**

```bash
git add .github/workflows/macos-spike.yml
git commit -m "ci(spike): temporary macos fuse-t feasibility job"
git push -u origin macos-fuse-t-e2e
```

Note: this is a non-docs commit, so the pre-commit hook runs the full workspace test suite + clippy (not "just a workflow file" — expect a couple of minutes).

- [ ] **Step 3: Trigger the spike and watch it**

```bash
gh workflow run macos-spike.yml --ref macos-fuse-t-e2e
sleep 5
gh run watch "$(gh run list --workflow=macos-spike.yml -L1 --json databaseId -q '.[0].databaseId')"
```

- [ ] **Step 4: Record FINDINGS**

Append a `## FINDINGS (Task 1)` section to this plan capturing, verbatim from the logs:
- The working install commands (exact tap/cask names; whether `--no-quarantine` was needed).
- The `PKG_CONFIG_PATH` / env needed for `fuser` to find fuse-t's libfuse.
- **Unknown 1 verdict:** did `--features fuser/libfuse` build with `macos-no-mount` present? → **Approach A** if yes, **Approach B** if no.
- **Unknown 2 verdict:** did the `mount` test pass, or did it fail on `volname`/`noappledouble`? → Task 3 needed iff it aborted on those options.
- **Unknown 3 verdict:** any leftover fuse mount after the run? → Task 4 cleanup step needed iff yes.

Expected: enough detail that Tasks 2–4 contain zero remaining unknowns. Do NOT delete `macos-spike.yml` yet — Task 4 removes it once the real job is green.

---

## Task 2: Add the `macos-mount` cargo feature

Implements **Approach A** (the expected spike outcome). If FINDINGS chose **Approach B**, use the B variant in Step 1 instead (both shown).

**Files:**
- Modify: `musefs-fuse/Cargo.toml` (the `[features]` table at `:10-11` and, for Approach B only, the macOS target dep at `:22-23`)

**Interfaces:**
- Produces: cargo feature `macos-mount` on crate `musefs-fuse`. Enabling it builds `fuser` with `libfuse`. Consumed by the CI job in Task 4 (`cargo test -p musefs-fuse --features macos-mount`).

- [ ] **Step 1: Add the feature to `musefs-fuse/Cargo.toml`**

**Approach A (default — `macos-no-mount` and `libfuse` coexist):** edit the `[features]` table:

```toml
[features]
metrics = ["musefs-core/metrics"]
# macOS-only: enable fuser's libfuse mount path so a macos-latest runner can
# mount through fuse-t's drop-in libfuse. Never enable on Linux (would link
# libfuse, breaking the static-musl build). See docs/.../2026-06-17-macos-fuse-t-e2e-design.md.
macos-mount = ["fuser/libfuse"]
```

Leave the macOS target dep (`musefs-fuse/Cargo.toml:22-23`) unchanged:

```toml
[target.'cfg(target_os = "macos")'.dependencies]
fuser = { version = "0.17", features = ["macos-no-mount"] }
```

**Approach B (only if FINDINGS says the two features conflict):** change the macOS target dep to drop `macos-no-mount` and add a fuse-t install to the existing `macos` compile job (a separate edit to `.github/workflows/ci.yml` — see the spec's Approach B note). The `[features]` line is identical. Do not do B unless Task 1 requires it.

- [ ] **Step 2: Confirm the Linux workspace is unaffected**

The feature is off by default and only adds a dep on macOS, so Linux builds must be byte-for-byte unchanged in behavior.

Run:
```bash
cargo build && cargo test --workspace 2>&1 | tail -5
```
Expected: builds; tests pass (`cargo test:` summary green). No new dependency pulled on Linux.

- [ ] **Step 3: Confirm the manifest parses with the feature selected (Linux, build-plan only)**

`fuser/libfuse` on Linux would try to link libfuse, so do NOT compile it — only check cargo resolves the feature graph:

Run:
```bash
cargo metadata -p musefs-fuse --features macos-mount --no-deps -q >/dev/null && echo "feature graph OK"
```
Expected: `feature graph OK`. The `-p musefs-fuse` scope is required: a workspace-root `cargo metadata --features X` silently accepts a *nonexistent* feature (verified — it exits 0 for a bogus name), so only the package-scoped form actually rejects a misspelled/missing `macos-mount`. This resolves the feature graph without compiling/linking libfuse, so it still honors "never link libfuse on Linux"; the actual link is exercised on macOS in Task 4.

- [ ] **Step 4: Commit**

```bash
git add musefs-fuse/Cargo.toml
git commit -m "feat(fuse): add macos-mount feature (opt-in fuser libfuse for fuse-t)"
```

---

## Task 3: Conditionalize macFUSE mount options for fuse-t

**CONDITIONAL — do this task only if Task 1 FINDINGS (Unknown 2) shows fuse-t aborts the mount on `volname`/`noappledouble`.** If fuse-t ignored them, skip to Task 4 and note "Task 3 not needed" in the plan.

The mount path `mount_config → platform::mount::options → extend_os_specific` (`musefs-fuse/src/lib.rs:894-896`, `musefs-fuse/src/platform/mount.rs:10-18,74-79`) unconditionally pushes macFUSE-only options on macOS. These are dead today (`macos-no-mount`) and go live under fuse-t. Gate them behind an env var the CI job sets, so production macFUSE behavior is unchanged.

**Files:**
- Modify: `musefs-fuse/src/platform/mount.rs:74-79` (the macOS `extend_os_specific`)

**Interfaces:**
- Produces: a runtime seam — when `MUSEFS_FUSE_T` is set in the environment, `extend_os_specific` omits the macFUSE-only options. Consumed by Task 4 (the CI job exports `MUSEFS_FUSE_T=1`). The env read is isolated in `extend_os_specific`; the option-building logic lives in a pure helper `push_macos_options(opts, fs_name, macfuse: bool)` that the test drives directly — **no env mutation, no `unsafe` in the test** (the workspace denies `unsafe_code`; bare `unsafe {}` would fail clippy `-D warnings`).

- [ ] **Step 1: Write the failing test**

The existing `#[cfg(test)] mod tests` in `musefs-fuse/src/platform/mount.rs:85` runs on every platform; add a macOS-gated test there. Insert after the existing tests. It calls the pure helper added in Step 3 with `macfuse = false` (the fuse-t case) — deterministic, no process-env mutation:

```rust
#[cfg(target_os = "macos")]
#[test]
fn fuse_t_omits_macfuse_options() {
    let mut opts = Vec::new();
    push_macos_options(&mut opts, "muse", false);
    let has_macfuse = opts.iter().any(|o| {
        matches!(o, MountOption::CUSTOM(s) if s.starts_with("volname=") || s == "noappledouble")
    });
    assert!(!has_macfuse, "fuse-t mount must omit macFUSE-only options");
}

#[cfg(target_os = "macos")]
#[test]
fn macfuse_keeps_volname_and_noappledouble() {
    let mut opts = Vec::new();
    push_macos_options(&mut opts, "muse", true);
    assert!(opts.iter().any(|o| matches!(o, MountOption::CUSTOM(s) if s == "volname=muse")));
    assert!(opts.iter().any(|o| matches!(o, MountOption::CUSTOM(s) if s == "noappledouble")));
}
```

- [ ] **Step 2: Run the test to verify it fails (macOS only)**

This test is `#[cfg(target_os = "macos")]`, so it compiles to nothing on Linux, AND `push_macos_options` does not exist yet — so it won't even compile on macOS until Step 3. Verify on the macOS runner (it runs inside the Task 4 job's `cargo test --workspace` leg; at this point in the plan, run it via the spike job by temporarily pointing `macos-spike.yml`'s test step at `cargo test -p musefs-fuse --lib`). Expected: compile error "cannot find function `push_macos_options`", then once Step 3's helper exists but before the gate is wired, `fuse_t_omits_macfuse_options` would FAIL.

On Linux confirm only that the workspace still compiles: `cargo build -p musefs-fuse` → builds (neither test body is compiled off-macOS).

- [ ] **Step 3: Split the env read from a pure option builder**

Replace `extend_os_specific` (`musefs-fuse/src/platform/mount.rs:74-80`) with an env-reading wrapper plus the pure helper the tests drive:

```rust
#[cfg(target_os = "macos")]
fn extend_os_specific(opts: &mut Vec<MountOption>, fs_name: &str) {
    // fuse-t (userspace FUSE↔NFS) does not accept macFUSE's volname/noappledouble
    // options and aborts the mount if given them. MUSEFS_FUSE_T (set by CI's macOS
    // e2e job) marks a fuse-t backend; absent it we assume macFUSE.
    let macfuse = std::env::var_os("MUSEFS_FUSE_T").is_none();
    push_macos_options(opts, fs_name, macfuse);
}

// Pure so it's testable without mutating the process environment (the workspace
// denies `unsafe_code`, so a test can't call the edition-2024 `unsafe`
// `set_var`). `macfuse` gates the macFUSE-only options.
#[cfg(target_os = "macos")]
fn push_macos_options(opts: &mut Vec<MountOption>, fs_name: &str, macfuse: bool) {
    if !macfuse {
        return;
    }
    // fuser 0.17 has no `VolName` variant; macOS-specific options go through
    // CUSTOM. `noappledouble` stops Finder writing ._ sidecar files.
    opts.push(MountOption::CUSTOM(format!("volname={fs_name}")));
    opts.push(MountOption::CUSTOM("noappledouble".to_string()));
}
```

- [ ] **Step 4: Run the tests to verify they pass (macOS)**

On the macOS runner: `cargo test -p musefs-fuse --lib platform::mount 2>&1 | tail` → both tests PASS. On Linux: `cargo test --workspace 2>&1 | tail -5` → still green (tests not compiled there; no behavior change off-macOS).

- [ ] **Step 5: Re-anchor mutants if the pre-commit hook complains**

Editing `mount.rs` may shift `.cargo/mutants.toml` line:col anchors. If the commit is rejected by `check_mutant_anchors.py`, re-anchor in the same commit per each entry's `# guard:` tag (try `python3 scripts/check_mutant_anchors.py --fix` first; verify the diff).

- [ ] **Step 6: Commit**

```bash
git add musefs-fuse/src/platform/mount.rs .cargo/mutants.toml
git commit -m "fix(fuse): omit macFUSE-only mount options under fuse-t (MUSEFS_FUSE_T)"
```

---

## Task 4: Add the `macos-e2e` CI job and remove the spike

**Files:**
- Modify: `.github/workflows/ci.yml` (add `macos-e2e` job after the `macos` job at `:369`; do NOT touch the `ci-ok` `needs` list at `:506`)
- Delete: `.github/workflows/macos-spike.yml`

- [ ] **Step 1: Add the `macos-e2e` job**

Insert after the `macos` job (`.github/workflows/ci.yml:369`), using the recipe recorded in Task 1 FINDINGS for the install/env lines. The skeleton (fill the two `# from FINDINGS` spots with the exact verified values):

```yaml
  macos-e2e:
    # Mounted FUSE e2e on macOS via fuse-t (userspace FUSE↔NFS, no kext).
    # Best-effort: continue-on-error and deliberately NOT in `ci-ok` — fuse-t is
    # newer than macFUSE and this must never block merges. Fuse-gated like the
    # freebsd job (hosted macOS minutes are ~10x), or on a release tag.
    needs: changes
    if: >-
      startsWith(github.ref, 'refs/tags/') ||
      needs.changes.outputs.fuse == 'true'
    runs-on: macos-latest
    continue-on-error: true
    timeout-minutes: 30
    env:
      MUSEFS_FUSE_T: "1"   # makes extend_os_specific skip macFUSE-only options (Task 3)
    steps:
      - uses: actions/checkout@df4cb1c069e1874edd31b4311f1884172cec0e10
        with:
          persist-credentials: false
      - name: Install fuse-t + ffmpeg
        run: |
          brew tap macos-fuse-t/homebrew-cask   # exact name from FINDINGS
          brew install --cask fuse-t             # exact name from FINDINGS
          brew install ffmpeg
      - name: Assert ffmpeg present (loud-fail, not silent skip)
        run: command -v ffmpeg >/dev/null || { echo "ffmpeg missing"; exit 1; }
      - uses: dtolnay/rust-toolchain@29eef336d9b2848a0b548edc03f92a220660cdb8
      - uses: Swatinem/rust-cache@e18b497796c12c097a38f9edb9d0641fb99eee32
      - name: FUSE end-to-end tests (fuse-t)
        env:
          PKG_CONFIG_PATH: /usr/local/lib/pkgconfig   # exact value from FINDINGS
        run: cargo test -p musefs-fuse --features macos-mount -- --ignored
```

If Task 1 Unknown 3 found a stale mount, add a final cleanup step:

```yaml
      - name: Clean up any leftover fuse-t mount
        if: always()
        run: mount | awk '/fuse|nfs/ {print $3}' | xargs -r -n1 umount -f || true
```

- [ ] **Step 2: Remove the temporary spike workflow**

```bash
git rm .github/workflows/macos-spike.yml
```

- [ ] **Step 3: Validate the workflow YAML locally**

Run (the pre-commit hook also yamllints tracked YAML):
```bash
yamllint .github/workflows/ci.yml && echo "yaml ok"
```
Expected: `yaml ok` (no errors; warnings tolerated per repo config).

- [ ] **Step 4: Confirm `macos-e2e` is NOT in `ci-ok`**

Run:
```bash
grep -n 'needs: \[changes' .github/workflows/ci.yml
```
Expected: the `ci-ok` `needs` list does NOT contain `macos-e2e` (best-effort gate).

- [ ] **Step 5: Commit and push**

```bash
git add .github/workflows/ci.yml
git commit -m "ci: add best-effort macOS fuse-t e2e job; drop spike"
git push
```

- [ ] **Step 6: Verify the real job runs green on a fuse-touching change**

The branch already touches `musefs-fuse/` and `ci.yml`, so `changes.fuse` is true and `macos-e2e` runs. Watch it:
```bash
RUN_ID="$(gh run list --workflow=ci.yml --branch=macos-fuse-t-e2e -L1 --json databaseId -q '.[0].databaseId')"
gh run watch "$RUN_ID"
# `gh run view --job` needs a JOB id, not a run id; read the run and grep the
# job's own conclusion (the run can be green while a continue-on-error job failed).
gh run view "$RUN_ID" --json jobs -q '.jobs[] | select(.name|test("macos-e2e")) | "\(.name): \(.conclusion)"'
```
Expected: `macos-e2e: success` — it mounts via fuse-t and passes the non-metrics `--ignored` subset. Because the job is `continue-on-error`, confirm it genuinely PASSED (not merely "didn't block the run") by reading that per-job conclusion.

---

## Task 5: Documentation

**Files:**
- Modify: `docs/src/guide/installation.md:9` (the "not E2E tested on macOS" note) and `:74` (platform table row)
- Modify: `docs/src/contributing/setup.md` — **replace** the stale macOS paragraph at `:172-174` (which still says "Mounted e2e on macOS/FUSE-T is not yet validated") with a `### macOS e2e (fuse-t)` subsection, immediately after the FreeBSD e2e block (which ends at the `Notes:` list, ~`:165-170`)

- [ ] **Step 1: Update the installation note (`installation.md:9`)**

Replace:
```markdown
> **Important:** Linux and FreeBSD are E2E tested. I don't have anything running macOS to test on, if you run this on one let me know if it works, or especially if it doesn't!
```
with:
```markdown
> **Important:** Linux and FreeBSD are E2E tested as required gates. macOS is E2E tested best-effort, via fuse-t on a hosted runner (see [macOS e2e](../contributing/setup.md#macos-e2e-fuse-t)) — report any macOS issues you hit.
```

- [ ] **Step 2: Update the platform-support table row (`installation.md:74`)**

Replace:
```markdown
| macOS (FUSE-T) | Best-effort | No | Compiles and runs unit tests with `macos-no-mount`; mounted e2e is not yet validated. |
```
with:
```markdown
| macOS (fuse-t) | Yes (userspace FUSE↔NFS, no kext) | No | Default build uses `macos-no-mount` (compile + unit tests). Mounted e2e runs best-effort in CI via fuse-t (`--features macos-mount`). |
```

- [ ] **Step 3: Add the `### macOS e2e (fuse-t)` subsection to `setup.md`**

**Replace** the now-false macOS paragraph at `docs/src/contributing/setup.md:172-174`:

```markdown
macOS support is best-effort: CI builds there with `fuser`'s `macos-no-mount`
feature, and the platform-specific logic is unit-tested. Mounted e2e on
macOS/FUSE-T is not yet validated.
```

with this subsection (it sits right after the FreeBSD e2e `Notes:` list, so the FreeBSD section stays intact):

```markdown
### macOS e2e (fuse-t)

The FUSE e2e suite also runs on macOS via [fuse-t](https://www.fuse-t.org/) — a
userspace FUSE↔NFS server that needs no kernel extension, so it works on
GitHub-hosted `macos-latest` runners (macFUSE's kext can't load there).

The default macOS build uses `fuser`'s `macos-no-mount` stub (compile + unit
tests only, no fuse-t needed). The opt-in `macos-mount` feature
(`musefs-fuse/Cargo.toml`) enables `fuser/libfuse`, which links fuse-t's drop-in
libfuse so the mount path runs. The feature is macOS-only — never pass it on
Linux (it would link libfuse and break the static-musl build).

**CI.** The `macos-e2e` job in [`.github/workflows/ci.yml`](../../../.github/workflows/ci.yml)
installs fuse-t + ffmpeg via Homebrew and runs
`cargo test -p musefs-fuse --features macos-mount -- --ignored` (the same
non-metrics subset FreeBSD runs; the `metrics`-gated passthrough/concurrency
tests are excluded). It is **best-effort**: gated on FUSE-surface changes or a
release tag like the FreeBSD job, `continue-on-error`, and deliberately not part
of the `ci-ok` required-checks aggregator — fuse-t is newer than macFUSE, so a
flaky run must never block merges. `MUSEFS_FUSE_T=1` tells the mount path to omit
macFUSE-only mount options that fuse-t rejects.
```

- [ ] **Step 4: Build the docs and link-check (if mdBook available)**

Run:
```bash
grep -n 'macos-e2e-fuse-t' docs/src/guide/installation.md && echo "anchor target referenced"
```
Expected: the cross-reference resolves to the new heading's slug. (Per the mdbook-linkcheck gotchas, `#anchor` fragments aren't verified by linkcheck, so just confirm the slug matches: `### macOS e2e (fuse-t)` → `macos-e2e-fuse-t`.)

- [ ] **Step 5: Commit (docs-only — cargo gate skipped)**

```bash
git add docs/src/guide/installation.md docs/src/contributing/setup.md
git commit -m "docs: macOS is now best-effort E2E tested via fuse-t"
```

---

## Self-Review

**Spec coverage:**
- fuse-t route, no kext → Task 1 (spike proves it), Task 4 (job). ✓
- Approach A opt-in feature, B fallback → Task 2 (both variants). ✓
- Phase 0 spike de-risking feature coexistence + mount options + unmount → Task 1 (all three unknowns). ✓
- Non-metrics subset, metrics tests auto-excluded → Task 4 (`--features macos-mount` without `metrics`); the corrected subset (no `concurrency`/`fault_injection`) is enforced by their `#![cfg(feature = "metrics")]`. ✓
- Best-effort, fuse-gated, not in ci-ok → Task 4 Steps 1 & 4. ✓
- ffmpeg loud-fail guard → Task 4 Step 1 (explicit step). ✓
- macFUSE-option conditionalization → Task 3 (conditional on spike). ✓
- Unmount cleanup → Task 4 Step 1 (conditional cleanup step). ✓
- Keep default macOS build dep-free → Task 2 Steps 2-3 + Global Constraints. ✓
- Docs (per the "plans must cover docs" convention) → Task 5. ✓

**Placeholder scan:** The only deferred values are the exact fuse-t tap/cask name and `PKG_CONFIG_PATH`, which are the literal deliverable of the Task 1 spike and are explicitly fed forward into Tasks 2/4 — not vague "TBD"s. Every code/edit step shows real content.

**Type consistency:** `macos-mount` feature name, `MUSEFS_FUSE_T` env var, and `extend_os_specific` signature are used identically across Tasks 2, 3, and 4.

---

## FINDINGS (Task 1 spike — run 27664409801, macos-latest arm64, fuse-t 1.2.7)

**Install recipe (works, no quarantine flag needed):**
```sh
brew tap macos-fuse-t/homebrew-cask
brew install --cask fuse-t      # installs fuse-t 1.2.7 via a sudo pkg installer
brew install ffmpeg
```
fuse-t lays libfuse down in `/usr/local/lib` (`libfuse-t.dylib`, `libfuse3.dylib`,
`libfuse3.4.dylib`, static `.a` variants) with pkg-config files
`/usr/local/lib/pkgconfig/fuse3.pc` and `fuse-t.pc` — **there is NO `fuse.pc`**
(`pkg-config --exists fuse` is false; `fuse3` exists). `fuser`'s build script found
libfuse with **no `PKG_CONFIG_PATH` override** (my `/opt/homebrew/lib/pkgconfig`
guess was wrong, yet it linked) — pkg-config's default macOS search path already
includes `/usr/local/lib/pkgconfig`. So no `PKG_CONFIG_PATH` is needed; the real
job can drop it.

**Unknown 1 (feature coexistence) — RESOLVED, and it kills Approach A.**
`cargo build -p musefs-fuse --features fuser/libfuse` compiled fine WITH the crate's
existing `macos-no-mount` macOS dep (build `Finished in 29.07s`). But the `mount`
test then failed 5/5 instantly (3.13s) with `"Mount is not enabled; this is
test-only configuration"` — fuser's **`macos-no-mount` stub**. Cargo unions
features, so `libfuse` + `macos-no-mount` together leaves the mount stubbed.
Because features are additive (no feature can remove `macos-no-mount`),
**Approach A is not viable**; mounting requires dropping `macos-no-mount`
entirely → **Approach B**.

**Unknown 2 (fuse-t accepts volname/noappledouble) — STILL PENDING.** The stub
short-circuited before any real mount, so we have NOT yet observed whether fuse-t
tolerates the macFUSE options. Must be re-checked once `macos-no-mount` is removed.

**Unknown 3 (clean unmount) — STILL PENDING (vacuous).** "no fuse mounts left"
was reported, but no real mount happened, so this proves nothing. Re-check after B.

**Consequence:** `macos-no-mount` is a compile-only placeholder that can never
mount (matching the docs' "not E2E tested on macOS" / "nothing running macOS to
test on"). Approach B (always link libfuse on macOS) is therefore what makes macOS
a real mounting platform — not merely a CI device. Tradeoff: building
`musefs-fuse` on macOS now needs a FUSE lib present (fuse-t or macFUSE), exactly
as Linux already needs `fuse3`/`libfuse3-dev`. The "dep-free macOS compile" the
original Approach A preserved was only possible because macOS couldn't really
mount. A second spike run (macos-no-mount removed) is needed to close Unknowns 2 & 3.
