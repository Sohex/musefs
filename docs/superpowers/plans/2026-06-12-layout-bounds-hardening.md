# Layout Bounds Hardening Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make `musefs-format` fail closed on adversarial DB-derived lengths at the exact arithmetic/validation boundary — bounds-check `Segment::OggArtSlice` in `RegionLayout::validate` (#273) and replace unchecked aggregate length math in the synthesis builders with checked helpers (#274).

**Architecture:** A new `pub(crate)` `size` module supplies `checked_add`/`checked_sum` returning `FormatError::TooLarge`; the builders (`mp3`/`mp4`/`wav`/`flac`/`ogg`) call them in place of `+`/`+=`/`sum::<u64>()` over attacker-controlled lengths. `RegionLayout::validate` gains an `OggArtSlice` case using a new checked `b64_len_checked`; `b64_len` delegates to it (no behaviour change for real images). `validate` stays the final belt-and-suspenders check.

**Tech Stack:** Rust (workspace crate `musefs-format`), `thiserror`, `cargo test`/`clippy`/`fmt`, `cargo +nightly fuzz build` for the out-of-workspace `fuzz/` crate.

**Spec:** `docs/superpowers/specs/2026-06-12-layout-bounds-hardening-design.md`

---

## File Structure

- **Create** `musefs-format/src/size.rs` — `pub(crate)` checked aggregate-size helpers (`checked_add`, `checked_sum`) returning `FormatError::TooLarge`. One responsibility: overflow-safe size arithmetic for builders.
- **Modify** `musefs-format/src/lib.rs` — register `mod size;` (between `pub mod probe;` at line 9 and `mod tagmap;` at line 10).
- **Modify** `musefs-format/src/ogg/b64.rs` — add `b64_len_checked`; make `b64_len` delegate; add a reader-safety boundary test.
- **Modify** `musefs-format/src/ogg/mod.rs` — re-export `b64_len_checked`; switch the two VorbisComment picture `value_len` computations to checked helpers; add an overflow test.
- **Modify** `musefs-format/src/layout.rs` — two new `LayoutError` variants; `OggArtSlice` case in `validate`; Part A tests.
- **Modify** `musefs-format/src/mp3.rs` — checked aggregates in `build_id3v2_segments`; overflow test.
- **Modify** `musefs-format/src/mp4.rs` — checked aggregates in `build_udta` and `synthesize`; overflow test.
- **Modify** `musefs-format/src/wav.rs` — checked aggregate for the RIFF size; note on testability.
- **Modify** `musefs-format/src/flac.rs` — checked aggregate for the picture block body; overflow test.

Commit order (each commit is green — the pre-commit hook runs the full workspace suite):
1. `size` helpers
2. `b64_len_checked` + delegation (fuzz build check after)
3. Part A `validate` + variants
4. mp3 · 5. mp4 · 6. wav · 7. flac · 8. ogg (fuzz build check after)

`b64_len_checked` (Task 2) must land before the ogg builder (Task 8) which depends on it.

---

## Task 1: `size` module — checked aggregate helpers

**Files:**
- Create: `musefs-format/src/size.rs`
- Modify: `musefs-format/src/lib.rs:5` (add `mod size;`)

- [ ] **Step 1: Create the module with helpers and failing tests**

Create `musefs-format/src/size.rs`:

```rust
//! Checked aggregate size arithmetic for synthesis builders. Aggregates over
//! attacker-controlled, DB-derived lengths must fail closed with
//! `FormatError::TooLarge` at the format arithmetic boundary, not wrap (release)
//! or panic (debug) and only fail later via `RegionLayout::validate`.

use crate::error::{FormatError, Result};

/// `a + b`, mapping `u64` overflow to `FormatError::TooLarge`.
pub(crate) fn checked_add(a: u64, b: u64) -> Result<u64> {
    a.checked_add(b).ok_or(FormatError::TooLarge)
}

/// Sum an iterator of `u64`, mapping any `u64` overflow to `FormatError::TooLarge`.
pub(crate) fn checked_sum(iter: impl IntoIterator<Item = u64>) -> Result<u64> {
    iter.into_iter().try_fold(0u64, checked_add)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checked_add_reports_overflow_as_too_large() {
        assert_eq!(checked_add(2, 3), Ok(5));
        assert_eq!(checked_add(u64::MAX, 1), Err(FormatError::TooLarge));
    }

    #[test]
    fn checked_sum_reports_overflow_as_too_large() {
        assert_eq!(checked_sum([1u64, 2, 3]), Ok(6));
        assert_eq!(checked_sum(std::iter::empty::<u64>()), Ok(0));
        assert_eq!(checked_sum([u64::MAX, 1]), Err(FormatError::TooLarge));
    }
}
```

