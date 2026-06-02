mod common;
use common::{make_flac, streaminfo_body, vorbis_comment_body};
use musefs_core::scan_directory;

// Block types: STREAMINFO=0, APPLICATION=2, SEEKTABLE=3, VORBIS_COMMENT=4, CUESHEET=5.
fn fixture() -> Vec<u8> {
    let blocks = vec![
        (0u8, streaminfo_body()),
        (2u8, b"testAPPLICATION-PAYLOAD".to_vec()),
        (3u8, vec![0xEE; 36]), // SEEKTABLE
        (
            4u8,
            vorbis_comment_body("v", &["ARTIST=Alice", "TITLE=Song"]),
        ),
        (5u8, vec![0x11; 48]), // CUESHEET
    ];
    make_flac(&blocks, &vec![0xCD; 4096])
}

#[test]
fn scan_splits_flac_into_structural_store_and_binary_tags() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.flac"), fixture()).unwrap();

    let db = musefs_db::Db::open_in_memory().unwrap();
    scan_directory(&db, dir.path()).unwrap();

    // Exactly one track; fetch its id via the structural query over both tracks.
    let track_id = 1i64; // first upserted track

    let structural = db.get_structural_blocks(track_id).unwrap();
    let kinds: Vec<&str> = structural.iter().map(|b| b.kind.as_str()).collect();
    assert!(kinds.contains(&"STREAMINFO"));
    assert!(kinds.contains(&"SEEKTABLE"));
    assert_eq!(structural.len(), 2);

    let binary = db.get_binary_tags(track_id).unwrap();
    let bkeys: Vec<&str> = binary.iter().map(|b| b.key.as_str()).collect();
    assert!(bkeys.contains(&"APPLICATION"));
    assert!(bkeys.contains(&"CUESHEET"));
    assert_eq!(binary.len(), 2);
}
