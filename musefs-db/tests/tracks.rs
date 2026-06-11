mod common;
use common::new_track;
use musefs_db::{Db, Format, NewArt, NewTrack, Tag, TrackArt};

#[test]
fn insert_then_get_by_id_and_path() {
    let db = Db::open_in_memory().unwrap();
    let id = db.upsert_track(&new_track("/music/a.flac")).unwrap();

    let by_id = db.get_track(id).unwrap().expect("track by id");
    assert_eq!(by_id.id, id);
    assert_eq!(by_id.backing_path, "/music/a.flac");
    assert_eq!(by_id.format, Format::Flac);
    assert_eq!(by_id.bounds.audio_offset(), 100);
    assert_eq!(by_id.content_version, 0);

    let by_path = db
        .get_track_by_path("/music/a.flac")
        .unwrap()
        .expect("track by path");
    assert_eq!(by_path.id, id);
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
            backing_path: "/x/a.flac".to_string(),
            format: Format::Flac,
            audio_offset: 0,
            audio_length: 0,
            backing_size: 0,
            backing_mtime: 0,
        })
        .unwrap();
    db.replace_tags(id, &[Tag::new("artist", "A", 0)]).unwrap();
    let art_id = db
        .upsert_art(&NewArt {
            mime: "image/png".to_string(),
            width: None,
            height: None,
            data: vec![1, 2, 3],
        })
        .unwrap();
    db.set_track_art(
        id,
        &[TrackArt {
            art_id,
            picture_type: 3,
            description: String::new(),
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
        backing_path: "/m/a.flac".to_string(),
        format: Format::Mp3,
        audio_offset: 222,
        audio_length: 333,
        backing_size: 555,
        backing_mtime: 555,
    };
    let id2 = db.upsert_track(&changed).unwrap();
    assert_eq!(id, id2, "conflict update must keep the same id");

    let t = db.get_track(id).unwrap().expect("track");
    assert_eq!(t.format, Format::Mp3);
    assert_eq!(t.bounds.audio_offset(), 222);
    assert_eq!(t.bounds.audio_length(), 333);
    assert_eq!(t.backing_size, 555);
    assert_eq!(t.backing_mtime, 555);
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
