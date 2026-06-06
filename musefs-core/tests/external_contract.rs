use musefs_core::{CoreError, HeaderCache, Mode};
use musefs_db::{Db, Format, NewTrack};
use musefs_format::fuzz_check::fixtures;

fn real_mtime(path: &std::path::Path) -> i64 {
    std::fs::metadata(path)
        .unwrap()
        .modified()
        .unwrap()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

#[test]
fn scanner_owned_bounds_mutation_returns_controlled_error() {
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
            backing_mtime: real_mtime(&audio_path),
        })
        .unwrap();

    let external = rusqlite::Connection::open(&db_path).unwrap();
    external
        .execute(
            "UPDATE tracks SET audio_length = audio_length + ?1 WHERE id = ?2",
            rusqlite::params![bytes.len() as i64, id],
        )
        .unwrap();

    let err = HeaderCache::new(Mode::Synthesis)
        .resolve(&db, id)
        .unwrap_err();
    let expected_path = audio_path.to_string_lossy();
    assert!(
        matches!(err, CoreError::BackingChanged(ref path) if path == expected_path.as_ref()),
        "unexpected error: {err:?}"
    );
}
