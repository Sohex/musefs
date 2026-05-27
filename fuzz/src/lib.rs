use arbitrary::{Arbitrary, Unstructured};
use musefs_format::{ArtInput, TagInput};

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
            picture_type: u.int_in_range(0..=20u32)?,
            width: u.int_in_range(0..=4096u32)?,
            height: u.int_in_range(0..=4096u32)?,
            data_len: u.int_in_range(0..=8192u64)?,
        });
    }
    Ok(out)
}
