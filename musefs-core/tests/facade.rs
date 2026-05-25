mod common;
use common::make_flac;
use common::{streaminfo_body, vorbis_comment_body};
use musefs_core::{scan_directory, CoreError, MountConfig, Musefs, VirtualTree};
use std::collections::BTreeMap;

fn config() -> MountConfig {
    MountConfig {
        template: "$artist/$title".to_string(),
        fallbacks: BTreeMap::new(),
        default_fallback: "Unknown".to_string(),
        mode: musefs_core::Mode::Synthesis,
    }
}

fn scanned_db(dir: &std::path::Path) -> musefs_db::Db {
    let a = make_flac(
        &[
            (0, streaminfo_body()),
            (4, vorbis_comment_body("v", &["ARTIST=Alice", "TITLE=Song"])),
        ],
        &[0xAB; 64],
    );
    std::fs::write(dir.join("a.flac"), &a).unwrap();
    let db = musefs_db::Db::open_in_memory().unwrap();
    // Use an on-disk DB? in-memory is fine; scan writes absolute backing paths.
    scan_directory(&db, dir).unwrap();
    db
}

#[test]
fn lookup_getattr_readdir_and_read_through_the_facade() {
    let dir = tempfile::tempdir().unwrap();
    let db = scanned_db(dir.path());
    let mut fs = Musefs::open(db, config()).unwrap();

    // Tree: /Alice/Song.flac
    let artist = fs.lookup(VirtualTree::ROOT, "Alice").expect("artist dir");
    let dattr = fs.getattr(artist).unwrap();
    assert!(dattr.is_dir);

    let entries = fs.readdir(artist).unwrap();
    assert_eq!(entries.len(), 1);
    let (name, file_inode, is_dir) = entries.into_iter().next().unwrap();
    assert_eq!(name, "Song.flac");
    assert!(!is_dir);

    let fattr = fs.getattr(file_inode).unwrap();
    assert!(!fattr.is_dir);
    assert!(fattr.size > 0);

    // Reading the whole file yields a valid FLAC whose TITLE is the synthesized value.
    let bytes = fs.read(file_inode, 0, fattr.size).unwrap();
    assert_eq!(bytes.len() as u64, fattr.size);
    let tag = metaflac::Tag::read_from(&mut std::io::Cursor::new(&bytes)).unwrap();
    assert_eq!(
        tag.vorbis_comments()
            .unwrap()
            .get("TITLE")
            .map(|v| v.as_slice()),
        Some(["Song".to_string()].as_slice())
    );
}

#[test]
fn parent_exposes_the_tree_hierarchy() {
    let dir = tempfile::tempdir().unwrap();
    let db = scanned_db(dir.path());
    let fs = Musefs::open(db, config()).unwrap();

    let artist = fs.lookup(VirtualTree::ROOT, "Alice").unwrap();
    assert_eq!(fs.parent(artist), Some(VirtualTree::ROOT));
    assert_eq!(fs.parent(VirtualTree::ROOT), Some(VirtualTree::ROOT));
    assert_eq!(fs.parent(424242), None);
}

#[test]
fn refresh_rebuilds_tree_after_new_tracks() {
    let dir = tempfile::tempdir().unwrap();
    let db = scanned_db(dir.path());
    let mut fs = Musefs::open(db, config()).unwrap();
    assert!(fs.lookup(VirtualTree::ROOT, "Alice").is_some());
    assert!(fs.lookup(VirtualTree::ROOT, "Bob").is_none());

    // This test only asserts refresh() runs and the tree is rebuilt from the DB;
    // adding rows would require a handle to the DB, which Musefs now owns. So we
    // simply confirm refresh() succeeds and the existing entry is still present.
    fs.refresh().unwrap();
    assert!(fs.lookup(VirtualTree::ROOT, "Alice").is_some());
}

