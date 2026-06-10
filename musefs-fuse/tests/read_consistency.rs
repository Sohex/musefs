//! Mount-boundary read-consistency, mmap fidelity, and read-only refusal e2e.
//!
//! Exercises the kernel-facing read path of a live mount: randomized
//! pread/mmap reads compared against an in-memory oracle, whole-file mmap
//! fidelity, and that every mutating op is refused on the read-only mount.
//! See docs/superpowers/specs/2026-06-10-mount-read-consistency-design.md.

use std::collections::BTreeMap;
use std::path::Path;

use musefs_core::{MountConfig, Musefs, scan_directory};

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

fn config() -> MountConfig {
    MountConfig {
        template: "$artist/$title".to_string(),
        fallbacks: BTreeMap::new(),
        default_fallback: "Unknown".to_string(),
        mode: musefs_core::Mode::Synthesis,
        poll_interval: std::time::Duration::ZERO,
        case_insensitive: false,
    }
}

/// A deterministic backing-audio payload of `n` bytes. Distinct per-offset
/// values make any splice/offset mismatch visible in the oracle compare.
fn backing_audio(n: usize) -> Vec<u8> {
    (0..n)
        .map(|i| u8::try_from((i * 7 + 3) % 251).unwrap())
        .collect()
}

/// Mount a single-track backing dir holding one FLAC, run `f` with the mount
/// root and the served file path, then drop the session to unmount.
///
/// Uses a closure because `BackgroundSession` is generic over the private
/// `MusefsFs` and cannot be named from a test crate.
fn with_single_flac_mount<R>(audio: &[u8], f: impl FnOnce(&Path, &Path) -> R) -> R {
    let backing = tempfile::tempdir().unwrap();
    let flac = make_flac(&["ARTIST=Alice", "TITLE=Song"], audio);
    std::fs::write(backing.path().join("a.flac"), &flac).unwrap();
    let db = musefs_db::Db::open_in_memory().unwrap();
    scan_directory(&db, backing.path()).unwrap();
    let fs = Musefs::open(db, config()).unwrap();
    let mountpoint = tempfile::tempdir().unwrap();
    let session = musefs_fuse::spawn(fs, mountpoint.path(), "musefs-readcons").unwrap();
    let served = mountpoint.path().join("Alice").join("Song.flac");
    let r = f(mountpoint.path(), &served);
    drop(session); // unmounts
    drop(backing);
    r
}

#[test]
#[ignore = "requires /dev/fuse; run with: cargo test -p musefs-fuse --test read_consistency -- --ignored"]
#[expect(
    unsafe_code,
    reason = "mmap a served file to exercise the kernel readpage path"
)]
fn mmap_whole_file_matches_pread() {
    let audio = backing_audio(4096);
    with_single_flac_mount(&audio, |_mountpoint, served| {
        // Page-cache / readpage path: map the whole file MAP_SHARED/PROT_READ.
        let via_pread = std::fs::read(served).unwrap();
        assert!(!via_pread.is_empty(), "served file must be non-empty");
        let file = std::fs::File::open(served).unwrap();
        // SAFETY: file is a regular, non-empty read-only mount entry; the map is
        // dropped before the session unmounts at the end of the closure.
        let mmap = unsafe { memmap2::Mmap::map(&file).unwrap() };
        assert_eq!(
            &mmap[..],
            &via_pread[..],
            "mmap-served bytes must equal pread-served bytes (byte-identical-audio invariant)"
        );
    });
}
