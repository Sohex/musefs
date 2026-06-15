# mdBook docs site on GitHub Pages — design

## Problem

The repo's documentation is correct and well-partitioned (post-#64) but
unwelcoming. `README.md` is a 647-line / 34K wall of text; `ARCHITECTURE.md`,
`CONTRIBUTING.md`, `BENCHMARKS.md`, the per-format `docs/*.md`, and the
`contrib/*` READMEs are each standalone files with no navigable home. There is
no published documentation site. A newcomer has nowhere to *browse*, and the
README front door buries the quick start under deep operational detail.

## Goal

Publish a navigable mdBook site to GitHub Pages and slim the README to a real
landing page. The book becomes the **single source of truth** for long-form
documentation; conventional repo locations keep thin **shims** so GitHub and
crates.io still work the way contributors and packagers expect.

## Content model

Canonical long-form content moves into `docs/src/`. Each conventional location
keeps a shim except where noted.

| Source today | Canonical destination | What stays behind |
| ------------ | --------------------- | ----------------- |
| `README.md` (usage guts) | user-guide pages in `docs/src/` | `README.md` → fuller landing page (see below) |
| `ARCHITECTURE.md` | `docs/src/architecture/*` | `ARCHITECTURE.md` → shim |
| `CONTRIBUTING.md` | `docs/src/contributing/*` | `CONTRIBUTING.md` → shim |
| `docs/{FLAC,MP3,M4A,OGG,WAV}.md` | `docs/src/formats/*` | moved; no shim (already under `docs/`) |
| `BENCHMARKS.md` | `docs/src/benchmarks.md` | **deleted — no shim** (it is hand-authored; nothing regenerates it) |
| `CHANGELOG.md` (full) | `docs/src/changelog.md` | `CHANGELOG.md` → curated user-facing highlights + link |
| `contrib/{beets,picard,lidarr,systemd}/README.md` + python-musefs README | `docs/src/integrations/*` | each `contrib/*/README.md` → shim |
| `contrib/CHANGELOG.md` | stays at `contrib/CHANGELOG.md` | linked from the Integrations overview; the book Changelog page covers the musefs project only |
| `SECURITY.md` | stays canonical at root | mirrored into the book via `{{#include ../../SECURITY.md}}` |

### Shims

A shim is ~3–5 lines: one sentence of context plus a link to the published
book page **and** to the in-repo source under `docs/src/...` (so it is useful
both on GitHub and offline). Shims carry no canonical content.

Exceptions to the plain-shim rule:

- **`README.md`** is a *fuller* landing page, not a bare pointer: the pitch
  (re-tagged/reorganized read-only view; original audio bytes never copied or
  modified), the CI badge, a complete **quick start** that gets someone
  installed-scanned-mounted standalone, a short "what it's for", the existing
  Status / License / Acknowledgements tail (kept — landing-page material), and
  links onward to the book for everything else (install detail, usage, formats,
  architecture, contributing). It must stand on its own through quick start
  without the reader leaving the page. Because `musefs/Cargo.toml` sets
  `readme = "../README.md"`, this file is also the crates.io front page:
  **onward links in `README.md` use absolute `https://sohex.github.io/musefs/`
  URLs**, never repo-relative `docs/src/...` paths (which crates.io cannot
  resolve). Shims viewed on GitHub (ARCHITECTURE/CONTRIBUTING/contrib) may use
  repo-relative `docs/src/...` source links plus the published URL.
- **`CHANGELOG.md`** keeps a curated, user-facing subset of changes plus a
  link to the full changelog in the book — a real subset, not a pointer.
- **`SECURITY.md`** stays canonical at root (GitHub's Security tab wants it
  there) and is surfaced in the book through `{{#include ../../SECURITY.md}}`
  rather than a raw symlink, so it survives non-Linux checkouts and needs no
  separate `SUMMARY.md` heading hack.
