//! Concurrent-reader coverage for the serve path WITHOUT a FUSE mount: many
//! threads share one `HeaderCache` and each holds its own read-only WAL
//! connection, exercising the quick_cache header cache and concurrent SQLite
//! reads under contention. Deterministic (bounded, barrier-synchronized,
//! asserts on bytes) so it can gate CI and run under AddressSanitizer.
mod common;

use std::sync::{Arc, Barrier};

use musefs_core::{HeaderCache, Mode, read_at_with_file};
use musefs_db::Db;

/// Build a file-backed store with `n` FLAC tracks (each a real backing file),
/// returning (db_path, track_ids, dir). Per-track audio differs so a
/// cross-wired read is detectable.
fn build_store(n: usize) -> (std::path::PathBuf, Vec<i64>, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("musefs.db");
    let db = Db::open(&db_path).unwrap();
    let mut ids = Vec::new();
    for i in 0..n {
        let src = dir.path().join(format!("track{i}.flac"));
        let i_byte = u8::try_from(i % 256).expect("bounded");
        let audio: Vec<u8> = (0u32..8192)
            .map(|b| {
                u8::try_from(b & 0xFF)
                    .expect("bounded")
                    .wrapping_add(i_byte)
            })
            .collect();
        let (audio_offset, audio_length) =
            common::write_flac(&src, &[&format!("TITLE=T{i}")], &audio);
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
        db.replace_tags(id, &[musefs_db::Tag::new("title", &format!("T{i}"), 0)])
            .unwrap();
        ids.push(id);
    }
    drop(db);
    (db_path, ids, dir)
}

/// Resolve + read one track fully on its own read-only connection + shared cache.
fn read_full(db_path: &std::path::Path, cache: &HeaderCache, id: i64) -> Vec<u8> {
    let db = Db::open_readonly(db_path).unwrap();
    let resolved = cache.resolve(&db, id).unwrap();
    let file = std::fs::File::open(&resolved.backing_path).unwrap();
    read_at_with_file(&resolved, &db, &file, 0, resolved.total_len).unwrap()
}

#[test]
fn same_file_from_many_threads_returns_identical_bytes() {
    let (db_path, ids, _dir) = build_store(1);
    let cache = Arc::new(HeaderCache::new(Mode::Synthesis));
    let reference = read_full(&db_path, &cache, ids[0]);

    const THREADS: usize = 16;
    const ITERS: usize = 50;
    let barrier = Arc::new(Barrier::new(THREADS));
    let db_path = Arc::new(db_path);
    let id0 = ids[0];
    let handles: Vec<_> = (0..THREADS)
        .map(|_| {
            let (cache, barrier, db_path, reference) = (
                cache.clone(),
                barrier.clone(),
                db_path.clone(),
                reference.clone(),
            );
            std::thread::spawn(move || {
                barrier.wait();
                for _ in 0..ITERS {
                    let got = read_full(&db_path, &cache, id0);
                    assert_eq!(got, reference, "concurrent same-file read diverged");
                }
            })
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }
}

#[test]
fn many_files_in_parallel_return_correct_bytes() {
    const N: usize = 12;
    let (db_path, ids, _dir) = build_store(N);
    let cache = Arc::new(HeaderCache::new(Mode::Synthesis));
    let references: Vec<Vec<u8>> = ids
        .iter()
        .map(|&id| read_full(&db_path, &cache, id))
        .collect();

    let barrier = Arc::new(Barrier::new(N));
    let db_path = Arc::new(db_path);
    let ids = Arc::new(ids);
    let references = Arc::new(references);
    let handles: Vec<_> = (0..N)
        .map(|t| {
            let (cache, barrier, db_path, ids, references) = (
                cache.clone(),
                barrier.clone(),
                db_path.clone(),
                ids.clone(),
                references.clone(),
            );
            std::thread::spawn(move || {
                barrier.wait();
                for k in 0..30 {
                    let idx = (t + k) % ids.len();
                    let got = read_full(&db_path, &cache, ids[idx]);
                    assert_eq!(got, references[idx], "parallel read of track {idx} wrong");
                }
            })
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }
}

#[test]
fn sustained_mixed_load_does_not_deadlock_or_corrupt() {
    const N: usize = 6;
    let (db_path, ids, _dir) = build_store(N);
    let cache = Arc::new(HeaderCache::new(Mode::Synthesis));
    let references: Vec<Vec<u8>> = ids
        .iter()
        .map(|&id| read_full(&db_path, &cache, id))
        .collect();

    const THREADS: usize = 24;
    const ITERS: usize = 100;
    let barrier = Arc::new(Barrier::new(THREADS));
    let db_path = Arc::new(db_path);
    let ids = Arc::new(ids);
    let references = Arc::new(references);
    let handles: Vec<_> = (0..THREADS)
        .map(|t| {
            let (cache, barrier, db_path, ids, references) = (
                cache.clone(),
                barrier.clone(),
                db_path.clone(),
                ids.clone(),
                references.clone(),
            );
            std::thread::spawn(move || {
                barrier.wait();
                for k in 0..ITERS {
                    let idx = (t * 7 + k) % ids.len();
                    let got = read_full(&db_path, &cache, ids[idx]);
                    assert_eq!(got, references[idx]);
                }
            })
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }
}
