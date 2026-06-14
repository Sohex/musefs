#![cfg(feature = "metrics")]
//! E2E: `.musefs-metrics` read model + audio invariant (#394).
//!
//! Run with:
//!   cargo test -p musefs-fuse --features metrics --test metrics_e2e -- --ignored --nocapture

use std::io::{Read, Seek, SeekFrom};

use musefs_core::{Musefs, scan_directory};
use musefs_fuse::FuseConfig;

mod common;
use common::{config, make_flac};

fn fuse_config() -> FuseConfig {
    FuseConfig {
        expose_metrics: true,
        ..FuseConfig::default()
    }
}

/// Read entire file by looping in `chunk_size` byte reads until empty (handles
/// st_size == 0 /proc-style files).
fn read_to_end_chunked(path: &std::path::Path, chunk_size: usize) -> Vec<u8> {
    let mut f = std::fs::File::open(path).unwrap();
    let mut buf = Vec::new();
    loop {
        let prev = buf.len();
        buf.resize(prev + chunk_size, 0);
        let n = f.read(&mut buf[prev..]).unwrap();
        buf.truncate(prev + n);
        if n == 0 {
            break;
        }
    }
    buf
}

#[test]
#[ignore = "requires /dev/fuse + libfuse; run with --ignored"]
fn metrics_surface_e2e() {
    // Build backing dir + scanned DB + Musefs.
    let backing = tempfile::tempdir().unwrap();
    let audio_bytes: Vec<u8> = (0..=255).cycle().take(256).collect();
    let flac = make_flac(&["ARTIST=Alice", "TITLE=Song"], &audio_bytes);
    std::fs::write(backing.path().join("a.flac"), &flac).unwrap();
    let db = musefs_db::Db::open_in_memory().unwrap();
    scan_directory(&db, backing.path()).unwrap();
    let fs = Musefs::open(db, config()).unwrap();

    // Mount with expose_metrics on.
    let mountpoint = tempfile::tempdir().unwrap();
    let session =
        musefs_fuse::spawn_with(fs, mountpoint.path(), "musefs-metrics-e2e", fuse_config())
            .unwrap();

    let metrics_path = mountpoint.path().join(".musefs-metrics").join("metrics");

    // --- Assertion 1: metrics file contains Prometheus text with expected names ---
    {
        let body = read_to_end_chunked(&metrics_path, 4096);
        assert!(!body.is_empty(), "metrics file must not be empty");
        let text = String::from_utf8(body).expect("metrics must be valid UTF-8");
        assert!(
            text.contains("musefs_handles_open"),
            "metrics must contain musefs_handles_open"
        );
        assert!(
            text.contains("musefs_reads_inflight_max 1024"),
            "metrics must contain musefs_reads_inflight_max 1024"
        );
        assert!(
            text.contains("musefs_readahead_budget_bytes 67108864"),
            "metrics must report the read-ahead budget (64 MiB) from MountConfig"
        );
    }

    // --- Assertion 2: open a track, hold it, metrics shows handles_open >= 1 ---
    let song = mountpoint.path().join("Alice").join("Song.flac");
    let mut track_fh = std::fs::File::open(&song).unwrap();
    // Read one byte to force the open to land.
    let mut one = [0u8; 1];
    track_fh.read_exact(&mut one).unwrap();

    {
        let body = read_to_end_chunked(&metrics_path, 4096);
        let text = String::from_utf8(body).unwrap();
        // Find the musefs_handles_open line and extract the value.
        let handles_line = text
            .lines()
            .find(|l| l.starts_with("musefs_handles_open "))
            .expect("musefs_handles_open line must exist");
        let val: u64 = handles_line
            .split_whitespace()
            .nth(1)
            .unwrap()
            .parse()
            .unwrap();
        assert!(
            val >= 1,
            "handles_open must be >= 1 while track is open, got {val}"
        );
    }
    drop(track_fh);

    // --- Assertion 3: pread at offset returns correct absolute slice ---
    {
        let full = read_to_end_chunked(&metrics_path, 4096);
        // Open a fresh handle for pread-style offset read.
        let mut f = std::fs::File::open(&metrics_path).unwrap();
        let mut slice = [0u8; 20];
        f.seek(SeekFrom::Start(10)).unwrap();
        f.read_exact(&mut slice).unwrap();
        assert_eq!(
            slice,
            full[10..30],
            "offset read must return absolute slice of the body"
        );
    }

    // --- Assertion 4: chunked read-to-EOF reconstructs the whole body ---
    {
        let body = read_to_end_chunked(&metrics_path, 8);
        // Read again with a different chunk size; both must be byte-identical.
        let body2 = read_to_end_chunked(&metrics_path, 1);
        assert_eq!(body, body2, "chunked reads must reconstruct the same body");
        assert!(!body.is_empty(), "body must not be empty");
    }

    // --- Assertion 5: .musefs-metrics appears in readdir of root ---
    {
        let entries: Vec<String> = std::fs::read_dir(mountpoint.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        assert!(
            entries.contains(&".musefs-metrics".to_string()),
            ".musefs-metrics must appear in root readdir, got: {entries:?}"
        );
    }

    // --- Assertion 6: audio invariant — audio payload of served track matches backing ---
    {
        let mounted = std::fs::read(&song).unwrap();
        let backing = std::fs::read(backing.path().join("a.flac")).unwrap();

        let mounted_scan =
            musefs_format::flac::locate_audio(&mounted).expect("mounted file must be valid FLAC");
        let backing_scan =
            musefs_format::flac::locate_audio(&backing).expect("backing file must be valid FLAC");

        let m_off = usize::try_from(mounted_scan.audio_offset).unwrap();
        let m_len = usize::try_from(mounted_scan.audio_length).unwrap();
        let b_off = usize::try_from(backing_scan.audio_offset).unwrap();
        let b_len = usize::try_from(backing_scan.audio_length).unwrap();

        assert_eq!(
            m_len, b_len,
            "audio lengths must match (mounted={m_len}, backing={b_len})"
        );
        assert_eq!(
            &mounted[m_off..m_off + m_len],
            &backing[b_off..b_off + b_len],
            "audio payload must be byte-identical (cardinal invariant)"
        );
    }

    drop(session);
}
