//! Concurrent-reader coverage for the serve path WITHOUT a FUSE mount: many
//! threads share one `HeaderCache` and each holds its own read-only WAL
//! connection, exercising the quick_cache header cache and concurrent SQLite
//! reads under contention. Deterministic (bounded, barrier-synchronized,
//! asserts on bytes) so it can gate CI and run under AddressSanitizer.
//!
//! The second half of the file covers the read-ahead pool's shared byte budget
//! under the same kind of load (#628) — the serve-path tests here run with
//! read-ahead disabled, so they never reach it.
mod common;

use std::sync::{Arc, Barrier, Mutex};

use musefs_core::{BackingReader, HeaderCache, Mode, ReadAhead, ReadAheadPool, read_at_with_file};
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
    let pool = ReadAheadPool::new(0);
    let buf = Arc::new(Mutex::new(ReadAhead::new(0)));
    let epoch = std::sync::atomic::AtomicU64::new(0);
    let br = BackingReader::new(&file, &buf, &pool, 0, resolved.stamp.size, &epoch);
    read_at_with_file(&resolved, &db, &br, 0, resolved.total_len).unwrap()
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

// `CoreError` must be `Send` so it can cross the thread boundary in the
// concurrent serve path exercised above.
#[test]
fn core_error_is_send() {
    fn assert_send<T: Send>() {}
    assert_send::<musefs_core::CoreError>();
}

// ---------------------------------------------------------------------------
// Read-ahead budget accounting under concurrency (#628).
//
// The serve-path tests above run with `ReadAheadPool::new(0)` — read-ahead
// disabled — so nothing they touch reaches the pool's shared `charged` counter.
// The tests below drive that counter directly from many threads, so the ASan
// and TSan legs (which run THIS binary) cover it too.
//
// The invariant under test is `charged == Σ(registered buffers' bytes.len())`,
// maintained across three concurrent sites: `reconcile` on the read path,
// `deregister` on handle drop, and `evict_one_coldest` under budget pressure.
// #536 fixed a real race between the last two — an eviction clearing a buffer
// while a `deregister` removed it either double-counted or leaked — by making
// the buffer length the single coordination point. That property had no
// concurrent regression test.
// ---------------------------------------------------------------------------

/// Backing bytes for the read-ahead stress. Small enough that 16 threads each
/// holding a window stay cheap under a sanitizer, large enough that a 1 MiB
/// per-stream window (the cap implied by `RA_BUDGET`) still leaves room to seek.
const RA_BACKING_LEN: u64 = 4 * 1024 * 1024;
/// Deliberately tiny: `per_stream_cap` is `budget / 4` == 1 MiB, so a handful of
/// live streams exhaust it and every subsequent miss must evict a colder one.
const RA_BUDGET: u64 = 4 * 1024 * 1024;
const RA_THREADS: u64 = 16;
/// Per round: this many seeks, each followed by a short sequential run — the
/// mix that grows a window geometrically and then resets it to the floor.
const RA_SEEKS_PER_ROUND: u32 = 3;
const RA_SEQ_RUN: u32 = 4;
const RA_CHUNK: u64 = 4096;

