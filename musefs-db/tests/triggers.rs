mod common;
use common::new_track;
use musefs_db::{ChecksumWrite, Db, Format, NewTrack, StructuralBlock, Tag};

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

/// A restamp that changes `backing_ctime_ns` and nothing else. That is what a
/// same-size rewrite with its old mtime put back (`touch -r`) looks like, and
/// also what a `chmod` looks like: ctime is the one stamp field userspace cannot
/// set back, so it separates neither case from the other. Only the checksums
/// written with it can.
fn ctime_restamp(t: &NewTrack) -> NewTrack {
    let mut restamped = t.clone();
    restamped.backing_ctime_ns += 1;
    restamped
}

/// A ctime-only restamp bumps `content_version` unless a checksum written in the
/// same statement proves the bytes unchanged: a fingerprint or content hash that
/// was stored before and is written again with the same value. Without the bump
/// a rewrite that kept its size and mtime served the same `content_version`, so
/// the served mtime held still and a kernel cache kept the old pages.
#[test]
fn a_ctime_only_restamp_bumps_unless_a_checksum_proves_the_bytes_unchanged() {
    use ChecksumWrite::{Clear, Keep, Set};
    let (fp, other, hash) = ("a".repeat(64), "b".repeat(64), "c".repeat(64));
    let cases: [(&str, [ChecksumWrite; 2], [ChecksumWrite; 2], bool); 7] = [
        (
            "a changed fingerprint",
            [Set(&fp), Keep],
            [Set(&other), Clear],
            true,
        ),
        (
            "a cleared fingerprint",
            [Set(&fp), Keep],
            [Clear, Clear],
            true,
        ),
        (
            "no checksum before or after",
            [Keep, Keep],
            [Clear, Clear],
            true,
        ),
        (
            "a first fingerprint proves nothing about before",
            [Keep, Keep],
            [Set(&fp), Clear],
            true,
        ),
        (
            "the same fingerprint",
            [Set(&fp), Keep],
            [Set(&fp), Clear],
            false,
        ),
        (
            "the same content hash",
            [Set(&fp), Set(&hash)],
            [Clear, Set(&hash)],
            false,
        ),
        (
            "both the same",
            [Set(&fp), Set(&hash)],
            [Set(&fp), Set(&hash)],
            false,
        ),
    ];
    for (what, stored, written, bumps) in cases {
        let db = Db::open_in_memory().unwrap();
        let t = new_track("/m/a.mp3");
        let id = db
            .upsert_track_with_checksums(&t, stored[0], stored[1])
            .unwrap();
        let before = db.track_content_version(id).unwrap();

        db.upsert_track_with_checksums(&ctime_restamp(&t), written[0], written[1])
            .unwrap();

        let after = db.track_content_version(id).unwrap();
        let expected = if bumps { before + 1 } else { before };
        assert_eq!(after, expected, "{what}");
    }
}

/// The bulk writer runs the same statement.
#[test]
fn a_bulk_writer_restamp_bumps_on_the_same_rule() {
    let db = Db::open_in_memory().unwrap();
    let t = new_track("/m/a.mp3");
    let fp = "a".repeat(64);
    let id = db
        .upsert_track_with_checksums(&t, ChecksumWrite::Set(&fp), ChecksumWrite::Keep)
        .unwrap();
    let before = db.track_content_version(id).unwrap();
    let mut bulk = db.bulk_writer().unwrap();
    bulk.upsert_track_with_checksums(
        &ctime_restamp(&t),
        ChecksumWrite::Clear,
        ChecksumWrite::Clear,
    )
    .unwrap();
    bulk.commit().unwrap();
    assert_eq!(db.track_content_version(id).unwrap(), before + 1);
}

/// The first revalidate after the upgrade writes a fingerprint where V4 left
/// none. That alone does not bump: a stamp that did not change, ctime included,
/// already vouches for the bytes, and a first fingerprint compares with nothing.
/// On a filesystem that keeps inodes the same pass records one where the
/// migration left the sentinel, and that bumps once, as it always has
/// (`tracks_geometry_au`); ctime does not add a second.
#[test]
fn the_first_revalidate_after_the_upgrade_bumps_only_for_the_inode() {
    let fp = "a".repeat(64);
    for (what, ino, bumps) in [
        ("no inode kept (FAT, exFAT)", None, 0),
        ("an inode recorded", Some(4242), 1),
    ] {
        let db = Db::open_in_memory().unwrap();
        let migrated = new_track("/m/a.mp3");
        let id = db.upsert_track(&migrated).unwrap();
        let before = db.track_content_version(id).unwrap();

        let mut probed = migrated.clone();
        probed.backing_ino = ino;
        db.upsert_track_with_checksums(&probed, ChecksumWrite::Set(&fp), ChecksumWrite::Clear)
            .unwrap();

        assert_eq!(
            db.track_content_version(id).unwrap(),
            before + bumps,
            "{what}"
        );
    }
}

/// A rename changes ctime and nothing else a stamp records, and the retarget
/// writes the fingerprint it was matched on, so the same rule leaves it alone:
/// the served bytes are the ones the row already described (#674).
#[test]
fn a_retarget_whose_fingerprint_matches_does_not_bump_for_ctime() {
    let db = Db::open_in_memory().unwrap();
    let t = new_track("/m/old.mp3");
    let fp = "a".repeat(64);
    let id = db
        .upsert_track_with_checksums(&t, ChecksumWrite::Set(&fp), ChecksumWrite::Keep)
        .unwrap();
    let before = db.track_content_version(id).unwrap();
    db.retarget_track(
        id,
        std::path::Path::new("/m/new.mp3"),
        t.backing_size,
        t.backing_mtime_ns,
        t.backing_ctime_ns + 1,
        t.backing_ino,
        t.audio_offset,
        t.audio_length,
        ChecksumWrite::Set(&fp),
        ChecksumWrite::Clear,
    )
    .unwrap();
    assert_eq!(db.track_content_version(id).unwrap(), before);
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
