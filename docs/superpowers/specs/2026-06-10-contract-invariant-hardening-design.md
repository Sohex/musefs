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
The `INSERT … SELECT` copy enforces every new `CHECK`, so any pre-existing
violating row aborts the migration with the violated constraint named — and thus
refuses the mount. With no live databases, this path runs only against fresh,
empty tables in practice.

**Trigger recreation is mandatory and order-sensitive — not "left intact."**
Dropping and renaming a table destroys every trigger defined *on* it. The V4
rebuild therefore must explicitly recreate, after each rebuilt table:

- `tracks` rebuild destroys the V3 changelog triggers `tracks_changelog_ai/au/ad`
  (defined ON `tracks`, `schema.rs` MIGRATION_V3). These must be recreated.
- `tags` rebuild destroys the V1 `tags_ai/au/ad` triggers.
- `track_art` rebuild destroys the V1 `track_art_ai/au/ad` triggers.

`structural_blocks` and `track_changes` are **not** in the rebuild set and are left
untouched. Critically, the changelog triggers must be recreated (or dropped for
the duration) such that the bulk `INSERT … SELECT` copy does **not** fire
`tracks_changelog_ai` — otherwise every copied row pumps the self-pruning
`track_changes` ring on a path meant to be transparent. The plan pins trigger
recreation to *after* the copy (or drops changelog triggers across the rebuild and
restores them last). The existing `migration_v2_tests`/`migration_v3_tests` style
tests are extended to assert trigger and changelog-ring fidelity across V4.

### Constraint set

Limited to invariants SQLite enforces cheaply and that musefs already assumes:

**`tracks`**
- `format IN ('flac','mp3','m4a','opus','vorbis','oggflac','wav')` — the exact
  `Format` serialization strings (pinned by `db_strings_are_pinned`).
- `audio_offset >= 0`, `audio_length >= 0`, `backing_size >= 0`,
  `backing_mtime >= 0`, `content_version >= 0`, `updated_at >= 0`.
- `audio_offset + audio_length <= backing_size` — the read path assumes the audio
  region lies within the backing file. **Verified to hold for every format**, so it
  is pinned (not hedged): FLAC/Ogg set `audio_length = file_len - audio_offset`
  (equality, `scan.rs:388-389`, `:428-429`); MP3 only shrinks `audio_end` from file
  length (`mp3.rs:45-60`); MP4 derives the bound from the `mdat` box and the parser
  rejects any box past EOF (`mp4.rs:78,121`); WAV rejects audio running past file
  end and allows only trailing chunks (`wav.rs:765`). Note: this bound is about the
  *backing file*, distinct from the synthesized-layout trailing-pad case in
  `assert_backing_covers_audio` (`fuzz_check.rs`) — a plan author must not conflate
  them and wrongly relax the CHECK.

**`tags`**
- `ordinal >= 0`.
- `value_blob IS NULL OR value = ''` — encodes the V2 binary-vs-text row shape
  (a binary row carries its bytes in `value_blob` and `''` in `value`). The
  asymmetry is intentional: a text row with empty `value` and NULL `value_blob` is
  legal (empty text tags), so the CHECK must not be "tightened" to forbid it.

**`art`**
- `byte_len = length(data)` — the stored length must match the blob. This CHECK is
  **load-bearing, not cosmetic**: the streaming art reader trusts `byte_len`
  (`get_art_meta` reads it without re-reading `data`, `art.rs:43-50`), so this
  constraint is the sole guarantor of `ArtMeta.byte_len`-vs-blob agreement.
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
path defends this itself (min-id rule, plus a Stage-B re-sort), but **both**
full-rebuild paths inherit their order purely from `list_tracks`'s `ORDER BY id`:
`build_full` and `rebuild_full` (`facade.rs:348-371`) each source entries from the
shared `render_entries` (`facade.rs:287-316`), which preserves `list_tracks` order.
The paths agree only by that coincidence; change the `ORDER BY`, or build
`render_entries` from a non-id-ordered source, and a track's inode flips between
rebuild paths.

### Fix

Sort entries by track `id` **in `render_entries`** — the single shared source of
both `build_full` and `rebuild_full`. This makes the full-rebuild order
self-established at one local point, covering both paths at once and rendering the
Stage-B fallback's separate re-sort redundant-but-harmless. The `ORDER BY id` in
`list_tracks` stays (harmless, good for locality) but is demoted from load-bearing
to incidental; the test comment at `tree.rs:1350-1352` is updated to make this the
documented spec of the behavior.

**Anti-option — do NOT sort inside `build_with_ci`.** Many `tree.rs` unit tests
call `build_with`/`build_with_ci` with deliberately id-unordered entries and assert
on insertion-order-dependent behavior (e.g. `introducing_id_is_min_descendant_track_id`
at `tree.rs:1226`, `apply_changes_handles_dir_vs_file_min_id_flip` at `:1341`).
Sorting inside the build primitive would silently reorder those inputs and mask the
very flips they pin. The sort belongs in `render_entries`, not the build path.
Sweep `assert_apply_matches_build` (`tree.rs:1467,1477`) for consistency so the
oracle and production agree on where sorting lives.

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

