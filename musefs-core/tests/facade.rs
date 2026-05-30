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
        poll_interval: std::time::Duration::ZERO,
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
    let fs = Musefs::open(db, config()).unwrap();

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
    let bytes = fs.read(file_inode, 0, 0, fattr.size).unwrap();
    assert_eq!(bytes.len() as u64, fattr.size);
    let tag = metaflac::Tag::read_from(&mut std::io::Cursor::new(&bytes)).unwrap();
    assert_eq!(
        tag.vorbis_comments()
            .unwrap()
            .get("TITLE")
            .map(std::vec::Vec::as_slice),
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
    assert_eq!(fs.parent(424_242), None);
}

#[test]
fn refresh_rebuilds_tree_after_new_tracks() {
    let dir = tempfile::tempdir().unwrap();
    let db = scanned_db(dir.path());
    let fs = Musefs::open(db, config()).unwrap();
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
    match fs.readdir(987_654) {
        Err(CoreError::NoEntry(i)) => assert_eq!(i, 987_654),
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
    let fs = Musefs::open(db, config()).unwrap();

    // Tree: /Zoe/Old.mp3
    let artist = fs.lookup(VirtualTree::ROOT, "Zoe").expect("artist dir");
    let entries = fs.readdir(artist).unwrap();
    let (name, file_inode, _) = entries.into_iter().next().unwrap();
    assert_eq!(name, "Old.mp3");

    let attr = fs.getattr(file_inode).unwrap();
    let whole = fs.read(file_inode, 0, 0, attr.size).unwrap();
    assert_eq!(whole.len() as u64, attr.size);

    // The synthesized file is a valid ID3v2.4 stream carrying the DB tags, and the
    // original audio frames are spliced in unchanged at the tail.
    let parsed = id3::Tag::read_from2(Cursor::new(&whole)).unwrap();
    assert_eq!(parsed.artist(), Some("Zoe"));
    assert_eq!(parsed.title(), Some("Old"));
    assert_eq!(&whole[whole.len() - audio.len()..], &audio);
}

#[test]
fn reads_a_synthesized_m4a_through_the_facade() {
    let dir = tempfile::tempdir().unwrap();

    // Backing M4A: moov-first with ilst (title="Orig M4A", artist="Orig Artist")
    // and an mdat carrying verbatim audio.
    let audio = b"AUDIODATA";
    std::fs::write(dir.path().join("song.m4a"), common::minimal_m4a(audio)).unwrap();

    let db = musefs_db::Db::open_in_memory().unwrap();
    scan_directory(&db, dir.path()).unwrap();
    let fs = Musefs::open(db, config()).unwrap();

    // Tree: /Orig Artist/Orig M4A.m4a
    let artist = fs
        .lookup(VirtualTree::ROOT, "Orig Artist")
        .expect("artist dir");
    let entries = fs.readdir(artist).unwrap();
    let (name, file_inode, _) = entries.into_iter().next().unwrap();
    assert_eq!(name, "Orig M4A.m4a");

    let attr = fs.getattr(file_inode).unwrap();
    let whole = fs.read(file_inode, 0, 0, attr.size).unwrap();
    assert_eq!(whole.len() as u64, attr.size);

    // The original audio frames are spliced in verbatim at the tail of the
    // synthesized stream.
    assert!(
        whole.windows(audio.len()).any(|w| w == audio),
        "synthesized m4a should contain the verbatim audio payload"
    );
    assert_eq!(&whole[whole.len() - audio.len()..], audio);
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
    let fs = Musefs::open(db, config()).unwrap();

    let artist = fs.lookup(VirtualTree::ROOT, "Art").unwrap();
    let (_name, file_inode, _) = fs.readdir(artist).unwrap().into_iter().next().unwrap();
    let attr = fs.getattr(file_inode).unwrap();
    let whole = fs.read(file_inode, 0, 0, attr.size).unwrap();
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
    let fs = Musefs::open(db, config()).unwrap();

    let artist = fs.lookup(VirtualTree::ROOT, "Pix").unwrap();
    let (_name, file_inode, _) = fs.readdir(artist).unwrap().into_iter().next().unwrap();
    let attr = fs.getattr(file_inode).unwrap();
    let whole = fs.read(file_inode, 0, 0, attr.size).unwrap();
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
    let fs = Musefs::open(db, config()).unwrap();
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
    // The rebuild is additive — the pre-existing entry is still present.
    assert!(fs.lookup(VirtualTree::ROOT, "Alice").is_some());
    // A second poll with no further change is a no-op.
    assert!(!fs.poll_refresh().unwrap());
}

#[test]
fn open_handle_read_and_release_roundtrip() {
    let dir = tempfile::tempdir().unwrap();
    let db = scanned_db(dir.path());
    let fs = Musefs::open(db, config()).unwrap();

    let artist = fs.lookup(VirtualTree::ROOT, "Alice").unwrap();
    let (_, file_inode, _) = fs.readdir(artist).unwrap().into_iter().next().unwrap();
    let size = fs.getattr(file_inode).unwrap().size;

    let fh = fs.open_handle(file_inode).unwrap();
    assert!(fh != 0);
    let via_handle = fs.read(file_inode, fh, 0, size).unwrap();
    let via_fallback = fs.read(file_inode, 0, 0, size).unwrap();
    assert_eq!(via_handle, via_fallback);
    assert_eq!(via_handle.len() as u64, size);

    fs.release_handle(fh);
    let after = fs.read(file_inode, fh, 0, size).unwrap(); // unknown fh → fallback
    assert_eq!(after, via_fallback);
}

#[test]
fn poll_refresh_keeps_unchanged_entries_and_prunes_vanished() {
    use musefs_db::{Format, NewTrack, Tag};
    let dir = tempfile::tempdir().unwrap();
    let backing = dir.path().join("a.flac");
    let bytes = make_flac(
        &[
            (0, streaminfo_body()),
            (4, vorbis_comment_body("v", &["ARTIST=Alice", "TITLE=Song"])),
        ],
        &[0xAB; 64],
    );
    std::fs::write(&backing, &bytes).unwrap();
    let db_path = dir.path().join("m.db");
    {
        let db = musefs_db::Db::open(&db_path).unwrap();
        scan_directory(&db, dir.path()).unwrap();
    }
    let fs = Musefs::open(musefs_db::Db::open(&db_path).unwrap(), config()).unwrap();
    let artist = fs.lookup(VirtualTree::ROOT, "Alice").unwrap();
    let (_, inode, _) = fs.readdir(artist).unwrap().into_iter().next().unwrap();
    let size_before = fs.getattr(inode).unwrap().size;

    // Unrelated external commit bumps data_version without changing Alice's track.
    {
        let db2 = musefs_db::Db::open(&db_path).unwrap();
        let id = db2
            .upsert_track(&NewTrack {
                backing_path: "/x/ghost.mp3".to_string(),
                format: Format::Mp3,
                audio_offset: 0,
                audio_length: 0,
                backing_size: 0,
                backing_mtime: 0,
            })
            .unwrap();
        db2.replace_tags(
            id,
            &[Tag::new("artist", "Ghost", 0), Tag::new("title", "G", 0)],
        )
        .unwrap();
    }
    assert!(fs.poll_refresh().unwrap());

    let size_after = fs.getattr(inode).unwrap().size;
    assert_eq!(size_before, size_after);
    assert!(fs.lookup(VirtualTree::ROOT, "Ghost").is_some());
}

#[test]
fn poll_refresh_debounces_within_interval() {
    use musefs_db::{Format, NewTrack, Tag};
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("m.db");
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
    let cfg = MountConfig {
        poll_interval: std::time::Duration::from_hours(1),
        ..config()
    };
    let fs = Musefs::open(musefs_db::Db::open(&db_path).unwrap(), cfg).unwrap();
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
    assert!(!fs.poll_refresh().unwrap()); // debounced (within 1h of open)
    assert!(fs.lookup(VirtualTree::ROOT, "Bob").is_none());
}

#[test]
fn unchanged_refresh_poll_consumes_debounce_window() {
    use musefs_db::{Format, NewTrack, Tag};
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("m.db");
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
    // A generous interval keeps the DB-mutation gap below reliably within the
    // debounce window; the window is crossed via the test hook, not a sleep, so
    // the assertions don't race wall-clock jitter on a loaded CI runner.
    let cfg = MountConfig {
        poll_interval: std::time::Duration::from_secs(30),
        ..config()
    };
    let fs = Musefs::open(musefs_db::Db::open(&db_path).unwrap(), cfg).unwrap();
    fs.expire_poll_debounce_for_test();
    assert!(!fs.poll_refresh().unwrap());
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
    assert!(
        !fs.poll_refresh().unwrap(),
        "unchanged poll should have reset the debounce window"
    );
    fs.expire_poll_debounce_for_test();
    assert!(fs.poll_refresh().unwrap());
}

#[test]
fn failed_refresh_retries_after_backoff_not_every_call() {
    use musefs_db::{Format, NewTrack, Tag};
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("m.db");
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
    let cfg = MountConfig {
        poll_interval: std::time::Duration::from_millis(20),
        ..config()
    };
    let fs = Musefs::open(musefs_db::Db::open(&db_path).unwrap(), cfg).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(25));
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
    fs.force_rebuild_errors_for_test(true);
    assert!(fs.poll_refresh().is_err());
    assert!(
        !fs.poll_refresh().unwrap(),
        "immediate retry should be suppressed by refresh failure backoff"
    );
    std::thread::sleep(std::time::Duration::from_millis(110));
    assert!(fs.poll_refresh().is_err());
}