- [ ] **Step 2: Register the module**

In `musefs-format/src/lib.rs`, add `mod size;` after the `pub mod probe;` line (line 9):

```rust
pub mod probe;
mod size;
mod tagmap;
```

- [ ] **Step 3: Run the tests to verify they pass**

Run: `cargo test -p musefs-format size::`
Expected: PASS (3 assertions across 2 tests).

- [ ] **Step 4: Lint**

Run: `cargo clippy -p musefs-format --all-targets`
Expected: no warnings.

- [ ] **Step 5: Commit**

```bash
git add musefs-format/src/size.rs musefs-format/src/lib.rs
git commit -m "feat(format): add checked aggregate-size helpers (#274)"
```

---

## Task 2: `b64_len_checked` + delegation

**Files:**
- Modify: `musefs-format/src/ogg/b64.rs:44-46` (the `b64_len` fn), test module at `:48`
- Modify: `musefs-format/src/ogg/mod.rs:8` (re-export)

- [ ] **Step 1: Write the failing reader-safety boundary test**

Add to the `#[cfg(test)] mod tests` block in `musefs-format/src/ogg/b64.rs`:

```rust
    #[test]
    fn b64_window_is_overflow_free_at_the_max_validated_boundary() {
        // For any layout that passes RegionLayout::validate, an OggArtSlice
        // satisfies offset + len <= b64_len(art_total) AND b64_len_checked(art_total)
        // is Some. Under those bounds b64_window's internal +/* cannot overflow.
        // Pin the worst case: the largest art_total whose b64_len still fits u64,
        // reading the final 4 output chars. In debug, any intermediate overflow
        // would panic here.
        let art_total = u64::MAX / 4 * 3; // b64_len_checked(art_total) is Some
        assert!(b64_len_checked(art_total).is_some());
        let total = b64_len(art_total);
        let w = b64_window(total - 4, 4, art_total);
        assert!(w.in_start <= art_total);
        assert!(w.in_len <= art_total);
    }
```

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo test -p musefs-format b64_window_is_overflow_free`
Expected: FAIL to compile — `b64_len_checked` does not exist yet.

- [ ] **Step 3: Add `b64_len_checked` and make `b64_len` delegate**

In `musefs-format/src/ogg/b64.rs`, replace the existing `b64_len`:

```rust
/// Total base64 output length for an image of `img_total` bytes.
pub fn b64_len(img_total: u64) -> u64 {
    img_total.div_ceil(3) * 4
}
```

with:

```rust
/// Total base64 output length for an image of `img_total` bytes, or `None` if it
/// overflows `u64`. Only an adversarial `img_total` can overflow; every real
/// image is far below this.
pub fn b64_len_checked(img_total: u64) -> Option<u64> {
    img_total.div_ceil(3).checked_mul(4)
}

/// Total base64 output length for an image of `img_total` bytes.
pub fn b64_len(img_total: u64) -> u64 {
    b64_len_checked(img_total).expect("base64 output length fits u64")
}
```

- [ ] **Step 4: Re-export `b64_len_checked`**

In `musefs-format/src/ogg/mod.rs`, change line 8 from:

```rust
pub use b64::{B64Window, b64_len, b64_window, encode_b64_slice};
```

to:

```rust
pub use b64::{B64Window, b64_len, b64_len_checked, b64_window, encode_b64_slice};
```

- [ ] **Step 5: Run the b64 tests**

Run: `cargo test -p musefs-format b64`
Expected: PASS — the new boundary test plus all pre-existing b64 tests (delegation returns identical values for real images).

- [ ] **Step 6: Confirm the out-of-workspace fuzz crate still builds**

Run: `cargo +nightly fuzz build` (from the repo root)
Expected: builds — adding a function and changing `b64_len`'s body is API-compatible.
(If the nightly toolchain or `cargo-fuzz` is unavailable, note it and skip; CI's smoke job covers it.)

- [ ] **Step 7: Lint and commit**

```bash
cargo clippy -p musefs-format --all-targets
git add musefs-format/src/ogg/b64.rs musefs-format/src/ogg/mod.rs
git commit -m "feat(format): add checked b64_len_checked; b64_len delegates (#273)"
```

---

## Task 3: Part A — `OggArtSlice` bounds in `RegionLayout::validate`

**Files:**
- Modify: `musefs-format/src/layout.rs:2-15` (`LayoutError`), `:142-160` (`validate`), `:163-203` (tests)

- [ ] **Step 1: Write the failing validation tests**

Add to `#[cfg(test)] mod tests` in `musefs-format/src/layout.rs`:

