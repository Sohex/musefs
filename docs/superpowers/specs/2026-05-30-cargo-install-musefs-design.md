# `cargo install musefs` — distribution design

Date: 2026-05-30
Status: Approved (brainstorming)

## Goal

Let users install the `musefs` binary from crates.io with a single command:

```bash
cargo install musefs
```

Today the binary is produced by the `musefs-cli` package, no crate named
`musefs` exists, and every inter-crate dependency is path-only — so nothing in
the workspace is publishable to crates.io. This work makes the workspace
publish-ready and adds a tag-triggered release workflow that publishes all
crates in dependency order.

## Decisions (from brainstorming)

- **Naming:** a thin `musefs` wrapper crate owns the binary; `musefs-cli` stays
  as the library crate holding all CLI logic. (Not a rename of `musefs-cli`.)
- **Scope:** repo publish-readiness **plus** release automation. Publishing is
  not performed in this work; the first real release happens when a tag is
  pushed.
- **Release mechanics:** tag push (`v*`) triggers a workflow that authenticates
  with a `CARGO_REGISTRY_TOKEN` repo secret (not OIDC trusted publishing).

## 1. Crate topology

Add a sixth crate, `musefs/`, and demote `musefs-cli` to a library:

- **New `musefs/` package:**
  - `Cargo.toml`: `name = "musefs"`, inherits `version`/`edition`/`license`/
    `repository` from the workspace, declares `[[bin]] name = "musefs"`,
    depends on `musefs-cli`.
  - `src/main.rs`: identical to the current `musefs-cli/src/main.rs`:

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

  - Needs `clap` as a direct dependency (for `Cli::parse()`), matching the
    version `musefs-cli` uses (`clap = { version = "4", features = ["derive"] }`).

- **`musefs-cli` changes:**
  - Remove the `[[bin]] name = "musefs"` section.
  - Delete `musefs-cli/src/main.rs`. The crate keeps `src/lib.rs` exposing
    `run` and `Cli`.
  - Rationale: two workspace packages both producing a binary named `musefs`
    would clobber each other in `target/`. Only the wrapper ships the binary.

- **Workspace:** add `"musefs"` to `members` in the root `Cargo.toml`.

Resulting dependency / publish order:

```
musefs-db → musefs-format → musefs-core → musefs-fuse → musefs-cli → musefs
```

## 2. Make path dependencies publishable

crates.io rejects path-only dependencies. Add a `version` alongside every
`path` inter-crate dependency, tracking the workspace version `0.2.0`:

```toml
musefs-db     = { path = "../musefs-db",     version = "0.2.0" }
musefs-format = { path = "../musefs-format", version = "0.2.0" }
musefs-core   = { path = "../musefs-core",   version = "0.2.0" }
musefs-fuse   = { path = "../musefs-fuse",   version = "0.2.0" }
musefs-cli    = { path = "../musefs-cli",    version = "0.2.0" }
```

Applied wherever each appears as a non-dev dependency (per the current
manifests):

- `musefs-format` → no inter-crate deps (only `thiserror`, `id3`, `base64`)
- `musefs-core` → `musefs-db`, `musefs-format`
- `musefs-fuse` → `musefs-core`
- `musefs-cli` → `musefs-db`, `musefs-core`, `musefs-fuse`
- `musefs` (new) → `musefs-cli`

`dev-dependencies` on path crates do not need a `version` for publishing, but
adding one is harmless; keep changes minimal and only touch what publishing
requires.

## 3. Metadata polish

- The `musefs` wrapper crate gets `description`, `keywords`, `categories`, and
  `readme = "../README.md"` so crates.io renders the project README on the
  install page. `license` and `repository` inherit from the workspace.
- Lower crates already carry `description`/`license`/`repository`; leave them
  as-is apart from the version additions in §2. No further metadata churn.

## 4. Release workflow — `.github/workflows/release.yml`

Follows existing CI conventions: pinned action SHAs (reuse the SHAs already in
`ci.yml`), `persist-credentials: false`, `fuse3 libfuse3-dev pkg-config`
install, `dtolnay/rust-toolchain`, `Swatinem/rust-cache`.

- **Trigger:** `on: push: tags: ['v*']`.
- **Permissions:** `contents: read`.
- **Concurrency:** group on the ref to avoid double-publish.
- **Steps:**
  1. Checkout, install FUSE build deps + pkg-config, install Rust toolchain,
     restore cache.
  2. **Version guard:** extract the workspace `version` from the root
     `Cargo.toml` and assert it equals the tag without its leading `v`
     (e.g. tag `v0.2.0` ⇒ workspace `0.2.0`). Fail the job on mismatch so a
     mistagged release cannot publish.
  3. Publish each crate in dependency order with `cargo publish -p <crate>
     --locked`, in the order from §1, with `CARGO_REGISTRY_TOKEN` exported as a
     step environment variable (`cargo publish` reads it automatically — no
     `--token` flag). Modern `cargo publish` blocks until the just-published
     version is available in the registry index before returning, so each
     subsequent crate's version requirement resolves without manual sleeps.

- **Prerequisite (manual, one-time):** add a `CARGO_REGISTRY_TOKEN` repository
  secret (Settings → Secrets and variables → Actions) holding a crates.io API
  token with publish scope. Documented in the spec and README.

Out of scope: OIDC trusted publishing, auto-created GitHub Release objects, and
prebuilt cross-platform binaries. Token-on-tag publishing only.

## 5. Documentation

- **README install section:** replace
  `cargo install --git https://github.com/Sohex/musefs musefs-cli` with:
  - Primary: `cargo install musefs` (from crates.io).
  - A short **prerequisites** note: `cargo install musefs` compiles from source,
    so the machine needs FUSE (`libfuse3` / `libfuse3-dev`) and `pkg-config`
    (Linux), the same as a local `cargo build`.
  - A `--git` fallback updated to the `musefs` crate:
    `cargo install --git https://github.com/Sohex/musefs musefs`.
- **CHANGELOG:** add an entry under Unreleased noting crates.io publication and
  `cargo install musefs`.

## 6. Testing / verification

- `cargo build --workspace` produces exactly one `musefs` binary.
- `cargo install --path musefs` builds and installs the binary locally.
- `cargo package -p <crate>` for each crate confirms the package assembles.
  Caveat: a full `cargo publish --dry-run` for crates above `musefs-db` can only
  fully resolve once the lower crates are actually on crates.io; end-to-end
  publish resolution is therefore first exercised by the initial tagged release
  run. This is expected and called out so it is not mistaken for a defect.
- `cargo fmt --all -- --check`, `cargo clippy --all-targets -- -D warnings`, and
  `cargo test --workspace` stay green. The wrapper move changes packaging, not
  behavior.

## Risks / notes

- **crates.io name availability:** `musefs` (and the `musefs-*` crate names)
  must be free on crates.io. If `musefs` is taken, fall back to a different
  published name and adjust the install command — this would reopen the naming
  decision.
- **First publish is irreversible per version:** crates.io does not allow
  re-publishing the same version. The version guard reduces the chance of an
  accidental publish; the first tagged run should be reviewed carefully.
