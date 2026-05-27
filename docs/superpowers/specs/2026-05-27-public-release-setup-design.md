# Public Release Setup (v0.2.0) — Design

Date: 2026-05-27
Status: Approved, ready for planning

## Problem

The musefs codebase is engineering-complete (all six formats, beets plugin,
performance/concurrency pass, arbitrary-tag support, end-to-end FUSE tests, MIT
license, complete README) but has never been published and lacks release
scaffolding: there is no GitHub repository, no CI, no dependency audit, no
changelog, and the only tag (`v0.1.0`) points at the early MVP/roadmap commit
214 commits back. This project ships the first public release.

## Goal

Publish `musefs` as a public GitHub repository at **`Sohex/musefs`** with the
release hygiene that signals a maintained project: CI, a scheduled security
audit, a changelog, current crate metadata, and a versioned **v0.2.0** tag +
GitHub Release. No new features.

## Decisions (settled during brainstorming)

- **Repo:** public, `Sohex/musefs`, SSH remote (matches the gh CLI's configured
  git protocol).
- **Version:** **v0.2.0**. The existing `v0.1.0` tag (the MVP milestone, per the
  ROADMAP's "Delivered in v0.1.0") is kept in history; current `main` becomes
  v0.2.0, matching the ROADMAP's "Delivered since v0.1.0" framing. All five crate
  versions bump to 0.2.0.
- **CI:** "core + FUSE e2e" — a `check` job (fmt + clippy + workspace tests) and
  an `e2e` job that installs libfuse3 and runs the `#[ignore]`d real-mount suite.
- **Security audit:** a separate `audit.yml` (scheduled weekly + on manifest/lock
  changes) so a newly-published advisory never reds an unrelated PR.
- **crates.io:** metadata only; **not published** this release. Users install via
  `cargo install --git` or build from source.
- **Release strategy (Approach A):** one-time manual `gh release create`; no
  prebuilt binaries, no release-on-tag automation, no governance scaffolding
  (Dependabot, branch protection, CONTRIBUTING/issue templates) — all are cheap
  incremental follow-ups, out of scope here.
- **Tracking / `.gitignore`:** ignore `/.claude/` (agent tooling, not project
  content); track `.serena/project.yml` and `.serena/.gitignore` (Serena is
  designed to be versioned; its nested ignore already excludes `cache/` and
  `project.local.yml`). The unused `docs/agents/` scaffolding (untracked, inert,
  unreferenced) is **deleted** outright rather than ignored.

## Part 1 — In-repo changes (committed before publish)

All changes land on the `release-setup` branch and merge to `main`.

### 1.1 Versioning + crate metadata

Introduce a `[workspace.package]` table in the root `Cargo.toml` so the five
crates share one source of truth:

```toml
[workspace.package]
version = "0.2.0"
edition = "2021"
license = "MIT"
repository = "https://github.com/Sohex/musefs"
```

Each member crate switches its `[package]` to inherit
(`version.workspace = true`, `edition.workspace = true`,
`license.workspace = true`, `repository.workspace = true`) and adds a one-line
`description`:

- `musefs-db` — "SQLite store and schema for musefs (tracks, tags, content-addressed art)."
- `musefs-format` — "On-the-fly audio metadata synthesis and byte-layout for musefs (FLAC/MP3/MP4/Ogg/WAV)."
- `musefs-core` — "Orchestration for musefs: virtual tree, tag resolution, and scanning."
- `musefs-fuse` — "FUSE adapter for musefs (fuser)."
- `musefs-cli` — "musefs command-line interface: scan a music library and mount a re-tagged virtual view."

`musefs-cli` additionally gets `keywords = ["fuse", "music", "metadata", "filesystem", "audio"]`
and `categories = ["filesystem", "multimedia::audio", "command-line-utilities"]`.
`Cargo.lock` is refreshed by a build and committed.

Internal `path` dependencies are left as-is (no version added) — the crates are
not being published, so path deps need no version field.

### 1.2 `CHANGELOG.md`

New file at repo root, Keep-a-Changelog format, SemVer. A `[0.2.0] - 2026-05-27`
entry (first public release) with an `### Added` summary drawn from the ROADMAP —
M4A/M4B, Ogg (Opus/Vorbis/FLAC-in-Ogg) and WAV synthesis; arbitrary-tag support
via the canonical vocabulary; the beets plugin; the performance/concurrency/
caching pass — plus a short `[0.1.0]` baseline line noting the MVP was never
publicly released.

### 1.3 `.github/workflows/ci.yml`

Triggers: `push` to `main`, and `pull_request`. Concurrency group cancels
superseded runs for the same ref. Two jobs on `ubuntu-latest`:

- **`check`:** `actions/checkout@v4`; `dtolnay/rust-toolchain@stable` (components
  `rustfmt`, `clippy`); `Swatinem/rust-cache@v2`; then
  `cargo fmt --all -- --check`, `cargo clippy --all-targets -- -D warnings`,
  `cargo test --workspace`. Mirrors the pre-commit hook plus the normal suite.
- **`e2e`:** checkout; `sudo apt-get update && sudo apt-get install -y fuse3
  libfuse3-dev pkg-config` (the `fuser` 0.14 dependency uses libfuse3 /
  `fusermount3`); stable toolchain + rust-cache; then
  `cargo test -p musefs-fuse -- --ignored` (the real-mount, byte-identical suite;
  GitHub's ubuntu runners provide `/dev/fuse`).

### 1.4 `.github/workflows/audit.yml`

Triggers: weekly `schedule` (cron), plus `push`/`pull_request` filtered to
`Cargo.toml`/`Cargo.lock` (and the workflow file). One `ubuntu-latest` job using
`rustsec/audit-check@v2` with `token: ${{ secrets.GITHUB_TOKEN }}` so advisories
surface as annotations / a tracked issue. Kept out of `ci.yml` so advisory-DB
changes never fail an unrelated PR.

### 1.5 README

Add a CI status badge at the top:
`[![CI](https://github.com/Sohex/musefs/actions/workflows/ci.yml/badge.svg)](https://github.com/Sohex/musefs/actions/workflows/ci.yml)`.
Add a one-line install option to the Build section:
`cargo install --git https://github.com/Sohex/musefs musefs-cli`.
Requirements/Usage are already accurate (Linux + FUSE stated) — no other edits.

### 1.6 `.gitignore` and tracking

Extend the root `.gitignore` from `/target` to also ignore `/.claude/`. Delete
the unused `docs/agents/` directory (untracked, inert, unreferenced) so it needs
no ignore entry. Stage `.serena/project.yml` and `.serena/.gitignore` for
tracking (the nested `.serena/.gitignore` already excludes `/cache` and
`/project.local.yml`, so no large/local files are added).

## Part 2 — GitHub execution sequence

Performed after Part 1 merges to `main`.

1. **Pre-push hygiene.** Confirm `git status` and the tree contain no secrets,
   credentials, or unintended files; verify the new `.gitignore` excludes
   `target/` and `.claude/`, that `docs/agents/` is gone, and that (via the nested
   ignore) the Serena cache/local are excluded. The push makes the code
   world-visible, so this is a hard gate.
2. **Create the repo (empty).** `gh repo create Sohex/musefs --public
   --description "<one-liner>"` (no `--source/--push`, for explicit control).
   One-liner: "Read-only passthrough FUSE filesystem presenting a virtually
   reorganized, re-tagged view of a music library backed by SQLite — without
   copying or modifying audio bytes."
3. **Wire the remote (SSH).** `git remote add origin git@github.com:Sohex/musefs.git`.
4. **Push `main`.** `git push -u origin main` — triggers the first CI run.
5. **Repo metadata.** `gh repo edit Sohex/musefs` to add topics: `rust`, `fuse`,
   `filesystem`, `music`, `metadata`, `sqlite`, `beets`, `flac`.
6. **Tags.** Push the existing MVP tag, then create and push the release tag:
   `git push origin v0.1.0`; `git tag -a v0.2.0 -m "musefs v0.2.0"`;
   `git push origin v0.2.0`.
7. **Release.** `gh release create v0.2.0 --title "musefs v0.2.0"
   --notes-file <the [0.2.0] CHANGELOG section>`. GitHub auto-attaches source
   tarballs; no binaries.
8. **Verify.** Watch the run (`gh run watch`); confirm `check` and `e2e` are
   green and the release page is correct. Report the release URL.

## Error handling / risks

- **e2e job on the runner.** If FUSE mounting fails on `ubuntu-latest` (libfuse
  packaging or `/dev/fuse` permissions), diagnose first; if it can't be made
  reliable, downgrade `e2e` to non-blocking (keep `check` as the required signal)
  and note it, rather than block the release. Do not weaken the tests themselves.
- **Advisory-DB flakiness.** Mitigated by isolating the audit into its own
  scheduled/path-filtered workflow (1.4).
- **Accidental disclosure.** Mitigated by the Part 2 pre-push hygiene gate and the
  `.gitignore` decisions (1.6).

## Verification / done criteria

- Workspace builds; `cargo fmt --check`, `cargo clippy --all-targets -- -D
  warnings`, and `cargo test --workspace` pass locally before push.
- `github.com/Sohex/musefs` exists, is public, has the description + topics, and
  shows the README with a (soon-green) CI badge.
- CI `check` and `e2e` jobs pass on the pushed `main`.
- Tags `v0.1.0` and `v0.2.0` are present; a `v0.2.0` GitHub Release exists with
  notes from the changelog.
- The published tree contains no `.claude/`, Serena cache, or secrets, and
  `docs/agents/` is absent.

## Out of scope (deferred, easy follow-ups)

- Publishing to crates.io (metadata is added now to make it a quick later step).
- Prebuilt release binaries and a release-on-tag automation workflow.
- Dependabot, branch protection / required status checks, CONTRIBUTING,
  issue/PR templates, CODE_OF_CONDUCT.
- A picard plugin and writable mount (already deferred in the project ROADMAP).
