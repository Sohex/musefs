mod common;
use common::{jpeg, new_track};
use musefs_db::{Db, NewArt, TrackArt};

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

#[test]
fn read_art_chunk_streams_a_slice() {
    let db = Db::open_in_memory().unwrap();
    let id = db
        .upsert_art(&NewArt {
            mime: "image/png".to_string(),
            width: Some(2),
            height: Some(3),
            data: vec![10, 11, 12, 13, 14, 15],
        })
        .unwrap();

    // A middle slice.
    assert_eq!(db.read_art_chunk(id, 2, 3).unwrap(), vec![12, 13, 14]);
    // From the start.
    assert_eq!(db.read_art_chunk(id, 0, 2).unwrap(), vec![10, 11]);

    // Metadata without loading the blob.
    let meta = db.get_art_meta(id).unwrap().unwrap();
    assert_eq!(meta.mime, "image/png");
    assert_eq!(meta.width, Some(2));
    assert_eq!(meta.byte_len, 6);

    assert!(db.get_art_meta(999_999).unwrap().is_none());
}

#[test]
fn gc_orphan_art_removes_unreferenced_rows() {
    let db = Db::open_in_memory().unwrap();
    let track = db
        .upsert_track(&musefs_db::NewTrack {
            backing_path: "/x/a.flac".to_string(),
            format: musefs_db::Format::Flac,
            audio_offset: 0,
            audio_length: 0,
            backing_size: 0,
            backing_mtime: 0,
        })
        .unwrap();
    let referenced = db
        .upsert_art(&NewArt {
            mime: "image/png".to_string(),
            width: None,
            height: None,
            data: vec![1, 2, 3],
        })
        .unwrap();
    let orphan = db
        .upsert_art(&NewArt {
            mime: "image/png".to_string(),
            width: None,
            height: None,
            data: vec![9, 9, 9],
        })
        .unwrap();
    db.set_track_art(
        track,
        &[musefs_db::TrackArt {
            art_id: referenced,
            picture_type: 3,
            description: String::new(),
            ordinal: 0,
        }],
    )
    .unwrap();

    let removed = db.gc_orphan_art().unwrap();
    assert_eq!(removed, 1);
    assert!(db.get_art(referenced).unwrap().is_some());
    assert!(db.get_art(orphan).unwrap().is_none());
}
