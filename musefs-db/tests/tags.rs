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

#[test]
fn tags_grouped_returns_all_tags_by_track() {
    let db = Db::open_in_memory().unwrap();
    let a = db.upsert_track(&new_track("/a.flac")).unwrap();
    let b = db.upsert_track(&new_track("/b.flac")).unwrap();
    db.replace_tags(
        a,
        &[Tag::new("artist", "Alice", 0), Tag::new("title", "A", 0)],
    )
    .unwrap();
    db.replace_tags(b, &[Tag::new("artist", "Bob", 0)]).unwrap();

    let grouped = db.tags_grouped().unwrap();
    assert_eq!(grouped.get(&a).map(std::vec::Vec::len), Some(2));
    assert_eq!(grouped.get(&b).map(std::vec::Vec::len), Some(1));
    // grouping must match per-track get_tags exactly (same order).
    assert_eq!(grouped.get(&a), Some(&db.get_tags(a).unwrap()));
    assert_eq!(grouped.get(&b), Some(&db.get_tags(b).unwrap()));
}
