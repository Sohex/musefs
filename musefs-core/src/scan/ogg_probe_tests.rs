use super::*;
use musefs_format::ogg::page_test_support::{build_header_pub, lace_packet_pub, vorbis_body_empty};
use std::io::Write;

#[test]
fn probe_detects_opus_and_seeds_tags() {
    let head = b"OpusHead\x01\x02\x38\x01\x80\xbb\x00\x00\x00\x00\x00".to_vec();
    let mut tags = b"OpusTags".to_vec();
    tags.extend_from_slice(&vorbis_body_empty());
    let (mut bytes, _) = build_header_pub(0x1234, &[&head, &tags]);
    let (audio, _) = lace_packet_pub(0x1234, 2, false, 960, &[0u8; 100]);
    bytes.extend_from_slice(&audio);

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("song.opus");
    std::fs::File::create(&path)
        .unwrap()
        .write_all(&bytes)
        .unwrap();

    let probed = probe_full(&path, &bytes).expect("opus should probe");
    assert_eq!(probed.format, Format::Opus);
    assert_eq!(probed.audio_offset, (bytes.len() - audio.len()) as u64);
}

#[test]
fn scan_single_opus_file_ingests_it() {
    let head = b"OpusHead\x01\x02\x38\x01\x80\xbb\x00\x00\x00\x00\x00".to_vec();
    let mut tags = b"OpusTags".to_vec();
    tags.extend_from_slice(&vorbis_body_empty());
    let (mut bytes, _) = build_header_pub(0x1234, &[&head, &tags]);
    let (audio, _) = lace_packet_pub(0x1234, 2, false, 960, &[0u8; 100]);
    bytes.extend_from_slice(&audio);

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("single.opus");
    std::io::Write::write_all(&mut std::fs::File::create(&path).unwrap(), &bytes).unwrap();

    let db = musefs_db::Db::open_in_memory().unwrap();
    // Pass the FILE path directly (not the directory).
    let stats = crate::scan_directory(&db, &path).unwrap();
    assert_eq!(stats.scanned, 1);
    assert_eq!(stats.skipped, 0);
}

#[test]
fn probe_recognizes_oga_alias() {
    let head = b"OpusHead\x01\x02\x38\x01\x80\xbb\x00\x00\x00\x00\x00".to_vec();
    let mut tags = b"OpusTags".to_vec();
    tags.extend_from_slice(&vorbis_body_empty());
    let (mut bytes, _) = build_header_pub(0x1234, &[&head, &tags]);
    let (audio, _) = lace_packet_pub(0x1234, 2, false, 960, &[0u8; 100]);
    bytes.extend_from_slice(&audio);

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("song.oga");
    std::fs::File::create(&path)
        .unwrap()
        .write_all(&bytes)
        .unwrap();

    let probed = probe_full(&path, &bytes).expect("oga should probe");
    assert_eq!(probed.format, Format::Opus);
}
