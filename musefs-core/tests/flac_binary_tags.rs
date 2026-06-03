mod common;
use common::{make_flac, streaminfo_body, vorbis_comment_body};
use musefs_core::{scan_directory, MountConfig, Musefs, VirtualTree};
use std::collections::BTreeMap;

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

    // Derive the id from DB state rather than assuming row-id ordering.
    let track_id = db.list_tracks().unwrap()[0].id;

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

fn config() -> MountConfig {
    MountConfig {
        template: "$artist/$title".to_string(),
        fallbacks: BTreeMap::new(),
        default_fallback: "Unknown".to_string(),
        mode: musefs_core::Mode::Synthesis,
        poll_interval: std::time::Duration::ZERO,
    }
}

fn read_whole(fs: &Musefs, inode: u64) -> Vec<u8> {
    let size = fs.getattr(inode).unwrap().size;
    let fh = fs.open_handle(inode).unwrap();
    let mut out = Vec::new();
    let mut off = 0u64;
    while off < size {
        let got = fs.read(inode, fh, off, 64 * 1024).unwrap();
        if got.is_empty() {
            break;
        }
        off += got.len() as u64;
        out.extend_from_slice(&got);
    }
    fs.release_handle(fh);
    out
}

// Fixture without CUESHEET (whose dummy body is too short for metaflac to parse).
fn serve_fixture() -> Vec<u8> {
    let blocks = vec![
        (0u8, streaminfo_body()),
        (2u8, b"testAPPLICATION-PAYLOAD".to_vec()),
        (3u8, vec![0xEE; 36]), // SEEKTABLE
        (
            4u8,
            vorbis_comment_body("v", &["ARTIST=Alice", "TITLE=Song"]),
        ),
    ];
    make_flac(&blocks, &vec![0xCD; 4096])
}

#[test]
fn rescanned_flac_serves_valid_file_with_binary_tags() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.flac"), serve_fixture()).unwrap();
    let db = musefs_db::Db::open_in_memory().unwrap();
    scan_directory(&db, dir.path()).unwrap();
    let fs = Musefs::open(db, config()).unwrap();

    let artist = fs.lookup(VirtualTree::ROOT, "Alice").unwrap();
    let (_, inode, _) = fs.readdir(artist).unwrap().into_iter().next().unwrap();
    let served = read_whole(&fs, inode);

    // Valid FLAC framing, and the APPLICATION block survived the round trip.
    let tag = metaflac::Tag::read_from(&mut std::io::Cursor::new(&served)).expect("valid FLAC");
    let (id, data) = tag
        .blocks()
        .find_map(|b| match b {
            metaflac::Block::Application(a) => Some((a.id.clone(), a.data.clone())),
            _ => None,
        })
        .expect("application block present");
    assert_eq!(&id, b"test");
    assert_eq!(data, b"APPLICATION-PAYLOAD");
}

use musefs_db::{Format, NewTrack, Tag};

#[test]
fn legacy_flac_without_structural_rows_serves_via_front_read_fallback() {
    let dir = tempfile::tempdir().unwrap();
    let bytes = serve_fixture();
    let path = dir.path().join("legacy.flac");
    std::fs::write(&path, &bytes).unwrap();
    let meta = std::fs::metadata(&path).unwrap();

    // Simulate a V1-scanned track: a track row + text tags, but NO structural_blocks
    // and NO binary value_blob rows. Resolve must fall back to the front re-read.
    let scan = musefs_format::flac::locate_audio(&bytes).unwrap();
    let db = musefs_db::Db::open_in_memory().unwrap();
    let id = db
        .upsert_track(&NewTrack {
            backing_path: path.to_string_lossy().into_owned(),
            format: Format::Flac,
            audio_offset: scan.audio_offset as i64,
            audio_length: scan.audio_length as i64,
            backing_size: meta.len() as i64,
            backing_mtime: meta
                .modified()
                .unwrap()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs() as i64,
        })
        .unwrap();
    db.replace_tags(
        id,
        &[Tag::new("artist", "Alice", 0), Tag::new("title", "Song", 0)],
    )
    .unwrap();
    assert!(db.get_structural_blocks(id).unwrap().is_empty());

    let fs = Musefs::open(db, config()).unwrap();
    let artist = fs.lookup(VirtualTree::ROOT, "Alice").unwrap();
    let (_, inode, _) = fs.readdir(artist).unwrap().into_iter().next().unwrap();
    let served = read_whole(&fs, inode);

    // The legacy path carries every preserved block inline, including APPLICATION.
    let tag = metaflac::Tag::read_from(&mut std::io::Cursor::new(&served)).expect("valid FLAC");
    assert!(tag
        .blocks()
        .any(|b| matches!(b, metaflac::Block::Application(_))));
}
