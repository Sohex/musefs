# PR 8 CI And Development Hardening Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Pin all GitHub Actions to commit SHAs, disable checkout credential persistence, add a pure CLI mount-config seam, and make pre-commit run the local test gate.

**Architecture:** This PR must be rebased after PR 1 and PR 7 so it sees every workflow file, including `coverage.yml` and the Beets CI job. Workflow hardening should be mechanical; CLI test seam stays in `musefs-cli`.

**Tech Stack:** GitHub Actions, shell hook, Rust CLI tests.

---

### Task 1: Harden Every Workflow Present After Rebase

**Files:**
- Modify: `.github/workflows/*.yml`

- [ ] **Step 1: List all action uses**

Run:

```bash
rg -n "uses: .+@" .github/workflows
```

Expected: includes `ci.yml`, `fuzz.yml`, `audit.yml`, `coverage.yml`, and any
workflow additions from earlier PRs.

- [ ] **Step 2: Pin all actions**

Replace every mutable tag (`@v4`, `@v5`, `@stable`, `@nightly`, `@v2`) with a
full 40-character commit SHA for that action. Verify each SHA exists upstream
before committing.

- [ ] **Step 3: Disable checkout credential persistence**

Every `actions/checkout@<sha>` step must include:

```yaml
with:
  persist-credentials: false
```

Preserve existing checkout `with` keys by merging this key into the existing
mapping.

- [ ] **Step 4: Verify workflow hardening**

Run:

```bash
rg -n "uses: .+@(v[0-9]+|stable|nightly|main|master)$" .github/workflows
rg -n "actions/checkout@" .github/workflows
```

Expected: first command has no output. Manually confirm every checkout block has
`persist-credentials: false`.

### Task 2: Add CLI Mount Config Test Seam

**Files:**
- Modify: `musefs-cli/src/lib.rs`
- Test: `musefs-cli/tests/cli.rs`

- [ ] **Step 1: Extract pure config builder**

Create:

```rust
#[allow(clippy::too_many_arguments)]
pub fn parse_mount_config(
    template: String,
    default_fallback: String,
    mode: musefs_core::Mode,
    poll_interval_ms: u64,
    attr_ttl_ms: u64,
    max_readahead_kib: u32,
    max_background: u16,
    keep_cache: bool,
) -> (MountConfig, musefs_fuse::FuseConfig) {
    let config = MountConfig {
        template,
        fallbacks: BTreeMap::new(),
        default_fallback,
        mode,
        poll_interval: Duration::from_millis(poll_interval_ms),
    };
    let fuse_config = musefs_fuse::FuseConfig {
        ttl: Duration::from_millis(attr_ttl_ms),
        max_readahead: max_readahead_kib.saturating_mul(1024),
        max_background,
        keep_cache,
    };
    (config, fuse_config)
}
```

Have `run_mount` call this function. Use `saturating_mul` for KiB to bytes.

- [ ] **Step 2: Add tests**

Add tests for:
- template/default fallback/mode/poll interval mapping;
- TTL, max background, keep-cache mapping;
- `u32::MAX` readahead saturates instead of wrapping.

- [ ] **Step 3: Verify CLI tests**

Run:

```bash
cargo test -p musefs-cli
```

### Task 3: Add Local Pre-Commit Test Gate

**Files:**
- Modify: `.githooks/pre-commit`
- Modify: README or contributor docs if hook behavior is documented there

- [ ] **Step 1: Update hook**

Use:

```sh
#!/bin/sh
# Pre-commit hook for musefs.
# Enable with: git config core.hooksPath .githooks
set -e

echo "pre-commit: cargo fmt --check"
cargo fmt --all -- --check

echo "pre-commit: cargo clippy --all-targets -- -D warnings"
cargo clippy --all-targets --quiet -- -D warnings

echo "pre-commit: cargo test --workspace"
cargo test --workspace --quiet

echo "pre-commit checks passed"
```

- [ ] **Step 2: Verify hook command**

Run:

```bash
.githooks/pre-commit
```

Expected: fmt, clippy, and workspace tests pass.

- [ ] **Step 3: Commit**

```bash
git add .github/workflows .githooks/pre-commit musefs-cli/src/lib.rs musefs-cli/tests/cli.rs README.md
git commit -m "ci: harden workflows and local test gate

Closes #2
Closes #18
Closes #20"
```
