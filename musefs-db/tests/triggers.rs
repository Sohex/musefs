mod common;
use common::new_track;
use musefs_db::{Db, Format, NewTrack, StructuralBlock, Tag};

/// A second well in the past, so a stamp of the current time cannot land on it.
const AGED: i64 = 1_000_000_000;

/// A file-backed store plus a raw connection to it, to plant an old `updated_at`
/// that the public API has no reason to write.
fn aged_store() -> (tempfile::TempDir, Db, rusqlite::Connection) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("musefs.db");
    let db = Db::open(&path).unwrap();
    let raw = rusqlite::Connection::open(&path).unwrap();
    (dir, db, raw)
}

/// One way a re-upsert can differ from the stored row.
type Change = fn(&mut NewTrack);

fn age(raw: &rusqlite::Connection, id: i64) {
    raw.execute(
        "UPDATE tracks SET updated_at = ?1 WHERE id = ?2",
        rusqlite::params![AGED, id],
    )
    .unwrap();
}

/// #757: a synthesized file's served second follows `updated_at`, so a
/// re-upsert finding the file exactly as recorded, which is what re-probing an
/// unchanged file does, must leave it and `content_version` alone.
#[test]
fn an_identical_reupsert_leaves_updated_at_and_content_version_alone() {
    let (_dir, db, raw) = aged_store();
    let id = db.upsert_track(&new_track("/m/a.flac")).unwrap();
    age(&raw, id);
    let before = db.track_content_version(id).unwrap();

    db.upsert_track(&new_track("/m/a.flac")).unwrap();

    let t = db.get_track(id).unwrap().unwrap();
    assert_eq!(t.updated_at, AGED);
    assert_eq!(t.content_version, before);
}

/// Each column the upsert writes is a real change when it differs, and moves
/// `updated_at` on its own: an `AND` slipped into the guard would let the other
/// six agreeing hide it.
#[test]
fn every_column_the_upsert_writes_moves_updated_at_when_it_changes() {
    let (_dir, db, raw) = aged_store();
    let changes: [(&str, Change); 7] = [
        ("format", |t| t.format = Format::Mp3),
        ("audio_offset", |t| t.audio_offset = 99),
        ("audio_length", |t| t.audio_length = 999),
        ("backing_size", |t| t.backing_size = 1101),
        ("backing_mtime_ns", |t| t.backing_mtime_ns += 1),
        ("backing_ctime_ns", |t| t.backing_ctime_ns += 1),
        ("backing_ino", |t| t.backing_ino = Some(7)),
    ];
    for (column, change) in changes {
        let path = format!("/m/{column}.flac");
        let id = db.upsert_track(&new_track(&path)).unwrap();
        age(&raw, id);
        let mut changed = new_track(&path);
        change(&mut changed);

        db.upsert_track(&changed).unwrap();

        assert!(
            db.get_track(id).unwrap().unwrap().updated_at > AGED,
            "a changed {column} is a change, so updated_at must move"
        );
    }
}

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
