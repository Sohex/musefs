//! Sanctioned integer conversions, justified by the 64-bit-only guard below.
//! A deliberate sibling of `musefs_db::convert`: the format crate is pure byte
//! surgery and does not link the SQLite store for one helper.

// musefs supports 64-bit targets only; this is the compile-time declaration
// of that boundary. It makes u64 <-> usize conversions lossless by
// construction.
const _: () = assert!(
    std::mem::size_of::<usize>() == 8,
    "musefs supports 64-bit targets only"
);

/// This crate's only sanctioned `u64 -> usize` cast (see the guard above).
#[expect(
    clippy::cast_possible_truncation,
    reason = "u64 -> usize is lossless on 64-bit targets; guarded by the const assert above"
)]
#[inline]
#[must_use]
pub(crate) fn usize_from(v: u64) -> usize {
    v as usize
}
