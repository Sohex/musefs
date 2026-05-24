#![allow(dead_code)]

use std::path::Path;

pub fn flac_block(block_type: u8, body: &[u8], is_last: bool) -> Vec<u8> {
    let mut out = Vec::new();
    out.push((if is_last { 0x80 } else { 0 }) | (block_type & 0x7F));
    let len = body.len();
    out.push(((len >> 16) & 0xFF) as u8);
    out.push(((len >> 8) & 0xFF) as u8);
    out.push((len & 0xFF) as u8);
    out.extend_from_slice(body);
    out
}

pub fn streaminfo_body() -> Vec<u8> {
    let mut b = vec![
        0x10, 0x00, 0x10, 0x00,
        0x00, 0x00, 0x00,
        0x00, 0x00, 0x00,
        0x0A, 0xC4, 0x42, 0xF0,
        0x00, 0x00, 0x00, 0x00,
    ];
    b.extend_from_slice(&[0u8; 16]);
    b
}

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

pub fn make_flac(blocks: &[(u8, Vec<u8>)], audio: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(b"fLaC");
    for (i, (bt, body)) in blocks.iter().enumerate() {
        out.extend_from_slice(&flac_block(*bt, body, i == blocks.len() - 1));
    }
    out.extend_from_slice(audio);
    out
}

/// Write a simple FLAC (STREAMINFO + comment + audio) to `path`,
/// returning (audio_offset, audio_length).
pub fn write_flac(path: &Path, comments: &[&str], audio: &[u8]) -> (i64, i64) {
    let si = streaminfo_body();
    let vc = vorbis_comment_body("orig", comments);
    let bytes = make_flac(&[(0, si), (4, vc)], audio);
    let audio_offset = (bytes.len() - audio.len()) as i64;
    std::fs::write(path, &bytes).unwrap();
    (audio_offset, audio.len() as i64)
}