```rust
    #[test]
    fn validate_rejects_raw_ogg_art_slice_past_source() {
        // raw slice: offset + len must be <= art_total. end = 15 > art_total = 12.
        let seg = Segment::OggArtSlice {
            art_id: 1,
            offset: 5,
            len: BlobLen::new(10).unwrap(),
            base64: false,
            art_total: 12,
        };
        assert_eq!(
            RegionLayout::new(vec![seg, Segment::BackingAudio { offset: 0, len: 1 }]).validate(),
            Err(LayoutError::OggArtSliceOutOfBounds)
        );
    }

    #[test]
    fn validate_rejects_base64_ogg_art_slice_past_source() {
        // base64 slice: end must be <= b64_len(art_total). art_total=3 -> b64_len=4.
        // end = 6 > 4.
        let seg = Segment::OggArtSlice {
            art_id: 1,
            offset: 2,
            len: BlobLen::new(4).unwrap(),
            base64: true,
            art_total: 3,
        };
        assert_eq!(
            RegionLayout::new(vec![seg, Segment::BackingAudio { offset: 0, len: 1 }]).validate(),
            Err(LayoutError::OggArtSliceOutOfBounds)
        );
    }

    #[test]
    fn validate_rejects_ogg_art_slice_offset_len_overflow() {
        let seg = Segment::OggArtSlice {
            art_id: 1,
            offset: u64::MAX,
            len: BlobLen::new(1).unwrap(), // offset + len overflows u64
            base64: false,
            art_total: u64::MAX,
        };
        assert_eq!(
            RegionLayout::new(vec![seg, Segment::BackingAudio { offset: 0, len: 1 }]).validate(),
            Err(LayoutError::OggArtSliceRangeOverflow)
        );
    }

    #[test]
    fn validate_rejects_base64_ogg_art_slice_when_b64_len_overflows() {
        // art_total near u64::MAX makes b64_len(art_total) overflow u64.
        let seg = Segment::OggArtSlice {
            art_id: 1,
            offset: 0,
            len: BlobLen::new(1).unwrap(),
            base64: true,
            art_total: u64::MAX,
        };
        assert_eq!(
            RegionLayout::new(vec![seg, Segment::BackingAudio { offset: 0, len: 1 }]).validate(),
            Err(LayoutError::OggArtSliceRangeOverflow)
        );
    }

    #[test]
    fn validate_accepts_ogg_art_slice_at_source_boundary() {
        // raw: end == art_total exactly. base64: end == b64_len(art_total) exactly.
        let raw = Segment::OggArtSlice {
            art_id: 1,
            offset: 2,
            len: BlobLen::new(10).unwrap(),
            base64: false,
            art_total: 12,
        };
        RegionLayout::new(vec![raw, Segment::BackingAudio { offset: 0, len: 1 }])
            .validate()
            .unwrap();
        let b64 = Segment::OggArtSlice {
            art_id: 1,
            offset: 0,
            len: BlobLen::new(4).unwrap(),
            base64: true,
            art_total: 3,
        };
        RegionLayout::new(vec![b64, Segment::BackingAudio { offset: 0, len: 1 }])
            .validate()
            .unwrap();
    }
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p musefs-format validate_rejects_raw_ogg_art_slice_past_source validate_accepts_ogg_art_slice_at_source_boundary`
Expected: FAIL to compile — `LayoutError::OggArtSliceOutOfBounds` / `OggArtSliceRangeOverflow` do not exist yet.

- [ ] **Step 3: Add the two `LayoutError` variants**

In `musefs-format/src/layout.rs`, inside `enum LayoutError`, after the `BackingRangeOverflow` variant (line 14), add:

