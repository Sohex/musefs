use std::io::Cursor;

use id3::TagLike;
use musefs_format::wav::{WavScan, synthesize_layout};
use musefs_format::{ArtInput, BlobLen, PictureType, RegionLayout, Segment, TagInput};

fn fmt_pcm_16bit_mono() -> Vec<u8> {
    let mut f = Vec::new();
    f.extend_from_slice(&1u16.to_le_bytes());
    f.extend_from_slice(&1u16.to_le_bytes());
    f.extend_from_slice(&44_100u32.to_le_bytes());
    f.extend_from_slice(&88_200u32.to_le_bytes());
    f.extend_from_slice(&2u16.to_le_bytes());
    f.extend_from_slice(&16u16.to_le_bytes());
    f
}

/// Flatten a layout, substituting `audio` for the backing-audio segment and the
/// matching bytes for each `ArtImage` segment.
fn assemble(layout: &RegionLayout, audio: &[u8], arts: &[(i64, &[u8])]) -> Vec<u8> {
    let mut out = Vec::new();
    for seg in layout.segments() {
        match seg {
            Segment::Inline(b) => out.extend_from_slice(b),
            Segment::BackingAudio { .. } => out.extend_from_slice(audio),
            Segment::ArtImage { art_id, .. } => {
                out.extend_from_slice(arts.iter().find(|(id, _)| id == art_id).unwrap().1);
            }
            other => unreachable!("unexpected segment in WAV layout: {other:?}"),
        }
    }
    out
}

#[test]
fn synthesizes_valid_riff_and_preserves_audio() {
    // 4 little-endian i16 PCM samples = 8 bytes of audio payload.
    let samples: Vec<i16> = vec![1000, -1000, 32000, -32000];
    let audio: Vec<u8> = samples.iter().flat_map(|s| s.to_le_bytes()).collect();
    let scan = WavScan {
        fmt: fmt_pcm_16bit_mono(),
        fact: None,
    };
    let tags = vec![
        TagInput::new("title", "Wave Song"),
        TagInput::new("artist", "Alice"),
    ];

    let layout = synthesize_layout(&scan, 0, audio.len() as u64, &tags, &[], &[]).unwrap();
    let bytes = assemble(&layout, &audio, &[]);

    // total_len equals the bytes actually produced (generate-and-measure).
    assert_eq!(bytes.len() as u64, layout.total_len());

    // RIFF header is well-formed and the size field == file_len - 8.
    assert_eq!(&bytes[0..4], b"RIFF");
    assert_eq!(&bytes[8..12], b"WAVE");
    let riff_size = u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]) as usize;
    assert_eq!(riff_size, bytes.len() - 8);

    // hound (an independent WAV reader) parses the container and recovers the
    // original PCM samples byte-for-byte.
    let mut reader = hound::WavReader::new(Cursor::new(&bytes)).expect("valid wav");
    assert_eq!(reader.spec().channels, 1);
    assert_eq!(reader.spec().sample_rate, 44_100);
    let decoded: Vec<i16> = reader.samples::<i16>().map(|s| s.unwrap()).collect();
    assert_eq!(decoded, samples);
}

#[test]
fn embeds_full_fidelity_id3_tag_with_art() {
    let audio = vec![0u8; 8];
    let art_bytes = vec![0xCAu8; 120];
    let scan = WavScan {
        fmt: fmt_pcm_16bit_mono(),
        fact: None,
    };
    let tags = vec![
        TagInput::new("title", "Cover Test"),
        TagInput::new("albumartist", "Various"), // no INFO field -> id3 only
    ];
    let arts = vec![ArtInput {
        art_id: 9,
        mime: "image/jpeg".to_string(),
        description: String::new(),
        picture_type: PictureType::new(3).unwrap(),
        width: 0,
        height: 0,
        data_len: BlobLen::new(art_bytes.len() as u64).unwrap(),
    }];

    let layout = synthesize_layout(&scan, 0, audio.len() as u64, &tags, &[], &arts).unwrap();
    // Art is a streamed segment, never materialized inline.
    assert!(
        layout
            .segments()
            .iter()
            .any(|s| matches!(s, Segment::ArtImage { art_id: 9, len, .. } if len.get() == 120))
    );

    let bytes = assemble(&layout, &audio, &[(9, &art_bytes)]);

    // Locate and parse the embedded `id3 ` chunk with the id3 crate.
    let pos = find_chunk(&bytes, b"id3 ").expect("an id3 chunk");
    let tag = id3::Tag::read_from2(Cursor::new(&bytes[pos.0..pos.0 + pos.1])).unwrap();
    assert_eq!(tag.title(), Some("Cover Test"));
    assert_eq!(
        tag.get("TPE2").and_then(|f| f.content().text()),
        Some("Various")
    );
    let pic = tag.pictures().next().expect("a picture frame");
    assert_eq!(pic.data, art_bytes);
}

