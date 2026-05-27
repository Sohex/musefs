use std::collections::BTreeMap;

use musefs_core::{scan_directory, MountConfig, Musefs};

// --- minimal proven FLAC fixture (mirrors musefs-core/tests/common) ---

fn flac_block(block_type: u8, body: &[u8], is_last: bool) -> Vec<u8> {
    let mut out = Vec::new();
    out.push((if is_last { 0x80 } else { 0 }) | (block_type & 0x7F));
    let len = body.len();
    out.push(((len >> 16) & 0xFF) as u8);
    out.push(((len >> 8) & 0xFF) as u8);
    out.push((len & 0xFF) as u8);
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
    out.extend_from_slice(&(vendor.len() as u32).to_le_bytes());
    out.extend_from_slice(vendor.as_bytes());
    out.extend_from_slice(&(comments.len() as u32).to_le_bytes());
    for c in comments {
        out.extend_from_slice(&(c.len() as u32).to_le_bytes());
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
    }
}

#[test]
#[ignore = "requires /dev/fuse; run with: cargo test -p musefs-fuse -- --ignored"]
fn end_to_end_read_through_mount() {
    // Build backing dir + scanned DB + Musefs.
    let backing = tempfile::tempdir().unwrap();
    let flac = make_flac(&["ARTIST=Alice", "TITLE=Song"], &[0xAB; 64]);
    std::fs::write(backing.path().join("a.flac"), &flac).unwrap();
    let db = musefs_db::Db::open_in_memory().unwrap();
    scan_directory(&db, backing.path()).unwrap();
    let fs = Musefs::open(db, config()).unwrap();

    // Mount it in the background.
    let mountpoint = tempfile::tempdir().unwrap();
    let session = musefs_fuse::spawn(fs, mountpoint.path(), "musefs-test").unwrap();

    // Read /Alice/Song.flac through the mount and decode it independently.
    let song = mountpoint.path().join("Alice").join("Song.flac");
    let bytes = std::fs::read(&song).unwrap();
    let tag = metaflac::Tag::read_from(&mut std::io::Cursor::new(&bytes)).unwrap();
    assert_eq!(
        tag.vorbis_comments()
            .unwrap()
            .get("TITLE")
            .map(std::vec::Vec::as_slice),
        Some(["Song".to_string()].as_slice())
    );

    // readdir through the mount.
    let mut names: Vec<String> = std::fs::read_dir(mountpoint.path().join("Alice"))
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    assert_eq!(names, vec!["Song.flac".to_string()]);

    drop(session); // unmounts
    drop(backing);
}

/// Minimal valid RIFF/WAVE: PCM 16-bit mono `fmt `, a `LIST`/`INFO` chunk
/// (IART=artist, INAM=title) so the scanner seeds tags, then `data`.
fn make_wav(artist: &str, title: &str, audio: &[u8]) -> Vec<u8> {
    let mut fmt = Vec::new();
    fmt.extend_from_slice(&1u16.to_le_bytes());
    fmt.extend_from_slice(&1u16.to_le_bytes());
    fmt.extend_from_slice(&44_100u32.to_le_bytes());
    fmt.extend_from_slice(&88_200u32.to_le_bytes());
    fmt.extend_from_slice(&2u16.to_le_bytes());
    fmt.extend_from_slice(&16u16.to_le_bytes());

    let mut info = b"INFO".to_vec();
    for (cc, val) in [(&b"IART"[..], artist), (&b"INAM"[..], title)] {
        let mut v = val.as_bytes().to_vec();
        v.push(0x00);
        info.extend_from_slice(cc);
        info.extend_from_slice(&(v.len() as u32).to_le_bytes());
        info.extend_from_slice(&v);
        if v.len() % 2 == 1 {
            info.push(0x00);
        }
    }

    let mut body = Vec::new();
    for (id, payload) in [
        (&b"fmt "[..], &fmt[..]),
        (&b"LIST"[..], &info[..]),
        (&b"data"[..], audio),
    ] {
        body.extend_from_slice(id);
        body.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        body.extend_from_slice(payload);
        if payload.len() % 2 == 1 {
            body.push(0x00);
        }
    }
    let mut out = b"RIFF".to_vec();
    out.extend_from_slice(&((body.len() + 4) as u32).to_le_bytes());
    out.extend_from_slice(b"WAVE");
    out.extend_from_slice(&body);
    out
}

