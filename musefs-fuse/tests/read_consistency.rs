//! Mount-boundary read-consistency, mmap fidelity, and read-only refusal e2e.
//!
//! Exercises the kernel-facing read path of a live mount: randomized
//! pread/mmap reads compared against an in-memory oracle, whole-file mmap
//! fidelity, and that every mutating op is refused on the read-only mount.
//! See docs/superpowers/specs/2026-06-10-mount-read-consistency-design.md.

use std::collections::BTreeMap;
use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use base64::Engine as _;
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

// --- deterministic, dependency-free PRNG for reproducible randomized reads ---

const SEED: u64 = 0x9E37_79B9_7F4A_7C15;

struct XorShift64(u64);

impl XorShift64 {
    fn new(seed: u64) -> Self {
        Self(seed)
    }
    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
    /// Uniform-ish value in `0..bound` (0 when `bound == 0`).
    fn below(&mut self, bound: u64) -> u64 {
        if bound == 0 {
            0
        } else {
            self.next_u64() % bound
        }
    }
}

/// Read `served` fully as the oracle, then fire `iters` seeded `(offset, len)`
/// reads at the live mount via both `pread` and `mmap`, asserting every in-bounds
/// byte matches the oracle and that reads starting past EOF return 0.
///
/// `seam`, when known (hermetic FLAC), injects the synthesized/`BackingAudio`
/// boundary and its neighbours into the offset set so the splice point is
/// straddled every run. `read_exact_at` tolerates kernel mid-file short reads
/// while still proving byte-fidelity at each offset/len.
#[expect(
    unsafe_code,
    reason = "mmap the served file to compare the readpage path against pread"
)]
#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_possible_wrap,
    reason = "bounded offset/length arithmetic over a small in-test file (n fits usize)"
)]
fn sweep_reads(served: &Path, seam: Option<u64>, iters: u32) {
    let oracle = std::fs::read(served).unwrap();
    let n = oracle.len() as u64;
    assert!(n > 0, "served file must be non-empty: {}", served.display());

    let file = std::fs::File::open(served).unwrap();
    // SAFETY: regular read-only file; map outlives no unmount within this fn.
    let mmap = unsafe { memmap2::Mmap::map(&file).unwrap() };
    assert_eq!(mmap.len() as u64, n, "mmap length must equal file length");

    let mut fixed: Vec<u64> = vec![0, 1, n.saturating_sub(1), n, n + 1];
    if let Some(s) = seam {
        for d in [0i64, -1, 1, -8, 8] {
            let o = s as i64 + d;
            if o >= 0 {
                fixed.push(o as u64);
            }
        }
    }

    let mut rng = XorShift64::new(SEED);
    for i in 0..iters {
        let offset = if (i as usize) < fixed.len() {
            fixed[i as usize]
        } else {
            rng.below(n + 2) // samples 0..=n+1, so past-EOF starts are covered
        };
        let len = match rng.below(4) {
            0 => 0,
            1 => 1,
            2 => rng.below(n + 2),
            // a read that starts in range but crosses EOF
            _ => (n + 1).saturating_sub(offset.min(n)) + rng.below(4),
        };

        let start = offset.min(n) as usize;
        let avail = (n - start as u64).min(len) as usize;

        // pread fidelity: read exactly the in-bounds portion and compare.
        let mut buf = vec![0u8; avail];
        file.read_exact_at(&mut buf, offset).unwrap_or_else(|e| {
            panic!(
                "read_exact_at failed: SEED={SEED:#x} offset={offset} len={len} avail={avail} \
                 n={n} file={}: {e}",
                served.display()
            )
        });
        assert_eq!(
            &buf[..],
            &oracle[start..start + avail],
            "pread bytes mismatch: SEED={SEED:#x} offset={offset} len={len} file={}",
            served.display()
        );

        if offset >= n {
            // A read that starts at/after EOF must return 0 bytes.
            let mut one = [0u8; 1];
            let got = file.read_at(&mut one, offset).unwrap();
            assert_eq!(
                got,
                0,
                "read starting past EOF must return 0: SEED={SEED:#x} offset={offset} n={n} file={}",
                served.display()
            );
        } else {
            // mmap fidelity for the in-bounds slice.
            let o = offset as usize;
            assert_eq!(
                &mmap[o..o + avail],
                &oracle[o..o + avail],
                "mmap bytes mismatch: SEED={SEED:#x} offset={offset} len={len} file={}",
                served.display()
            );
        }
    }
}

