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

#[test]
fn scans_mp3_files_seeding_tracks_and_tags() {
    use id3::TagLike;
    use musefs_db::Format;

    let dir = tempfile::tempdir().unwrap();

    // Build an MP3: a real ID3v2.4 tag (via the id3 crate) + a fake audio frame.
    let mut tag = id3::Tag::new();
    tag.set_artist("Bob");
    tag.set_title("Track");
    let mut bytes = Vec::new();
    tag.write_to(&mut bytes, id3::Version::Id3v24).unwrap();
    let audio_len = 8u64;
    bytes.extend_from_slice(&[0xFF, 0xFB, 1, 2, 3, 4, 5, 6]);
    let audio_offset = bytes.len() as u64 - audio_len;
    std::fs::write(dir.path().join("song.mp3"), &bytes).unwrap();

    let db = Db::open_in_memory().unwrap();
    let stats = scan_directory(&db, dir.path()).unwrap();
    assert_eq!(stats.scanned, 1);

    let tracks = db.list_tracks().unwrap();
    assert_eq!(tracks.len(), 1);
    let t = &tracks[0];
    assert_eq!(t.format, Format::Mp3);
    assert_eq!(t.audio_offset as u64, audio_offset);
    assert_eq!(t.audio_length, audio_len as i64);

    let tags = db.get_tags(t.id).unwrap();
    assert!(tags
        .iter()
        .any(|tag| tag.key == "artist" && tag.value == "Bob"));
    assert!(tags
        .iter()
        .any(|tag| tag.key == "title" && tag.value == "Track"));
}
