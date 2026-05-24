mod common;
use common::{make_flac, streaminfo_body, vorbis_comment_body};
use musefs_core::scan_directory;
use musefs_db::Db;

#[test]
fn scans_flac_files_seeding_tracks_and_tags() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("sub")).unwrap();

    let a = make_flac(
        &[
            (0, streaminfo_body()),
            (4, vorbis_comment_body("v", &["TITLE=A", "ARTIST=X"])),
        ],
        &[0xAA; 30],
    );
    std::fs::write(dir.path().join("a.flac"), &a).unwrap();
    let b = make_flac(
        &[
            (0, streaminfo_body()),
            (4, vorbis_comment_body("v", &["TITLE=B"])),
        ],
        &[0xBB; 40],
    );
    std::fs::write(dir.path().join("sub/b.flac"), &b).unwrap();
    std::fs::write(dir.path().join("notes.txt"), b"hello").unwrap();

    let db = Db::open_in_memory().unwrap();
    let stats = scan_directory(&db, dir.path()).unwrap();
    assert_eq!(stats.scanned, 2);

    let tracks = db.list_tracks().unwrap();
    assert_eq!(tracks.len(), 2);

    let a_track = tracks
        .iter()
        .find(|t| t.backing_path.ends_with("a.flac"))
        .unwrap();
    let tags = db.get_tags(a_track.id).unwrap();
    assert!(tags.iter().any(|t| t.key == "title" && t.value == "A"));
    assert!(tags.iter().any(|t| t.key == "artist" && t.value == "X"));
    assert!(a_track.audio_length == 30);
}

#[test]
fn rescanning_is_idempotent() {
    let dir = tempfile::tempdir().unwrap();
    let a = make_flac(
        &[
            (0, streaminfo_body()),
            (4, vorbis_comment_body("v", &["TITLE=A"])),
        ],
        &[0xAA; 30],
    );
    std::fs::write(dir.path().join("a.flac"), &a).unwrap();

    let db = Db::open_in_memory().unwrap();
    scan_directory(&db, dir.path()).unwrap();
    scan_directory(&db, dir.path()).unwrap();
    assert_eq!(db.list_tracks().unwrap().len(), 1);
}
