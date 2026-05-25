mod common;
use common::{make_flac, streaminfo_body, vorbis_comment_body};
use musefs_core::{revalidate, scan_directory};
use musefs_db::{Db, Tag};

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

#[test]
fn revalidate_skips_unchanged_prunes_missing_and_gcs_art() {
    let dir = tempfile::tempdir().unwrap();
    let a = make_flac(
        &[
            (0, streaminfo_body()),
            (4, vorbis_comment_body("v", &["TITLE=A"])),
        ],
        &[0xAA; 30],
    );
    std::fs::write(dir.path().join("a.flac"), &a).unwrap();
    let gone = make_flac(
        &[
            (0, streaminfo_body()),
            (4, vorbis_comment_body("v", &["TITLE=G"])),
        ],
        &[0xBB; 30],
    );
    std::fs::write(dir.path().join("gone.flac"), &gone).unwrap();

    let db = Db::open_in_memory().unwrap();
    scan_directory(&db, dir.path()).unwrap();
    assert_eq!(db.list_tracks().unwrap().len(), 2);

    // An external edit to a's tags that a revalidate must NOT clobber (the file is
    // unchanged on disk, so revalidate should skip re-reading it).
    let a_id = db
        .list_tracks()
        .unwrap()
        .into_iter()
        .find(|t| t.backing_path.ends_with("a.flac"))
        .unwrap()
        .id;
    db.replace_tags(a_id, &[Tag::new("title", "Edited", 0)])
        .unwrap();

    // Delete gone.flac from disk so revalidate prunes its track.
    std::fs::remove_file(dir.path().join("gone.flac")).unwrap();

    let stats = revalidate(&db, dir.path()).unwrap();
    assert_eq!(stats.unchanged, 1); // a.flac (size+mtime match) is skipped
    assert_eq!(stats.pruned, 1); // gone.flac's track is removed

    let tracks = db.list_tracks().unwrap();
    assert_eq!(tracks.len(), 1);
    // The skipped file kept its externally-edited tag (not re-seeded from disk).
    let tags = db.get_tags(tracks[0].id).unwrap();
    assert!(tags.iter().any(|t| t.key == "title" && t.value == "Edited"));
}

fn flac_with_picture(comments: &[&str], img: &[u8]) -> Vec<u8> {
    use common::{flac_block, streaminfo_body, vorbis_comment_body};
    fn picture_body(pic_type: u32, mime: &str, data: &[u8]) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(&pic_type.to_be_bytes());
        b.extend_from_slice(&(mime.len() as u32).to_be_bytes());
        b.extend_from_slice(mime.as_bytes());
        b.extend_from_slice(&0u32.to_be_bytes()); // empty description
        b.extend_from_slice(&0u32.to_be_bytes()); // width
        b.extend_from_slice(&0u32.to_be_bytes()); // height
        b.extend_from_slice(&0u32.to_be_bytes()); // depth
        b.extend_from_slice(&0u32.to_be_bytes()); // colors
        b.extend_from_slice(&(data.len() as u32).to_be_bytes());
        b.extend_from_slice(data);
        b
    }
    let mut out = Vec::new();
    out.extend_from_slice(b"fLaC");
    out.extend_from_slice(&flac_block(0, &streaminfo_body(), false));
    out.extend_from_slice(&flac_block(4, &vorbis_comment_body("v", comments), false));
    out.extend_from_slice(&flac_block(6, &picture_body(3, "image/png", img), true));
    out.extend_from_slice(&[0xAAu8; 24]);
    out
}