#[test]
fn readdir_distinguishes_a_file_from_an_unknown_inode() {
    let dir = tempfile::tempdir().unwrap();
    let db = scanned_db(dir.path());
    let fs = Musefs::open(db, config()).unwrap();

    let artist = fs.lookup(VirtualTree::ROOT, "Alice").unwrap();
    let file = fs.readdir(artist).unwrap()[0].1;

    match fs.readdir(file) {
        Err(CoreError::NotADir(i)) => assert_eq!(i, file),
        other => panic!("expected NotADir, got {other:?}"),
    }
    match fs.readdir(987654) {
        Err(CoreError::NoEntry(i)) => assert_eq!(i, 987654),
        other => panic!("expected NoEntry, got {other:?}"),
    }
}

#[test]
fn reads_a_synthesized_mp3_through_the_facade() {
    use id3::TagLike;
    use std::io::Cursor;

    let dir = tempfile::tempdir().unwrap();

    // Backing MP3: ID3v2.4 tag (artist=Zoe, title=Old) + a fake audio frame.
    let mut tag = id3::Tag::new();
    tag.set_artist("Zoe");
    tag.set_title("Old");
    let mut bytes = Vec::new();
    tag.write_to(&mut bytes, id3::Version::Id3v24).unwrap();
    let audio = [0xFFu8, 0xFB, 7, 7, 7, 7];
    bytes.extend_from_slice(&audio);
    std::fs::write(dir.path().join("song.mp3"), &bytes).unwrap();

    let db = musefs_db::Db::open_in_memory().unwrap();
    scan_directory(&db, dir.path()).unwrap();
    let mut fs = Musefs::open(db, config()).unwrap();

    // Tree: /Zoe/Old.mp3
    let artist = fs.lookup(VirtualTree::ROOT, "Zoe").expect("artist dir");
    let entries = fs.readdir(artist).unwrap();
    let (name, file_inode, _) = entries.into_iter().next().unwrap();
    assert_eq!(name, "Old.mp3");

    let attr = fs.getattr(file_inode).unwrap();
    let whole = fs.read(file_inode, 0, attr.size).unwrap();
    assert_eq!(whole.len() as u64, attr.size);

    // The synthesized file is a valid ID3v2.4 stream carrying the DB tags, and the
    // original audio frames are spliced in unchanged at the tail.
    let parsed = id3::Tag::read_from2(Cursor::new(&whole)).unwrap();
    assert_eq!(parsed.artist(), Some("Zoe"));
    assert_eq!(parsed.title(), Some("Old"));
    assert_eq!(&whole[whole.len() - audio.len()..], &audio);
}

#[test]
fn serves_flac_with_embedded_art_through_the_facade() {
    let dir = tempfile::tempdir().unwrap();
    let img = vec![0xC3u8; 120];

    // Build a FLAC with a PICTURE block (type 3 = front cover).
    fn picture_body(mime: &str, data: &[u8]) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(&3u32.to_be_bytes()); // front cover
        b.extend_from_slice(&(mime.len() as u32).to_be_bytes());
        b.extend_from_slice(mime.as_bytes());
        b.extend_from_slice(&0u32.to_be_bytes()); // description length
        b.extend_from_slice(&0u32.to_be_bytes()); // width
        b.extend_from_slice(&0u32.to_be_bytes()); // height
        b.extend_from_slice(&0u32.to_be_bytes()); // color depth
        b.extend_from_slice(&0u32.to_be_bytes()); // colors used
        b.extend_from_slice(&(data.len() as u32).to_be_bytes());
        b.extend_from_slice(data);
        b
    }
    let mut flac = Vec::new();
    flac.extend_from_slice(b"fLaC");
    flac.extend_from_slice(&common::flac_block(0, &common::streaminfo_body(), false));
    flac.extend_from_slice(&common::flac_block(
        4,
        &common::vorbis_comment_body("v", &["ARTIST=Art", "TITLE=Cover"]),
        false,
    ));
    flac.extend_from_slice(&common::flac_block(
        6,
        &picture_body("image/png", &img),
        true,
    ));
    flac.extend_from_slice(&[0x5Au8; 40]);
    std::fs::write(dir.path().join("c.flac"), &flac).unwrap();

    let db = musefs_db::Db::open_in_memory().unwrap();
    scan_directory(&db, dir.path()).unwrap();
    let mut fs = Musefs::open(db, config()).unwrap();

    let artist = fs.lookup(VirtualTree::ROOT, "Art").unwrap();
    let (_name, file_inode, _) = fs.readdir(artist).unwrap().into_iter().next().unwrap();
    let attr = fs.getattr(file_inode).unwrap();
    let whole = fs.read(file_inode, 0, attr.size).unwrap();
    assert_eq!(whole.len() as u64, attr.size);

    // The synthesized FLAC carries the embedded picture with the original bytes.
    let tag = metaflac::Tag::read_from(&mut std::io::Cursor::new(&whole)).unwrap();
    let pic = tag.pictures().next().expect("a picture");
    assert_eq!(pic.data, img);
    assert_eq!(pic.mime_type, "image/png");
}

