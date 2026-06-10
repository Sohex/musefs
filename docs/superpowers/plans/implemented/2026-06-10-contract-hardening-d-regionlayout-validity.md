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
| `musefs-format/src/flac.rs`, `mp3.rs`, `mp4.rs` | Synthesis builders construct `Segment::ArtImage`/`BinaryTag` lengths from already-`BlobLen` `ArtInput`/`BinaryTagInput` fields — the edit is to **drop the `.get()`** at the segment-build sites (`flac.rs:297` `bt.len.get()`/`318` `art.data_len.get()`, `mp3.rs:357`/`370`, `mp4.rs:680`/`709`), passing the `BlobLen` through directly. `wav.rs` builds **no** such literal in production (delegates to `mp3::build_id3v2_segments`) — its only affected line is the `#[cfg(test)]` accessor sweep at `wav.rs:622`. |
| `musefs-format/src/ogg/mod.rs` | **No production `Segment::ArtImage`/`BinaryTag` literal** (ogg art becomes `OggArtSlice` built in `page.rs`) — the only changes are `#[cfg(test)]` reader-helper reads of the `OggArtSlice` `len` field at `mod.rs:931`/`968` (`*len` → `len.get()`). |
| `musefs-format/src/ogg/page.rs` | **(Reviewer-added — was missing.)** The real `Segment::OggArtSlice` is built in `emit_segments` at `page.rs:401` (`len: (oe - os) as u64`, a per-page art **slice width**, not a whole-payload length). Retype to `BlobLen::new((oe - os) as u64).expect("ogg art slice span is non-empty")`. Same-crate `#[cfg(test)]` consumers read the field: `page.rs:563` (`flatten`, `*offset + *len`), `page.rs:637` (`Some(*len)`) → `len.get()`; and the mutation-anchor test at `page.rs:642-646` (`matches!(… len: 0, …)`) cannot compile under `BlobLen` — see Task 2 Step 6 for its disposition and the `emit_segments` mutant it guards. |
| `musefs-format/src/fuzz_check.rs` | **No change for `Segment`** — verified it builds none of the three metadata variants (grep: zero `ArtImage`/`BinaryTag`/`OggArtSlice` literals). It does carry `.cargo/mutants.toml` anchors (`fuzz_check.rs` entries at toml lines 187/198/337), so if a stray edit shifts its lines the pre-commit **mutant-anchor drift guard** rejects the commit — do not touch this file. |
| `musefs-format/tests/layout.rs` | External integration tests: valid layouts → `validated`; invalid layouts (empty/overflow) → `new_unchecked` (gated by `fuzzing` dev-dep). |
| `musefs-core/src/reader.rs` | `StructureOnly` raw `new` (`reader.rs:144`) → `validated(...).map_err(FormatError::InvalidLayout)?` (reuse the existing error route — see Task 3 Step 2); `.segments` field reads at `reader.rs:333`/`372` → `.segments()`; cached `total_len` at `reader.rs:290` already reads `layout.total_len()`; defensive `layout.validate()` before cache insert. The `#[cfg(test)]` `RegionLayout::new` callers (`reader.rs:509`, `782`, `833`, `865`, `1281`) are in `musefs-core`, foreign to `musefs-format`, so they break under `pub(crate) new` and migrate too — see Task 3 Step 5. |
| `musefs-core/src/facade.rs` (`:1155`), `musefs-core/src/ogg_index.rs` (`:302`), `musefs-core/tests/read_at.rs` (`:114`) (+ `reader.rs`/`proptest_read_fidelity.rs` test sites) | Field-access and `new` call-site sweep. |

---

## Task 1: Cache totals, privatize `segments`, drop `Default`, accessor sweep

