mod common;
use common::{make_flac, streaminfo_body, vorbis_comment_body};
use musefs_format::flac::locate_audio;
use musefs_format::FormatError;

#[test]
fn locates_audio_after_metadata_and_preserves_streaminfo() {
    let si = streaminfo_body();
    let vc = vorbis_comment_body("oldvendor", &["TITLE=Old"]);
    let audio = vec![0xAA; 50];
    let file = make_flac(&[(0, si.clone()), (4, vc)], &audio);

    let scan = locate_audio(&file).unwrap();

    assert_eq!(scan.audio_offset, (file.len() - audio.len()) as u64);
    assert_eq!(scan.audio_length, audio.len() as u64);

    assert_eq!(scan.preserved.len(), 1);
    assert_eq!(scan.preserved[0].block_type, 0);
    assert_eq!(scan.preserved[0].body, si);
}

#[test]
fn preserves_structural_blocks_but_not_padding() {
    let si = streaminfo_body();
    let seektable = vec![0u8; 18];
    let padding = vec![0u8; 32];
    let audio = vec![0x11; 10];
    let file = make_flac(&[(0, si.clone()), (3, seektable.clone()), (1, padding)], &audio);

    let scan = locate_audio(&file).unwrap();

    let types: Vec<u8> = scan.preserved.iter().map(|b| b.block_type).collect();
    assert_eq!(types, vec![0, 3], "STREAMINFO + SEEKTABLE preserved, PADDING dropped");
    assert_eq!(scan.preserved[1].body, seektable);
    assert_eq!(scan.audio_length, audio.len() as u64);
}

#[test]
fn rejects_non_flac_input() {
    assert_eq!(locate_audio(b"NOPExxxx").unwrap_err(), FormatError::NotFlac);
}

#[test]
fn rejects_truncated_metadata() {
    let mut file = Vec::new();
    file.extend_from_slice(b"fLaC");
    file.extend_from_slice(&[0x80, 0x00, 0x03, 0xE8]); // last block, type 0, len 1000
    assert_eq!(locate_audio(&file).unwrap_err(), FormatError::Malformed);
}
