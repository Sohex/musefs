# Format via strum and Db read/write typestate

**Date:** 2026-06-04
**Issues:** #129 — `Format` round-trips through hand-written stringly
`as_str`/`parse`; #130 — read-only vs writable DB connections are not
distinguished at the type level
**Status:** Approved

Two independent fixes covered by one spec, shipped as **two separate PRs**
with no ordering dependency (#129 touches `models.rs` and three call sites;
#130 restructures `impl Db` blocks across `musefs-db` and serve-path
signatures in `musefs-core`).

Issue #135 (`backing_path` byte-wise uniqueness) was triaged alongside these
and closed with a decision, not a change: musefs supports Linux only, so
backing filesystems are treated as case-sensitive and the BINARY collation is
intended behavior.

## Part 1 — #129: derive the `Format` string mapping with strum (PR 1)

### Problem

`musefs-db/src/models.rs` maps `Format` to and from its DB text
representation via a hand-written `as_str()`/`parse()` pair. The mapping is a
serialization concern maintained by hand in two directions: `as_str`'s match
is exhaustive (the compiler forces a new arm when a variant is added), but
`parse` silently stays incomplete, and nothing verifies the pair remains
symmetric.

### Design

Add `strum` (with the `derive` feature) as a dependency of `musefs-db` and
derive the mapping:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, EnumString, IntoStaticStr, EnumIter)]
#[strum(serialize_all = "lowercase")]
pub enum Format { Flac, Mp3, M4a, Opus, Vorbis, OggFlac, Wav }
```

`serialize_all = "lowercase"` yields exactly the current DB strings,
including `OggFlac` → `"oggflac"`.

- **`as_str()` stays** as a one-line inherent convenience delegating to the
  derived `IntoStaticStr` (`<&'static str>::from(self)`), so the existing
  call sites (`tracks.rs:61`, `bulk.rs:59`, `musefs-core/src/facade.rs:228`)
  do not change.
- **Hand-written `parse()` is deleted.** Its one production call site — the
  `parse_format_col` row-mapping helper at `tracks.rs:11` — switches to the
  derived `FromStr` via `fmt.parse::<Format>().ok()`, keeping its existing
  `.ok_or_else` mapping to `rusqlite::Error::FromSqlConversionFailure` with
  the `"unknown format {fmt}"` message unchanged.
- The `mutants`-feature `Default` derive on `Format` is untouched.

### Testing

The three per-format round-trip unit tests in `models.rs` collapse into two:

1. **Exhaustive round-trip:** `for f in Format::iter()`, assert
   `f.as_str().parse::<Format>() == Ok(f)`. Symmetry holds by construction
   for any future variant — adding one cannot dodge this test.
2. **String pinning:** assert the full explicit `(variant, string)` table.
   The strings are a DB contract (external writers — beets/Picard — store
   them); with `serialize_all`, a careless variant rename would silently
   change the stored string. This test makes that a loud failure.

## Part 2 — #130: `Db<Mode>` typestate (PR 2)

### Problem

`musefs_db::Db` is a single type returned by both `open()` (writable) and
`open_readonly()`; the capability lives only in a runtime open flag. A write
API called on a read-only connection — or a serving path handed a writable
one — compiles fine and only fails (or silently succeeds) at runtime.

### Design: marker-type generic with a `ReadWrite` default

**musefs-db (`lib.rs`):**

```rust
pub struct ReadOnly;
pub struct ReadWrite;

pub struct Db<M = ReadWrite> {
    conn: Connection,
    path: Option<PathBuf>,
    _mode: PhantomData<M>,
}
```

- `impl Db<ReadWrite>`: `open()`, `open_in_memory()`, `configure()`, and a
  new `into_read_only(self) -> Db<ReadOnly>` that rewraps the same
  connection. Degrading is type-level only; runtime behavior of the
  connection is unchanged.
- `impl Db<ReadOnly>`: `open_readonly()` (now honestly typed).
- `impl<M> Db<M>`: `user_version()`, `data_version()`, `path()`.
- The `mutants`-feature `Default` impl becomes `Default for Db<ReadWrite>`.

**Per-module split** — each `impl Db` block in `tracks.rs`, `tags.rs`,
`art.rs`, `structural.rs`, `bulk.rs` splits into a read block
(`impl<M> Db<M>`) and a write block (`impl Db<ReadWrite>`):

- *Read surface* (`impl<M> Db<M>`): `get_track`, `get_track_by_path`,
  `list_tracks`, `track_content_version`, `begin_read`, `end_read`,
  `list_render_keys`, `changelog_since`, `render_keys_for`, `get_tags`,
  `tags_for_tracks`, `tags_grouped`, `get_binary_tags`,
  `read_binary_tag_chunk_into`, `read_binary_tag_chunk`, `get_art`,
  `get_art_meta`, `read_art_chunk_into`, `read_art_chunk`, `get_track_art`,
  `track_ids_with_structural_blocks`, `get_structural_blocks`.
- *Write surface* (`impl Db<ReadWrite>`): `upsert_track`, `delete_track`,
  `set_format_for_test`, `delete_changelog_through_for_test`,
  `replace_tags`, `set_binary_tags`, `upsert_art`, `set_track_art`,
  `gc_orphan_art`, `set_structural_blocks`, `apply_bulk_pragmas_self`,
  `bulk_writer` (and `BulkWriter` keeps borrowing the writable type).

**musefs-core (`db_pool.rs`):** `DbPool::new(db: Db)` still takes the
writable mount connection but immediately degrades it via
`into_read_only()`:

- `PerThread.poll` becomes `ReentrantMutex<Db<ReadOnly>>`.
- `Shared` becomes `Arc<ReentrantMutex<Db<ReadOnly>>>`.
- The thread-local `PER_PATH` cache holds `Rc<Db<ReadOnly>>` (worker threads
  already open via `open_readonly`).
- `with` hands out `&Db<ReadOnly>`. Every `&Db` parameter downstream in the
  serve path (`reader.rs`, `facade.rs`, `mapping.rs`) becomes
  `&Db<ReadOnly>`, after which the compiler proves the serve path contains
  no write call.

**What does not change:** `Db` defaults to `ReadWrite`, so `scan.rs`,
`musefs-cli`, `Musefs::open(db: Db, ...)`, and every existing test spelling
compile as-is. `error.rs`, the schema/migration machinery, and all runtime
behavior are untouched — the change is purely type-level. The poll
connection keeps its WAL/configure setup from `open()`; only its type
degrades.

### Testing

The existing suites are the regression net — the split compiling against
them is itself the verification that no read path lost a method and no
write path gained one. One addition: a `compile_fail` doctest on
`Db<ReadOnly>` demonstrating that a write API (e.g. `upsert_track`) does not
resolve, pinning the guarantee against future backsliding.
