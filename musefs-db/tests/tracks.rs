use std::path::Path;
mod common;
use common::{jpeg, new_track};
use musefs_db::{Db, Format, NewArt, NewTrack, Tag, TrackArt};

#[test]
fn insert_then_get_by_id_and_path() {
    let db = Db::open_in_memory().unwrap();
    let id = db.upsert_track(&new_track("/music/a.flac")).unwrap();

    let by_id = db.get_track(id).unwrap().expect("track by id");
    assert_eq!(by_id.id, id);
    assert_eq!(by_id.backing_path, Path::new("/music/a.flac"));
    assert_eq!(by_id.format, Format::Flac);
    assert_eq!(by_id.bounds.audio_offset(), 100);
    assert_eq!(by_id.content_version, 0);

    let by_path = db
        .get_track_by_path(Path::new("/music/a.flac"))
        .unwrap()
        .expect("track by path");
    assert_eq!(by_path.id, id);
}

#[test]
fn track_identity_returns_content_version_and_backing_identity() {
    let db = Db::open_in_memory().unwrap();
    let id = db.upsert_track(&new_track("/music/a.flac")).unwrap();
    // Link art so content_version is bumped off its 0 default, pinning the
    // returned stamp to a real, non-default value.
    let art_id = db.upsert_art(&jpeg(vec![1, 2, 3])).unwrap();
    db.set_track_art(
        id,
        &[TrackArt {
            art_id,
            picture_type: 3,
            description: String::new(),
            mime: "image/png".into(),
            width: None,
            height: None,
            depth: 0,
            colors: 0,
            ordinal: 0,
        }],
    )
    .unwrap();

    let cv = db.track_content_version(id).unwrap();
    assert!(
        cv > 0,
        "linking art must bump content_version above the default"
    );
    let track = db.get_track(id).unwrap().expect("track by id");
    let identity = db.track_identity(id).unwrap().expect("identity by id");
    assert_eq!(identity.content_version, cv);
    assert_eq!(
        identity.backing_path,
        std::path::PathBuf::from("/music/a.flac")
    );
    assert_eq!(identity.backing_size, track.backing_size);
    assert_eq!(identity.backing_mtime_ns, track.backing_mtime_ns);
    assert_eq!(identity.backing_ctime_ns, track.backing_ctime_ns);
    assert_eq!(identity.backing_ino, track.backing_ino);
    assert!(db.track_identity(999_999).unwrap().is_none());
}

#[test]
fn upsert_updates_existing_row_keeping_same_id() {
    let db = Db::open_in_memory().unwrap();
    let id = db.upsert_track(&new_track("/music/a.flac")).unwrap();

    let mut changed = new_track("/music/a.flac");
    changed.audio_offset = 222;
    changed.backing_size = 1222;
    let id2 = db.upsert_track(&changed).unwrap();

    assert_eq!(id, id2);
    assert_eq!(
        db.get_track(id).unwrap().unwrap().bounds.audio_offset(),
        222
    );
}

#[test]
fn list_tracks_returns_all() {
    let db = Db::open_in_memory().unwrap();
    db.upsert_track(&new_track("/music/a.flac")).unwrap();
    db.upsert_track(&new_track("/music/b.flac")).unwrap();
    assert_eq!(db.list_tracks().unwrap().len(), 2);
}

#[test]
fn get_missing_track_returns_none() {
    let db = Db::open_in_memory().unwrap();
    assert!(db.get_track(999).unwrap().is_none());
}

#[test]
fn rescan_with_changed_geometry_bumps_content_version() {
    let db = Db::open_in_memory().unwrap();
    let id = db.upsert_track(&new_track("/music/a.flac")).unwrap();
    db.replace_tags(id, &[Tag::new("title", "T", 0)]).unwrap();
    let cv_before = db.track_content_version(id).unwrap();
    assert!(cv_before > 0);

    let mut rescan = new_track("/music/a.flac");
    rescan.audio_offset = 100;
    rescan.audio_length = 900;
    db.upsert_track(&rescan).unwrap();

    // Exactly +1 (not just ">") is deliberate: it asserts the `tracks_geometry_au`
    // WHEN guard halts the trigger's own nested content_version UPDATE, so the
    // recursion terminates after a single bump rather than running away.
    assert_eq!(
        db.track_content_version(id).unwrap(),
        cv_before + 1,
        "a geometry-changing rescan must bump content_version exactly once"
    );
}