```rust
    /// An Ogg art slice's offset + length overflowed u64, or its base64 output
    /// length (`b64_len(art_total)`) overflowed u64.
    #[error("ogg art slice range (offset + length, or base64 output length) overflowed u64")]
    OggArtSliceRangeOverflow,
    /// An Ogg art slice names an output window past the end of its source art.
    #[error("ogg art slice output window exceeds the source art length")]
    OggArtSliceOutOfBounds,
```

- [ ] **Step 4: Add the `OggArtSlice` case to `validate`**

In `RegionLayout::validate`, the loop currently reads:

```rust
        for seg in &self.segments {
            let len = seg.len();
            if len == 0 && !matches!(seg, Segment::BackingAudio { .. } | Segment::OggAudio { .. }) {
                return Err(LayoutError::EmptySegment);
            }
            if let Segment::BackingAudio { offset, len } | Segment::OggAudio { offset, len, .. } =
                seg
            {
                offset
                    .checked_add(*len)
                    .ok_or(LayoutError::BackingRangeOverflow)?;
            }
            total = total.checked_add(len).ok_or(LayoutError::TotalOverflow)?;
        }
```

Insert the `OggArtSlice` check after the `BackingAudio`/`OggAudio` block and before the `total = ...` line:

```rust
            if let Segment::OggArtSlice {
                offset,
                len: slice_len,
                base64,
                art_total,
                ..
            } = seg
            {
                let permitted = if *base64 {
                    crate::ogg::b64_len_checked(*art_total)
                        .ok_or(LayoutError::OggArtSliceRangeOverflow)?
                } else {
                    *art_total
                };
                let end = offset
                    .checked_add(slice_len.get())
                    .ok_or(LayoutError::OggArtSliceRangeOverflow)?;
                if end > permitted {
                    return Err(LayoutError::OggArtSliceOutOfBounds);
                }
            }
            total = total.checked_add(len).ok_or(LayoutError::TotalOverflow)?;
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p musefs-format -- validate_rejects validate_accepts_ogg_art_slice`
Expected: PASS (all five new tests).

- [ ] **Step 6: Run the whole format suite (validate is on the synthesis path)**

Run: `cargo test -p musefs-format`
Expected: PASS — no existing layout/synthesis test regresses.

- [ ] **Step 7: Lint and commit**

```bash
cargo clippy -p musefs-format --all-targets
git add musefs-format/src/layout.rs
git commit -m "feat(format): validate OggArtSlice source bounds in RegionLayout (#273)"
```

---

## Task 4: mp3 — checked aggregates in `build_id3v2_segments`

**Files:**
- Modify: `musefs-format/src/mp3.rs` — imports, `build_id3v2_segments` (`:237-395`), tests

- [ ] **Step 1: Write the failing overflow test**

Add to `#[cfg(test)] mod tests` in `musefs-format/src/mp3.rs`:

```rust
    #[test]
    fn build_id3v2_segments_checked_art_len_rejects_overflow() {
        // A hostile art data_len near u64::MAX must fail closed with TooLarge at
        // the checked add, not panic (debug) / wrap (release).
        let mk = |data_len: u64| ArtInput {
            art_id: 1,
            mime: "image/png".to_string(),
            description: String::new(),
            picture_type: PictureType::new(3).unwrap(),
            width: 0,
            height: 0,
            data_len: BlobLen::new(data_len).unwrap(),
        };
        assert_eq!(
            build_id3v2_segments(&[], &[], &[mk(u64::MAX)]).err(),
            Some(FormatError::TooLarge)
        );
    }
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p musefs-format build_id3v2_segments_checked_art_len_rejects_overflow`
Expected: FAIL — panics with "attempt to add with overflow" at `framing.len() as u64 + art.data_len.get()` (debug), so the `assert_eq!` is never reached.

- [ ] **Step 3: Add the `size` import**

At the top of `musefs-format/src/mp3.rs`, add to the imports:

```rust
use crate::size;
```

- [ ] **Step 4: Convert the accumulation sites**

