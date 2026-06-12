mod common;
use musefs_db::Db;

// A ctime-only change (mtime forged back after an in-place same-size rewrite)
// must NOT be skipped as "unchanged": revalidate re-probes it.
#[test]
fn revalidate_reprobes_on_ctime_only_change() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("a.flac");
    common::write_flac(&src, &["TITLE=Old"], &[0xAB; 4096]);
    let db_path = dir.path().join("m.db");
    {
        let db = Db::open(&db_path).unwrap();
        musefs_core::scan_directory(&db, dir.path()).unwrap();
    }
    let original_modified = std::fs::metadata(&src).unwrap().modified().unwrap();

    // Rewrite in place (same size, new tag), then forge mtime back. ctime moved.
    common::write_flac(&src, &["TITLE=New"], &[0xCD; 4096]);
    let f = std::fs::OpenOptions::new().write(true).open(&src).unwrap();
    f.set_times(std::fs::FileTimes::new().set_modified(original_modified))
        .unwrap();
    drop(f);

    let db = Db::open(&db_path).unwrap();
    let stats = musefs_core::revalidate(&db, dir.path()).unwrap();
    assert_eq!(stats.updated, 1, "ctime-only change must be re-probed");
}
