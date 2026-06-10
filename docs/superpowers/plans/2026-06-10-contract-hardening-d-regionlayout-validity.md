# Plan D — #201: unskippable RegionLayout validity

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make `RegionLayout` impossible to construct or cache in an invalid state — hide unchecked construction, cache validated totals behind a private `segments` field, type metadata-segment lengths with `BlobLen`, and defensively re-validate at the reader's cache boundary.

**Architecture:** This plan is sequenced **after Plan C — #200, which is assumed already merged**: Plan C added the non-zero `BlobLen` newtype (`NonZeroU64`-backed, `new(u64) -> Option<BlobLen>`, `get(self) -> u64`) to `musefs-format` but deliberately left `Segment` untouched — `Segment` is Plan D's territory. Plan D consumes `BlobLen` to type the `ArtImage`/`BinaryTag`/`OggArtSlice` length fields so a zero/impossible metadata length is unrepresentable, makes `RegionLayout::new` non-public (migrating one production site, several same-crate unit tests, and external integration-test crates), caches `total_len`/`header_len` behind a now-private `segments` field, and adds a defensive `validate()` at `reader.rs` before caching.

**Tech Stack:** Rust, musefs-format layout / musefs-core reader

---

## File Structure

| File | Responsibility / change |
| ---- | ----------------------- |
| `musefs-format/src/layout.rs` | `Segment` metadata lengths become `BlobLen`; `RegionLayout` caches `total_len`/`header_len`, field `segments` goes private, `Default` removed; `new` → `pub(crate)` + `#[cfg(any(test, feature = "fuzzing"))] new_unchecked`; `validate()` gains backing-range bounds check. |
| `musefs-format/Cargo.toml` | (no new feature) — reuse the existing `fuzzing` feature to gate the public test constructor. |
| `musefs-format/src/flac.rs`, `mp3.rs`, `mp4.rs`, `wav.rs`, `ogg/mod.rs` | Synthesis builders now construct `Segment::ArtImage`/`BinaryTag`/`OggArtSlice` with `BlobLen`; already call `validated(...)`. |
| `musefs-format/src/fuzz_check.rs` | Same-crate `#[cfg(test)]` builders switch to `new_unchecked` / `BlobLen`. |
| `musefs-format/tests/layout.rs` | External integration tests: valid layouts → `validated`; invalid layouts (empty/overflow) → `new_unchecked` (gated by `fuzzing` dev-dep). |
| `musefs-core/src/reader.rs` | `StructureOnly` raw `new` → `validated`; `.segments` field reads → `.segments()`; cached `total_len` sourced from `layout.total_len()`; defensive `layout.validate()?` before cache insert. |
| `musefs-core/src/facade.rs`, `musefs-core/src/ogg_index.rs`, `musefs-core/tests/read_at.rs` (+ `reader.rs`/`proptest_read_fidelity.rs` test sites) | Field-access and `new` call-site sweep. |

---

## Task 1: Cache totals, privatize `segments`, drop `Default`, accessor sweep

