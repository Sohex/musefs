# Repository Guidelines

## Project Structure & Module Organization

musefs is a Rust 2021 Cargo workspace. Crates are layered: `musefs-db` owns SQLite schema and access, `musefs-format` handles FLAC/MP3/MP4/Ogg/WAV metadata synthesis and layouts, `musefs-core` orchestrates scanning, virtual trees, and reads, `musefs-fuse` is the FUSE adapter, and `musefs-cli` provides the `musefs` binary. Integration tests live in each crate's `tests/`; shared helpers are usually under `tests/common/`. Fuzzing is outside the workspace in `fuzz/`. Architecture notes and plans live under `docs/`, especially `docs/ROADMAP.md` and `CLAUDE.md`.

## Build, Test, and Development Commands

- `cargo build` builds the full workspace.
- `cargo build --release` builds the optimized CLI binary.
- `cargo run -p musefs-cli -- scan <dir> --db <db>` runs the scanner locally.
- `cargo run -p musefs-cli -- mount <mountpoint> --db <db>` mounts a read-only view; Linux FUSE support is required.
- `cargo test` runs normal unit, integration, and property tests.
- `cargo test -p musefs-core read_at` runs tests matching a substring in one crate.
- `cargo test -p musefs-fuse -- --ignored` runs real mount end-to-end tests; requires `/dev/fuse` and libfuse.
- `cargo clippy --all-targets -- -D warnings` and `cargo fmt --check` match pre-commit expectations.

## Coding Style & Naming Conventions

Use `rustfmt` and Rust naming conventions: `snake_case` for functions/modules, `PascalCase` for types, and `SCREAMING_SNAKE_CASE` for constants. Workspace Clippy policy is in the root `Cargo.toml`: `clippy::pedantic` is enabled with documented allowances. Keep `musefs-fuse` and `musefs-cli` thin; put cross-cutting behavior in `musefs-core`. Preserve the central invariant: original audio bytes are never modified.

## Testing Guidelines

Add focused unit tests near code for small logic and integration tests under `<crate>/tests/` for cross-module behavior. Property tests use `proptest`; format-layer tests using fuzz fixtures run with `cargo test -p musefs-format --features fuzzing`. Fuzz targets require nightly, for example `cargo +nightly fuzz run mp3`. Name tests by behavior, such as `read_at_spans_inline_and_backing_segments`.

## Commit & Pull Request Guidelines

Recent history uses conventional-style subjects such as `fix(format): ...`, `test(format): ...`, `docs: ...`, `ci: ...`, and `build(fuzz): ...`. Keep commits scoped and imperative. Pull requests should summarize behavior changes, list tests run, link relevant issues or roadmap items, and include screenshots or mount logs only when CLI/FUSE behavior changes. Enable hooks with `git config core.hooksPath .githooks`.

## Agent-Specific Notes

Read `CLAUDE.md` before changing read, synthesis, scan, cache, or schema behavior. Do not relax the byte-preservation invariant without an explicit design update.
