use musefs_db::{Db, Format, NewTrack};
use musefs_format::fuzz_check::fixtures;

fn real_mtime_ns(path: &std::path::Path) -> i64 {
    use std::os::unix::fs::MetadataExt;
    let meta = std::fs::metadata(path).unwrap();
    meta.mtime() * 1_000_000_000 + meta.mtime_nsec()
}

fn real_ctime_ns(path: &std::path::Path) -> i64 {
    use std::os::unix::fs::MetadataExt;
    let meta = std::fs::metadata(path).unwrap();
    meta.ctime() * 1_000_000_000 + meta.ctime_nsec()
}

#[test]
fn scanner_owned_bounds_mutation_is_rejected_by_the_contract() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("musefs.db");
    let audio_path = dir.path().join("sample.mp3");
    let bytes = fixtures::mp3();
    std::fs::write(&audio_path, &bytes).unwrap();
    let bounds = musefs_format::mp3::locate_audio(&bytes).unwrap();

    let db = Db::open(&db_path).unwrap();
    let id = db
        .upsert_track(&NewTrack {
            backing_path: audio_path.to_string_lossy().into_owned(),
            format: Format::Mp3,
            audio_offset: bounds.audio_offset,
            audio_length: bounds.audio_length,
            backing_size: bytes.len() as u64,
            backing_mtime_ns: real_mtime_ns(&audio_path),
            backing_ctime_ns: real_ctime_ns(&audio_path),
        })
        .unwrap();

    // An external scanner mutating the audio bounds past the backing file is
    // rejected at the SQLite contract boundary by the V4 bounds CHECK — it
    // fails fast at write time rather than being discovered later at read.
    let external = rusqlite::Connection::open(&db_path).unwrap();
    let rejected = external.execute(
        "UPDATE tracks SET audio_length = audio_length + ?1 WHERE id = ?2",
        rusqlite::params![i64::try_from(bytes.len()).unwrap(), id],
    );
    assert!(
        rejected.is_err(),
        "bounds CHECK must reject an external audio_length overrun"
    );
}