#[test]
#[ignore = "requires /dev/fuse; run with: cargo test -p musefs-fuse --test read_consistency -- --ignored"]
fn randomized_reads_match_oracle_flac() {
    // Sizable backing audio so the synthesized/BackingAudio seam sits well inside
    // the file, not at an edge.
    let audio = backing_audio(8192);
    let audio_len = audio.len() as u64;
    with_single_flac_mount(&audio, |_mountpoint, served| {
        let n = std::fs::metadata(served).unwrap().len();
        // The served FLAC is [synth metadata][original audio]; the trailing
        // `audio_len` bytes are the BackingAudio segment, so the splice seam is
        // at n - audio_len.
        let seam = n - audio_len;
        sweep_reads(served, Some(seam), 2000);
    });
}

fn cstr(p: &Path) -> CString {
    CString::new(p.as_os_str().as_bytes()).unwrap()
}

fn last_errno() -> i32 {
    std::io::Error::last_os_error().raw_os_error().unwrap()
}

/// Assert a libc mutating call failed (`ret == -1`) with an errno in `accepted`.
/// The contract is "mutation is refused", not "refused with exactly EROFS".
fn assert_refused(ret: i32, accepted: &[i32], what: &str) {
    assert_eq!(
        ret, -1,
        "{what} unexpectedly succeeded on a read-only mount"
    );
    let e = last_errno();
    assert!(
        accepted.contains(&e),
        "{what}: errno {e} not in accepted set {accepted:?}"
    );
}

#[test]
#[ignore = "requires /dev/fuse; run with: cargo test -p musefs-fuse --test read_consistency -- --ignored"]
#[expect(
    unsafe_code,
    reason = "raw libc mutation syscalls to probe read-only refusal at the mount"
)]
fn write_ops_are_refused_on_read_only_mount() {
    let audio = backing_audio(1024);
    with_single_flac_mount(&audio, |mountpoint, served| {
        let existing = cstr(served);
        let new_file = cstr(&mountpoint.join("Alice").join("new.flac"));
        let new_dir = cstr(&mountpoint.join("Alice").join("newdir"));
        let times = [libc::timeval {
            tv_sec: 0,
            tv_usec: 0,
        }; 2];

        // SAFETY: all paths are valid CStrings; fds are closed below.
        unsafe {
            assert_refused(
                libc::open(existing.as_ptr(), libc::O_WRONLY),
                &[libc::EROFS],
                "open(O_WRONLY)",
            );
            assert_refused(
                libc::open(existing.as_ptr(), libc::O_RDWR),
                &[libc::EROFS],
                "open(O_RDWR)",
            );
            assert_refused(
                libc::open(new_file.as_ptr(), libc::O_WRONLY | libc::O_CREAT, 0o644),
                &[libc::EROFS],
                "open(O_CREAT) new path",
            );
            assert_refused(libc::unlink(existing.as_ptr()), &[libc::EROFS], "unlink");
            assert_refused(
                libc::truncate(existing.as_ptr(), 0),
                &[libc::EROFS],
                "truncate",
            );

            // ftruncate on a read-only fd: EINVAL (fd not writable) is checked
            // independently of the RO mount, so accept both.
            let rofd = libc::open(existing.as_ptr(), libc::O_RDONLY);
            assert!(rofd >= 0, "opening the served file O_RDONLY should succeed");
            assert_refused(
                libc::ftruncate(rofd, 0),
                &[libc::EINVAL, libc::EROFS],
                "ftruncate",
            );
            libc::close(rofd);

            assert_refused(
                libc::mkdir(new_dir.as_ptr(), 0o755),
                &[libc::EROFS],
                "mkdir",
            );
            assert_refused(
                libc::chmod(existing.as_ptr(), 0o644),
                &[libc::EROFS, libc::EPERM],
                "chmod",
            );
            assert_refused(
                libc::utimes(existing.as_ptr(), times.as_ptr()),
                &[libc::EROFS, libc::EPERM, libc::EACCES],
                "utimes",
            );
        }
    });
}

/// How a fixture embeds cover art. ffmpeg's Ogg muxer rejects a mapped
/// `attached_pic` stream ("codec none"), so Ogg formats carry art via a
/// `METADATA_BLOCK_PICTURE` comment tag instead; the others use an
/// attached-picture video stream.
#[derive(Clone, Copy, PartialEq)]
enum Art {
    AttachedPic,
    MetadataBlock,
    None,
}