- **`PictureType`** — replaces the raw `u32`/`i64` picture-type fields, validated to
  `0..=20`. The Rust-side mirror of the #199 `track_art` CHECK. Applied at **both**
  the DB-read boundary (`TrackArt`, `ArtInput`) *and* the scan boundary
  (`EmbeddedPicture.picture_type`, `input.rs:32-42`) — the latter is the actual
  untrusted-bytes entry point, where a malformed FLAC/APIC picture-type byte first
  appears, so it is the highest-value construction site.
- **`TrackBounds`** — `{ audio_offset, audio_length }` with a checked constructor
  enforcing `audio_offset + audio_length <= backing_size` (the same now-pinned bound
  as the #199 CHECK). Kills, at construction, the out-of-file audio range the reader
  assumes away.
- **`BlobLen`** — a **non-zero** length newtype for art / binary-tag payload lengths
  and the metadata segments that carry them (`ArtImage`, `BinaryTag`, `OggArtSlice`).
  Its invariant is non-*zero*, not merely non-negative: those fields are already
  `u64`, so a non-negative newtype would be pure churn (and a decision-rule
  violation). Non-zero encodes the layout's `EmptySegment` rule at the type level,
  so Plan D's `validate()` can rely on it for metadata segments. `BackingAudio` keeps
  raw `u64` — a zero-length backing audio run is valid (see `layout.rs::validate`).

### Placement

`TryFrom`/validated constructors live at the `musefs-db` row readers (untrusted SQL
row → validated type) and the scanner's picture-extraction path (untrusted file
bytes → validated type), rejecting bad rows/bytes at the boundary rather than deep
in a consumer. `musefs-format` synthesis inputs (`ArtInput`, `BinaryTagInput`)
accept the validated types instead of bare primitives. Two follow-throughs the plan
must honor: (1) the fuzz crate builds these inputs, so `cargo +nightly fuzz build`
is part of Plan C verification; (2) the DTOs carry
`#[cfg_attr(feature = "mutants", derive(Default))]` (`models.rs`), so any newtype
embedded in them must also derive/implement `Default` under the `mutants` feature or
the mutation-gate build breaks.

## Plan D — #201: unskippable RegionLayout validity

- **Hide unchecked construction — this is a real migration, not a visibility flip.**
  `RegionLayout::new` must not remain a freely-callable public unchecked
  constructor. `pub(crate)`/`#[cfg(test)]` alone is **insufficient**: `new` is called
  from external integration-test crates (`musefs-format/tests/layout.rs`,
  `musefs-core/tests/read_at.rs`) that cannot see crate-private or `cfg(test)`
  items, *and* from a production path — the `Mode::StructureOnly` reader builds a raw
  `RegionLayout::new(vec![BackingAudio…])` at `reader.rs:144`. Plan D therefore must:
  (a) migrate the `StructureOnly` production site and the synthesis path so
  production obtains layouts only via `validated(...)`; (b) migrate the
  integration-test call sites to `validated(...)` (most construct layouts that should
  validate anyway), or expose a deliberately-public test constructor (e.g.
  `new_unchecked` behind a `test-util` feature) for the cases that intentionally
  build invalid layouts to exercise `validate()`.
- **Store checked totals, and make `segments` private.** `total_len`/`header_len`
  are computed once at construction and stored; the accessors return the stored
  values rather than re-summing. This requires making the currently-`pub`
  `RegionLayout.segments` field private (else a caller mutating it desyncs the cache)
  and removing or hand-implementing the derived `Default` for the same reason.
  Callers already mostly use the `.segments()` accessor; the plan sweeps for any
  direct field access.
- **Strengthen `validate()`** beyond the current empty-segment / total-overflow
  checks to cover backing-range bounds and art/blob handle lengths — expressed via
  Plan C's non-zero `BlobLen` so a metadata segment cannot be assembled with a
  zero/impossible length.
- **Defensive core check.** Today `reader.rs` validates only at construction
  (`synthesize_layout → RegionLayout::validated`); at `reader.rs:277` it merely reads
  `layout.total_len()`. Plan D *adds* a defensive `layout.validate()?` before caching
  — cheap belt-and-suspenders at the consuming boundary (present tense: this call
  does not exist yet).

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
  and the #200 `TrackBounds` constructor): **verified true for every format and now
  pinned** (FLAC/Ogg equality, MP3/MP4/WAV `<=`; evidence in Plan A). The residual
  risk is a future format/scanner change violating it — the CHECK and the
  `TrackBounds` constructor are exactly what would catch that regression.
- **Migration trigger fidelity** (#199): the `V4` table rebuild destroys and must
  recreate the V1 `tags`/`track_art` triggers and the V3 `tracks` changelog
  triggers, with recreation ordered *after* the bulk copy so the rebuild does not
  pump the `track_changes` ring. The existing migration tests guard this and are
  extended (see Plan A Mechanism).
- **Hiding `RegionLayout::new`** (#201): touches external integration-test crates
  and a production `StructureOnly` call site; treated as a migration with a
  sanctioned test-construction path, not a one-line visibility change (see Plan D).
- **Newtype churn** (#200): bounded by the lean set and the decision rule; opaque
  handles stay raw to avoid boundary thrash. `BlobLen` earns its place only as a
  non-zero type — a non-negative version is explicitly rejected as churn.