#[test]
fn delete_track_cascades_tags_and_track_art() {
    let db = Db::open_in_memory().unwrap();
    let id = db
        .upsert_track(&NewTrack {
            backing_path: std::path::PathBuf::from("/x/a.flac"),
            format: Format::Flac,
            audio_offset: 0,
            audio_length: 0,
            backing_size: 0,
            backing_mtime_ns: 0,
            backing_ctime_ns: 0,
            backing_ino: None,
        })
        .unwrap();
    db.replace_tags(id, &[Tag::new("artist", "A", 0)]).unwrap();
    let art_id = db
        .upsert_art(&NewArt {
            data: vec![1, 2, 3],
        })
        .unwrap();
    db.set_track_art(
        id,
        &[TrackArt {
            art_id,
            picture_type: 3,
            description: String::new(),
            mime: "image/png".into(),
            width: None,
            height: None,
            depth: 0,
            colors: 0,
            ordinal: 0,
        }],
    )
    .unwrap();

    db.delete_track(id).unwrap();

    assert!(db.get_track(id).unwrap().is_none());
    assert!(db.get_tags(id).unwrap().is_empty());
    assert!(db.get_track_art(id).unwrap().is_empty());
    // The art row itself remains (GC is a separate step) until gc_orphan_art runs.
    assert!(db.get_art(art_id).unwrap().is_some());
}

#[test]
fn upsert_conflict_updates_all_mutable_columns() {
    let db = Db::open_in_memory().unwrap();
    let id = db.upsert_track(&new_track("/m/a.flac")).unwrap();

    // Same backing_path => ON CONFLICT update path; change every mutable column.
    let changed = NewTrack {
        backing_path: std::path::PathBuf::from("/m/a.flac"),
        format: Format::Mp3,
        audio_offset: 222,
        audio_length: 333,
        backing_size: 555,
        backing_mtime_ns: 555,
        backing_ctime_ns: 666,
        backing_ino: None,
    };
    let id2 = db.upsert_track(&changed).unwrap();
    assert_eq!(id, id2, "conflict update must keep the same id");

    let t = db.get_track(id).unwrap().expect("track");
    assert_eq!(t.format, Format::Mp3);
    assert_eq!(t.bounds.audio_offset(), 222);
    assert_eq!(t.bounds.audio_length(), 333);
    assert_eq!(t.backing_size, 555);
    assert_eq!(t.backing_mtime_ns, 555);
    assert_eq!(t.backing_ctime_ns, 666);
}

#[test]
fn changelog_since_returns_distinct_ids_and_seq_bounds() {
    let db = Db::open_in_memory().unwrap();
    let id1 = db.upsert_track(&new_track("/a.flac")).unwrap();
    let id2 = db.upsert_track(&new_track("/b.flac")).unwrap();
    db.replace_tags(id1, &[Tag::new("ARTIST", "X", 0)]).unwrap();
    db.replace_tags(id1, &[Tag::new("ARTIST", "Y", 0)]).unwrap();

    let log = db.changelog_since(0).unwrap();
    // Duplicates collapse: id1 appears once despite multiple changelog rows.
    assert_eq!(log.changed_ids, vec![id1, id2]);
    assert!(log.max_seq >= 2);
    assert_eq!(log.min_seq, 1);

    // A watermark past everything returns no ids but the same bounds.
    let later = db.changelog_since(log.max_seq).unwrap();
    assert!(later.changed_ids.is_empty());
    assert_eq!(later.max_seq, log.max_seq);
}

#[test]
fn changelog_since_empty_table_reports_zero_bounds() {
    let db = Db::open_in_memory().unwrap();
    let log = db.changelog_since(0).unwrap();
    assert!(log.changed_ids.is_empty());
    assert_eq!((log.min_seq, log.max_seq), (0, 0));
    assert!(!log.malformed);
}

/// A row whose `track_id` is not an integer, in a store written with its
/// constraints off (#760). The read must not fail on it: an error advances no
/// watermark, so the refresh would hit the same row on every poll. It is
/// skipped and reported instead, and only while it is past the watermark.
#[test]
fn changelog_since_skips_a_malformed_row_and_reports_it() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("s.db");
    let db = Db::open(&path).unwrap();
    let id = db.upsert_track(&new_track("/a.flac")).unwrap();

    let raw = rusqlite::Connection::open(&path).unwrap();
    raw.pragma_update(None, "ignore_check_constraints", true)
        .unwrap();
    for bad in ["'not an id'", "1.5", "X'01'"] {
        raw.execute(
            &format!("INSERT INTO track_changes (track_id) VALUES ({bad})"),
            [],
        )
        .unwrap();
    }

    let log = db.changelog_since(0).unwrap();
    assert_eq!(
        log.changed_ids,
        vec![id],
        "only the readable id is returned"
    );
    assert!(log.malformed, "the unreadable rows are reported");

    let later = db.changelog_since(log.max_seq).unwrap();
    assert!(
        !later.malformed,
        "a malformed row behind the watermark is none of the caller's business"
    );
}

