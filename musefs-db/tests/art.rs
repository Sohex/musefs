mod common;
use common::{jpeg, new_track};
use musefs_db::{Db, TrackArt};

#[test]
fn identical_bytes_dedup_to_one_row() {
    let db = Db::open_in_memory().unwrap();
    let id1 = db.upsert_art(&jpeg(vec![1, 2, 3, 4])).unwrap();
    let id2 = db.upsert_art(&jpeg(vec![1, 2, 3, 4])).unwrap();
    let id3 = db.upsert_art(&jpeg(vec![9, 9, 9])).unwrap();

    assert_eq!(id1, id2, "identical bytes must dedup to the same id");
    assert_ne!(id1, id3, "different bytes must get a new id");
}

#[test]
fn get_art_returns_data_and_len() {
    let db = Db::open_in_memory().unwrap();
    let id = db.upsert_art(&jpeg(vec![1, 2, 3, 4])).unwrap();
    let art = db.get_art(id).unwrap().expect("art row");
    assert_eq!(art.data, vec![1, 2, 3, 4]);
    assert_eq!(art.byte_len, 4);
    assert_eq!(art.mime, "image/jpeg");
}

#[test]
fn set_and_get_track_art() {
    let db = Db::open_in_memory().unwrap();
    let track = db.upsert_track(&new_track("/m/a.flac")).unwrap();
    let art_id = db.upsert_art(&jpeg(vec![1, 2, 3])).unwrap();

    db.set_track_art(
        track,
        &[TrackArt {
            art_id,
            picture_type: 3,
            description: "front".to_string(),
            ordinal: 0,
        }],
    )
    .unwrap();

    let got = db.get_track_art(track).unwrap();
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].art_id, art_id);
    assert_eq!(got[0].picture_type, 3);
    assert_eq!(got[0].description, "front");
}

#[test]
fn linking_art_bumps_content_version() {
    let db = Db::open_in_memory().unwrap();
    let track = db.upsert_track(&new_track("/m/a.flac")).unwrap();
    let before = db.track_content_version(track).unwrap();
    let art_id = db.upsert_art(&jpeg(vec![1, 2, 3])).unwrap();

    db.set_track_art(
        track,
        &[TrackArt {
            art_id,
            picture_type: 3,
            description: String::new(),
            ordinal: 0,
        }],
    )
    .unwrap();

    assert!(db.track_content_version(track).unwrap() > before);
}
