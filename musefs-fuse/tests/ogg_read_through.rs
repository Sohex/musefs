use std::collections::BTreeMap;

use musefs_core::{scan_directory, MountConfig, Musefs};

/// Generate a tiny tagged .opus via ffmpeg. Returns false (skip) if ffmpeg or the
/// libopus encoder is unavailable.
fn make_opus_fixture(path: &std::path::Path) -> bool {
    std::process::Command::new("ffmpeg")
        .args([
            "-f",
            "lavfi",
            "-i",
            "anullsrc=r=48000:cl=stereo",
            "-t",
            "0.2",
            "-c:a",
            "libopus",
            "-metadata",
            "title=Roygbiv",
            "-metadata",
            "artist=Boards",
            "-y",
        ])
        .arg(path)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
        && path.exists()
}

/// Read every Ogg packet's data. The `ogg` crate validates page CRCs while
/// reading, so a corrupt page makes `read_packet` error (panicking the test).
fn read_packets(bytes: &[u8]) -> Vec<Vec<u8>> {
    let mut rdr = ogg::PacketReader::new(std::io::Cursor::new(bytes.to_vec()));
    let mut out = Vec::new();
    while let Some(p) = rdr.read_packet().expect("valid Ogg pages (CRC ok)") {
        out.push(p.data);
    }
    out
}

fn find_one_file(root: &std::path::Path) -> std::path::PathBuf {
    let entry = std::fs::read_dir(root)
        .unwrap()
        .next()
        .expect("non-empty dir")
        .unwrap();
    let p = entry.path();
    if entry.file_type().unwrap().is_dir() {
        find_one_file(&p)
    } else {
        p
    }
}

fn config() -> MountConfig {
    MountConfig {
        template: "$artist/$title".to_string(),
        fallbacks: BTreeMap::new(),
        default_fallback: "Unknown".to_string(),
        mode: musefs_core::Mode::Synthesis,
    }
}

#[test]
#[ignore = "requires /dev/fuse + libfuse + ffmpeg; run with --ignored"]
fn opus_read_through_validates_pages_and_audio() {
    let backing = tempfile::tempdir().unwrap();
    let src = backing.path().join("in.opus");
    if !make_opus_fixture(&src) {
        eprintln!("ffmpeg/libopus unavailable; skipping");
        return;
    }
    let source_bytes = std::fs::read(&src).unwrap();

    let db = musefs_db::Db::open_in_memory().unwrap();
    scan_directory(&db, backing.path()).unwrap();
    let fs = Musefs::open(db, config()).unwrap();

    let mountpoint = tempfile::tempdir().unwrap();
    let session = musefs_fuse::spawn(fs, mountpoint.path(), "musefs-ogg-test").unwrap();

    let mounted_path = find_one_file(mountpoint.path());
    let mounted = std::fs::read(&mounted_path).unwrap();

    // 1. Pages well-formed: read_packets panics on any bad CRC.
    let mp = read_packets(&mounted);
    let sp = read_packets(&source_bytes);

    // 2. Header packets present and re-tagged.
    assert!(mp[0].starts_with(b"OpusHead"));
    assert!(mp[1].starts_with(b"OpusTags"));
    assert!(
        mp[1]
            .windows(b"TITLE=Roygbiv".len())
            .any(|w| w == b"TITLE=Roygbiv"),
        "synthesized OpusTags should carry the rewritten title"
    );

    // 3. Audio packets (codec frames) byte-identical to the source — repagination
    //    changes page framing/sequence numbers only, never the audio packets.
    assert_eq!(mp.len(), sp.len());
    assert_eq!(&mp[2..], &sp[2..]);

    drop(session); // unmounts
    drop(backing);
}
