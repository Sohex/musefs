# Public Release Setup (v0.2.0) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking. **This is an ops/release plan, not a feature plan** — there are no new unit tests; "verification" steps run build/lint/parse/`gh` commands and check their output. **Part 2 performs irreversible, externally-visible actions (creating a PUBLIC repo, pushing, publishing a release). Run Part 2 directly with the user in the loop — do NOT delegate it to fresh subagents.**

**Goal:** Publish `musefs` as a public GitHub repo at `Sohex/musefs` with release hygiene — workspace-inherited v0.2.0 + crate metadata, a changelog, CI (check + FUSE e2e), a scheduled security audit, a README CI badge, and a manual v0.2.0 tag + GitHub Release.

**Architecture:** Part 1 makes all in-repo changes on the `release-setup` branch (already created and holding the design spec) and merges to `main`. Part 2 creates the GitHub repo, pushes, tags, and cuts the release via the `gh` CLI (account `Sohex`, SSH protocol), then verifies CI is green.

**Tech Stack:** Cargo workspace (5 crates, edition 2021); GitHub Actions (`dtolnay/rust-toolchain`, `Swatinem/rust-cache`, `rustsec/audit-check`); `gh` CLI; `fuser` 0.14 → libfuse3.

**Spec:** `docs/superpowers/specs/2026-05-27-public-release-setup-design.md`

**Preconditions:** On branch `release-setup` (off `main`). `gh auth status` shows account `Sohex`, SSH, scopes incl. `repo` + `workflow`. No `origin` remote yet. The pre-commit hook runs `cargo fmt --all -- --check` + `cargo clippy --all-targets -- -D warnings`, so each commit is gated; run those before committing.

**Commit trailer:** end every commit message with
`Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>`.

---

## Part 1 — In-repo changes (branch `release-setup`)

### Task 1: Workspace versioning + crate metadata

**Files:**
- Modify: `Cargo.toml` (root — add `[workspace.package]`)
- Modify: `musefs-db/Cargo.toml`, `musefs-format/Cargo.toml`, `musefs-core/Cargo.toml`, `musefs-fuse/Cargo.toml`, `musefs-cli/Cargo.toml` (`[package]` blocks)

- [ ] **Step 1: Add `[workspace.package]` to the root `Cargo.toml`**

Insert this block immediately after the `members = [...]` line (i.e. after the `[workspace]` table, before the `[workspace.lints.clippy]` comment block):

```toml
[workspace.package]
version = "0.2.0"
edition = "2021"
license = "MIT"
repository = "https://github.com/Sohex/musefs"
```

- [ ] **Step 2: Convert each crate's `[package]` to inherit + add `description`**

Replace the `[package]` block in each crate with the versions below (leave every other section — `[dependencies]`, `[lints]`, `[[bin]]`, etc. — untouched).

`musefs-db/Cargo.toml`:
```toml
[package]
name = "musefs-db"
description = "SQLite store and schema for musefs (tracks, tags, content-addressed art)."
version.workspace = true
edition.workspace = true
license.workspace = true
repository.workspace = true
```

`musefs-format/Cargo.toml`:
```toml
[package]
name = "musefs-format"
description = "On-the-fly audio metadata synthesis and byte-layout for musefs (FLAC/MP3/MP4/Ogg/WAV)."
version.workspace = true
edition.workspace = true
license.workspace = true
repository.workspace = true
```

`musefs-core/Cargo.toml`:
```toml
[package]
name = "musefs-core"
description = "Orchestration for musefs: virtual tree, tag resolution, and scanning."
version.workspace = true
edition.workspace = true
license.workspace = true
repository.workspace = true
```

`musefs-fuse/Cargo.toml`:
```toml
[package]
name = "musefs-fuse"
description = "FUSE adapter for musefs (fuser)."
version.workspace = true
edition.workspace = true
license.workspace = true
repository.workspace = true
```

`musefs-cli/Cargo.toml` (also gets `keywords` + `categories`):
```toml
[package]
name = "musefs-cli"
description = "musefs command-line interface: scan a music library and mount a re-tagged virtual view."
version.workspace = true
edition.workspace = true
license.workspace = true
repository.workspace = true
keywords = ["fuse", "music", "metadata", "filesystem", "audio"]
categories = ["filesystem", "multimedia::audio", "command-line-utilities"]
```

