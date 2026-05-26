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
            Segment::OggAudio { .. } => unreachable!("no Ogg audio in this fixture"),
            Segment::OggArtSlice { .. } => unreachable!("OggArtSlice only in ogg synthesis"),
        }
    }
    out
}

/// Decode the 28-bit syncsafe ID3v2 tag-size field from the 10-byte header.
fn header_size_field(bytes: &[u8]) -> u64 {
    ((bytes[6] as u64) << 21)
        | ((bytes[7] as u64) << 14)
        | ((bytes[8] as u64) << 7)
        | (bytes[9] as u64)
}

#[test]
fn synthesizes_id3v24_text_frames_and_preserves_audio() {
    let audio = [0xFFu8, 0xFB, 9, 8, 7, 6, 5, 4];
    let tags = vec![
        TagInput::new("artist", "Alice"),
        TagInput::new("title", "Song"),
        TagInput::new("album", "Record"),
    ];
    let layout = synthesize_layout(0, audio.len() as u64, &tags, &[]).unwrap();

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
    let layout = synthesize_layout(0, audio.len() as u64, &tags, &arts).unwrap();

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

#[test]
fn embedded_size_field_matches_the_frame_region() {
    let audio = [0xFFu8, 0xFB, 9, 8, 7, 6, 5, 4];
    let art_bytes = vec![0xBEu8; 150];
    let tags = vec![
        TagInput::new("artist", "Testbed"),
        TagInput::new("title", "Size Check"),
    ];
    let arts = vec![ArtInput {
        art_id: 1,
        mime: "image/jpeg".to_string(),
        description: String::new(),
        picture_type: 3,
        width: 0,
        height: 0,
        data_len: art_bytes.len() as u64,
    }];
    let layout = synthesize_layout(0, audio.len() as u64, &tags, &arts).unwrap();
    let bytes = assemble(&layout, &audio, &[(1, &art_bytes)]);

    // Verify the 10-byte ID3v2.4 header magic and version/flags.
    assert_eq!(&bytes[0..3], b"ID3");
    assert_eq!(&bytes[3..6], &[0x04, 0x00, 0x00]);

    // The embedded syncsafe size must exactly equal the frame region length.
    let expected_frame_region = layout.header_len() - 10;
    assert_eq!(
        header_size_field(&bytes),
        expected_frame_region,
        "syncsafe size field does not match measured frame region"
    );

    // The assembled byte count must match layout.total_len().
    assert_eq!(layout.total_len(), bytes.len() as u64);
}

#[test]
fn empty_tag_when_no_tags_or_art() {
    let audio = [0xFFu8, 0xFB, 0, 0];
    let layout = synthesize_layout(0, audio.len() as u64, &[], &[]).unwrap();

    // Exactly two segments: the 10-byte inline header and the backing audio.
    assert_eq!(layout.segments.len(), 2);
    assert!(matches!(&layout.segments[0], Segment::Inline(b) if b.len() == 10));
    assert!(matches!(&layout.segments[1], Segment::BackingAudio { .. }));

    let bytes = assemble(&layout, &audio, &[]);

    // The 10-byte ID3v2.4 header must be well-formed with size == 0.
    assert_eq!(&bytes[0..3], b"ID3");
    assert_eq!(&bytes[3..6], &[0x04, 0x00, 0x00]);
    assert_eq!(
        header_size_field(&bytes),
        0,
        "empty tag must have size field 0"
    );

    // Audio follows directly after the 10-byte header.
    assert_eq!(&bytes[10..], &audio);
}

#[test]
fn unknown_key_becomes_txxx() {
    let audio = [0xFFu8, 0xFB, 0, 0];
    let tags = vec![TagInput::new("mood", "calm")];
    let layout = synthesize_layout(0, audio.len() as u64, &tags, &[]).unwrap();
    let bytes = assemble(&layout, &audio, &[]);

    let tag = id3::Tag::read_from2(Cursor::new(&bytes)).unwrap();
    let txxx = tag
        .extended_texts()
        .find(|et| et.description == "mood")
        .expect("a TXXX frame with description 'mood'");
    assert_eq!(txxx.value, "calm");
}

#[test]
fn multi_value_text_frame_round_trips() {
    let audio = [0xFFu8, 0xFB, 0, 0];
    let tags = vec![
        TagInput::new("artist", "Alice"),
        TagInput::new("artist", "Bob"),
    ];
    let layout = synthesize_layout(0, audio.len() as u64, &tags, &[]).unwrap();
    let bytes = assemble(&layout, &audio, &[]);

    let tag = id3::Tag::read_from2(Cursor::new(&bytes)).unwrap();
    // TPE1 frame carries both values NUL-joined; split and assert both survive.
    let frame = tag.get("TPE1").expect("a TPE1 frame");
    let raw = frame.content().text().expect("TPE1 content must be text");
    let values: Vec<&str> = raw.split('\0').collect();
    assert!(
        values.contains(&"Alice"),
        "expected 'Alice' in TPE1, got {values:?}"
    );
    assert!(
        values.contains(&"Bob"),
        "expected 'Bob' in TPE1, got {values:?}"
    );
}

#[test]
fn multiple_art_frames_keep_order() {
    let audio = [0xFFu8, 0xFB, 0, 0];
    let art1 = vec![0xAAu8; 50];
    let art2 = vec![0xBBu8; 80];
    let arts = vec![
        ArtInput {
            art_id: 1,
            mime: "image/jpeg".to_string(),
            description: String::new(),
            picture_type: 3,
            width: 0,
            height: 0,
            data_len: art1.len() as u64,
        },
        ArtInput {
            art_id: 2,
            mime: "image/png".to_string(),
            description: String::new(),
            picture_type: 4,
            width: 0,
            height: 0,
            data_len: art2.len() as u64,
        },
    ];
    let layout = synthesize_layout(0, audio.len() as u64, &[], &arts).unwrap();

    // The layout must contain ArtImage segments for art_id 1 then 2.
    let art_segs: Vec<i64> = layout
        .segments
        .iter()
        .filter_map(|s| match s {
            Segment::ArtImage { art_id, .. } => Some(*art_id),
            _ => None,
        })
        .collect();
    assert_eq!(art_segs, vec![1, 2]);

    let bytes = assemble(&layout, &audio, &[(1, &art1), (2, &art2)]);
    let tag = id3::Tag::read_from2(Cursor::new(&bytes)).unwrap();

    let pics: Vec<_> = tag.pictures().collect();
    assert_eq!(pics.len(), 2, "expected 2 picture frames");
    assert_eq!(pics[0].data, art1);
    assert_eq!(pics[1].data, art2);
}

#[test]
fn synthesize_errors_on_oversized_frame() {
    use musefs_format::FormatError;
    // A single frame whose data exceeds the 28-bit per-frame syncsafe limit.
    let arts = vec![ArtInput {
        art_id: 1,
        mime: "image/jpeg".to_string(),
        description: String::new(),
        picture_type: 3,
        width: 0,
        height: 0,
        data_len: 0x1000_0000, // 256 MiB, over the per-frame limit
    }];
    assert_eq!(
        synthesize_layout(0, 0, &[], &arts),
        Err(FormatError::TooLarge)
    );
}

#[test]
fn synthesize_errors_when_frames_sum_past_the_tag_limit() {
    use musefs_format::FormatError;
    // Each frame is individually under the 28-bit per-frame limit, but together
    // they push the accumulated tag size over it — exercising the total guard.
    let art = |id| ArtInput {
        art_id: id,
        mime: "image/jpeg".to_string(),
        description: String::new(),
        picture_type: id as u32,
        width: 0,
        height: 0,
        data_len: 0x0800_0000, // 128 MiB each; two sum past the 256 MiB tag limit
    };
    let arts = vec![art(1), art(2)];
    assert_eq!(
        synthesize_layout(0, 0, &[], &arts),
        Err(FormatError::TooLarge)
    );
}
