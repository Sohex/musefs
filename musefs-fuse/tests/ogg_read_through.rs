use std::collections::BTreeMap;

use musefs_core::{scan_directory, MountConfig, Musefs};

/// Encode a tiny tagged fixture with ffmpeg. `args` are the codec-specific ffmpeg
/// args (codec + container). Returns false (skip) if ffmpeg/codec is unavailable.
fn make_fixture(path: &std::path::Path, codec_args: &[&str]) -> bool {
    let mut cmd = std::process::Command::new("ffmpeg");
    cmd.args([
        "-f",
        "lavfi",
        "-i",
        "anullsrc=r=48000:cl=stereo",
        "-t",
        "0.3",
    ]);
    cmd.args(codec_args);
    cmd.args([
        "-metadata",
        "title=Roygbiv",
        "-metadata",
        "artist=Boards",
        "-y",
    ]);
    cmd.arg(path)
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

/// Mount a single-track backing dir containing `src`, read the one synthesized
/// file back, and validate: all page CRCs valid (read_packets), the comment packet
/// carries the rewritten title, and the AUDIO packets are byte-identical to the
/// source. The number of header packets is derived from the source via
/// `read_header`, so the audio-packet suffix comparison is codec-agnostic (OggFLAC
/// header packet count varies).
fn mount_and_validate(src: &std::path::Path) {
    let source_bytes = std::fs::read(src).unwrap();
    let backing = src.parent().unwrap();

    let db = musefs_db::Db::open_in_memory().unwrap();
    scan_directory(&db, backing).unwrap();
    let fs = Musefs::open(db, config()).unwrap();

    let mountpoint = tempfile::tempdir().unwrap();
    let session = musefs_fuse::spawn(fs, mountpoint.path(), "musefs-ogg-test").unwrap();

    let mounted_path = find_one_file(mountpoint.path());
    let mounted = std::fs::read(&mounted_path).unwrap();

    let mp = read_packets(&mounted); // panics on any bad page CRC
    let sp = read_packets(&source_bytes);

    // Re-tagged: the rewritten title appears somewhere in the regenerated header.
    assert!(
        mp.iter().any(|p| p
            .windows(b"TITLE=Roygbiv".len())
            .any(|w| w == b"TITLE=Roygbiv")),
        "synthesized header should carry the rewritten title"
    );

    // Audio packets byte-identical. Header packet count can differ between source
    // and synthesized (e.g. OggFLAC drops PADDING/PICTURE), but the audio packets
    // are a byte-identical suffix of equal length. Derive the source header count
    // from read_header and compare the trailing audio packets.
    let src_header = musefs_format::ogg::read_header(&source_bytes)
        .unwrap()
        .packets
        .len();
    let n_audio = sp.len() - src_header;
    assert!(n_audio > 0, "expected at least one audio packet");
    assert_eq!(
        &mp[mp.len() - n_audio..],
        &sp[sp.len() - n_audio..],
        "audio packets must be byte-identical"
    );

    drop(session);
}

#[test]
#[ignore = "requires /dev/fuse + libfuse + ffmpeg; run with --ignored"]
fn opus_read_through_validates_pages_and_audio() {
    let backing = tempfile::tempdir().unwrap();
    let src = backing.path().join("in.opus");
    if !make_fixture(&src, &["-c:a", "libopus"]) {
        eprintln!("ffmpeg/libopus unavailable; skipping");
        return;
    }
    mount_and_validate(&src);
}

#[test]
#[ignore = "requires /dev/fuse + libfuse + ffmpeg; run with --ignored"]
fn vorbis_read_through_validates_pages_and_audio() {
    let backing = tempfile::tempdir().unwrap();
    let src = backing.path().join("in.ogg");
    if !make_fixture(&src, &["-c:a", "libvorbis"]) {
        eprintln!("ffmpeg/libvorbis unavailable; skipping");
        return;
    }
    mount_and_validate(&src);
}

#[test]
#[ignore = "requires /dev/fuse + libfuse + ffmpeg; run with --ignored"]
fn oggflac_read_through_validates_pages_and_audio() {
    let backing = tempfile::tempdir().unwrap();
    let src = backing.path().join("in.oga");
    // FLAC-in-Ogg: flac codec in the ogg container.
    if !make_fixture(&src, &["-c:a", "flac", "-f", "ogg"]) {
        eprintln!("ffmpeg/flac-in-ogg unavailable; skipping");
        return;
    }
    mount_and_validate(&src);
}