Make `total_len`/`header_len` stored-at-construction values, make the `segments` field private (so a mutator can't desync the cache), hand-remove the derived `Default`, and migrate every direct `.segments` **field** read to the `.segments()` accessor — all in one green commit, because privatizing the field breaks foreign-crate reads simultaneously.

> **Sequencing correction (do this exactly).** This task keeps `RegionLayout::new` **`pub`** — it ONLY privatizes the `segments` *field*. Demoting `new` to `pub(crate)` is deferred to Task 3, where it lands together with the migration of every foreign-crate `new` caller. (If you demote `new` here, the foreign `RegionLayout::new` callers in `musefs-core` — `facade.rs:1155`, `reader.rs:509/782/833/865/1281`, `tests/read_at.rs:114` — break and Task 1's commit goes red, which the pre-commit hook rejects.) Privatizing the field is independent: a `pub` constructor does not expose a private field, so foreign `new(...)` calls keep compiling while foreign `.segments` field reads are swept to `.segments()` in Step 4.

**Files:**
- `musefs-format/src/layout.rs` (struct `RegionLayout` lines 64–125; `new`/`validated`/`segments`/`total_len`/`header_len`/`validate`)
- `musefs-format/src/wav.rs` (line 622, `#[cfg(test)]`)
- `musefs-core/src/reader.rs` (`.segments` field reads at lines **333** and **372**; cached total already at **290** — verified against current `main`, the plan's old 320/359/277 were pre-Plan-C and are stale)
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

      // Stays `pub` in Task 1 (foreign `new` callers still need it); demoted to
      // `pub(crate)` in Task 3 together with their migration.
      pub fn new(segments: Vec<Segment>) -> RegionLayout {
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
  Leave `validate()` (lines 114–124) as-is for now; it still iterates `&self.segments` (same module, fine). `new` stays **`pub`** in this task (see the Sequencing correction above) — all `new` callers, same-crate and foreign, keep compiling; its demotion to `pub(crate)` and the `new_unchecked` escape hatch land in Task 3.

- [ ] **Step 3b: Fix the same-crate `validate()` caller of the overflow path.** `validate()` still recomputes `total` for the overflow check; that is correct and must stay (it is the *checked* construction path). No change needed beyond confirming it reads `&self.segments` (private but same-module — OK).

- [ ] **Step 4: Sweep foreign-crate `.segments` field reads to `.segments()`.** The field is now private outside `musefs-format`. Edit:
  - `musefs-core/src/reader.rs:333` `…layout\n.segments` → `…layout.segments()` (a multi-line `.segments` field read; verify the exact span by grepping `\.segments\b` — it is NOT 320)
  - `musefs-core/src/reader.rs:372` `for seg in &resolved.layout.segments` → `for seg in resolved.layout.segments()` (NOT 359)
  - `musefs-core/src/ogg_index.rs:302` `for seg in &layout.segments` → `for seg in layout.segments()` (this is a `#[cfg(test)]` mod in `musefs-core`, still foreign to `musefs-format`)
  - `musefs-format/src/wav.rs:622` `for s in &layout.segments` → `for s in layout.segments()` (same-crate `#[cfg(test)]`; the field is module-private, so the field read would still compile, but switch to the accessor for consistency)
  - External test crates touched in this commit: `musefs-format/tests/{synthesize_tags.rs, mp3_synthesize.rs, wav_synthesize.rs, synthesize_art.rs, common/mod.rs}` and `musefs-core/tests/{reader.rs, proptest_read_fidelity.rs}` each read `.segments` as a field. Convert every `layout.segments` / `resolved.layout.segments` field read to the `.segments()` accessor. Exact sites (from grep): `synthesize_tags.rs:100-102`, `mp3_synthesize.rs:11,78,135-137,223`, `wav_synthesize.rs:22,96,215,262`, `synthesize_art.rs:47,143,183`, `common/mod.rs:19`, `reader.rs:138,158`, `proptest_read_fidelity.rs:232,331,501,553,781`.

  > NOTE: `.segments().len()` and `.segments()[i]` index access work identically on the returned slice, so `layout.segments.len()` → `layout.segments().len()` and `layout.segments[0]` → `layout.segments()[0]`.

- [ ] **Step 5: Migrate the `total` source in `reader.rs:290` (no behavior change, compile check).** `let total = layout.total_len();` (line **290**, not the old 277) already reads the stored value — leave as-is. No edit needed; this step is a verification that the production caching path compiles against the now-private field.

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

- [ ] **Step 6: Retype every `ArtImage`/`BinaryTag`/`OggArtSlice` construction AND fix every pattern-match site.** Two distinct edit kinds — do not confuse them:

  **(a) Production synthesis — DROP the `.get()`, do NOT wrap in `BlobLen::new().expect()`.** Plan C already typed `ArtInput.data_len` and `BinaryTagInput.len` as `BlobLen`; the segment builders currently call `.get()` to down-convert to the old `u64` field. With the field now `BlobLen`, the conversion disappears — pass the `BlobLen` straight through by deleting `.get()`:
  - `musefs-format/src/flac.rs:297` `len: bt.len.get(),` → `len: bt.len,`
  - `musefs-format/src/flac.rs:318` `len: art.data_len.get(),` → `len: art.data_len,`
  - `musefs-format/src/mp3.rs:357` `len: bt.len.get(),` → `len: bt.len,`
  - `musefs-format/src/mp3.rs:370` `len: art.data_len.get(),` → `len: art.data_len,`
  - `musefs-format/src/mp4.rs:680` `len: bt.len.get(),` → `len: bt.len,`
  - `musefs-format/src/mp4.rs:709` `len: a.data_len.get(),` → `len: a.data_len,`
  (Re-grep `len: .*\.get()` in these files before editing in case Plan C's line numbers shift; the *pattern* is the anchor, not the line.)

  **(b) Production OggArtSlice — `ogg/page.rs:401`, the slice width.** This is the ONE production site where the length is a freshly computed `u64`, not a threaded `BlobLen`. In `emit_segments`:
  ```rust
  len: BlobLen::new((oe - os) as u64)
      .expect("ogg art slice span is non-empty"),
  ```
  The span is non-empty because `emit_segments` only emits an `OggArtSlice` for a real art run (`oe > os`). This `.expect()` is also load-bearing for the mutation gate — see the page.rs:646 note below.

  **(c) Pattern-match / struct-pattern sites that won't compile under `BlobLen`** (you cannot bind `len: 0` or `len: 50` as a pattern against a newtype). These are `-D warnings`/compile breaks the original plan never listed — convert each to bind `len, ..` and compare via `.get()` (or `s.len()`):
  - `musefs-format/src/mp4.rs:1163` `Some(Segment::ArtImage { len: 100, .. })` → bind `{ len, .. }` and assert `len.get() == 100`.
  - `musefs-format/src/mp4.rs:1410` `matches!(segs[1], Segment::ArtImage { art_id: 7, len: 50 })` → `matches!(segs[1], Segment::ArtImage { art_id: 7, .. })` plus a `len.get() == 50` check, or compare `segs[1].len() == 50`.
  - `musefs-format/src/mp4.rs:1434` `matches!(s, Segment::ArtImage { art_id: 9, len: 40 })` → same treatment.
  - `musefs-format/tests/wav_synthesize.rs:220` `Segment::ArtImage { art_id: 2, len: 64 }` (a `matches!`/pattern) → same treatment.
  - `musefs-format/src/ogg/page.rs:642-646` the mutation-anchor assertion `!matches!(s, Segment::OggArtSlice { len: 0, .. })` — **cannot compile** (`len: 0` against `BlobLen`) and is now **vacuous** (a zero-length `OggArtSlice` is unrepresentable). **Delete this assertion.** The mutant it guarded (`emit_segments` `<` → `<=`, per the comment) is still killed: that mutant forces a zero-width slice, which makes `BlobLen::new(0).expect(...)` at page.rs:401 **panic** inside this same test → mutant caught. The in-diff mutation gate re-tests `page.rs` (it is in the diff), so **verify the `emit_segments` mutant stays caught** after the change; if it survives, restore teeth with an explicit width assertion (e.g. assert each `OggArtSlice` slice width sums to `art_out.len()` — the `art_served` assertion just above already does this) rather than re-adding the impossible `len: 0` form.

  **(d) Same-crate `#[cfg(test)]` OggArtSlice field reads** (`*len` → `len.get()`): `musefs-format/src/ogg/page.rs:563` (`flatten`: `*offset + *len`), `page.rs:637` (`Some(*len)`), `musefs-format/src/ogg/mod.rs:931` and `:968` (the `materialize_header` / read helpers: `b64_window(*offset, *len, …)` and `usize_from(*len)`).

  **(e) `layout.rs` unit `tests` mod** — `binary_tag_segment_len_and_validate` builds `BinaryTag { len: 12 }` and `{ len: 0 }`. The `len: 0` case is now impossible to express — replace that sub-assertion with `assert!(BlobLen::new(0).is_none());` and keep the non-empty path with `len: BlobLen::new(12).unwrap()`.

  **(f) `fuzz_check.rs` — NO change.** Verified: it builds none of the three metadata variants. Do not touch it (its mutants.toml anchors are line-fragile).

  **(g) External tests**: `musefs-core/tests/read_at.rs:114` area (`ArtImage { len: art.len() as u64 }` → `len: BlobLen::new(art.len() as u64).unwrap()`); the `reader.rs`/`facade.rs` `#[cfg(test)]` literals (the `RegionLayout::new` callers migrated in Task 3 — `reader.rs:782` OggArtSlice `len: full_b64.len() as u64`, `:833` OggArtSlice, `:1281` BinaryTag) get `BlobLen::new(...).unwrap()`; and `proptest_read_fidelity.rs` / `synthesize_art.rs`. **Grep `ArtImage {`, `OggArtSlice {`, `BinaryTag {` across `musefs-format` and `musefs-core` and fix every construction AND every `matches!`/struct-pattern site** — the two are different edits (see (a) vs (c)).

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
    musefs-format/src/mp4.rs musefs-format/src/ogg/mod.rs musefs-format/src/ogg/page.rs \
    musefs-format/tests/layout.rs musefs-format/tests/wav_synthesize.rs \
    musefs-core/src/reader.rs musefs-core/tests/read_at.rs \
    <every other touched test file>
  # NOTE 1: musefs-core/src/reader.rs IS staged — its #[cfg(test)] mod builds
  # OggArtSlice/BinaryTag literals (:782/:833/:1281) whose `len:` now needs
  # BlobLen::new(...).unwrap(). These keep `RegionLayout::new` here (still pub
  # until Task 3); Task 3 later flips them to `validated`.
  # NOTE 2: musefs-core/src/facade.rs is NOT touched by this task — its only
  # Segment literal is BackingAudio (len stays raw u64). It changes in Task 3 only.
  # NOTE 3: musefs-format/src/wav.rs and src/fuzz_check.rs are NOT staged —
  # wav.rs has no production Segment literal (Task 1 handled its one test line) and
  # fuzz_check.rs builds none of the three variants (verified).
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

`new` is still `pub` (Task 1 deliberately did NOT demote it). **This task demotes `new` to `pub(crate)` AND migrates every foreign caller in the SAME commit** — that is the only way to keep the commit green (the moment `new` becomes `pub(crate)`, every `musefs-core` caller stops compiling). It adds the `fuzzing`-gated public `new_unchecked` escape hatch, migrates the **production** `StructureOnly` site, and migrates the **external (`musefs-core`) integration-test + `#[cfg(test)]`** call sites, deciding per site between `validated(...)` and `new_unchecked`.

**Files:**
- `musefs-format/src/layout.rs` (demote `fn new` to `pub(crate)`; add `new_unchecked`)
- `musefs-core/src/reader.rs:144` (production `StructureOnly`) **and** its `#[cfg(test)]` `new` callers at `:509/:782/:833/:865/:1281`
- `musefs-core/src/facade.rs:1155` (`#[cfg(test)]` `new` caller)
- `musefs-format/tests/layout.rs` (external; mixes valid + intentionally-invalid layouts)
- `musefs-core/tests/read_at.rs:114` (external; valid layout)
- Test path: `musefs-format/tests/layout.rs`

- [ ] **Step 0: Demote `new` to `pub(crate)`.** In `musefs-format/src/layout.rs`, change `pub fn new(segments: Vec<Segment>)` (left `pub` by Task 1) to `pub(crate) fn new(segments: Vec<Segment>)`. This is the breaking change; Steps 1–5 below are its in-commit fixups. After this edit the workspace will not build until every foreign `RegionLayout::new` caller (Step 5's list) is migrated — land them all in this one commit.

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

- [ ] **Step 2: Migrate the production `StructureOnly` site — reuse the existing `FormatError::InvalidLayout` route; do NOT add a new `CoreError` variant.** Edit `musefs-core/src/reader.rs:144`. The single `BackingAudio` layout is always valid, so use `validated` and surface the (practically-unreachable) error.

  > **Verified error routing (do not invent a second path).** `LayoutError` already converts into `FormatError` via `FormatError::InvalidLayout(#[from] crate::layout::LayoutError)` at `musefs-format/src/error.rs:18`, and `FormatError` already converts into `CoreError::Format`. That is the single, existing route every synthesis path uses. **However**, Rust's `?` does NOT chain two `From`s automatically, so a bare `RegionLayout::validated(…)?` (which yields `Result<_, LayoutError>`) will **not** compile in a `Result<_, CoreError>` function. Bridge the first hop explicitly with `.map_err(FormatError::InvalidLayout)` so the `?` then uses the existing `From<FormatError> for CoreError`:
  ```rust
  Mode::StructureOnly => {
      // Pure passthrough: the synthesized "file" is the backing file itself.
      // The stored audio bounds are irrelevant here — the whole file is served
      // verbatim — so they are not validated in this mode.
      let layout = RegionLayout::validated(vec![Segment::BackingAudio {
          offset: 0,
          len: meta.len(),
      }])
      .map_err(musefs_format::FormatError::InvalidLayout)?;
      (layout, meta.len(), track.backing_mtime)
  }
  ```
  Ensure `FormatError` is imported in `reader.rs` (or use the fully-qualified `musefs_format::FormatError::InvalidLayout` as above). **Do NOT add `CoreError::Layout(#[from] LayoutError)`** — it would create a second errno path for the same condition and an ambiguous `#[from]` (`LayoutError` would convert into both `FormatError` and `CoreError`). `musefs-core/src/error.rs` is therefore **unchanged** by this plan.

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
  Expected after migration: only the `pub(crate) fn new` / `new_unchecked` definition lines in `layout.rs`, plus any same-crate `musefs-format` `#[cfg(test)]` callers (which CAN see `pub(crate) new`). **Every `RegionLayout::new` call in `musefs-core` is in a foreign crate and breaks under `pub(crate)`** — verified current sites (NOT the plan's old 496/769/820/852/1233/1137): `musefs-core/src/facade.rs:1155`, and `musefs-core/src/reader.rs:509`, `:782`, `:833`, `:865`, `:1281` (all `#[cfg(test)]`), plus `musefs-core/tests/read_at.rs:114`. They all build **valid** layouts → switch each to `RegionLayout::validated(...).unwrap()` (reserve `new_unchecked` only for the two intentionally-invalid `layout.rs` tests). The OggArtSlice/BinaryTag literals among them (`reader.rs:782`/`:833`/`:1281`) also need their `len:` wrapped in `BlobLen::new(...).unwrap()` per Task 2 Step 6(g). Re-run the grep until only the `layout.rs` definitions and same-crate `musefs-format` `#[cfg(test)]` callers remain.

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
    musefs-format/tests/layout.rs musefs-core/tests/read_at.rs
  # NOTE: musefs-core/src/error.rs is NOT staged — the existing
  # FormatError::InvalidLayout -> CoreError::Format route is reused (Step 2),
  # so no new CoreError variant is added.
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
- `musefs-core/src/reader.rs` (after the `match self.mode { … }` block yields `(layout, total_len, mtime_secs_val)`, at the `let total = layout.total_len();` around **line 290**, before the `ResolvedFile` is constructed)
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

- [ ] **Step 3: Add the defensive check in `build`.** In `musefs-core/src/reader.rs`, immediately after the `match self.mode { … }` expression binds `(layout, total_len, mtime_secs_val)` and before computing `cache_bytes` (around the `let total = layout.total_len();` at **line 290** — re-confirm by grep, the old 280/282 are stale), insert:
  ```rust
  // Defensive belt-and-suspenders: production layouts are already built via
  // RegionLayout::validated, but re-validate at the cache boundary so a future
  // construction path that skips validation cannot poison the cache.
  layout
      .validate()
      .map_err(musefs_format::FormatError::InvalidLayout)?;
  ```
  This reuses the same `LayoutError → FormatError → CoreError::Format` bridge as Task 3 Step 2 (Rust's `?` will not chain the two `From`s, hence the explicit `.map_err`). No new `CoreError` variant.

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
- **`Default` removal is safe — verified.** Grep for `RegionLayout::default()` / `..Default::default()` on `RegionLayout` across `musefs-format` + `musefs-core` returns **no callers** (checked against current `main`), so dropping the derived `Default` breaks nothing. Re-verify before Task 1's commit, but expect zero hits.
- **`mutants` feature interaction (Plan C follow-through):** `Segment`/`RegionLayout` are not `mutants`-defaulted DTOs, so Plan D adds no `mutants`-gated `Default`.
- **`LayoutError → CoreError` routing — reuse the EXISTING path, add nothing.** `FormatError::InvalidLayout(#[from] LayoutError)` (`musefs-format/src/error.rs:18`) plus the existing `From<FormatError> for CoreError` is the single route. Because `?` will not chain two `From`s, the two new `musefs-core` call sites (Task 3 Step 2 `validated()`, Task 4 Step 3 `validate()`) must `.map_err(musefs_format::FormatError::InvalidLayout)?`. Do **not** add `CoreError::Layout` — `musefs-core/src/error.rs` is untouched by this plan.
- **Mutation gate — no `.cargo/mutants.toml` re-anchoring needed.** No `exclude_re` entry anchors a line in `layout.rs` or `ogg/page.rs`, and the production edits (dropping `.get()` at the synthesis sites) do not change line counts, so no existing anchor shifts. The pre-commit **mutant-anchor drift guard** validates all anchors on every commit, so any accidental drift is caught loudly — keep `fuzz_check.rs` untouched (it carries anchors and no `Segment` literals). Separately, the in-diff mutation gate WILL re-test `ogg/page.rs` (it is in the diff); confirm the `emit_segments` `<`→`<=` mutant stays caught after the `OggArtSlice` retype (Task 2 Step 6(c)).
- **Retype direction (the easy-to-get-wrong part):** in production synthesis the `BlobLen` already exists on `ArtInput`/`BinaryTagInput` — the edit is to **delete `.get()`**, never to wrap a `u64` in `BlobLen::new().expect()`. Only `ogg/page.rs:401` (a freshly computed slice width) constructs a `BlobLen` from a raw `u64`. Construction literals are one edit; `matches!`/struct **patterns** (`len: 50`) are a different edit — bind `len, ..` and compare `.get()`.
