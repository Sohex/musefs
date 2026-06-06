# Integer cast convention (issue #133)

**Date:** 2026-06-06
**Issue:** [#133](https://github.com/Sohex/musefs/issues/133) — No convention for
integer conversions: ~300 bare `as` casts

## Problem

`musefs-core` and `musefs-format` (and the rest of the workspace) contain
hundreds of bare `as` integer casts and almost no `try_from`. `as` silently
truncates/wraps, which matters at every i64↔u64 boundary and on 32-bit
targets. The workspace `Cargo.toml` currently blanket-allows the four clippy
cast lints with a "casts are deliberate" comment — a stance, but not a
convention anything enforces per-site.

Measured baseline (clippy with the four lints force-enabled, 64-bit host):
**451 violations** — 409 cast warnings + 42 `cast_lossless` — of which ~190
are in tests/benches fixture code. Dominant classes:

| Class | Count | Disposition |
| --- | --- | --- |
| `u64 → usize` | 150 | sanctioned helper (64-bit guard) |
| `usize → u32` | 117 | genuine narrowing; mostly test fixtures |
| `usize → i64` | 50 | db writes — mostly dissolved by db type change |
| `usize → u8` | 29 | test fixtures |
| `u64 → u32` | 22 | narrowing; individual judgment |
| `u64 → i64` / `i64 → u64`-family | 37 | db boundary — dissolved by db type change |
| `u32/u64 → u8` byte extraction | 4 | restructure (`to_be_bytes`) or `#[expect]` |
| `cast_lossless` (widening via `as`) | 42 | mechanical `From` fixes |

## Decisions taken

1. **64-bit only, declared.** musefs supports 64-bit targets only, enforced by
   a compile-time guard (`const _: () = assert!(size_of::<usize>() == 8, ...)`).
   This makes usize↔u64 lossless by construction.
2. **Convention + lints + full migration.** All four cast lints flip from
   `allow` to `warn` in `[workspace.lints.clippy]`; CI's `-D warnings` makes
   them hard gates. All existing violations are migrated — the lints then
   enforce the convention permanently.
3. **Type-driven at the db boundary.** Non-negative quantities in
   `musefs-db` row structs become unsigned; i64 is confined to the db layer.
   Rationale: the SQLite store is the declared interface external tools
   (beets/Picard) write to out-of-band, so row values are untrusted input.
   Today a negative `audio_offset` wraps via `as u64` into a huge offset
   (guarded only by one hand-rolled check at `musefs-core/src/reader.rs:154`).
   rusqlite 0.40 already ships checked conversions — `FromSql for u64`
   errors `OutOfRange` on negative values, `ToSql for u64` is fallible above
   `i64::MAX` — so validation happens once at the row boundary through the
   existing `rusqlite::Error → DbError → CoreError` chain. No new error
   variants.

## The convention

Replaces the "casts are deliberate" comment block in `Cargo.toml`; a short
version goes into CLAUDE.md's Conventions section.

1. **Widening** (`u8/u16/u32` → wider): use `From` (`u64::from(x)`), never
   `as`. Enforced by `cast_lossless`.
2. **usize↔u64**: 64-bit-only is declared by the compile-time guard.
   `usize as u64` is fine (clippy-clean on supported targets). `u64 → usize`
   goes through the sanctioned helper `convert::usize_from(u64)` — the one
   place a pointer-width truncation `#[expect]` lives.
3. **Genuine narrowing** (`u64→u32`, `usize→u32`, `→u8`): prefer
   restructuring (`to_be_bytes`/`from_be_bytes`, indexing); else `try_from` —
   propagated with `?` where the value is input-dependent (parser/file data),
   `.expect()` in tests and fixture builders where values are
   literal-bounded.
4. **i64 never crosses the db boundary upward** for non-negative quantities.
   Row structs expose unsigned types; rusqlite does the checked conversion at
   both ends.
5. **Deliberate bit-truncation** keeps `as` with an inline
   `#[expect(clippy::..., reason = "...")]` — expected to be rare once byte
   extraction moves to `to_be_bytes`.

## Component changes

### `musefs-db`

- **`models.rs` type flips** (SQLite schema, migrations, and the generated
  Python `schema.py` are untouched — storage stays `INTEGER`; only Rust-side
  types change):
  - `Track`/`NewTrack`: `audio_offset`, `audio_length`, `backing_size` →
    `u64`. **Stay i64:** `backing_mtime` (pre-1970 mtimes are legal),
    `updated_at` (trigger-set timestamp), `id` (rowid), `content_version`
    (bookkeeping counter).
  - `Art`/`ArtMeta`/`BinaryTagRow`: `byte_len` → `u64`; `width`/`height` →
    `u32` (Option-ness unchanged).
  - `Tag`/`TrackArt`/`BinaryTag`: `ordinal` → `u64` (write side becomes a
    clippy-clean `usize as u64`); `picture_type` → `u8` (APIC type byte).
- **New `convert.rs`**: `pub fn usize_from(v: u64) -> usize` containing the
  workspace's only pointer-width `#[expect(clippy::cast_possible_truncation)]`
  (latencyfs, standalone, carries its own), adjacent to the 64-bit const
  guard. db hosts it because it is the workspace's base
  crate and needs it itself (`art.rs:62`, `tags.rs:116`).

### `musefs-core`

- Re-export the helper (`pub use musefs_db::convert;`) so `musefs-fuse`
  (which depends only on core) can reach it without a new dependency edge.
- The manual negative-bounds check at `reader.rs:154-156` dissolves into the
  type system; `track.audio_offset as u64` reads and `as i64` writes in
  `reader.rs`/`scan.rs`/`mapping.rs` disappear.

### `musefs-format`

- Parser-internal narrowings get individual judgment: already-bounds-checked
  values keep a justified path (restructure or helper); input-dependent ones
  get `try_from` + `?` into the existing per-format error.

### `musefs-fuse`, `musefs-cli`

- A handful of sites; use the re-exported helper / `try_from` per the
  convention.
- One special case: `attr.mtime_secs as u64` (`musefs-fuse/src/lib.rs:111`)
  is a real sign-loss on a legitimately-negative value (pre-1970 mtime wraps
  to a far-future date today). Fixing that display behavior is out of scope —
  the site keeps `as` with an `#[expect(clippy::cast_sign_loss, reason)]`
  documenting the pre-existing limitation.

### `musefs-latencyfs`

- Standalone by design (no musefs deps) — stays that way. Its ~10 sites are
  handled locally: fuser attr-struct `u64 as u32` fields fed from test
  fixtures get `try_from(...).expect(...)` or a local `#[expect]` with
  reason, plus its own one-line guard for its single `u64 → usize`.

### Workspace `Cargo.toml`

- `cast_possible_truncation`, `cast_sign_loss`, `cast_possible_wrap`,
  `cast_lossless` flip from `allow` to `warn`. Lands **last**, once the
  workspace is warning-clean.

## Migration order

Dependency order so the tree compiles at each step:

1. db model type flips + `convert.rs`.
2. Consumers: core, fuse, cli — including tests/ and benches/ (the hidden
   `--all-targets` consumers).
3. format parser-internal narrowings.
4. latencyfs local fixes.
5. Lint flip in workspace `Cargo.toml` + convention comment + CLAUDE.md note.

The ~190 test/bench fixture sites are mechanical
(`u32::try_from(len).unwrap()` or typed literals).

## Behavioral changes

Exactly one intended: a corrupt row (negative offset/length/byte_len written
by an external tool) now errors at row-read (`FromSqlError::OutOfRange`)
instead of relying on the single hand-rolled check in `reader.rs`. Everything
else — including the byte-identical serve-path invariant — must be unchanged.

## Validation

- `cargo clippy --all-targets -- -D warnings` clean, **and** the
  `-p musefs-db --features mutants --all-targets` variant (both CI gates) —
  the enforcement proof.
- Full test suite + proptests (`cargo test`, `cargo test -p musefs-format`,
  `cargo test -p musefs-core --test proptest_read_fidelity`).
- `cargo +nightly fuzz build` for all targets — the fuzz crate is
  out-of-workspace and otherwise only breaks in CI's smoke job.
- In-diff mutation gate (`-j2`, output on `/tmp`, non-empty-diff sanity
  check). Known risk: a large mechanical diff makes the run long;
  `usize_from` is a mutation target whose kills come from existing read-path
  tests.
- `cargo fmt --all --check` before push.

## Out of scope

- SQLite schema or migration changes (none needed).
- Python plugin / `schema.py` changes (schema unchanged).
- Any behavioral change beyond the corrupt-row read error above.
- 32-bit target support (explicitly declared unsupported instead).
