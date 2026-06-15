mod common;
use common::{make_flac, streaminfo_body, vorbis_comment_body};
use musefs_core::{ChecksumTier, ScanOptions, scan_directory_with};
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
