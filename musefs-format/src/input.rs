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
    pub picture_type: u32,
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
}
