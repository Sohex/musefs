#![allow(dead_code)]

use std::collections::HashMap;

use musefs_format::{RegionLayout, Segment};

pub use musefs_format::fuzz_check::fixtures::{make_flac, streaminfo_body, vorbis_comment_body};

/// Resolve a RegionLayout into concrete bytes, given the original backing bytes, an
/// art-id -> image-bytes map, and a payload-id -> bytes map for binary tag segments.
/// Independent of production assembly; used to verify splicing.
pub fn resolve_layout(
    layout: &RegionLayout,
    backing: &[u8],
    art: &HashMap<i64, Vec<u8>>,
    binary_tags: &HashMap<i64, Vec<u8>>,
) -> Vec<u8> {
    let mut out = Vec::new();
    for seg in &layout.segments {
        match seg {
            Segment::Inline(b) => out.extend_from_slice(b),
            Segment::ArtImage { art_id, len } => {
                let img = art.get(art_id).expect("art bytes provided");
                assert_eq!(img.len() as u64, *len, "art length mismatch in layout");
                out.extend_from_slice(img);
            }
            Segment::BackingAudio { offset, len } => {
                let o = *offset as usize;
                let l = *len as usize;
                out.extend_from_slice(&backing[o..o + l]);
            }
            Segment::OggAudio { .. } => unreachable!("no Ogg audio in this fixture"),
            Segment::OggArtSlice { .. } => unreachable!("OggArtSlice only in ogg synthesis"),
            Segment::BinaryTag { payload_id, len } => {
                let payload = binary_tags
                    .get(payload_id)
                    .expect("binary tag bytes provided");
                assert_eq!(
                    payload.len() as u64,
                    *len,
                    "binary tag length mismatch in layout"
                );
                out.extend_from_slice(payload);
            }
        }
    }
    out
}
