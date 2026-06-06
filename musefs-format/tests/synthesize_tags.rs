mod common;
use std::collections::HashMap;
use std::io::Cursor;

use common::{make_flac, resolve_layout, streaminfo_body, vorbis_comment_body};
use musefs_format::flac::{locate_audio, synthesize_layout};
use musefs_format::{Segment, TagInput};

fn fixture() -> (Vec<u8>, Vec<u8>) {
    let si = streaminfo_body();
    let vc = vorbis_comment_body("oldvendor", &["TITLE=Old"]);
    let audio = vec![0xAB; 64];
    let file = make_flac(&[(0, si), (4, vc)], &audio);
    (file, audio)
}

#[test]
fn measured_lengths_match_assembled_bytes() {
    let (file, audio) = fixture();
    let scan = locate_audio(&file).unwrap();

    let tags = vec![
        TagInput::new("title", "New Title"),
        TagInput::new("artist", "A"),
    ];
    let layout = synthesize_layout(
        &scan.preserved,
        scan.audio_offset,
        scan.audio_length,
        &tags,
        &[],
        &[],
    )
    .unwrap();

    let assembled = resolve_layout(&layout, &file, &HashMap::new(), &HashMap::new());
    assert_eq!(assembled.len() as u64, layout.total_len());
    assert_eq!(
        layout.header_len(),
        assembled.len() as u64 - audio.len() as u64
    );
    assert_eq!(
        &assembled[usize::try_from(layout.header_len()).unwrap()..],
        &audio[..]
    );
}

#[test]
fn metaflac_reads_synthesized_vorbis_comments_and_preserves_streaminfo() {
    let (file, _audio) = fixture();
    let scan = locate_audio(&file).unwrap();

    let tags = vec![
        TagInput::new("title", "New Title"),
        TagInput::new("artist", "First"),
        TagInput::new("artist", "Second"),
    ];
    let layout = synthesize_layout(
        &scan.preserved,
        scan.audio_offset,
        scan.audio_length,
        &tags,
        &[],
        &[],
    )
    .unwrap();
    let assembled = resolve_layout(&layout, &file, &HashMap::new(), &HashMap::new());

    let tag = metaflac::Tag::read_from(&mut Cursor::new(&assembled)).expect("valid FLAC metadata");

    let vc = tag.vorbis_comments().expect("vorbis comments present");
    assert_eq!(
        vc.get("TITLE").map(std::vec::Vec::as_slice),
        Some(["New Title".to_string()].as_slice())
    );
    assert_eq!(
        vc.get("ARTIST").map(std::vec::Vec::as_slice),
        Some(["First".to_string(), "Second".to_string()].as_slice())
    );

    let si = tag.get_streaminfo().expect("streaminfo present");
    assert_eq!(si.sample_rate, 44100);
    assert_eq!(si.num_channels, 2);
}

#[test]
fn vorbis_comment_block_is_the_last_metadata_block_when_no_art() {
    let (file, _audio) = fixture();
    let scan = locate_audio(&file).unwrap();
    let layout = synthesize_layout(
        &scan.preserved,
        scan.audio_offset,
        scan.audio_length,
        &[TagInput::new("title", "X")],
        &[],
        &[],
    )
    .unwrap();

    assert_eq!(layout.segments.len(), 2);
    assert!(matches!(layout.segments[0], Segment::Inline(_)));
    assert!(matches!(layout.segments[1], Segment::BackingAudio { .. }));
}
