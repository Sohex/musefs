# Edition 2024 bump — design

**Date:** 2026-06-08
**Status:** Approved, ready for implementation planning
**Scope:** PR 1 of a two-PR sequence. This PR performs the mechanical migration
to Rust edition 2024. A follow-up PR 2 (separately brainstormed) applies the
ergonomic cleanups the edition unlocks; PR 1 only *inventories* them.

## Goal

Move the musefs workspace from edition 2021 to edition 2024 with no behavioral
change, keeping every gate green (the pre-commit hook runs the full workspace
suite, so the bump commit must be green). Produce a written inventory of
edition-2024-enabled improvements to seed PR 2.

## Background / why this is small

Reconnaissance shows the migration is nearly mechanical:

- **No `unsafe`, no `static mut`, no `extern` blocks.** The headline 2024
  footgun-removals (`unsafe extern`, `#[unsafe(...)]` attributes,
  `unsafe_op_in_unsafe_fn`, `&` to `static mut`) do not apply — consistent with
  the no-unsafe posture established in #168.
- **One `-> impl` return** across the codebase → negligible RPIT
  lifetime-capture (`+ use<>`) migration risk.
- **No `rust-version`/MSRV commitment**; CI runs on default `stable` (1.96
  locally), well past the 1.85 that edition 2024 requires. The bump is
  mechanically safe.

Only two code-touching issues exist, both required for the edition to compile.

## What changes

### 1. Edition declaration (2 files)

- `Cargo.toml` → `[workspace.package] edition = "2024"`. All 7 workspace crates
  (`musefs-db`, `musefs-format`, `musefs-core`, `musefs-fuse`, `musefs-cli`,
  `musefs`, `musefs-latencyfs`) inherit via `edition.workspace = true`.
- `fuzz/Cargo.toml` → `edition = "2024"`. The `fuzz` crate is excluded from the
  workspace, so it is bumped independently.

### 2. Forced fixup A — the `gen` reserved keyword

`gen` becomes a reserved keyword in edition 2024. It is used as an identifier in
~12 sites:

- `musefs-core/src/facade.rs`: a `gen: AtomicU64` field on the facade handle
  struct plus local bindings reading/writing it.
- `musefs-core/src/tree.rs`: `for gen in 0..N` test loop variables and
  format-string interpolations of `gen`.

**Decision:** rename the identifiers to readable names (e.g. `gen` →
`generation`) rather than papering over them with `r#gen` raw-identifier cruft.
The struct field rename goes through `find_referencing_symbols` to update every
use; the test loop variables are local renames. Pure rename — no behavioral
change.

**Catch the interpolations:** `find_referencing_symbols` will not see `gen`
inside `format!("Gen{gen}…")` interpolations or doc comments in `tree.rs`
(≈ lines 903–944), and those break the build silently if missed. After the
symbolic rename, run a textual sweep (`grep -rnw gen`) over the touched files
to confirm zero remaining bare-`gen` identifier occurrences.

### 3. Forced fixup B — `std::env::set_var` / `remove_var` become `unsafe`

Edition 2024 marks `std::env::set_var` and `std::env::remove_var` as `unsafe fn`
(process-global mutation is unsound in the presence of concurrent readers). The
workspace **denies `unsafe_code`**, including in tests (`--all-targets`).

The 15 call sites (6 `set_var` + 9 `remove_var`) live in
`musefs-core/tests/common_corpus_smoke.rs`, which
deliberately mutates the `MUSEFS_BENCH_*` environment to exercise the
config-from-env path in `tests/common/corpus.rs`. Those env vars are the bench
harness's established shell-driven config protocol (read across `corpus.rs`,
`bench_ingest.rs`, `bench_refresh.rs`, and the `read_throughput` bench), so
redesigning the smoke test to avoid process-env mutation is out of scope for an
edition bump.

**Decision:** concentrate the now-`unsafe` calls into a single audited test
helper rather than scattering `unsafe` across 15 sites.