This accumulator line appears **8 times** at three different indentation levels (text/id3-text paths at 16 spaces; the TXXX/COMMENT/USLT/fallback-TXXX `for value in values` loops at 20 spaces; and the **POPM and UFID** accumulators at 8 spaces). Use a **`replace_all`** keyed on the substring **starting at `frames_len`** (no leading whitespace in the match), so each line's existing indentation is left untouched and all 8 sites — including POPM/UFID — are converted in one operation. Do not key on an indented copy (it would match only the sites at that one indent level), and do not edit site-by-site (you may miss POPM/UFID).

Replace this substring (exactly, no leading spaces):

```
frames_len += 10 + data.len() as u64;
```

with:

```
frames_len = size::checked_add(frames_len, 10 + data.len() as u64)?;
```

After the replace, confirm with `grep -c "frames_len += 10 + data.len() as u64;" musefs-format/src/mp3.rs` returning `0`. (`data` is a freshly built `Vec`, so `10 + data.len()` cannot itself overflow; only the running `frames_len` accumulation is the DB-derived aggregate.)

Replace the binary-tag site:

```rust
        frames_len += 10 + bt.len.get();
```

with (here `bt.len.get()` is a raw DB length, so the inner `10 + …` is checked too):

```rust
        frames_len = size::checked_add(frames_len, size::checked_add(10, bt.len.get())?)?;
```

Replace the art-loop body length:

```rust
        let data_len = framing.len() as u64 + art.data_len.get();
```

with:

```rust
        let data_len = size::checked_add(framing.len() as u64, art.data_len.get())?;
```

Replace the art-loop accumulation:

```rust
        frames_len += 10 + data_len;
```

with:

```rust
        frames_len = size::checked_add(frames_len, size::checked_add(10, data_len)?)?;
```

Replace the final return:

```rust
    Ok((segments, 10 + frames_len))
```

with:

```rust
    Ok((segments, size::checked_add(10, frames_len)?))
```

- [ ] **Step 5: Run the test to verify it passes**

Run: `cargo test -p musefs-format build_id3v2_segments_checked_art_len_rejects_overflow`
Expected: PASS.

- [ ] **Step 6: Run mp3 tests (including the existing total-tag boundary test)**

Run: `cargo test -p musefs-format mp3`
Expected: PASS — `build_id3v2_segments_rejects_oversized_total_tag` and all other mp3 tests still pass.

- [ ] **Step 7: Lint and commit**

```bash
cargo clippy -p musefs-format --all-targets
git add musefs-format/src/mp3.rs
git commit -m "fix(format): checked frame-length aggregates in mp3 synthesis (#274)"
```

---

## Task 5: mp4 — checked aggregates in `build_udta` and `synthesize`

**Files:**
- Modify: `musefs-format/src/mp4.rs` — imports, `build_udta` (`:625-777`), `synthesize` (`:838-843`), tests

- [ ] **Step 1: Write the failing overflow test**

Add to `#[cfg(test)] mod tests` in `musefs-format/src/mp4.rs`:

```rust
    #[test]
    fn build_udta_checked_art_len_rejects_overflow() {
        // A hostile art data_len near u64::MAX must fail closed with TooLarge at
        // the covr_size fold, not panic (debug) / wrap (release).
        let mk = |data_len: u64| crate::input::ArtInput {
            art_id: 1,
            mime: "image/png".to_string(),
            description: String::new(),
            picture_type: PictureType::new(3).unwrap(),
            width: 0,
            height: 0,
            data_len: BlobLen::new(data_len).unwrap(),
        };
        assert_eq!(
            build_udta(&[], &[], &[mk(u64::MAX)]).err(),
            Some(FormatError::TooLarge)
        );
    }
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p musefs-format build_udta_checked_art_len_rejects_overflow`
Expected: FAIL — panics with "attempt to add with overflow" inside the `16 + a.data_len.get()` sum (debug).

- [ ] **Step 3: Add the `size` import**

At the top of `musefs-format/src/mp4.rs`, add:

```rust
use crate::size;
```

- [ ] **Step 4: Convert the `build_udta` aggregates**

Replace the first streamed accumulation:

```rust
        streamed_total += bt.len.get();
```

with:

```rust
        streamed_total = size::checked_add(streamed_total, bt.len.get())?;
```

Replace the covr size:

```rust
        let covr_size: u64 = 8 + arts.iter().map(|a| 16 + a.data_len.get()).sum::<u64>();
```

with (each `16 + data_len` and the running sum are checked):

