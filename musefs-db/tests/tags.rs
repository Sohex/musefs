mod common;
use common::new_track;
use musefs_db::{BinaryTag, Db, Tag};

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

#[test]
fn tags_grouped_empty_db_is_empty_map() {
    let db = Db::open_in_memory().unwrap();
    assert!(db.tags_grouped().unwrap().is_empty());
}

#[test]
fn tags_grouped_preserves_key_ordinal_order_for_multivalue() {
    let db = Db::open_in_memory().unwrap();
    let t = db.upsert_track(&new_track("/m/a.flac")).unwrap();
    db.replace_tags(
        t,
        &[
            Tag::new("artist", "Second", 1),
            Tag::new("artist", "First", 0),
            Tag::new("genre", "Rock", 0),
        ],
    )
    .unwrap();

    let grouped = db.tags_grouped().unwrap();
    assert_eq!(
        grouped.get(&t),
        Some(&vec![
            Tag::new("artist", "First", 0),
            Tag::new("artist", "Second", 1),
            Tag::new("genre", "Rock", 0),
        ]),
        "multi-value group must be ordered by (key, ordinal), matching get_tags"
    );
}

#[test]
fn read_binary_tag_chunk_into_matches_vec_variant_and_errors_on_short_read() {
    let db = Db::open_in_memory().unwrap();
    let track = db.upsert_track(&new_track("/m/a.flac")).unwrap();
    db.set_binary_tags(
        track,
        &[BinaryTag {
            key: "APIC".into(),
            payload: (0u8..64).collect(),
            ordinal: 0,
        }],
    )
    .unwrap();
    let payload_id = db.get_binary_tags(track).unwrap()[0].rowid;

    let expected = db.read_binary_tag_chunk(payload_id, 3, 5).unwrap();
    let mut buf = vec![0u8; 5];
    db.read_binary_tag_chunk_into(payload_id, 3, &mut buf)
        .unwrap();
    assert_eq!(buf, expected);

    let mut over = vec![0u8; 128];
    assert!(db
        .read_binary_tag_chunk_into(payload_id, 3, &mut over)
        .is_err());
}
