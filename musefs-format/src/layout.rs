/// Validation errors discovered in a layout at synthesis time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LayoutError {
    /// A segment reported zero length.
    EmptySegment,
    /// Total length overflowed u64.
    TotalOverflow,
}

/// One contiguous run of bytes in a synthesized virtual file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Segment {
    /// Generated framing/text bytes, fully materialized.
    Inline(Vec<u8>),
    /// Image bytes the caller splices in from its art store; only the length is known here.
    ArtImage { art_id: i64, len: u64 },
    /// A run of the original backing file's audio frames.
    BackingAudio { offset: u64, len: u64 },
    /// A run of original audio pages served with each page's sequence number
    /// shifted by `seq_delta` and its CRC recomputed. The byte length is unchanged
    /// (renumbering patches in place), so `len` equals the backing audio length.
    OggAudio {
        offset: u64,
        len: u64,
        seq_delta: i64,
    },
    /// A run of an embedded picture's serialized bytes, served lazily from the art
    /// store (never stored in the layout). When `base64`, the run is `len` chars of
    /// `base64(image)` starting at output offset `offset`; otherwise it is `len`
    /// raw image bytes starting at raw offset `offset`. `art_total` is the raw image
    /// byte length (needed to clip the final base64 group).
    OggArtSlice {
        art_id: i64,
        offset: u64,
        len: u64,
        base64: bool,
        art_total: u64,
    },
    /// An opaque binary tag payload (e.g. an ID3 `PRIV` frame body or a FLAC
    /// `APPLICATION` block body) streamed from the DB at read time; only the
    /// length is known here. `payload_id` is the caller's `tags` rowid handle.
    BinaryTag { payload_id: i64, len: u64 },
}

impl Segment {
    pub fn len(&self) -> u64 {
        match self {
            Segment::Inline(b) => b.len() as u64,
            Segment::ArtImage { len, .. }
            | Segment::BackingAudio { len, .. }
            | Segment::OggAudio { len, .. }
            | Segment::OggArtSlice { len, .. }
            | Segment::BinaryTag { len, .. } => *len,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// An ordered description of a synthesized virtual file: the metadata region
/// (inline framing + art images) followed by the backing audio.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RegionLayout {
    pub segments: Vec<Segment>,
}

impl RegionLayout {
    pub fn new(segments: Vec<Segment>) -> RegionLayout {
        RegionLayout { segments }
    }

    pub fn validated(segments: Vec<Segment>) -> Result<RegionLayout, LayoutError> {
        let layout = RegionLayout::new(segments);
        layout.validate()?;
        Ok(layout)
    }

    /// The ordered segments composing the synthesized virtual file.
    pub fn segments(&self) -> &[Segment] {
        &self.segments
    }

    /// True if any segment streams an opaque binary tag payload from the DB. Used by
    /// the reader to decide whether a read needs the transactional content_version
    /// guard (plain Inline/BackingAudio layouts don't).
    pub fn has_binary_tag(&self) -> bool {
        self.segments
            .iter()
            .any(|s| matches!(s, Segment::BinaryTag { .. }))
    }

    /// Total size of the synthesized virtual file in bytes.
    pub fn total_len(&self) -> u64 {
        self.segments.iter().map(Segment::len).sum()
    }

    /// Size of the synthesized metadata region preceding the backing audio.
    pub fn header_len(&self) -> u64 {
        self.segments
            .iter()
            .filter(|s| !matches!(s, Segment::BackingAudio { .. } | Segment::OggAudio { .. }))
            .map(Segment::len)
            .sum()
    }

    /// Validate basic producer invariants. Returns `Ok(())` if the layout is
    /// structurally sound (no empty metadata segments, lengths don't overflow).
    /// Zero-length backing audio is valid for formats that can represent an
    /// empty media payload.
    pub fn validate(&self) -> Result<(), LayoutError> {
        let mut total: u64 = 0;
        for seg in &self.segments {
            let len = seg.len();
            if len == 0 && !matches!(seg, Segment::BackingAudio { .. } | Segment::OggAudio { .. }) {
                return Err(LayoutError::EmptySegment);
            }
            total = total.checked_add(len).ok_or(LayoutError::TotalOverflow)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn binary_tag_segment_len_and_validate() {
        let seg = Segment::BinaryTag {
            payload_id: 5,
            len: 12,
        };
        assert_eq!(seg.len(), 12);
        // Non-empty binary tag passes validation.
        RegionLayout::validated(vec![seg, Segment::BackingAudio { offset: 0, len: 1 }]).unwrap();
        // Empty binary tag is rejected (EmptySegment), like empty art.
        let err = RegionLayout::validated(vec![Segment::BinaryTag {
            payload_id: 5,
            len: 0,
        }]);
        assert!(matches!(err, Err(LayoutError::EmptySegment)));
    }
}
