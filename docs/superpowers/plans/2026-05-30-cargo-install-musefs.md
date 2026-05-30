# `cargo install musefs` Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make the workspace publishable to crates.io so users can run `cargo install musefs`, with a tag-triggered release workflow that publishes all crates in dependency order.

**Architecture:** Add a thin `musefs` wrapper crate that owns the `musefs` binary and depends on the existing `musefs-cli` library crate (demoted to lib-only). Give every inter-crate `path` dependency a `version` so crates.io accepts them. Add `.github/workflows/release.yml` that publishes `db → format → core → fuse → cli → musefs` on a `v*` tag using a `CARGO_REGISTRY_TOKEN` secret.

**Tech Stack:** Rust / Cargo workspace, GitHub Actions, crates.io.

Spec: `docs/superpowers/specs/2026-05-30-cargo-install-musefs-design.md`

---

## File Structure

- **Create** `musefs/Cargo.toml` — wrapper package manifest (`name = "musefs"`, `[[bin]] name = "musefs"`).
- **Create** `musefs/src/main.rs` — the `main()` entrypoint (moved verbatim from `musefs-cli/src/main.rs`).
- **Delete** `musefs-cli/src/main.rs` — `musefs-cli` becomes library-only.
- **Modify** `musefs-cli/Cargo.toml` — remove `[[bin]]`; drop `clap`/`anyhow` from deps only if unused by `lib.rs` (verify; default is to leave them).
- **Modify** root `Cargo.toml` — add `"musefs"` to `members`.
- **Modify** `musefs-core/Cargo.toml`, `musefs-fuse/Cargo.toml`, `musefs-cli/Cargo.toml` — add `version` to inter-crate path deps.
- **Create** `.github/workflows/release.yml` — tag-triggered publish.
- **Modify** `README.md` — install section.
- **Modify** `CHANGELOG.md` — Unreleased entry.

---

## Task 1: Create the `musefs` wrapper crate and demote `musefs-cli` to lib-only

**Files:**
- Create: `musefs/Cargo.toml`
- Create: `musefs/src/main.rs`
- Delete: `musefs-cli/src/main.rs`
- Modify: `musefs-cli/Cargo.toml` (remove `[[bin]]`)
- Modify: `Cargo.toml` (add `"musefs"` to `members`)

- [ ] **Step 1: Create the wrapper manifest `musefs/Cargo.toml`**

```toml
[package]
name = "musefs"
description = "Read-only FUSE filesystem presenting a re-tagged virtual view of a music library."
version.workspace = true
edition.workspace = true
license.workspace = true
repository.workspace = true
readme = "../README.md"
keywords = ["fuse", "music", "metadata", "filesystem", "audio"]
categories = ["filesystem", "multimedia::audio", "command-line-utilities"]

[[bin]]
name = "musefs"
path = "src/main.rs"

[dependencies]
clap = { version = "4", features = ["derive"] }
musefs-cli = { path = "../musefs-cli", version = "0.2.0" }

[lints]
workspace = true
```

- [ ] **Step 2: Create `musefs/src/main.rs` (verbatim copy of the current CLI entrypoint)**

```rust
use clap::Parser;
use musefs_cli::{run, Cli};

fn main() {
    if let Err(e) = run(Cli::parse()) {
        eprintln!("musefs: {e:#}");
        std::process::exit(1);
    }
}
```

- [ ] **Step 3: Delete the old binary entrypoint**

Run: `git rm musefs-cli/src/main.rs`
Expected: `rm 'musefs-cli/src/main.rs'`

- [ ] **Step 4: Remove the `[[bin]]` section from `musefs-cli/Cargo.toml`**

Delete exactly these three lines:

```toml
[[bin]]
name = "musefs"
path = "src/main.rs"
```

Leave the rest of `musefs-cli/Cargo.toml` unchanged (the `[dependencies]` keep `clap` and `anyhow` — `lib.rs` uses them). After this edit `musefs-cli` builds only its `src/lib.rs`.

