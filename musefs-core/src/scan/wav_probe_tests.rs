use super::*;
use std::io::Write;

fn build_wav() -> Vec<u8> {
    let mut fmt = Vec::new();
    fmt.extend_from_slice(&1u16.to_le_bytes());
    fmt.extend_from_slice(&1u16.to_le_bytes());
    fmt.extend_from_slice(&44_100u32.to_le_bytes());
    fmt.extend_from_slice(&88_200u32.to_le_bytes());
    fmt.extend_from_slice(&2u16.to_le_bytes());
    fmt.extend_from_slice(&16u16.to_le_bytes());

    let data = vec![0u8; 16];
    let mut body = Vec::new();
    for (id, payload) in [(b"fmt ", &fmt), (b"data", &data)] {
        body.extend_from_slice(id);
        body.extend_from_slice(&u32::try_from(payload.len()).unwrap().to_le_bytes());
        body.extend_from_slice(payload);
    }
    let mut out = b"RIFF".to_vec();
    out.extend_from_slice(&u32::try_from(body.len() + 4).unwrap().to_le_bytes());
    out.extend_from_slice(b"WAVE");
    out.extend_from_slice(&body);
    out
}

#[test]
fn probe_detects_wav() {
    let bytes = build_wav();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("song.wav");
    std::fs::File::create(&path)
        .unwrap()
        .write_all(&bytes)
        .unwrap();

    let probed = probe_full(&path, &bytes).expect("wav should probe");
    assert_eq!(probed.format, Format::Wav);
    assert_eq!(probed.audio_length, 16);
}

/// #770: a big-endian RIFX file is ingested, and served as RIFX. The serve path
/// learns the byte order from the backing file's own front (`read_structure`
/// over `[0, audio_offset)`), so nothing about it is stored.
#[test]
fn rifx_wav_scans_and_serves_as_rifx() {
    use musefs_format::fuzz_check::fixtures;
    use musefs_format::wav::ByteOrder;

    let bytes = fixtures::wav_in(&[0x0102, -2, 300, i16::MIN, i16::MAX, 7], ByteOrder::Big);
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("big.wav");
    std::fs::write(&path, &bytes).unwrap();

    let db = musefs_db::Db::open_in_memory().unwrap();
    let stats = crate::scan_directory(&db, &path).unwrap();
    assert_eq!((stats.scanned, stats.failed), (1, 0));
    let track = db.list_tracks().unwrap().remove(0);
    assert_eq!(track.format, Format::Wav);
    db.replace_tags(track.id, &[musefs_db::Tag::new("title", "Big Endian", 0)])
        .unwrap();

    let resolved = crate::HeaderCache::new(crate::Mode::Synthesis)
        .resolve(&db, track.id)
        .unwrap();
    let out = crate::read_at(&resolved, &db, 0, resolved.total_len).unwrap();

    assert_eq!(&out[0..4], b"RIFX");
    let range = |b: &wav::WavBounds| {
        usize_from(b.audio_offset)..usize_from(b.audio_offset + b.audio_length)
    };
    let source = wav::locate_audio(&bytes).unwrap();
    let served = wav::locate_audio(&out).unwrap();
    assert_eq!(&out[range(&served)], &bytes[range(&source)]);
    assert_eq!(
        wav::read_structure(&out).unwrap(),
        wav::read_structure(&bytes).unwrap()
    );
    assert!(
        wav::read_tags(&out).contains(&("title".to_string(), "Big Endian".to_string())),
        "{:?}",
        wav::read_tags(&out)
    );
}

#[test]
fn scan_single_wav_file_ingests_it() {
    let bytes = build_wav();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("single.wav");
    std::fs::File::create(&path)
        .unwrap()
        .write_all(&bytes)
        .unwrap();

    let db = musefs_db::Db::open_in_memory().unwrap();
    let stats = crate::scan_directory(&db, &path).unwrap();
    assert_eq!(stats.scanned, 1);
    assert_eq!(stats.skipped, 0);
}