- **`BENCHMARKS.md`** is deleted outright (no shim). Its content is canonical
  at `docs/src/benchmarks.md`. It is hand-authored — no harness regenerates it
  — so nothing recreates a stale root file; the only follow-up is repointing
  the textual references to it (see Ripple effects).

## Book structure (`docs/src/SUMMARY.md`)

```
Introduction                 (pitch + "what it's for" + status)
User Guide
  Quick start
  Installation               (prebuilt / cargo / source)
  Running in containers      (required flags, non-root, mount-visibility, sharing)
  Scanning                   (+ content checksums / move re-identification)
  Mounting & path templates
  Tuning & metrics
  Ownership, permissions & config   (ownership, env vars, systemd)
  FAQ
Formats                      (overview + FLAC / MP3 / M4A / OGG / WAV)
Integrations                 (overview + beets / Picard / Lidarr / systemd / python-musefs)
Architecture                 (ARCHITECTURE.md content, split into a few pages)
Contributing                 (CONTRIBUTING.md content, split into a few pages)
Benchmarks
Changelog                    (full)
Security                     (flat docs/src/security.md; {{#include ../../SECURITY.md}})
```

`python-musefs` is a library (the store-contract package), not a plugin —
grouped under Integrations for discoverability but described as such. The
Security page is **flat** (`docs/src/security.md`, no subdirectory) so the
`{{#include ../../SECURITY.md}}` relative depth is correct.

The user guide maps the README's many H3/H4 sections onto eight coherent pages
rather than one page per heading, to avoid fragmentation. `docs/superpowers/`
is untouched and excluded from the book — mdBook renders only what
`SUMMARY.md` references.

## Tooling & deployment

- **Book root** `docs/`: `docs/book.toml`, sources in `docs/src/`, build output
  `docs/book/` added to `.gitignore`. Putting the book under `docs/` keeps every
  book artifact (including `book.toml`) within the pre-commit cargo-gate skip
  set (paths under `docs/` or `*.md`), so doc-only commits still skip the Rust
  test gate.
- **Build-output path with linkcheck enabled**: with `mdbook-linkcheck` active
  as a backend, the HTML renderer output nests under `docs/book/html/` (not
  `docs/book/`). The Pages artifact upload and any local-preview instructions
  must target `docs/book/html`. Pin the renderer/linkcheck config in
  `book.toml` so this path is deterministic.
- **Tools**: `mdbook` + `mdbook-linkcheck`, both pinned to explicit versions in
  CI. `mdbook-linkcheck` lags `mdbook` releases and is version-sensitive — the
  plan must pick a *compatible* pinned pair, not the latest of each
  independently.
- **`.github/workflows/docs.yml`**:
  - On pull requests touching docs: `mdbook build` + `mdbook-linkcheck`, **no
    deploy**. Broken book links or build failures fail CI.
  - On push to `main`: build + deploy to Pages via the official Pages actions
    (`actions/configure-pages`, `actions/upload-pages-artifact` pointed at
    `docs/book/html`, `actions/deploy-pages`), with GitHub Actions as the Pages
    source (no `gh-pages` branch).
  - Required workflow plumbing (the deploy fails without these):
    - job-level `permissions: { pages: write, id-token: write, contents: read }`
    - `concurrency: { group: "pages", cancel-in-progress: false }`
    - the deploy job declares `environment: github-pages`.
  - YAML must pass the pre-commit `yamllint` leg.
- **One-time repo setting (manual/admin)**: GitHub repo Settings → Pages →
  Source must be set to **GitHub Actions**. The first deploy fails until this is
  done; call it out as a prerequisite in the plan, not a code step.
- **URL**: `https://sohex.github.io/musefs/`; `book.toml` `site-url = "/musefs/"`.
  No custom domain.

## Ripple effects

These break silently if missed. The list below is grep-derived against the
current tree, not exhaustive-by-assertion — **the plan's first step is a fresh
repo-wide grep** for every moved/deleted path and anchor, and the result is the
authoritative checklist. Known couplings today:

