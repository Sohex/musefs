//! End-to-end: the `musefs` binary unmounts cleanly when sent SIGTERM, via the
//! CLI's fusermount3-based stop-signal handler. Ignored by default (needs
//! /dev/fuse + fusermount3), like the other FUSE e2e tests.

use std::process::{Child, Command};
use std::time::{Duration, Instant};

// --- minimal proven FLAC fixture (mirrors musefs-fuse/tests/mount.rs) ---

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

fn wait_until(mut cond: impl FnMut() -> bool, timeout: Duration) -> bool {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if cond() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    cond()
}

fn wait_exit(child: &mut Child, timeout: Duration) -> Option<std::process::ExitStatus> {
    let start = Instant::now();
    while start.elapsed() < timeout {
        match child.try_wait().unwrap() {
            Some(status) => return Some(status),
            None => std::thread::sleep(Duration::from_millis(100)),
        }
    }
    None
}

#[test]
#[ignore = "requires /dev/fuse + fusermount3; run with: cargo test -p musefs -- --ignored"]
fn sigterm_unmounts_cleanly() {
    let bin = env!("CARGO_BIN_EXE_musefs");

    // Backing dir + on-disk DB scanned via the real binary.
    let backing = tempfile::tempdir().unwrap();
    std::fs::write(
        backing.path().join("a.flac"),
        make_flac(&["ARTIST=Alice", "TITLE=Song"], &[0xAB; 64]),
    )
    .unwrap();
    let dbfile = tempfile::NamedTempFile::new().unwrap();
    let db = dbfile.path().to_str().unwrap();
    let scan = Command::new(bin)
        .args(["scan", backing.path().to_str().unwrap(), "--db", db])
        .status()
        .unwrap();
    assert!(scan.success(), "scan failed");

    // Mount as a child process.
    let mp = tempfile::tempdir().unwrap();
    let mut child = Command::new(bin)
        .args(["mount", mp.path().to_str().unwrap(), "--db", db])
        .spawn()
        .unwrap();

    let song = mp.path().join("Alice").join("Song.flac");
    assert!(
        wait_until(|| song.exists(), Duration::from_secs(15)),
        "mount did not come up"
    );

    // Send SIGTERM and assert a clean exit + unmounted mountpoint.
    let pid = rustix::process::Pid::from_child(&child);
    rustix::process::kill_process(pid, rustix::process::Signal::TERM).unwrap();

    let status = wait_exit(&mut child, Duration::from_secs(15))
        .unwrap_or_else(|| panic!("daemon did not exit after SIGTERM"));
    assert!(status.success(), "daemon exited non-zero: {status:?}");
    assert!(
        !song.exists(),
        "mount still present after SIGTERM (stale endpoint)"
    );
}
