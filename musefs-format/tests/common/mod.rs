#![allow(dead_code)]

use std::collections::HashMap;

use musefs_format::{RegionLayout, Segment};

/// Build a FLAC metadata block (4-byte header + body) independently of production code.
pub fn flac_block(block_type: u8, body: &[u8], is_last: bool) -> Vec<u8> {
    let mut out = Vec::new();
    let first = (if is_last { 0x80 } else { 0 }) | (block_type & 0x7F);
    out.push(first);
    let len = body.len();
    out.push(((len >> 16) & 0xFF) as u8);
    out.push(((len >> 8) & 0xFF) as u8);
    out.push((len & 0xFF) as u8);
    out.extend_from_slice(body);
    out
}

/// A structurally valid STREAMINFO body: 44100 Hz, 2 channels, 16-bit, unknown frame/sample counts.
pub fn streaminfo_body() -> Vec<u8> {
    let mut b = vec![
        0x10, 0x00, // min block size = 4096
        0x10, 0x00, // max block size = 4096
        0x00, 0x00, 0x00, // min frame size = 0 (unknown)
        0x00, 0x00, 0x00, // max frame size = 0 (unknown)
        0x0A, 0xC4, 0x42, 0xF0, // sample_rate=44100, channels=2, bps=16, top of total samples
        0x00, 0x00, 0x00, 0x00, // remaining total-samples bits = 0
    ];
    b.extend_from_slice(&[0u8; 16]); // MD5 signature = 0
    b
}

/// Minimal VORBIS_COMMENT body with the given already-formatted `KEY=value` comments.
pub fn vorbis_comment_body(vendor: &str, comments: &[&str]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&(vendor.len() as u32).to_le_bytes());
    out.extend_from_slice(vendor.as_bytes());
    out.extend_from_slice(&(comments.len() as u32).to_le_bytes());
    for c in comments {
        out.extend_from_slice(&(c.len() as u32).to_le_bytes());
        out.extend_from_slice(c.as_bytes());
    }
    out
}

/// Assemble a full FLAC byte stream: marker + blocks (last-flag auto-set on the final block) + audio.
pub fn make_flac(blocks: &[(u8, Vec<u8>)], audio: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(b"fLaC");
    for (i, (bt, body)) in blocks.iter().enumerate() {
        let is_last = i == blocks.len() - 1;
        out.extend_from_slice(&flac_block(*bt, body, is_last));
    }
    out.extend_from_slice(audio);
    out
}

/// Resolve a RegionLayout into concrete bytes, given the original backing bytes and an
/// art-id -> image-bytes map. Independent of production assembly; used to verify splicing.
pub fn resolve_layout(
    layout: &RegionLayout,
    backing: &[u8],
    art: &HashMap<i64, Vec<u8>>,
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
        }
    }
    out
}
