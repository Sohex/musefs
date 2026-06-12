//! `HeaderCache::resolve` re-stats the backing file and rejects a track whose
//! file changed size/mtime since scan. Driven by a real file mutation — no
//! fault seam needed.
mod common;

use musefs_core::{CoreError, HeaderCache, Mode};
use musefs_db::Db;
use std::os::unix::fs::MetadataExt;

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
            backing_mtime_ns: common::real_mtime_ns(&src),
            backing_ctime_ns: common::real_ctime_ns(&src),
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

// A same-size in-place rewrite within the same whole second (the common case in
// a fast test) changed only the sub-second mtime + ctime. The old whole-second
// guard would have passed it; the ns stamp must reject it.
#[test]
#[allow(clippy::cast_possible_truncation)]
fn same_size_subsecond_rewrite_yields_backing_changed() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("a.flac");
    let (audio_offset, audio_length) = common::write_flac(&src, &["TITLE=T"], &[0xAB; 4096]);
    let db = Db::open_in_memory().unwrap();
    let meta = std::fs::metadata(&src).unwrap();
    let id = db
        .upsert_track(&musefs_db::NewTrack {
            backing_path: src.to_string_lossy().into_owned(),
            format: musefs_db::Format::Flac,
            audio_offset,
            audio_length,
            backing_size: meta.len(),
            backing_mtime_ns: common::real_mtime_ns(&src),
            backing_ctime_ns: common::real_ctime_ns(&src),
        })
        .unwrap();
    db.replace_tags(id, &[musefs_db::Tag::new("title", "T", 0)])
        .unwrap();
    HeaderCache::new(Mode::Synthesis).resolve(&db, id).unwrap();

    // Rewrite the same number of bytes in place: size identical, mtime/ctime move.
    std::fs::write(&src, {
        let mut v = std::fs::read(&src).unwrap();
        v[audio_offset as usize] ^= 0xFF;
        v
    })
    .unwrap();

    let err = HeaderCache::new(Mode::Synthesis)
        .resolve(&db, id)
        .unwrap_err();
    assert!(matches!(err, CoreError::BackingChanged(_)), "got {err:?}");
}

// Adversary rewrites in place, then forges mtime back to the stored value.
// mtime_ns now matches; only ctime (un-forgeable) caught the change.
#[test]
#[allow(clippy::cast_possible_truncation)]
fn forged_mtime_is_caught_by_ctime() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("a.flac");
    let (audio_offset, audio_length) = common::write_flac(&src, &["TITLE=T"], &[0xAB; 4096]);
    let db = Db::open_in_memory().unwrap();
    let meta = std::fs::metadata(&src).unwrap();
    let original_modified = meta.modified().unwrap();
    let id = db
        .upsert_track(&musefs_db::NewTrack {
            backing_path: src.to_string_lossy().into_owned(),
            format: musefs_db::Format::Flac,
            audio_offset,
            audio_length,
            backing_size: meta.len(),
            backing_mtime_ns: common::real_mtime_ns(&src),
            backing_ctime_ns: common::real_ctime_ns(&src),
        })
        .unwrap();
    db.replace_tags(id, &[musefs_db::Tag::new("title", "T", 0)])
        .unwrap();
    HeaderCache::new(Mode::Synthesis).resolve(&db, id).unwrap();

    // In-place same-size rewrite, then reset mtime back to the scanned instant.
    let mut v = std::fs::read(&src).unwrap();
    v[audio_offset as usize] ^= 0xFF;
    std::fs::write(&src, v).unwrap();
    let f = std::fs::OpenOptions::new().write(true).open(&src).unwrap();
    f.set_times(std::fs::FileTimes::new().set_modified(original_modified))
        .unwrap();
    drop(f);

    // mtime_ns now equals the stored stamp; ctime advanced and must trip the guard.
    let err = HeaderCache::new(Mode::Synthesis)
        .resolve(&db, id)
        .unwrap_err();
    assert!(matches!(err, CoreError::BackingChanged(_)), "got {err:?}");
}

// The synthesized file's displayed mtime stays a plausible whole-second value
// (≈ the backing file's mtime), not the renamed column's raw nanoseconds.
#[test]
fn displayed_mtime_is_whole_seconds() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("a.flac");
    let (audio_offset, audio_length) = common::write_flac(&src, &["TITLE=T"], &[0xAB; 4096]);
    let db = Db::open_in_memory().unwrap();
    let meta = std::fs::metadata(&src).unwrap();
    let id = db
        .upsert_track(&musefs_db::NewTrack {
            backing_path: src.to_string_lossy().into_owned(),
            format: musefs_db::Format::Flac,
            audio_offset,
            audio_length,
            backing_size: meta.len(),
            backing_mtime_ns: common::real_mtime_ns(&src),
            backing_ctime_ns: common::real_ctime_ns(&src),
        })
        .unwrap();
    db.replace_tags(id, &[musefs_db::Tag::new("title", "T", 0)])
        .unwrap();
    let resolved = HeaderCache::new(Mode::Synthesis).resolve(&db, id).unwrap();
    // Plausible epoch-seconds (this millennium), never ~10^18.
    assert!(resolved.mtime_secs >= meta.mtime() && resolved.mtime_secs < 32_503_680_000);
}