- [ ] **Step 3: Refresh `Cargo.lock` and verify versions + lints**

Run: `cargo build --workspace`
Expected: builds successfully; `Cargo.lock` updates the five `musefs-*` entries to `0.2.0`.

Then confirm the bump and that metadata parses:
Run: `cargo metadata --no-deps --format-version 1 | tr ',' '\n' | grep -E '"name":"musefs|"version":"0.2.0"' | head`
Expected: each `musefs-*` package shows `"version":"0.2.0"`.

Run: `cargo fmt --all -- --check && cargo clippy --all-targets -- -D warnings`
Expected: both pass (no diff, no warnings).

- [ ] **Step 4: Commit**

```bash
git add Cargo.toml Cargo.lock musefs-db/Cargo.toml musefs-format/Cargo.toml musefs-core/Cargo.toml musefs-fuse/Cargo.toml musefs-cli/Cargo.toml
git commit -m "chore: bump workspace to 0.2.0 and add crate metadata"
```

---

### Task 2: `CHANGELOG.md`

**Files:**
- Create: `CHANGELOG.md`

- [ ] **Step 1: Create the changelog**

Create `CHANGELOG.md` at the repo root:

```markdown
# Changelog

All notable changes to this project are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project
adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.2.0] - 2026-05-27

First public release.

### Added

- **Formats:** synthesis for M4A/M4B (MP4), Ogg (Opus, Vorbis, FLAC-in-Ogg), and
  WAV, alongside the existing FLAC and MP3 — metadata generated on the fly from
  the SQLite store and spliced in front of byte-identical backing audio.
- **Arbitrary tag support:** a single canonical tag vocabulary maps common fields
  to each format's native slot (ID3 frame / MP4 atom / Vorbis field); any other
  tag round-trips through the format's extension slot (ID3 `TXXX`, MP4 `----`
  freeform, raw Vorbis field). User-defined key casing is preserved.
- **beets plugin** (`contrib/beets/`): syncs beets' canonical tags and cover art
  into the store keyed by each file's real path, with no remount and no audio
  rewrite.
- **Performance, concurrency & caching pass:** worker-pool offload of blocking
  reads, lock-free virtual-tree swap, per-handle I/O, a bounded LRU header-layout
  cache, debounced single-flighted refresh with stable inodes, kernel/mount
  tuning flags, bounded-memory MP4 resolves, and opt-in `--keep-cache` with
  auto-invalidation.

### Notes

- Read-only mount; tag edits happen out-of-band against the SQLite store and are
  picked up automatically (`PRAGMA data_version` polling). See the README "Tag
  handling" section for round-trip limitations.

## [0.1.0]

- Initial MVP (FLAC and MP3 synthesis, virtual tree with beets-style templates,
  `synthesis` / `structure-only` mount modes, auto-refresh, `scan` /
  `scan --revalidate`). Never published publicly; superseded by 0.2.0.
```

- [ ] **Step 2: Verify it renders as valid Markdown**

Run: `head -5 CHANGELOG.md`
Expected: shows the `# Changelog` heading and intro lines (sanity check; no broken fences).

- [ ] **Step 3: Commit**

```bash
git add CHANGELOG.md
git commit -m "docs: add CHANGELOG with the 0.2.0 release notes"
```

---

### Task 3: CI workflow (`ci.yml`)

**Files:**
- Create: `.github/workflows/ci.yml`

- [ ] **Step 1: Create the workflow**

Create `.github/workflows/ci.yml`:

```yaml
name: CI

on:
  push:
    branches: [main]
  pull_request:

concurrency:
  group: ci-${{ github.ref }}
  cancel-in-progress: true

jobs:
  check:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
        with:
          components: rustfmt, clippy
      - uses: Swatinem/rust-cache@v2
      - name: Format
        run: cargo fmt --all -- --check
      - name: Clippy
        run: cargo clippy --all-targets -- -D warnings
      - name: Test
        run: cargo test --workspace

  e2e:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - name: Install libfuse3
        run: sudo apt-get update && sudo apt-get install -y fuse3 libfuse3-dev pkg-config
      - uses: dtolnay/rust-toolchain@stable
      - uses: Swatinem/rust-cache@v2
      - name: FUSE end-to-end tests
        run: cargo test -p musefs-fuse -- --ignored
```

