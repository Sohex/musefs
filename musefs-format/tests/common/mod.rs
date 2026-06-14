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
    for seg in layout.segments() {
        match seg {
            Segment::Inline(b) => out.extend_from_slice(b),
            Segment::ArtImage { art_id, len } => {
                let img = art.get(art_id).expect("art bytes provided");
                assert_eq!(img.len() as u64, len.get(), "art length mismatch in layout");
                out.extend_from_slice(img);
            }
            Segment::BackingAudio { offset, len } => {
                let o = usize::try_from(*offset).unwrap();
                let l = usize::try_from(*len).unwrap();
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
                    len.get(),
                    "binary tag length mismatch in layout"
                );
                out.extend_from_slice(payload);
            }
        }
    }
    out
}

/// A minimal FLAC file (STREAMINFO + VORBIS_COMMENT) with `len` bytes of audio
/// filled with `fill`. Returns the file and the raw audio payload.
pub fn flac_fixture(fill: u8, len: usize) -> (Vec<u8>, Vec<u8>) {
    let si = streaminfo_body();
    let vc = vorbis_comment_body("oldvendor", &["TITLE=Old"]);
    let audio = vec![fill; len];
    let file = make_flac(&[(0, si), (4, vc)], &audio);
    (file, audio)
}

/// A 16-byte PCM `fmt ` payload: mono, 44.1 kHz, 16-bit.
pub fn fmt_pcm_16bit_mono() -> Vec<u8> {
    let mut f = Vec::new();
    f.extend_from_slice(&1u16.to_le_bytes()); // wFormatTag = PCM
    f.extend_from_slice(&1u16.to_le_bytes()); // channels
    f.extend_from_slice(&44_100u32.to_le_bytes()); // sample rate
    f.extend_from_slice(&88_200u32.to_le_bytes()); // byte rate
    f.extend_from_slice(&2u16.to_le_bytes()); // block align
    f.extend_from_slice(&16u16.to_le_bytes()); // bits per sample
    f
}

/// Build a minimal valid `RIFF/WAVE` file from a list of `(fourcc, payload)` chunks.
pub fn build_wav(chunks: &[(&[u8; 4], Vec<u8>)]) -> Vec<u8> {
    let mut body = Vec::new();
    for (id, payload) in chunks {
        body.extend_from_slice(*id);
        body.extend_from_slice(&u32::try_from(payload.len()).unwrap().to_le_bytes());
        body.extend_from_slice(payload);
        if payload.len() % 2 == 1 {
            body.push(0x00);
        }
    }
    let mut out = Vec::new();
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&u32::try_from(body.len() + 4).unwrap().to_le_bytes());
    out.extend_from_slice(b"WAVE");
    out.extend_from_slice(&body);
    out
}
