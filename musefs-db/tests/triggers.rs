mod common;
use common::new_track;
use musefs_db::{Db, StructuralBlock, Tag};

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

#[test]
fn geometry_change_bumps_content_version_by_exactly_one() {
    let db = Db::open_in_memory().unwrap();
    let id = db.upsert_track(&new_track("/m/a.flac")).unwrap();
    assert_eq!(db.track_content_version(id).unwrap(), 0);

    let mut changed = new_track("/m/a.flac");
    changed.audio_offset = 222;
    changed.backing_size = 1300;
    db.upsert_track(&changed).unwrap();

    assert_eq!(
        db.track_content_version(id).unwrap(),
        1,
        "geometry change must bump content_version exactly once"
    );
}

#[test]
fn identical_rescan_does_not_bump_content_version() {
    let db = Db::open_in_memory().unwrap();
    let id = db.upsert_track(&new_track("/m/a.flac")).unwrap();
    db.upsert_track(&new_track("/m/a.flac")).unwrap();
    assert_eq!(db.track_content_version(id).unwrap(), 0);
}

#[test]
fn structural_block_change_bumps_content_version() {
    let db = Db::open_in_memory().unwrap();
    let id = db.upsert_track(&new_track("/m/a.flac")).unwrap();
    let before = db.track_content_version(id).unwrap();
    db.set_structural_blocks(
        id,
        &[StructuralBlock {
            kind: "STREAMINFO".to_string(),
            ordinal: 0,
            body: vec![1, 2, 3, 4],
        }],
    )
    .unwrap();
    assert!(
        db.track_content_version(id).unwrap() > before,
        "structural block write must bump content_version"
    );
}
