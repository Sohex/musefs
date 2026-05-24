mod common;
use common::new_track;
use musefs_db::{Db, Format, Tag};

#[test]
fn insert_then_get_by_id_and_path() {
    let db = Db::open_in_memory().unwrap();
    let id = db.upsert_track(&new_track("/music/a.flac")).unwrap();

    let by_id = db.get_track(id).unwrap().expect("track by id");
    assert_eq!(by_id.id, id);
    assert_eq!(by_id.backing_path, "/music/a.flac");
    assert_eq!(by_id.format, Format::Flac);
    assert_eq!(by_id.audio_offset, 100);
    assert_eq!(by_id.content_version, 0);

    let by_path = db.get_track_by_path("/music/a.flac").unwrap().expect("track by path");
    assert_eq!(by_path.id, id);
}

#[test]
fn upsert_updates_existing_row_keeping_same_id() {
    let db = Db::open_in_memory().unwrap();
    let id = db.upsert_track(&new_track("/music/a.flac")).unwrap();

    let mut changed = new_track("/music/a.flac");
    changed.audio_offset = 222;
    let id2 = db.upsert_track(&changed).unwrap();

    assert_eq!(id, id2);
    assert_eq!(db.get_track(id).unwrap().unwrap().audio_offset, 222);
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
fn rescan_does_not_reset_content_version() {
    let db = Db::open_in_memory().unwrap();
    let id = db.upsert_track(&new_track("/music/a.flac")).unwrap();
    db.replace_tags(id, &[Tag::new("title", "T", 0)]).unwrap();
    let cv_before = db.track_content_version(id).unwrap();
    assert!(cv_before > 0);

    // Re-scan the same path with updated offsets; this must NOT reset the
    // version counter, which tracks tag/art edits.
    let mut rescan = new_track("/music/a.flac");
    rescan.audio_offset = 999;
    db.upsert_track(&rescan).unwrap();

    assert_eq!(
        db.track_content_version(id).unwrap(),
        cv_before,
        "re-scan must not reset content_version"
    );
}
