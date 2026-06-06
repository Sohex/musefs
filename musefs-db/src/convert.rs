//! Sanctioned integer conversions, justified by the 64-bit-only guard below.

// musefs supports 64-bit targets only; this is the compile-time declaration
// of that boundary. It makes u64 <-> usize conversions lossless by
// construction everywhere in the workspace.
const _: () = assert!(
    std::mem::size_of::<usize>() == 8,
    "musefs supports 64-bit targets only"
);

/// The workspace's only sanctioned `u64 -> usize` cast (see the guard above).
#[expect(
    clippy::cast_possible_truncation,
    reason = "u64 -> usize is lossless on 64-bit targets; guarded by the const assert above"
)]
#[inline]
#[must_use]
pub fn usize_from(v: u64) -> usize {
    v as usize
}

#[cfg(test)]
mod tests {
    use super::usize_from;

    #[test]
    fn usize_from_is_lossless_across_the_range() {
        assert_eq!(usize_from(0), 0);
        assert_eq!(usize_from(u64::from(u32::MAX) + 1), 4_294_967_296);
        assert_eq!(usize_from(u64::MAX), usize::MAX);
    }
}
