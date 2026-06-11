use crate::error::{FormatError, Result};
use std::collections::HashMap;

/// A source of raw art bytes used during synthesis to compute page CRCs. The
/// production implementation (in `musefs-core`) streams from the SQLite blob
/// store; tests and fuzzing use [`MapArtSource`]. `offset` and `buf.len()` are in
/// raw-image coordinates; a read past the stored image is an error (mirrors the
/// short-read semantics of the DB blob path), surfaced as `FormatError::ArtRead`.
pub trait ArtSource {
    fn read_window(&self, art_id: i64, offset: u64, buf: &mut [u8]) -> Result<()>;
}

/// In-memory `ArtSource` over an `art_id -> image bytes` map. For tests/fuzz.
#[derive(Default)]
pub struct MapArtSource {
    images: HashMap<i64, Vec<u8>>,
}

impl MapArtSource {
    pub fn new(images: impl IntoIterator<Item = (i64, Vec<u8>)>) -> Self {
        Self {
            images: images.into_iter().collect(),
        }
    }
}

impl ArtSource for MapArtSource {
    fn read_window(&self, art_id: i64, offset: u64, buf: &mut [u8]) -> Result<()> {
        let img = self
            .images
            .get(&art_id)
            .ok_or(FormatError::ArtRead { art_id })?;
        let start = crate::convert::usize_from(offset);
        let end = start
            .checked_add(buf.len())
            .filter(|&e| e <= img.len())
            .ok_or(FormatError::ArtRead { art_id })?;
        buf.copy_from_slice(&img[start..end]);
        Ok(())
    }
}