- [ ] **Step 5: Add the wrapper to the workspace members**

In root `Cargo.toml`, change:

```toml
members = ["musefs-db", "musefs-format", "musefs-core", "musefs-fuse", "musefs-cli"]
```

to:

```toml
members = ["musefs-db", "musefs-format", "musefs-core", "musefs-fuse", "musefs-cli", "musefs"]
```

- [ ] **Step 6: Verify the workspace builds with exactly one `musefs` binary**

Run: `cargo build --workspace 2>&1 | tail -5 && ls target/debug/musefs`
Expected: build finishes; `target/debug/musefs` exists. No "multiple packages ... produce a binary named musefs" warning.

- [ ] **Step 7: Verify the binary runs**

Run: `target/debug/musefs --help | head -3`
Expected: clap help text for the `musefs` CLI (same as before the move).

- [ ] **Step 8: Commit**

```bash
git add musefs/Cargo.toml musefs/src/main.rs Cargo.toml musefs-cli/Cargo.toml
git add -u musefs-cli/src/main.rs
git commit -m "feat: add thin musefs wrapper crate, make musefs-cli lib-only"
```

---

## Task 2: Version the inter-crate path dependencies

**Files:**
- Modify: `musefs-core/Cargo.toml`
- Modify: `musefs-fuse/Cargo.toml`
- Modify: `musefs-cli/Cargo.toml`