#[test]
fn emits_native_info_chunk_for_mapped_tags() {
    let audio = vec![0u8; 8];
    let scan = WavScan {
        fmt: fmt_pcm_16bit_mono(),
        fact: None,
    };
    let tags = vec![
        TagInput::new("title", "Hello"),
        TagInput::new("artist", "Bob"),
    ];
    let layout = synthesize_layout(&scan, 0, audio.len() as u64, &tags, &[], &[]).unwrap();
    let bytes = assemble(&layout, &audio, &[]);

    let (off, len) = find_chunk(&bytes, b"LIST").expect("a LIST chunk");
    let body = &bytes[off..off + len];
    assert_eq!(&body[0..4], b"INFO");
    // Skip the leading "INFO" form type, then walk the subchunks.
    let sub = &body[4..];
    // INAM (title) subchunk value is NUL-terminated "Hello".
    let inam = find_chunk(sub, b"INAM").expect("an INAM subchunk");
    assert_eq!(&sub[inam.0..inam.0 + inam.1], b"Hello\0");
}

#[test]
fn pads_odd_data_payload_to_word_boundary() {
    let audio = vec![0xABu8; 7]; // odd length
    let scan = WavScan {
        fmt: fmt_pcm_16bit_mono(),
        fact: None,
    };
    let layout = synthesize_layout(&scan, 0, audio.len() as u64, &[], &[], &[]).unwrap();
    let bytes = assemble(&layout, &audio, &[]);
    // File length is even and total_len accounts for the pad byte.
    assert_eq!(bytes.len() % 2, 0);
    assert_eq!(bytes.len() as u64, layout.total_len());
    // The `data` chunk size field still reports the true (odd) payload length.
    let (off, _) = find_chunk(&bytes, b"data").expect("a data chunk");
    let size = u32::from_le_bytes([
        bytes[off - 4],
        bytes[off - 3],
        bytes[off - 2],
        bytes[off - 1],
    ]);
    assert_eq!(size, 7);
}

#[test]
fn rejects_audio_over_32bit() {
    let scan = WavScan {
        fmt: fmt_pcm_16bit_mono(),
        fact: None,
    };
    let res = synthesize_layout(&scan, 0, u64::from(u32::MAX) + 1, &[], &[], &[]);
    assert_eq!(res, Err(musefs_format::FormatError::TooLarge));
}

/// Find the first chunk with `id`, returning `(payload_offset, payload_len)`.
/// Skips the 12-byte RIFF header when present, else starts at 0.
fn find_chunk(buf: &[u8], id: &[u8; 4]) -> Option<(usize, usize)> {
    let mut pos = if buf.len() >= 12 && &buf[0..4] == b"RIFF" {
        12
    } else {
        0
    };
    while pos + 8 <= buf.len() {
        let cid = &buf[pos..pos + 4];
        let size =
            u32::from_le_bytes([buf[pos + 4], buf[pos + 5], buf[pos + 6], buf[pos + 7]]) as usize;
        if cid == id {
            return Some((pos + 8, size));
        }
        pos += 8 + size + (size & 1);
    }
    None
}

#[test]
fn keeps_real_art_when_mixed_with_empty() {
    let audio = [0u8; 8];
    let art_bytes = [0xCDu8; 64];
    let scan = WavScan {
        fmt: fmt_pcm_16bit_mono(),
        fact: None,
    };
    let tags = vec![TagInput::new("title", "Mixed")];
    let arts = vec![ArtInput {
        art_id: 2,
        mime: "image/jpeg".to_string(),
        description: String::new(),
        picture_type: PictureType::new(3).unwrap(),
        width: 0,
        height: 0,
        data_len: BlobLen::new(art_bytes.len() as u64).unwrap(),
    }];

    let layout = synthesize_layout(&scan, 0, audio.len() as u64, &tags, &[], &arts).unwrap();
    let art_segs: Vec<&Segment> = layout
        .segments()
        .iter()
        .filter(|s| matches!(s, Segment::ArtImage { .. }))
        .collect();
    assert_eq!(art_segs.len(), 1, "only the real art survives");
    assert!(matches!(
        art_segs[0],
        Segment::ArtImage { art_id: 2, len, .. } if len.get() == 64
    ));
}
