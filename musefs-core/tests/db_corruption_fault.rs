//! A byte-corrupted SQLite store must surface as a mapped error from the DB
//! layer, not a panic, when the serve path reads it.
use std::io::{Seek, SeekFrom, Write};

use musefs_db::Db;

#[test]
fn corrupt_db_header_errors_instead_of_panicking() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("musefs.db");

    // Build a valid store with one track.
    {
        let db = Db::open(&db_path).unwrap();
        db.upsert_track(&musefs_db::NewTrack {
            backing_path: "/nonexistent/a.flac".into(),
            format: musefs_db::Format::Flac,
            audio_offset: 0,
            audio_length: 1,
            backing_size: 1,
            backing_mtime: 0,
        })
        .unwrap();
    } // connection dropped, file flushed

    // Clobber the 16-byte SQLite magic header ("SQLite format 3\0").
    {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .open(&db_path)
            .unwrap();
        f.seek(SeekFrom::Start(0)).unwrap();
        f.write_all(&[0u8; 16]).unwrap();
        f.flush().unwrap();
    }

    // Opening read-only and listing must be an Err (whether the failure lands at
    // open or at first query), never a panic or a wrong-but-Ok result.
    let result = Db::open_readonly(&db_path).and_then(|db| db.list_tracks());
    assert!(
        result.is_err(),
        "corrupt DB must yield a DbError, got {result:?}"
    );
}
