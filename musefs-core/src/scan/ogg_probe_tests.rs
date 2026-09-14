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

/// Stream A (serial 0x1234) complete, then a second logical bitstream under its
/// own serial: a chain in the shape RFC 3533 defines.
fn chained_opus_bytes() -> Vec<u8> {
    let head = b"OpusHead\x01\x02\x38\x01\x80\xbb\x00\x00\x00\x00\x00".to_vec();
    let mut tags = b"OpusTags".to_vec();
    tags.extend_from_slice(&vorbis_body_empty());
    let (mut bytes, _) = build_header_pub(0x1234, &[&head, &tags]);
    let (audio, _) = lace_packet_pub(0x1234, 2, false, 960, &[0u8; 100]);
    bytes.extend_from_slice(&audio);

    let (b_header, b_pages) = build_header_pub(0x5678, &[&head, &tags]);
    bytes.extend_from_slice(&b_header);
    let (b_audio, _) = lace_packet_pub(0x5678, b_pages, false, 960, &[1u8; 100]);
    bytes.extend_from_slice(&b_audio);
    bytes
}

#[test]
fn scan_skips_a_chained_ogg_and_says_so() {
    // Chained Ogg was accepted, and a tag edit then renumbered the second
    // stream's pages into self-consistent corruption (#722). Both probe paths
    // must now refuse the file, and the operator must be told why.
    crate::warn_limit::log_capture::install();
    let bytes = chained_opus_bytes();

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("chained-scan.opus");
    std::fs::File::create(&path)
        .unwrap()
        .write_all(&bytes)
        .unwrap();

    assert!(
        probe_full(&path, &bytes).is_none(),
        "the oracle path must refuse it too"
    );

    let db = musefs_db::Db::open_in_memory().unwrap();
    let stats = crate::scan_directory(&db, &path).unwrap();
    assert_eq!(stats.scanned, 0);
    assert_eq!(stats.failed, 1);

    let logged = crate::warn_limit::log_capture::messages_containing("chained-scan.opus");
    assert_eq!(logged.len(), 1, "one skip, one line: {logged:?}");
    assert!(logged[0].contains("chained Ogg"), "{}", logged[0]);
}

/// Plant the row an older binary stored for `path`: every build since #722
/// refuses a chained file, so no scan of this one would write it (#747).
fn plant_stored_row(db: &musefs_db::Db, path: &std::path::Path) {
    let meta = std::fs::metadata(path).unwrap();
    let stamp = BackingStamp::from_metadata(&meta);
    db.upsert_track(&musefs_db::NewTrack {
        backing_path: std::fs::canonicalize(path).unwrap(),
        format: Format::Opus,
        audio_offset: 0,
        audio_length: meta.len(),
        backing_size: stamp.size,
        backing_mtime_ns: stamp.mtime_ns,
        backing_ctime_ns: stamp.ctime_ns,
        // As V4 leaves every row, which makes revalidate re-probe it.
        backing_ino: None,
    })
    .unwrap();
}

/// #747: a chained Ogg row stored before 2.0.0 fails every revalidate — the
/// probe refuses it and nothing is written — and neither a rescan nor pruning
/// missing files removes it. `--prune` does, and only for that refusal: a stored
/// file that merely fails to parse, which a download still in progress can,
/// keeps its row.
#[test]
fn revalidate_prunes_a_stored_chained_ogg_only_when_asked() {
    crate::warn_limit::log_capture::install();
    let dir = tempfile::tempdir().unwrap();
    let chained = dir.path().join("stuck-chained.opus");
    std::fs::write(&chained, chained_opus_bytes()).unwrap();
    let broken = dir.path().join("stuck-broken.opus");
    std::fs::write(&broken, b"OggS and nothing after it").unwrap();

    let db = musefs_db::Db::open_in_memory().unwrap();
    plant_stored_row(&db, &chained);
    plant_stored_row(&db, &broken);

    for pass in 0..2 {
        let stats = crate::revalidate(&db, dir.path()).unwrap();
        assert_eq!(
            (stats.failed, stats.pruned),
            (2, 0),
            "pass {pass}: both fail, neither is pruned unasked"
        );
        assert_eq!(db.list_tracks().unwrap().len(), 2);
    }
    // Unasked, the run says what `--prune` would remove. The count is this
    // test's own: no other test stores a refused file, so no other can log it.
    let told = crate::warn_limit::log_capture::messages_containing(
        "1 stored track(s) are in a form this version refuses to serve",
    );
    assert!(!told.is_empty(), "an unpruned refusal is reported");

    let opts = ScanOptions {
        prune: true,
        ..ScanOptions::default()
    };
    let stats = crate::revalidate_with(&db, dir.path(), &opts).unwrap();
    assert_eq!(stats.pruned, 1, "the chained row, and only it");
    let left = db.list_tracks().unwrap();
    assert_eq!(left.len(), 1);
    assert!(
        left[0].backing_path.ends_with("stuck-broken.opus"),
        "the unparseable file keeps its row: {}",
        left[0].backing_path.display()
    );

    let stats = crate::revalidate(&db, dir.path()).unwrap();
    assert_eq!(stats.failed, 1, "the failure count comes back down");
}

#[test]
fn scan_ingests_an_oggflac_whose_header_count_is_unknown() {
    // A zero following-packet count means "unknown", not "none": the real
    // VORBIS_COMMENT still follows. Reading it as "none" left the tags inside
    // the audio region, un-ingested and replayed by synthesis (#723).
    let mut streaminfo = Vec::new();
    streaminfo.push(0u8); // STREAMINFO, not the last block
    streaminfo.extend_from_slice(&34u32.to_be_bytes()[1..]); // 24-bit length
    streaminfo.extend(std::iter::repeat_n(0u8, 34));

    let mut mapping = vec![0x7F];
    mapping.extend_from_slice(b"FLAC");
    mapping.push(1);
    mapping.push(0);
    mapping.extend_from_slice(&0u16.to_be_bytes()); // count: unknown
    mapping.extend_from_slice(b"fLaC");
    mapping.extend_from_slice(&streaminfo);

    let body = vorbis_body_with(&[("title", "RealTitle")]);
    let mut comment = vec![0x80 | 4]; // VORBIS_COMMENT, last block
    comment.extend_from_slice(&u32::try_from(body.len()).unwrap().to_be_bytes()[1..]);
    comment.extend_from_slice(&body);

    let (mut bytes, pages) = build_header_pub(0x4321, &[&mapping, &comment]);
    let header_len = bytes.len();
    let (audio, _) = lace_packet_pub(0x4321, pages, false, 4096, &[0xFFu8, 0xF8, 0x69, 0x18]);
    bytes.extend_from_slice(&audio);

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("unknown-count.oga");
    std::fs::File::create(&path)
        .unwrap()
        .write_all(&bytes)
        .unwrap();

    let probed = probe_full(&path, &bytes).expect("oggflac should probe");
    assert_eq!(probed.format, Format::OggFlac);
    assert_eq!(probed.audio_offset, header_len as u64);
    assert_eq!(
        probed.tags,
        vec![("title".to_string(), "RealTitle".to_string())]
    );

    let db = musefs_db::Db::open_in_memory().unwrap();
    let stats = crate::scan_directory(&db, &path).unwrap();
    assert_eq!(stats.scanned, 1);
    assert_eq!(stats.failed, 0);
}