/// Rounds of stream churn per thread. Deliberately modest: this binary is what
/// the ASan and TSan CI legs run and a sanitized build is 10-50x slower, so the
/// default keeps both new tests to ~2s under ThreadSanitizer (measured) instead
/// of adding minutes to every run. `MUSEFS_READAHEAD_STRESS_ROUNDS` cranks it up
/// for a local soak — 200, the scale of the report in #628, is ~9s under TSan.
fn ra_rounds() -> u64 {
    std::env::var("MUSEFS_READAHEAD_STRESS_ROUNDS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(24)
}

/// A backing file of position-derived bytes plus its contents, so every read
/// below can be checked against an oracle as well as counted.
fn ra_backing(dir: &std::path::Path) -> (std::fs::File, Vec<u8>) {
    use std::io::Write;
    let path = dir.join("readahead.bin");
    let len = usize::try_from(RA_BACKING_LEN).expect("64-bit target");
    let data: Vec<u8> = (0..len)
        .map(|i| u8::try_from(i % 251).expect("bounded by 251"))
        .collect();
    std::fs::File::create(&path)
        .unwrap()
        .write_all(&data)
        .unwrap();
    (std::fs::File::open(&path).unwrap(), data)
}

/// Drive one registered stream through `RA_SEEKS_PER_ROUND` seeks, each followed
/// by a sequential run — misses (which call `permitted_window`, evicting other
/// streams) interleaved with hits, all charging the shared budget via
/// `reconcile`. Asserts bytes as it goes so an accounting fix that corrupts the
/// window ring cannot pass quietly.
fn ra_drive(
    file: &std::fs::File,
    data: &[u8],
    pool: &ReadAheadPool,
    buf: &Arc<Mutex<ReadAhead>>,
    key: usize,
    seed: u64,
) {
    let chunk = usize::try_from(RA_CHUNK).expect("64-bit target");
    let epoch = std::sync::atomic::AtomicU64::new(0);
    let br = BackingReader::new(file, buf, pool, key, RA_BACKING_LEN, &epoch);
    let mut got = vec![0u8; chunk];
    let mut rng = seed | 1;
    for _ in 0..RA_SEEKS_PER_ROUND {
        rng = rng
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        let mut off = rng % (RA_BACKING_LEN - RA_CHUNK);
        for _ in 0..RA_SEQ_RUN {
            br.read_exact_at(&mut got, off).unwrap();
            let lo = usize::try_from(off).expect("64-bit target");
            assert_eq!(
                got,
                data[lo..lo + chunk],
                "read-ahead byte mismatch at {off}"
            );
            off += RA_CHUNK;
            if off + RA_CHUNK > RA_BACKING_LEN {
                break;
            }
        }
    }
}

/// Sum of what the registered buffers actually hold — the right-hand side of
/// the `charged == Σ bytes` invariant. Only safe to call once every reader has
/// joined; a lock taken mid-read would just observe a torn moment.
fn ra_buffered(streams: &[(usize, Arc<Mutex<ReadAhead>>)]) -> u64 {
    streams.iter().map(|(_, b)| b.lock().unwrap().len()).sum()
}

/// Churn: many threads repeatedly register a stream, read through it, and
/// deregister it, against a budget small enough that eviction fires constantly.
/// Every stream is deregistered by the end, so the pool must be holding nothing:
/// a leaked charge (an eviction and a `deregister` both declining to uncharge)
/// leaves `charged` positive, a double uncharge underflows it to a huge value.
#[test]
fn readahead_churn_ends_with_nothing_charged() {
    let dir = tempfile::tempdir().unwrap();
    let (file, data) = ra_backing(dir.path());
    let pool = Arc::new(ReadAheadPool::new(RA_BUDGET));
    let rounds = ra_rounds();

    std::thread::scope(|s| {
        for tid in 0..RA_THREADS {
            let (file, data, pool) = (&file, &data, &pool);
            s.spawn(move || {
                for round in 0..rounds {
                    // Slab keys are unique per live handle; re-using one while
                    // another thread holds it registered would drop that
                    // registration and strand its bytes by construction.
                    let key = usize::try_from(tid * rounds + round).expect("64-bit target");
                    let buf = Arc::new(Mutex::new(ReadAhead::new(pool.per_stream_cap())));
                    pool.register(key, Arc::clone(&buf));
                    ra_drive(file, data, pool, &buf, key, tid * 7919 + round);
                    // Reads for this handle are done (same thread), mirroring
                    // release-after-last-read on the real serve path.
                    pool.deregister(key);
                }
            });
        }
    });

    assert_eq!(
        pool.charged(),
        0,
        "every stream was deregistered; charged budget must return to 0"
    );
}

/// The property #536 restored, asserted directly: with streams still registered
/// and all readers joined, the pool's `charged` must equal the bytes those
/// buffers really hold. Also pins that the overshoot the racy grant permits is
/// transient — once reads serialize again, eviction pulls the pool back inside
/// its budget.
#[test]
fn readahead_charged_matches_registered_buffers_at_quiescence() {
    let dir = tempfile::tempdir().unwrap();
    let (file, data) = ra_backing(dir.path());
    let pool = Arc::new(ReadAheadPool::new(RA_BUDGET));
    let rounds = ra_rounds();

    // One long-lived stream per thread, registered for the whole run.
    let streams: Vec<(usize, Arc<Mutex<ReadAhead>>)> = (0..RA_THREADS)
        .map(|tid| {
            let key = usize::try_from(tid + 1).expect("64-bit target");
            let buf = Arc::new(Mutex::new(ReadAhead::new(pool.per_stream_cap())));
            pool.register(key, Arc::clone(&buf));
            (key, buf)
        })
        .collect();

    std::thread::scope(|s| {
        for (tid, (key, buf)) in streams.iter().enumerate() {
            let (file, data, pool) = (&file, &data, &pool);
            let tid = u64::try_from(tid).expect("bounded by RA_THREADS");
            s.spawn(move || {
                for round in 0..rounds {
                    ra_drive(file, data, pool, buf, *key, tid * 6151 + round);
                }
            });
        }
    });

    // Quiesced: no reader holds a buffer lock, so the sum is exact.
    let buffered = ra_buffered(&streams);
    assert!(
        buffered > 0,
        "streams must still hold read-ahead bytes, else the invariant is 0 == 0"
    );
    assert_eq!(
        pool.charged(),
        buffered,
        "charged budget diverged from the bytes the registered buffers hold"
    );
    // Per-stream containment is the half of the envelope that IS exact under
    // concurrency: a grant is capped at `per_stream_cap` before any budget
    // arithmetic, so no single stream can run away with the whole pool.
    for (key, buf) in &streams {
        let held = buf.lock().unwrap().len();
        assert!(
            held <= pool.per_stream_cap(),
            "stream {key} holds {held} > per-stream cap {}",
            pool.per_stream_cap()
        );
    }

    // `permitted_window` loads `charged`, grants, and only charges after the
    // fill completes, so concurrent misses can each grant against the same free
    // room and overshoot the budget — racy by design (see `has_room_for`). What
    // must hold is that the overshoot does not stick: one serialized pass evicts
    // colder streams until the grant fits, so the pool settles back inside its
    // envelope, still with the counter matching the buffers.
    for (key, buf) in &streams {
        let seed = u64::try_from(*key).expect("bounded by RA_THREADS");
        ra_drive(&file, &data, &pool, buf, *key, seed);
    }
    assert!(
        pool.charged() <= pool.budget(),
        "serialized reads must settle the pool back inside its budget: charged {} > budget {}",
        pool.charged(),
        pool.budget()
    );
    assert_eq!(
        pool.charged(),
        ra_buffered(&streams),
        "charged budget diverged from the buffers after the settling pass"
    );

    for (key, _) in &streams {
        pool.deregister(*key);
    }
    assert_eq!(
        pool.charged(),
        0,
        "release of every stream must uncharge all"
    );
}
