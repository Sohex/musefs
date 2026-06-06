//! Regression guard for the scan pipeline's byte-budget backpressure: with the
//! in-flight budget cap equal to the per-batch byte-flush threshold, a worker
//! blocking in `budget.acquire` (called before `send`) must never strand the
//! writer parked on `recv`. Pre-fix this deadlocked on art-bearing libraries.

use musefs_core::{scan_directory_with, ScanOptions};
use musefs_db::Db;

/// A minimal FLAC carrying a PICTURE block of `data_len` image bytes (so the
/// scanned track has a non-zero art weight for the budget). marker + STREAMINFO
/// (not last) + PICTURE (last) + a little audio.
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
    body.extend_from_slice(&(mime.len() as u32).to_be_bytes());
    body.extend_from_slice(mime);
    body.extend_from_slice(&0u32.to_be_bytes()); // description length
    body.extend_from_slice(&0u32.to_be_bytes()); // width
    body.extend_from_slice(&0u32.to_be_bytes()); // height
    body.extend_from_slice(&0u32.to_be_bytes()); // depth
    body.extend_from_slice(&0u32.to_be_bytes()); // colors
    body.extend_from_slice(&(data_len as u32).to_be_bytes());
    body.extend(std::iter::repeat_n(0u8, data_len));
    v.push(0x86); // last-block flag (0x80) | PICTURE (0x06)
    let blen = body.len();
    v.extend_from_slice(&[(blen >> 16) as u8, (blen >> 8) as u8, blen as u8]);
    v.extend_from_slice(&body);
    v.extend_from_slice(b"AUDIO");
    v
}

#[test]
fn pipeline_completes_under_art_backpressure() {
    use std::time::{Duration, Instant};

    let dir = tempfile::tempdir().unwrap();
    for i in 0..8 {
        std::fs::write(dir.path().join(format!("a{i}.flac")), flac_with_art(6)).unwrap();
    }

    // Cap the in-flight budget below two files' cumulative art (6 bytes each), so a
    // second concurrent `acquire` blocks while the writer's batch sits below the
    // flush threshold — the exact pre-fix deadlock window.
    let root = dir.path().to_path_buf();
    let handle = std::thread::spawn(move || {
        let db = Db::open_in_memory().unwrap();
        scan_directory_with(
            &db,
            &root,
            &ScanOptions {
                jobs: 4,
                batch_bytes: 8,
                ..Default::default()
            },
        )
        .unwrap();
        db.list_tracks().unwrap().len()
    });

    // Watchdog: fail fast with a clear message instead of hanging forever.
    let start = Instant::now();
    while !handle.is_finished() {
        assert!(
            start.elapsed() < Duration::from_secs(30),
            "scan pipeline deadlocked under art backpressure"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    let scanned = handle.join().unwrap();
    assert_eq!(scanned, 8, "all art-bearing files should ingest");
}
