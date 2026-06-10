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
| `musefs-format/src/mp4.rs` | mp4 parser/synthesis use `PictureType`/`BlobLen` accessors | modified |
| `musefs-format/src/ogg/mod.rs` | ogg synthesis uses `.get()` on `meta.data_len` | modified |
| `musefs-format/src/wav.rs` (tests) | test `ArtInput`/`BinaryTagInput` literals updated | modified |
| `musefs-format/tests/*.rs` | integration-test `ArtInput`/`BinaryTagInput`/`EmbeddedPicture` literals updated | modified |
| `musefs-core/src/mapping.rs` | db→format bridge: build `ArtInput`/`BinaryTagInput` with `PictureType::new`/`BlobLen::new`, dropping zero-length entries at construction | modified |
| `musefs-core/src/scan.rs` | scan boundary: `pic.picture_type` is now a validated `PictureType` (clamp redundancy removed); `track.bounds()` reads in db-comparison tests | modified |
| `musefs-core/src/reader.rs` | read hot path reads `track.bounds().audio_offset()` / `.audio_length()` | modified |
| `musefs-core/tests/*.rs` | `Track` audio-field reads updated to `bounds()` accessors | modified |
| `fuzz/src/lib.rs` | `arb_arts`/`arb_binary_tags` build `PictureType`/`BlobLen` (drop zero-length art) | modified |
| `fuzz/fuzz_targets/ogg.rs` | `ArtInput` literal updated | modified |

---

## Resolved ambiguities (read before starting)

These two design decisions were resolved against the actual code; the tasks below depend on them.

1. **`BlobLen` is non-zero, but `data_len == 0` / `len == 0` are today legal, load-bearing `ArtInput`/`BinaryTagInput` values.** Every synthesis path filters degenerate empty art/tags by an in-band zero check — `flac.rs:254,301`, `mp3.rs:347,365`, `mp4.rs:836`, `ogg/mod.rs:262` — with comments calling out that a zero-length `ArtImage`/`BinaryTag` segment would fail `RegionLayout::validate` (`EmptySegment`). A strictly non-zero `BlobLen` makes `data_len == 0` unrepresentable. **Resolution:** move the zero-drop to the *construction boundaries* (the db→format bridge in `mapping.rs`, the scan path, and the fuzz builders). A zero-length entry is dropped before the `ArtInput`/`BinaryTagInput` is built — observably identical to today, since synthesis would have skipped it anyway. The now-unreachable in-synthesis `== 0` skip branches are removed (their guard can never fire once the field is a `NonZeroU64`), and the format-internal tests that fed zero-length art/tags to exercise the skip are re-pointed at the construction boundary or deleted as dead. This honors the prompt's "apply `BlobLen` only to the DTO length fields and call `.get()` at segment build" while keeping the EmptySegment invariant intact for Plan D.

2. **Scan-boundary picture-type policy = clamp to 0 (not skip, not error).** Today `scan.rs:571-575` and `:656-660` *clamp* an out-of-range `pic.picture_type` to `0` — never dropping or erroring the picture. Because `EmbeddedPicture.picture_type` becomes a `PictureType` built in the format parsers, the validation moves to the actual untrusted-bytes entry point, the FLAC parser (`flac.rs:386`, the only parser reading a picture-type from raw file bytes; mp3 derives it from the already-valid `id3` enum, mp4 hardcodes `3`). To preserve today's observable behavior, the FLAC parser clamps an out-of-range byte to `PictureType::new(0)` (`PictureType::new(raw).unwrap_or(PictureType::ZERO)`), and `scan.rs`'s now-redundant `if pic.picture_type <= 20 { … } else { 0 }` clamp is replaced by `pic.picture_type.get()`. The integration test asserts that a FLAC file with an out-of-range picture-type byte is ingested with stored picture_type `0`.

3. **`TrackBounds` fits `Track`'s row reader cleanly.** `Track` is constructed as a struct literal in exactly ONE place — `row_to_track` (`tracks.rs:35`) — so `TrackBounds::new` has a single call site. `NewTrack` (the write DTO) keeps raw `u64` fields and is unaffected. Consumers of `track.audio_offset`/`track.audio_length` are bounded and enumerated in Task 1.

---

## Task 1: `TrackBounds` in musefs-db

Define the newtype, add its error variant, embed it in `Track`, and validate it at the row reader. Because changing `Track`'s fields breaks every `track.audio_offset`/`track.audio_length` reader in the workspace, the field change and ALL call-site fixups land in one green commit.

