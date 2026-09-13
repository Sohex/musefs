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
- **`#[non_exhaustive]`.** Mark a public enum or struct when every consumer in
  another crate can take a new variant or field safely: errors and results, and
  configuration structs callers build from `default()`. Leave it exhaustive when
  another crate matches or builds it member by member on a path where a new one
  must be wired deliberately — `Segment`, the store's write inputs — and say so
  on the type. If marking takes away an exhaustive check the workspace relied
  on across a crate boundary, add a stand-in (as
  `a_new_format_must_be_wired_into_the_dispatch` does). `tests/` and `benches/`
  count as other crates.
- **Schema migrations.** Append to `MIGRATIONS` in `musefs-db/src/schema.rs`;
  each entry carries its SQL, the release that introduced it (`since`), a
  one-line `summary` the `migrate` command prints, and a `Gate`. **A `Gated`
  step may only be introduced by a major release** — a `const` assertion
  rejects the build otherwise. Gate a step that rewrites data the user did not
  ask to have rewritten, needs the store's size again in free disk, or ends
  compatibility with older binaries; everything else is `Transparent` and is
  applied by any open. See
  [the store](../architecture/store.md#transparent-and-gated-migrations).

## Adding a format

1. Implement probe + `synthesize_layout` in `musefs-format` (mirror an
   existing module — `flac.rs`, `mp3.rs`, `mp4.rs`, `ogg/`, `wav.rs`),
   returning a `RegionLayout`.
2. Add the variant to `musefs-db`'s `Format` enum, then wire it into the
   `match track.format` arms in `reader::HeaderCache::resolve`
   (`musefs-core/src/reader.rs`) and into `scan.rs` (extension list, probe
   dispatch). `Format` is `#[non_exhaustive]`, so the compiler will not point
   at a missing arm in another crate. `a_new_format_must_be_wired_into_the_dispatch`
   (in `reader.rs`) fails instead; it only sees its own list of variants, so
   add the new one there once the arm exists.
3. Extend the test surface: a `fuzz_check::fixtures::<fmt>()` minimal file,
   a `fuzz/fuzz_targets/<fmt>.rs` target with a seed in `generate_seeds`, a
   `musefs-format/tests/proptest_<fmt>.rs`, and a manifest row in
   `musefs-core/tests/interop_emit.rs`.
4. Write `docs/<FMT>.md` (follow the shape of the existing five).
