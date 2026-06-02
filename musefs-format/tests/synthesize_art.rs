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
    let layout = synthesize_layout(&scan, &[TagInput::new("title", "T")], &[art]).unwrap();

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
    let assembled = resolve_layout(&layout, &file, &art_map, &HashMap::new());
    assert_eq!(assembled.len() as u64, layout.total_len());
    assert_eq!(&assembled[layout.header_len() as usize..], &audio[..]);
}

#[test]
fn metaflac_reads_synthesized_picture() {
    let (file, _audio) = fixture();
    let scan = locate_audio(&file).unwrap();

    let image = vec![0x77u8; 1234];
    let art = cover(42, image.len() as u64);
    let layout = synthesize_layout(&scan, &[TagInput::new("title", "T")], &[art]).unwrap();

    let mut art_map = HashMap::new();
    art_map.insert(42i64, image.clone());
    let assembled = resolve_layout(&layout, &file, &art_map, &HashMap::new());

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

#[test]
fn synthesize_errors_on_oversized_picture() {
    use musefs_format::flac::FlacScan;
    use musefs_format::FormatError;
    let scan = FlacScan {
        audio_offset: 0,
        audio_length: 0,
        preserved: vec![],
    };
    // data_len is only a count here (bytes are streamed), so this needs no allocation.
    let art = ArtInput {
        art_id: 1,
        mime: "image/png".to_string(),
        description: String::new(),
        picture_type: 3,
        width: 0,
        height: 0,
        data_len: 0x0100_0000, // just over the 24-bit FLAC PICTURE block limit
    };
    assert_eq!(
        synthesize_layout(&scan, &[], &[art]),
        Err(FormatError::TooLarge)
    );
}

#[test]
fn zero_byte_art_is_skipped_so_the_track_still_serves() {
    let (file, audio) = fixture();
    let scan = locate_audio(&file).unwrap();

    // A picture with no image data is degenerate; synthesis must skip it rather than
    // emit an empty PICTURE block (which would fail layout validation and brick the
    // track).
    let art = cover(7, 0); // data_len == 0
    let layout = synthesize_layout(&scan, &[TagInput::new("title", "T")], &[art]).unwrap();

    // No ArtImage segment was emitted.
    assert!(!layout
        .segments
        .iter()
        .any(|s| matches!(s, Segment::ArtImage { .. })));

    // The track still round-trips: header + verbatim audio (no art bytes needed).
    let art_map = HashMap::new();
    let assembled = resolve_layout(&layout, &file, &art_map, &HashMap::new());
    assert_eq!(assembled.len() as u64, layout.total_len());
    assert_eq!(&assembled[layout.header_len() as usize..], &audio[..]);

    // metaflac sees a valid FLAC with zero pictures.
    let tag = metaflac::Tag::read_from(&mut Cursor::new(&assembled)).expect("valid FLAC metadata");
    assert_eq!(tag.pictures().count(), 0);
}

#[test]
fn zero_byte_art_skipped_among_valid_art_keeps_block_framing_valid() {
    // Guards the `is_last` flag: filtering the empty art must not leave the final
    // real PICTURE block without its last-block bit. metaflac parsing the whole
    // chain validates that.
    let (file, _audio) = fixture();
    let scan = locate_audio(&file).unwrap();
    let image = vec![0x55u8; 64];
    let empty = cover(1, 0);
    let real = cover(2, image.len() as u64);
    let layout = synthesize_layout(&scan, &[TagInput::new("title", "T")], &[empty, real]).unwrap();

    let art_segs: Vec<_> = layout
        .segments
        .iter()
        .filter(|s| matches!(s, Segment::ArtImage { .. }))
        .collect();
    assert_eq!(art_segs.len(), 1);

    let mut art_map = HashMap::new();
    art_map.insert(2i64, image.clone());
    let assembled = resolve_layout(&layout, &file, &art_map, &HashMap::new());

    // Re-parse the synthesized chain: locate_audio follows the last-block flag, so
    // if the empty art were miscounted (leaving the final real block without its
    // last-block bit) the walk would run past the metadata into the audio frames
    // and fail. This pins the `data_len > 0` filter against `>= 0`.
    let rescan = locate_audio(&assembled).expect("synthesized FLAC must parse");
    assert_eq!(rescan.audio_offset, layout.header_len());

    let tag = metaflac::Tag::read_from(&mut Cursor::new(&assembled)).expect("valid FLAC metadata");
    let pics: Vec<_> = tag.pictures().collect();
    assert_eq!(pics.len(), 1);
    assert_eq!(pics[0].data, image);
}
