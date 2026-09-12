mod common;
use common::{make_flac, streaminfo_body, vorbis_comment_body};
use musefs_core::{ChecksumTier, MatchStrictness, ScanOptions, scan_directory_with};
use musefs_db::Db;

fn opts(tier: ChecksumTier) -> ScanOptions {
    ScanOptions {
        jobs: 1,
        checksum: tier,
        ..Default::default()
    }
}

#[test]
fn full_tier_populates_both_columns_fingerprint_tier_only_one_none_neither() {
    for (tier, want_fp, want_ch) in [
        (ChecksumTier::None, false, false),
        (ChecksumTier::Fingerprint, true, false),
        (ChecksumTier::Full, true, true),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let flac = make_flac(
            &[
                (0, streaminfo_body()),
                (4, vorbis_comment_body("v", &["TITLE=A"])),
            ],
            &[0xAB; 32],
        );
        std::fs::write(dir.path().join("a.flac"), flac).unwrap();
        let db = Db::open_in_memory().unwrap();
        scan_directory_with(&db, dir.path(), &opts(tier)).unwrap();
        let t = &db.list_tracks().unwrap()[0];
        assert_eq!(
            t.fingerprint.is_some(),
            want_fp,
            "tier {tier:?} fingerprint"
        );
        assert_eq!(
            t.content_hash.is_some(),
            want_ch,
            "tier {tier:?} content_hash"
        );
        if want_ch {
            assert_eq!(t.content_hash.as_ref().unwrap().len(), 64);
        }
    }
}

fn full_opts(strictness: MatchStrictness) -> ScanOptions {
    ScanOptions {
        jobs: 1,
        checksum: ChecksumTier::Full,
        strictness,
        ..Default::default()
    }
}

fn write_a_flac(dir: &std::path::Path, name: &str, audio: &[u8]) -> std::path::PathBuf {
    let p = dir.join(name);
    let flac = make_flac(
        &[
            (0, streaminfo_body()),
            (4, vorbis_comment_body("v", &["TITLE=A"])),
        ],
        audio,
    );
    std::fs::write(&p, flac).unwrap();
    p
}

#[test]
fn pure_move_retargets_keeping_id_and_tags() {
    let dir = tempfile::tempdir().unwrap();
    let old = write_a_flac(dir.path(), "old.flac", &[0xAB; 64]);
    let db = Db::open_in_memory().unwrap();
    scan_directory_with(&db, dir.path(), &full_opts(MatchStrictness::Auto)).unwrap();
    let id = db.list_tracks().unwrap()[0].id;

    // Move the file and rescan the directory.
    let new = dir.path().join("new.flac");
    std::fs::rename(&old, &new).unwrap();
    scan_directory_with(&db, dir.path(), &full_opts(MatchStrictness::Auto)).unwrap();

    let tracks = db.list_tracks().unwrap();
    assert_eq!(tracks.len(), 1, "moved file must not create a second row");
    assert_eq!(tracks[0].id, id, "retarget keeps the id");
    assert!(tracks[0].backing_path.ends_with("new.flac"));
}

#[test]
fn copy_with_original_present_inserts_fresh() {
    let dir = tempfile::tempdir().unwrap();
    let orig = write_a_flac(dir.path(), "orig.flac", &[0xAB; 64]);
    let db = Db::open_in_memory().unwrap();
    scan_directory_with(&db, dir.path(), &full_opts(MatchStrictness::Auto)).unwrap();

    std::fs::copy(&orig, dir.path().join("copy.flac")).unwrap();
    scan_directory_with(&db, dir.path(), &full_opts(MatchStrictness::Auto)).unwrap();

    assert_eq!(
        db.list_tracks().unwrap().len(),
        2,
        "copy must not steal identity"
    );
}