```rust
        let covr_size: u64 = arts.iter().try_fold(8u64, |acc, a| {
            size::checked_add(acc, size::checked_add(16, a.data_len.get())?)
        })?;
```

Replace the per-art data size:

```rust
            let data_size = 8 + 8 + a.data_len.get(); // data header + type + locale + image
```

with:

```rust
            let data_size = size::checked_add(16, a.data_len.get())?; // data header + type + locale + image
```

Replace the second streamed accumulation:

```rust
            streamed_total += a.data_len.get();
```

with:

```rust
            streamed_total = size::checked_add(streamed_total, a.data_len.get())?;
```

Replace the three enclosing box sizes:

```rust
    let ilst_size = 8 + ilst_inline_len + streamed_total;
    let meta_inline_len = 4 + hdlr.len() as u64 + 8 + ilst_inline_len; // [vf][hdlr][ilst hdr][ilst inline]
    let meta_size = 8 + meta_inline_len + streamed_total;
    let udta_inline_len = 8 + meta_inline_len; // [meta hdr][meta inline]
    let udta_size = 8 + udta_inline_len + streamed_total;
```

with (only the `*_size` values fold in `streamed_total`, the DB aggregate; `*_inline_len` sum only inline framing and stay as-is):

```rust
    let ilst_size = size::checked_sum([8, ilst_inline_len, streamed_total])?;
    let meta_inline_len = 4 + hdlr.len() as u64 + 8 + ilst_inline_len; // [vf][hdlr][ilst hdr][ilst inline]
    let meta_size = size::checked_sum([8, meta_inline_len, streamed_total])?;
    let udta_inline_len = 8 + meta_inline_len; // [meta hdr][meta inline]
    let udta_size = size::checked_sum([8, udta_inline_len, streamed_total])?;
```

- [ ] **Step 5: Convert the `synthesize` aggregates**

Replace:

```rust
    let new_moov_size = 8 + kept.len() as u64 + udta_total;
```

with:

```rust
    let new_moov_size = size::checked_sum([8, kept.len() as u64, udta_total])?;
```

Replace:

```rust
    let new_mdat_payload_pos =
        scan.ftyp.len() as u64 + new_moov_size + scan.mdat_header.len() as u64;
```

with:

```rust
    let new_mdat_payload_pos = size::checked_sum([
        scan.ftyp.len() as u64,
        new_moov_size,
        scan.mdat_header.len() as u64,
    ])?;
```

(`udta_total` — the `.sum()` over `udta_segments` lengths just above — is left unchecked: it is only reached once `build_udta` has already succeeded, which bounds every box size to `<= u32::MAX`, so the sum cannot overflow.)

- [ ] **Step 6: Run the test to verify it passes**

Run: `cargo test -p musefs-format build_udta_checked_art_len_rejects_overflow`
Expected: PASS.

- [ ] **Step 7: Run mp4 tests (including the `new_moov_size` / `udta_size` boundary tests)**

Run: `cargo test -p musefs-format mp4`
Expected: PASS — `synthesize_new_moov_size_exactly_u32_max_is_ok` and the other mp4 tests still pass.

- [ ] **Step 8: Lint and commit**

```bash
cargo clippy -p musefs-format --all-targets
git add musefs-format/src/mp4.rs
git commit -m "fix(format): checked box-size aggregates in mp4 synthesis (#274)"
```

---

## Task 6: wav — checked aggregate for the RIFF size

**Files:**
- Modify: `musefs-format/src/wav.rs` — imports, the `synthesize_layout` RIFF-size block (`:275-276`)

- [ ] **Step 1: Add the `size` import**

At the top of `musefs-format/src/wav.rs`, add:

```rust
use crate::size;
```

- [ ] **Step 2: Convert the body length and RIFF size**

Replace:

```rust
    let body_len: u64 = segments.iter().map(Segment::len).sum();
    let riff_size = u32::try_from(body_len + 4).map_err(|_| FormatError::TooLarge)?;
```

with:

```rust
    let body_len: u64 = size::checked_sum(segments.iter().map(Segment::len))?;
    let riff_size =
        u32::try_from(size::checked_add(body_len, 4)?).map_err(|_| FormatError::TooLarge)?;
```

- [ ] **Step 3: Run wav tests**

Run: `cargo test -p musefs-format wav`
Expected: PASS — `synthesize_rejects_riff_size_overflow` (the observable `riff_size > u32::MAX` boundary) and all other wav tests still pass.

