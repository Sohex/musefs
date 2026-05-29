use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use musefs_core::{scan_directory, MountConfig, Musefs};
use sha2::{Digest, Sha256};

#[derive(Clone, Copy)]
struct PlaybackCase {
    source_name: &'static str,
    served_ext: &'static str,
    title: &'static str,
    artist: &'static str,
    freq: u32,
    codec_args: &'static [&'static str],
}

fn config() -> MountConfig {
    MountConfig {
        template: "$artist/$title".to_string(),
        fallbacks: BTreeMap::new(),
        default_fallback: "Unknown".to_string(),
        mode: musefs_core::Mode::Synthesis,
        poll_interval: std::time::Duration::ZERO,
    }
}

fn playback_cases() -> Vec<PlaybackCase> {
    vec![
        PlaybackCase {
            source_name: "flac.flac",
            served_ext: "flac",
            title: "PCM FLAC",
            artist: "PCM Artist",
            freq: 330,
            codec_args: &["-c:a", "flac"],
        },
        PlaybackCase {
            source_name: "mp3.mp3",
            served_ext: "mp3",
            title: "PCM MP3",
            artist: "PCM Artist",
            freq: 440,
            codec_args: &["-c:a", "libmp3lame", "-q:a", "5"],
        },
        PlaybackCase {
            source_name: "m4a.m4a",
            served_ext: "m4a",
            title: "PCM M4A",
            artist: "PCM Artist",
            freq: 550,
            codec_args: &["-c:a", "aac", "-b:a", "64k"],
        },
        PlaybackCase {
            source_name: "opus.opus",
            served_ext: "opus",
            title: "PCM Opus",
            artist: "PCM Artist",
            freq: 660,
            codec_args: &["-c:a", "libopus"],
        },
        PlaybackCase {
            source_name: "vorbis.ogg",
            served_ext: "vorbis",
            title: "PCM Vorbis",
            artist: "PCM Artist",
            freq: 770,
            codec_args: &["-c:a", "libvorbis"],
        },
        PlaybackCase {
            source_name: "oggflac.oga",
            served_ext: "oggflac",
            title: "PCM OggFLAC",
            artist: "PCM Artist",
            freq: 880,
            codec_args: &["-c:a", "flac", "-f", "ogg"],
        },
        PlaybackCase {
            source_name: "wav.wav",
            served_ext: "wav",
            title: "PCM WAV",
            artist: "PCM Artist",
            freq: 990,
            codec_args: &["-c:a", "pcm_s16le"],
        },
    ]
}

fn make_audio_fixture(path: &Path, case: PlaybackCase) -> bool {
    let mut cmd = Command::new("ffmpeg");
    let input = format!(
        "sine=frequency={}:duration=0.4:sample_rate=48000",
        case.freq
    );
    let title = format!("title={}", case.title);
    let artist = format!("artist={}", case.artist);
    cmd.args([
        "-hide_banner",
        "-loglevel",
        "error",
        "-y",
        "-f",
        "lavfi",
        "-i",
        input.as_str(),
    ]);
    cmd.args(case.codec_args);
    cmd.args(["-metadata", title.as_str(), "-metadata", artist.as_str()]);
    cmd.arg(path)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
        && path.exists()
}

fn pcm_sha256(path: &Path) -> String {
    let output = Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error"])
        .arg("-i")
        .arg(path)
        .args([
            "-map",
            "0:a:0",
            "-f",
            "s16le",
            "-acodec",
            "pcm_s16le",
            "-ac",
            "2",
            "-ar",
            "48000",
            "-",
        ])
        .output()
        .unwrap_or_else(|err| panic!("failed to run ffmpeg for {}: {err}", path.display()));
    assert!(
        output.status.success(),
        "ffmpeg decode failed for {}: {}",
        path.display(),
        String::from_utf8_lossy(&output.stderr)
    );
    let digest = Sha256::digest(&output.stdout);
    let mut s = String::with_capacity(64);
    for b in digest {
        use std::fmt::Write;
        let _ = write!(s, "{b:02x}");
    }
    s
}

fn mounted_path(mountpoint: &Path, case: PlaybackCase) -> PathBuf {
    mountpoint
        .join(case.artist)
        .join(format!("{}.{}", case.title, case.served_ext))
}

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
#[ignore = "requires /dev/fuse + libfuse + ffmpeg; run with --ignored"]
fn all_supported_formats_decode_to_same_pcm_sha_as_source() {
    if Command::new("ffmpeg")
        .arg("-version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_or(true, |status| !status.success())
    {
        eprintln!("ffmpeg unavailable; skipping playback PCM E2E");
        return;
    }

    let backing = tempfile::tempdir().unwrap();
    let mut generated = Vec::new();
    for case in playback_cases() {
        let src = backing.path().join(case.source_name);
        if make_audio_fixture(&src, case) {
            generated.push((case, src));
        } else {
            eprintln!(
                "ffmpeg codec/container unavailable for {}; skipping",
                case.source_name
            );
        }
    }

    assert!(
        !generated.is_empty(),
        "no playback fixtures could be generated; ffmpeg codecs may be unavailable"
    );

    let db = musefs_db::Db::open_in_memory().unwrap();
    scan_directory(&db, backing.path()).unwrap();
    let fs = Musefs::open(db, config()).unwrap();

    let mountpoint = tempfile::tempdir().unwrap();
    let session = musefs_fuse::spawn(fs, mountpoint.path(), "musefs-playback-pcm").unwrap();

    for (case, src) in generated {
        let mounted = mounted_path(mountpoint.path(), case);
        assert!(
            mounted.exists(),
            "expected mounted path {} to exist; tree entries: {:?}",
            mounted.display(),
            walk_tree(mountpoint.path()),
        );
        assert_eq!(
            pcm_sha256(&mounted),
            pcm_sha256(&src),
            "{} should decode to the same canonical PCM as {}",
            mounted.display(),
            src.display()
        );
    }

    drop(session);
}
