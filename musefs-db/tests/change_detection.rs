mod common;
use common::new_track;
use musefs_db::{Db, Tag};

#[test]
fn data_version_changes_after_external_connection_writes() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("musefs.db");

    let db = Db::open(&path).unwrap();
    let track = db.upsert_track(&new_track("/m/a.flac")).unwrap();
    let v1 = db.data_version().unwrap();

    // A separate connection (simulating an external tagger) commits a change.
    {
        let other = Db::open(&path).unwrap();
        other
            .replace_tags(track, &[Tag::new("title", "X", 0)])
            .unwrap();
    }

    let v2 = db.data_version().unwrap();
    assert_ne!(
        v1, v2,
        "data_version must change after another connection writes"
    );
}
