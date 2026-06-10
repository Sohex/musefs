//! Reader failure paths under injected backing-read faults. Gated on `metrics`.
//! The fault seam is process-global, so `set_backing_fault` serializes on an
//! internal lock held for the guard's lifetime — concurrent fault tests in this
//! binary take turns rather than clobbering each other's kind.
#![cfg(feature = "metrics")]

mod common;

use musefs_core::metrics::{BackingFault, set_backing_fault};
use musefs_core::{CoreError, HeaderCache, Mode, ResolvedFile, read_at};
use musefs_db::Db;

fn resolve_one_flac() -> (Db, std::sync::Arc<ResolvedFile>, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("a.flac");
    let (audio_offset, audio_length) = common::write_flac(&src, &["TITLE=Faulty"], &[0xAB; 4096]);
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
    db.replace_tags(id, &[musefs_db::Tag::new("title", "Faulty", 0)])
        .unwrap();
    let resolved = HeaderCache::new(Mode::Synthesis).resolve(&db, id).unwrap();
    (db, resolved, dir)
}

#[test]
fn eio_on_backing_read_surfaces_as_core_io_error() {
    let (db, resolved, _dir) = resolve_one_flac();
    let _guard = set_backing_fault(BackingFault::Eio);
    // Read a range that lands in the audio (BackingAudio) segment.
    let err = read_at(&resolved, &db, resolved.total_len - 16, 16).unwrap_err();
    match err {
        CoreError::Io(e) => assert_eq!(e.raw_os_error(), Some(5), "EIO maps to CoreError::Io(EIO)"),
        other => panic!("expected CoreError::Io, got {other:?}"),
    }
}

#[test]
fn short_backing_read_surfaces_as_core_io_error() {
    let (db, resolved, _dir) = resolve_one_flac();
    let _guard = set_backing_fault(BackingFault::ShortRead { prefix: 2 });
    let err = read_at(&resolved, &db, resolved.total_len - 16, 16).unwrap_err();
    match err {
        CoreError::Io(e) => assert_eq!(e.kind(), std::io::ErrorKind::UnexpectedEof),
        other => panic!("expected CoreError::Io(UnexpectedEof), got {other:?}"),
    }
}