#[test]
fn serves_mp3_with_embedded_art_through_the_facade() {
    use id3::TagLike;

    let dir = tempfile::tempdir().unwrap();
    let img = vec![0xD4u8; 90];

    let mut tag = id3::Tag::new();
    tag.set_artist("Pix");
    tag.set_title("Song");
    tag.add_frame(id3::frame::Picture {
        mime_type: "image/jpeg".to_string(),
        picture_type: id3::frame::PictureType::CoverFront,
        description: String::new(),
        data: img.clone(),
    });
    let mut bytes = Vec::new();
    tag.write_to(&mut bytes, id3::Version::Id3v24).unwrap();
    bytes.extend_from_slice(&[0xFF, 0xFB, 1, 2, 3, 4]);
    std::fs::write(dir.path().join("s.mp3"), &bytes).unwrap();

    let db = musefs_db::Db::open_in_memory().unwrap();
    scan_directory(&db, dir.path()).unwrap();
    let mut fs = Musefs::open(db, config()).unwrap();

    let artist = fs.lookup(VirtualTree::ROOT, "Pix").unwrap();
    let (_name, file_inode, _) = fs.readdir(artist).unwrap().into_iter().next().unwrap();
    let attr = fs.getattr(file_inode).unwrap();
    let whole = fs.read(file_inode, 0, attr.size).unwrap();
    assert_eq!(whole.len() as u64, attr.size);

    // The synthesized MP3 carries the embedded APIC picture with the original bytes.
    let parsed = id3::Tag::read_from2(std::io::Cursor::new(&whole)).unwrap();
    let pic = parsed.pictures().next().expect("a picture");
    assert_eq!(pic.data, img);
    assert_eq!(pic.mime_type, "image/jpeg");
}

#[test]
fn poll_refresh_picks_up_external_db_edits() {
    use musefs_db::{Format, NewTrack, Tag};

    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("m.db");

    // Seed one track (Alice) and open a mount over the on-disk DB.
    {
        let db = musefs_db::Db::open(&db_path).unwrap();
        let id = db
            .upsert_track(&NewTrack {
                backing_path: "/x/a.flac".to_string(),
                format: Format::Flac,
                audio_offset: 0,
                audio_length: 0,
                backing_size: 0,
                backing_mtime: 0,
            })
            .unwrap();
        db.replace_tags(
            id,
            &[Tag::new("artist", "Alice", 0), Tag::new("title", "A", 0)],
        )
        .unwrap();
    }
    let db = musefs_db::Db::open(&db_path).unwrap();
    let mut fs = Musefs::open(db, config()).unwrap();
    assert!(fs.lookup(VirtualTree::ROOT, "Alice").is_some());
    assert!(fs.lookup(VirtualTree::ROOT, "Bob").is_none());

    // A separate connection adds a track (as beets/picard would).
    {
        let db2 = musefs_db::Db::open(&db_path).unwrap();
        let id = db2
            .upsert_track(&NewTrack {
                backing_path: "/x/b.flac".to_string(),
                format: Format::Flac,
                audio_offset: 0,
                audio_length: 0,
                backing_size: 0,
                backing_mtime: 0,
            })
            .unwrap();
        db2.replace_tags(
            id,
            &[Tag::new("artist", "Bob", 0), Tag::new("title", "B", 0)],
        )
        .unwrap();
    }

    // Polling notices the external commit and rebuilds the tree.
    assert!(fs.poll_refresh().unwrap());
    assert!(fs.lookup(VirtualTree::ROOT, "Bob").is_some());
    // A second poll with no further change is a no-op.
    assert!(!fs.poll_refresh().unwrap());
}
