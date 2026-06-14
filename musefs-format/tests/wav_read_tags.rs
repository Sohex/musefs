mod common;

use common::{build_wav, fmt_pcm_16bit_mono};
use id3::{TagLike, Version};
use musefs_format::wav::{read_pictures, read_tags};

/// An `INFO` payload (FourCC + NUL-terminated, word-aligned subchunk values).
fn info_payload(pairs: &[(&[u8; 4], &str)]) -> Vec<u8> {
    let mut p = b"INFO".to_vec();
    for (cc, val) in pairs {
        let mut v = val.as_bytes().to_vec();
        v.push(0x00);
        p.extend_from_slice(*cc);
        p.extend_from_slice(&u32::try_from(v.len()).unwrap().to_le_bytes());
        p.extend_from_slice(&v);
        if v.len() % 2 == 1 {
            p.push(0x00);
        }
    }
    p
}

/// A standalone ID3v2.4 tag (with a picture) built by the id3 crate.
fn id3_payload_with_picture() -> Vec<u8> {
    let mut tag = id3::Tag::new();
    tag.set_title("Id3 Title");
    tag.set_artist("Id3 Artist");
    tag.add_frame(id3::frame::Picture {
        mime_type: "image/png".to_string(),
        picture_type: id3::frame::PictureType::CoverFront,
        description: String::new(),
        data: vec![0x89, 0x50, 0x4E, 0x47, 1, 2, 3, 4],
    });
    let mut buf = Vec::new();
    tag.write_to(&mut buf, Version::Id3v24).unwrap();
    buf
}

#[test]
fn reads_info_only() {
    let wav = build_wav(&[
        (b"fmt ", fmt_pcm_16bit_mono()),
        (
            b"LIST",
            info_payload(&[(b"INAM", "Info Title"), (b"IART", "Info Artist")]),
        ),
        (b"data", vec![0u8; 4]),
    ]);
    let tags = read_tags(&wav);
    assert!(tags.contains(&("title".to_string(), "Info Title".to_string())));
    assert!(tags.contains(&("artist".to_string(), "Info Artist".to_string())));
    assert!(read_pictures(&wav).is_empty());
}

#[test]
fn reads_id3_only_including_art() {
    let wav = build_wav(&[
        (b"fmt ", fmt_pcm_16bit_mono()),
        (b"data", vec![0u8; 4]),
        (b"id3 ", id3_payload_with_picture()), // trailing metadata chunk
    ]);
    let tags = read_tags(&wav);
    assert!(tags.contains(&("title".to_string(), "Id3 Title".to_string())));
    let pics = read_pictures(&wav);
    assert_eq!(pics.len(), 1);
    assert_eq!(pics[0].mime, "image/png");
}

#[test]
fn merges_with_id3_winning_and_info_filling_gaps() {
    // id3 has title+artist; INFO has artist (loses) + genre (fills a gap).
    let wav = build_wav(&[
        (b"fmt ", fmt_pcm_16bit_mono()),
        (
            b"LIST",
            info_payload(&[(b"IART", "Info Artist"), (b"IGNR", "Ambient")]),
        ),
        (b"data", vec![0u8; 4]),
        (b"id3 ", id3_payload_with_picture()),
    ]);
    let tags = read_tags(&wav);
    // id3 artist wins; INFO artist is dropped.
    assert!(tags.contains(&("artist".to_string(), "Id3 Artist".to_string())));
    assert!(!tags.contains(&("artist".to_string(), "Info Artist".to_string())));
    // INFO genre fills the gap (no genre in id3).
    assert!(tags.contains(&("genre".to_string(), "Ambient".to_string())));
}

#[test]
fn returns_empty_when_untagged() {
    let wav = build_wav(&[(b"fmt ", fmt_pcm_16bit_mono()), (b"data", vec![0u8; 4])]);
    assert!(read_tags(&wav).is_empty());
    assert!(read_pictures(&wav).is_empty());
}