(`musefs-format` has no inter-crate deps; the `musefs` wrapper's `musefs-cli` dep was already versioned in Task 1.)

- [ ] **Step 1: Version `musefs-core` deps**

In `musefs-core/Cargo.toml`, under `[dependencies]`, change:

```toml
musefs-db = { path = "../musefs-db" }
musefs-format = { path = "../musefs-format" }
```

to:

```toml
musefs-db = { path = "../musefs-db", version = "0.2.0" }
musefs-format = { path = "../musefs-format", version = "0.2.0" }
```

Leave the `musefs-format` line under `[dev-dependencies]` (the `features = ["fuzzing"]` one) unchanged — dev-deps don't need a version for publishing.

- [ ] **Step 2: Version `musefs-fuse` dep**

In `musefs-fuse/Cargo.toml`, under `[dependencies]`, change:

```toml
musefs-core = { path = "../musefs-core" }
```

to:

```toml
musefs-core = { path = "../musefs-core", version = "0.2.0" }
```

- [ ] **Step 3: Version `musefs-cli` deps**

In `musefs-cli/Cargo.toml`, under `[dependencies]`, change:

```toml
musefs-db = { path = "../musefs-db" }
musefs-core = { path = "../musefs-core" }
musefs-fuse = { path = "../musefs-fuse" }
```

to:

```toml
musefs-db = { path = "../musefs-db", version = "0.2.0" }
musefs-core = { path = "../musefs-core", version = "0.2.0" }
musefs-fuse = { path = "../musefs-fuse", version = "0.2.0" }
```

- [ ] **Step 4: Verify the workspace still builds and resolves**

Run: `cargo build --workspace 2>&1 | tail -3`
Expected: builds cleanly (path takes precedence locally; the `version` is only consulted by crates.io).

- [ ] **Step 5: Verify each crate packages cleanly**

Run:

```bash
for c in musefs-db musefs-format musefs-core musefs-fuse musefs-cli musefs; do
  echo "=== $c ==="; cargo package -p "$c" --allow-dirty --no-verify 2>&1 | tail -2
done
```

Expected: each prints `Packaged ... files` (or `Packaging`/`Packaged` lines) with no error. `--no-verify` skips the compile step (which for upper crates would need the lower crates already on crates.io); this confirms the manifest and file list assemble. `--allow-dirty` is fine here since the tree may have uncommitted plan edits.

- [ ] **Step 6: Commit**

```bash
git add musefs-core/Cargo.toml musefs-fuse/Cargo.toml musefs-cli/Cargo.toml
git commit -m "build: version inter-crate path deps for crates.io publishing"
```

---

## Task 3: Add the tag-triggered release workflow

**Files:**
- Create: `.github/workflows/release.yml`

- [ ] **Step 1: Create `.github/workflows/release.yml`**

```yaml
name: Release

on:
  push:
    tags:
      - 'v*'

concurrency:
  group: release-${{ github.ref }}
  cancel-in-progress: false

permissions:
  contents: read

jobs:
  publish:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@de0fac2e4500dabe0009e67214ff5f5447ce83dd
        with:
          persist-credentials: false
      - name: Install libfuse3
        run: sudo apt-get update && sudo apt-get install -y fuse3 libfuse3-dev pkg-config
      - uses: dtolnay/rust-toolchain@29eef336d9b2848a0b548edc03f92a220660cdb8
      - uses: Swatinem/rust-cache@e18b497796c12c097a38f9edb9d0641fb99eee32
      - name: Verify tag matches workspace version
        run: |
          TAG_VERSION="${GITHUB_REF_NAME#v}"
          WS_VERSION="$(grep -m1 '^version' Cargo.toml | sed -E 's/.*"([^"]+)".*/\1/')"
          echo "tag=$TAG_VERSION workspace=$WS_VERSION"
          if [ "$TAG_VERSION" != "$WS_VERSION" ]; then
            echo "::error::Tag $GITHUB_REF_NAME does not match workspace version $WS_VERSION"
            exit 1
          fi
      - name: Publish crates in dependency order
        env:
          CARGO_REGISTRY_TOKEN: ${{ secrets.CARGO_REGISTRY_TOKEN }}
        run: |
          set -euo pipefail
          for c in musefs-db musefs-format musefs-core musefs-fuse musefs-cli musefs; do
            echo "=== publishing $c ==="
            cargo publish -p "$c" --locked
          done
```

Notes for the implementer:
- The SHAs above are copied from `.github/workflows/ci.yml` (same checkout / toolchain / cache actions) to keep pinning consistent.
- `cargo publish` reads `CARGO_REGISTRY_TOKEN` from the environment; no `--token` flag needed.
- Modern `cargo publish` blocks until the just-published version is live in the registry index before returning, so the next crate's `version = "0.2.0"` requirement resolves without manual sleeps.
- `cancel-in-progress: false` so a publish run is never interrupted mid-sequence.

- [ ] **Step 2: Lint the YAML for basic validity**

Run: `python -c "import yaml,sys; yaml.safe_load(open('.github/workflows/release.yml')); print('ok')"`
Expected: `ok`

- [ ] **Step 3: Dry-run the version-guard logic locally**

Run:

```bash
GITHUB_REF_NAME=v0.2.0
TAG_VERSION="${GITHUB_REF_NAME#v}"
WS_VERSION="$(grep -m1 '^version' Cargo.toml | sed -E 's/.*"([^"]+)".*/\1/')"
echo "tag=$TAG_VERSION workspace=$WS_VERSION"; [ "$TAG_VERSION" = "$WS_VERSION" ] && echo MATCH || echo MISMATCH
```

Expected: `tag=0.2.0 workspace=0.2.0` then `MATCH`.

- [ ] **Step 4: Commit**

```bash
git add .github/workflows/release.yml
git commit -m "ci: add tag-triggered crates.io release workflow"
```

---

## Task 4: Update the docs

**Files:**
- Modify: `README.md` (the `## Build` section, lines ~110-122)
- Modify: `CHANGELOG.md` (Unreleased → Added)

- [ ] **Step 1: Update the README install/build section**

Replace this block in `README.md`:

```markdown
## Build

```bash
cargo build --release
```

The binary is `musefs` (the `musefs-cli` crate).

Or install the `musefs` binary directly from the repository:

```bash
cargo install --git https://github.com/Sohex/musefs musefs-cli
```
```

with:

```markdown
## Install

Install the `musefs` binary from crates.io:

```bash
cargo install musefs
```

`cargo install` compiles from source, so the same prerequisites as a local
build apply: a Rust toolchain plus FUSE (`libfuse3` / `libfuse3-dev`) and
`pkg-config` on Linux.

Or install the latest from the repository:

```bash
cargo install --git https://github.com/Sohex/musefs musefs
```

## Build

```bash
cargo build --release
```

The binary is `musefs` (the `musefs` crate).
```

- [ ] **Step 2: Add the CHANGELOG entry**

In `CHANGELOG.md`, under `## [Unreleased]` → `### Added`, add this as the first bullet:

```markdown
- **crates.io distribution:** the `musefs` binary is now published to crates.io
  and installable with `cargo install musefs`. A new thin `musefs` wrapper crate
  owns the binary (`musefs-cli` is now a library crate), and a tag-triggered
  release workflow publishes all crates in dependency order.
```

- [ ] **Step 3: Verify the README has no stale `musefs-cli` install reference**

Run: `grep -n "cargo install" README.md`
Expected: shows `cargo install musefs`, `cargo install --git ... musefs`, and the unrelated `cargo install cargo-fuzz` line — and NO `musefs-cli`.

- [ ] **Step 4: Commit**

```bash
git add README.md CHANGELOG.md
git commit -m "docs: document cargo install musefs and crates.io distribution"
```

---

## Task 5: Final verification

**Files:** none (verification only)

- [ ] **Step 1: Format check**

Run: `cargo fmt --all -- --check`
Expected: no output, exit 0.

- [ ] **Step 2: Clippy**

Run: `cargo clippy --all-targets -- -D warnings 2>&1 | tail -3`
Expected: finishes with no warnings/errors.

- [ ] **Step 3: Workspace tests**

Run: `cargo test --workspace 2>&1 | tail -5`
Expected: all tests pass (FUSE e2e stay `#[ignore]`d).

- [ ] **Step 4: Local install smoke test**

Run: `cargo install --path musefs --locked --force 2>&1 | tail -3 && musefs --help | head -2`
Expected: `Installed package musefs ...`; `musefs --help` prints the CLI help.

- [ ] **Step 5: Confirm the published-name binary mapping**

Run: `cargo metadata --no-deps --format-version 1 | python -c "import json,sys; d=json.load(sys.stdin); print([ (p['name'], [t['name'] for t in p['targets'] if 'bin' in t['kind']]) for p in d['packages'] if p['name'] in ('musefs','musefs-cli')])"`
Expected: `[('musefs', ['musefs']), ('musefs-cli', [])]` (order may vary) — the binary lives only in the `musefs` package.

- [ ] **Step 6: Post-merge manual step (document, do not run in CI)**

Add a `CARGO_REGISTRY_TOKEN` repository secret (GitHub → Settings → Secrets and variables → Actions) holding a crates.io API token with publish scope. The release workflow needs it on the first `v*` tag push. No code change; this is an operator action recorded here for the handoff.

---

## Self-Review

**Spec coverage:**
- §1 Crate topology → Task 1 (wrapper crate, lib-only `musefs-cli`, members).
- §2 Versioned path deps → Task 2 (+ wrapper dep versioned in Task 1).
- §3 Metadata polish → Task 1 Step 1 (`readme`, `keywords`, `categories`, `description` on `musefs`).
- §4 Release workflow → Task 3 (tag trigger, version guard, ordered publish, token secret in Task 5 Step 6).
- §5 Documentation → Task 4 (README install + prerequisites + `--git` fallback; CHANGELOG).
- §6 Testing/verification → Task 2 Step 5 (`cargo package`), Task 5 (fmt/clippy/test/install).

**Placeholder scan:** No TBD/TODO; every code/config step shows full content; commands have expected output.

**Type/name consistency:** Wrapper `main.rs` uses `musefs_cli::{run, Cli}` matching the existing `lib.rs` exports. Crate names and the `0.2.0` version string are consistent across all manifests and the workflow guard.
