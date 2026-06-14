#![cfg(feature = "metrics")]
//! StructureOnly passthrough: after `open`, the kernel must serve reads
//! directly from the registered backing fd — byte-identical content with ZERO
//! daemon preads. The Synthesis control test proves the pread counter
//! observable is live (a broken counter would make the zero-assert vacuous).
//!
//! Run with:
//!   cargo test -p musefs-fuse --features metrics --test passthrough -- --ignored --nocapture --test-threads=1

use std::collections::BTreeMap;
use std::io::Read;

use musefs_core::{Mode, MountConfig, Musefs, metrics, scan_directory};

// ---------------------------------------------------------------------------
// Minimal proven FLAC fixture (mirrors tests/mount.rs exactly)
// ---------------------------------------------------------------------------

fn flac_block(block_type: u8, body: &[u8], is_last: bool) -> Vec<u8> {
    let mut out = Vec::new();
    out.push((if is_last { 0x80 } else { 0 }) | (block_type & 0x7F));
    let len = body.len();
    out.push(u8::try_from((len >> 16) & 0xFF).expect("FLAC block length high byte fits in u8"));
    out.push(u8::try_from((len >> 8) & 0xFF).expect("FLAC block length middle byte fits in u8"));
    out.push(u8::try_from(len & 0xFF).expect("FLAC block length low byte fits in u8"));
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
    out.extend_from_slice(
        &u32::try_from(vendor.len())
            .expect("vendor length fits in u32")
            .to_le_bytes(),
    );
    out.extend_from_slice(vendor.as_bytes());
    out.extend_from_slice(
        &u32::try_from(comments.len())
            .expect("comment count fits in u32")
            .to_le_bytes(),
    );
    for c in comments {
        out.extend_from_slice(
            &u32::try_from(c.len())
                .expect("comment length fits in u32")
                .to_le_bytes(),
        );
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

fn config(mode: Mode) -> MountConfig {
    MountConfig {
        template: "$artist/$title".to_string(),
        fallbacks: BTreeMap::new(),
        default_fallback: "Unknown".to_string(),
        mode,
        poll_interval: std::time::Duration::ZERO,
        case_insensitive: false,
        read_ahead_budget: 64 * 1024 * 1024,
        read_ahead_prefetch: false,
        skip_on_missing: false,
    }
}

/// FUSE passthrough landed in mainline 6.9.
fn kernel_supports_passthrough() -> bool {
    let rel = std::fs::read_to_string("/proc/sys/kernel/osrelease").unwrap_or_default();
    let mut parts = rel.trim().split(|c: char| !c.is_ascii_digit());
    let major: u32 = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
    let minor: u32 = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
    (major, minor) >= (6, 9)
}

/// Scan one ~2 MiB FLAC into a fresh on-disk DB and mount it. Returns the
/// backing bytes, the virtual path, the session, the backing TempDir guard,
/// and the (deliberately leaked, mirrors concurrency.rs) mountpoint path.
fn mount_one_track(
    mode: Mode,
) -> (
    Vec<u8>,
    std::path::PathBuf,
    fuser::BackgroundSession,
    tempfile::TempDir,
    std::path::PathBuf,
) {
    let backing = tempfile::tempdir().unwrap();
    let audio = vec![0xABu8; 2 * 1024 * 1024];
    let flac = make_flac(&["ARTIST=Alpha", "TITLE=Track"], &audio);
    std::fs::write(backing.path().join("a.flac"), &flac).unwrap();

    // On-disk DB so musefs_db uses the PerThread pool (mirrors tests/concurrency.rs).
    let db_path = backing.path().join("m.db");
    let db = musefs_db::Db::open(&db_path).unwrap();
    scan_directory(&db, backing.path()).unwrap();

    let fs = Musefs::open(db, config(mode)).unwrap();
    let mountpoint = tempfile::tempdir().unwrap();
    let session = musefs_fuse::spawn(fs, mountpoint.path(), "musefs-passthrough-test").unwrap();
    let mnt = mountpoint.keep(); // keep mount alive for the test's duration
    let virt = mnt.join("Alpha").join("Track.flac");
    (flac, virt, session, backing, mnt)
}

/// The backing-open ioctl is CAP_SYS_ADMIN-gated; without it passthrough
/// falls back to daemon reads and the zero-pread assert cannot hold.
/// Mirrors the daemon's `cap_eff_has_sys_admin` (src/convert.rs) — keep the two
/// predicates in sync.
fn have_cap_sys_admin() -> bool {
    let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    status
        .lines()
        .find_map(|l| l.strip_prefix("CapEff:"))
        .and_then(|hex| u64::from_str_radix(hex.trim(), 16).ok())
        .is_some_and(|mask| mask & (1 << 21) != 0)
}

#[test]
#[ignore = "real mount; needs /dev/fuse + kernel >= 6.9 + CAP_SYS_ADMIN — build as user, run test binary via sudo"]
fn structure_only_reads_are_kernel_passthrough() {
    if !kernel_supports_passthrough() {
        eprintln!("kernel < 6.9: no FUSE passthrough; skipping");
        return;
    }
    if !have_cap_sys_admin() {
        eprintln!("no CAP_SYS_ADMIN: backing-open ioctl would EPERM; skipping (run via sudo)");
        return;
    }
    let (backing_bytes, virt, session, _backing, _mnt) = mount_one_track(Mode::StructureOnly);

    // Sequencing matters: FUSE `open` fires here (on_open and warmup counters
    // land), THEN reset, THEN read — so the pread assertion has a clean
    // baseline and covers exactly the reads of this fd.
    let mut f = std::fs::File::open(&virt).expect("open through mount");
    metrics::reset();
    let mut served = Vec::new();
    f.read_to_end(&mut served).expect("read through mount");

    assert_eq!(
        served, backing_bytes,
        "StructureOnly must serve backing bytes verbatim"
    );
    let snap = metrics::snapshot();
    assert_eq!(
        snap.preads, 0,
        "daemon served {} preads — kernel passthrough did not engage",
        snap.preads
    );

    // Close + unmount exercise the BackingId release path (release drops the
    // map entry; session drop tears down the channel) — a regression there
    // manifests as a hang here.
    drop(f);
    drop(session);
}

#[test]
#[ignore = "real mount; needs /dev/fuse — run with: cargo test -p musefs-fuse --features metrics --test passthrough -- --ignored --nocapture --test-threads=1"]
fn synthesis_reads_still_go_through_the_daemon() {
    let (_backing_bytes, virt, session, _backing, _mnt) = mount_one_track(Mode::Synthesis);

    let mut f = std::fs::File::open(&virt).expect("open through mount");
    metrics::reset();
    let mut served = Vec::new();
    f.read_to_end(&mut served).expect("read through mount");

    // Synthesis splices a fresh header; the read MUST hit the daemon. This
    // proves the pread counter observable is live, so the passthrough test's
    // zero-assert cannot pass vacuously.
    let snap = metrics::snapshot();
    assert!(
        snap.preads > 0,
        "expected daemon preads on a Synthesis mount; the metrics observable is broken"
    );
    drop(f);
    drop(session);
}
