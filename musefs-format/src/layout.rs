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
    /// An Ogg art slice's offset + length overflowed u64, or its base64 output
    /// length (`b64_len(art_total)`) overflowed u64.
    #[error("ogg art slice range (offset + length, or base64 output length) overflowed u64")]
    OggArtSliceRangeOverflow,
    /// An Ogg art slice names an output window past the end of its source art.
    #[error("ogg art slice output window exceeds the source art length")]
    OggArtSliceOutOfBounds,
}

/// One contiguous run of bytes in a synthesized virtual file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Segment {
    /// Generated framing/text bytes, fully materialized.
    Inline(Vec<u8>),
    /// Image bytes the caller splices in from its art store; only the length is known here.
    ArtImage { art_id: i64, len: BlobLen },
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
        len: BlobLen,
        base64: bool,
        art_total: u64,
    },
    /// An opaque binary tag payload (e.g. an ID3 `PRIV` frame body or a FLAC
    /// `APPLICATION` block body) streamed from the DB at read time; only the
    /// length is known here. `payload_id` is the caller's `tags` rowid handle.
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
        let total_len = segments
            .iter()
            .map(Segment::len)
            .fold(0u64, u64::saturating_add);
        let header_len = segments
            .iter()
            .filter(|s| !matches!(s, Segment::BackingAudio { .. } | Segment::OggAudio { .. }))
            .map(Segment::len)
            .fold(0u64, u64::saturating_add);
        RegionLayout {
            segments,
            total_len,
            header_len,
        }
    }

    // Unvalidated construction is crate-internal: only same-crate tests build
    // layouts this way; production code reaches a layout solely via `validated`.
    #[allow(dead_code)] // used only in #[cfg(test)] / #[cfg(feature = "fuzzing")] paths
    pub(crate) fn new(segments: Vec<Segment>) -> RegionLayout {
        RegionLayout::from_segments(segments)
    }

    pub fn validated(segments: Vec<Segment>) -> Result<RegionLayout, LayoutError> {
        let layout = RegionLayout::from_segments(segments);
        layout.validate()?;
        Ok(layout)
    }

    /// Build a layout **without** validation. Test-only escape hatch for
    /// integration tests that deliberately construct invalid layouts to exercise
    /// `validate()`. Gated behind the `fuzzing` feature so production code (which
    /// has only `validated`) cannot reach it.
    #[cfg(feature = "fuzzing")]
    pub fn new_unchecked(segments: Vec<Segment>) -> RegionLayout {
        RegionLayout::from_segments(segments)
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
            if let Segment::BackingAudio { offset, len } | Segment::OggAudio { offset, len, .. } =
                seg
            {
                offset
                    .checked_add(*len)
                    .ok_or(LayoutError::BackingRangeOverflow)?;
            }
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
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_rejects_raw_ogg_art_slice_past_source() {
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
            len: BlobLen::new(1).unwrap(),
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

    #[test]
    fn binary_tag_segment_len_and_validate() {
        let seg = Segment::BinaryTag {
            payload_id: 5,
            len: BlobLen::new(12).unwrap(),
        };
        assert_eq!(seg.len(), 12);
        // Non-empty binary tag passes validation.
        RegionLayout::validated(vec![seg, Segment::BackingAudio { offset: 0, len: 1 }]).unwrap();
        // Zero-length binary tag cannot be constructed (BlobLen rejects 0).
        assert!(BlobLen::new(0).is_none());
    }

    #[test]
    fn has_binary_tag_detects_binary_segment() {
        let with = RegionLayout::new(vec![
            Segment::BinaryTag {
                payload_id: 1,
                len: BlobLen::new(3).unwrap(),
            },
            Segment::BackingAudio { offset: 0, len: 8 },
        ]);
        assert!(
            with.has_binary_tag(),
            "layout with a BinaryTag must report true"
        );

        let without = RegionLayout::new(vec![
            Segment::Inline(vec![1, 2, 3]),
            Segment::BackingAudio { offset: 0, len: 8 },
        ]);
        assert!(
            !without.has_binary_tag(),
            "layout with no BinaryTag must report false"
        );
    }
}
