//! Regression guard for the scan pipeline's abort path: when a batch commit
//! fails, `run_pipeline` propagates the error — but a worker parked in
//! `ByteBudget::acquire` waits on a release only a successful flush performs, so
//! nothing would ever wake it. `scan_directory_with` is public API and an
//! embedder that catches the error and carries on (the Python plugin surface)
//! would accumulate one leaked thread — plus its in-flight art bytes — per
//! blocked worker, per failed scan (#618).

// The stranding this guards against is platform-independent; the *observation* is
// not. `live_threads` counts entries under `/proc/self/task`, which exists only on
// Linux — and the macOS and FreeBSD legs both run `cargo test --workspace`, so
// without this gate the helper's `expect` panics there rather than testing
// anything. Linux carries the required gates (including both sanitizer legs), so
// the regression stays covered where it is checked.
#![cfg(target_os = "linux")]

use musefs_core::{ScanOptions, scan_directory_with};
use musefs_db::{Db, Format, NewTrack};
use std::time::{Duration, Instant};

/// A minimal FLAC carrying a PICTURE block of `data_len` image bytes, so the
/// probed track has a non-zero art weight for the budget (mirrors the fixture in
/// `pipeline_backpressure.rs`). marker + STREAMINFO (not last) + PICTURE (last)
/// + a little audio.
fn flac_with_art(data_len: usize) -> Vec<u8> {
    let mut v = b"fLaC".to_vec();
    // STREAMINFO (type 0), not last, 34-byte body.
    v.push(0x00);
    v.extend_from_slice(&[0, 0, 34]);
    v.extend(std::iter::repeat_n(0u8, 34));
    // PICTURE (type 6), last block.
    let mut body = Vec::new();
    body.extend_from_slice(&3u32.to_be_bytes()); // picture type (front cover)
    let mime = b"image/png";
    body.extend_from_slice(&u32::try_from(mime.len()).unwrap().to_be_bytes());
    body.extend_from_slice(mime);
    body.extend_from_slice(&0u32.to_be_bytes()); // description length
    body.extend_from_slice(&0u32.to_be_bytes()); // width
    body.extend_from_slice(&0u32.to_be_bytes()); // height
    body.extend_from_slice(&0u32.to_be_bytes()); // depth
    body.extend_from_slice(&0u32.to_be_bytes()); // colors
    body.extend_from_slice(&u32::try_from(data_len).unwrap().to_be_bytes());
    body.extend(std::iter::repeat_n(0u8, data_len));
    v.push(0x86); // last-block flag (0x80) | PICTURE (0x06)
    let blen = body.len();
    v.extend_from_slice(&[
        u8::try_from((blen >> 16) & 0xFF).unwrap(),
        u8::try_from((blen >> 8) & 0xFF).unwrap(),
        u8::try_from(blen & 0xFF).unwrap(),
    ]);
    v.extend_from_slice(&body);
    v.extend_from_slice(b"AUDIO");
    v
}

/// Live threads in this process. This test binary holds no other test, so the
/// count is a precise observation of the pipeline's own workers.
fn live_threads() -> usize {
    std::fs::read_dir("/proc/self/task")
        .expect("/proc/self/task")
        .count()
}

#[test]
fn failed_flush_does_not_strand_budget_blocked_workers() {
    let dir = tempfile::tempdir().unwrap();
    let lib = dir.path().join("lib");
    std::fs::create_dir(&lib).unwrap();
    for i in 0..16 {
        std::fs::write(lib.join(format!("a{i}.flac")), flac_with_art(6)).unwrap();
    }

    let db_path = dir.path().join("musefs.db");
    let db = Db::open(&db_path).unwrap();
    // Wedge the store: a second connection holds SQLite's single write lock in an
    // uncommitted transaction, so the scan's first batch write fails (busy) just
    // as a full disk or an I/O fault would.
    let blocker = Db::open(&db_path).unwrap();
    let mut hold = blocker.bulk_writer().unwrap();
    hold.upsert_track(&NewTrack {
        backing_path: "/blocker.flac".into(),
        format: Format::Flac,
        audio_offset: 0,
        audio_length: 1,
        backing_size: 1,
        backing_mtime_ns: 0,
        backing_ctime_ns: 0,
    })
    .unwrap();

    let baseline = live_threads();
    // Cap the in-flight budget below two files' cumulative art, so workers pile
    // up in `acquire` behind the batch the failing flush never releases.
    let err = scan_directory_with(
        &db,
        &lib,
        &ScanOptions {
            jobs: 4,
            batch_bytes: 8,
            ..Default::default()
        },
    )
    .expect_err("a wedged store must fail the batch commit");
    assert!(
        db.list_tracks().unwrap().is_empty(),
        "nothing may have been persisted through a wedged store"
    );

    // Workers are joined before the error propagates, so this is normally true on
    // the first read; poll briefly so a thread still winding down cannot flake it.
    let start = Instant::now();
    while live_threads() > baseline && start.elapsed() < Duration::from_secs(10) {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(
        live_threads(),
        baseline,
        "aborted scan ({err}) left worker threads parked on the byte budget"
    );
}
