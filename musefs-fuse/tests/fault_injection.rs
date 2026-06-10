//! An injected EIO backing read surfaces as an I/O error through a real FUSE
//! mount, proving the process-global fault seam reaches the worker thread.
#![cfg(feature = "metrics")]

use std::collections::BTreeMap;

use musefs_core::metrics::{BackingFault, set_backing_fault};
use musefs_core::{Mode, MountConfig, Musefs, scan_directory};

fn flac_block(block_type: u8, body: &[u8], is_last: bool) -> Vec<u8> {
    let mut out = Vec::new();
    out.push((if is_last { 0x80 } else { 0 }) | (block_type & 0x7F));
    let len = body.len();
    out.push(u8::try_from((len >> 16) & 0xFF).unwrap());
    out.push(u8::try_from((len >> 8) & 0xFF).unwrap());
    out.push(u8::try_from(len & 0xFF).unwrap());
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
    out.extend_from_slice(&u32::try_from(vendor.len()).unwrap().to_le_bytes());
    out.extend_from_slice(vendor.as_bytes());
    out.extend_from_slice(&u32::try_from(comments.len()).unwrap().to_le_bytes());
    for c in comments {
        out.extend_from_slice(&u32::try_from(c.len()).unwrap().to_le_bytes());
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
        poll_interval: std::time::Duration::ZERO,
        case_insensitive: false,
    }
}

#[test]
#[ignore = "requires /dev/fuse; run with: cargo test -p musefs-fuse --features metrics -- --ignored"]
fn eio_backing_read_surfaces_through_the_mount() {
    let backing = tempfile::tempdir().unwrap();
    let flac = make_flac(&["ARTIST=Alice", "TITLE=Song"], &vec![0xAB; 256 * 1024]);
    std::fs::write(backing.path().join("a.flac"), &flac).unwrap();

    let db = musefs_db::Db::open_in_memory().unwrap();
    scan_directory(&db, backing.path()).unwrap();
    let fs = Musefs::open(db, config()).unwrap();
    let mountpoint = tempfile::tempdir().unwrap();
    let session = musefs_fuse::spawn(fs, mountpoint.path(), "musefs-fault-test").unwrap();

    let song = mountpoint.path().join("Alice").join("Song.flac");

    // With EIO injected on the next backing read, reading the (audio-bearing)
    // file through the mount must fail with an I/O error, not succeed or hang.
    let _guard = set_backing_fault(BackingFault::Eio);
    let err = std::fs::read(&song).expect_err("read should fail under injected EIO");
    // FUSE maps the reader's CoreError::Io(EIO) straight back to errno EIO, so a
    // tight assertion guards against a false pass from an unrelated failure.
    assert_eq!(
        err.raw_os_error(),
        Some(5),
        "injected EIO should surface as EIO through the mount, got {err:?}"
    );

    drop(session);
}
