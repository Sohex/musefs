mod common;
use std::collections::HashMap;
use std::io::Cursor;

use common::{make_flac, resolve_layout, streaminfo_body, vorbis_comment_body};
use musefs_format::flac::{locate_audio, synthesize_layout};
use musefs_format::{ArtInput, Segment, TagInput};

fn fixture() -> (Vec<u8>, Vec<u8>) {
    let si = streaminfo_body();
    let vc = vorbis_comment_body("oldvendor", &["TITLE=Old"]);
    let audio = vec![0xCD; 80];
    let file = make_flac(&[(0, si), (4, vc)], &audio);
    (file, audio)
}

fn cover(art_id: i64, data_len: u64) -> ArtInput {
    ArtInput {
        art_id,
        mime: "image/jpeg".to_string(),
        description: "front".to_string(),
        picture_type: 3,
        width: 500,
        height: 500,
        data_len,
    }
}

#[test]
fn art_becomes_an_artimage_segment_and_lengths_are_exact() {
    let (file, audio) = fixture();
    let scan = locate_audio(&file).unwrap();

    let image = vec![0x77u8; 1234];
    let art = cover(42, image.len() as u64);
    let layout = synthesize_layout(&scan, &[TagInput::new("title", "T")], &[art]);

    let art_segs: Vec<&Segment> = layout
        .segments
        .iter()
        .filter(|s| matches!(s, Segment::ArtImage { .. }))
        .collect();
    assert_eq!(art_segs.len(), 1);
    assert_eq!(
        *art_segs[0],
        Segment::ArtImage {
            art_id: 42,
            len: 1234
        }
    );

    let mut art_map = HashMap::new();
    art_map.insert(42i64, image.clone());
    let assembled = resolve_layout(&layout, &file, &art_map);
    assert_eq!(assembled.len() as u64, layout.total_len());
    assert_eq!(&assembled[layout.header_len() as usize..], &audio[..]);
}

#[test]
fn metaflac_reads_synthesized_picture() {
    let (file, _audio) = fixture();
    let scan = locate_audio(&file).unwrap();

    let image = vec![0x77u8; 1234];
    let art = cover(42, image.len() as u64);
    let layout = synthesize_layout(&scan, &[TagInput::new("title", "T")], &[art]);

    let mut art_map = HashMap::new();
    art_map.insert(42i64, image.clone());
    let assembled = resolve_layout(&layout, &file, &art_map);

    let tag = metaflac::Tag::read_from(&mut Cursor::new(&assembled)).expect("valid FLAC metadata");
    let pics: Vec<_> = tag.pictures().collect();
    assert_eq!(pics.len(), 1);
    let p = pics[0];
    assert_eq!(p.mime_type, "image/jpeg");
    assert_eq!(p.description, "front");
    assert_eq!(p.width, 500);
    assert_eq!(p.height, 500);
    assert_eq!(p.data, image);
}