struct SweepCase {
    /// Backing filename; also seeds a distinct virtual title via `title`.
    name: &'static str,
    title: &'static str,
    codec_args: &'static [&'static str],
    /// How cover art is embedded (drives the ArtImage/BinaryTag/OggArtSlice paths).
    art: Art,
}

fn sweep_cases() -> &'static [SweepCase] {
    &[
        SweepCase {
            name: "flac.flac",
            title: "SweepFlac",
            codec_args: &["-c:a", "flac"],
            art: Art::AttachedPic,
        },
        SweepCase {
            name: "mp3.mp3",
            title: "SweepMp3",
            codec_args: &["-c:a", "libmp3lame", "-q:a", "5"],
            art: Art::AttachedPic,
        },
        SweepCase {
            name: "m4a.m4a",
            title: "SweepM4a",
            codec_args: &["-c:a", "aac", "-b:a", "64k"],
            art: Art::AttachedPic,
        },
        SweepCase {
            name: "opus.opus",
            title: "SweepOpus",
            codec_args: &["-c:a", "libopus"],
            art: Art::MetadataBlock,
        },
        SweepCase {
            name: "vorbis.ogg",
            title: "SweepVorbis",
            codec_args: &["-c:a", "libvorbis"],
            art: Art::MetadataBlock,
        },
        SweepCase {
            // OggFLAC carries art as a native PICTURE packet, which ffmpeg won't
            // write into an Ogg container (attached_pic fails; the
            // METADATA_BLOCK_PICTURE comment is read only for Opus/Vorbis). It is
            // still swept for read consistency, just without an art segment.
            name: "oggflac.oga",
            title: "SweepOggFlac",
            codec_args: &["-c:a", "flac", "-f", "ogg"],
            art: Art::None,
        },
        SweepCase {
            name: "wav.wav",
            title: "SweepWav",
            codec_args: &["-c:a", "pcm_s16le"],
            art: Art::None,
        },
    ]
}

/// A valid 4x4 PNG cover image. ffmpeg 8's PNG decoder rejects malformed chunks,
/// so this must be a real, decodable image.
const COVER_PNG: &[u8] = &[
    0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x04, 0x00, 0x00, 0x00, 0x04, 0x08, 0x02, 0x00, 0x00, 0x00, 0x26, 0x93, 0x09,
    0x29, 0x00, 0x00, 0x00, 0x09, 0x70, 0x48, 0x59, 0x73, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00,
    0x01, 0x00, 0x4F, 0x25, 0xC4, 0xD6, 0x00, 0x00, 0x00, 0x14, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9C,
    0x63, 0x64, 0x60, 0xF8, 0xC7, 0x00, 0x03, 0x2C, 0x0C, 0x48, 0x00, 0x37, 0x07, 0x00, 0x32, 0x3E,
    0x01, 0x0C, 0x1C, 0xDB, 0xAF, 0x41, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42,
    0x60, 0x82,
];

/// Build a FLAC METADATA PICTURE block body (the same structure used verbatim in a
/// FLAC `PICTURE` block and, base64-encoded, in a Vorbis `METADATA_BLOCK_PICTURE`
/// tag): picture type, MIME, description, dimensions, then the image. Big-endian.
fn flac_picture_block(png: &[u8]) -> Vec<u8> {
    let mime: &[u8] = b"image/png";
    let mut out = Vec::new();
    out.extend_from_slice(&3u32.to_be_bytes()); // type: front cover
    out.extend_from_slice(&u32::try_from(mime.len()).unwrap().to_be_bytes());
    out.extend_from_slice(mime);
    out.extend_from_slice(&0u32.to_be_bytes()); // description length (empty)
    out.extend_from_slice(&4u32.to_be_bytes()); // width
    out.extend_from_slice(&4u32.to_be_bytes()); // height
    out.extend_from_slice(&24u32.to_be_bytes()); // color depth
    out.extend_from_slice(&0u32.to_be_bytes()); // colors used (0 = non-indexed)
    out.extend_from_slice(&u32::try_from(png.len()).unwrap().to_be_bytes());
    out.extend_from_slice(png);
    out
}

/// Number of embedded pictures in `path`, via musefs's own readers, for the
/// formats whose art the sweep verifies (FLAC and the Ogg family). `None` for
/// formats without a reader used here (mp3/m4a/wav).
fn embedded_pic_count(path: &Path) -> Option<usize> {
    let bytes = std::fs::read(path).ok()?;
    match path.extension()?.to_str()? {
        "flac" => Some(musefs_format::flac::read_pictures(&bytes).ok()?.len()),
        "opus" | "ogg" | "oga" => Some(musefs_format::ogg::read_pictures(&bytes).ok()?.len()),
        _ => None,
    }
}

