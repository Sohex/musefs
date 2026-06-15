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
| `BENCHMARKS.md` | `docs/src/benchmarks.md` | **deleted — no shim**; regen harness retargets |
| `CHANGELOG.md` (full) | `docs/src/changelog.md` | `CHANGELOG.md` → curated user-facing highlights + link |
| `contrib/{beets,picard,lidarr}/README.md` + python-musefs | `docs/src/integrations/*` | each `contrib/*/README.md` → shim |
| `SECURITY.md` | stays canonical at root | mirrored into the book via `{{#include ../../SECURITY.md}}` |

### Shims

A shim is ~3–5 lines: one sentence of context plus a link to the published
book page **and** to the in-repo source under `docs/src/...` (so it is useful
both on GitHub and offline). Shims carry no canonical content.

Exceptions to the plain-shim rule:

- **`README.md`** is a *fuller* landing page, not a bare pointer: the pitch
  (re-tagged/reorganized read-only view; original audio bytes never copied or
  modified), the CI badge, a complete **quick start** that gets someone
  installed-scanned-mounted standalone, a short "what it's for", and links
  onward to the book for everything else (install detail, usage, formats,
  architecture, contributing). It must stand on its own through quick start
  without the reader leaving the page.
- **`CHANGELOG.md`** keeps a curated, user-facing subset of changes plus a
  link to the full changelog in the book — a real subset, not a pointer.
- **`SECURITY.md`** stays canonical at root (GitHub's Security tab wants it
  there) and is surfaced in the book through `{{#include ../../SECURITY.md}}`
  rather than a raw symlink, so it survives non-Linux checkouts and needs no
  separate `SUMMARY.md` heading hack.
- **`BENCHMARKS.md`** is deleted outright (no shim). Its content is canonical
  at `docs/src/benchmarks.md`, and the benchmark-regeneration harness is
  pointed at the new path so a future bench run does not recreate or clobber a
  stale root file.

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
Integrations                 (overview + beets / Picard / Lidarr / python-musefs)
Architecture                 (ARCHITECTURE.md content, split into a few pages)
Contributing                 (CONTRIBUTING.md content, split into a few pages)
Benchmarks
Changelog                    (full)
Security                     ({{#include}} of root SECURITY.md)
```

The user guide maps the README's many H3/H4 sections onto ~9 coherent pages
rather than one page per heading, to avoid fragmentation. `docs/superpowers/`
is untouched and excluded from the book — mdBook renders only what
`SUMMARY.md` references.

## Tooling & deployment

- **Book root** `docs/`: `docs/book.toml`, sources in `docs/src/`, build output
  `docs/book/` added to `.gitignore`. Putting the book under `docs/` keeps every
  book artifact (including `book.toml`) within the pre-commit cargo-gate skip
  set (paths under `docs/` or `*.md`), so doc-only commits still skip the Rust
  test gate.
- **Tools**: `mdbook` + `mdbook-linkcheck`, both pinned to explicit versions in
  CI for reproducibility.
- **`.github/workflows/docs.yml`**:
  - On pull requests touching docs: `mdbook build` + `mdbook-linkcheck`, **no
    deploy**. Broken book links or build failures fail CI.
  - On push to `main`: build + deploy to Pages via the official Pages actions
    (`actions/configure-pages`, `actions/upload-pages-artifact`,
    `actions/deploy-pages`), with GitHub Actions as the Pages source (no
    `gh-pages` branch).
  - YAML must pass the pre-commit `yamllint` leg.
- **URL**: `https://sohex.github.io/musefs/`; `book.toml` `site-url = "/musefs/"`.
  No custom domain.

## Ripple effects

These break silently if missed; the implementation plan must handle each:

- **`CLAUDE.md`** links to `ARCHITECTURE.md#the-segment-model`,
  `ARCHITECTURE.md#the-external-writer-contract`, and `CONTRIBUTING.md#...`
  anchors that die once those files become shims. Repoint them to the in-repo
  `docs/src/...` source paths (clickable offline, stable across the move).
- **Benchmark regen harness** currently writes `BENCHMARKS.md`; retarget it to
  `docs/src/benchmarks.md` (grep the bench harness / scripts for the path).
- **Cross-links** in README, contrib READMEs, and format docs to absorbed files
  must be updated to book paths or in-repo source paths.
- **Link checking**: book-internal links are covered by `mdbook-linkcheck`; any
  existing repo-wide markdown link sweep must still pass against the new shims
  and relocated files.

## Validation

- `mdbook build` clean; `mdbook-linkcheck` reports zero broken links.
- Pre-commit stays green: book files live under `docs/`/`*.md` so the cargo gate
  keeps skipping; `docs.yml` passes `yamllint`.
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