- Add the helper **in `common_corpus_smoke.rs` itself**, not in the shared
  `tests/common/` module. The 15 mutation sites and the env-lock `static` are
  already file-private to the smoke test, and `tests/common/corpus.rs` only
  *reads* env. Putting the helper (and its `#[expect(unsafe_code)]`) in shared
  `common/` would spread the attribute into the ~25 test binaries that include
  the module and risk an unfulfilled-`expect` failure under `-D warnings` in
  binaries that never call it.
- The helper — e.g. `set_env(key, val)` and `remove_env(key)` — wraps the
  `unsafe { std::env::set_var(...) }` / `remove_var` in **one** place under a
  single, scope-minimal `#[expect(unsafe_code, reason = "test-only env
  mutation, serialized by the env lock; std marked env mutation unsafe in
  edition 2024")]` on the `unsafe` block.
- The safety precondition (no concurrent env access) is already met: every
  env-touching test acquires `ENV_LOCK` first. The helper documents this.
- Replace the 15 `std::env::set_var`/`remove_var` call sites with calls to the
  helper.

This is exactly the per-site opt-in the workspace lint comment prescribes for
"a genuinely-necessary unsafe", keeps the `deny` lint meaningful everywhere
else, and is the only `unsafe` reintroduced.

### 4. Catch-all migration pass

**Sequencing:** apply the `gen` rename (fixup A) and the env helper (fixup B)
**before** running `cargo fix --edition`. If `cargo fix` runs first against the
raw env calls, it auto-inserts 15 separate `unsafe { … }` blocks that the
`deny(unsafe_code)` lint then rejects — wasted churn. Hand-fix A and B, then run
`cargo fix --edition` to catch anything residual.

Run `cargo fix --edition`, then review its output by hand:

- Keep any genuinely-needed `+ use<>` precise-capture bound on the single
  `-> impl` return.
- Watch for never-type fallback (`!` → `()`) and `if let` tail-temporary-scope
  changes flagged by the migration; address only what the compiler reports.
- **Revert purely-cosmetic churn** so the final diff stays mechanical and
  reviewable (declaration + `gen` rename + env helper + any real compiler-forced
  fix, nothing else).

### 5. Docs

Update the two live docs that name the edition; leave historical
specs/plans under `docs/superpowers/` untouched (frozen records):

- `CONTRIBUTING.md:9` — "edition 2021" → "edition 2024".
- `README.md:154` — "Rust (2021 edition)" → "Rust (2024 edition)".

## Verification

All must be green (pre-commit hook runs the full workspace suite):

- `cargo build`
- `cargo test` (full workspace)
- `cargo test --doc` (doctests — run explicitly; do not assume the pre-commit
  invocation covers them)
- `cargo clippy --all-targets -- -D warnings`
- `cargo clippy --all-targets --all-features -- -D warnings` (covers the
  non-default feature-gated code: `metrics` in core/fuse, `fuzzing` in format,
  `mutants` in db — the edition change applies to gated code too)
- `cargo build --no-default-features` (the other end of the feature surface)
- `cargo fmt --all --check`
- FreeBSD cross-clippy: `cargo clippy --all-targets --target x86_64-unknown-freebsd`
  (the project's only non-Linux lint gate)
- `cargo +nightly fuzz build` (confirm the independently-bumped fuzz crate
  still builds)

FUSE e2e (`--ignored`, sudo-gated) is unaffected by an edition bump and is not
required to change.

## Deliverable for PR 2 — improvement-candidate inventory

During/after the migration, write a notes file to
`docs/superpowers/specs/2026-06-08-edition-2024-improvements-notes.md` cataloging
the edition-2024-enabled cleanups, **without applying any of them**:

- **Let-chains** (the primary win — `if let … && let … && cond`, stable only
  under edition 2024). Survey the ~103 `if let` sites for nested ladders that
  collapse; list concrete file:line candidates with a one-line before/after
  sketch.
- Any `+ use<>` precise-capture simplifications or never-type ergonomics noticed
  in passing.

This notes file is the input to PR 2's own brainstorm/spec and is superseded by
that spec.

## Out of scope

- Applying any ergonomic cleanup (let-chains etc.) — that is PR 2.
- Redesigning the bench `MUSEFS_BENCH_*` env config protocol.
- Any `rust-version`/MSRV policy addition.
