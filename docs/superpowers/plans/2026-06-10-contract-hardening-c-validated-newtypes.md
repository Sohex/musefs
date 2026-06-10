# Plan C — #200: pragmatic validated newtypes

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Introduce exactly three validated newtypes — `TrackBounds` (musefs-db), `PictureType` and `BlobLen` (musefs-format) — so that an out-of-file audio range, an out-of-range ID3/FLAC picture type, and a zero-length art/binary-tag payload become unconstructible at the crate boundaries that own them.

**Architecture:** `musefs-db` and `musefs-format` are independent sibling crates (format depends on db only as a dev-dependency); `musefs-core` depends on both and bridges them. `TrackBounds` therefore lives in db (validated at the `tracks` row reader), while `PictureType`/`BlobLen` live in format (the synthesis-input concern); the validated construction of format inputs from db rows happens in core, at the db→format bridge and the scan boundary. No new cross-crate type and no new production dependency are added.

**Tech Stack:** Rust, musefs-db / musefs-format / musefs-core

---

## File Structure

| File | Responsibility | Change |
| ---- | -------------- | ------ |
| `musefs-db/src/models.rs` | `TrackBounds` newtype definition; `Track` embeds it in place of the two raw `u64` fields | modified |
| `musefs-db/src/error.rs` | new `DbError::AudioBoundsOutOfRange` variant for the `TrackBounds` invariant | modified |
| `musefs-db/src/tracks.rs` | `row_to_track` constructs `TrackBounds` via the checked constructor (the SQL→type boundary); db-internal tests read `track.bounds()` | modified |
| `musefs-db/src/lib.rs` | re-export `TrackBounds` | modified |
| `musefs-format/src/input.rs` | `PictureType` + `BlobLen` definitions; `ArtInput`/`EmbeddedPicture` use `PictureType`; `ArtInput.data_len`/`BinaryTagInput.len` use `BlobLen` | modified |
| `musefs-format/src/lib.rs` | re-export `PictureType`, `BlobLen` | modified |
| `musefs-format/src/flac.rs` | FLAC parser builds `EmbeddedPicture.picture_type` via `PictureType::new` (clamp out-of-range to 0, matching scan's current policy); synthesis read sites use `.get()` | modified |
| `musefs-format/src/mp3.rs` | mp3 parser/synthesis use `PictureType`/`BlobLen` accessors | modified |
| `musefs-format/src/mp4.rs` | mp4 parser/synthesis use `PictureType`/`BlobLen` accessors (incl. binary-tag `bt.len` at 670-684 + zero-guard removal) | modified |
| `musefs-format/src/ogg/mod.rs` | ogg synthesis uses `.get()` on `meta.data_len`; zero-art filter removed | modified |
| `musefs-format/src/wav.rs` | **no change** — synthesis delegates to `build_id3v2_segments`; no `ArtInput`/`BinaryTagInput` literals; `wav_front` is unrelated | unchanged |
| `musefs-format/tests/*.rs` | integration-test `ArtInput`/`BinaryTagInput` literals (Task 4) + `EmbeddedPicture` asserts in `flac_pictures.rs:36` / `mp3_pictures.rs:23` (Task 3) | modified |
| `musefs-core/src/mapping.rs` | db→format bridge: build `ArtInput`/`BinaryTagInput` with `PictureType::new`/`BlobLen::new`, dropping zero-length entries at construction | modified |
| `musefs-core/src/error.rs` | new `CoreError::InvalidPictureType` for the bridge's out-of-range picture type | modified |
| `musefs-fuse/src/lib.rs` | add `InvalidPictureType` to the exhaustive `errno` match (→ EIO) | modified |
| `musefs-core/src/scan.rs` | scan boundary: `pic.picture_type` is now a validated `PictureType` (clamp redundancy removed); `track.bounds` reads in db-comparison tests (incl. `:1971`) | modified |
| `musefs-core/src/reader.rs` | read hot path reads `track.bounds.audio_offset()` / `.audio_length()` (incl. the `:155` guard expression). The in-module over-EOF test is owned by Plan A, not this plan. | modified |
| `musefs-db/tests/tracks.rs` | `Track` audio-field reads → `bounds()` accessors; bounds-valid fix in `upsert_conflict_updates_all_mutable_columns` | modified |
| `musefs-core/tests/{scan,scan_counters,probe_equivalence}.rs` | `Track` audio-field reads → `bounds()` accessors (`tests/reader.rs` and `tests/external_contract.rs` are NOT touched here — Plan A owns their over-EOF tests) | modified |
| `fuzz/src/lib.rs` | `arb_arts`/`arb_binary_tags` build `PictureType`/`BlobLen` (drop zero-length art) | modified |
| `fuzz/fuzz_targets/ogg.rs` | `ArtInput` literal updated; zero `data_len` bounded/dropped | modified |

---

## Resolved ambiguities (read before starting)

These two design decisions were resolved against the actual code; the tasks below depend on them.

1. **`BlobLen` is non-zero, but `data_len == 0` / `len == 0` are today legal, load-bearing `ArtInput`/`BinaryTagInput` values.** Every synthesis path filters degenerate empty art/tags by an in-band zero check — `flac.rs:254` (art count filter) & `300-303` (art guard), `mp3.rs:347-350` (binary guard) & `365-369` (art guard), `mp4.rs:670-673` (binary guard) & `835-836` (art filter), `ogg/mod.rs:260-264` (art filter) — with comments calling out that a zero-length `ArtImage`/`BinaryTag` segment would fail `RegionLayout::validate` (`EmptySegment`). (FLAC binary `APPLICATION`/`CUESHEET` tags are never zero-dropped today — they ride through `valid_binary` with a `> MAX_BLOCK_BODY` cap only — but `BlobLen` makes them non-zero by type regardless.) A strictly non-zero `BlobLen` makes `data_len == 0` unrepresentable. **Resolution:** move the zero-drop to the *construction boundaries* (the db→format bridge in `mapping.rs`, the scan path, and the fuzz builders). A zero-length entry is dropped before the `ArtInput`/`BinaryTagInput` is built — observably identical to today, since synthesis would have skipped it anyway. The now-unreachable in-synthesis `== 0` skip branches are removed (their guard can never fire once the field is a `NonZeroU64`), and the format-internal tests that fed zero-length art/tags to exercise the skip are re-pointed at the construction boundary or deleted as dead. This honors the prompt's "apply `BlobLen` only to the DTO length fields and call `.get()` at segment build" while keeping the EmptySegment invariant intact for Plan D.

2. **Scan-boundary picture-type policy = clamp to 0 (not skip, not error).** Today `scan.rs:571-575` and `:656-660` *clamp* an out-of-range `pic.picture_type` to `0` — never dropping or erroring the picture. Because `EmbeddedPicture.picture_type` becomes a `PictureType` built in the format parsers, the validation moves to the actual untrusted-bytes entry point, the FLAC parser (`flac.rs:386`, the only parser reading a picture-type from raw file bytes; mp3 derives it from the already-valid `id3` enum, mp4 hardcodes `3`). To preserve today's observable behavior, the FLAC parser clamps an out-of-range byte to `PictureType::new(0)` (`PictureType::new(raw).unwrap_or(PictureType::ZERO)`), and `scan.rs`'s now-redundant `if pic.picture_type <= 20 { … } else { 0 }` clamp is replaced by `pic.picture_type.get()`. The integration test asserts that a FLAC file with an out-of-range picture-type byte is ingested with stored picture_type `0`.

3. **`TrackBounds` fits `Track`'s row reader cleanly.** `Track` is constructed as a struct literal in exactly ONE place — `row_to_track` (`tracks.rs:35`) — so `TrackBounds::new` has a single call site. `NewTrack` (the write DTO) keeps raw `u64` fields and is unaffected. Consumers of `track.audio_offset`/`track.audio_length` are bounded and enumerated in Task 1.

---

## Task 1: `TrackBounds` in musefs-db

Define the newtype, add its error variant, embed it in `Track`, and validate it at the row reader. Because changing `Track`'s fields breaks every `track.audio_offset`/`track.audio_length` reader in the workspace, the field change and ALL call-site fixups land in one green commit.

**Files:**
- `musefs-db/src/error.rs` (add variant)
- `musefs-db/src/models.rs` (lines 86-95 `Track`; add `TrackBounds` near it)
- `musefs-db/src/tracks.rs` (lines 32-46 `row_to_track`; tests at 239-265)
- `musefs-db/src/lib.rs` (lines 13-16 re-export block)
- `musefs-db/tests/tracks.rs` (Track audio-field reads at lines 14, 34, 133-134; **and** the `upsert_conflict_updates_all_mutable_columns` bounds fix — see Step 1.12)
- `musefs-core/src/reader.rs` (production reads at lines 155, 178, 196-197, 204-205, 245, 249-250, 257, 270-271 — Step 1.11. The in-module over-EOF test is owned by Plan A.)
- `musefs-core/src/scan.rs` (test asserts at 1883-1884, 1918; **plus the `norm` closure read at line 1971**)
- `musefs-core/tests/scan.rs` (Track audio-field reads at lines 43, 114, 115)
- `musefs-core/tests/probe_equivalence.rs` (line 42)
- `musefs-core/tests/scan_counters.rs` (the `rows` closure read at line 62 **and** the asserts at lines 147-148)
- (`musefs-core/tests/reader.rs` and `tests/external_contract.rs` are NOT modified by this plan — Plan A owns their over-EOF tests, which assert the V4 CHECK rejection and read no `Track` audio fields.)
- Test path: `musefs-db/src/models.rs` (unit tests for `TrackBounds`), `musefs-db/src/tracks.rs` (row-read integration)

> **Verified read-site inventory (Track values only; `NewTrack` literals keep raw `u64` and are unaffected).** Every site below reads `audio_offset`/`audio_length` off a `Track` returned by `get_track`/`list_tracks` and so breaks the instant the fields move into `bounds`:
> - Production: `reader.rs:155,178,196-197,204-205,245,249-250,257,270-271`.
> - In-crate tests: `scan.rs:1883-1884,1918,1971`.
> - Integration tests: `tests/scan.rs:43,114,115`; `tests/probe_equivalence.rs:42`; `tests/scan_counters.rs:62,147-148`; `db tests/tracks.rs:14,34,133-134`.
> Sites that LOOK like reads but are NOT `Track` values (do not touch): `scan.rs:183-242` (`Probed` built from format-crate bounds), `tests/external_contract.rs:32-33` / `tests/interop_emit.rs` `b.*` (a `musefs_format::*::AudioBounds`/`WavBounds`), `bulk.rs:191` (`list_tracks().len()` only). `mapping.rs`/`facade.rs`/`metrics.rs`/`incremental_refresh.rs`/`structural.rs`/`tags.rs`/`art.rs`/`bulk.rs` `audio_offset:`/`audio_length:` are all `NewTrack` literals — unchanged.

- [ ] **Step 1.1: Write failing unit tests for `TrackBounds`.** Append a `#[cfg(test)] mod track_bounds_tests` to `musefs-db/src/models.rs`:
  ```rust
  #[cfg(test)]
  mod track_bounds_tests {
      use super::TrackBounds;

      #[test]
      fn accepts_in_range() {
          let b = TrackBounds::new(10, 20, 100).unwrap();
          assert_eq!(b.audio_offset(), 10);
          assert_eq!(b.audio_length(), 20);
      }

      #[test]
      fn accepts_exact_fit() {
          let b = TrackBounds::new(30, 70, 100).unwrap();
          assert_eq!(b.audio_offset(), 30);
          assert_eq!(b.audio_length(), 70);
      }

      #[test]
      fn accepts_zero_length() {
          // A zero-length audio run is valid (e.g. structure-only edge).
          let b = TrackBounds::new(0, 0, 0).unwrap();
          assert_eq!(b.audio_length(), 0);
      }

      #[test]
      fn rejects_exceeding_backing_size() {
          assert!(TrackBounds::new(50, 60, 100).is_err());
      }

      #[test]
      fn rejects_offset_plus_length_overflow() {
          assert!(TrackBounds::new(u64::MAX, 1, u64::MAX).is_err());
      }
  }
  ```

- [ ] **Step 1.2: Run + see fail.** `cargo test -p musefs-db track_bounds_tests` — fails to compile: `cannot find type 'TrackBounds' in this scope`.

- [ ] **Step 1.3: Add the `DbError` variant.** Edit `musefs-db/src/error.rs` to:
  ```rust
  use thiserror::Error;

  #[derive(Debug, Error)]
  pub enum DbError {
      #[error(transparent)]
      Sqlite(#[from] rusqlite::Error),
      #[error("audio bounds out of range: offset {audio_offset} + length {audio_length} exceeds backing_size {backing_size}")]
      AudioBoundsOutOfRange {
          audio_offset: u64,
          audio_length: u64,
          backing_size: u64,
      },
  }

  pub type Result<T> = std::result::Result<T, DbError>;
  ```

- [ ] **Step 1.4: Define `TrackBounds`.** Insert before the `Track` struct in `musefs-db/src/models.rs` (use `crate::DbError`):
  ```rust
  /// Validated audio-region bounds for a track: `audio_offset + audio_length`
  /// is guaranteed to fit within `backing_size`, so the reader can splice the
  /// audio region without re-checking. Built at the `tracks` row reader.
  #[cfg_attr(feature = "mutants", derive(Default))]
  #[derive(Debug, Clone, Copy, PartialEq, Eq)]
  pub struct TrackBounds {
      audio_offset: u64,
      audio_length: u64,
  }

  impl TrackBounds {
      /// Err if `audio_offset + audio_length` overflows or exceeds `backing_size`.
      pub fn new(
          audio_offset: u64,
          audio_length: u64,
          backing_size: u64,
      ) -> Result<TrackBounds, crate::DbError> {
          let end = audio_offset
              .checked_add(audio_length)
              .filter(|&end| end <= backing_size)
              .ok_or(crate::DbError::AudioBoundsOutOfRange {
                  audio_offset,
                  audio_length,
                  backing_size,
              })?;
          let _ = end;
          Ok(TrackBounds {
              audio_offset,
              audio_length,
          })
      }

      pub fn audio_offset(&self) -> u64 {
          self.audio_offset
      }

      pub fn audio_length(&self) -> u64 {
          self.audio_length
      }
  }
  ```
  (`#[cfg_attr(feature = "mutants", derive(Default))]` is required because `Track` derives `Default` under the `mutants` feature and embeds `TrackBounds`.)

- [ ] **Step 1.5: Embed `TrackBounds` in `Track`.** Replace the `audio_offset`/`audio_length` fields (lines 86-87) so `Track` becomes:
  ```rust
  #[cfg_attr(feature = "mutants", derive(Default))]
  #[derive(Debug, Clone, PartialEq, Eq)]
  pub struct Track {
      pub id: i64,
      pub backing_path: String,
      pub format: Format,
      pub bounds: TrackBounds,
      pub backing_size: u64,
      pub backing_mtime: i64,
      pub content_version: i64,
      pub updated_at: i64,
  }
  ```

- [ ] **Step 1.6: Run + see the `TrackBounds` unit tests pass, db-internal build still red.** `cargo test -p musefs-db track_bounds_tests` — the five new tests pass. `cargo build -p musefs-db` still fails at `row_to_track` and the db tests (field rename); fix next.

- [ ] **Step 1.7: Construct `TrackBounds` in `row_to_track`.** Edit `musefs-db/src/tracks.rs` (lines 31-45). The reader returns `rusqlite::Result<Track>`; map the `DbError` from `TrackBounds::new` to the same conversion error `parse_format_col` uses:
  ```rust
  fn row_to_track(r: &Row) -> rusqlite::Result<Track> {
      let fmt: String = r.get("format")?;
      let format = parse_format_col(&fmt)?;
      let audio_offset: u64 = r.get("audio_offset")?;
      let audio_length: u64 = r.get("audio_length")?;
      let backing_size: u64 = r.get("backing_size")?;
      let bounds = TrackBounds::new(audio_offset, audio_length, backing_size).map_err(|e| {
          rusqlite::Error::FromSqlConversionFailure(
              usize::MAX,
              rusqlite::types::Type::Integer,
              e.to_string().into(),
          )
      })?;
      Ok(Track {
          id: r.get("id")?,
          backing_path: r.get("backing_path")?,
          format,
          bounds,
          backing_size,
          backing_mtime: r.get("backing_mtime")?,
          content_version: r.get("content_version")?,
          updated_at: r.get("updated_at")?,
      })
  }
  ```
  Add `use crate::models::TrackBounds;` to the existing `use crate::models::{...}` import at `tracks.rs:1`.

- [ ] **Step 1.8: Fix db-internal test readers.** In `musefs-db/src/tracks.rs` the `negative_audio_bounds_tests` mod (239-265) still passes — it only asserts `get_track(id).is_err()` on a negative `audio_offset` (the `u64` row-read already fails). No body change needed there. Add a new test in that mod proving the `TrackBounds` invariant (layer 2) fires on a bounds violation that the per-column `u64` read alone would NOT catch. Because Plan A's V4 `audio_offset + audio_length <= backing_size` CHECK (already merged when Plan C runs) rejects such a row at write, the test must plant it via `PRAGMA ignore_check_constraints` — which is exactly the point: a row that somehow slips past the SQLite CHECK is still rejected by the Rust row reader:
  ```rust
  #[test]
  fn out_of_range_bounds_error_at_row_read() {
      let db = Db::open_in_memory().unwrap();
      let id = db
          .upsert_track(&NewTrack {
              backing_path: "/x.flac".into(),
              format: Format::Flac,
              audio_offset: 0,
              audio_length: 1,
              backing_size: 1,
              backing_mtime: 0,
          })
          .unwrap();
      // Plant offset+length > backing_size past the V4 CHECK (layer 1) so we can
      // prove TrackBounds (layer 2) rejects it at row read.
      db.conn
          .pragma_update(None, "ignore_check_constraints", true)
          .unwrap();
      db.conn
          .execute("UPDATE tracks SET audio_length = 5 WHERE id = ?1", [id])
          .unwrap();
      db.conn
          .pragma_update(None, "ignore_check_constraints", false)
          .unwrap();
      assert!(
          db.get_track(id).is_err(),
          "audio_offset + audio_length > backing_size must fail row-read"
      );
  }
  ```

- [ ] **Step 1.9: Re-export `TrackBounds`.** Edit `musefs-db/src/lib.rs` (lines 13-16) to add `TrackBounds` to the `pub use models::{…}` list:
  ```rust
  pub use models::{
      Art, ArtMeta, BinaryTag, BinaryTagRow, Format, NewArt, NewTrack, StructuralBlock, Tag, Track,
      TrackArt, TrackBounds,
  };
  ```

- [ ] **Step 1.10: Run db crate green.** `cargo test -p musefs-db` — all pass (the new bounds tests included).

- [ ] **Step 1.11: Fix core read-path consumers.** In `musefs-core/src/reader.rs`, replace each `track.audio_offset`/`track.audio_length` with `track.bounds.audio_offset()`/`track.bounds.audio_length()` at lines 155, 178, 196-197, 204-205, 245, 249-250, 257, 270-271. Example for line 155:
  ```rust
  if track
      .bounds
      .audio_offset()
      .saturating_add(track.bounds.audio_length())
      > meta.len()
  {
  ```
  Apply the same `.bounds.audio_offset()` / `.bounds.audio_length()` substitution at every other listed line.

- [ ] **Step 1.12: Fix db-crate integration test readers (`musefs-db/tests/tracks.rs`).** Three reads of a `Track` returned by `get_track` break; one also needs a bounds-valid `NewTrack`:
  - Line 14: `assert_eq!(by_id.bounds.audio_offset(), 100);` (`new_track` sets offset 100, length 1000, backing_size 1100 → 1100 ≤ 1100, valid).
  - Line 34: `assert_eq!(db.get_track(id).unwrap().unwrap().bounds.audio_offset(), 222);` (`changed.audio_offset = 222` is a write to the `NewTrack` field — keep as-is; only the read on line 34 changes).
  - Lines 133-134: `assert_eq!(t.bounds.audio_offset(), 222);` / `assert_eq!(t.bounds.audio_length(), 333);`.
  - **Bounds fix in `upsert_conflict_updates_all_mutable_columns` (lines 120-136):** the `changed` `NewTrack` uses `audio_offset: 222, audio_length: 333, backing_size: 444` — `222 + 333 = 555 > 444`, so `get_track(id)` at line 131 now FAILS `TrackBounds::new` at row read. Bump `backing_size` so the row is readable; change `backing_size: 444` to `backing_size: 555` and update the assert at line 135 to `assert_eq!(t.backing_size, 555);`. (The test's purpose — every mutable column round-trips on conflict-update — is preserved; only the now-invalid bounds combination is corrected.)

- [ ] **Step 1.13: Fix core in-crate + integration test readers.** Update the `Track`-typed reads:
  - `musefs-core/src/scan.rs:1883-1884`: `assert_eq!(track.bounds.audio_offset(), full.bounds.audio_offset());` / `assert_eq!(track.bounds.audio_length(), full.bounds.audio_length());`
  - `musefs-core/src/scan.rs:1918`: `assert_eq!(usize_from(track.bounds.audio_length()), b"DIFFERENT-AUDIO".len());`
  - `musefs-core/src/scan.rs:1971` (the `norm` closure): `.map(|t| (t.backing_path, t.bounds.audio_offset(), t.bounds.audio_length()))`.
  - `musefs-core/tests/scan.rs:43`: `assert!(a_track.bounds.audio_length() == 30);`
  - `musefs-core/tests/scan.rs:114-115`: `assert_eq!(t.bounds.audio_offset(), audio_offset);` / `assert_eq!(t.bounds.audio_length(), audio_len);`
  - `musefs-core/tests/probe_equivalence.rs:42`: `out.push((t.backing_path, t.bounds.audio_offset(), t.bounds.audio_length(), tags, art));`
  - `musefs-core/tests/scan_counters.rs:62` (the `rows` closure): `.map(|t| (t.backing_path, t.bounds.audio_offset(), t.bounds.audio_length()))`.
  - `musefs-core/tests/scan_counters.rs:147-148`: `assert_eq!(track.bounds.audio_offset(), o_track.bounds.audio_offset());` / `assert_eq!(track.bounds.audio_length(), o_track.bounds.audio_length());`

  > **Note on the over-EOF error-assertion tests.** Plan A (already merged) rewrote the three over-EOF-bounds tests — `build_rejects_audio_region_past_end_of_file` (`musefs-core/src/reader.rs`), `bounds_check_rejects_audio_region_overrunning_the_file` (`musefs-core/tests/reader.rs`), and `scanner_owned_bounds_mutation_is_rejected_by_the_contract` (`musefs-core/tests/external_contract.rs`) — to assert the V4 CHECK rejects the over-EOF **write**. They no longer commit an over-EOF row, call `resolve`, or read `Track` audio fields, so this plan does **not** touch them and they are unaffected by the `Track` field rename. `TrackBounds` (layer 2) is the row-read defense and is tested directly by `out_of_range_bounds_error_at_row_read` (Step 1.8). The `reader.rs:155` production guard — whose expression Step 1.11 rewrites to `track.bounds.…()` — is now logically unreachable (line 119 enforces `meta.len() == backing_size`, and `TrackBounds` enforces `offset + length <= backing_size`, so `offset + length <= meta.len()` always holds in Synthesis); it is left in place as a deliberate defense-in-depth guard.

- [ ] **Step 1.14: Run + see workspace green.** `cargo test -p musefs-db -p musefs-core` — all pass. Then `cargo clippy --all-targets -p musefs-db -p musefs-core -- -D warnings`.

- [ ] **Step 1.15: Commit.** Stage exactly the files touched (the read-site fixups in `reader.rs`, both `tracks.rs`, and `tests/scan.rs` MUST land in this same commit, or the workspace build/tests go red — the pre-commit hook rejects that). Note: `tests/reader.rs` and `tests/external_contract.rs` are NOT in this set — Plan A owns them and this plan does not modify them:
  ```bash
  git add musefs-db/src/error.rs musefs-db/src/models.rs musefs-db/src/tracks.rs \
          musefs-db/src/lib.rs musefs-db/tests/tracks.rs \
          musefs-core/src/reader.rs musefs-core/src/scan.rs \
          musefs-core/tests/scan.rs \
          musefs-core/tests/probe_equivalence.rs musefs-core/tests/scan_counters.rs
  git commit -F - <<'EOF'
  feat(db): add validated TrackBounds newtype (#200)

  TrackBounds wraps tracks.audio_offset/audio_length with a checked
  constructor enforcing offset + length <= backing_size at the row
  reader — the untrusted SQL->type boundary. Track embeds it; the
  reader hot path and tests read via the accessors.

  This adds the layer-2 (Rust row-reader) defense beneath Plan A's
  layer-1 SQLite CHECK: a row that slips past the CHECK is rejected at
  get_track (DbError::AudioBoundsOutOfRange -> CoreError::Db). The
  reader.rs:155 live-file guard is now logically unreachable but kept
  as defense-in-depth.

  Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>
  EOF
  ```

---

## Task 2: `PictureType` and `BlobLen` definitions in musefs-format

Define both newtypes (with their unit tests) in `input.rs` and re-export them, BEFORE changing any field type. This task is self-contained and green on its own.

**Files:**
- `musefs-format/src/input.rs` (add the two types + tests; do not yet change the DTO fields)
- `musefs-format/src/lib.rs` (line 18 re-export)
- Test path: `musefs-format/src/input.rs` (`mod tests`)

- [ ] **Step 2.1: Write failing unit tests.** Extend the existing `#[cfg(test)] mod tests` in `musefs-format/src/input.rs`:
  ```rust
  #[test]
  fn picture_type_accepts_full_range() {
      for v in 0..=20u32 {
          assert_eq!(super::PictureType::new(v).unwrap().get(), v);
      }
  }

  #[test]
  fn picture_type_rejects_out_of_range() {
      assert!(super::PictureType::new(21).is_none());
      assert!(super::PictureType::new(u32::MAX).is_none());
  }

  #[test]
  fn blob_len_rejects_zero() {
      assert!(super::BlobLen::new(0).is_none());
  }

  #[test]
  fn blob_len_round_trips_nonzero() {
      assert_eq!(super::BlobLen::new(1).unwrap().get(), 1);
      assert_eq!(super::BlobLen::new(u64::MAX).unwrap().get(), u64::MAX);
  }
  ```

- [ ] **Step 2.2: Run + see fail.** `cargo test -p musefs-format input::tests` — fails to compile: `cannot find type 'PictureType'` / `'BlobLen'`.

- [ ] **Step 2.3: Define both newtypes.** Insert at the top of `musefs-format/src/input.rs` (above `TagInput`):
  ```rust
  use std::num::NonZeroU64;

  /// An ID3/FLAC picture type, validated to the `0..=20` range (the #199
  /// `track_art` CHECK, mirrored Rust-side at the synthesis-input boundary).
  #[derive(Debug, Clone, Copy, PartialEq, Eq)]
  pub struct PictureType(u8);

  impl PictureType {
      /// The "Other" picture type (0); the clamp target for an out-of-range byte.
      pub const ZERO: PictureType = PictureType(0);

      pub fn new(v: u32) -> Option<PictureType> {
          if v > 20 {
              None
          } else {
              Some(PictureType(v as u8))
          }
      }

      pub fn get(self) -> u32 {
          u32::from(self.0)
      }
  }

  /// A non-zero payload length for an art image or binary tag. The non-zero
  /// invariant encodes the layout's `EmptySegment` rule at the type level:
  /// a degenerate empty payload is dropped at the construction boundary, so a
  /// metadata segment can never carry a zero length.
  #[derive(Debug, Clone, Copy, PartialEq, Eq)]
  pub struct BlobLen(NonZeroU64);

  impl BlobLen {
      pub fn new(v: u64) -> Option<BlobLen> {
          NonZeroU64::new(v).map(BlobLen)
      }

      pub fn get(self) -> u64 {
          self.0.get()
      }
  }
  ```

- [ ] **Step 2.4: Re-export.** Edit `musefs-format/src/lib.rs:18`:
  ```rust
  pub use input::{
      ArtInput, BinaryTagInput, BlobLen, EmbeddedBinaryTag, EmbeddedPicture, PictureType, TagInput,
  };
  ```

- [ ] **Step 2.5: Run + see pass.** `cargo test -p musefs-format input::tests` — the four new tests pass. `cargo clippy -p musefs-format -- -D warnings`.

- [ ] **Step 2.6: Commit.**
  ```bash
  git add musefs-format/src/input.rs musefs-format/src/lib.rs
  git commit -F - <<'EOF'
  feat(format): add PictureType and BlobLen validated newtypes (#200)

  PictureType validates 0..=20; BlobLen is a non-zero payload length.
  Definitions only — field types are migrated in the next commits.

  Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>
  EOF
  ```

---

## Task 3: Apply `PictureType` to `EmbeddedPicture` and the parsers

Change `EmbeddedPicture.picture_type` to `PictureType`, fix the three parsers and every read/test site in one green commit. `EmbeddedPicture` is built only inside the format crate (FLAC/MP3/MP4 parsers) and consumed by core's scan; this commit covers the format side, the next handles `ArtInput`.

**Files:**
- `musefs-format/src/input.rs` (`EmbeddedPicture.picture_type` field)
- `musefs-format/src/flac.rs` (line 386 read, 420 construct, 675 test assert; line 955 is an `ArtInput` literal — Task 4)
- `musefs-format/src/mp3.rs` (line 545 construct)
- `musefs-format/src/mp4.rs` (line 460 construct)
- `musefs-format/tests/flac_pictures.rs` (`EmbeddedPicture` read assert at line 36)
- `musefs-format/tests/mp3_pictures.rs` (`EmbeddedPicture` read assert at line 23)
- `musefs-core/src/scan.rs` (lines 571-575, 656-660 clamp; test closure literal at 1048-1050)
- Test path: `musefs-format` parser tests + `musefs-core` scan tests

> **Verified `EmbeddedPicture.picture_type` read sites (all break when the field becomes `PictureType`):** construct at `flac.rs:420`, `mp3.rs:545`, `mp4.rs:460`; in-crate test asserts `flac.rs:675`; **integration-test asserts `tests/flac_pictures.rs:36` and `tests/mp3_pictures.rs:23`** (both `assert_eq!(p.picture_type, 3)` on a parsed `EmbeddedPicture` — the review flagged these were missing). `mp3_pictures.rs:11` (`id3::frame::PictureType::CoverFront`) is the upstream `id3` crate type — unaffected. `tests/proptest_flac.rs:21,35` only build `Vec::<ArtInput>::new()` — no literal, no change.

- [ ] **Step 3.1: Write a failing format-side test pinning the FLAC clamp policy.** Add to the existing FLAC test module (near the picture tests at `flac.rs:675`) a test that an out-of-range picture-type byte is clamped to 0 at parse:
  ```rust
  #[test]
  fn read_picture_clamps_out_of_range_type() {
      // Build a minimal PICTURE body with picture_type = 99 (out of 0..=20).
      let mut body = Vec::new();
      body.extend_from_slice(&99u32.to_be_bytes()); // picture_type
      body.extend_from_slice(&3u32.to_be_bytes()); // mime len
      body.extend_from_slice(b"png");
      body.extend_from_slice(&0u32.to_be_bytes()); // desc len
      body.extend_from_slice(&0u32.to_be_bytes()); // width
      body.extend_from_slice(&0u32.to_be_bytes()); // height
      body.extend_from_slice(&0u32.to_be_bytes()); // depth
      body.extend_from_slice(&0u32.to_be_bytes()); // colors
      body.extend_from_slice(&1u32.to_be_bytes()); // data len
      body.push(0xAB); // data
      let pic = super::parse_picture_block(&body).unwrap();
      assert_eq!(pic.picture_type.get(), 0, "out-of-range type clamps to 0");
  }
  ```
  (Adjust the parser fn name to the real one wrapping `flac.rs:386`; confirm via `get_symbols_overview` on `flac.rs`. If the body is parsed by an inline closure, exercise via the public `read_pictures` with a full minimal FLAC instead.)

- [ ] **Step 3.2: Run + see fail.** `cargo test -p musefs-format read_picture_clamps_out_of_range_type` — fails to compile (`picture_type` is `u32`, no `.get()`).

- [ ] **Step 3.3: Change the field type.** In `musefs-format/src/input.rs` line 38, change `EmbeddedPicture.picture_type` from `pub picture_type: u32,` to `pub picture_type: PictureType,`.

- [ ] **Step 3.4: Fix the FLAC parser (the untrusted byte).** At `flac.rs:386`, `picture_type` is `read_u32_be(body, pos)?` (raw file bytes). Wrap construction at the `EmbeddedPicture { picture_type, … }` (line 420):
  ```rust
  picture_type: PictureType::new(picture_type).unwrap_or(PictureType::ZERO),
  ```
  Add `use crate::input::PictureType;` (or `use crate::PictureType;`) to the file's imports. Update the FLAC synthesis read site `flac.rs:208` (`art.picture_type.to_be_bytes()`) — `art` here is an `ArtInput` whose `picture_type` becomes `PictureType` in Task 4, so leave 208 for Task 4; this commit only touches `EmbeddedPicture`. Update the test assert at `flac.rs:675` (`assert_eq!(p.picture_type, 3)`) to `assert_eq!(p.picture_type.get(), 3)`. (Line 955 is an `ArtInput` literal — Task 4.)

- [ ] **Step 3.5: Fix the MP3 parser.** At `mp3.rs:545`, the `id3` crate yields an already-valid type; wrap:
  ```rust
  picture_type: PictureType::new(u8::from(p.picture_type).into())
      .expect("id3 picture_type is always 0..=20"),
  ```
  Add the `PictureType` import to `mp3.rs`.

- [ ] **Step 3.6: Fix the MP4 parser.** At `mp4.rs:460`, replace `picture_type: 3,` with `picture_type: PictureType::new(3).expect("3 is in range"),`. Add the `PictureType` import to `mp4.rs`.

- [ ] **Step 3.7: Fix the scan boundary (remove redundant clamp).** In `musefs-core/src/scan.rs`, `pic.picture_type` is now a `PictureType`. Replace both clamp blocks (lines 571-575 and 656-660):
  ```rust
  let picture_type = if pic.picture_type <= 20 {
      pic.picture_type
  } else {
      0
  };
  ```
  with a single line (the value is already validated at parse):
  ```rust
  let picture_type = pic.picture_type.get();
  ```
  Update the scan test `EmbeddedPicture` closure at `scan.rs:1048-1050`. Verified body: `let pic = |n: usize| EmbeddedPicture { mime: …, picture_type: 3, …, data: vec![0u8; n] }` — `n` sizes the `data`, and `picture_type` is the hardcoded literal `3` (NOT `n`). Change only that field to `picture_type: PictureType::new(3).unwrap(),` (leave `n`/`data` alone). `TrackArt.picture_type` stays raw `u32` (guarded by the #199 CHECK) — `scan.rs` stores `picture_type` (a `u32` from `.get()`) into `TrackArt`, so no further change there.

- [ ] **Step 3.7a: Fix the format integration-test `EmbeddedPicture` read asserts.** These read `picture_type` off a parsed `EmbeddedPicture` and break the moment the field is a `PictureType`:
  - `musefs-format/tests/flac_pictures.rs:36`: `assert_eq!(p.picture_type, 3);` → `assert_eq!(p.picture_type.get(), 3);`
  - `musefs-format/tests/mp3_pictures.rs:23`: `assert_eq!(p.picture_type, 3);` → `assert_eq!(p.picture_type.get(), 3);`
  (Both are `--all-targets`/`cargo test` integration targets — they compile in this commit, so they must be fixed here or the commit lands red.)

- [ ] **Step 3.8: Run + see pass.** `cargo test -p musefs-format -p musefs-core` — all pass, including the new clamp test and the two integration-test asserts above. `cargo clippy --all-targets -p musefs-format -p musefs-core -- -D warnings`.

- [ ] **Step 3.9: Write the core integration test for the scan policy.** Add to `musefs-core/tests/scan.rs` (or the nearest existing scan-art test file — confirm by listing `musefs-core/tests/`) a test that ingesting a FLAC with an out-of-range picture-type byte stores `picture_type == 0`:
  ```rust
  #[test]
  fn scan_clamps_out_of_range_flac_picture_type() {
      // Build / fixture a FLAC whose embedded PICTURE has picture_type = 99,
      // scan it, and assert the stored track_art row has picture_type 0.
      // (Reuse the existing FLAC-with-art fixture helper in this test module;
      //  patch the picture-type bytes to 99 before writing the file.)
      // ... arrange fixture ...
      // let arts = db.get_track_art(track_id).unwrap();
      // assert_eq!(arts[0].picture_type, 0);
  }
  ```
  (Flesh out using the existing FLAC-art scan fixture in that file; the assertion is `arts[0].picture_type == 0`.)

- [ ] **Step 3.10: Run + see pass.** `cargo test -p musefs-core scan_clamps_out_of_range_flac_picture_type`.

- [ ] **Step 3.11: Commit.**
  ```bash
  git add musefs-format/src/input.rs musefs-format/src/flac.rs musefs-format/src/mp3.rs \
          musefs-format/src/mp4.rs musefs-format/tests/flac_pictures.rs \
          musefs-format/tests/mp3_pictures.rs \
          musefs-core/src/scan.rs musefs-core/tests/scan.rs
  git commit -F - <<'EOF'
  feat(format): type EmbeddedPicture.picture_type as PictureType (#200)

  Validation moves to the FLAC parser (the untrusted-bytes entry); an
  out-of-range picture-type byte clamps to 0, matching scan's prior
  policy. scan.rs's redundant clamp is removed.

  Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>
  EOF
  ```

---

## Task 4: Apply `PictureType` + `BlobLen` to `ArtInput` / `BinaryTagInput`

Change `ArtInput.picture_type` to `PictureType`, `ArtInput.data_len` and `BinaryTagInput.len` to `BlobLen`. Fix the bridge (`mapping.rs`), every synthesis read site, and every test literal in one green commit. This is the largest ripple — plan it as one cohesive change.

**Files:**
- `musefs-format/src/input.rs` (`ArtInput.picture_type`, `ArtInput.data_len`; `BinaryTagInput.len`)
- `musefs-format/src/flac.rs` (208 picture_type; 226, 254, 300-303 guard, 305, 319 data_len; 283, 289, 296 binary len; test closures `mk` at 951 / 1003; test asserts/literals)
- `musefs-format/src/mp3.rs` (199 picture_type; 347-350 binary-len guard, 355, 359, 361; 365-369 art guard, 372, 378 data_len; test closures `mk` at 1032 / 1101; tests 1065, 1099 — see Step 4.10a)
- `musefs-format/src/mp4.rs` (**670-673 binary-len guard removal, 678, 682, 684 binary len**; 690, 699, 711, 713 data_len; 835-836 art filter; tests 1417, 1450, 2348 — see Step 4.10a)
- `musefs-format/src/ogg/mod.rs` (260-264 filter; 297 picture_type; 315, 357, 407, 420, 468, 481 `meta.data_len`; `art_input` helper at 1005; test 1119 — see Step 4.10a)
- `musefs-format/src/wav.rs` (**no `ArtInput`/`BinaryTagInput` literals — verified: synthesis delegates to `build_id3v2_segments`; the `wav_front(data_len)` helper at line 727 is a RIFF `data` chunk size `u64`, NOT `ArtInput.data_len` — DO NOT touch it**)
- `musefs-format/tests/*.rs` — verified literal-bearing files: `proptest_mp4.rs`, `proptest_mp3.rs`, `proptest_wav.rs`, `mp4_oracle.rs`, `synthesize_art.rs`, `mp3_synthesize.rs`, `wav_synthesize.rs`, `roundtrip.rs` (see Step 4.10). `flac_pictures.rs`/`mp3_pictures.rs` `EmbeddedPicture` asserts are handled in Task 3; `proptest_flac.rs` only builds `Vec::<ArtInput>::new()` (no change).
- `musefs-core/src/mapping.rs` (verified: `track_art_to_inputs` lines 33-60 build `ArtInput`; `binary_tags_to_inputs` 62-76 build `BinaryTagInput`; `track_art_images` line 86 reads `a.data_len`)
- `musefs-core/src/error.rs` (add `CoreError::InvalidPictureType` — see Step 4.9)
- Test path: `musefs-format` synthesis tests + `musefs-core/src/mapping.rs` tests

- [ ] **Step 4.1: Write failing bridge tests.** In `musefs-core/src/mapping.rs` `mod tests`, add tests proving the bridge drops zero-length entries and rejects an out-of-range picture type at construction. Because `track_art_to_inputs` reads `picture_type`/`byte_len` from db rows, malformed values are guarded by #199 at the DB but the bridge must still handle the zero-length-drop:
  ```rust
  #[test]
  fn bridge_drops_zero_length_art() {
      use musefs_db::{NewArt, TrackArt};
      let dir = tempfile::tempdir().unwrap();
      let path = dir.path().join("z.db");
      let db = Db::open(&path).unwrap();
      let tid = db
          .upsert_track(&NewTrack {
              backing_path: "/a.flac".into(),
              format: Format::Flac,
              audio_offset: 0,
              audio_length: 0,
              backing_size: 0,
              backing_mtime: 0,
          })
          .unwrap();
      let nonempty = db
          .upsert_art(&NewArt { mime: "image/png".into(), width: None, height: None, data: vec![1, 2, 3] })
          .unwrap();
      let empty = db
          .upsert_art(&NewArt { mime: "image/png".into(), width: None, height: None, data: vec![] })
          .unwrap();
      db.set_track_art(
          tid,
          &[
              TrackArt { art_id: nonempty, picture_type: 3, description: String::new(), ordinal: 0 },
              TrackArt { art_id: empty, picture_type: 3, description: String::new(), ordinal: 1 },
          ],
      )
      .unwrap();
      let inputs = super::track_art_to_inputs(&db, tid).unwrap();
      // The zero-length art is dropped at construction (synthesis would skip it).
      assert_eq!(inputs.len(), 1);
      assert_eq!(inputs[0].art_id, nonempty);
      assert_eq!(inputs[0].data_len.get(), 3);
      assert_eq!(inputs[0].picture_type.get(), 3);
  }
  ```
  Update the EXISTING `track_art_to_inputs_errors_on_negative_byte_len` test: it currently asserts a `byte_len == 0` row still produces an input (line 271-278, `ids == vec![good, bad, zero]`). Under the drop-zero policy the zero row is now dropped, so that assertion changes to `ids == vec![good, bad]`. (The `zero` art's `byte_len` is set to 0 by the raw UPDATE at line 274.) Update that test's expected `ids` accordingly.

- [ ] **Step 4.2: Run + see fail.** `cargo test -p musefs-core bridge_drops_zero_length_art` — fails to compile (`data_len`/`picture_type` are still raw; `.get()` missing).

- [ ] **Step 4.3: Change the DTO field types.** In `musefs-format/src/input.rs`:
  - `ArtInput.picture_type` (line 27): `pub picture_type: PictureType,`
  - `ArtInput.data_len` (line 30): `pub data_len: BlobLen,`
  - `BinaryTagInput.len` (line 54): `pub len: BlobLen,`

- [ ] **Step 4.4: Fix the FLAC synthesis read sites.** In `musefs-format/src/flac.rs`:
  - 208: `out.extend_from_slice(&art.picture_type.get().to_be_bytes());`
  - 226: `&u32::try_from(art.data_len.get())…`
  - 254: the `data_len > 0` filter — now every `ArtInput.data_len` is non-zero, so the filter is `arts.iter().count()` (every art is non-empty). Replace `arts.iter().filter(|a| a.data_len > 0).count()` with `arts.len()`.
  - 300-303: the `if art.data_len == 0 { continue; }` guard can never fire — remove the guard (the loop body runs for every art).
  - 305: `let body_len = framing.len() as u64 + art.data_len.get();`
  - 319: `len: art.data_len.get(),`
  - binary side: 283 `if bt.len > MAX_BLOCK_BODY` → `if bt.len.get() > MAX_BLOCK_BODY`; 289 `crate::convert::usize_from(bt.len.get())`; 296 `len: bt.len.get(),`.

- [ ] **Step 4.5: Fix the MP3 synthesis read sites.** In `musefs-format/src/mp3.rs`:
  - 199: `d.push(art.picture_type.get() as u8);`
  - binary: 347 `if bt.len == 0` guard — unreachable, remove it (every `bt.len` is non-zero); 355 `usize_from(bt.len.get())`; 359 `len: bt.len.get(),`; 361 `frames_len += 10 + bt.len.get();`
  - art: 365 `if art.data_len == 0` guard — unreachable, remove; 372 `framing.len() as u64 + art.data_len.get()`; 378 `len: art.data_len.get(),`.

- [ ] **Step 4.6: Fix the MP4 synthesis read sites.** In `musefs-format/src/mp4.rs`, the binary-tag loop in `build_udta` (verified lines 669-685) and the covr loop (687-714):
  - **670-673: remove the `if bt.len == 0 { … continue; }` guard entirely** — `bt.len` is now a non-zero `BlobLen`, so the guard can never fire (it would also fail to compile: `BlobLen == 0`). Delete the three-line `if` block (including its `EmptySegment` comment); the loop body runs for every binary tag.
  - 678: `ilst_inline.extend_from_slice(&freeform_binary_prefix(mean, name, bt.len.get())?);`
  - 682: `len: bt.len.get(),`
  - 684: `streamed_total += bt.len.get();`
  - 690: `let covr_size: u64 = 8 + arts.iter().map(|a| 16 + a.data_len.get()).sum::<u64>();`
  - 699: `let data_size = 8 + 8 + a.data_len.get();`
  - 711: `len: a.data_len.get(),`
  - 713: `streamed_total += a.data_len.get();`
  - 835-836 (in `synthesize_layout`, BEFORE `build_udta` is called): the `let arts: Vec<ArtInput> = arts.iter().filter(|a| a.data_len > 0).cloned().collect();` filter is now total. Replace with `let arts: Vec<ArtInput> = arts.to_vec();` and delete the preceding "Skip zero-byte art" comment. (Keep the `let arts` rebinding — `build_udta` takes `&[ArtInput]` and the local Vec keeps the call shape; `arts.to_vec()` is the idiomatic clippy-clean form.)

- [ ] **Step 4.7: Fix the Ogg synthesis read sites.** In `musefs-format/src/ogg/mod.rs` (`meta` is `&ArtInput`):
  - 260-264: the `let arts: Vec<OggArt> = arts.iter().filter(|a| a.meta.data_len > 0).copied().collect();` filter is now total. **Delete the whole rebinding (lines 257-264, including the "Exclude zero-byte art" comment) and use the `arts` parameter directly** in the following `build_packets_with_art(header, tags, arts)?` call. Keeping `arts.iter().copied().collect()` without the filter would be a needless clone that `clippy -D warnings` may flag — drop the rebinding entirely instead.
  - 297: `out.extend_from_slice(&art.picture_type.get().to_be_bytes());`
  - 315: `&u32::try_from(art.data_len.get())…`
  - 357: `+ b64_len(a.meta.data_len.get());`
  - 407: `crate::convert::usize_from(b64_len(art.meta.data_len.get()))`
  - 420: `art_total: art.meta.data_len.get(),`
  - 468: `let body_len = prefix.len() as u64 + art.meta.data_len.get();`
  - 481: `art_total: art.meta.data_len.get(),`
  - (`picture_prefix` at 297/315 takes a `&ArtInput`, so its `art.picture_type`/`art.data_len` reads are the ones above.)

- [ ] **Step 4.8: WAV synthesis — no change.** Verified: `wav.rs` `synthesize_layout` (line 255) delegates art/binary handling to `crate::mp3::build_id3v2_segments(tags, binary_tags, arts)` and reads NEITHER `a.data_len` NOR `bt.len` directly — the zero-drop and `.get()` calls all happen inside `build_id3v2_segments` (Step 4.5). WAV has no `ArtInput`/`BinaryTagInput` literals in its own test module, and the `wav_front(data_len)` helper at line 727 is an unrelated RIFF `data`-chunk size `u64` — DO NOT touch it. WAV synthesis needs no edit; only the `tests/wav_synthesize.rs` / `tests/proptest_wav.rs` literals (Step 4.10) change.

- [ ] **Step 4.9: Fix the bridge in `mapping.rs`.** First add the picture-type error variant to `musefs-core/src/error.rs` `enum CoreError` (verified: it has `OrphanedArt { track_id, art_id }` but NO picture-type variant). `OrphanedArt` is the wrong label here (the art row is present, not orphaned), so add a dedicated variant and map it to `EIO` alongside the others (match the existing `OrphanedArt` mapping in the FUSE/errno layer — grep for `OrphanedArt` to find the errno arm and add `InvalidPictureType` next to it):
  ```rust
  #[error("track {track_id} art {art_id} has out-of-range picture_type {value} (expected 0..=20)")]
  InvalidPictureType { track_id: i64, art_id: i64, value: u32 },
  ```
  **The `errno` match in `musefs-fuse/src/lib.rs:82-93` is exhaustive over `CoreError`** (verified) — add `| CoreError::InvalidPictureType { .. }` to the existing `=> fuser::Errno::EIO` arm (next to `OrphanedArt` at line 91), or `musefs-fuse` fails to compile (non-exhaustive match) and the commit lands red. (`reply_errno` at line 100 has a `_ =>` wildcard — verified — so it needs no change.)

  Then rewrite the `track_art_to_inputs` loop body (verified lines 33-60; the `ArtInput { … }` push is at 51-59) to construct `PictureType`/`BlobLen`, dropping zero-length art and surfacing an out-of-range picture type (`#199` CHECK makes the latter unreachable in practice — defense-in-depth):
  ```rust
  let Some(data_len) = musefs_format::BlobLen::new(meta.byte_len) else {
      continue; // zero-length art: synthesis would skip it anyway (now type-level).
  };
  let Some(picture_type) = musefs_format::PictureType::new(ta.picture_type) else {
      return Err(crate::error::CoreError::InvalidPictureType {
          track_id,
          art_id: ta.art_id,
          value: ta.picture_type,
      });
  };
  inputs.push(ArtInput {
      art_id: ta.art_id,
      mime: meta.mime,
      description: ta.description,
      picture_type,
      width: meta.width.unwrap_or(0),
      height: meta.height.unwrap_or(0),
      data_len,
  });
  ```
  (`ta.picture_type` is `u32` and `meta.byte_len` is `u64` — verified — so `PictureType::new`/`BlobLen::new` typecheck directly.)
  Fix `binary_tags_to_inputs` (verified lines 62-76; the `.map(...)` is at 70-74) to drop zero-length tags:
  ```rust
  .filter_map(|row| {
      musefs_format::BlobLen::new(row.byte_len).map(|len| BinaryTagInput {
          key: row.key,
          payload_id: row.rowid,
          len,
      })
  })
  ```
  Fix `track_art_images` (verified line 86): `db.read_art_chunk(a.art_id, 0, musefs_db::convert::usize_from(a.data_len.get()))?`.

- [ ] **Step 4.10: Fix every (non-zero-feeding) format test literal.** Update all `ArtInput { … picture_type: N, … data_len: L, … }` and `BinaryTagInput { … len: L }` literals that pass NON-ZERO values, across `musefs-format/src/{flac,mp3,mp4,ogg/mod}.rs` test modules and the verified `musefs-format/tests/*.rs` files:
  - `picture_type: 3` → `picture_type: PictureType::new(3).unwrap()`
  - `data_len: 40` → `data_len: BlobLen::new(40).unwrap()`
  - `len: 7` → `len: BlobLen::new(7).unwrap()`
  - Closures fed only non-zero values — `let mk = |data_len: u64| ArtInput { … data_len: BlobLen::new(data_len).unwrap(), … picture_type: PictureType::new(3).unwrap(), … }` (verified: `flac.rs:951`, `mp3.rs:1032`, `mp3.rs:1101`, `ogg/mod.rs:1354`), `let mk = |len: u64| BinaryTagInput { … len: BlobLen::new(len).unwrap() }` (`flac.rs:1003`), and `fn art_input(art_id, mime, len: usize)` (`ogg/mod.rs:1005`, used by several non-skip tests): wrap inside the body so callers keep passing the raw integer. The `art_input` helper also has `picture_type: 3` → wrap it too.
  - proptest/oracle literals (`tests/proptest_mp4.rs:23,57`, `tests/proptest_mp3.rs:114`, `tests/proptest_wav.rs:166`, `tests/mp4_oracle.rs:85,94`, `tests/synthesize_art.rs:17-18,106`, `tests/roundtrip.rs`, `tests/mp3_synthesize.rs`, `tests/wav_synthesize.rs`): wrap `len`/`data_len`/`picture_type`. For proptest generators producing `len`/`data_len`, constrain the strategy to non-zero (`1u64..=N`) and wrap, OR `prop_filter` out zero — note the coverage change in a comment. (Confirm each file's exact literals via grep before editing; the line numbers above are the current anchors.)

- [ ] **Step 4.10a: Dispose of the six in-synthesis zero-skip tests (verified inventory + per-test disposition).** With `BlobLen` non-zero, these tests can no longer construct a `data_len: 0` literal AND their assertion (an in-synthesis skip that no longer exists) is dead. The zero-drop is now covered upstream by `bridge_drops_zero_length_art` (Step 4.1). Disposition per test:
  - **`musefs-format/src/mp3.rs:1065` `build_id3v2_segments_skips_zero_byte_art`** — sole purpose is the in-synthesis skip of a `data_len: 0` art. **DELETE** (remove the whole `#[test] fn`).
  - **`musefs-format/src/mp3.rs:1099` `build_id3v2_segments_keeps_real_art_when_mixed_with_empty`** — feeds `mk(1, 0)` + `mk(2, 16)`, asserts only `(2,16)` emits. The `mk(1, 0)` is now unconstructible. **MIGRATE:** drop the zero entry, feed `&[mk(2, 16)]`, and assert `art_segs == vec![(2_i64, 16_u64)]` (the `mk` closure is migrated per Step 4.10 to wrap `BlobLen::new(data_len).unwrap()`; it is now only fed non-zero). The migrated test still pins that a real art emits one `ArtImage` segment with the right `(art_id, len)`.
  - **`musefs-format/src/mp4.rs:1417` `synthesize_skips_zero_length_art`** — sole purpose is the in-synthesis skip (asserts no `ArtImage`/`covr` for a `data_len: 0` art). **DELETE.**
  - **`musefs-format/src/mp4.rs:1450` `synthesize_picks_first_nonempty_art`** — feeds `empty` (`data_len: 0`) + `real` (`data_len: 40`), asserts `Segment::ArtImage { art_id: 9, len: 40 }` is served. **MIGRATE:** drop the `empty` literal, feed `&[real]`, keep the same assertion (the real art with `data_len: BlobLen::new(40).unwrap()` still streams). Rename intent to "real art is served" or keep the name; the point that art reaches synthesis survives.
  - **`musefs-format/src/mp4.rs:2348` `synthesize_layout_emits_all_nonzero_arts`** — its `art = |id, len|` closure feeds `art(1,5), art(2,0), art(3,7)`, asserts `art_segs == vec![(1,5),(3,7)]`. **MIGRATE:** migrate the closure to wrap `BlobLen::new(len).unwrap()` (Step 4.10), drop the `art(2,0)` entry, feed `&[art(1,5), art(3,7)]`, and keep `assert_eq!(art_segs, vec![(1,5),(3,7)])`. This still pins input-order preservation across multiple streamed arts (its real value).
  - **`musefs-format/src/ogg/mod.rs:1119` `synthesize_opus_skips_zero_byte_art`** — builds `art_input(1, "image/jpeg", 0)` (zero) ahead of a real one, asserts the zero is dropped at synthesis. The `art_input` helper can no longer take `0` (it now wraps `BlobLen::new(len).unwrap()`, which panics on 0). **DELETE** (the in-synthesis ogg filter at 260-264 is removed; the drop is upstream/type-level).
  - After these edits, **remove the now-dead in-synthesis skip branches** they exercised: `flac.rs` `nonempty_art` filter (Step 4.4), `mp3.rs` both `== 0` guards (Step 4.5), `mp4.rs` `bt.len == 0` guard + `data_len > 0` art filter (Step 4.6), `ogg/mod.rs` `filter(|a| a.meta.data_len > 0)` (Step 4.7). Each removed branch is matched by the upstream `mapping.rs` drop (Step 4.9) — verified `mapping.rs` is the SOLE production producer of `ArtInput`/`BinaryTagInput`, so no zero can reach synthesis once the bridge drops it before `BlobLen::new`.

- [ ] **Step 4.11: Run + see pass.** `cargo test -p musefs-format -p musefs-core -p musefs-fuse` — all pass (include `musefs-fuse` because the new `CoreError::InvalidPictureType` variant touches its exhaustive `errno` match). `cargo clippy --all-targets -p musefs-format -p musefs-core -p musefs-fuse -- -D warnings` (watch for now-`unused` imports or dead `MAX`-comparison code revealed by the removed guards).

- [ ] **Step 4.12: Commit.** Stage the type change WITH the bridge, the new error variant, its FUSE errno arm, every synthesis-read fixup, and every test literal/disposition — all in one commit (the pre-commit hook runs the full workspace, so a missing call-site fixup rejects it). Note `musefs-format/src/wav.rs` is NOT staged — it has no edits (Step 4.8):
  ```bash
  git add musefs-format/src/input.rs musefs-format/src/flac.rs musefs-format/src/mp3.rs \
          musefs-format/src/mp4.rs musefs-format/src/ogg/mod.rs \
          musefs-format/tests \
          musefs-core/src/mapping.rs musefs-core/src/error.rs \
          musefs-fuse/src/lib.rs
  git commit -F - <<'EOF'
  feat: type ArtInput/BinaryTagInput lengths as BlobLen, picture_type as PictureType (#200)

  ArtInput.data_len and BinaryTagInput.len become non-zero BlobLen;
  ArtInput.picture_type becomes PictureType. The db->format bridge drops
  zero-length payloads at construction (synthesis already skipped them),
  so the in-synthesis zero-skip branches are removed and the EmptySegment
  invariant is now type-level for Plan D. An out-of-range picture_type at
  the bridge is surfaced as the new CoreError::InvalidPictureType (mapped
  to EIO; #199 CHECK makes it unreachable in practice). The four src tests
  that only exercised the removed in-synthesis zero-skip are deleted; the
  three mixed-art tests are migrated to feed only non-zero art (the drop is
  now covered by mapping's bridge_drops_zero_length_art).

  Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>
  EOF
  ```

---

## Task 5: Update the fuzz crate and full verification

The out-of-workspace `fuzz/` crate constructs `ArtInput`/`BinaryTagInput` and is not built by `cargo build`/`test`/`clippy`; fix it and run the full gate.

**Files:**
- `fuzz/src/lib.rs` (lines 25-33 `arb_arts`, 45-49 `arb_binary_tags`)
- `fuzz/fuzz_targets/ogg.rs` (line 41 `ArtInput` literal)
- Test path: `cargo +nightly fuzz build`

- [ ] **Step 5.1: Fix `arb_arts`.** In `fuzz/src/lib.rs`, the generator builds `picture_type: 0..=20` and `data_len: 0..=8192`. Make picture_type a `PictureType` and data_len a non-zero `BlobLen` (bound `1..=8192` to keep every generated art non-empty):
  ```rust
  out.push(ArtInput {
      art_id: i as i64,
      mime: "image/png".to_string(),
      description: String::arbitrary(u)?,
      picture_type: musefs_format::PictureType::new(u.int_in_range(0..=20u32)?)
          .expect("0..=20 is valid"),
      width: u.int_in_range(0..=4096u32)?,
      height: u.int_in_range(0..=4096u32)?,
      data_len: musefs_format::BlobLen::new(u.int_in_range(1..=8192u64)?)
          .expect("1..=8192 is non-zero"),
  });
  ```
  Import `PictureType`/`BlobLen` via the existing `use musefs_format::{…}` at the top.

- [ ] **Step 5.2: Fix `arb_binary_tags`.** `len` is already `1..=4096` (non-zero); just wrap:
  ```rust
  len: musefs_format::BlobLen::new(u.int_in_range(1..=4096u64)?).expect("1..=4096 is non-zero"),
  ```

- [ ] **Step 5.3: Fix `fuzz/fuzz_targets/ogg.rs` (verified lines 37-50).** The loop draws `let len = u.int_in_range(0..=8192usize)...` then `let bytes = u.bytes(len)...` and builds `ArtInput { picture_type: u.int_in_range(0..=20u32).unwrap_or(3), …, data_len: bytes.len() as u64 }`. Both `len == 0` and a short `u.bytes` read make `data_len == 0` reachable, which would panic `BlobLen::new(...).unwrap()`. Skip the zero case BEFORE constructing the `ArtInput` so empty art is dropped (mirrors the bridge), and wrap both fields:
  ```rust
  let len = u.int_in_range(1..=8192usize).unwrap_or(1);
  let bytes = u.bytes(len).map(<[u8]>::to_vec).unwrap_or_default();
  if bytes.is_empty() {
      continue; // empty art is dropped at the construction boundary.
  }
  inputs.push(ArtInput {
      art_id: i as i64,
      mime: "image/png".to_string(),
      description: String::new(),
      picture_type: musefs_format::PictureType::new(u.int_in_range(0..=20u32).unwrap_or(3))
          .unwrap_or(musefs_format::PictureType::ZERO),
      width: 0,
      height: 0,
      data_len: musefs_format::BlobLen::new(bytes.len() as u64).expect("non-empty"),
  });
  images.push(bytes);
  ```
  (Keep `images.push(bytes)` paired with the input so the later `zip` stays aligned; the `continue` drops both.) Add `PictureType`/`BlobLen` to the `use musefs_format::{…}` import at the top of the target.

- [ ] **Step 5.4: Verify the fuzz build.** `cargo +nightly fuzz build` — must succeed (this is the only way to catch the out-of-workspace breakage). Expect: `Compiling musefs-fuzz` then `Finished` with no errors.

- [ ] **Step 5.5: Commit.**
  ```bash
  git add fuzz/src/lib.rs fuzz/fuzz_targets/ogg.rs
  git commit -F - <<'EOF'
  test(fuzz): build ArtInput/BinaryTagInput with PictureType/BlobLen (#200)

  Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>
  EOF
  ```

- [ ] **Step 5.6: Full workspace verification.** Run, in order, and confirm each is green:
  ```bash
  cargo fmt --all --check
  cargo test
  cargo clippy --all-targets -- -D warnings
  cargo +nightly fuzz build
  ```
  All four must pass. `cargo test` is the workspace suite (excludes FUSE e2e by convention); `clippy --all-targets` compiles benches and tests too. If any is red, fix before considering the plan complete — the pre-commit hook runs fmt + clippy `-D warnings` + the full workspace test suite + ruff and rejects a red commit, so every commit above must already be individually green.

- [ ] **Step 5.7: Mutants feature sanity (optional but recommended).** `cargo build --features mutants -p musefs-db` to confirm the `Track`/`TrackBounds` `Default` derive under the `mutants` feature compiles (Step 1.4 added `#[cfg_attr(feature = "mutants", derive(Default))]` to `TrackBounds`).

---

## Self-review against the spec's Plan C

- **PictureType** (0..=20): defined in `musefs-format` (Task 2), applied to `ArtInput.picture_type` + `EmbeddedPicture.picture_type` (Tasks 3-4); `TrackArt.picture_type` stays raw `u32` guarded by #199. Construction at the FLAC parser (untrusted bytes) and the db→format bridge. ✓
- **TrackBounds**: defined in `musefs-db` (Task 1), `TryFrom`-style checked construction at the `tracks` row reader, embedded in `Track`, `Default` under `mutants`. ✓
- **BlobLen** (non-zero, exact API): defined in `musefs-format` (Task 2), applied ONLY to `ArtInput.data_len` + `BinaryTagInput.len` (Task 4); `layout.rs`/`Segment` untouched (Plan D's territory); `.get()` passes the raw `u64` into the still-`u64` Segment fields. ✓
- **Both construction boundaries** (scan + db→format bridge) in `musefs-core` covered (Tasks 3-4); the scan failure policy (clamp to 0) is documented and matched to current behavior. ✓
- **mutants-Default note**: `TrackBounds` carries the `cfg_attr` derive (Step 1.4). ✓
- **fuzz follow-through**: Task 5 fixes the builders and runs `cargo +nightly fuzz build`. ✓
- Exact signatures for `PictureType`/`BlobLen`/`TrackBounds` match the prompt verbatim (Steps 1.4, 2.3). ✓
- Each type-change commit bundles the field change with all call-site + test fixups to stay green (Tasks 1, 3, 4). ✓
- **Layer-2 row-reader defense (Task 1):** `TrackBounds` rejects an out-of-file audio range at `get_track`/`row_to_track` (`DbError::AudioBoundsOutOfRange` → `CoreError::Db`), beneath Plan A's layer-1 SQLite CHECK. Tested directly by `out_of_range_bounds_error_at_row_read` (Step 1.8), which plants the bad row past the CHECK via `ignore_check_constraints`. The over-EOF *integration* tests are owned by Plan A (they assert the CHECK rejects the write); this plan does not touch `tests/reader.rs`/`tests/external_contract.rs`. The `reader.rs:155` guard becomes logically unreachable but is kept as defense-in-depth. ✓
- **New error variant (Task 4):** the bridge's out-of-range `picture_type` is surfaced via `CoreError::InvalidPictureType` (added to `error.rs` and the FUSE `errno` EIO arm in the SAME commit); `#199` CHECK makes it unreachable in practice. ✓
- **Six zero-feeding format tests (Task 4, Step 4.10a):** 3 DELETED (sole-purpose in-synthesis skip: `mp3.rs:1065`, `mp4.rs:1417`, `ogg/mod.rs:1119`), 3 MIGRATED to feed only non-zero art (`mp3.rs:1099`, `mp4.rs:1450`, `mp4.rs:2348`). The zero-drop they covered is replaced by `mapping.rs::bridge_drops_zero_length_art`. ✓
