# mdBook Docs Site Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Publish a navigable mdBook documentation site to GitHub Pages with the book under `docs/` as the single source of truth, and slim `README.md` to a landing page.

**Architecture:** Long-form content moves into `docs/src/` (book root at `docs/`, build output `docs/book/html`). Conventional locations keep thin shims (README a fuller landing page; ARCHITECTURE/CONTRIBUTING/contrib READMEs shims; BENCHMARKS deleted; SECURITY mirrored via `{{#include}}`). A `docs.yml` workflow builds + link-checks on PRs and builds + deploys to Pages on push to `main`.

**Tech Stack:** mdBook + mdbook-linkcheck, GitHub Actions Pages deployment.

**Spec:** `docs/superpowers/specs/2026-06/2026-06-15-mdbook-docs-site-design.md`

---

## Execution notes (read before starting)

- **Work on a feature branch**, merged via PR at the end. `docs.yml` deploys only on push to `main`, so intermediate states never deploy.
- **Commit isolation.** The pre-commit hook skips the cargo/clippy/test gate only when *every* staged path is under `docs/` or is a `*.md` file. Note that `ARCHITECTURE.md`, `CONTRIBUTING.md`, `CHANGELOG.md`, `CLAUDE.md`, and `contrib/*/README.md` are all `*.md`, so editing them stays docs-only. Only **Task 2** (`.github/workflows/docs.yml`) and **Task 7** (code/scripts) trip the full gate — they are deliberately isolated.
- **Green at every commit.** During migration, root files stay until their content is moved; book pages link cross-area targets either to already-created book pages or to still-existing root files, so `mdbook build` (which runs linkcheck) passes at every commit. Each task names the exact link forms.
- **Validation command** for doc tasks is `mdbook build docs` (this runs the linkcheck backend; a broken intra-book link fails the build). Run it from the repo root. Note linkcheck flags broken *file* links but **not** broken `#fragment` anchors — Task 11 has a manual anchor audit for those.
- **Root-escape link depth (IMPORTANT — corrects the link tables below):** the book source root is `docs/src/`. A page nested one directory deep (`docs/src/architecture/foo.md`, `docs/src/guide/foo.md`, `docs/src/integrations/foo.md`, `docs/src/contributing/foo.md`) reaches the REPO root with `../../../` (three levels), NOT `../../`. A top-level page (`docs/src/changelog.md`, `benchmarks.md`, `security.md`, `introduction.md`) reaches repo root with `../../` (two levels). Several link tables below were written with `../../` for one-level-deep pages — use `../../../` there. When repointing existing links, **grep the file for the target filename to see its actual current `../` depth** rather than trusting the table's old-string; the moved contrib READMEs, for example, genuinely contain `../../CONTRIBUTING.md` (2 levels, correct for them) while architecture pages contain `../../../CONTRIBUTING.md` (3 levels). Verified at Task 3.
- **Page-before-entry:** never append a `SUMMARY.md` entry for a page that doesn't exist yet — `mdbook build` silently creates an empty stub for it, which a broad `git add docs/src/...` would then commit. Each task creates its pages before (or in the same step as) the SUMMARY entry; run `git status` before each commit to confirm no unexpected empty files were generated.
- **Local tooling install (once):**
  ```bash
  cargo install mdbook --version 0.4.40 --locked
  cargo install mdbook-linkcheck --version 0.7.7 --locked
  ```
  (If 0.7.7 is incompatible with the installed mdBook, pick the latest mdbook-linkcheck that lists 0.4.x support and update the versions in `book.toml`'s CI pin and Task 2.)
- **Prerequisite for first deploy (manual, repo admin):** GitHub repo Settings → Pages → Source = **GitHub Actions**. The deploy job fails until this is set. Not a code step.

---

## Final book layout (reference)

```
docs/
  book.toml
  src/
    SUMMARY.md
    introduction.md
    guide/{quick-start,installation,containers,scanning,mounting,tuning,configuration,faq}.md
    formats/{overview,flac,mp3,m4a,ogg,wav}.md
    integrations/{overview,beets,picard,lidarr,systemd,python-musefs}.md
    architecture/{overview,serving,store,tree-scanning}.md
    contributing/{setup,testing,conventions,plugins,releasing}.md
    benchmarks.md
    changelog.md
    security.md
  book/            # build output, gitignored
  superpowers/     # untouched, excluded from the book
```

**Final `docs/src/SUMMARY.md`** (built up incrementally across tasks; this is the end state):

```markdown
# Summary

[Introduction](introduction.md)

# User Guide

- [Quick start](guide/quick-start.md)
- [Installation](guide/installation.md)
- [Running in containers](guide/containers.md)
- [Scanning](guide/scanning.md)
- [Mounting & path templates](guide/mounting.md)
- [Tuning & metrics](guide/tuning.md)
- [Ownership, permissions & config](guide/configuration.md)
- [FAQ](guide/faq.md)

# Formats

- [Overview](formats/overview.md)
- [FLAC](formats/flac.md)
- [MP3](formats/mp3.md)
- [M4A](formats/m4a.md)
- [Ogg](formats/ogg.md)
- [WAV](formats/wav.md)

# Integrations

- [Overview](integrations/overview.md)
- [beets](integrations/beets.md)
- [Picard](integrations/picard.md)
- [Lidarr](integrations/lidarr.md)
- [systemd](integrations/systemd.md)
- [python-musefs](integrations/python-musefs.md)

# Internals

- [Architecture overview](architecture/overview.md)
- [The serving model](architecture/serving.md)
- [The SQLite store](architecture/store.md)
- [Freshness, tree & scanning](architecture/tree-scanning.md)

# Contributing

- [Getting set up](contributing/setup.md)
- [Test tiers](contributing/testing.md)
- [Conventions & adding a format](contributing/conventions.md)
- [Python plugins](contributing/plugins.md)
- [Releasing](contributing/releasing.md)

# Reference

- [Benchmarks](benchmarks.md)
- [Changelog](changelog.md)
- [Security](security.md)
```

**Architecture page split (source `##` section → page):**

| Page | Source `ARCHITECTURE.md` sections (preserved anchors) |
| ---- | ----------------------------------------------------- |
| `architecture/overview.md` | Design overview (`#design-overview`), Crate layout (`#crate-layout`) |
| `architecture/serving.md` | The segment model (`#the-segment-model`), Backing read-ahead (`#backing-read-ahead`), Mount modes (`#mount-modes`), Synthetic telemetry namespace (`#synthetic-telemetry-namespace`) |
| `architecture/store.md` (H1 `# The store & external-writer contract`, to avoid a duplicate-title stack) | The SQLite store (`#the-sqlite-store`), The external-writer contract (`#the-external-writer-contract`) |
| `architecture/tree-scanning.md` | Freshness: two version counters (`#freshness-two-version-counters`), Virtual tree (`#virtual-tree`), Scanning (`#scanning`), The contrib ecosystem (`#the-contrib-ecosystem`) |

**Contributing page split:**

| Page | Source `CONTRIBUTING.md` sections (preserved anchors) |
| ---- | ----------------------------------------------------- |
| `contributing/setup.md` | Getting set up, Build & test |
| `contributing/testing.md` | Test tiers beyond `cargo test` (`#coverage-guided-fuzzing` lives here) |
| `contributing/conventions.md` | Code conventions, Adding a format |
| `contributing/plugins.md` | Python plugins (contrib) (`#python-plugins-contrib`) |
| `contributing/releasing.md` | Releasing the Python packages, Releasing the Rust crates and binaries, PRs & commits |

**README user-guide split:**

| Page | Source `README.md` sections |
| ---- | --------------------------- |
| `introduction.md` | title pitch, What it's for, Status |
| `guide/quick-start.md` | Quick start |
| `guide/installation.md` | Installing (Prebuilt binaries, Building from source), Platform support |
| `guide/containers.md` | Container images + all its subsections |
| `guide/scanning.md` | Scan, Content checksums and move re-identification |
| `guide/mounting.md` | Mount, Path templates |
| `guide/tuning.md` | Tuning, Metrics (`#metrics`) |
| `guide/configuration.md` | Ownership and permissions (`#ownership-and-permissions`), Configuring with environment variables, Running as a systemd user service |
| `guide/faq.md` | FAQ |
| `formats/overview.md` | Supported formats |

---

## Task 1: Scaffold the book

**Files:**
- Create: `docs/book.toml`
- Create: `docs/src/SUMMARY.md`
- Create: `docs/src/introduction.md`
- Modify: `.gitignore`

- [ ] **Step 1: Create `docs/book.toml`**

```toml
[book]
title = "musefs"
authors = ["Conor Futro"]
description = "A read-only passthrough FUSE filesystem presenting a re-tagged, reorganized view of a music library."
src = "src"
language = "en"

[output.html]
site-url = "/musefs/"
git-repository-url = "https://github.com/Sohex/musefs"
edit-url-template = "https://github.com/Sohex/musefs/edit/main/docs/src/{path}"
default-theme = "navy"

[output.linkcheck]
follow-web-links = false
warning-policy = "error"
# LOAD-BEARING: the migration keeps every commit green by linking cross-area
# targets at still-existing root files via ../../FILE. mdbook-linkcheck forbids
# links that escape the book root (docs/) unless this is set. It also permits
# the final-state ../../../contrib/CHANGELOG.md link. Do not remove.
traverse-parent-directories = true
```

> **Verified behaviors** (mdbook 0.4.40 + mdbook-linkcheck 0.7.7): `mdbook build docs` runs the linkcheck backend and emits HTML to `docs/book/html`; without `traverse-parent-directories = true`, any `../../…` link fails with *"Linking outside of the root directory is forbidden"*; linkcheck **skips** `https://` links (because `follow-web-links = false`) and **does not validate `#fragment` anchors** (so a green build does not prove anchors resolve — see Task 11's anchor audit).

- [ ] **Step 2: Create `docs/src/SUMMARY.md`** (intro-only for now; later tasks append)

```markdown
# Summary

[Introduction](introduction.md)
```

- [ ] **Step 3: Create `docs/src/introduction.md`**

Assemble from existing `README.md` content (so the prose is the validated original):
1. Copy the title pitch paragraph(s) from `README.md` (the lines under `# musefs` down to just before `## Quick start`), minus the CI badge line.
2. Append the **What it's for** section body (README `## What it's for`).
3. Append the **Status** section body (README `## Status`), under a `## Status` heading.
Apply these link edits to the pasted text:

| Old | New |
| --- | --- |
| `(contrib/beets/README.md)` | `(integrations/beets.md)` |
| `(contrib/picard/README.md)` | `(integrations/picard.md)` |
| `(contrib/lidarr/README.md)` | `(integrations/lidarr.md)` |
| `(contrib/)` | `(integrations/overview.md)` |
| `(CHANGELOG.md)` | `(changelog.md)` |
| `(ARCHITECTURE.md)` | `(architecture/overview.md)` |
| `(CONTRIBUTING.md)` | `(contributing/setup.md)` |

Top the file with an `# Introduction` H1.

- [ ] **Step 4: Add build output to `.gitignore`**

Append to `.gitignore`:
```
/docs/book/
```

- [ ] **Step 5: Build to verify**

Run: `mdbook build docs`
Expected: `... mdbook::book] Book building has started` then success; `docs/book/html/index.html` exists, no link errors.

- [ ] **Step 6: Commit** (docs-only — cargo gate skips)

```bash
git add docs/book.toml docs/src/SUMMARY.md docs/src/introduction.md .gitignore
git commit -m "docs: scaffold mdBook (book.toml, SUMMARY, introduction)"
```

---

## Task 2: CI workflow (build + link-check on PRs, deploy on main)

**Files:**
- Create: `.github/workflows/docs.yml`

> This commit touches a non-doc path, so the **full cargo/clippy/test gate runs** in pre-commit. Expect it to take a while and pass.

- [ ] **Step 1: Resolve action commit SHAs** (repo convention: pin to commit SHA with a `# vX.Y` comment, like `ci.yml`)

Run and note each `.sha`:
```bash
gh api repos/actions/checkout/commits/v4 --jq .sha
gh api repos/actions/configure-pages/commits/v5 --jq .sha
gh api repos/actions/upload-pages-artifact/commits/v3 --jq .sha
gh api repos/actions/deploy-pages/commits/v4 --jq .sha
```

- [ ] **Step 2: Create `.github/workflows/docs.yml`** (substitute the four `<sha>` values)

```yaml
name: docs

on:
  push:
    branches: [main]
    paths:
      - 'docs/**'
      - '.github/workflows/docs.yml'
  pull_request:
    paths:
      - 'docs/**'
      - '.github/workflows/docs.yml'

permissions:
  contents: read

concurrency:
  group: pages
  cancel-in-progress: false

env:
  MDBOOK_VERSION: '0.4.40'
  MDBOOK_LINKCHECK_VERSION: '0.7.7'

jobs:
  build:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@<sha>  # v4 — use the repo-standard checkout SHA from ci.yml
        with:
          persist-credentials: false
      - name: Install mdbook + mdbook-linkcheck
        run: |
          mkdir -p "$HOME/.local/bin"
          curl -fsSL "https://github.com/rust-lang/mdBook/releases/download/v${MDBOOK_VERSION}/mdbook-v${MDBOOK_VERSION}-x86_64-unknown-linux-gnu.tar.gz" | tar -xz -C "$HOME/.local/bin"
          curl -fsSL "https://github.com/Michael-F-Bryan/mdbook-linkcheck/releases/download/v${MDBOOK_LINKCHECK_VERSION}/mdbook-linkcheck.x86_64-unknown-linux-gnu.zip" -o /tmp/linkcheck.zip
          unzip -o -d "$HOME/.local/bin" /tmp/linkcheck.zip
          chmod +x "$HOME/.local/bin/mdbook" "$HOME/.local/bin/mdbook-linkcheck"
          echo "$HOME/.local/bin" >> "$GITHUB_PATH"
      - name: Build book (runs linkcheck backend)
        run: mdbook build docs
      - name: Upload Pages artifact
        if: github.ref == 'refs/heads/main'
        uses: actions/upload-pages-artifact@<sha>  # v3
        with:
          path: docs/book/html

  deploy:
    if: github.ref == 'refs/heads/main'
    needs: build
    runs-on: ubuntu-latest
    permissions:
      pages: write
      id-token: write
      contents: read
    environment:
      name: github-pages
      url: ${{ steps.deployment.outputs.page_url }}
    steps:
      - id: configure
        uses: actions/configure-pages@<sha>  # v5
      - id: deployment
        uses: actions/deploy-pages@<sha>  # v4
```

- [ ] **Step 3: Lint the workflow YAML**

Run: `yamllint -c .yamllint .github/workflows/docs.yml`
Expected: no output (clean). If `yamllint` is absent, skip — the pre-commit hook runs it.

- [ ] **Step 4: Commit** (full gate runs — let it pass)

```bash
git add .github/workflows/docs.yml
git commit -m "ci: add mdBook build/link-check + Pages deploy workflow"
```

---

## Task 3: Migrate Architecture

**Files:**
- Create: `docs/src/architecture/{overview,serving,store,tree-scanning}.md`
- Modify: `docs/src/SUMMARY.md`
- Replace with shim: `ARCHITECTURE.md`
- Modify: `CLAUDE.md`

- [ ] **Step 1: Create the four architecture pages** from `ARCHITECTURE.md` per the split table above. For each page, paste its source `##` sections verbatim, then give the page an `# <Title>` H1 (`Architecture overview`, `The serving model`, `The SQLite store`, `Freshness, tree & scanning`).

- [ ] **Step 2: Fix outbound links in the pasted architecture content.** None of these targets are in the book yet, so point **every** one at the still-existing root file (correct depth from `docs/src/architecture/`). Linkcheck passes because all root files still exist. Tasks 4/5/6 repoint the formats/contributing/benchmarks links to book pages once those pages land.

| Old (in ARCHITECTURE.md) | New (root form, in book pages) |
| --- | --- |
| `(README.md)` | `(../../README.md)` |
| `(CONTRIBUTING.md)` (all bare occurrences) | `(../../CONTRIBUTING.md)` |
| `(BENCHMARKS.md)` | `(../../BENCHMARKS.md)` |
| `(BENCHMARKS.md#backing-read-ahead-255)` | `(../../BENCHMARKS.md#backing-read-ahead-255)` |
| `(docs/FLAC.md)` | `(../../docs/FLAC.md)` |
| `(docs/MP3.md)` | `(../../docs/MP3.md)` |
| `(docs/M4A.md)` | `(../../docs/M4A.md)` |
| `(docs/OGG.md)` | `(../../docs/OGG.md)` |
| `(docs/WAV.md)` | `(../../docs/WAV.md)` |
| `(docs/)` | `(../../docs/)` |
| `(contrib/beets/README.md)` | `(../../contrib/beets/README.md)` |
| `(contrib/picard/README.md)` | `(../../contrib/picard/README.md)` |
| `(contrib/lidarr/README.md)` | `(../../contrib/lidarr/README.md)` |

(The `contrib/*` links get repointed to integration pages in Task 9 Step 6, and `docs/<FMT>.md`/`CONTRIBUTING.md`/`BENCHMARKS.md` in Tasks 4/5/6.)

- [ ] **Step 3: Append architecture entries to `docs/src/SUMMARY.md`**

```markdown

# Internals

- [Architecture overview](architecture/overview.md)
- [The serving model](architecture/serving.md)
- [The SQLite store](architecture/store.md)
- [Freshness, tree & scanning](architecture/tree-scanning.md)
```

- [ ] **Step 4: Replace `ARCHITECTURE.md` with a shim**

```markdown
# Architecture

The architecture reference now lives in the musefs documentation site:

- **Published:** <https://sohex.github.io/musefs/architecture/overview.html>
- **In-repo source:** [`docs/src/architecture/`](docs/src/architecture/)

Start with the [architecture overview](docs/src/architecture/overview.md),
the [serving model](docs/src/architecture/serving.md), the
[SQLite store & external-writer contract](docs/src/architecture/store.md), and
[freshness, tree & scanning](docs/src/architecture/tree-scanning.md).
```

- [ ] **Step 5: Repoint `CLAUDE.md` architecture anchors** to in-repo book sources

| Old | New |
| --- | --- |
| `[ARCHITECTURE.md](ARCHITECTURE.md#the-segment-model)` | `[architecture/serving.md](docs/src/architecture/serving.md#the-segment-model)` |
| `[ARCHITECTURE.md](ARCHITECTURE.md)` (row in the doc table) | `[Architecture](docs/src/architecture/overview.md)` |
| `[ARCHITECTURE.md](ARCHITECTURE.md#the-external-writer-contract)` | `[architecture/store.md](docs/src/architecture/store.md#the-external-writer-contract)` |

- [ ] **Step 6: Build to verify**

Run: `mdbook build docs`
Expected: success, no broken links (architecture pages' cross-links point to existing root files / not-yet pages via root forms).

- [ ] **Step 7: Commit** (docs-only)

```bash
git add docs/src/architecture docs/src/SUMMARY.md ARCHITECTURE.md CLAUDE.md
git commit -m "docs: migrate ARCHITECTURE.md into the book; leave a shim"
```

---

## Task 4: Migrate Formats

**Files:**
- Create: `docs/src/formats/{overview,flac,mp3,m4a,ogg,wav}.md`
- Modify: `docs/src/SUMMARY.md`
- Modify: `docs/src/architecture/{serving,store,tree-scanning}.md` (repoint to new format pages)
- Delete: `docs/{FLAC,MP3,M4A,OGG,WAV}.md`

- [ ] **Step 1: Move each format doc.** Copy `docs/FLAC.md` → `docs/src/formats/flac.md` (and MP3/M4A/OGG/WAV likewise). In each, repoint the architecture link:

| Old | New |
| --- | --- |
| `(../ARCHITECTURE.md#the-segment-model)` | `(../architecture/serving.md#the-segment-model)` |
| `(../ARCHITECTURE.md#backing-read-ahead)` | `(../architecture/serving.md#backing-read-ahead)` |

- [ ] **Step 2: Create `docs/src/formats/overview.md`**

```markdown
# Supported formats

musefs synthesizes fresh metadata for each supported container while serving
the original audio bytes verbatim. Each format has its own page for the exact
synthesis behavior and lossy edges.

| Format | Extensions | What is synthesized |
| ------ | ---------- | ------------------- |
| [FLAC](flac.md) | `.flac` | Regenerates the metadata blocks; preserves `STREAMINFO`/`SEEKTABLE` bit-exact |
| [MP3](mp3.md) | `.mp3` | Regenerates the ID3v2.4 tag; audio frames (incl. Xing/LAME) untouched |
| [M4A](m4a.md) | `.m4a`, `.m4b` | Rebuilds the `moov` atom, patching chunk offsets; `mdat` served verbatim |
| [Ogg](ogg.md) | `.ogg`, `.oga`, `.opus` | Regenerates header pages; audio pages verbatim, only page seq/CRC patched in place |
| [WAV](wav.md) | `.wav` | Regenerates the RIFF front (`LIST`/`INFO` + embedded ID3v2); `data` payload verbatim |
```

- [ ] **Step 3: Append formats to `docs/src/SUMMARY.md`** (place this block immediately after the Introduction line / before `# Internals`)

```markdown

# Formats

- [Overview](formats/overview.md)
- [FLAC](formats/flac.md)
- [MP3](formats/mp3.md)
- [M4A](formats/m4a.md)
- [Ogg](formats/ogg.md)
- [WAV](formats/wav.md)
```

- [ ] **Step 4: Repoint architecture pages' format links** (created with root forms in Task 3) — in `docs/src/architecture/serving.md`, `store.md`, `tree-scanning.md` wherever they appear:

| Old | New |
| --- | --- |
| `(../../docs/FLAC.md)` | `(../formats/flac.md)` |
| `(../../docs/MP3.md)` | `(../formats/mp3.md)` |
| `(../../docs/M4A.md)` | `(../formats/m4a.md)` |
| `(../../docs/OGG.md)` | `(../formats/ogg.md)` |
| `(../../docs/WAV.md)` | `(../formats/wav.md)` |
| `(../formats/overview.md)` already-correct if used | — |

(The architecture `docs/` links were set to `(../../docs/<F>.md)` in Task 3 Step 2's correction; this step flips them to book pages now that the pages exist.)

- [ ] **Step 5: Delete the old format docs**

```bash
git rm docs/FLAC.md docs/MP3.md docs/M4A.md docs/OGG.md docs/WAV.md
```

- [ ] **Step 6: Build to verify**

Run: `mdbook build docs`
Expected: success; no link errors.

- [ ] **Step 7: Commit** (docs-only)

```bash
git add docs/src/formats docs/src/SUMMARY.md docs/src/architecture
git commit -m "docs: migrate per-format docs into the book"
```

---

## Task 5: Migrate Contributing

**Files:**
- Create: `docs/src/contributing/{setup,testing,conventions,plugins,releasing}.md`
- Modify: `docs/src/SUMMARY.md`
- Replace with shim: `CONTRIBUTING.md`
- Modify: `docs/src/architecture/*` (repoint CONTRIBUTING links), `CLAUDE.md`

- [ ] **Step 1: Create the five contributing pages** from `CONTRIBUTING.md` per the split table. Paste source `##` sections verbatim; give each page an `# <Title>` H1.

- [ ] **Step 2: Fix outbound links in the pasted contributing content:**

| Old | New |
| --- | --- |
| `(ARCHITECTURE.md)` | `(../architecture/overview.md)` |
| `(ARCHITECTURE.md#crate-layout)` | `(../architecture/overview.md#crate-layout)` |
| `(ARCHITECTURE.md#the-contrib-ecosystem)` | `(../architecture/tree-scanning.md#the-contrib-ecosystem)` |
| `(docs/)` | `(../formats/overview.md)` |
| `(BENCHMARKS.md)` | `(../../BENCHMARKS.md)` (repointed to the book in Task 6) |

- [ ] **Step 3: Append contributing entries to `docs/src/SUMMARY.md`** (after the Integrations placeholder position / before `# Reference`; for now place after Internals)

```markdown

# Contributing

- [Getting set up](contributing/setup.md)
- [Test tiers](contributing/testing.md)
- [Conventions & adding a format](contributing/conventions.md)
- [Python plugins](contributing/plugins.md)
- [Releasing](contributing/releasing.md)
```

- [ ] **Step 4: Replace `CONTRIBUTING.md` with a shim**

```markdown
# Contributing

The contributor guide now lives in the musefs documentation site:

- **Published:** <https://sohex.github.io/musefs/contributing/setup.html>
- **In-repo source:** [`docs/src/contributing/`](docs/src/contributing/)

Jump to [getting set up](docs/src/contributing/setup.md),
[test tiers](docs/src/contributing/testing.md),
[conventions & adding a format](docs/src/contributing/conventions.md),
[Python plugins](docs/src/contributing/plugins.md), or
[releasing](docs/src/contributing/releasing.md).
```

- [ ] **Step 5: Repoint architecture pages' CONTRIBUTING links** (set to `(../../CONTRIBUTING.md)` in Task 3). In `docs/src/architecture/serving.md`, `store.md`, `tree-scanning.md`:

| Old | New |
| --- | --- |
| `(../../CONTRIBUTING.md)` | `(../contributing/setup.md)` |

- [ ] **Step 6: Repoint `CLAUDE.md` contributing anchors:**

| Old | New |
| --- | --- |
| `[CONTRIBUTING.md](CONTRIBUTING.md)` (doc table row) | `[Contributing](docs/src/contributing/setup.md)` |
| `[CONTRIBUTING.md](CONTRIBUTING.md#coverage-guided-fuzzing)` | `[contributing/testing.md](docs/src/contributing/testing.md#coverage-guided-fuzzing)` |
| `[CONTRIBUTING.md](CONTRIBUTING.md#python-plugins-contrib)` (both occurrences) | `[contributing/plugins.md](docs/src/contributing/plugins.md#python-plugins-contrib)` |

- [ ] **Step 7: Build to verify**

Run: `mdbook build docs`
Expected: success; no link errors.

- [ ] **Step 8: Commit** (docs-only)

```bash
git add docs/src/contributing docs/src/SUMMARY.md CONTRIBUTING.md docs/src/architecture CLAUDE.md
git commit -m "docs: migrate CONTRIBUTING.md into the book; leave a shim"
```

---

## Task 6: Migrate Benchmarks (delete, no shim)

**Files:**
- Create: `docs/src/benchmarks.md`
- Modify: `docs/src/SUMMARY.md`
- Modify: `docs/src/architecture/serving.md`, `docs/src/contributing/*` (repoint BENCHMARKS links)
- Delete: `BENCHMARKS.md`

- [ ] **Step 1: Move content**

```bash
git mv BENCHMARKS.md docs/src/benchmarks.md
```
(The file already starts with `# Benchmarks`, so no heading edit is needed.)

- [ ] **Step 2: Append to `docs/src/SUMMARY.md`** (under a `# Reference` section at the end)

```markdown

# Reference

- [Benchmarks](benchmarks.md)
```

- [ ] **Step 3: Repoint book pages that referenced `../../BENCHMARKS.md`** to the in-book page. In `docs/src/architecture/serving.md` and any `docs/src/contributing/*` page:

| Old | New |
| --- | --- |
| `(../../BENCHMARKS.md)` | `(../benchmarks.md)` |
| `(../../BENCHMARKS.md#backing-read-ahead-255)` | `(../benchmarks.md#backing-read-ahead-255)` |

(Note: architecture pages are one level under `docs/src/`, so `../benchmarks.md` is correct from `architecture/` and `contributing/`.)

- [ ] **Step 4: Build to verify**

Run: `mdbook build docs`
Expected: success; no link errors.

- [ ] **Step 5: Commit** (docs-only)

```bash
git add docs/src/benchmarks.md docs/src/SUMMARY.md docs/src/architecture docs/src/contributing
git commit -m "docs: migrate BENCHMARKS.md into the book (no shim)"
```

---

## Task 7: Repoint code/script references to BENCHMARKS & CONTRIBUTING

> **This commit touches Rust/shell/Python — the full cargo/clippy/test + shellcheck gate runs.** Keep it isolated.

**Files:**
- Modify: `musefs-cli/src/lib.rs:123`
- Modify: `musefs-db/src/schema.rs:212` (then regenerate Python mirrors)
- Modify: `benches/storage_tunables_bench.sh:4`
- Modify: `scripts/freebsd-vm/provision.sh:5`, `scripts/freebsd-vm/run-local.sh:12`

- [ ] **Step 1: Edit the `musefs-cli` doc comment** (`musefs-cli/src/lib.rs:123`)

Change `See BENCHMARKS.md.` → `See the benchmarks chapter of the docs (https://sohex.github.io/musefs/benchmarks.html).`

- [ ] **Step 2: Edit the bench script comment** (`benches/storage_tunables_bench.sh:4`)

Change the `BENCHMARKS.md (#storage-tunables)` reference to `the benchmarks chapter of the docs (https://sohex.github.io/musefs/benchmarks.html#storage-tunables)`.

- [ ] **Step 3: Edit the FreeBSD scripts** — replace `CONTRIBUTING.md` with the published contributor-guide URL.

In `scripts/freebsd-vm/provision.sh:5` and `scripts/freebsd-vm/run-local.sh:12`, change `CONTRIBUTING.md` (the "FreeBSD e2e section") references to `https://sohex.github.io/musefs/contributing/testing.html` (the test-tiers page, which holds the FreeBSD e2e content).

- [ ] **Step 4: Edit the schema SQL comment** (`musefs-db/src/schema.rs:212`)

The comment reads `-- BENCHMARKS.md). Hash function is now fixed, so the CHECK is added here.` Change the `BENCHMARKS.md` token to a path-free phrase so the mirrored comment never dangles, e.g. `-- the benchmarks docs). Hash function is now fixed, so the CHECK is added here.`

- [ ] **Step 5: Regenerate and re-vendor the Python schema mirrors** (required — the mirrors are generated, not hand-edited)

Run:
```bash
MUSEFS_REGEN_SCHEMA_PY=1 cargo test -p musefs-db schema_py
```
Then re-vendor the regenerated schema into the two consumers (copy per the existing vendor flow in `CONTRIBUTING`/`docs/src/contributing/plugins.md`):
- `contrib/python-musefs/src/musefs_common/schema.py`
- `contrib/picard/musefs/_common/schema.py`

- [ ] **Step 6: Verify the schema drift test passes**

Run: `cargo test -p musefs-db schema_py`
Expected: PASS (no drift). Also confirm no `user_version` change occurred (the edit is a comment): `git diff musefs-db/src/schema.rs` shows only the comment line changed.

- [ ] **Step 7: Sanity-build the workspace**

Run: `cargo build` and `cargo clippy --all-targets`
Expected: clean.

- [ ] **Step 8: Commit** (full gate runs — let it pass)

```bash
git add musefs-cli/src/lib.rs musefs-db/src/schema.rs benches/storage_tunables_bench.sh \
  scripts/freebsd-vm/provision.sh scripts/freebsd-vm/run-local.sh \
  contrib/python-musefs/src/musefs_common/schema.py contrib/picard/musefs/_common/schema.py
git commit -m "refs: repoint BENCHMARKS/CONTRIBUTING references to the docs site"
```

---

## Task 8: Migrate Changelog (full → book, curated subset at root)

**Files:**
- Create: `docs/src/changelog.md`
- Modify: `docs/src/SUMMARY.md`
- Rewrite: `CHANGELOG.md` (curated user-facing subset)

- [ ] **Step 1: Copy the full changelog into the book**

```bash
cp CHANGELOG.md docs/src/changelog.md
```
Repoint the README/contrib links in the **book** copy:

| Old | New |
| --- | --- |
| `(README.md#metrics)` | `(guide/tuning.md#metrics)` |
| `(README.md#ownership-and-permissions)` | `(guide/configuration.md#ownership-and-permissions)` |
| `(README.md#supported-formats)` | `(formats/overview.md)` |
| `(contrib/systemd/README.md#hardening)` | `(integrations/systemd.md#hardening)` |
| `(contrib/CHANGELOG.md)` | `(integrations/overview.md#contrib-changelog)` |

(The targets land in Tasks 9–10; to keep this commit green, temporarily use the published absolute URLs instead — e.g. `(https://sohex.github.io/musefs/guide/tuning.html#metrics)` — which linkcheck skips because `follow-web-links = false`. Convert these back to relative book links in Task 11's reconciliation once all pages exist.)

- [ ] **Step 2: Append to `docs/src/SUMMARY.md`** (in the `# Reference` block, after Benchmarks)

```markdown
- [Changelog](changelog.md)
```

- [ ] **Step 3: Rewrite root `CHANGELOG.md` as a curated user-facing subset**

Keep the `# Changelog` header and the Keep-a-Changelog/SemVer preamble. Replace the detailed entry history with a short, user-facing highlights list (notable user-visible features/fixes per release — not internal refactors), then a pointer:

```markdown
> The full, detailed changelog (including internal changes) lives in the
> documentation site: <https://sohex.github.io/musefs/changelog.html>.
> For the contrib plugin packages, see [`contrib/CHANGELOG.md`](contrib/CHANGELOG.md).
```

Preserve the existing `contrib/CHANGELOG.md` cross-link. The curated highlights are authored by selecting the user-facing bullet points already present in the full changelog — do not invent entries.

- [ ] **Step 4: Build to verify**

Run: `mdbook build docs`
Expected: success.

- [ ] **Step 5: Commit** (docs-only)

```bash
git add docs/src/changelog.md docs/src/SUMMARY.md CHANGELOG.md
git commit -m "docs: full changelog to the book; curate a user-facing root CHANGELOG"
```

---

## Task 9: Migrate Integrations (contrib READMEs)

**Files:**
- Create: `docs/src/integrations/{overview,beets,picard,lidarr,systemd,python-musefs}.md`
- Modify: `docs/src/SUMMARY.md`
- Replace with shims: `contrib/{beets,picard,lidarr,systemd,python-musefs}/README.md`

- [ ] **Step 1: Move each contrib README** into the book:
  - `contrib/beets/README.md` → `docs/src/integrations/beets.md`
  - `contrib/picard/README.md` → `docs/src/integrations/picard.md`
  - `contrib/lidarr/README.md` → `docs/src/integrations/lidarr.md`
  - `contrib/systemd/README.md` → `docs/src/integrations/systemd.md`
  - `contrib/python-musefs/README.md` → `docs/src/integrations/python-musefs.md`

- [ ] **Step 2: Fix outbound links in the pasted integration pages** (depth is now `docs/src/integrations/`):

| Old | New |
| --- | --- |
| `(../../README.md)` | `(../introduction.md)` |
| `(../../ARCHITECTURE.md)` | `(../architecture/overview.md)` |
| `(../../ARCHITECTURE.md#the-external-writer-contract)` | `(../architecture/store.md#the-external-writer-contract)` |
| `(../../CONTRIBUTING.md#python-plugins-contrib)` | `(../contributing/plugins.md#python-plugins-contrib)` |
| `(../python-musefs/README.md)` | `(python-musefs.md)` |
| `(../beets/README.md)` | `(beets.md)` |
| `(../picard/README.md)` | `(picard.md)` |
| `(../lidarr/README.md)` | `(lidarr.md)` |

(The five contrib READMEs link only to other READMEs / `../../README.md` / `../../ARCHITECTURE.md`, all covered by the table above — none link to `musefs.conf.example` or test fixtures, so there are no in-dir file links to fix here. The one book→`contrib/` file link is the `contrib/CHANGELOG.md` reference created in Step 3, and the guide's `musefs.conf.example` link in Task 10; both stay repo-relative and resolve because of `traverse-parent-directories = true`.)

- [ ] **Step 3: Create `docs/src/integrations/overview.md`**

```markdown
# Integrations

External tools write tags and art into the musefs SQLite store; a live mount
reflects their edits without copying audio. Each integration has its own page.

- [beets](beets.md) — the `musefs` beets plugin
- [Picard](picard.md) — the MusicBrainz Picard plugin
- [Lidarr](lidarr.md) — Custom Script integration
- [systemd](systemd.md) — running musefs as a user/system service
- [python-musefs](python-musefs.md) — the shared store-contract library behind the plugins

<a id="contrib-changelog"></a>
The plugin packages have their own changelog at
[`contrib/CHANGELOG.md`](../../../contrib/CHANGELOG.md).
```

- [ ] **Step 4: Append to `docs/src/SUMMARY.md`** (place after the Formats block / before `# Internals`)

```markdown

# Integrations

- [Overview](integrations/overview.md)
- [beets](integrations/beets.md)
- [Picard](integrations/picard.md)
- [Lidarr](integrations/lidarr.md)
- [systemd](integrations/systemd.md)
- [python-musefs](integrations/python-musefs.md)
```

- [ ] **Step 5: Replace each contrib README with a shim.** Example for `contrib/beets/README.md`:

```markdown
# musefs beets plugin

The full guide now lives in the musefs documentation site:

- **Published:** <https://sohex.github.io/musefs/integrations/beets.html>
- **In-repo source:** [`docs/src/integrations/beets.md`](../../docs/src/integrations/beets.md)
```

Repeat for picard, lidarr, systemd, python-musefs (swap the title, URL slug, and source path).

- [ ] **Step 6: Repoint architecture pages' contrib links** (set to `(../../contrib/<plugin>/README.md)` in Task 3) to the new integration pages. In `docs/src/architecture/store.md` and `docs/src/architecture/tree-scanning.md`:

| Old | New |
| --- | --- |
| `(../../contrib/beets/README.md)` | `(../integrations/beets.md)` |
| `(../../contrib/picard/README.md)` | `(../integrations/picard.md)` |
| `(../../contrib/lidarr/README.md)` | `(../integrations/lidarr.md)` |

- [ ] **Step 7: Build to verify**

Run: `mdbook build docs`
Expected: success; no link errors.

- [ ] **Step 8: Commit** (docs-only)

```bash
git add docs/src/integrations docs/src/architecture docs/src/SUMMARY.md \
  contrib/beets/README.md contrib/picard/README.md contrib/lidarr/README.md \
  contrib/systemd/README.md contrib/python-musefs/README.md
git commit -m "docs: migrate contrib READMEs into the Integrations section; leave shims"
```

---

## Task 10: Migrate the User Guide (README sections)

**Files:**
- Create: `docs/src/guide/{quick-start,installation,containers,scanning,mounting,tuning,configuration,faq}.md`
- Modify: `docs/src/SUMMARY.md`

- [ ] **Step 1: Carve the README user-guide sections** into the eight guide pages per the README split table. Paste each section's body verbatim; give each page an `# <Title>` H1. Do **not** delete the README yet (Task 11 slims it).

- [ ] **Step 2: Fix outbound links in the pasted guide content** (depth `docs/src/guide/`):

| Old | New |
| --- | --- |
| `(contrib/beets/README.md)` | `(../integrations/beets.md)` |
| `(contrib/picard/README.md)` | `(../integrations/picard.md)` |
| `(contrib/lidarr/README.md)` | `(../integrations/lidarr.md)` |
| `(contrib/systemd/README.md)` | `(../integrations/systemd.md)` |
| `(contrib/systemd/)` | `(../integrations/systemd.md)` |
| `(contrib/systemd/musefs.conf.example)` | `(../../../contrib/systemd/musefs.conf.example)` |
| `(BENCHMARKS.md#storage-tunables)` | `(../benchmarks.md#storage-tunables)` |
| `(ARCHITECTURE.md#the-sqlite-store)` | `(../architecture/store.md#the-sqlite-store)` |
| `(docs/FLAC.md)` … `(docs/WAV.md)` | `(../formats/flac.md)` … `(../formats/wav.md)` |
| Intra-README anchor links between moved sections (e.g. a link to `#metrics`) | the corresponding `../<page>.md#anchor` |

- [ ] **Step 3: Append to `docs/src/SUMMARY.md`** (the `# User Guide` block, placed right after the `[Introduction]` line)

```markdown

# User Guide

- [Quick start](guide/quick-start.md)
- [Installation](guide/installation.md)
- [Running in containers](guide/containers.md)
- [Scanning](guide/scanning.md)
- [Mounting & path templates](guide/mounting.md)
- [Tuning & metrics](guide/tuning.md)
- [Ownership, permissions & config](guide/configuration.md)
- [FAQ](guide/faq.md)
```

- [ ] **Step 4: Build to verify**

Run: `mdbook build docs`
Expected: success; no link errors. Confirm `docs/src/SUMMARY.md` section ordering now matches the "Final SUMMARY" reference (User Guide, Formats, Integrations, Internals, Contributing, Reference).

- [ ] **Step 5: Commit** (docs-only)

```bash
git add docs/src/guide docs/src/SUMMARY.md
git commit -m "docs: migrate README usage sections into the User Guide"
```

---

## Task 11: Slim the README, add the Security page, reconcile

**Files:**
- Modify: `SECURITY.md` (repoint internal links before it's included)
- Rewrite: `README.md`
- Create: `docs/src/security.md`
- Modify: `docs/src/SUMMARY.md`
- Modify: `docs/src/changelog.md` (convert temp absolute links back to relative)

- [ ] **Step 1: Repoint `SECURITY.md`'s internal links to absolute Pages URLs.** `{{#include}}` pastes SECURITY.md verbatim into `docs/src/security.md`, so its relative links would resolve against `docs/src/` and break the build (and they point at soon-to-be-shim anchors). Absolute URLs are correct both on the GitHub Security tab and inside the book, and linkcheck skips them (`follow-web-links = false`):

| Old (SECURITY.md lines 5, 28, 30) | New |
| --- | --- |
| `[CHANGELOG.md](CHANGELOG.md)` (both occurrences) | `[CHANGELOG.md](https://sohex.github.io/musefs/changelog.html)` |
| `[CONTRIBUTING.md](CONTRIBUTING.md#test-tiers-beyond-cargo-test)` | `[CONTRIBUTING.md](https://sohex.github.io/musefs/contributing/testing.html#test-tiers-beyond-cargo-test)` |

- [ ] **Step 2: Create `docs/src/security.md`** (mirrors root `SECURITY.md` via include)

```markdown
# Security

{{#include ../../SECURITY.md}}
```

- [ ] **Step 3: Append to `docs/src/SUMMARY.md`** (in `# Reference`, after Changelog)

```markdown
- [Security](security.md)
```

- [ ] **Step 4: Convert the temporary absolute links in `docs/src/changelog.md`** (from Task 8 Step 1) back to relative book links now that the targets exist:

| Old (temp absolute) | New (relative) |
| --- | --- |
| `(https://sohex.github.io/musefs/guide/tuning.html#metrics)` | `(guide/tuning.md#metrics)` |
| `(https://sohex.github.io/musefs/guide/configuration.html#ownership-and-permissions)` | `(guide/configuration.md#ownership-and-permissions)` |
| `(https://sohex.github.io/musefs/formats/overview.html)` | `(formats/overview.md)` |
| `(https://sohex.github.io/musefs/integrations/systemd.html#hardening)` | `(integrations/systemd.md#hardening)` |
| `(https://sohex.github.io/musefs/integrations/overview.html#contrib-changelog)` | `(integrations/overview.md#contrib-changelog)` |

- [ ] **Step 5: Rewrite `README.md` as the landing page.** Keep verbatim from the current README: the title + pitch + CI badge, the **Quick start** section, and the **License** + **Acknowledgements** sections. Add a short **Documentation** section with absolute Pages URLs (crates.io-safe — the README is `musefs/Cargo.toml`'s `readme`). Structure:

```markdown
# musefs

<!-- keep the existing CI badge line -->

<!-- keep the existing one-paragraph pitch -->

## Quick start

<!-- keep the existing Quick start block verbatim -->

## Documentation

Full documentation lives at **<https://sohex.github.io/musefs/>**:

- [Installation](https://sohex.github.io/musefs/guide/installation.html) ·
  [Scanning](https://sohex.github.io/musefs/guide/scanning.html) ·
  [Mounting & path templates](https://sohex.github.io/musefs/guide/mounting.html) ·
  [Tuning](https://sohex.github.io/musefs/guide/tuning.html) ·
  [FAQ](https://sohex.github.io/musefs/guide/faq.html)
- [Supported formats](https://sohex.github.io/musefs/formats/overview.html)
- [Integrations: beets, Picard, Lidarr, systemd](https://sohex.github.io/musefs/integrations/overview.html)
- [Architecture](https://sohex.github.io/musefs/architecture/overview.html) ·
  [Contributing](https://sohex.github.io/musefs/contributing/setup.html) ·
  [Benchmarks](https://sohex.github.io/musefs/benchmarks.html) ·
  [Changelog](https://sohex.github.io/musefs/changelog.html)

## License

<!-- keep verbatim -->

## Acknowledgements

<!-- keep verbatim -->
```

In the kept Quick start block, repoint any links into other docs to absolute Pages URLs (e.g. a "see Installation" pointer → `https://sohex.github.io/musefs/guide/installation.html`).

- [ ] **Step 6: Full build + link check**

Run: `mdbook build docs`
Expected: success; zero broken links across the whole book (including the `{{#include}}` of SECURITY.md and the changelog relative links).

- [ ] **Step 7: Manual anchor audit.** mdbook-linkcheck does **not** validate `#fragment` anchors, so a green build does not prove anchor links resolve. Verify each anchor-bearing cross-page link points at a heading that exists on the destination page. For every link of the form `(<page>.md#<anchor>)` in the book, confirm the destination page has a heading whose slug is `<anchor>`:

```bash
# list every intra-book anchored link, then eyeball each against its target page's headings
grep -rnoE '\]\([^)]+\.md#[a-z0-9-]+\)' docs/src
```
Spot-check the known ones: `changelog.md` → `guide/tuning.md#metrics`, `guide/configuration.md#ownership-and-permissions`, `integrations/systemd.md#hardening`, `integrations/overview.md#contrib-changelog`; `formats/*` → `architecture/serving.md#the-segment-model` / `#backing-read-ahead`; guide/integration pages → `architecture/store.md#the-sqlite-store` / `#the-external-writer-contract`, `contributing/plugins.md#python-plugins-contrib`. Fix any anchor whose target heading slug differs.

- [ ] **Step 8: Repo-wide orphan sweep** — confirm no dangling references to moved/deleted paths remain outside the book sources and intended shims:

```bash
grep -rnE '\]\((\.\./)*(ARCHITECTURE|CONTRIBUTING|BENCHMARKS)\.md' \
  --include='*.md' --include='*.rs' --include='*.sh' --include='*.py' . \
  | grep -v 'docs/superpowers' | grep -v 'docs/src/'
grep -rn 'docs/FLAC.md\|docs/MP3.md\|docs/M4A.md\|docs/OGG.md\|docs/WAV.md' \
  --include='*.md' --include='*.rs' . | grep -v 'docs/superpowers'
```
Expected: only the intended shim self-references (root `ARCHITECTURE.md`/`CONTRIBUTING.md` shims pointing at `docs/src/...`) and nothing pointing at the deleted `BENCHMARKS.md` or old `docs/<FMT>.md` paths. Fix any stragglers.

- [ ] **Step 9: Confirm pre-commit-relevant gates**

Run: `cargo fmt --all --check` (should be clean — no Rust changed here) and re-run `yamllint -c .yamllint .github/workflows/docs.yml`.

- [ ] **Step 10: Commit** (docs-only — `SECURITY.md` is `*.md`, so the gate still skips)

```bash
git add SECURITY.md README.md docs/src/security.md docs/src/SUMMARY.md docs/src/changelog.md
git commit -m "docs: slim README to a landing page; add Security page; reconcile links"
```

- [ ] **Step 11: Open the PR**

```bash
git push -u origin <branch>
gh pr create --title "docs: publish mdBook site to GitHub Pages" --body "<summary + Fixes #N if an issue exists>"
```
Then set repo Settings → Pages → Source = **GitHub Actions** (one-time) so the post-merge deploy succeeds.

---

## Self-review notes

- **Spec coverage:** content model (Tasks 3–11), shims (3,5,9,11), BENCHMARKS-no-shim + ripple (6,7), CHANGELOG split (8), SECURITY include + internal-link repoint (11 Steps 1–2), book under `docs/` + gitignore (1), `docs.yml` permissions/concurrency/environment + `docs/book/html` artifact (2), one-time Pages source setting (Execution notes + Task 11 Step 11), version-pinning (1,2), ripple checklist incl. crates.io README absolute links (11) and schema-py vendor regen (7), commit isolation (Execution notes; Tasks 2 and 7 flagged). The spec ripple `SECURITY.md → CHANGELOG/CONTRIBUTING` is now handled in Task 11 Step 1. All spec sections map to a task.
- **Cross-link integrity:** every migration task points cross-area links at targets that already exist (book pages or still-present root files); cyclic links (architecture↔formats, architecture↔contributing, changelog→guide) use a documented temporary form (root path or `follow-web-links=false` absolute URL) and are repointed once both ends exist (Tasks 4, 5, 6, 11). Root-escaping `../../…` links are permitted only because `traverse-parent-directories = true` is set in `book.toml` (Task 1) — verified against mdbook-linkcheck 0.7.7.
- **Anchor stability:** architecture/contributing splits cut on `##` boundaries so referenced anchors (`#the-segment-model`, `#the-external-writer-contract`, `#crate-layout`, `#the-sqlite-store`, `#the-contrib-ecosystem`, `#backing-read-ahead`, `#coverage-guided-fuzzing`, `#python-plugins-contrib`) survive on their new pages; external refs (CLAUDE.md, code/scripts) repointed in Tasks 3, 5, 7. Because linkcheck does **not** validate anchors, Task 11 Step 7 manually audits every intra-book `#anchor` link rather than relying on the green build.
