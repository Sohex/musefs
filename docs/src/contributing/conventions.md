# Conventions & adding a format

## Code conventions

- **Errors.** Each crate has its own `error.rs` with a `thiserror` enum;
  `musefs-core` wraps lower layers in `CoreError`; the CLI is the only
  `anyhow` consumer. Internal error paths never discard diagnostics: no
  `Result<_, ()>`, no `.map_err(|_| …)` that drops a source — each variant
  carries its source (`#[from]`) or a static reason naming the broken
  invariant.
- **Integer conversions.** The four clippy cast lints are deny-via-CI.
  Widenings use `From`; `u64 -> usize` only via the sanctioned `usize_from`
  helpers (`musefs_db::convert`, re-exported by core; `musefs-format` and
  `musefs-latencyfs` carry crate-local siblings — the workspace is declared
  64-bit-only); genuine narrowings use `try_from` (`?` for input-dependent
  values, `.expect` for structurally bounded ones, `.unwrap` in tests);
  deliberate bit-truncation keeps `as` under a reasoned `#[expect]`.
  Non-negative DB row fields are unsigned; rusqlite's checked conversions
  (feature `fallible_uint`) validate at the row boundary.
- **Lint policy.** `clippy::pedantic` minus a few intentional/noisy groups,
  defined in the root `Cargo.toml` under `[workspace.lints]`. The hook and
  CI deny all warnings.
- **Unsafe code.** `unsafe_code = "deny"` is set for the workspace members in
  the root `Cargo.toml` (`[workspace.lints.rust]`); the standalone `fuzz/`
  crate is outside the workspace and is not covered. A genuinely-necessary
  `unsafe` is opted in per-site with `#[expect(unsafe_code, reason = "...")]`
  — never a bare `unsafe` block and never by relaxing the workspace lint, so
  every `unsafe` is greppable and review-visible. Prefer a safe crate (e.g.
  `rustix` for syscalls) over hand-rolled FFI.
- **Layering.** Keep `musefs-fuse`, `musefs-cli`, and the `musefs` binary
  thin; cross-cutting logic belongs in `musefs-core`
  (see [the crate layout](../architecture/overview.md#crate-layout)).
- **Hidden API consumers.** `benches/` directories and each crate's
  `tests/` are compiled only by `--all-targets`: after an API change,
  compile-check with `cargo clippy --all-targets`, not `cargo build`.

## Adding a format

1. Implement probe + `synthesize_layout` in `musefs-format` (mirror an
   existing module — `flac.rs`, `mp3.rs`, `mp4.rs`, `ogg/`, `wav.rs`),
   returning a `RegionLayout`.
2. Add the variant to `musefs-db`'s `Format` enum, then wire it into the
   `match track.format` arms in `reader::HeaderCache::resolve`
   (`musefs-core/src/reader.rs`) and into `scan.rs` (extension list, probe
   dispatch).
3. Extend the test surface: a `fuzz_check::fixtures::<fmt>()` minimal file,
   a `fuzz/fuzz_targets/<fmt>.rs` target with a seed in `generate_seeds`, a
   `musefs-format/tests/proptest_<fmt>.rs`, and a manifest row in
   `musefs-core/tests/interop_emit.rs`.
4. Write `docs/<FMT>.md` (follow the shape of the existing five).
