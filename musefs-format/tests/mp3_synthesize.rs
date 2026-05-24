use std::io::Cursor;

use id3::TagLike;
use musefs_format::mp3::synthesize_layout;
use musefs_format::{ArtInput, RegionLayout, Segment, TagInput};

/// Flatten a layout into a byte buffer, substituting `audio` for the backing-audio
/// segment and the matching bytes for each `ArtImage` segment.
fn assemble(layout: &RegionLayout, audio: &[u8], arts: &[(i64, &[u8])]) -> Vec<u8> {
    let mut out = Vec::new();
    for seg in &layout.segments {
        match seg {
            Segment::Inline(b) => out.extend_from_slice(b),
            Segment::BackingAudio { .. } => out.extend_from_slice(audio),
            Segment::ArtImage { art_id, .. } => {
                let bytes = arts.iter().find(|(id, _)| id == art_id).unwrap().1;
                out.extend_from_slice(bytes);
            }
        }
    }
    out
}

#[test]
fn synthesizes_id3v24_text_frames_and_preserves_audio() {
    let audio = [0xFFu8, 0xFB, 9, 8, 7, 6, 5, 4];
    let tags = vec![
        TagInput::new("artist", "Alice"),
        TagInput::new("title", "Song"),
        TagInput::new("album", "Record"),
    ];
    let layout = synthesize_layout(0, audio.len() as u64, &tags, &[]);

    // total_len must equal the bytes actually produced (generate-and-measure).
    let bytes = assemble(&layout, &audio, &[]);
    assert_eq!(bytes.len() as u64, layout.total_len());

    // Independent oracle: the id3 crate parses our synthesized tag.
    let tag = id3::Tag::read_from2(Cursor::new(&bytes)).unwrap();
    assert_eq!(tag.artist(), Some("Alice"));
    assert_eq!(tag.title(), Some("Song"));
    assert_eq!(tag.album(), Some("Record"));

    // The original audio frames follow the tag unchanged.
    assert_eq!(&bytes[bytes.len() - audio.len()..], &audio);
}

#[test]
fn synthesizes_apic_with_streamed_image_bytes() {
    let audio = [0xFFu8, 0xFB, 0, 0];
    let art_bytes = vec![0xCAu8; 200];
    let tags = vec![TagInput::new("title", "Cover")];
    let arts = vec![ArtInput {
        art_id: 7,
        mime: "image/jpeg".to_string(),
        description: String::new(),
        picture_type: 3, // front cover
        width: 0,
        height: 0,
        data_len: art_bytes.len() as u64,
    }];
    let layout = synthesize_layout(0, audio.len() as u64, &tags, &arts);

    // The image is a streamed segment, not materialized inline.
    assert!(layout
        .segments
        .iter()
        .any(|s| matches!(s, Segment::ArtImage { art_id: 7, len } if *len == 200)));

    let bytes = assemble(&layout, &audio, &[(7, &art_bytes)]);
    assert_eq!(bytes.len() as u64, layout.total_len());

    let tag = id3::Tag::read_from2(Cursor::new(&bytes)).unwrap();
    let pic = tag.pictures().next().expect("a picture frame");
    assert_eq!(pic.mime_type, "image/jpeg");
    assert_eq!(pic.data, art_bytes);
    assert_eq!(tag.title(), Some("Cover"));
}
