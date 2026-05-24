mod common;
use common::new_track;
use musefs_db::{Db, Tag};

#[test]
fn replace_then_get_returns_tags_ordered() {
    let db = Db::open_in_memory().unwrap();
    let track = db.upsert_track(&new_track("/m/a.flac")).unwrap();

    db.replace_tags(
        track,
        &[
            Tag::new("artist", "B-side", 1),
            Tag::new("artist", "A-side", 0),
            Tag::new("title", "Song", 0),
        ],
    )
    .unwrap();

    let got = db.get_tags(track).unwrap();
    // Ordered by (key, ordinal): artist#0, artist#1, title#0
    assert_eq!(
        got,
        vec![
            Tag::new("artist", "A-side", 0),
            Tag::new("artist", "B-side", 1),
            Tag::new("title", "Song", 0),
        ]
    );
}

#[test]
fn replace_overwrites_previous_tags() {
    let db = Db::open_in_memory().unwrap();
    let track = db.upsert_track(&new_track("/m/a.flac")).unwrap();

    db.replace_tags(track, &[Tag::new("title", "Old", 0)])
        .unwrap();
    db.replace_tags(track, &[Tag::new("title", "New", 0)])
        .unwrap();

    assert_eq!(
        db.get_tags(track).unwrap(),
        vec![Tag::new("title", "New", 0)]
    );
}