#[test]
fn scan_ingests_and_dedups_embedded_art() {
    let dir = tempfile::tempdir().unwrap();
    let img = vec![0x42u8; 100];
    std::fs::write(
        dir.path().join("a.flac"),
        flac_with_picture(&["TITLE=A"], &img),
    )
    .unwrap();
    std::fs::create_dir(dir.path().join("sub")).unwrap();
    std::fs::write(
        dir.path().join("sub/b.flac"),
        flac_with_picture(&["TITLE=B"], &img),
    )
    .unwrap();

    let db = Db::open_in_memory().unwrap();
    scan_directory(&db, dir.path()).unwrap();

    let tracks = db.list_tracks().unwrap();
    assert_eq!(tracks.len(), 2);

    // Both tracks link art, and the identical image is stored once (dedup by sha256).
    let mut art_ids = std::collections::HashSet::new();
    for t in &tracks {
        let ta = db.get_track_art(t.id).unwrap();
        assert_eq!(ta.len(), 1);
        assert_eq!(ta[0].picture_type, 3);
        art_ids.insert(ta[0].art_id);
    }
    assert_eq!(art_ids.len(), 1, "identical art should dedup to one row");

    let only = *art_ids.iter().next().unwrap();
    assert_eq!(db.get_art_meta(only).unwrap().unwrap().byte_len, 100);
}

fn flac_with_pictures(comments: &[&str], pics: &[(u32, &[u8])]) -> Vec<u8> {
    use common::{flac_block, streaminfo_body, vorbis_comment_body};
    fn picture_body(pic_type: u32, mime: &str, data: &[u8]) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(&pic_type.to_be_bytes());
        b.extend_from_slice(&(mime.len() as u32).to_be_bytes());
        b.extend_from_slice(mime.as_bytes());
        b.extend_from_slice(&0u32.to_be_bytes()); // description
        b.extend_from_slice(&0u32.to_be_bytes()); // width
        b.extend_from_slice(&0u32.to_be_bytes()); // height
        b.extend_from_slice(&0u32.to_be_bytes()); // depth
        b.extend_from_slice(&0u32.to_be_bytes()); // colors
        b.extend_from_slice(&(data.len() as u32).to_be_bytes());
        b.extend_from_slice(data);
        b
    }
    let mut out = Vec::new();
    out.extend_from_slice(b"fLaC");
    out.extend_from_slice(&flac_block(0, &streaminfo_body(), false));
    out.extend_from_slice(&flac_block(
        4,
        &vorbis_comment_body("v", comments),
        pics.is_empty(),
    ));
    for (i, (pt, data)) in pics.iter().enumerate() {
        out.extend_from_slice(&flac_block(
            6,
            &picture_body(*pt, "image/png", data),
            i == pics.len() - 1,
        ));
    }
    out.extend_from_slice(&[0xAAu8; 24]);
    out
}

#[test]
fn scan_clamps_out_of_range_picture_type() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("a.flac"),
        flac_with_pictures(&["TITLE=A"], &[(99, &[0x11u8; 40])]),
    )
    .unwrap();

    let db = Db::open_in_memory().unwrap();
    scan_directory(&db, dir.path()).unwrap();

    let t = &db.list_tracks().unwrap()[0];
    let ta = db.get_track_art(t.id).unwrap();
    assert_eq!(ta.len(), 1);
    assert_eq!(ta[0].picture_type, 0); // 99 is out of range (0..=20) -> clamped to 0
    assert_eq!(ta[0].ordinal, 0);
}

#[test]
fn scan_filters_oversized_art_without_ordinal_gaps() {
    let dir = tempfile::tempdir().unwrap();
    // Over MAX_ART_BYTES (16 MiB - 1 KiB) but still within FLAC's 24-bit block limit.
    let big = vec![0u8; 16_776_500];
    let small = vec![0x22u8; 50];
    std::fs::write(
        dir.path().join("a.flac"),
        flac_with_pictures(&["TITLE=A"], &[(3, &big), (4, &small)]),
    )
    .unwrap();

    let db = Db::open_in_memory().unwrap();
    scan_directory(&db, dir.path()).unwrap();

    let t = &db.list_tracks().unwrap()[0];
    let ta = db.get_track_art(t.id).unwrap();
    // The oversized first picture is skipped; the survivor keeps a gapless ordinal 0.
    assert_eq!(ta.len(), 1);
    assert_eq!(ta[0].ordinal, 0);
    assert_eq!(ta[0].picture_type, 4);
    assert_eq!(db.get_art_meta(ta[0].art_id).unwrap().unwrap().byte_len, 50);
}