**Files:**
- `musefs-db/src/error.rs` (add variant)
- `musefs-db/src/models.rs` (lines 83-95 `Track`; add `TrackBounds` near it)
- `musefs-db/src/tracks.rs` (lines 31-45 `row_to_track`; tests at 239-265)
- `musefs-db/src/lib.rs` (lines 13-16 re-export block)
- `musefs-core/src/reader.rs` (lines 155, 178, 196-197, 204-205, 245, 249-250, 257, 270-271)
- `musefs-core/src/scan.rs` (test asserts at 1883-1884, 1918)
- `musefs-core/tests/probe_equivalence.rs` (line 42)
- `musefs-core/tests/scan_counters.rs` (lines 147-148)
- Test path: `musefs-db/src/models.rs` (unit tests for `TrackBounds`), `musefs-db/src/tracks.rs` (row-read integration)

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

- [ ] **Step 1.8: Fix db-internal test readers.** In `musefs-db/src/tracks.rs` the `negative_audio_bounds_tests` mod (239-265) still passes — it only asserts `get_track(id).is_err()` on a negative `audio_offset` (the `u64` row-read already fails). No body change needed there. Add a new test in that mod proving the `TrackBounds` invariant fires on a bounds violation that the per-column `u64` read alone would NOT catch:
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
      // offset+length now exceeds backing_size: caught by TrackBounds, not by the u64 read.
      db.conn
          .execute("UPDATE tracks SET audio_length = 5 WHERE id = ?1", [id])
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

- [ ] **Step 1.12: Fix core test consumers.** Update the `Track`-typed reads:
  - `musefs-core/src/scan.rs:1883-1884`: `assert_eq!(track.bounds.audio_offset(), full.bounds.audio_offset());` / `assert_eq!(track.bounds.audio_length(), full.bounds.audio_length());`
  - `musefs-core/src/scan.rs:1918`: `assert_eq!(usize_from(track.bounds.audio_length()), b"DIFFERENT-AUDIO".len());`
  - `musefs-core/tests/probe_equivalence.rs:42`: `out.push((t.backing_path, t.bounds.audio_offset(), t.bounds.audio_length(), tags, art));`
  - `musefs-core/tests/scan_counters.rs:147-148`: `assert_eq!(track.bounds.audio_offset(), o_track.bounds.audio_offset());` / `assert_eq!(track.bounds.audio_length(), o_track.bounds.audio_length());`

- [ ] **Step 1.13: Run + see workspace green.** `cargo test -p musefs-db -p musefs-core` — all pass. Then `cargo clippy --all-targets -p musefs-db -p musefs-core -- -D warnings`.

