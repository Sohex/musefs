use super::*;
use musefs_format::ogg::page_test_support::{
    build_header_pub, lace_packet_pub, vorbis_body_empty, vorbis_body_with,
};
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

/// A FLAC PICTURE block body carrying a one-byte PNG, base64-encoded the way a
/// `METADATA_BLOCK_PICTURE` comment value is.
fn encoded_picture(marker: u8) -> String {
    use base64::Engine;
    let mut block = Vec::new();
    block.extend_from_slice(&3u32.to_be_bytes()); // picture type: front cover
    block.extend_from_slice(&9u32.to_be_bytes());
    block.extend_from_slice(b"image/png");
    block.extend_from_slice(&0u32.to_be_bytes()); // description length
    block.extend_from_slice(&1u32.to_be_bytes()); // width
    block.extend_from_slice(&1u32.to_be_bytes()); // height
    block.extend_from_slice(&8u32.to_be_bytes()); // depth
    block.extend_from_slice(&0u32.to_be_bytes()); // colors used
    block.extend_from_slice(&1u32.to_be_bytes()); // image length
    block.push(marker);
    base64::engine::general_purpose::STANDARD.encode(&block)
}

#[test]
fn probe_logs_an_undecodable_picture_and_keeps_the_others() {
    // The scan path calls the reader as `.unwrap_or_default()`, so a dropped
    // picture reaches the operator only if it is logged here (#673).
    crate::warn_limit::log_capture::install();

    let good = encoded_picture(0xAB);
    let head = b"OpusHead\x01\x02\x38\x01\x80\xbb\x00\x00\x00\x00\x00".to_vec();
    let mut tags = b"OpusTags".to_vec();
    tags.extend_from_slice(&vorbis_body_with(&[
        ("METADATA_BLOCK_PICTURE", "not!valid!base64"),
        ("METADATA_BLOCK_PICTURE", &good),
    ]));
    let (mut bytes, _) = build_header_pub(0x1234, &[&head, &tags]);
    let (audio, _) = lace_packet_pub(0x1234, 2, false, 960, &[0u8; 100]);
    bytes.extend_from_slice(&audio);

    let dir = tempfile::tempdir().unwrap();
    // A path unique to this test: the capture buffer is shared by the whole
    // test binary, so the needle has to pick out only these records.
    let path = dir.path().join("undecodable-art-probe.opus");
    std::fs::File::create(&path)
        .unwrap()
        .write_all(&bytes)
        .unwrap();

    let probed = probe_full(&path, &bytes).expect("opus should probe");
    // The valid picture survives the bad one.
    assert_eq!(probed.pictures.len(), 1);
    assert_eq!(probed.pictures[0].data, vec![0xAB]);

    let logged = crate::warn_limit::log_capture::messages_containing("undecodable-art-probe.opus");
    assert_eq!(logged.len(), 1, "one drop, one warn line: {logged:?}");
    assert!(logged[0].contains("undecodable base64"), "{}", logged[0]);
    assert!(logged[0].contains("16 bytes"), "{}", logged[0]);
}
