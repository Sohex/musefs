/// One contiguous run of bytes in a synthesized virtual file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Segment {
    /// Generated framing/text bytes, fully materialized.
    Inline(Vec<u8>),
    /// Image bytes the caller splices in from its art store; only the length is known here.
    ArtImage { art_id: i64, len: u64 },
    /// A run of the original backing file's audio frames.
    BackingAudio { offset: u64, len: u64 },
}

impl Segment {
    pub fn len(&self) -> u64 {
        match self {
            Segment::Inline(b) => b.len() as u64,
            Segment::ArtImage { len, .. } => *len,
            Segment::BackingAudio { len, .. } => *len,
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

    /// The ordered segments composing the synthesized virtual file.
    pub fn segments(&self) -> &[Segment] {
        &self.segments
    }

    /// Total size of the synthesized virtual file in bytes.
    pub fn total_len(&self) -> u64 {
        self.segments.iter().map(Segment::len).sum()
    }

    /// Size of the synthesized metadata region preceding the backing audio.
    pub fn header_len(&self) -> u64 {
        self.segments
            .iter()
            .filter(|s| !matches!(s, Segment::BackingAudio { .. }))
            .map(|s| s.len())
            .sum()
    }
}