- [ ] **Step 2: Verify the YAML parses**

Run: `python3 -c "import yaml; yaml.safe_load(open('.github/workflows/ci.yml')); print('ok')"`
Expected: prints `ok`. (If `actionlint` is installed, also run `actionlint .github/workflows/ci.yml` and expect no errors. Full validation happens when CI runs in Part 2.)

- [ ] **Step 3: Commit**

```bash
git add .github/workflows/ci.yml
git commit -m "ci: add fmt/clippy/test + FUSE e2e workflow"
```

---

### Task 4: Security audit workflow (`audit.yml`)

**Files:**
- Create: `.github/workflows/audit.yml`

- [ ] **Step 1: Create the workflow**

Create `.github/workflows/audit.yml`:

```yaml
name: Security audit

on:
  schedule:
    - cron: '0 6 * * 1'  # Mondays 06:00 UTC
  push:
    paths:
      - '**/Cargo.toml'
      - 'Cargo.lock'
      - '.github/workflows/audit.yml'
  pull_request:
    paths:
      - '**/Cargo.toml'
      - 'Cargo.lock'
      - '.github/workflows/audit.yml'

jobs:
  audit:
    runs-on: ubuntu-latest
    permissions:
      contents: read
      issues: write
      checks: write
    steps:
      - uses: actions/checkout@v4
      - uses: rustsec/audit-check@v2
        with:
          token: ${{ secrets.GITHUB_TOKEN }}
```

- [ ] **Step 2: Verify the YAML parses**

Run: `python3 -c "import yaml; yaml.safe_load(open('.github/workflows/audit.yml')); print('ok')"`
Expected: prints `ok`.

- [ ] **Step 3: Commit**

```bash
git add .github/workflows/audit.yml
git commit -m "ci: add scheduled cargo-audit (RustSec) workflow"
```

---

### Task 5: README CI badge + install line

**Files:**
- Modify: `README.md` (top-of-file badge; Build section install line)

- [ ] **Step 1: Add the CI badge under the title**

In `README.md`, insert a blank line + the badge immediately after the `# musefs` title line (line 1), before the existing description paragraph:

```markdown
# musefs

[![CI](https://github.com/Sohex/musefs/actions/workflows/ci.yml/badge.svg)](https://github.com/Sohex/musefs/actions/workflows/ci.yml)
```

- [ ] **Step 2: Add a `cargo install` option to the Build section**

In the `## Build` section, after the existing `cargo build --release` block and its "The binary is `musefs`…" line, add:

```markdown
Or install the `musefs` binary directly from the repository:

```bash
cargo install --git https://github.com/Sohex/musefs musefs-cli
```
```

- [ ] **Step 3: Verify**

Run: `grep -n "badge.svg\|cargo install --git" README.md`
Expected: both lines present (badge near the top, install line in Build).

- [ ] **Step 4: Commit**

```bash
git add README.md
git commit -m "docs: add CI badge and cargo install instructions to README"
```

---

### Task 6: `.gitignore` + Serena tracking

**Files:**
- Modify: `.gitignore`
- Track: `.serena/project.yml`, `.serena/.gitignore`

(`docs/agents/` was already deleted — untracked, inert, unreferenced — so it needs no ignore entry. `.claude/settings.local.json` is already excluded by the environment; ignoring `/.claude/` covers the rest. The nested `.serena/.gitignore` already excludes `/cache` and `/project.local.yml`.)

- [ ] **Step 1: Extend `.gitignore`**

Change `.gitignore` from its single `/target` line to:

```gitignore
/target
/.claude/
```

- [ ] **Step 2: Verify the ignore + stage the Serena files**