#[test]
fn render_keys_for_returns_only_requested_existing_ids() {
    let db = Db::open_in_memory().unwrap();
    let id1 = db.upsert_track(&new_track("/a.flac")).unwrap();
    let _id2 = db.upsert_track(&new_track("/b.flac")).unwrap();
    let keys = db.render_keys_for(&[id1, 999_999]).unwrap();
    assert_eq!(keys.len(), 1);
    assert_eq!(keys[0].0, id1);
}

#[test]
fn delete_changelog_through_for_test_prunes_the_prefix() {
    let db = Db::open_in_memory().unwrap();
    let id = db.upsert_track(&new_track("/a.flac")).unwrap();
    db.replace_tags(id, &[Tag::new("ARTIST", "X", 0)]).unwrap();
    let log = db.changelog_since(0).unwrap();
    assert!(log.max_seq >= 2);

    db.delete_changelog_through_for_test(log.max_seq - 1)
        .unwrap();
    let after = db.changelog_since(0).unwrap();
    assert_eq!(
        (after.min_seq, after.max_seq),
        (log.max_seq, log.max_seq),
        "rows through max_seq - 1 must actually be deleted"
    );
}

/// An over-cap `backing_path` is refused by every reader from its length alone
/// (#758). The row is smuggled past the V4 `CHECK`, as a store written with its
/// constraints off would hold it; each reader has to reject it from
/// `length(backing_path)` before loading the value, or a crafted store picks the
/// size of the allocation.
#[test]
fn every_backing_path_reader_refuses_an_over_cap_path() {
    use musefs_db::DbError;
    use musefs_db::limits::MAX_BACKING_PATH_BYTES;
    use std::os::unix::ffi::OsStrExt;

    // Never formats the value: on failure that would print the 64 KiB path.
    fn refused<T>(reader: &str, got: musefs_db::Result<T>) {
        match got {
            Err(DbError::FieldTooLarge {
                table: "tracks",
                field: "backing_path",
                ..
            }) => {}
            Err(other) => panic!("{reader} refused with the wrong error: {other}"),
            Ok(_) => panic!("{reader} loaded the over-cap path"),
        }
    }

    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("s.db");
    let db = Db::open(&store).unwrap();
    let mut long = vec![b'/'];
    long.resize(usize::try_from(MAX_BACKING_PATH_BYTES).unwrap() + 1, b'a');
    let fingerprint = "a".repeat(64);

    let raw = rusqlite::Connection::open(&store).unwrap();
    raw.pragma_update(None, "ignore_check_constraints", true)
        .unwrap();
    raw.execute(
        "INSERT INTO tracks (backing_path, format, audio_offset, audio_length, \
         backing_size, backing_mtime_ns, updated_at, fingerprint) \
         VALUES (?1, 'flac', 0, 0, 0, 0, 0, ?2)",
        rusqlite::params![long, fingerprint],
    )
    .unwrap();
    let id = raw.last_insert_rowid();

    refused("get_track", db.get_track(id));
    refused(
        "get_track_by_path",
        db.get_track_by_path(Path::new(std::ffi::OsStr::from_bytes(&long))),
    );
    refused("list_tracks", db.list_tracks());
    refused(
        "tracks_by_fingerprint",
        db.tracks_by_fingerprint(&fingerprint),
    );
    refused("track_identity", db.track_identity(id));
    refused("list_backing_paths", db.list_backing_paths());
}

/// The cap is inclusive: a path exactly at it stores and reads back, and one
/// byte more is refused at the write.
#[test]
fn a_backing_path_at_the_cap_round_trips_and_one_byte_more_is_refused() {
    use musefs_db::limits::MAX_BACKING_PATH_BYTES;
    let cap = usize::try_from(MAX_BACKING_PATH_BYTES).unwrap();
    let db = Db::open_in_memory().unwrap();

    let at_cap = format!("/{}", "a".repeat(cap - 1));
    let id = db.upsert_track(&new_track(&at_cap)).unwrap();
    let track = db
        .get_track(id)
        .unwrap()
        .expect("the at-cap track reads back");
    assert_eq!(track.backing_path, Path::new(&at_cap));

    let over = format!("/{}", "b".repeat(cap));
    let err = db.upsert_track(&new_track(&over)).unwrap_err().to_string();
    assert!(err.contains("CHECK constraint failed"), "{err}");
}

#[test]
fn format_round_trips_through_db_string() {
    assert_eq!(Format::Flac.as_str(), "flac");
    assert_eq!(Format::Mp3.as_str(), "mp3");
    assert_eq!("flac".parse::<Format>(), Ok(Format::Flac));
    assert_eq!("mp3".parse::<Format>(), Ok(Format::Mp3));
    assert_eq!(
        "ogg".parse::<Format>(),
        Err(strum::ParseError::VariantNotFound)
    );
}
