//! Checked aggregate size arithmetic for synthesis builders. Aggregates over
//! attacker-controlled, DB-derived lengths must fail closed with
//! `FormatError::TooLarge` at the format arithmetic boundary, not wrap (release)
//! or panic (debug) and only fail later via `RegionLayout::validate`.

use crate::error::{FormatError, Result};

/// `a + b`, mapping `u64` overflow to `FormatError::TooLarge`.
#[allow(dead_code)]
pub(crate) fn checked_add(a: u64, b: u64) -> Result<u64> {
    a.checked_add(b).ok_or(FormatError::TooLarge)
}

/// Sum an iterator of `u64`, mapping any `u64` overflow to `FormatError::TooLarge`.
#[allow(dead_code)]
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