Make `total_len`/`header_len` stored-at-construction values, make the `segments` field private (so a mutator can't desync the cache), hand-remove the derived `Default`, and migrate every direct `.segments` **field** read to the `.segments()` accessor — all in one green commit, because privatizing the field breaks foreign-crate reads simultaneously.

**Files:**
- `musefs-format/src/layout.rs` (struct `RegionLayout` lines 64–125; `new`/`validated`/`segments`/`total_len`/`header_len`/`validate`)
- `musefs-format/src/wav.rs` (line 622, `#[cfg(test)]`)
- `musefs-core/src/reader.rs` (lines 320, 359 production; cached total at 277/293)
- `musefs-core/src/ogg_index.rs` (line 302, `#[cfg(test)]`)
- Test path (regression): `musefs-format/tests/layout.rs`

- [ ] **Step 1: Write a failing regression test that cached totals equal the segment sum.** Append to `musefs-format/tests/layout.rs`:
  ```rust
  #[test]
  fn cached_totals_equal_segment_sum() {
      let layout = RegionLayout::validated(vec![
          Segment::Inline(vec![0u8; 10]),
          Segment::ArtImage {
              art_id: 7,
              len: musefs_format::BlobLen::new(100).unwrap(),
          },
          Segment::Inline(vec![0u8; 5]),
          Segment::BackingAudio {
              offset: 200,
              len: 1000,
          },
      ])
      .unwrap();
      // Stored values, returned without re-summing.
      assert_eq!(layout.total_len(), 10 + 100 + 5 + 1000);
      assert_eq!(layout.header_len(), 10 + 100 + 5);
      // Stored values agree with a fresh sum over the public segment view.
      let sum: u64 = layout.segments().iter().map(Segment::len).sum();
      assert_eq!(layout.total_len(), sum);
  }
  ```
  (This test also exercises the `BlobLen`-typed `ArtImage` introduced in Task 2; for Task 1 alone, temporarily keep `len: 100` raw — see Step 2. The final form above is the Task-2 state. For Task 1 land it with `len: 100`.)

  Task-1 form to commit now:
  ```rust
  #[test]
  fn cached_totals_equal_segment_sum() {
      let layout = RegionLayout::validated(vec![
          Segment::Inline(vec![0u8; 10]),
          Segment::ArtImage { art_id: 7, len: 100 },
          Segment::Inline(vec![0u8; 5]),
          Segment::BackingAudio { offset: 200, len: 1000 },
      ])
      .unwrap();
      assert_eq!(layout.total_len(), 10 + 100 + 5 + 1000);
      assert_eq!(layout.header_len(), 10 + 100 + 5);
      let sum: u64 = layout.segments().iter().map(Segment::len).sum();
      assert_eq!(layout.total_len(), sum);
  }
  ```

- [ ] **Step 2: Run it and watch it fail to compile/assert.** Command:
  ```
  cargo test -p musefs-format --test layout cached_totals_equal_segment_sum
  ```
  Expected: passes today (the accessors re-sum and the field is public) — so this test alone is not yet load-bearing. Land it anyway as the regression anchor; the real fail surfaces when a later mutator-style edit desyncs the cache. To prove it bites, temporarily make `total_len()` return `0` and confirm `assertion left == right` fails, then revert.

- [ ] **Step 3: Rewrite `RegionLayout` to store totals, privatize the field, and drop `Default`.** Replace the struct + `impl` head in `musefs-format/src/layout.rs` (lines 64–108):
  ```rust
  /// An ordered description of a synthesized virtual file: the metadata region
  /// (inline framing + art images) followed by the backing audio. Totals are
  /// computed once at construction; `segments` is private so they cannot desync.
  #[derive(Debug, Clone, PartialEq, Eq)]
  pub struct RegionLayout {
      segments: Vec<Segment>,
      total_len: u64,
      header_len: u64,
  }

  impl RegionLayout {
      fn from_segments(segments: Vec<Segment>) -> RegionLayout {
          let total_len = segments.iter().map(Segment::len).sum();
          let header_len = segments
              .iter()
              .filter(|s| !matches!(s, Segment::BackingAudio { .. } | Segment::OggAudio { .. }))
              .map(Segment::len)
              .sum();
          RegionLayout {
              segments,
              total_len,
              header_len,
          }
      }

      pub(crate) fn new(segments: Vec<Segment>) -> RegionLayout {
          RegionLayout::from_segments(segments)
      }

      pub fn validated(segments: Vec<Segment>) -> Result<RegionLayout, LayoutError> {
          let layout = RegionLayout::from_segments(segments);
          layout.validate()?;
          Ok(layout)
      }

      /// The ordered segments composing the synthesized virtual file.
      pub fn segments(&self) -> &[Segment] {
          &self.segments
      }

      /// True if any segment streams an opaque binary tag payload from the DB.
      pub fn has_binary_tag(&self) -> bool {
          self.segments
              .iter()
              .any(|s| matches!(s, Segment::BinaryTag { .. }))
      }

      /// Total size of the synthesized virtual file in bytes (stored at construction).
      pub fn total_len(&self) -> u64 {
          self.total_len
      }

      /// Size of the synthesized metadata region preceding the backing audio (stored).
      pub fn header_len(&self) -> u64 {
          self.header_len
      }
  ```
  Leave `validate()` (lines 114–124) as-is for now; it still iterates `&self.segments` (same module, fine). `new` is now `pub(crate)`: same-crate `#[cfg(test)]` and `fuzz_check` callers still compile; the `pub` test constructor for foreign crates lands in Task 3.

- [ ] **Step 3b: Fix the same-crate `validate()` caller of the overflow path.** `validate()` still recomputes `total` for the overflow check; that is correct and must stay (it is the *checked* construction path). No change needed beyond confirming it reads `&self.segments` (private but same-module — OK).

- [ ] **Step 4: Sweep foreign-crate `.segments` field reads to `.segments()`.** The field is now private outside `musefs-format`. Edit:
  - `musefs-core/src/reader.rs:320` `resolved.layout.segments` → `resolved.layout.segments()`
  - `musefs-core/src/reader.rs:359` `for seg in &resolved.layout.segments` → `for seg in resolved.layout.segments()`
  - `musefs-core/src/ogg_index.rs:302` `for seg in &layout.segments` → `for seg in layout.segments()` (this is a `#[cfg(test)]` mod in `musefs-core`, still foreign to `musefs-format`)
  - `musefs-format/src/wav.rs:622` `for s in &layout.segments` → `for s in layout.segments()` (same-crate `#[cfg(test)]`; the field is module-private, so the field read would still compile, but switch to the accessor for consistency)
  - External test crates touched in this commit: `musefs-format/tests/{synthesize_tags.rs, mp3_synthesize.rs, wav_synthesize.rs, synthesize_art.rs, common/mod.rs}` and `musefs-core/tests/{reader.rs, proptest_read_fidelity.rs}` each read `.segments` as a field. Convert every `layout.segments` / `resolved.layout.segments` field read to the `.segments()` accessor. Exact sites (from grep): `synthesize_tags.rs:100-102`, `mp3_synthesize.rs:11,78,135-137,223`, `wav_synthesize.rs:22,96,215,262`, `synthesize_art.rs:47,143,183`, `common/mod.rs:19`, `reader.rs:138,158`, `proptest_read_fidelity.rs:232,331,501,553,781`.

  > NOTE: `.segments().len()` and `.segments()[i]` index access work identically on the returned slice, so `layout.segments.len()` → `layout.segments().len()` and `layout.segments[0]` → `layout.segments()[0]`.

- [ ] **Step 5: Migrate the `total` source in `reader.rs:277` (no behavior change, compile check).** `let total = layout.total_len();` (line 277) now reads the stored value — leave as-is. Confirm `cache_bytes` (line 282) already uses `.segments()`. No edit needed; this step is a verification that the production caching path compiles against the private field.

- [ ] **Step 6: Run the workspace suite green.** Command:
  ```
  cargo test -p musefs-format -p musefs-core
  ```
  Expected: all pass, including the new `cached_totals_equal_segment_sum`. Then `cargo clippy --all-targets -- -D warnings` (catches any missed field read and an unused-`from_segments` lint if mis-wired) and `cargo fmt --all --check`.

- [ ] **Step 7: Commit.** Stage exactly the touched files:
  ```
  git add musefs-format/src/layout.rs musefs-format/src/wav.rs musefs-format/tests/layout.rs \
    musefs-format/tests/synthesize_tags.rs musefs-format/tests/mp3_synthesize.rs \
    musefs-format/tests/wav_synthesize.rs musefs-format/tests/synthesize_art.rs \
    musefs-format/tests/common/mod.rs \
    musefs-core/src/reader.rs musefs-core/src/ogg_index.rs \
    musefs-core/tests/reader.rs musefs-core/tests/proptest_read_fidelity.rs
  git commit -F - <<'EOF'
  refactor(format): cache RegionLayout totals, privatize segments field (#201)

  Compute total_len/header_len once at construction and store them; the
  accessors return stored values instead of re-summing. Make the segments
  field private (a mutator would desync the cache) and drop derived Default.
  Sweep all direct .segments field reads to the .segments() accessor.

  Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>
  EOF
  ```

---

## Task 2: Type `Segment` metadata lengths with `BlobLen`; strengthen `validate()`

Replace the raw `u64` length on `ArtImage`/`BinaryTag`/`OggArtSlice` with Plan C's non-zero `BlobLen`, making a zero-length metadata segment unrepresentable. Add a backing-range bounds check to `validate()`. All `Segment` literals across the workspace change shape in this one commit.

**Files:**
- `musefs-format/src/layout.rs` (`Segment` enum lines 12–62; `validate` lines 114–124; `LayoutError` lines 1–10)
- `musefs-format/src/{flac.rs, mp3.rs, mp4.rs, wav.rs, ogg/mod.rs}` (segment construction)
- `musefs-format/src/fuzz_check.rs` (same-crate `#[cfg(test)]` literals)
- All `musefs-format/tests/*.rs` and `musefs-core` literals that build these three variants
- Test path: `musefs-format/tests/layout.rs`

- [ ] **Step 1: Write failing tests for the new invariants.** Append to `musefs-format/tests/layout.rs`:
  ```rust
  #[test]
  fn zero_length_metadata_is_unrepresentable() {
      // BlobLen::new(0) is None — a zero-length art/binary segment cannot be built.
      assert!(musefs_format::BlobLen::new(0).is_none());
  }

  #[test]
  fn out_of_bounds_backing_range_is_rejected() {
      // A backing run whose offset+len overflows u64 must fail validation.
      let err = RegionLayout::validated(vec![Segment::BackingAudio {
          offset: u64::MAX,
          len: 1,
      }]);
      assert_eq!(err, Err(LayoutError::BackingRangeOverflow));
  }
  ```

- [ ] **Step 2: Run and watch it fail to compile.** Command:
  ```
  cargo test -p musefs-format --test layout out_of_bounds_backing_range_is_rejected
  ```
  Expected: compile error — `LayoutError::BackingRangeOverflow` does not exist; `BlobLen` may already exist from Plan C (the first test then passes once it compiles).

- [ ] **Step 3: Add the new `LayoutError` variant and the `BlobLen` import.** Edit `musefs-format/src/layout.rs` top:
  ```rust
  use crate::BlobLen;

  /// Validation errors discovered in a layout at synthesis time.
  #[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
  pub enum LayoutError {
      /// A segment reported zero length.
      #[error("a segment reported zero length")]
      EmptySegment,
      /// Total length overflowed u64.
      #[error("total layout length overflowed u64")]
      TotalOverflow,
      /// A backing-audio run's offset + length overflowed u64.
      #[error("backing-audio range offset + length overflowed u64")]
      BackingRangeOverflow,
  }
  ```
  (Confirm Plan C re-exported `BlobLen` from `crate` — grep `pub use` for it; if it lives at `crate::blob_len::BlobLen`, adjust the `use` path. The lib re-export at `lib.rs:19` should be extended to include `BlobLen` if Plan C didn't already; verify and add `pub use layout::...` / `pub use blob_len::BlobLen;` as Plan C placed it.)

- [ ] **Step 4: Re-type the three metadata variants and update `Segment::len()`.** Edit the `Segment` enum and `impl Segment`:
  ```rust
  pub enum Segment {
      Inline(Vec<u8>),
      ArtImage { art_id: i64, len: BlobLen },
      BackingAudio { offset: u64, len: u64 },
      OggAudio { offset: u64, len: u64, seq_delta: i64 },
      OggArtSlice {
          art_id: i64,
          offset: u64,
          len: BlobLen,
          base64: bool,
          art_total: u64,
      },
      BinaryTag { payload_id: i64, len: BlobLen },
  }

  impl Segment {
      pub fn len(&self) -> u64 {
          match self {
              Segment::Inline(b) => b.len() as u64,
              Segment::ArtImage { len, .. }
              | Segment::OggArtSlice { len, .. }
              | Segment::BinaryTag { len, .. } => len.get(),
              Segment::BackingAudio { len, .. } | Segment::OggAudio { len, .. } => *len,
          }
      }

      pub fn is_empty(&self) -> bool {
          self.len() == 0
      }
  }
  ```

- [ ] **Step 5: Strengthen `validate()` with the backing-range bounds check.** Because metadata lengths are now non-zero by type, the `EmptySegment` arm can never trigger for `ArtImage`/`OggArtSlice`/`BinaryTag` — keep the arm (it still guards an empty `Inline(vec![])`). Add the backing overflow check:
  ```rust
  pub fn validate(&self) -> Result<(), LayoutError> {
      let mut total: u64 = 0;
      for seg in &self.segments {
          let len = seg.len();
          if len == 0 && !matches!(seg, Segment::BackingAudio { .. } | Segment::OggAudio { .. }) {
              return Err(LayoutError::EmptySegment);
          }
          if let Segment::BackingAudio { offset, len }
          | Segment::OggAudio { offset, len, .. } = seg
          {
              offset
                  .checked_add(*len)
                  .ok_or(LayoutError::BackingRangeOverflow)?;
          }
          total = total.checked_add(len).ok_or(LayoutError::TotalOverflow)?;
      }
      Ok(())
  }
  ```

- [ ] **Step 6: Fix every `ArtImage`/`BinaryTag`/`OggArtSlice` literal in the workspace.** Each `len: <expr>` becomes `len: BlobLen::new(<expr>).expect("…non-zero")` in production synthesis paths (where the value is already known non-zero), or `len: BlobLen::new(<expr>).unwrap()` in tests. Sites:
  - **Production synthesis** (`flac.rs`, `mp3.rs`, `mp4.rs`, `wav.rs`, `ogg/mod.rs`): wherever these crates build the three variants. Art/binary payload lengths come from `ArtInput`/`BinaryTagInput` (Plan C typed those with `BlobLen` at the DB/scan boundary), so prefer threading the existing `BlobLen` through rather than `new(...).unwrap()`. Where a raw `u64` is still in hand and provably non-zero, use `BlobLen::new(x).expect("art/tag payload length is non-zero")` and leave a one-line WHY comment only if the non-zero source is non-obvious.
  - **Same-crate tests** `fuzz_check.rs` (any literal building these variants — none in lines 324–404 today, but sweep the whole file) and `layout.rs` unit `tests` mod (`binary_tag_segment_len_and_validate` at line 132 builds `BinaryTag { len: 12 }` and `{ len: 0 }`). The `len: 0` case becomes impossible to express — replace that sub-assertion with `assert!(BlobLen::new(0).is_none());` and keep the non-empty validation path with `len: BlobLen::new(12).unwrap()`.
  - **External tests**: `tests/read_at.rs:114` (`ArtImage { len: art.len() as u64 }` → `BlobLen::new(art.len() as u64).unwrap()`), and the OggArtSlice/BinaryTag/ArtImage literals in `reader.rs` unit tests (lines 769, 820, 852-no, 1233) and `proptest_read_fidelity.rs` / `synthesize_art.rs`. Grep `ArtImage {`, `OggArtSlice {`, `BinaryTag {` across `musefs-format` and `musefs-core` and fix each.

- [ ] **Step 7: Run green.** Commands:
  ```
  cargo test -p musefs-format -p musefs-core
  cargo clippy --all-targets -- -D warnings
  cargo fmt --all --check
  ```
  Expected: `zero_length_metadata_is_unrepresentable` and `out_of_bounds_backing_range_is_rejected` pass; all migrated literals compile.

- [ ] **Step 8: Commit.** Stage by exact name (all files touched in Step 6 — list them explicitly, no `git add -A`):
  ```
  git add musefs-format/src/layout.rs musefs-format/src/flac.rs musefs-format/src/mp3.rs \
    musefs-format/src/mp4.rs musefs-format/src/wav.rs musefs-format/src/ogg/mod.rs \
    musefs-format/src/fuzz_check.rs musefs-format/tests/layout.rs musefs-format/tests/read_at.rs \
    <every other touched test file>
  git commit -F - <<'EOF'
  feat(format): type Segment metadata lengths with BlobLen; backing-range check (#201)

  ArtImage/BinaryTag/OggArtSlice carry a non-zero BlobLen, making a
  zero-length metadata segment unrepresentable. validate() additionally
  rejects a backing-audio range whose offset+len overflows u64.

  Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>
  EOF
  ```

---

## Task 3: Hide `new`; migrate all call sites; expose `new_unchecked` for invalid-layout tests

`new` was already demoted to `pub(crate)` in Task 1, which keeps same-crate `#[cfg(test)]` and `fuzz_check` callers compiling. This task migrates the **production** `StructureOnly` site and the **external integration-test** crates that can no longer see `new`, deciding per site between `validated(...)` and a deliberately-public `new_unchecked` (gated behind the existing `fuzzing` feature).

**Files:**
- `musefs-format/src/layout.rs` (add `new_unchecked`)
- `musefs-core/src/reader.rs:144` (production `StructureOnly`)
- `musefs-format/tests/layout.rs` (external; mixes valid + intentionally-invalid layouts)
- `musefs-core/tests/read_at.rs:114` (external; valid layout)
- Test path: `musefs-format/tests/layout.rs`

- [ ] **Step 1: Add the public, feature-gated `new_unchecked` constructor.** Insert into `impl RegionLayout` in `musefs-format/src/layout.rs`, right after `validated`:
  ```rust
  /// Build a layout **without** validation. Test-only escape hatch for
  /// integration tests that deliberately construct invalid layouts to exercise
  /// `validate()`. Gated behind the `fuzzing` feature so production code (which
  /// has only `validated`) cannot reach it.
  #[cfg(feature = "fuzzing")]
  pub fn new_unchecked(segments: Vec<Segment>) -> RegionLayout {
      RegionLayout::from_segments(segments)
  }
  ```
  No `Cargo.toml` edit is required: the `fuzzing` feature already exists (`musefs-format/Cargo.toml:17`) and is already enabled for `musefs-format`'s own test build (self dev-dep, line 29) and for `musefs-core`'s test build (`musefs-core/Cargo.toml:33`, `features = ["fuzzing"]`). External test crates therefore already see `new_unchecked` — no feature wiring to add.

- [ ] **Step 2: Migrate the production `StructureOnly` site.** Edit `musefs-core/src/reader.rs:144`. The single `BackingAudio` layout is always valid, so use `validated` and surface the (practically-unreachable) error as the layout/synthesis error path:
  ```rust
  Mode::StructureOnly => {
      // Pure passthrough: the synthesized "file" is the backing file itself.
      // The stored audio bounds are irrelevant here — the whole file is served
      // verbatim — so they are not validated in this mode.
      let layout = RegionLayout::validated(vec![Segment::BackingAudio {
          offset: 0,
          len: meta.len(),
      }])?;
      (layout, meta.len(), track.backing_mtime)
  }
  ```
  Confirm `CoreError: From<LayoutError>` exists (synthesis already does `RegionLayout::validated(segments)?` in the format crate and `?`-propagates through `synthesize_layout` → `CoreError::Format`); the `?` here needs `LayoutError → CoreError`. Check `musefs-core/src/error.rs` — if `LayoutError` is not yet a `CoreError` source (today it only ever flows up wrapped in `FormatError`), add a `#[from] LayoutError` arm (e.g. `CoreError::Layout(LayoutError)`) and map it to the same errno the format synthesis errors use. Verify with `find_referencing_symbols` on `LayoutError`.

- [ ] **Step 3: Migrate the external `read_at.rs` test (valid layout → `validated`).** Edit `musefs-core/tests/read_at.rs:114`:
  ```rust
  let layout = RegionLayout::validated(vec![
      Segment::Inline(vec![0xAA, 0xBB]),
      Segment::ArtImage {
          art_id,
          len: musefs_format::BlobLen::new(art.len() as u64).unwrap(),
      },
  ])
  .unwrap();
  ```

- [ ] **Step 4: Migrate `musefs-format/tests/layout.rs` per call site.** Decision rule: a layout that *should* be valid uses `validated(...).unwrap()`; a layout built **to exercise a validation failure** uses `new_unchecked(...)` then asserts `validate()`. Classify the existing tests:
  - `lengths_sum_segments_and_exclude_audio_from_header` (line 5): valid → `validated(...).unwrap()`.
  - `empty_single_segment_layout_fails_validation` (line 31, `Inline(vec![])`): **intentionally invalid** → `new_unchecked(...)` then `assert_eq!(layout.validate(), Err(LayoutError::EmptySegment))`.
  - `valid_layout_passes_validation` (line 37): valid → `validated(...).unwrap()` (and drop the now-redundant `.validate()` assertion, or keep it on the `validated` result — prefer asserting `validated(...).is_ok()`).
  - `empty_backing_segment_passes_validation` (line 49, zero-len `BackingAudio`): valid → `validated(...).unwrap()`.
  - `total_overflow_detected` (line 55, `u64::MAX + 1`): **intentionally invalid** → `new_unchecked(...)` then `assert_eq!(layout.validate(), Err(LayoutError::TotalOverflow))`.

  Example rewrite of the two invalid-layout tests:
  ```rust
  #[test]
  fn empty_single_segment_layout_fails_validation() {
      let layout = RegionLayout::new_unchecked(vec![Segment::Inline(vec![])]);
      assert_eq!(layout.validate(), Err(LayoutError::EmptySegment));
  }

  #[test]
  fn total_overflow_detected() {
      let layout = RegionLayout::new_unchecked(vec![
          Segment::BackingAudio { offset: 0, len: u64::MAX },
          Segment::BackingAudio { offset: 0, len: 1 },
      ]);
      assert_eq!(layout.validate(), Err(LayoutError::TotalOverflow));
  }
  ```

- [ ] **Step 5: Confirm no remaining external `RegionLayout::new` call.** Command:
  ```
  grep -rn "RegionLayout::new\b" --include=*.rs musefs-core musefs-format
  ```
  Expected: only `pub(crate) fn new` / `new_unchecked` definition lines in `layout.rs`, plus same-crate `#[cfg(test)]` callers (`reader.rs` unit tests at 496/769/820/852/1233, `facade.rs:1137`, `fuzz_check.rs`, `layout.rs` unit tests). Those same-crate callers — wait: `reader.rs`/`facade.rs` unit tests are in `musefs-core`, which is a **foreign crate** to `musefs-format`, so `pub(crate) new` is invisible to them. **Migrate them too**: they all build valid layouts → switch to `RegionLayout::validated(...).unwrap()` (the `fuzzing` feature is on in `musefs-core`'s test build, but `validated` is the right call for these valid layouts; reserve `new_unchecked` only for the two intentionally-invalid `layout.rs` tests). Re-run the grep until only the `layout.rs` definitions and same-crate `musefs-format` `#[cfg(test)]` callers remain.

  > Resolved ambiguity: the only `new_unchecked` consumers are the two intentionally-invalid tests in `musefs-format/tests/layout.rs`. Every other external call site builds a valid layout and migrates to `validated(...).unwrap()`. The `musefs-format` *same-crate* `#[cfg(test)]`/`fuzz_check` callers keep using `pub(crate) new` (they can see it); no churn there beyond the `BlobLen` literal changes from Task 2.

- [ ] **Step 6: Run green.** Commands:
  ```
  cargo test -p musefs-format -p musefs-core
  cargo clippy --all-targets -- -D warnings
  cargo fmt --all --check
  ```

- [ ] **Step 7: Commit.** Stage exact files:
  ```
  git add musefs-format/src/layout.rs musefs-core/src/reader.rs musefs-core/src/facade.rs \
    musefs-core/src/error.rs musefs-format/tests/layout.rs musefs-core/tests/read_at.rs
  git commit -F - <<'EOF'
  feat(format): hide RegionLayout::new; production uses validated() (#201)

  RegionLayout::new is now pub(crate); production obtains layouts only via
  validated(). The StructureOnly reader path and all external integration
  tests migrate to validated(); a fuzzing-gated public new_unchecked serves
  the two tests that build deliberately invalid layouts to exercise validate().

  Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>
  EOF
  ```

---

## Task 4: Defensive `validate()` at the reader cache boundary

Add a `layout.validate()?` in `HeaderCache::build` before the `ResolvedFile` is constructed and cached — belt-and-suspenders at the consuming boundary (the spec is explicit this call does not exist yet).

**Files:**
- `musefs-core/src/reader.rs` (after the `match self.mode { … }` block yields `(layout, total_len, mtime_secs_val)`, before constructing `ResolvedFile` at line 291)
- Test path (same-crate `#[cfg(test)]`): `musefs-core/src/reader.rs` tests mod

- [ ] **Step 1: Write a failing test that the cache boundary rejects an invalid layout.** The cleanest seam is a small helper that takes a `RegionLayout` and runs the defensive check the build path uses. Add to the `reader.rs` `#[cfg(test)] mod tests`:
  ```rust
  #[test]
  fn build_rejects_layout_failing_validation() {
      // A layout with an empty Inline segment fails validate(); the defensive
      // check at the cache boundary must surface it rather than cache it.
      let bad = RegionLayout::new(vec![Segment::Inline(vec![])]); // pub(crate), same crate? No.
      let err = bad.validate();
      assert!(err.is_err());
  }
  ```
  > NOTE: `RegionLayout::new` is `pub(crate)` in `musefs-format`, invisible here. Use the `fuzzing`-gated `new_unchecked` (on in `musefs-core`'s test build): `RegionLayout::new_unchecked(vec![Segment::Inline(vec![])])`. This test asserts the *property* the boundary relies on. The behavioral assertion — that `build` propagates the error — is best exercised through a synthetic path; since `build` is private and needs a `Db`/`Track`, assert instead that the defensive line exists by unit-testing it via a thin extracted check. Prefer extracting `fn defensive_validate(layout: &RegionLayout) -> Result<()>` and testing that directly (Step 3).

- [ ] **Step 2: Run and watch it fail.** Command:
  ```
  cargo test -p musefs-core --lib build_rejects_layout_failing_validation
  ```
  Expected: compile error (`new_unchecked`/helper not yet wired) or assertion to drive the implementation.

- [ ] **Step 3: Add the defensive check in `build`.** In `musefs-core/src/reader.rs`, immediately after the `match self.mode { … }` expression binds `(layout, total_len, mtime_secs_val)` (line 280) and before computing `cache_bytes` (line 282), insert:
  ```rust
  // Defensive belt-and-suspenders: production layouts are already built via
  // RegionLayout::validated, but re-validate at the cache boundary so a future
  // construction path that skips validation cannot poison the cache.
  layout.validate()?;
  ```
  This needs `LayoutError → CoreError` (the same `?` mapping added in Task 3 Step 2). If Task 3 added `CoreError::Layout(#[from] LayoutError)`, the `?` compiles directly.

  For the test seam, factor the predicate the test asserts on into the production code path itself rather than a parallel helper — the single `layout.validate()?` line is the production check; the unit test at Step 1 asserts the underlying `RegionLayout::validate()` behavior it relies on. Keep the test asserting `RegionLayout::new_unchecked(...).validate().is_err()` (a true regression guard that the boundary check has teeth).

- [ ] **Step 4: Run green.** Commands:
  ```
  cargo test -p musefs-core
  cargo clippy --all-targets -- -D warnings
  cargo fmt --all --check
  ```

- [ ] **Step 5: Commit.** Stage exact files:
  ```
  git add musefs-core/src/reader.rs
  git commit -F - <<'EOF'
  feat(core): defensively validate RegionLayout before caching (#201)

  build() now calls layout.validate()? before constructing/caching the
  ResolvedFile — cheap belt-and-suspenders at the consuming boundary so a
  future construction path that skips validation cannot poison the cache.

  Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>
  EOF
  ```

---

## Task 5: Full verification gate

Run the complete workspace gate plus the out-of-workspace fuzz build (Plan D changed format-layer `Segment` signatures, which the fuzz crate consumes).

**Files:** none (verification only)

- [ ] **Step 1: Full workspace test suite.**
  ```
  cargo test
  ```
  Expected: all crates green (FUSE e2e excluded by default).

- [ ] **Step 2: Lint with `--all-targets` (benches + tests/ compile here).**
  ```
  cargo clippy --all-targets -- -D warnings
  ```
  Expected: clean. This catches any hidden API consumer in `benches/` or crate `tests/` dirs that still constructs `Segment` with a raw length or reads `.segments` as a field.

- [ ] **Step 3: Format check.**
  ```
  cargo fmt --all --check
  ```
  Expected: no diff.

- [ ] **Step 4: Out-of-workspace fuzz build.**
  ```
  cargo +nightly fuzz build
  ```
  Expected: builds. The grep showed no `RegionLayout` usage in `fuzz/` today, but the fuzz targets consume `musefs-format` synthesis APIs that now thread `BlobLen` — if any target builds `Segment::ArtImage`/`BinaryTag`/`OggArtSlice` literals or `ArtInput`/`BinaryTagInput`, fix them with `BlobLen::new(...)` and rebuild. Resolve any breakage before declaring the gate green.

- [ ] **Step 5: Sanity-check the diff against the spec's Plan D bullets.** Confirm: (a) `new` is `pub(crate)` and no external `RegionLayout::new` remains (Task 3 grep); (b) `total_len`/`header_len` are stored fields and `segments` is private with no derived `Default` (Task 1); (c) `validate()` covers `BlobLen` non-zero metadata + backing-range overflow (Task 2); (d) `reader.rs build` calls `layout.validate()?` before caching (Task 4). Each maps to a landed task.

---

## Notes for the implementer

- **One breaking change per commit, never split from its fixups.** Tasks 1, 2, and 3 each flip a visibility/type/field that breaks many call sites at once; the pre-commit hook runs the full workspace test suite + clippy `-D warnings` + fmt + ruff and rejects any red commit. Each such task's edits (type/visibility change AND every caller AND every test) must land together.
- **`Default` removal:** the struct no longer derives `Default` (a defaulted empty-segments layout would cache `total_len = 0` honestly, but the spec mandates removing/hand-implementing it so a zero-value layout can't slip past `validated`). Grep `RegionLayout::default()` / `..Default::default()` for `RegionLayout` before Task 1's commit — the grep in investigation found none, but re-verify.
- **`mutants` feature interaction (Plan C follow-through):** if any DTO embeds a `BlobLen` and derives `Default` under `feature = "mutants"`, that is Plan C's concern; `Segment`/`RegionLayout` are not `mutants`-defaulted DTOs, so Plan D adds no `mutants`-gated `Default`.
- **`LayoutError → CoreError`:** verify the mapping once in Task 3 Step 2 and reuse it in Task 4; both the `StructureOnly` `validated()?` and the defensive `validate()?` depend on it.
