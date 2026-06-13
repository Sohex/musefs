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
fn get_track_art_with_meta_joins_the_art_row() {
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

    let got = db.get_track_art_with_meta(track).unwrap();
    assert_eq!(got.len(), 1);
    let (ta, meta) = &got[0];
    assert_eq!(ta.art_id, art_id);
    assert_eq!(ta.picture_type, 3);
    assert_eq!(ta.description, "front");
    assert_eq!(ta.ordinal, 0);
    let meta = meta.as_ref().expect("joined art metadata must be present");
    assert_eq!(meta.mime, "image/jpeg");
    assert_eq!(meta.byte_len, 3);
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
            backing_mtime_ns: 0,
            backing_ctime_ns: 0,
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

#[test]
fn shared_art_survives_until_last_reference_gone() {
    let db = Db::open_in_memory().unwrap();
    let t1 = db.upsert_track(&new_track("/m/a.flac")).unwrap();
    let t2 = db.upsert_track(&new_track("/m/b.flac")).unwrap();
    let art = db.upsert_art(&jpeg(vec![7, 7, 7])).unwrap();
    let link = |ord| TrackArt {
        art_id: art,
        picture_type: 3,
        description: String::new(),
        ordinal: ord,
    };
    db.set_track_art(t1, &[link(0)]).unwrap();
    db.set_track_art(t2, &[link(0)]).unwrap();

    // Drop one reference: still linked by t2 => survives gc.
    db.set_track_art(t1, &[]).unwrap();
    assert_eq!(db.gc_orphan_art().unwrap(), 0);
    assert!(db.get_art(art).unwrap().is_some());

    // Drop the last reference: now an orphan.
    db.set_track_art(t2, &[]).unwrap();
    assert_eq!(db.gc_orphan_art().unwrap(), 1);
    assert!(db.get_art(art).unwrap().is_none());
}

#[test]
fn set_track_art_replaces_links() {
    let db = Db::open_in_memory().unwrap();
    let t = db.upsert_track(&new_track("/m/a.flac")).unwrap();
    let a = db.upsert_art(&jpeg(vec![1])).unwrap();
    let b = db.upsert_art(&jpeg(vec![2])).unwrap();

    db.set_track_art(
        t,
        &[
            TrackArt {
                art_id: a,
                picture_type: 3,
                description: "front".to_string(),
                ordinal: 0,
            },
            TrackArt {
                art_id: b,
                picture_type: 4,
                description: "back".to_string(),
                ordinal: 1,
            },
        ],
    )
    .unwrap();
    assert_eq!(db.get_track_art(t).unwrap().len(), 2);

    // Replace: a single, re-described link (relink + reorder).
    db.set_track_art(
        t,
        &[TrackArt {
            art_id: b,
            picture_type: 3,
            description: "now-front".to_string(),
            ordinal: 0,
        }],
    )
    .unwrap();
    let got = db.get_track_art(t).unwrap();
    assert_eq!(got.len(), 1, "old links are cleared before insert");
    assert_eq!(got[0].art_id, b);
    assert_eq!(got[0].description, "now-front");

    // Empty items clears all links.
    db.set_track_art(t, &[]).unwrap();
    assert!(db.get_track_art(t).unwrap().is_empty());
}

#[test]
fn read_art_chunk_into_matches_vec_variant_and_errors_on_short_read() {
    let db = Db::open_in_memory().unwrap();
    let id = db.upsert_art(&jpeg((0u8..64).collect())).unwrap();

    let expected = db.read_art_chunk(id, 3, 5).unwrap();
    let mut buf = vec![0u8; 5];
    db.read_art_chunk_into(id, 3, &mut buf).unwrap();
    assert_eq!(buf, expected);
    assert_eq!(buf, vec![3, 4, 5, 6, 7]);

    // Reading past the blob end must error, not zero-fill (read_at_exact contract).
    let mut over = vec![0u8; 128];
    assert!(db.read_art_chunk_into(id, 3, &mut over).is_err());
}