> **No new u64-overflow unit test:** the wav body is the id3 tag (bounded to ≤ `0x0FFF_FFFF` by `build_id3v2_segments`) plus a `u32`-bounded `data` chunk, so `body_len` cannot approach `u64::MAX` through the public API — the conversion is defensive, and `synthesize_rejects_riff_size_overflow` already pins the reachable boundary. Do not invent an unreachable test.

- [ ] **Step 4: Lint and commit**

```bash
cargo clippy -p musefs-format --all-targets
git add musefs-format/src/wav.rs
git commit -m "fix(format): checked RIFF-size aggregate in wav synthesis (#274)"
```

---

## Task 7: flac — checked aggregate for the picture block body

**Files:**
- Modify: `musefs-format/src/flac.rs` — imports, the picture loop (`:304`), tests

- [ ] **Step 1: Write the failing overflow test**

Add to `#[cfg(test)] mod tests` in `musefs-format/src/flac.rs`:

```rust
    #[test]
    fn synthesize_layout_checked_picture_len_rejects_overflow() {
        // A hostile art data_len near u64::MAX must fail closed with TooLarge at
        // the checked add, not panic (debug) / wrap (release) past the
        // MAX_BLOCK_BODY guard.
        let mk = |data_len: u64| ArtInput {
            art_id: 1,
            mime: "image/png".to_string(),
            description: String::new(),
            picture_type: PictureType::new(3).unwrap(),
            width: 0,
            height: 0,
            data_len: BlobLen::new(data_len).unwrap(),
        };
        assert_eq!(
            synthesize_layout(&[], 0, 0, &[], &[], &[mk(u64::MAX)]),
            Err(FormatError::TooLarge)
        );
    }
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p musefs-format synthesize_layout_checked_picture_len_rejects_overflow`
Expected: FAIL — panics with "attempt to add with overflow" at `framing.len() as u64 + art.data_len.get()` (debug).

- [ ] **Step 3: Add the `size` import**

At the top of `musefs-format/src/flac.rs`, add:

```rust
use crate::size;
```

- [ ] **Step 4: Convert the picture body length**

Replace:

```rust
        let body_len = framing.len() as u64 + art.data_len.get();
```

with:

```rust
        let body_len = size::checked_add(framing.len() as u64, art.data_len.get())?;
```

- [ ] **Step 5: Run the test to verify it passes**

Run: `cargo test -p musefs-format synthesize_layout_checked_picture_len_rejects_overflow`
Expected: PASS.

- [ ] **Step 6: Run flac tests (including the picture-block boundary test)**

Run: `cargo test -p musefs-format flac`
Expected: PASS — `synthesize_layout_picture_block_size_boundary_is_inclusive` and all other flac tests still pass.

- [ ] **Step 7: Lint and commit**

```bash
cargo clippy -p musefs-format --all-targets
git add musefs-format/src/flac.rs
git commit -m "fix(format): checked picture-block aggregate in flac synthesis (#274)"
```

---

## Task 8: ogg — checked VorbisComment picture `value_len`

**Files:**
- Modify: `musefs-format/src/ogg/mod.rs` — imports, `build_packets_with_art` pre-flight (`:345-353`), `comment_packet_chunks` emit (`:396-405`), tests

- [ ] **Step 1: Write the failing overflow test**

Add to `#[cfg(test)] mod tests` in `musefs-format/src/ogg/mod.rs` (mirrors the existing `oversized_full_art_value_rejected_by_build_packets`):

```rust
    #[test]
    fn near_u64_max_art_value_rejected_by_build_packets() {
        // data_len near u64::MAX makes b64_len(data_len) overflow u64; the builder
        // must fail closed with TooLarge at the checked b64 length, not panic
        // (debug) inside the pre-flight value_len computation.
        let meta = crate::input::ArtInput {
            art_id: 0,
            mime: "image/jpeg".to_string(),
            description: String::new(),
            data_len: crate::input::BlobLen::new(u64::MAX).unwrap(),
            picture_type: crate::input::PictureType::new(3).unwrap(),
            width: 0,
            height: 0,
        };
        let art = OggArt { meta: &meta };
        let header = OggHeader {
            codec: Codec::Vorbis,
            serial: 0,
            packets: vec![vec![], vec![], vec![]],
            header_pages: 1,
            audio_offset: 0,
        };
        assert_eq!(
            build_packets_with_art(&header, &[], &[art]),
            Err(FormatError::TooLarge)
        );
    }
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p musefs-format near_u64_max_art_value_rejected_by_build_packets`
Expected: FAIL — panics with "attempt to multiply with overflow" inside `b64_len(a.meta.data_len.get())` (debug).

