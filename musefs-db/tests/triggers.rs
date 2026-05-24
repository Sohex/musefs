mod common;
use common::new_track;
use musefs_db::{Db, Tag};

#[test]
fn tag_changes_bump_content_version() {
    let db = Db::open_in_memory().unwrap();
    let track = db.upsert_track(&new_track("/m/a.flac")).unwrap();
    assert_eq!(db.track_content_version(track).unwrap(), 0);

    db.replace_tags(track, &[Tag::new("title", "First", 0)])
        .unwrap();
    let after_insert = db.track_content_version(track).unwrap();
    assert!(after_insert > 0, "insert should bump content_version");

    db.replace_tags(track, &[Tag::new("title", "Second", 0)])
        .unwrap();
    let after_replace = db.track_content_version(track).unwrap();
    assert!(
        after_replace > after_insert,
        "replacing tags should bump content_version again"
    );
}
