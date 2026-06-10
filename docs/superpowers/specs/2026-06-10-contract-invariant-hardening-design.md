# Hardening implicit invariants into enforced contracts

Design spec for GitHub issues #188, #199, #200, #201.

## Thesis

Four issues describe the same class of problem at four different boundaries: an
invariant that musefs' Rust/core/format code treats as guaranteed is in fact only
held by **implicit, non-local agreement** — a SQL `ORDER BY` in one crate, a
producer's discipline in another, a query convention, a caller's memory. Each is a
correctness hazard the day someone changes the distant thing the invariant
silently leans on.

The unifying fix: **move enforcement to the boundary where the violation becomes
structurally impossible, or is caught at construction.** A constraint the database
rejects at commit. A type that cannot be built in an invalid state. A layout that
cannot exist unvalidated. A build path that establishes its own determinism rather
than inheriting it.

| Issue | Boundary | Invariant today rests on… | Enforced by |
| ----- | -------- | ------------------------- | ----------- |
| #199 | SQLite ↔ external writer | Rust row readers rejecting bad rows *after* commit | `CHECK` constraints at commit |
| #200 | `db → core → format` (Rust) | Every caller remembering positive lengths / valid metadata | Validated newtypes at construction |
| #201 | `format → core` | Producer discipline calling `validated()` | Unskippable validation; `new` hidden |
| #188 | full-rebuild vs incremental tree build | `list_tracks` carrying `ORDER BY id` | Build path sorting by id itself |

## Scope

In scope: one spec (this document) and **four independent implementation plans**,
each landing green commit-by-commit. The plans are sequenced A → B → C → D; A and B
are independent, C is foundational for D.

- **Plan A — #199**: a new `MIGRATION_V4` adding SQL `CHECK` constraints, Rust
  raw-SQL rejection tests, Python schema-mirror regen.
- **Plan B — #188**: make the full-rebuild tree path establish disambiguation order
  locally instead of inheriting it from SQL.
- **Plan C — #200**: a *pragmatic* validated-newtype subset across the crate
  boundaries, with `TryFrom` at the DB row readers.
- **Plan D — #201**: make `RegionLayout` validity unskippable; consume Plan C's
  length type.

Out of scope (YAGNI):

- **No comprehensive newtype suite.** The full candidate list in #200 (`TrackBounds`,
  `BackingSnapshot`, `BackingPath`, `TagKey`, `OrderedTags`, `TagOrdinal`, `ArtId`,
  `PayloadId`, `BlobLen`, `PictureType`, `MimeType`, `ContentVersion`) is **not**
  adopted wholesale. See Plan C for the decision rule and the lean set; opaque DB
  row handles stay raw.
- **No Python-writer integration tests for #199.** The contract is SQL-level;
  Rust raw-SQL rejection tests cover it fully. No beets-venv / Picard-import test
  surface is added.
- **No quarantine/repair migration path.** There are no live databases in the wild;
  pre-existing constraint violations hard-fail the migration (theoretical in
  practice).
- **No edit to `MIGRATION_V1..V3`.** Migrations are append-only history pinned by
  the schema-mirror and migration tests. #199 is strictly additive (`V4`).
- **No rewrite of the changelog ring, triggers, or freshness machinery.**

## Plan A — #199: DB-level CHECK constraints

### Mechanism

SQLite cannot add a `CHECK` to an existing table in place, so `MIGRATION_V4`
rebuilds `tracks`, `tags`, `art`, and `track_art` via the standard sequence
(create `*_new` with constraints, `INSERT … SELECT` from old, drop old, rename).
The triggers and `structural_blocks` / `track_changes` tables are recreated or
left intact as appropriate so the V1/V3 trigger behavior is preserved across the
rebuild. The `INSERT … SELECT` copy enforces every new `CHECK`, so any
pre-existing violating row aborts the migration with the violated constraint named
— and thus refuses the mount. With no live databases, this path runs only against
fresh, empty tables in practice.

### Constraint set

Limited to invariants SQLite enforces cheaply and that musefs already assumes:

**`tracks`**
- `format IN ('flac','mp3','m4a','opus','vorbis','oggflac','wav')` — the exact
  `Format` serialization strings (pinned by `db_strings_are_pinned`).
- `audio_offset >= 0`, `audio_length >= 0`, `backing_size >= 0`,
  `backing_mtime >= 0`, `content_version >= 0`, `updated_at >= 0`.
- `audio_offset + audio_length <= backing_size` — the read path assumes the audio
  region lies within the backing file. **Risk/verify (see below): confirm every
  scanned format satisfies this before pinning it; if any legitimately does not,
  this constraint is dropped and the fact recorded.**

**`tags`**
- `ordinal >= 0`.
- `value_blob IS NULL OR value = ''` — encodes the V2 binary-vs-text row shape
  (a binary row carries its bytes in `value_blob` and `''` in `value`).

**`art`**
- `byte_len = length(data)` — the stored length must match the blob.
- `length(sha256) = 64` — hex SHA-256 shape.
- `width IS NULL OR width >= 0`, `height IS NULL OR height >= 0`.

**`track_art`**
- `picture_type BETWEEN 0 AND 20` — the ID3/FLAC picture-type range.
- `ordinal >= 0`.

### Tests & follow-through

- Rust tests issue one invalid `INSERT`/`UPDATE` per `CHECK` against a migrated DB
  and assert SQLite rejects it; plus a smoke test that a fully-valid row set
  migrates and reads cleanly.