#[test]
fn strict_refuses_when_candidate_has_no_content_hash() {
    let dir = tempfile::tempdir().unwrap();
    let old = write_a_flac(dir.path(), "old.flac", &[0xCD; 64]);
    let db = Db::open_in_memory().unwrap();
    // Seed at fingerprint tier => candidate has fingerprint but no content_hash.
    scan_directory_with(
        &db,
        dir.path(),
        &ScanOptions {
            jobs: 1,
            checksum: ChecksumTier::Fingerprint,
            ..Default::default()
        },
    )
    .unwrap();
    let id = db.list_tracks().unwrap()[0].id;

    std::fs::rename(&old, dir.path().join("new.flac")).unwrap();
    scan_directory_with(&db, dir.path(), &full_opts(MatchStrictness::Strict)).unwrap();

    let tracks = db.list_tracks().unwrap();
    // Strict cannot confirm (no candidate content_hash) => fresh insert, old orphaned.
    assert!(
        tracks
            .iter()
            .any(|t| t.id != id && t.backing_path.ends_with("new.flac"))
    );
}

#[test]
fn fast_retargets_despite_content_mismatch() {
    let dir = tempfile::tempdir().unwrap();
    let a = write_a_flac(dir.path(), "a.flac", &[0xAA; 64]);
    let db = Db::open_in_memory().unwrap();
    scan_directory_with(&db, dir.path(), &full_opts(MatchStrictness::Fast)).unwrap();
    let id = db.list_tracks().unwrap()[0].id;

    // Delete A; create B with the same tags + same length but different bytes:
    // same fingerprint, different content_hash.
    std::fs::remove_file(&a).unwrap();
    write_a_flac(dir.path(), "b.flac", &[0xBB; 64]);
    scan_directory_with(&db, dir.path(), &full_opts(MatchStrictness::Fast)).unwrap();

    let tracks = db.list_tracks().unwrap();
    assert_eq!(tracks.len(), 1, "Fast retargets despite content mismatch");
    assert_eq!(tracks[0].id, id, "retarget keeps the id");
    assert!(tracks[0].backing_path.ends_with("b.flac"));
}

#[test]
fn auto_rejects_forged_fingerprint_match() {
    let dir = tempfile::tempdir().unwrap();
    let a = write_a_flac(dir.path(), "a.flac", &[0xAA; 64]);
    let db = Db::open_in_memory().unwrap();
    scan_directory_with(&db, dir.path(), &full_opts(MatchStrictness::Auto)).unwrap();
    let id = db.list_tracks().unwrap()[0].id;

    // Delete A; create B with the same fingerprint but different content.
    std::fs::remove_file(&a).unwrap();
    write_a_flac(dir.path(), "b.flac", &[0xBB; 64]);
    scan_directory_with(&db, dir.path(), &full_opts(MatchStrictness::Auto)).unwrap();

    // A carries a content_hash (Full seed) => Auto full-hashes B, mismatch => fresh insert.
    let tracks = db.list_tracks().unwrap();
    assert_eq!(tracks.len(), 2, "Auto refuses a forged fingerprint match");
    let b = tracks
        .iter()
        .find(|t| t.backing_path.ends_with("b.flac"))
        .expect("b.flac inserted fresh");
    assert_ne!(b.id, id, "fresh insert gets a new id");
}

#[test]
fn ambiguous_fingerprint_match_inserts_fresh() {
    let dir = tempfile::tempdir().unwrap();
    // Two files with identical content + tags => they share a fingerprint.
    let a1 = write_a_flac(dir.path(), "a1.flac", &[0xAA; 64]);
    let a2 = write_a_flac(dir.path(), "a2.flac", &[0xAA; 64]);
    let db = Db::open_in_memory().unwrap();
    scan_directory_with(&db, dir.path(), &full_opts(MatchStrictness::Auto)).unwrap();

    // Delete both; create b.flac with the same content => matches TWO missing candidates.
    std::fs::remove_file(&a1).unwrap();
    std::fs::remove_file(&a2).unwrap();
    write_a_flac(dir.path(), "b.flac", &[0xAA; 64]);
    scan_directory_with(&db, dir.path(), &full_opts(MatchStrictness::Auto)).unwrap();

    let tracks = db.list_tracks().unwrap();
    assert_eq!(tracks.len(), 3, "ambiguous match inserts fresh");
    assert!(tracks.iter().any(|t| t.backing_path.ends_with("b.flac")));
}

use musefs_core::revalidate_with;

