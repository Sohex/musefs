# Layout bounds hardening (#273 + #274)

## Summary

Two coordinated hardening changes in `musefs-format`, both failing closed at the
exact arithmetic/validation boundary instead of relying on a downstream check:

- **#273** — `RegionLayout::validate` does not bounds-check `Segment::OggArtSlice`
  against its source art. Add the missing invariants.
- **#274** — Several format synthesis builders use unchecked `+` / `+=` /
  `sum::<u64>()` over attacker-controlled, DB-derived lengths before the final
  format-size or `RegionLayout` validation boundary. Replace that aggregate math
  with checked helpers that return `FormatError::TooLarge`.

Both findings come from the same adversarial audit. In ordinary scanner-produced
data the values are small; under a hostile or externally written store
(`art.byte_len`, binary tag lengths, structural block bodies, text values) they
can be adversarial. `RegionLayout::validate` often catches total-length overflow
eventually, but that is too late and too indirect: debug builds can panic on
overflow instead of returning `TooLarge`, release builds may wrap intermediate
box/frame sizes and only fail later by accident, and future changes could weaken
the later validation and turn a wrapped size into emitted metadata.

## Part A — `OggArtSlice` bounds in `RegionLayout::validate` (#273)

### Current state

`RegionLayout::validate` (`musefs-format/src/layout.rs`) validates empty
non-backing segments, `BackingAudio`/`OggAudio` range overflow
(`offset + len`), and total synthesized length overflow. It does **not** verify
that an `OggArtSlice` names a valid output window for its source art.

`Segment::OggArtSlice { art_id, offset, len, base64, art_total }`:

- raw (`base64 == false`): the run is `len` raw image bytes starting at raw
  offset `offset`; the source is `art_total` bytes.
- base64 (`base64 == true`): the run is `len` chars of `base64(image)` starting
  at output offset `offset`; the full encoded length is `b64_len(art_total)`.

The reader path (`musefs-core/src/reader.rs`, and `musefs-format/src/ogg`)
assumes these relationships when it computes
`b64_window(*offset + within, n, *art_total)` or streams raw art at
`*offset + within`. An invalid layout can currently pass validation and reach
serving code, producing unexpected read errors, short reads, or panics
depending on which path observes the out-of-range slice.

### Change

Add a case for `Segment::OggArtSlice` to the `validate` loop. `len` is a
`BlobLen` (`NonZeroU64`), so the existing `EmptySegment` check already covers
zero length; the new logic only concerns bounds:

1. Compute the permitted output length:
   - raw: `art_total`
   - base64: `b64_len(art_total)`
2. `end = offset.checked_add(len.get())`; overflow → `OggArtSliceRangeOverflow`.
3. `end > permitted` → `OggArtSliceOutOfBounds`.

`b64_len` (`img_total.div_ceil(3) * 4`) can itself overflow `u64` for an
adversarial `art_total`. To keep validation total, introduce a checked variant
and have the existing function delegate to it:

```rust
// musefs-format/src/ogg/b64.rs
pub fn b64_len_checked(img_total: u64) -> Option<u64> {
    img_total.div_ceil(3).checked_mul(4)
}

pub fn b64_len(img_total: u64) -> u64 {
    b64_len_checked(img_total).expect("b64 output length fits u64")
}
```

For every real image (`img_total` from a scanned file) `b64_len` returns the
same value as today; only a deliberately hostile `art_total` near `u64::MAX`
can trip the overflow. Inside `validate`, base64 slices use `b64_len_checked`
and treat `None` as a bounds failure (fail closed): a `None` permitted length
maps to `OggArtSliceRangeOverflow`.

### Error variants

`LayoutError` gains two variants, mirroring the existing split between an
overflow error (`BackingRangeOverflow`) and a semantic violation
(`TotalOverflow`):

```rust
/// An Ogg art slice's offset + length overflowed u64, or its base64 output
/// length overflowed u64.
#[error("ogg art slice range offset + length overflowed u64")]
OggArtSliceRangeOverflow,
/// An Ogg art slice names an output window past the end of its source art.
#[error("ogg art slice output window exceeds the source art length")]
OggArtSliceOutOfBounds,
```

### Tests (`layout.rs` `tests`)

- raw slice with `offset + len > art_total` → `OggArtSliceOutOfBounds`
- base64 slice with `offset + len > b64_len(art_total)` → `OggArtSliceOutOfBounds`
- slice with `offset + len` overflowing `u64` → `OggArtSliceRangeOverflow`
- base64 slice with `art_total` whose `b64_len` overflows `u64` →
  `OggArtSliceRangeOverflow`