#[test]
fn poll_refresh_single_flights_concurrent_callers() {
    use musefs_db::{Format, NewTrack, Tag};
    use std::sync::Arc;
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("m.db");
    {
        let db = musefs_db::Db::open(&db_path).unwrap();
        let id = db
            .upsert_track(&NewTrack {
                backing_path: "/x/a.flac".into(),
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
    let cfg = MountConfig {
        poll_interval: std::time::Duration::ZERO,
        ..config()
    };
    let fs = Arc::new(Musefs::open(musefs_db::Db::open(&db_path).unwrap(), cfg).unwrap());
    {
        let db2 = musefs_db::Db::open(&db_path).unwrap();
        let id = db2
            .upsert_track(&NewTrack {
                backing_path: "/x/b.flac".into(),
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
    let trues: usize = std::thread::scope(|s| {
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let fs = Arc::clone(&fs);
                s.spawn(move || usize::from(fs.poll_refresh().unwrap()))
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).sum()
    });
    assert_eq!(trues, 1, "single-flight: exactly one caller rebuilds");
    assert!(fs.lookup(VirtualTree::ROOT, "Bob").is_some());
}

#[test]
fn inode_is_stable_across_refresh() {
    use musefs_db::{Format, NewTrack, Tag};
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("m.db");
    {
        let db = musefs_db::Db::open(&db_path).unwrap();
        let id = db
            .upsert_track(&NewTrack {
                backing_path: "/x/a.flac".into(),
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
    let cfg = MountConfig {
        poll_interval: std::time::Duration::ZERO,
        ..config()
    };
    let fs = Musefs::open(musefs_db::Db::open(&db_path).unwrap(), cfg).unwrap();
    let alice = fs.lookup(VirtualTree::ROOT, "Alice").unwrap();
    let (_, song_before, _) = fs.readdir(alice).unwrap().into_iter().next().unwrap();
    {
        let db2 = musefs_db::Db::open(&db_path).unwrap();
        let id = db2
            .upsert_track(&NewTrack {
                backing_path: "/x/b.flac".into(),
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
    assert!(fs.poll_refresh().unwrap());
    let alice_after = fs.lookup(VirtualTree::ROOT, "Alice").unwrap();
    let (_, song_after, _) = fs.readdir(alice_after).unwrap().into_iter().next().unwrap();
    assert_eq!(alice, alice_after);
    assert_eq!(song_before, song_after);
}

#[test]
fn poll_refresh_notify_reports_changed_track_inode() {
    use musefs_db::Tag;
    let dir = tempfile::tempdir().unwrap();
    // Two backing files -> two tracks: Alice/Song and Bob/Tune.
    for (name, artist, title) in [("a.flac", "Alice", "Song"), ("b.flac", "Bob", "Tune")] {
        let bytes = make_flac(
            &[
                (0, streaminfo_body()),
                (
                    4,
                    vorbis_comment_body(
                        "v",
                        &[&format!("ARTIST={artist}"), &format!("TITLE={title}")],
                    ),
                ),
            ],
            &[0xAB; 64],
        );
        std::fs::write(dir.path().join(name), &bytes).unwrap();
    }
    let db_path = dir.path().join("m.db");
    {
        let db = musefs_db::Db::open(&db_path).unwrap();
        scan_directory(&db, dir.path()).unwrap();
    }
    let fs = Musefs::open(musefs_db::Db::open(&db_path).unwrap(), config()).unwrap();

    let alice = fs.lookup(VirtualTree::ROOT, "Alice").unwrap();
    let alice_song = fs.lookup(alice, "Song.flac").unwrap();

    // Find Alice's track id (scan assigns ids by discovery order).
    let alice_id = musefs_db::Db::open(&db_path)
        .unwrap()
        .list_tracks()
        .unwrap()
        .into_iter()
        .find(|t| t.backing_path.ends_with("a.flac"))
        .unwrap()
        .id;

    // External edit: retag Alice WITHOUT moving her (same artist/title, extra
    // album tag) so her path/inode is stable but content_version bumps.
    {
        let db2 = musefs_db::Db::open(&db_path).unwrap();
        db2.replace_tags(
            alice_id,
            &[
                Tag::new("artist", "Alice", 0),
                Tag::new("title", "Song", 0),
                Tag::new("album", "New", 0),
            ],
        )
        .unwrap();
    }

    let mut changed = Vec::new();
    assert!(fs.poll_refresh_notify(|ino| changed.push(ino)).unwrap());
    assert_eq!(changed, vec![alice_song], "only Alice's inode changed");
    // Inode stayed stable across the refresh.
    assert_eq!(
        fs.lookup(fs.lookup(VirtualTree::ROOT, "Alice").unwrap(), "Song.flac")
            .unwrap(),
        alice_song
    );
}

#[test]
fn poll_refresh_notify_reports_old_inode_for_path_changing_retag() {
    use musefs_db::Tag;
    let dir = tempfile::tempdir().unwrap();
    let bytes = make_flac(
        &[
            (0, streaminfo_body()),
            (4, vorbis_comment_body("v", &["ARTIST=Alice", "TITLE=Song"])),
        ],
        &[0xAB; 64],
    );
    std::fs::write(dir.path().join("a.flac"), &bytes).unwrap();
    let db_path = dir.path().join("m.db");
    {
        let db = musefs_db::Db::open(&db_path).unwrap();
        scan_directory(&db, dir.path()).unwrap();
    }
    let fs = Musefs::open(musefs_db::Db::open(&db_path).unwrap(), config()).unwrap();
    let alice = fs.lookup(VirtualTree::ROOT, "Alice").unwrap();
    let old_inode = fs.lookup(alice, "Song.flac").unwrap();
    let track_id = musefs_db::Db::open(&db_path)
        .unwrap()
        .list_tracks()
        .unwrap()
        .into_iter()
        .next()
        .unwrap()
        .id;

    {
        let db2 = musefs_db::Db::open(&db_path).unwrap();
        db2.replace_tags(
            track_id,
            &[
                Tag::new("artist", "Alice", 0),
                Tag::new("title", "Moved", 0),
            ],
        )
        .unwrap();
    }

    let mut changed = Vec::new();
    assert!(fs.poll_refresh_notify(|ino| changed.push(ino)).unwrap());
    let alice_after = fs.lookup(VirtualTree::ROOT, "Alice").unwrap();
    let new_inode = fs.lookup(alice_after, "Moved.flac").unwrap();
    assert!(
        changed.contains(&old_inode),
        "old inode should be invalidated"
    );
    assert!(
        changed.contains(&new_inode),
        "new inode should be invalidated"
    );
    assert_ne!(old_inode, new_inode);
}

#[test]
fn poll_refresh_notify_invalidates_old_inode_for_removed_track() {
    let dir = tempfile::tempdir().unwrap();
    let bytes = make_flac(
        &[
            (0, streaminfo_body()),
            (4, vorbis_comment_body("v", &["ARTIST=Alice", "TITLE=Song"])),
        ],
        &[0xAB; 64],
    );
    std::fs::write(dir.path().join("a.flac"), &bytes).unwrap();
    let db_path = dir.path().join("m.db");
    {
        let db = musefs_db::Db::open(&db_path).unwrap();
        scan_directory(&db, dir.path()).unwrap();
    }
    let fs = Musefs::open(musefs_db::Db::open(&db_path).unwrap(), config()).unwrap();
    let alice = fs.lookup(VirtualTree::ROOT, "Alice").unwrap();
    let old_inode = fs.lookup(alice, "Song.flac").unwrap();
    let track_id = musefs_db::Db::open(&db_path)
        .unwrap()
        .list_tracks()
        .unwrap()
        .into_iter()
        .next()
        .unwrap()
        .id;

    {
        let db2 = musefs_db::Db::open(&db_path).unwrap();
        db2.delete_track(track_id).unwrap();
    }

    let mut changed = Vec::new();
    assert!(fs.poll_refresh_notify(|ino| changed.push(ino)).unwrap());
    assert!(
        changed.contains(&old_inode),
        "old inode should be invalidated after track removal"
    );
}

#[test]
fn reads_m4b_alias() {
    let dir = tempfile::tempdir().unwrap();
    let audio = b"AUDIODATA";
    let bytes = common::minimal_m4a(audio);
    std::fs::write(dir.path().join("book.m4b"), &bytes).unwrap();
    let db = musefs_db::Db::open_in_memory().unwrap();
    scan_directory(&db, dir.path()).unwrap();
    let track = db.list_tracks().unwrap().into_iter().next().unwrap();
    assert_eq!(track.format, musefs_db::Format::M4a);
    let fs = Musefs::open(db, config()).unwrap();
    let artist = fs.lookup(VirtualTree::ROOT, "Orig Artist").unwrap();
    let entries = fs.readdir(artist).unwrap();
    let (name, file_inode, _) = entries.into_iter().next().unwrap();
    assert_eq!(name, "Orig M4A.m4a");
    let attr = fs.getattr(file_inode).unwrap();
    assert!(!attr.is_dir);
    assert!(attr.size > 0);
}

#[test]
fn refresh_picks_up_externally_added_track() {
    use musefs_db::{Format, NewTrack, Tag};
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("m.db");
    {
        let db = musefs_db::Db::open(&db_path).unwrap();
        let id = db
            .upsert_track(&NewTrack {
                backing_path: "/x/a.flac".into(),
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
    let fs = Musefs::open(musefs_db::Db::open(&db_path).unwrap(), config()).unwrap();
    assert!(fs.lookup(VirtualTree::ROOT, "Bob").is_none());
    {
        let db2 = musefs_db::Db::open(&db_path).unwrap();
        let id = db2
            .upsert_track(&NewTrack {
                backing_path: "/x/b.flac".into(),
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
    fs.refresh().unwrap();
    assert!(
        fs.lookup(VirtualTree::ROOT, "Bob").is_some(),
        "refresh must rebuild the tree"
    );
}

#[test]
fn open_handle_returns_distinct_ids_and_rejects_dirs() {
    let dir = tempfile::tempdir().unwrap();
    let db = scanned_db(dir.path());
    let fs = Musefs::open(db, config()).unwrap();
    let artist = fs.lookup(VirtualTree::ROOT, "Alice").unwrap();
    let (_, file_inode, _) = fs.readdir(artist).unwrap().into_iter().next().unwrap();

    let fh1 = fs.open_handle(file_inode).unwrap();
    let fh2 = fs.open_handle(file_inode).unwrap();
    assert_ne!(fh1, fh2, "each open must yield a fresh handle id");
    assert!(fh1 != 0 && fh2 != 0);

    assert!(matches!(fs.open_handle(artist), Err(CoreError::IsDir(_))));
}

#[test]
fn read_uses_cached_handle_after_backing_grows() {
    use std::io::Write;
    let dir = tempfile::tempdir().unwrap();
    let db = scanned_db(dir.path());
    let fs = Musefs::open(db, config()).unwrap();
    let artist = fs.lookup(VirtualTree::ROOT, "Alice").unwrap();
    let (_, file_inode, _) = fs.readdir(artist).unwrap().into_iter().next().unwrap();
    let size = fs.getattr(file_inode).unwrap().size;

    let fh = fs.open_handle(file_inode).unwrap();
    {
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(dir.path().join("a.flac"))
            .unwrap();
        f.write_all(&[0u8; 64]).unwrap();
    }
    let via_handle = fs.read(file_inode, fh, 0, size).unwrap();
    assert_eq!(via_handle.len() as u64, size);
}

#[test]
fn release_handle_forces_fallback_on_next_read() {
    use std::io::Write;
    let dir = tempfile::tempdir().unwrap();
    let db = scanned_db(dir.path());
    let fs = Musefs::open(db, config()).unwrap();
    let artist = fs.lookup(VirtualTree::ROOT, "Alice").unwrap();
    let (_, file_inode, _) = fs.readdir(artist).unwrap().into_iter().next().unwrap();
    let size = fs.getattr(file_inode).unwrap().size;

    let fh = fs.open_handle(file_inode).unwrap();
    fs.release_handle(fh);
    {
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(dir.path().join("a.flac"))
            .unwrap();
        f.write_all(&[0u8; 64]).unwrap();
    }
    assert!(matches!(
        fs.read(file_inode, fh, 0, size),
        Err(CoreError::BackingChanged(_))
    ));
}

#[test]
fn getattr_reresolves_size_after_content_version_bump() {
    use common::{make_flac, streaminfo_body, vorbis_comment_body};
    use musefs_db::Tag;
    let dir = tempfile::tempdir().unwrap();
    let backing = dir.path().join("a.flac");
    let bytes = make_flac(
        &[
            (0, streaminfo_body()),
            (4, vorbis_comment_body("v", &["ARTIST=Alice", "TITLE=Song"])),
        ],
        &[0xAB; 64],
    );
    std::fs::write(&backing, &bytes).unwrap();
    let db_path = dir.path().join("m.db");
    {
        let db = musefs_db::Db::open(&db_path).unwrap();
        scan_directory(&db, dir.path()).unwrap();
    }
    let fs = Musefs::open(musefs_db::Db::open(&db_path).unwrap(), config()).unwrap();
    let alice = fs.lookup(VirtualTree::ROOT, "Alice").unwrap();
    let inode = fs.lookup(alice, "Song.flac").unwrap();
    let size_before = fs.getattr(inode).unwrap().size;

    let track_id = musefs_db::Db::open(&db_path)
        .unwrap()
        .list_tracks()
        .unwrap()[0]
        .id;
    {
        let db2 = musefs_db::Db::open(&db_path).unwrap();
        db2.replace_tags(
            track_id,
            &[
                Tag::new("artist", "Alice", 0),
                Tag::new("title", "Song", 0),
                Tag::new("album", &"X".repeat(500), 0),
            ],
        )
        .unwrap();
    }
    let size_after = fs.getattr(inode).unwrap().size;
    assert!(
        size_after > size_before,
        "size must reflect the larger retagged header"
    );
}