Run: `git status --porcelain --untracked-files=all`
Expected: `.claude/...` no longer appears (now ignored); the only untracked entries are `.serena/.gitignore` and `.serena/project.yml`. `.serena/cache/` and `.serena/project.local.yml` must NOT appear (excluded by the nested ignore).

Then stage:
```bash
git add .gitignore .serena/.gitignore .serena/project.yml
```

Run: `git status --porcelain` and confirm the staged set is exactly `.gitignore`, `.serena/.gitignore`, `.serena/project.yml` (no cache, no `.claude`, no `project.local.yml`).

- [ ] **Step 3: Commit**

```bash
git commit -m "chore: ignore /.claude, track Serena project config"
```

---

### Task 7: Merge `release-setup` into `main`

**Files:** none (git operation)

- [ ] **Step 1: Final local verification on the branch**

Run: `cargo build --workspace && cargo fmt --all -- --check && cargo clippy --all-targets -- -D warnings && cargo test --workspace`
Expected: all pass. (The `#[ignore]`d FUSE e2e tests don't run here; CI covers them in Part 2.)

- [ ] **Step 2: Merge to main (no fast-forward)**

```bash
git checkout main
git merge --no-ff release-setup -m "$(cat <<'EOF'
Merge branch 'release-setup': v0.2.0 public-release scaffolding

Workspace 0.2.0 + crate metadata, CHANGELOG, CI (check + FUSE e2e), scheduled
cargo-audit, README CI badge, .gitignore/Serena tracking.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```
Expected: merge commit created on `main`.

- [ ] **Step 3: Confirm state**

Run: `git --no-pager log --oneline -3 && git branch --show-current`
Expected: on `main`, with the merge commit at HEAD.

---

## Part 2 — Publish (run directly, with the user; not via subagents)

### Task 8: Pre-push hygiene gate

**Files:** none (inspection)

- [ ] **Step 1: Confirm the working tree is clean and safe to publish**

Run: `git status --porcelain` → Expected: empty (nothing uncommitted/untracked).
Run: `git ls-files | grep -E '^\.claude/|^docs/agents/|^\.serena/cache/|settings\.local|project\.local' || echo "clean"` → Expected: `clean` (none of these are tracked).
Run: `git ls-files | grep -E '\.serena/(project\.yml|\.gitignore)$'` → Expected: both Serena files ARE tracked.

- [ ] **Step 2: Scan for obvious secrets**

Run: `git grep -nIE '(ghp_|github_pat_|-----BEGIN [A-Z]+ PRIVATE KEY-----|AKIA[0-9A-Z]{16})' -- . ':!docs/**' || echo "no secret patterns found"`
Expected: `no secret patterns found`. (Investigate any hit before proceeding — the next step makes the code world-visible.)

---

### Task 9: Create the public repo, wire the remote, push `main`

**Files:** none (`gh`/`git`)

- [ ] **Step 1: Create the empty public repo with a description**

```bash
gh repo create Sohex/musefs --public \
  --description "Read-only passthrough FUSE filesystem presenting a virtually reorganized, re-tagged view of a music library backed by SQLite — without copying or modifying audio bytes."
```
Expected: prints the new repo URL `https://github.com/Sohex/musefs`. (No `--source/--push` — the remote is wired explicitly next.)

- [ ] **Step 2: Add the SSH remote (matches the gh CLI's configured protocol)**

```bash
git remote add origin git@github.com:Sohex/musefs.git
git remote -v
```
Expected: `origin` points at `git@github.com:Sohex/musefs.git` (fetch + push).

- [ ] **Step 3: Push `main` (triggers the first CI run)**

```bash
git push -u origin main
```
Expected: push succeeds; `main` set as upstream.

Run: `gh repo view Sohex/musefs --json visibility,defaultBranchRef -q '.visibility + " " + .defaultBranchRef.name'`
Expected: `PUBLIC main`.

---

### Task 10: Repo topics

**Files:** none (`gh`)

- [ ] **Step 1: Add discovery topics**

```bash
gh repo edit Sohex/musefs \
  --add-topic rust --add-topic fuse --add-topic filesystem \
  --add-topic music --add-topic metadata --add-topic sqlite \
  --add-topic beets --add-topic flac
```
Run: `gh repo view Sohex/musefs --json repositoryTopics -q '.repositoryTopics'`
Expected: lists the eight topics.

---

### Task 11: Tags + GitHub Release

**Files:** none (`git`/`gh`)

- [ ] **Step 1: Push the existing MVP tag, create + push the release tag**

```bash
git push origin v0.1.0
git tag -a v0.2.0 -m "musefs v0.2.0"
git push origin v0.2.0
```
Run: `git ls-remote --tags origin`
Expected: both `refs/tags/v0.1.0` and `refs/tags/v0.2.0` present.

- [ ] **Step 2: Cut the GitHub Release from the changelog section**

Extract the `[0.2.0]` section of the changelog into release notes and create the release:
```bash
awk '/^## \[0.2.0\]/{f=1;next} /^## \[0.1.0\]/{f=0} f' CHANGELOG.md > /tmp/musefs-0.2.0-notes.md
gh release create v0.2.0 --title "musefs v0.2.0" --notes-file /tmp/musefs-0.2.0-notes.md
```
Run: `gh release view v0.2.0 --json name,tagName,isDraft -q '.name + " " + .tagName + " draft=" + (.isDraft|tostring)'`
Expected: `musefs v0.2.0 v0.2.0 draft=false`.

---

### Task 12: Verify CI and report

**Files:** none (`gh`)

- [ ] **Step 1: Watch the CI run triggered by the `main` push**

```bash
gh run list --workflow=ci.yml --limit 1
gh run watch "$(gh run list --workflow=ci.yml --limit 1 --json databaseId -q '.[0].databaseId')" --exit-status
```
Expected: the run completes; `check` and `e2e` both succeed (exit status 0).

- [ ] **Step 2: If `e2e` fails on the runner (FUSE/libfuse), apply the spec fallback**

Diagnose first (read the job log: `gh run view --log-failed`). If the failure is a runner FUSE/libfuse environment issue that can't be made reliable (NOT a real test regression), make the `e2e` job non-blocking rather than block the release: add `continue-on-error: true` to the `e2e` job in `.github/workflows/ci.yml`, keeping `check` as the required signal. Commit on a short branch, push, and note the limitation. Do NOT weaken the tests themselves. If instead `check` fails or `e2e` reveals a real regression, STOP and report — that's a genuine problem, not a release-scaffolding issue.

- [ ] **Step 3: Report**

Report the repo URL, the release URL (`gh release view v0.2.0 --json url -q .url`), and the final CI status for both jobs.

---

## Self-Review (completed during planning)

**Spec coverage:**
- Versioning + workspace.package + crate metadata + cli keywords/categories → Task 1.
- CHANGELOG → Task 2.
- CI (check + e2e, libfuse3) → Task 3.
- Scheduled `audit.yml` (RustSec, path/cron triggers) → Task 4.
- README badge + install line → Task 5.
- `.gitignore` (`/.claude/`) + Serena tracking + docs/agents deletion → Task 6 (deletion already done; Task 6 records/verifies it).
- Merge to main → Task 7.
- Pre-push hygiene gate → Task 8.
- Create public repo + SSH remote + push → Task 9.
- Topics/description → Tasks 9 (description) + 10 (topics).
- Tags v0.1.0 + v0.2.0 → Task 11.
- GitHub Release from changelog → Task 11.
- Verify CI green + e2e fallback → Task 12.

**Placeholder scan:** No TBD/TODO. Workflow files and manifest edits are given in full; the release-notes extraction uses a concrete `awk` range over the changelog headings written in Task 2.

**Consistency:** Repo `Sohex/musefs`, version `0.2.0`, the `https://github.com/Sohex/musefs` repository URL, the workflow filenames (`ci.yml`/`audit.yml`), and the changelog heading format (`## [0.2.0]` / `## [0.1.0]`) used by the Task 11 `awk` extraction all match across tasks. The `e2e` libfuse3 packages match the `fuser` 0.14 dependency.

**Out of scope (per spec):** crates.io publish, prebuilt binaries, release automation, Dependabot, branch protection, CONTRIBUTING/templates.