- Regenerate the Python schema mirror
  (`MUSEFS_REGEN_SCHEMA_PY=1 cargo test -p musefs-db schema_py`) and re-vendor.
- Update ARCHITECTURE.md's external-writer contract section: the listed invariants
  are now DB-enforced, not convention.

## Plan B — #188: disambiguation determinism

### Problem recap

Inode stability for colliding paths depends on disambiguation being deterministic
(which of a colliding pair gets the bare name vs ` (2)`). The incremental rebuild
path defends this itself (min-id rule, plus a Stage-B re-sort), but the
full-rebuild path (`facade.rs::render_entries` → `build_full`) inherits its order
purely from `list_tracks`'s `ORDER BY id`. The two paths agree only by that
coincidence; change the `ORDER BY`, or build `render_entries` from a non-id-ordered
source, and a track's inode flips between rebuild paths.

### Fix

Make the **build path self-defending**: establish the disambiguation order by
sorting entries by track `id` inside the full-rebuild build path itself, so
correctness no longer depends on the SQL clause or on the incremental path's
separate re-sort. The `ORDER BY id` in `list_tracks` stays (harmless, good for
locality) but is demoted from load-bearing to incidental — the spec and the test
comment at `tree.rs:1350-1352` are updated to say so.

Exact insertion point (full path's pre-build sort vs sort inside `build_with_ci`)
is a Plan-B detail; the requirement is that **a single, local point in the
full-rebuild path guarantees id order**, matching the incremental path's
guarantee, independent of any upstream ordering.

### Test

Feed the full-rebuild path deliberately id-unordered entries and assert it
produces the same disambiguation (and thus the same inode assignment) as the
incremental path for a colliding pair.

## Plan C — #200: pragmatic validated newtypes

### Decision rule

Introduce a newtype **only where it eliminates an invalid state a Rust caller
could construct today that compiles** — not for every primitive. `Format` already
satisfies this (a strum `EnumString` enum; no change needed). Opaque DB row handles
with no validatable invariant (`art_id`, `payload_id`/`rowid`, `track_id`) stay raw
`i64` — a conscious YAGNI skip recorded in the spec.

### Lean set

- **`PictureType`** — replaces the raw `u32`/`i64` picture-type fields in
  `ArtInput` and `TrackArt`; validated to `0..=20`. The Rust-side mirror of the
  #199 `track_art` CHECK.
- **`TrackBounds`** — `{ audio_offset, audio_length }` with a checked constructor
  enforcing `audio_offset + audio_length <= backing_size` (gated on the same
  verification as the #199 bound). Kills, at construction, the out-of-file audio
  range the reader assumes away.
- **`BlobLen`** — a non-negative length newtype shared by art / binary-tag payload
  lengths, giving Plan D a typed length to store and segments to carry.

### Placement

`TryFrom`/validated constructors live at the `musefs-db` row readers (untrusted SQL
row → validated type, rejecting bad rows at the read boundary rather than deep in a
consumer). `musefs-format` synthesis inputs (`ArtInput`, `BinaryTagInput`) accept
the validated types instead of bare primitives. The fuzz crate builds these inputs,
so `cargo +nightly fuzz build` is part of Plan C verification.

## Plan D — #201: unskippable RegionLayout validity

- **Hide unchecked construction.** `RegionLayout::new` becomes non-public
  (`pub(crate)` or `#[cfg(test)]`); production synthesis can only obtain a
  `RegionLayout` through `validated(...)`.
- **Store checked totals.** `total_len` and `header_len` are computed once at
  construction and stored; `total_len()`/`header_len()` return the stored values
  rather than re-summing segment lengths after the fact.
- **Strengthen `validate()`** beyond the current empty-segment / total-overflow
  checks to cover backing-range bounds and art/blob handle lengths — expressed via
  Plan C's `BlobLen` so a segment cannot be assembled with an impossible length.
- **Defensive core check.** `musefs-core/src/reader.rs` (~`:277`) calls
  `layout.validate()?` before caching the layout — cheap belt-and-suspenders at
  the consuming boundary.

Plan D depends on Plan C's `BlobLen`; sequence C before D.

## Cross-cutting

### Documentation

ARCHITECTURE.md's external-writer contract section is updated by Plan A to state the
now-enforced invariants. Each plan updates the docs touched by its own change as
part of its commits (per the repo convention that docs are kept current).

### Fuzz crate

Plans C and D change format-layer signatures; the out-of-workspace `fuzz/` crate is
not built by `cargo build`/`test`/`clippy`, so each of those plans runs
`cargo +nightly fuzz build` in verification.

### Risk register

- **`audio_offset + audio_length <= backing_size`** (used by both the #199 CHECK
  and the #200 `TrackBounds` constructor): must be verified true for every scanned
  format before it is pinned. If any format legitimately stores audio with trailing
  backing bytes beyond `offset+length`, the bound is relaxed or dropped in both
  places, and the spec/plan record why. This is the single load-bearing assumption
  shared across plans.
- **Newtype churn** (#200): bounded by the lean set and the decision rule; opaque
  handles stay raw to avoid boundary thrash.
- **Migration trigger fidelity** (#199): the `V4` table rebuild must preserve the
  V1/V3 trigger and changelog-ring behavior exactly; the existing migration tests
  guard this and are extended.