#[test]
fn revalidate_backfills_fingerprint_on_unchanged_files() {
    let dir = tempfile::tempdir().unwrap();
    write_a_flac(dir.path(), "a.flac", &[0xAB; 64]);
    let db = Db::open_in_memory().unwrap();
    // Initial scan with no checksums.
    scan_directory_with(
        &db,
        dir.path(),
        &ScanOptions {
            jobs: 1,
            checksum: ChecksumTier::None,
            ..Default::default()
        },
    )
    .unwrap();
    assert!(db.list_tracks().unwrap()[0].fingerprint.is_none());

    // Revalidate at the fingerprint tier: the file is unchanged but missing the
    // fingerprint, so it must be re-processed (backfilled), not skipped.
    let stats = revalidate_with(
        &db,
        dir.path(),
        &ScanOptions {
            jobs: 1,
            checksum: ChecksumTier::Fingerprint,
            ..Default::default()
        },
    )
    .unwrap();
    assert!(
        db.list_tracks().unwrap()[0].fingerprint.is_some(),
        "backfilled"
    );
    assert_eq!(stats.updated, 1);
}

#[test]
fn revalidate_full_backfills_content_hash_on_fingerprint_tier_row() {
    let dir = tempfile::tempdir().unwrap();
    write_a_flac(dir.path(), "a.flac", &[0xAB; 64]);
    let db = Db::open_in_memory().unwrap();
    // Seed at the fingerprint tier: fingerprint set, content_hash NULL.
    scan_directory_with(
        &db,
        dir.path(),
        &ScanOptions {
            jobs: 1,
            checksum: ChecksumTier::Fingerprint,
            ..Default::default()
        },
    )
    .unwrap();
    let seeded = &db.list_tracks().unwrap()[0];
    assert!(seeded.fingerprint.is_some(), "fingerprint seeded");
    assert!(seeded.content_hash.is_none(), "content_hash not yet set");

    // Revalidate at the Full tier WITHOUT touching the file: the
    // `!has_fingerprint || !has_content_hash` gate must re-process the row to
    // backfill content_hash (kills the `||`->`&&` and the two `delete !` mutants
    // — any of which would leave the fp-present/ch-absent track skipped).
    let stats = revalidate_with(
        &db,
        dir.path(),
        &ScanOptions {
            jobs: 1,
            checksum: ChecksumTier::Full,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(stats.updated, 1, "the fp-only row must be re-processed");
    assert!(
        db.list_tracks().unwrap()[0].content_hash.is_some(),
        "content_hash backfilled at Full tier"
    );
}

#[test]
fn revalidate_full_reprocesses_row_missing_fingerprint() {
    let dir = tempfile::tempdir().unwrap();
    write_a_flac(dir.path(), "a.flac", &[0xAB; 64]);
    let db = Db::open_in_memory().unwrap();
    // Seed with no checksums, then force a row that has a content_hash but NO
    // fingerprint — the case that exercises the Full arm's `!has_fingerprint`
    // half of `!has_fingerprint || !has_content_hash`.
    scan_directory_with(
        &db,
        dir.path(),
        &ScanOptions {
            jobs: 1,
            checksum: ChecksumTier::None,
            ..Default::default()
        },
    )
    .unwrap();
    let id = db.list_tracks().unwrap()[0].id;
    db.set_track_checksums(
        id,
        musefs_db::ChecksumWrite::Keep,
        musefs_db::ChecksumWrite::Set(&"d".repeat(64)),
    )
    .unwrap();
    let seeded = &db.list_tracks().unwrap()[0];
    assert!(seeded.fingerprint.is_none(), "fingerprint absent");
    assert!(seeded.content_hash.is_some(), "content_hash present");

    // Full tier must re-process the row to backfill the missing fingerprint
    // (kills the Full-arm `delete !` on `!has_fingerprint` — dropping it would
    // leave this row skipped because content_hash is already present).
    let stats = revalidate_with(
        &db,
        dir.path(),
        &ScanOptions {
            jobs: 1,
            checksum: ChecksumTier::Full,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(
        stats.updated, 1,
        "row missing a fingerprint must be re-processed"
    );
    assert!(
        db.list_tracks().unwrap()[0].fingerprint.is_some(),
        "backfilled"
    );
}

#[test]
fn two_new_files_matching_one_orphan_retarget_one_insert_one() {
    let dir = tempfile::tempdir().unwrap();
    // Scan a single file so it gets a DB row.
    let a = write_a_flac(dir.path(), "a.flac", &[0xAA; 64]);
    let db = Db::open_in_memory().unwrap();
    scan_directory_with(&db, dir.path(), &full_opts(MatchStrictness::Auto)).unwrap();
    let id_a = db.list_tracks().unwrap()[0].id;

    // Delete a.flac, then create two new files with the identical content
    // (same tags, same audio) so both share a.flac's fingerprint + content_hash.
    std::fs::remove_file(&a).unwrap();
    write_a_flac(dir.path(), "b.flac", &[0xAA; 64]);
    write_a_flac(dir.path(), "c.flac", &[0xAA; 64]);
    scan_directory_with(&db, dir.path(), &full_opts(MatchStrictness::Auto)).unwrap();

    let tracks = db.list_tracks().unwrap();
    // Exactly 2 rows: the orphan was retargeted by one new file, the other was
    // inserted fresh — no double-claim, no third row.
    assert_eq!(tracks.len(), 2, "one retarget + one fresh insert = 2 rows");
    let retargeted: Vec<_> = tracks.iter().filter(|t| t.id == id_a).collect();
    assert_eq!(retargeted.len(), 1, "exactly one row keeps id_a");
    // Both b.flac and c.flac must appear across the two rows.
    let paths: Vec<_> = tracks.iter().map(|t| t.backing_path.as_str()).collect();
    assert!(paths.iter().any(|p| p.ends_with("b.flac")));
    assert!(paths.iter().any(|p| p.ends_with("c.flac")));
    // The non-retargeted row has a new id.
    let fresh: Vec<_> = tracks.iter().filter(|t| t.id != id_a).collect();
    assert_eq!(fresh.len(), 1, "exactly one fresh row");
}

/// Seed one FLAC at the `full` tier and hand back its row.
fn seed_at_full(dir: &std::path::Path, audio: &[u8]) -> (Db, musefs_db::Track) {
    write_a_flac(dir, "a.flac", audio);
    let db = Db::open_in_memory().unwrap();
    scan_directory_with(&db, dir, &full_opts(MatchStrictness::Auto)).unwrap();
    let row = db.list_tracks().unwrap()[0].clone();
    assert!(row.content_hash.is_some(), "full tier seeds a hash");
    (db, row)
}

/// #689: a pass at a tier below `full` must not leave the previous bytes'
/// `content_hash` on a row whose file was rewritten in place. The column is the
/// documented forensic identity of the *current* backing file, and a stale one
/// also poisons move recovery, because a later move is then refused on a hash
/// comparison it can never satisfy.
///
/// Both in-place paths reach this: `scan --force`, which re-ingests a known
/// path, and `revalidate`, which refreshes only the structural columns.
#[test]
fn in_place_rewrite_at_fingerprint_tier_clears_the_stale_content_hash() {
    for revalidating in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let (db, seeded) = seed_at_full(dir.path(), &[0xAA; 64]);
        let stale = seeded.content_hash.clone().unwrap();

        // Rewrite the same path with different bytes, then re-probe it at the
        // default tier, which computes no full hash for it.
        write_a_flac(dir.path(), "a.flac", &[0xBB; 96]);
        let reprobe = ScanOptions {
            force: true,
            ..opts(ChecksumTier::Fingerprint)
        };
        if revalidating {
            musefs_core::revalidate_with(&db, dir.path(), &reprobe).unwrap();
        } else {
            scan_directory_with(&db, dir.path(), &reprobe).unwrap();
        }

        let t = &db.list_tracks().unwrap()[0];
        assert_eq!(t.id, seeded.id, "same row, rewritten in place");
        assert_ne!(
            t.fingerprint, seeded.fingerprint,
            "sanity: the rewrite really did change the content (revalidating={revalidating})"
        );
        assert_eq!(
            t.content_hash, None,
            "a hash of the old bytes must be cleared, not preserved (was {stale})"
        );
    }
}

/// The user-visible half of #689: after an in-place rewrite, a later move of
/// that file still retargets its row. With the stale hash left in place, Auto
/// full-hashes the moved file, compares it against the *old* bytes' hash,
/// refuses, and orphans the curated tags and art.
#[test]
fn a_rewritten_file_can_still_be_move_recovered() {
    let dir = tempfile::tempdir().unwrap();
    let (db, seeded) = seed_at_full(dir.path(), &[0xAA; 64]);
    let path = write_a_flac(dir.path(), "a.flac", &[0xBB; 96]);
    scan_directory_with(
        &db,
        dir.path(),
        &ScanOptions {
            force: true,
            ..opts(ChecksumTier::Fingerprint)
        },
    )
    .unwrap();

    std::fs::rename(&path, dir.path().join("moved.flac")).unwrap();
    scan_directory_with(&db, dir.path(), &full_opts(MatchStrictness::Auto)).unwrap();

    let tracks = db.list_tracks().unwrap();
    assert_eq!(tracks.len(), 1, "moved file must not orphan its row");
    assert_eq!(tracks[0].id, seeded.id);
    assert!(tracks[0].backing_path.ends_with("moved.flac"));
}

/// An unchanged file re-probed at a lower tier keeps the hash it already has:
/// `Clear` is driven by an observed content change, not by the pass's tier, so
/// the tier-preservation property the old `COALESCE` protected survives (#689).
#[test]
fn unchanged_file_at_fingerprint_tier_keeps_its_content_hash() {
    let dir = tempfile::tempdir().unwrap();
    let (db, seeded) = seed_at_full(dir.path(), &[0xAA; 64]);

    scan_directory_with(
        &db,
        dir.path(),
        &ScanOptions {
            force: true,
            ..opts(ChecksumTier::Fingerprint)
        },
    )
    .unwrap();
    assert_eq!(
        db.list_tracks().unwrap()[0].content_hash,
        seeded.content_hash,
        "an untouched file's hash is still true of it"
    );
}

/// A `--fast` retarget confirms nothing by design, so when it produces no hash
/// for the arriving file the candidate's stored one — which described the file
/// that left — has to be dropped rather than inherited (#689).
#[test]
fn fast_retarget_without_a_new_hash_clears_the_stale_one() {
    let dir = tempfile::tempdir().unwrap();
    let a = write_a_flac(dir.path(), "a.flac", &[0xAA; 64]);
    let db = Db::open_in_memory().unwrap();
    scan_directory_with(&db, dir.path(), &full_opts(MatchStrictness::Fast)).unwrap();
    assert!(db.list_tracks().unwrap()[0].content_hash.is_some());

    std::fs::remove_file(&a).unwrap();
    write_a_flac(dir.path(), "b.flac", &[0xBB; 64]);
    scan_directory_with(
        &db,
        dir.path(),
        &ScanOptions {
            jobs: 1,
            checksum: ChecksumTier::Fingerprint,
            strictness: MatchStrictness::Fast,
            ..Default::default()
        },
    )
    .unwrap();

    let tracks = db.list_tracks().unwrap();
    assert_eq!(tracks.len(), 1, "Fast still retargets");
    assert!(tracks[0].backing_path.ends_with("b.flac"));
    assert_eq!(
        tracks[0].content_hash, None,
        "an unconfirmed retarget must not inherit the departed file's hash"
    );
}

/// The `fingerprint`-tier move of a file whose row already carries a
/// `content_hash`: Auto escalates, re-reads the moved file, confirms it against
/// the stored hash, and persists the value so the next scan need not re-read.
/// The confirm is the one hash computed outside the probe, and it now refuses to
/// answer unless the file still matches the stamp the probe committed to (#690).
#[test]
fn fingerprint_tier_move_confirms_against_the_stored_hash() {
    let dir = tempfile::tempdir().unwrap();
    let (db, seeded) = seed_at_full(dir.path(), &[0xAA; 64]);

    std::fs::rename(dir.path().join("a.flac"), dir.path().join("moved.flac")).unwrap();
    scan_directory_with(&db, dir.path(), &opts(ChecksumTier::Fingerprint)).unwrap();

    let tracks = db.list_tracks().unwrap();
    assert_eq!(tracks.len(), 1, "the confirmed move must retarget in place");
    assert_eq!(tracks[0].id, seeded.id);
    assert!(tracks[0].backing_path.ends_with("moved.flac"));
    assert_eq!(
        tracks[0].content_hash, seeded.content_hash,
        "the confirm's hash is persisted, so the next scan need not re-read"
    );
}
