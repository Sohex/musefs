use arbitrary::{Arbitrary, Unstructured};
use musefs_format::{ArtInput, BlobLen, BinaryTagInput, PictureType, TagInput};

/// Cap fuzz input size to avoid pathological slow paths / false-positive
/// timeouts on chunk/frame parsers (64-128 KiB is ample for full coverage).
pub const MAX_INPUT: usize = 128 * 1024;

/// Build a small vec of TagInputs from fuzzer entropy.
pub fn arb_tags(u: &mut Unstructured) -> arbitrary::Result<Vec<TagInput>> {
    let n = u.int_in_range(0..=8u8)?;
    let mut out = Vec::new();
    for _ in 0..n {
        let key = String::arbitrary(u)?;
        let value = String::arbitrary(u)?;
        out.push(TagInput::new(&key, &value));
    }
    Ok(out)
}

/// Build a small vec of ArtInputs (data_len bounded so synthesis stays cheap).
pub fn arb_arts(u: &mut Unstructured) -> arbitrary::Result<Vec<ArtInput>> {
    let n = u.int_in_range(0..=2u8)?;
    let mut out = Vec::new();
    for i in 0..n {
        out.push(ArtInput {
            art_id: i as i64,
            mime: "image/png".to_string(),
            description: String::arbitrary(u)?,
            picture_type: PictureType::new(u.int_in_range(0..=20u32)?)
                .expect("0..=20 is valid"),
            width: u.int_in_range(0..=4096u32)?,
            height: u.int_in_range(0..=4096u32)?,
            data_len: BlobLen::new(u.int_in_range(1..=8192u64)?)
                .expect("1..=8192 is non-zero"),
        });
    }
    Ok(out)
}

/// Build a small vec of BinaryTagInputs (synthetic handles + bounded lengths; the
/// synthesis path never reads payload bytes, only `len` for box sizing).
pub fn arb_binary_tags(u: &mut Unstructured) -> arbitrary::Result<Vec<BinaryTagInput>> {
    let n = u.int_in_range(0..=4u8)?;
    let mut out = Vec::new();
    for i in 0..n {
        let name = String::arbitrary(u)?;
        out.push(BinaryTagInput {
            key: format!("----:com.apple.iTunes:{name}"),
            payload_id: i as i64 + 1,
            len: BlobLen::new(u.int_in_range(1..=4096u64)?).expect("1..=4096 is non-zero"),
        });
    }
    Ok(out)
}