- [ ] **Step 3: Add the `size` import**

At the top of `musefs-format/src/ogg/mod.rs`, add (if not already present):

```rust
use crate::size;
```

- [ ] **Step 4: Convert the pre-flight value length**

In `build_packets_with_art`, replace:

```rust
            for a in arts {
                let prefix = picture_prefix(a.meta)?;
                let b64_prefix_len = b64_len(prefix.len() as u64);
                let value_len = METADATA_BLOCK_PICTURE_KEY.len() as u64
                    + b64_prefix_len
                    + b64_len(a.meta.data_len.get());
                if value_len > u64::from(u32::MAX) {
                    return Err(FormatError::TooLarge);
                }
            }
```

with:

```rust
            for a in arts {
                let prefix = picture_prefix(a.meta)?;
                let b64_prefix_len =
                    b64_len_checked(prefix.len() as u64).ok_or(FormatError::TooLarge)?;
                let b64_image_len =
                    b64_len_checked(a.meta.data_len.get()).ok_or(FormatError::TooLarge)?;
                let value_len = size::checked_sum([
                    METADATA_BLOCK_PICTURE_KEY.len() as u64,
                    b64_prefix_len,
                    b64_image_len,
                ])?;
                if value_len > u64::from(u32::MAX) {
                    return Err(FormatError::TooLarge);
                }
            }
```

(`b64_len_checked` is in scope via the re-export added in Task 2.)

- [ ] **Step 5: Convert the emitted value length**

In `comment_packet_chunks`, replace:

```rust
        let value_len = METADATA_BLOCK_PICTURE_KEY.len()
            + b64_prefix.len()
            + crate::convert::usize_from(b64_len(art.meta.data_len.get()));
        head.extend_from_slice(
            &u32::try_from(value_len)
                .map_err(|_| FormatError::TooLarge)?
                .to_le_bytes(),
        );
```

with (compute in `u64` with checked helpers, then narrow to `u32`):

```rust
        let b64_image_len =
            b64_len_checked(art.meta.data_len.get()).ok_or(FormatError::TooLarge)?;
        let value_len = size::checked_sum([
            METADATA_BLOCK_PICTURE_KEY.len() as u64,
            b64_prefix.len() as u64,
            b64_image_len,
        ])?;
        head.extend_from_slice(
            &u32::try_from(value_len)
                .map_err(|_| FormatError::TooLarge)?
                .to_le_bytes(),
        );
```

- [ ] **Step 6: Run the test to verify it passes**

Run: `cargo test -p musefs-format near_u64_max_art_value_rejected_by_build_packets`
Expected: PASS.

- [ ] **Step 7: Run ogg tests (including the existing u32 value-length boundary tests)**

Run: `cargo test -p musefs-format ogg`
Expected: PASS — `oversized_full_art_value_rejected_by_build_packets`, `sum_overflow_art_value_rejected_by_build_packets`, and the `value_len == u32::MAX` acceptance test still pass.

- [ ] **Step 8: Confirm the fuzz crate still builds**

Run: `cargo +nightly fuzz build`
Expected: builds — the ogg changes are internal (no public signature change).
(Skip with a note if the nightly toolchain / `cargo-fuzz` is unavailable.)

- [ ] **Step 9: Full workspace check, lint, commit**

```bash
cargo test -p musefs-format
cargo clippy -p musefs-format --all-targets
git add musefs-format/src/ogg/mod.rs
git commit -m "fix(format): checked VorbisComment picture value_len in ogg synthesis (#274)"
```

---

## Final verification

- [ ] **Run the full workspace suite and lint (mirrors the pre-commit gate)**

Run:
```bash
cargo fmt --all --check
cargo clippy --all-targets
cargo test
```
Expected: all green.

- [ ] **Confirm the fuzz crate builds one last time**

Run: `cargo +nightly fuzz build`
Expected: builds.