- [ ] **Step 1.14: Commit.** Stage exactly the files touched:
  ```bash
  git add musefs-db/src/error.rs musefs-db/src/models.rs musefs-db/src/tracks.rs \
          musefs-db/src/lib.rs musefs-core/src/reader.rs musefs-core/src/scan.rs \
          musefs-core/tests/probe_equivalence.rs musefs-core/tests/scan_counters.rs
  git commit -F - <<'EOF'
  feat(db): add validated TrackBounds newtype (#200)

  TrackBounds wraps tracks.audio_offset/audio_length with a checked
  constructor enforcing offset + length <= backing_size at the row
  reader — the untrusted SQL->type boundary. Track embeds it; the
  reader hot path and tests read via the accessors.

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
- `musefs-format/src/input.rs` (line 38 `EmbeddedPicture.picture_type`)
- `musefs-format/src/flac.rs` (line 386 read, 418-425 construct, 675 test assert, 955 test literal)
- `musefs-format/src/mp3.rs` (line 545 construct)
- `musefs-format/src/mp4.rs` (line 460 construct)
- `musefs-core/src/scan.rs` (lines 571-575, 656-660 clamp; test literal at 1048)
- Test path: `musefs-format` parser tests + `musefs-core` scan tests

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
  Update the scan test `EmbeddedPicture` literal at `scan.rs:1048` (`let pic = |n: usize| EmbeddedPicture { … picture_type: n as u32, … }` or similar) to wrap with `PictureType::new(n as u32).unwrap()` (confirm the exact body via `find_symbol`). `TrackArt.picture_type` stays raw `u32` (guarded by the #199 CHECK) — `scan.rs` stores `picture_type` (a `u32` from `.get()`) into `TrackArt`, so no further change there.

- [ ] **Step 3.8: Run + see pass.** `cargo test -p musefs-format -p musefs-core` — all pass, including the new clamp test. `cargo clippy --all-targets -p musefs-format -p musefs-core -- -D warnings`.

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
          musefs-format/src/mp4.rs musefs-core/src/scan.rs musefs-core/tests/scan.rs
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
- `musefs-format/src/input.rs` (lines 27, 30 `ArtInput`; line 54 `BinaryTagInput.len`)
- `musefs-format/src/flac.rs` (208 picture_type; 226, 254, 301, 305, 319 data_len; 283, 289, 296 binary len; test literals 949-955, 999-1003)
- `musefs-format/src/mp3.rs` (199 picture_type; 347, 355, 359, 361 binary len; 365, 372, 378 data_len; test literals)
- `musefs-format/src/mp4.rs` (678 binary len; 690, 699, 711, 713, 836 data_len; many test literals)
- `musefs-format/src/ogg/mod.rs` (262, 297, 315, 357, 407, 420, 468, 481 — `meta.data_len` + picture_type; test literals)
- `musefs-format/src/wav.rs` (test literals only)
- `musefs-format/tests/*.rs` (all `ArtInput`/`BinaryTagInput` literals — see ripple list)
- `musefs-core/src/mapping.rs` (lines 50-58 `ArtInput`, 71-75 `BinaryTagInput`, 86 `read_art_chunk(... a.data_len ...)`)
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

- [ ] **Step 4.6: Fix the MP4 synthesis read sites.** In `musefs-format/src/mp4.rs`:
  - 678: `freeform_binary_prefix(mean, name, bt.len.get())?`
  - 690: `…map(|a| 16 + a.data_len.get()).sum::<u64>()`
  - 699: `let data_size = 8 + 8 + a.data_len.get();`
  - 711: `len: a.data_len.get(),`
  - 713: `streamed_total += a.data_len.get();`
  - 836: the `arts.iter().filter(|a| a.data_len > 0)` filter is now total — replace with `arts.to_vec()` (or drop the filter and keep `arts.iter().cloned().collect()`). Keep the binding shape `let arts: Vec<ArtInput> = …;`.

- [ ] **Step 4.7: Fix the Ogg synthesis read sites.** In `musefs-format/src/ogg/mod.rs` (`meta` is `&ArtInput`):
  - 262: `.filter(|a| a.meta.data_len > 0)` is now total — remove the filter (the `arts` are all non-empty); keep `.copied().collect()`.
  - 297: `out.extend_from_slice(&art.picture_type.get().to_be_bytes());`
  - 315: `&u32::try_from(art.data_len.get())…`
  - 357: `+ b64_len(a.meta.data_len.get());`
  - 407: `crate::convert::usize_from(b64_len(art.meta.data_len.get()))`
  - 420: `art_total: art.meta.data_len.get(),`
  - 468: `let body_len = prefix.len() as u64 + art.meta.data_len.get();`
  - 481: `art_total: art.meta.data_len.get(),`
  - (`picture_prefix` at 297/315 takes a `&ArtInput`, so its `art.picture_type`/`art.data_len` reads are the ones above.)

- [ ] **Step 4.8: Fix the WAV synthesis (if any data_len read).** WAV synthesis filters zero art in `synthesize_layout`; confirm via `find_symbol` whether it reads `a.data_len` directly. If it has a `data_len > 0` filter, make it total like the others; otherwise WAV touches only test literals.

- [ ] **Step 4.9: Fix the bridge in `mapping.rs`.** Rewrite `track_art_to_inputs` (lines 50-58) to construct `PictureType`/`BlobLen`, dropping zero-length art:
  ```rust
  let Some(data_len) = musefs_format::BlobLen::new(meta.byte_len) else {
      continue; // zero-length art: synthesis would skip it anyway.
  };
  let Some(picture_type) = musefs_format::PictureType::new(ta.picture_type) else {
      // #199 CHECK guarantees 0..=20; out of range means a malformed
      // external write that bypassed the constraint — surface it.
      return Err(crate::error::CoreError::OrphanedArt { track_id, art_id: ta.art_id });
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
  (Decide the picture-type-out-of-range error: reuse `OrphanedArt` only if it semantically fits; otherwise add a dedicated `CoreError::InvalidPictureType { track_id, art_id, value }` variant — check `musefs-core/src/error.rs` via `get_symbols_overview` and pick the cleaner option, documenting it in the commit. The simplest spec-faithful choice: the #199 CHECK makes this unreachable in practice, so a dedicated variant kept for defense-in-depth is the honest fit.)
  Fix `binary_tags_to_inputs` (71-75) to drop zero-length tags:
  ```rust
  .filter_map(|row| {
      musefs_format::BlobLen::new(row.byte_len).map(|len| BinaryTagInput {
          key: row.key,
          payload_id: row.rowid,
          len,
      })
  })
  ```
  Fix `track_art_images` (line 86): `usize_from(a.data_len.get())`.

- [ ] **Step 4.10: Fix every format test literal.** Update all `ArtInput { … picture_type: N, … data_len: L, … }` and `BinaryTagInput { … len: L }` literals across `musefs-format/src/{flac,mp3,mp4,ogg/mod,wav}.rs` test modules and `musefs-format/tests/*.rs`:
  - `picture_type: 3` → `picture_type: PictureType::new(3).unwrap()`
  - `data_len: 40` → `data_len: BlobLen::new(40).unwrap()`
  - `len: 7` → `len: BlobLen::new(7).unwrap()`
  - For literals/closures that intentionally pass `0` to exercise the (now-removed) skip (e.g. `mp4.rs:1455` `empty` with `data_len: 0`, `mp3.rs:1070` `empty`, `flac.rs` empty-art tests): these tested the in-synthesis skip that no longer exists. Convert each to the new contract — either drop the zero-art literal from the input vec and assert the remaining non-empty art is served, or delete the test if its sole purpose was the in-synthesis zero-skip (the drop now happens upstream, covered by `bridge_drops_zero_length_art`). Document each deletion in the commit body.
  - Closures like `let mk = |data_len: u64| ArtInput { … data_len, … }` (flac.rs:951, mp3.rs:1032, ogg/mod.rs:1354) and `art_input(art_id, mime, len: usize)` (ogg/mod.rs:1005): change the parameter to take the raw value and wrap inside — `data_len: BlobLen::new(data_len).unwrap()` — OR change the closure signature to accept a `BlobLen`. Pick per call-site readability; the closures are only fed non-zero values except the explicit empty-art tests handled above.
  - proptest/oracle literals (`tests/proptest_mp4.rs:23,57`, `tests/proptest_mp3.rs:114`, `tests/proptest_wav.rs:166`, `tests/mp4_oracle.rs:85,94`, `tests/synthesize_art.rs:17-18,106`, `tests/roundtrip.rs`, `tests/mp3_synthesize.rs`, `tests/wav_synthesize.rs`): wrap `len`/`data_len`/`picture_type`. For proptest generators producing `len`/`data_len`, constrain the strategy to non-zero (e.g. `1u64..=N`) and wrap, OR `prop_filter` out zero — note the coverage change in a comment.

- [ ] **Step 4.11: Run + see pass.** `cargo test -p musefs-format -p musefs-core` — all pass. `cargo clippy --all-targets -p musefs-format -p musefs-core -- -D warnings` (watch for now-`unused` imports or dead `MAX`-comparison code revealed by the removed guards).

- [ ] **Step 4.12: Commit.**
  ```bash
  git add musefs-format/src/input.rs musefs-format/src/flac.rs musefs-format/src/mp3.rs \
          musefs-format/src/mp4.rs musefs-format/src/ogg/mod.rs musefs-format/src/wav.rs \
          musefs-format/tests musefs-core/src/mapping.rs
  # plus musefs-core/src/error.rs if a new variant was added
  git commit -F - <<'EOF'
  feat: type ArtInput/BinaryTagInput lengths as BlobLen, picture_type as PictureType (#200)

  ArtInput.data_len and BinaryTagInput.len become non-zero BlobLen;
  ArtInput.picture_type becomes PictureType. The db->format bridge drops
  zero-length payloads at construction (synthesis already skipped them),
  so the in-synthesis zero-skip branches are removed and the EmptySegment
  invariant is now type-level for Plan D.

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

- [ ] **Step 5.3: Fix `fuzz/fuzz_targets/ogg.rs:41`.** Wrap the `ArtInput` literal's `picture_type`/`data_len` (confirm exact values via reading lines 41-50) with `PictureType::new(...).unwrap()` / `BlobLen::new(...)` — if the target can generate zero data_len, `prop`-style drop it or bound to non-zero.

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