#[test]
#[ignore = "requires /dev/fuse; run with: cargo test -p musefs-fuse -- --ignored"]
fn end_to_end_read_through_mount_wav() {
    // Build backing dir + scanned DB + Musefs.
    let backing = tempfile::tempdir().unwrap();
    let audio: Vec<u8> = (0..64u8).collect();
    let wav = make_wav("Alice", "Song", &audio);
    std::fs::write(backing.path().join("a.wav"), &wav).unwrap();
    let db = musefs_db::Db::open_in_memory().unwrap();
    scan_directory(&db, backing.path()).unwrap();
    let fs = Musefs::open(db, config()).unwrap();

    // Mount it in the background.
    let mountpoint = tempfile::tempdir().unwrap();
    let session = musefs_fuse::spawn(fs, mountpoint.path(), "musefs-test-wav").unwrap();

    // Read /Alice/Song.wav through the mount.
    let song = mountpoint.path().join("Alice").join("Song.wav");
    let bytes = std::fs::read(&song).unwrap();

    // It is a valid WAV and the data payload is the original audio byte-for-byte.
    assert_eq!(&bytes[0..4], b"RIFF");
    assert_eq!(&bytes[8..12], b"WAVE");
    let bounds = musefs_format::wav::locate_audio(&bytes).unwrap();
    assert_eq!(
        &bytes[bounds.audio_offset as usize..(bounds.audio_offset + bounds.audio_length) as usize],
        audio.as_slice()
    );

    // The title round-trips through the synthesized metadata.
    let tags = musefs_format::wav::read_tags(&bytes);
    assert!(tags.contains(&("title".to_string(), "Song".to_string())));

    // readdir through the mount.
    let mut names: Vec<String> = std::fs::read_dir(mountpoint.path().join("Alice"))
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    assert_eq!(names, vec!["Song.wav".to_string()]);

    drop(session); // unmounts
    drop(backing);
}

#[test]
#[ignore = "requires /dev/fuse; run with: cargo test -p musefs-fuse -- --ignored"]
fn concurrent_spawns_do_not_race() {
    use std::sync::{Arc, Barrier};

    // Many threads mount at once, maximizing overlap of the fusermount3 fd-passing
    // handshake. That handshake forks/execs and is not safe to run concurrently
    // from one process (it races the fd table: "fd N is not a socket"), so without
    // serialization at least one spawn fails intermittently. A clean run proves the
    // handshake is serialized.
    let n = 8;
    let barrier = Arc::new(Barrier::new(n));
    let handles: Vec<_> = (0..n)
        .map(|i| {
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                let backing = tempfile::tempdir().unwrap();
                let flac = make_flac(&["ARTIST=Alice", "TITLE=Song"], &[0xAB; 64]);
                std::fs::write(backing.path().join("a.flac"), &flac).unwrap();
                let db = musefs_db::Db::open_in_memory().unwrap();
                scan_directory(&db, backing.path()).unwrap();
                let fs = Musefs::open(db, config()).unwrap();
                let mountpoint = tempfile::tempdir().unwrap();

                barrier.wait(); // release all threads into spawn() together
                let session = musefs_fuse::spawn(fs, mountpoint.path(), "musefs-race")
                    .unwrap_or_else(|e| panic!("mount {i} raced: {e}"));
                drop(session); // unmounts
                drop(backing);
            })
        })
        .collect();
    for h in handles {
        h.join().expect("a concurrent mount thread panicked (race)");
    }
}
