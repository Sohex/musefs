use base64::Engine as _;
use musefs_core::{Musefs, scan_directory};

mod common;
use common::{COVER_PNG, config, flac_picture_block};

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
        .is_ok_and(|s| s.success())
        && path.exists()
}

/// Whether the ffmpeg binary is present. Distinguishes a genuinely-absent
/// toolchain (legitimate skip) from a present ffmpeg whose codec/invocation
/// failed to produce a fixture (a real failure to surface, not swallow).
fn ffmpeg_available() -> bool {
    std::process::Command::new("ffmpeg")
        .arg("-version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
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

/// Generate an Ogg fixture carrying a cover image via a base64
/// `METADATA_BLOCK_PICTURE` comment — ffmpeg cannot mux an `attached_pic` stream
/// into an Ogg container, so this is the only route Opus/Vorbis art takes. `-t`
/// precedes `-i anullsrc` to bound the otherwise-infinite audio input. Returns the
/// cover image bytes if encoding succeeded, else None (skip).
fn make_fixture_with_cover(
    dir: &std::path::Path,
    audio_name: &str,
    codec_args: &[&str],
) -> Option<(std::path::PathBuf, Vec<u8>)> {
    let b64 = base64::engine::general_purpose::STANDARD.encode(flac_picture_block(COVER_PNG));
    let mbp = format!("METADATA_BLOCK_PICTURE={b64}");
    let out = dir.join(audio_name);
    let mut cmd = std::process::Command::new("ffmpeg");
    cmd.args([
        "-t",
        "0.3",
        "-f",
        "lavfi",
        "-i",
        "anullsrc=r=48000:cl=stereo",
    ]);
    cmd.args(codec_args);
    cmd.args(["-metadata", "title=Cover", "-metadata", mbp.as_str(), "-y"]);
    cmd.arg(&out)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    let ok = cmd.status().is_ok_and(|s| s.success()) && out.exists();
    if ok {
        Some((out, COVER_PNG.to_vec()))
    } else {
        None
    }
}

#[test]
#[ignore = "requires /dev/fuse + libfuse + ffmpeg; run with --ignored"]
fn opus_read_through_preserves_embedded_art() {
    if !ffmpeg_available() {
        eprintln!("ffmpeg unavailable; skipping");
        return;
    }
    let backing = tempfile::tempdir().unwrap();
    let (src, _cover) = make_fixture_with_cover(backing.path(), "in.opus", &["-c:a", "libopus"])
        .expect("ffmpeg present but the libopus cover fixture failed to generate");

    // The source's own embedded art (as the scanner will ingest it).
    let source_bytes = std::fs::read(&src).unwrap();
    let src_pics = musefs_format::ogg::read_pictures(&source_bytes).unwrap();
    assert!(!src_pics.is_empty(), "fixture should carry a cover");

    let db = musefs_db::Db::open_in_memory().unwrap();
    musefs_core::scan_directory(&db, backing.path()).unwrap();
    let fs = musefs_core::Musefs::open(db, config()).unwrap();
    let mountpoint = tempfile::tempdir().unwrap();
    let session = musefs_fuse::spawn(fs, mountpoint.path(), "musefs-ogg-art").unwrap();

    let mounted = std::fs::read(find_one_file(mountpoint.path())).unwrap();
    // All pages valid (read_packets panics on bad CRC).
    let _ = read_packets(&mounted);
    // The synthesized file carries the same image bytes as the source.
    let out_pics = musefs_format::ogg::read_pictures(&mounted).unwrap();
    assert_eq!(out_pics.len(), 1);
    assert_eq!(out_pics[0].data, src_pics[0].data);

    drop(session);
}

#[test]
#[ignore = "requires /dev/fuse + libfuse + ffmpeg; run with --ignored"]
fn opus_read_through_validates_pages_and_audio() {
    if !ffmpeg_available() {
        eprintln!("ffmpeg unavailable; skipping");
        return;
    }
    let backing = tempfile::tempdir().unwrap();
    let src = backing.path().join("in.opus");
    assert!(
        make_fixture(&src, &["-c:a", "libopus"]),
        "ffmpeg present but the libopus fixture failed to generate"
    );
    mount_and_validate(&src);
}

#[test]
#[ignore = "requires /dev/fuse + libfuse + ffmpeg; run with --ignored"]
fn vorbis_read_through_validates_pages_and_audio() {
    if !ffmpeg_available() {
        eprintln!("ffmpeg unavailable; skipping");
        return;
    }
    let backing = tempfile::tempdir().unwrap();
    let src = backing.path().join("in.ogg");
    assert!(
        make_fixture(&src, &["-c:a", "libvorbis"]),
        "ffmpeg present but the libvorbis fixture failed to generate"
    );
    mount_and_validate(&src);
}

#[test]
#[ignore = "requires /dev/fuse + libfuse + ffmpeg; run with --ignored"]
fn oggflac_read_through_validates_pages_and_audio() {
    if !ffmpeg_available() {
        eprintln!("ffmpeg unavailable; skipping");
        return;
    }
    let backing = tempfile::tempdir().unwrap();
    let src = backing.path().join("in.oga");
    // FLAC-in-Ogg: flac codec in the ogg container.
    assert!(
        make_fixture(&src, &["-c:a", "flac", "-f", "ogg"]),
        "ffmpeg present but the flac-in-ogg fixture failed to generate"
    );
    mount_and_validate(&src);
}
