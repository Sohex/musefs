mod common;

use common::write_m4a_moov_last;
use musefs_core::scan_directory;
use musefs_db::Db;

#[test]
fn moov_last_m4a_scans_as_one_track() {
    let dir = tempfile::tempdir().unwrap();
    let (_off, _len) = write_m4a_moov_last(&dir.path().join("a.m4a"), &[0x11u8; 256]);
    let db = Db::open_in_memory().unwrap();
    let stats = scan_directory(&db, dir.path()).unwrap();
    assert_eq!(stats.scanned, 1, "moov-at-end M4A should probe & ingest");
    assert_eq!(stats.skipped, 0);
}