- **`BENCHMARKS.md` textual references** (it is deleted, so these dangle).
  Repoint to the book page / appropriate anchor:
  - `musefs-cli/src/lib.rs:123` (doc comment)
  - `benches/storage_tunables_bench.sh:4` (comment)
  - `ARCHITECTURE.md:44`, `ARCHITECTURE.md:99`; `README.md:448`
  - SQL-comment mirrors `musefs-db/src/schema.rs:212`,
    `contrib/picard/musefs/_common/schema.py:217`,
    `contrib/python-musefs/src/musefs_common/schema.py:214`. **Caveat:** the
    two Python files are *vendored mirrors* of the Rust schema string — per
    `CLAUDE.md`, the schema text is regenerated via
    `MUSEFS_REGEN_SCHEMA_PY=1 cargo test -p musefs-db schema_py` then
    re-vendored. Edit the Rust source and regenerate; do **not** hand-edit the
    Python mirrors (the `schema_py` drift test will reject it).
- **`CLAUDE.md`** anchor links into `ARCHITECTURE.md#the-segment-model`,
  `ARCHITECTURE.md#the-external-writer-contract`, and `CONTRIBUTING.md#...` die
  once those files become shims. Repoint to in-repo `docs/src/...` source paths.
- **File→file anchor cross-refs** between docs that are *both* moving/shimming:
  - `CONTRIBUTING.md:458`, `CONTRIBUTING.md:481` → `ARCHITECTURE.md#...`
  - `ARCHITECTURE.md:102-103` → the moving `docs/{FLAC,…}.md` format paths
  - every `docs/{FLAC,MP3,M4A,OGG,WAV}.md` → `../ARCHITECTURE.md#the-segment-model`
  - `SECURITY.md:5/28/30` → CHANGELOG / CONTRIBUTING
- **Shell scripts** referencing CONTRIBUTING.md: `scripts/freebsd-vm/provision.sh:5`,
  `scripts/freebsd-vm/run-local.sh:12`.
- **crates.io**: `musefs/Cargo.toml:8` `readme = "../README.md"` — the slimmed
  README is the crates.io front page; its onward links must be absolute Pages
  URLs (see the README shim note), since crates.io cannot resolve repo-relative
  paths into the (un-committed) book output.
- **Link checking**: book-internal links are covered by `mdbook-linkcheck`; any
  existing repo-wide markdown link sweep must still pass against the new shims
  and relocated files.

## Validation

- `mdbook build` clean; `mdbook-linkcheck` reports zero broken links.
- Pre-commit stays green: pure doc relocation lives under `docs/`/`*.md` so the
  cargo gate keeps skipping; `docs.yml` passes `yamllint`. **Commit isolation:**
  the ripple edits that touch code (`musefs-cli/src/lib.rs`, `musefs-db/src/schema.rs`
  + schema-py regen, the `benches/` script, the `scripts/freebsd-vm/` scripts)
  are **not** docs-only and trigger the full cargo/shellcheck gate — the plan
  must isolate them into their own green commits, separate from the bulk
  docs-only relocation commits.
- Repo-wide grep finds no orphaned references to deleted/absorbed docs
  (`BENCHMARKS.md`, old `docs/*.md` format paths, absorbed anchors).
- Manual: every shim resolves; `README.md` renders as a clean landing page on
  GitHub and carries a reader through quick start without leaving the page;
  deployed site navigates correctly under `/musefs/`.

## Out of scope

- No content rewrites beyond what relocation requires (the #64 rework already
  validated the text against the code). Pages are moved and re-cross-linked,
  not re-authored.
- No book versioning, search-tuning, custom theme, or `mdbook` plugins beyond
  `mdbook-linkcheck`.
- `docs/superpowers/` specs and plans stay as-is, outside the book.
