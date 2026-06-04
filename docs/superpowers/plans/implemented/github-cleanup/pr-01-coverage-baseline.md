# PR 1 Coverage Baseline Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a dedicated Rust coverage workflow and coverage documentation for issue #21.

**Architecture:** This PR only adds coverage collection. It deliberately does not pin workflow actions; PR 8 will rebase later and harden every workflow present at that point, including this one.

**Tech Stack:** GitHub Actions, cargo-llvm-cov, Codecov, Rust workspace.

---

### Task 1: Coverage Workflow And Docs

**Files:**
- Create: `.github/workflows/coverage.yml`
- Create: `docs/COVERAGE.md`

- [ ] **Step 1: Add the coverage workflow**

Create `.github/workflows/coverage.yml`:

```yaml
name: coverage

on:
  push:
    branches: [main]
  pull_request:

concurrency:
  group: coverage-${{ github.ref }}
  cancel-in-progress: true

permissions:
  contents: read

env:
  CARGO_TERM_COLOR: always

jobs:
  coverage:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - name: Install libfuse3
        run: sudo apt-get update && sudo apt-get install -y fuse3 libfuse3-dev pkg-config
      - uses: Swatinem/rust-cache@v2
      - name: Install Rust
        uses: dtolnay/rust-toolchain@stable
        with:
          components: llvm-tools-preview
      - name: Install cargo-llvm-cov
        uses: taiki-e/install-action@v2
        with:
          tool: cargo-llvm-cov
      - name: Collect coverage
        run: cargo llvm-cov --workspace --exclude musefs-fuse --lcov --output-path lcov.info
      - name: Upload to Codecov
        uses: codecov/codecov-action@v5
        with:
          files: lcov.info
          fail_ci_if_error: true
          token: ${{ secrets.CODECOV_TOKEN }}
```

- [ ] **Step 2: Add coverage documentation**

Create `docs/COVERAGE.md`:

```markdown
# Coverage

Coverage uses `cargo-llvm-cov` and uploads LCOV data to Codecov.

## Local Usage

```bash
cargo install cargo-llvm-cov
cargo llvm-cov --workspace --exclude musefs-fuse --lcov --output-path lcov.info
```

`musefs-fuse` is excluded from the default coverage job because its ignored
end-to-end tests require `/dev/fuse` and a real FUSE environment. Run those
separately when needed:

```bash
cargo test -p musefs-fuse -- --ignored
```

## CI Setup

Set `CODECOV_TOKEN` as a repository secret if Codecov requires token-based
upload for this repository. The workflow fails if upload fails.
```

- [ ] **Step 3: Verify locally**

Run:

```bash
cargo llvm-cov --workspace --exclude musefs-fuse --lcov --output-path /tmp/musefs-lcov.info
```

Expected: command exits successfully and `/tmp/musefs-lcov.info` exists. If
`cargo-llvm-cov` is not installed, install it with `cargo install cargo-llvm-cov`
or record that local coverage verification was skipped because the tool is not
installed.

- [ ] **Step 4: Commit**

```bash
git add .github/workflows/coverage.yml docs/COVERAGE.md
git commit -m "ci: add coverage baseline with cargo-llvm-cov and Codecov

Closes #21"
```
