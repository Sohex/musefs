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
