#![cfg(feature = "metrics")]
//! Concurrency test: a slow backing read must NOT block an unrelated metadata op.
//!
//! Run with:
//!   cargo test -p musefs-fuse --features metrics -- --ignored --nocapture --test-threads=1

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use fuser::BackgroundSession;
use musefs_core::{scan_directory, Mode, MountConfig, Musefs};

// ---------------------------------------------------------------------------
// Minimal proven FLAC fixture (mirrors tests/mount.rs exactly)
// ---------------------------------------------------------------------------

fn flac_block(block_type: u8, body: &[u8], is_last: bool) -> Vec<u8> {
    let mut out = Vec::new();
    out.push((if is_last { 0x80 } else { 0 }) | (block_type & 0x7F));
    let len = body.len();
    out.push(((len >> 16) & 0xFF) as u8);
    out.push(((len >> 8) & 0xFF) as u8);
    out.push((len & 0xFF) as u8);
    out.extend_from_slice(body);
    out
}

fn streaminfo_body() -> Vec<u8> {
    let mut b = vec![
        0x10, 0x00, 0x10, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x0A, 0xC4, 0x42, 0xF0, 0x00,
        0x00, 0x00, 0x00,
    ];
    b.extend_from_slice(&[0u8; 16]);
    b
}

fn vorbis_comment_body(vendor: &str, comments: &[&str]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&(vendor.len() as u32).to_le_bytes());
    out.extend_from_slice(vendor.as_bytes());
    out.extend_from_slice(&(comments.len() as u32).to_le_bytes());
    for c in comments {
        out.extend_from_slice(&(c.len() as u32).to_le_bytes());
        out.extend_from_slice(c.as_bytes());
    }
    out
}

fn make_flac(comments: &[&str], audio: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(b"fLaC");
    out.extend_from_slice(&flac_block(0, &streaminfo_body(), false));
    out.extend_from_slice(&flac_block(4, &vorbis_comment_body("orig", comments), true));
    out.extend_from_slice(audio);
    out
}

fn config() -> MountConfig {
    MountConfig {
        template: "$artist/$title".to_string(),
        fallbacks: BTreeMap::new(),
        default_fallback: "Unknown".to_string(),
        mode: Mode::Synthesis,
    }
}

// ---------------------------------------------------------------------------
// Setup: two-track mount with an on-disk DB (required for PerThread pool)
// ---------------------------------------------------------------------------

/// Returns (mountpoint, big_file_path, other_file_path, session, _backing_dir).
/// The backing `TempDir` is returned to keep it alive for the duration of the
/// mount (backing files must outlive the mount, mirroring tests/mount.rs).
fn setup_two_track_mount() -> (
    PathBuf,
    PathBuf,
    PathBuf,
    BackgroundSession,
    tempfile::TempDir,
) {
    let backing = tempfile::tempdir().unwrap();

    // Big FLAC: >= 2 MiB of audio payload so a 50ms/chunk read stays in-flight
    // for well over 100ms (kernel sends ~128 KiB chunks → ~16 FUSE reads = ~800ms).
    let big_audio = vec![0xABu8; 2 * 1024 * 1024];
    let big_flac = make_flac(&["ARTIST=Alpha", "TITLE=BigTrack"], &big_audio);
    std::fs::write(backing.path().join("big.flac"), &big_flac).unwrap();

    // Small FLAC: tiny, distinct artist/title so it renders to a different path.
    let small_flac = make_flac(&["ARTIST=Beta", "TITLE=SmallTrack"], &[0xCDu8; 64]);
    std::fs::write(backing.path().join("small.flac"), &small_flac).unwrap();

    // On-disk DB so musefs_db uses the PerThread pool (no shared lock during reads).
    let db_path = backing.path().join("m.db");
    let db = musefs_db::Db::open(&db_path).unwrap();
    scan_directory(&db, backing.path()).unwrap();

    let fs = Musefs::open(db, config()).unwrap();

    let mountpoint = tempfile::tempdir().unwrap();
    let session = musefs_fuse::spawn(fs, mountpoint.path(), "musefs-concurrency-test").unwrap();

    // Derive virtual paths from the known template ($artist/$title.flac).
    let mnt = mountpoint.keep(); // leak: keep mount alive
    let big = mnt.join("Alpha").join("BigTrack.flac");
    let other = mnt.join("Beta").join("SmallTrack.flac");

    (mnt, big, other, session, backing)
}

// ---------------------------------------------------------------------------
// The test
// ---------------------------------------------------------------------------

#[test]
#[ignore = "real mount; needs /dev/fuse — run with: cargo test -p musefs-fuse --features metrics -- --ignored --nocapture --test-threads=1"]
fn slow_read_does_not_block_stat() {
    // 50 ms per backing pread call. The big file is >2 MiB; the kernel sends
    // ~128 KiB chunks, so there are ~16 FUSE read calls → ~800ms total.
    // The fault duration is parsed once into a process-global OnceLock on the
    // first on_pread. This is its own integration-test binary with a single test,
    // so no earlier on_pread can have initialized it — setting the env var here,
    // before any read, is guaranteed to be observed.
    unsafe { std::env::set_var("MUSEFS_FAULT_PREAD_US", "50000") };

    let (_mnt, big, other, _session, _backing) = setup_two_track_mount();

    // Warm up the mount: stat both paths once so lookup/getattr are primed and
    // the virtual tree is built (avoids counting mount-initialization latency).
    let _ = std::fs::metadata(&other).expect("other file visible before test");
    let _ = std::fs::metadata(&big).expect("big file visible before test");

    // Start a large sequential read of `big` on a background thread.
    let big_clone = big.clone();
    let reader = std::thread::spawn(move || {
        // Result intentionally discarded: the point is to drive the (slow) FUSE
        // read round-trips, not to inspect the bytes.
        let _ = std::fs::read(&big_clone);
    });

    // Give the reader time to get mid-read (well before it finishes).
    std::thread::sleep(Duration::from_millis(150));

    // A metadata op on the OTHER file must return promptly despite the slow read.
    let t = Instant::now();
    let md = std::fs::metadata(&other).expect("stat of other file failed");
    let elapsed = t.elapsed();

    eprintln!("stat elapsed: {elapsed:?}  (file size: {} bytes)", md.len());

    assert!(md.len() > 0, "expected non-zero size from stat");
    // If the slow read serialized metadata ops (the old single-threaded model),
    // this stat would queue behind the in-flight read (~hundreds of ms). A
    // generous 200ms budget proves non-serialization while tolerating CI jitter
    // (observed in practice: tens of microseconds).
    assert!(
        elapsed < Duration::from_millis(200),
        "stat blocked for {elapsed:?} behind a slow read — metadata ops appear serialized"
    );

    reader.join().unwrap();
}
