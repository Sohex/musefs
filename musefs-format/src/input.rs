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