/// Encode `case` into `dir` with ffmpeg, embedding cover art per `case.art`.
/// `-t` precedes `-i anullsrc` so it bounds the otherwise-infinite audio input —
/// placing it after leaves the audio unbounded and an attached_pic mux never
/// terminates. Returns `true` on success; `false` if ffmpeg/the codec is missing.
fn make_sweep_fixture(dir: &Path, case: &SweepCase) -> bool {
    let out = dir.join(case.name);
    let title = format!("title={}", case.title);
    let mut cmd = Command::new("ffmpeg");
    cmd.args(["-hide_banner", "-loglevel", "error", "-y"]);
    cmd.args([
        "-t",
        "0.3",
        "-f",
        "lavfi",
        "-i",
        "anullsrc=r=48000:cl=stereo",
    ]);

    match case.art {
        Art::AttachedPic => {
            let cover = dir.join(format!("{}.cover.png", case.name));
            std::fs::write(&cover, COVER_PNG).unwrap();
            cmd.args(["-i"]);
            cmd.arg(&cover);
            cmd.args(["-map", "0:a", "-map", "1:v"]);
            cmd.args(case.codec_args);
            cmd.args(["-metadata", title.as_str(), "-metadata", "artist=Sweep"]);
            // Explicit cover codec: the mp4/ipod muxer's default video-encoder
            // selection fails for attached_pic, so pin it (png works for all three).
            cmd.args(["-c:v", "png", "-disposition:v", "attached_pic"]);
        }
        Art::MetadataBlock => {
            let b64 =
                base64::engine::general_purpose::STANDARD.encode(flac_picture_block(COVER_PNG));
            let mbp = format!("METADATA_BLOCK_PICTURE={b64}");
            cmd.args(case.codec_args);
            cmd.args(["-metadata", title.as_str(), "-metadata", "artist=Sweep"]);
            cmd.args(["-metadata", mbp.as_str()]);
        }
        Art::None => {
            cmd.args(case.codec_args);
            cmd.args(["-metadata", title.as_str(), "-metadata", "artist=Sweep"]);
        }
    }

    cmd.arg(&out).stdout(Stdio::null()).stderr(Stdio::null());
    cmd.status().is_ok_and(|s| s.success()) && out.exists()
}

/// All regular files under `dir`, recursively.
fn walk_tree(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let p = entry.path();
            if p.is_dir() {
                out.extend(walk_tree(&p));
            } else {
                out.push(p);
            }
        }
    }
    out
}

#[test]
#[ignore = "requires /dev/fuse + libfuse + ffmpeg; run with: cargo test -p musefs-fuse --test read_consistency -- --ignored"]
fn randomized_reads_match_oracle_all_formats() {
    if Command::new("ffmpeg")
        .arg("-version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_or(true, |s| !s.success())
    {
        eprintln!("ffmpeg unavailable; skipping multi-format read-consistency sweep");
        return;
    }

    let backing = tempfile::tempdir().unwrap();
    let missing: Vec<&str> = sweep_cases()
        .iter()
        .filter(|case| !make_sweep_fixture(backing.path(), case))
        .map(|case| case.name)
        .collect();
    assert!(
        missing.is_empty(),
        "fixtures failed to generate (ffmpeg codec missing or broken invocation): {missing:?}"
    );

    // The cover art must actually be embedded, or the ArtImage/OggArtSlice splice
    // segments this sweep exists to exercise would silently be absent. Verify it
    // with musefs's own readers for the formats they cover.
    for case in sweep_cases().iter().filter(|c| c.art != Art::None) {
        if let Some(pics) = embedded_pic_count(&backing.path().join(case.name)) {
            assert!(pics > 0, "{} should carry embedded cover art", case.name);
        }
    }

    let db = musefs_db::Db::open_in_memory().unwrap();
    scan_directory(&db, backing.path()).unwrap();
    let fs = Musefs::open(db, config()).unwrap();
    let mountpoint = tempfile::tempdir().unwrap();
    let session = musefs_fuse::spawn(fs, mountpoint.path(), "musefs-readcons-sweep").unwrap();

    let served = walk_tree(mountpoint.path());
    assert_eq!(
        served.len(),
        sweep_cases().len(),
        "every generated fixture should be served exactly once: {served:?}"
    );
    for path in &served {
        sweep_reads(path, None, 500);
    }

    drop(session);
    drop(backing);
}
