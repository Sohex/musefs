# Coverage

Coverage uses `cargo-llvm-cov` and uploads to Codecov.

## Local Usage

```bash
cargo install cargo-llvm-cov
cargo llvm-cov --workspace --exclude musefs-fuse --open
cargo llvm-cov --workspace --exclude musefs-fuse --lcov --output-path lcov.info
```

FUSE e2e tests are excluded because they require a real mount at `/dev/fuse`.
They are covered by the separate `e2e` CI job.

## CI Setup

The `coverage.yml` workflow runs on every push/PR.

### Required Secrets

- `CODECOV_TOKEN` — Repository secret from codecov.io.

### Why cargo-llvm-cov?

- No recompilation or instrumentation wrappers needed
- Works with Rust's built-in `-C instrument-coverage`
- Handles workspaces and proc-macros correctly
- Single binary, fast execution