- passing boundary: raw `end == art_total` and base64 `end == b64_len(art_total)`
  both validate `Ok`

## Part B — checked aggregate length math in builders (#274)

### Helpers

New `pub(crate)` module `musefs-format/src/size.rs` (kept separate from
`convert.rs`, which is scoped to sanctioned casts):

```rust
//! Checked aggregate size arithmetic for synthesis builders. Aggregates over
//! attacker-controlled, DB-derived lengths must fail closed with
//! `FormatError::TooLarge` at the format arithmetic boundary, not wrap (release)
//! or panic (debug) and only fail later via `RegionLayout::validate`.

pub(crate) fn checked_add(a: u64, b: u64) -> Result<u64> {
    a.checked_add(b).ok_or(FormatError::TooLarge)
}

pub(crate) fn checked_sum(iter: impl IntoIterator<Item = u64>) -> Result<u64> {
    iter.into_iter().try_fold(0u64, checked_add)
}
```

`u32` box/frame-size conversions keep their existing
`u32::try_from(...).map_err(|_| FormatError::TooLarge)` form (and mp3's
`SYNCHSAFE_MAX` filter), but now take an **already checked** `u64`, so the `+`
inside the `try_from` argument can no longer wrap.

### Sites

- **`mp3.rs`** (`build_id3v2_segments`): every `frames_len += 10 + …` accumulation
  (the per-frame `10 + data.len()`, `10 + bt.len.get()`, `10 + data_len` cases)
  and the final returned `10 + frames_len`.
- **`mp4.rs`** (`build_udta` / `synthesize`): `streamed_total += …`;
  `covr_size = 8 + Σ(16 + data_len)`; `ilst_size` / `meta_size` / `udta_size`
  (`8 + inline_len + streamed_total`); `new_moov_size`
  (`8 + kept.len() + udta_total`); `new_mdat_payload_pos`
  (`ftyp.len() + new_moov_size + mdat_header.len()`).
- **`wav.rs`**: `body_len + 4` inside the `riff_size` `u32::try_from`.
- **`flac.rs`**: picture/body aggregate lengths
  (`framing.len() as u64 + art_len`, comment/body length sums).
- **`ogg/mod.rs`**: `b64_prefix_len + b64_len(...)`,
  `prefix.len() as u64 + b64_len(data_len)`, and Vorbis/MBP value-length sums.

Each `+` / `+=` / `sum::<u64>()` over a DB-derived length becomes a
`size::checked_add` / `size::checked_sum` returning `FormatError::TooLarge`.
Constant-only additions over already-bounded local buffer lengths
(`Vec::len()` of freshly built inline bytes) are left as-is where they cannot be
attacker-driven; the audit's named sites — all of which mix in a DB-derived
length — are the ones converted.

### Tests

Per affected builder, construct synthetic `ArtInput` / `BinaryTagInput` with
lengths near `u64::MAX` (or several `i64::MAX`-sized inputs that sum past
`u64::MAX`) and assert `Err(FormatError::TooLarge)` rather than a panic. These
sit alongside the existing per-format overflow tests
(`synthesize_rejects_riff_size_overflow`, the mp3 `frames_len` boundary tests,
the mp4 `new_moov_size` boundary tests).

## Non-goals

- `RegionLayout::validate` keeps its role as the final belt-and-suspenders check;
  it is not removed or weakened. Part B makes builders fail closed *earlier* so
  validation is no longer the first line that discovers format arithmetic
  overflow.
- No new public API beyond `b64_len_checked` and the two `LayoutError` variants.
  The `size.rs` helpers are `pub(crate)`.
- No changes to serving/read paths; the reader already assumes the invariants
  Part A now enforces.

## Sequencing

The pre-commit hook runs the full workspace test suite, so each commit must be
green:

1. `size.rs` helpers + their unit tests.
2. `b64_len_checked` delegation + Part A `validate` changes, the two
   `LayoutError` variants, and the layout bounds tests.
3. Per-format builder conversions to the helpers, each commit carrying its own
   `TooLarge` tests (mp3, mp4, wav, flac, ogg).

The `fuzz/` crate is outside the workspace and consumes format-layer APIs;
run `cargo +nightly fuzz build` after the `b64`/signature touchpoints to confirm
no silent breakage.
