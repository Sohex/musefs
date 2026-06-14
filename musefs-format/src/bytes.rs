//! Bounds-checked fixed-width integer readers shared across the format parsers.
//! Each reads a big- or little-endian integer at `pos`, returning
//! [`FormatError::Malformed`] if the field runs past the end of `data`.

use crate::error::{FormatError, Result};

pub(crate) fn read_u32_be(data: &[u8], pos: usize) -> Result<u32> {
    let s = data.get(pos..pos + 4).ok_or(FormatError::Malformed)?;
    Ok(u32::from_be_bytes(s.try_into().unwrap()))
}

pub(crate) fn read_u64_be(data: &[u8], pos: usize) -> Result<u64> {
    let s = data.get(pos..pos + 8).ok_or(FormatError::Malformed)?;
    Ok(u64::from_be_bytes(s.try_into().unwrap()))
}

pub(crate) fn read_u32_le(data: &[u8], pos: usize) -> Result<u32> {
    let s = data.get(pos..pos + 4).ok_or(FormatError::Malformed)?;
    Ok(u32::from_le_bytes(s.try_into().unwrap()))
}
