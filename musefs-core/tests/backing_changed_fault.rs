//! `HeaderCache::resolve` re-stats the backing file and rejects a track whose
//! file changed size/mtime since scan. Driven by a real file mutation — no
//! fault seam needed.
mod common;

use musefs_core::{CoreError, HeaderCache, Mode};
use musefs_db::Db;

#[test]
fn shrinking_the_backing_file_after_scan_yields_backing_changed() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("a.flac");
    let (audio_offset, audio_length) = common::write_flac(&src, &["TITLE=T"], &[0xAB; 4096]);
    let db = Db::open_in_memory().unwrap();
    let id = db
        .upsert_track(&musefs_db::NewTrack {
            backing_path: src.to_string_lossy().into_owned(),
            format: musefs_db::Format::Flac,
            audio_offset,
            audio_length,
            backing_size: std::fs::metadata(&src).unwrap().len(),
            backing_mtime: common::real_mtime(&src),
        })
        .unwrap();
    db.replace_tags(id, &[musefs_db::Tag::new("title", "T", 0)])
        .unwrap();

    // First resolve succeeds (file matches the scanned stat).
    let cache = HeaderCache::new(Mode::Synthesis);
    cache.resolve(&db, id).unwrap();

    // Truncate the backing file: its size now disagrees with the stored
    // backing_size. A fresh resolve (new HeaderCache, no cache hit) must error.
    let f = std::fs::OpenOptions::new().write(true).open(&src).unwrap();
    f.set_len(10).unwrap();
    drop(f);

    let err = HeaderCache::new(Mode::Synthesis)
        .resolve(&db, id)
        .unwrap_err();
    match err {
        CoreError::BackingChanged(path) => assert!(path.ends_with("a.flac")),
        other => panic!("expected BackingChanged, got {other:?}"),
    }
}
