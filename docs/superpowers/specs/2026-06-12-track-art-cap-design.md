# Cap per-track `track_art` row count on the serve path

**Issue:** #316 (tracking: #280)
**Date:** 2026-06-12
**Status:** Design approved, pending implementation plan

## Problem

Text and binary tag reads are bounded per track by `check_tag_count` against
`MAX_TAGS_PER_TRACK` (4096). The `track_art` serve path has **no equivalent
row-count cap**:

- `Db::get_track_art` (`musefs-db/src/art.rs:77-99`) iterates every `track_art`
  row for a track into an unbounded `Vec<TrackArt>`. It caps each row's
  `description` length (`check_field_len` / `MAX_ART_DESCRIPTION_LEN`) but never
  the number of rows.
- `mapping::track_art_to_inputs` (`musefs-core/src/mapping.rs:33-89`) then
  iterates each entry into an unbounded `Vec<ArtInput>`, producing an O(N)-segment
  layout at resolve time. It caps each art blob's *bytes* (`MAX_ART_BYTES`) but
  again not the *count*.

A crafted `--db` (in scope per `SECURITY.md`) can plant millions of small
`track_art` rows for a single track. Even with each blob streamed and byte-capped,
resolving that track allocates two attacker-proportional vectors and an O(N)
segment layout — failing open (allocation/CPU DoS) instead of returning a
controlled error the way the tag path does.

This is the same "uncapped per-track DB row count materialized before use" class
already solved for tags.

## Threat model and scoping

- **Affected trust boundary:** a hostile SQLite store read at serve time. The
  store is the documented external-writer contract (`SECURITY.md`,
  `ARCHITECTURE.md#the-external-writer-contract`).
- **In scope:** the serve path — `Db::get_track_art` and, transitively, every
  consumer of its result (`track_art_to_inputs` is the only non-test one).
- **Out of scope:** the scanner's embedded-picture *count* vector. The issue
  notes the scanner caps picture *bytes* but not *count*, making a hostile media
  file a weaker, related vector. It is deliberately excluded here because:
  1. Media files are a softer trust boundary than a crafted SQLite store.
  2. At ingest, `probed.pictures` is **already** a materialized
     `Vec<EmbeddedPicture>`; the `scan.rs:559-577` accept-loop only iterates it.
     The transient count allocation happens upstream, inside each format's
     `read_pictures` (`flac`/`mp3`/`mp4`/`ogg`/`wav`). A single `.take(N)` in
     `scan.rs` would cap only what gets *stored*, not what the parser already
     allocated — defense-in-depth that looks complete but isn't. A complete fix
     would have to live in all five parsers, which is a distinct format-layer
     change deserving its own issue.

## Design

Enforce the cap in the DB layer, mirroring `check_tag_count` exactly. The guard
lives at the single site where the unbounded `Vec<TrackArt>` is built, so the fix
is one place and transitively bounds everything downstream.

**Rejected alternative — guard in `track_art_to_inputs` (core layer):** strictly
worse. `get_track_art` would still build the unbounded `Vec<TrackArt>` before core
ever sees it, so the allocation DoS would survive. The guard must sit where the
Vec grows.

### Changes

1. **`musefs-db/src/limits.rs`** — add

   ```rust
   /// Max `track_art` rows materialized per track. Art is low-cardinality
   /// (cover/back/leaflet/per-disc); this is a crafted-DB corruption backstop,
   /// not a semantic limit. Mirrors `MAX_TAGS_PER_TRACK`'s reader-guard role.
   pub const MAX_ART_ROWS_PER_TRACK: usize = 4096;
   ```

   next to `MAX_TAGS_PER_TRACK`. Add `assert_eq!(MAX_ART_ROWS_PER_TRACK, 4096)` to
   the existing `cap_values_are_pinned` test.

2. **`musefs-db/src/error.rs`** — add a `DbError` variant mirroring
   `TooManyValues`:

   ```rust
   #[error(
       "track {track_id} has {count} track_art rows, exceeds the {max}-row cap (crafted or corrupt DB)"
   )]
   TooManyArtRows {
       track_id: i64,
       count: usize,
       max: usize,
   },
   ```

   and a `check_art_count(track_id, count)` helper mirroring `check_tag_count`,
   keeping a single `>` boundary site (one mutation target) shared by every art
   reader. Add a boundary unit test `art_count_accepts_at_cap_rejects_above`
   mirroring `tag_count_accepts_at_cap_rejects_above` (accepts at the cap, rejects
   one over) to pin the `>` against `>=`/`==` mutants.

3. **`musefs-db/src/art.rs`** — in `get_track_art`, call
   `check_art_count(track_id, out.len())?` immediately after each `out.push(...)`,
   so the Vec is bounded at cap+1 before erroring (same shape as the tag readers).
   Import `check_art_count` alongside the existing `check_field_len`. Add two
   integration tests mirroring the existing description-cap pair
   (`get_track_art_rejects_oversize_description` /
   `get_track_art_accepts_description_at_cap`): 4096 rows → `Ok`, 4097 rows →
   `Err(TooManyArtRows)`.

### Data flow

Crafted DB → `get_track_art` materializes rows one at a time → errors at row 4097
→ `DbError::TooManyArtRows` → `?` → `CoreError::Db` (via the existing
`#[from] musefs_db::DbError`, `musefs-core/src/error.rs:6`) → resolve fails with a
controlled error instead of allocating attacker-proportional vectors.
`track_art_to_inputs` needs no change: it inherits the bound (returns at most 4096
`ArtInput`s, or the propagated error).

### No schema/migration change

Like the tag-count cap, a per-track row `COUNT` cannot be expressed as a column
`CHECK`, so this is reader-guard-only. No `MIGRATION_V*`, no `user_version` bump,
no Python schema-mirror regeneration, no `picard` `user_version == N` test churn.

## Testing

- `limits.rs`: `MAX_ART_ROWS_PER_TRACK` pinned in `cap_values_are_pinned`.
- `error.rs`: `art_count_accepts_at_cap_rejects_above` inclusive-boundary unit
  test (the mutation-gate anchor for the single `>` site).
- `art.rs`: two `get_track_art` integration tests — at cap `Ok`, over cap
  `TooManyArtRows`.
- The full workspace suite stays green at the single commit (no red-test
  intermediate), satisfying the pre-commit gate and the in-diff mutation gate.
