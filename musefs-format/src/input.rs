use std::num::NonZeroU64;

/// An ID3/FLAC picture type, validated to the `0..=20` range (the #199
/// `track_art` CHECK, mirrored Rust-side at the synthesis-input boundary).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PictureType(u8);

impl PictureType {
    /// The "Other" picture type (0); the clamp target for an out-of-range byte.
    pub const ZERO: PictureType = PictureType(0);

    pub fn new(v: u32) -> Option<PictureType> {
        let b = u8::try_from(v).ok()?;
        if b > 20 { None } else { Some(PictureType(b)) }
    }

    pub fn get(self) -> u32 {
        u32::from(self.0)
    }
}

/// A non-zero payload length for an art image or binary tag. The non-zero
/// invariant encodes the layout's `EmptySegment` rule at the type level:
/// a degenerate empty payload is dropped at the construction boundary, so a
/// metadata segment can never carry a zero length.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlobLen(NonZeroU64);

impl BlobLen {
    pub fn new(v: u64) -> Option<BlobLen> {
        NonZeroU64::new(v).map(BlobLen)
    }

    pub fn get(self) -> u64 {
        self.0.get()
    }
}

/// One Vorbis/ID3 tag value to synthesize. Multi-valued tags are passed as
/// multiple `TagInput`s in the desired order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TagInput {
    pub key: String,
    pub value: String,
}

impl TagInput {
    pub fn new(key: &str, value: &str) -> TagInput {
        TagInput {
            key: key.to_string(),
            value: value.to_string(),
        }
    }
}

/// A reference to one embedded picture to synthesize. The image bytes themselves
/// are NOT held here — only `data_len`, the exact byte length — because the caller
/// streams the image into the spliced region at read time. `art_id` is an opaque
/// handle the caller maps back to its blob store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtInput {
    pub art_id: i64,
    pub mime: String,
    pub description: String,
    pub picture_type: u32,
    pub width: u32,
    pub height: u32,
    pub data_len: u64,
}

/// An embedded picture extracted from a backing file at scan time (a FLAC PICTURE
/// block or an MP3 APIC frame), before it is content-addressed and stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmbeddedPicture {
    pub mime: String,
    pub picture_type: PictureType,
    pub description: String,
    pub width: u32,
    pub height: u32,
    pub data: Vec<u8>,
}

/// A reference to one opaque binary tag payload to synthesize. Like `ArtInput`,
/// the bytes are NOT held here — only `len` and `payload_id`, an opaque handle the
/// caller (musefs-core) maps to the `tags` rowid it streams from. `key` is the
/// format-private identifier the synthesis path decodes (ID3 frame id,
/// `APPLICATION`/`CUESHEET`, `----:<mean>:<name>`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BinaryTagInput {
    pub key: String,
    pub payload_id: i64,
    pub len: u64,
}

/// A binary tag frame extracted at scan time: the format-private identifier
/// (`key` — an ID3 4-char frame id, a FLAC `APPLICATION`/`CUESHEET`, or an MP4
/// `----:<mean>:<name>`) and the raw post-header body (`payload`). Unlike
/// `BinaryTagInput`, this owns the bytes (scan ingests them into the DB).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmbeddedBinaryTag {
    pub key: String,
    pub payload: Vec<u8>,
}

#[cfg(test)]
mod tests {
    use super::BinaryTagInput;

    #[test]
    fn binary_tag_input_constructs() {
        let b = BinaryTagInput {
            key: "PRIV".into(),
            payload_id: 7,
            len: 3,
        };
        assert_eq!(b.payload_id, 7);
        assert_eq!(b.len, 3);
    }

    #[test]
    fn embedded_binary_tag_constructs() {
        let e = super::EmbeddedBinaryTag {
            key: "PRIV".into(),
            payload: vec![1, 2, 3],
        };
        assert_eq!(e.key, "PRIV");
        assert_eq!(e.payload.len(), 3);
    }

    #[test]
    fn picture_type_accepts_full_range() {
        for v in 0..=20u32 {
            assert_eq!(super::PictureType::new(v).unwrap().get(), v);
        }
    }

    #[test]
    fn picture_type_rejects_out_of_range() {
        assert!(super::PictureType::new(21).is_none());
        assert!(super::PictureType::new(u32::MAX).is_none());
    }

    #[test]
    fn blob_len_rejects_zero() {
        assert!(super::BlobLen::new(0).is_none());
    }

    #[test]
    fn blob_len_round_trips_nonzero() {
        assert_eq!(super::BlobLen::new(1).unwrap().get(), 1);
        assert_eq!(super::BlobLen::new(u64::MAX).unwrap().get(), u64::MAX);
    }
}
