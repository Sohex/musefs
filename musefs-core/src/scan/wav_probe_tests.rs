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

/// `fmt ` plus a `LIST('wavl')` of `data_len` bytes of `data` and a `slnt`
/// (#769), with no top-level `data` chunk. With `payload` false, only the front
/// up to the inner `data` header is returned, for a caller to extend sparsely.
fn wavl_wav(data_len: u32, payload: bool) -> Vec<u8> {
    let fmt = &build_wav()[12..36]; // the `fmt ` chunk, header included
    let list_len = 4 + 8 + data_len + 8 + 4;
    let mut out = b"RIFF".to_vec();
    out.extend_from_slice(&(4 + 24 + 8 + list_len).to_le_bytes());
    out.extend_from_slice(b"WAVE");
    out.extend_from_slice(fmt);
    out.extend_from_slice(b"LIST");
    out.extend_from_slice(&list_len.to_le_bytes());
    out.extend_from_slice(b"wavl");
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_len.to_le_bytes());
    if payload {
        out.extend(std::iter::repeat_n(0x11u8, usize_from(u64::from(data_len))));
        out.extend_from_slice(b"slnt");
        out.extend_from_slice(&4u32.to_le_bytes());
        out.extend_from_slice(&1_000u32.to_le_bytes());
    }
    out
}

#[test]
fn scan_refuses_a_wavl_waveform_by_name() {
    // #769: no mainstream decoder plays a LIST('wavl') waveform, so the scan
    // refuses it, and tells the operator why rather than calling it unparseable.
    crate::warn_limit::log_capture::install();
    let bytes = wavl_wav(16, true);
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("wavl-scan.wav");
    std::fs::write(&path, &bytes).unwrap();

    assert!(probe_full(&path, &bytes).is_none());
    match probe_file(&path, WINDOW, ChecksumTier::Fingerprint).unwrap() {
        ProbeOutcome::Failed(f) => {
            assert_eq!(f.reason, SkipReason::Unsupported);
            assert!(f.message.contains("LIST('wavl')"), "{}", f.message);
        }
        other => panic!("expected an unsupported refusal, got {other:?}"),
    }

    let db = musefs_db::Db::open_in_memory().unwrap();
    let stats = crate::scan_directory(&db, &path).unwrap();
    assert_eq!((stats.scanned, stats.failed), (0, 1));
    let logged = crate::warn_limit::log_capture::messages_containing("wavl-scan.wav");
    assert_eq!(logged.len(), 1, "one skip, one line: {logged:?}");
    assert!(logged[0].contains("LIST('wavl')"), "{}", logged[0]);
}

#[test]
fn an_oversize_wavl_waveform_is_refused_by_name_too() {
    // Past the probe ceiling the bounded parse never completes and the probe
    // falls back to trusting the front's headers. That fallback must refuse the
    // wavl list by name as well, not call the file unparseable.
    use std::io::Write;
    let data_len = u32::try_from(MAX_PROBE_BYTES + (16 << 20)).unwrap();
    let front = wavl_wav(data_len, false);
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("long-wavl.wav");
    let mut f = std::fs::File::create(&path).unwrap();
    f.write_all(&front).unwrap();
    f.set_len(front.len() as u64 + u64::from(data_len) + 12)
        .unwrap();
    drop(f);

    match probe_file(&path, WINDOW, ChecksumTier::Fingerprint).unwrap() {
        ProbeOutcome::Failed(f) => {
            assert_eq!(f.reason, SkipReason::Unsupported);
            assert!(f.message.contains("LIST('wavl')"), "{}", f.message);
        }
        other => panic!("expected an unsupported refusal, got {other:?}"),
    }
}

/// The past-the-ceiling WAV fallback is for `.wav` files alone. An oversize file
/// under another supported extension can reach that fallback too: the Ogg arm
/// widens on any parse error until the probe cap, so a mislabelled `.ogg` holding
/// RIFF/WAVE bytes gets there. It must stay unparseable, not be ingested as a WAV
/// on the strength of a front it was never named for.
#[test]
fn an_oversize_non_wav_holding_riff_bytes_is_not_taken_for_a_wav() {
    use std::io::Write;
    let data_len = u32::try_from(MAX_PROBE_BYTES + (16 << 20)).unwrap();
    let fmt = &build_wav()[12..36]; // the `fmt ` chunk, header included
    let mut front = b"RIFF".to_vec();
    front.extend_from_slice(&(4 + 24 + 8 + data_len).to_le_bytes());
    front.extend_from_slice(b"WAVE");
    front.extend_from_slice(fmt);
    front.extend_from_slice(b"data");
    front.extend_from_slice(&data_len.to_le_bytes());
    let file_len = front.len() as u64 + u64::from(data_len);

    let dir = tempfile::tempdir().unwrap();
    for name in ["long-riff.ogg", "long-riff.wav"] {
        let path = dir.path().join(name);
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(&front).unwrap();
        f.set_len(file_len).unwrap();
    }

    match probe_file(
        &dir.path().join("long-riff.ogg"),
        WINDOW,
        ChecksumTier::None,
    )
    .unwrap()
    {
        ProbeOutcome::Failed(f) => assert_eq!(f.reason, SkipReason::Unparseable, "{}", f.message),
        other => panic!("a .ogg must not probe as a WAV: {other:?}"),
    }
    // The same bytes named `.wav` are served, so the refusal above is the name's.
    match probe_file(
        &dir.path().join("long-riff.wav"),
        WINDOW,
        ChecksumTier::None,
    )
    .unwrap()
    {
        ProbeOutcome::Probed(p, _, _) => assert_eq!(p.format, Format::Wav),
        other => panic!("expected the .wav to probe, got {other:?}"),
    }
}

/// A stored WAV later rewritten as LIST('wavl') can no longer be refreshed, so
/// it fails every revalidate. Like a stored chained Ogg (#747), `--prune` is
/// what removes its row, and only when asked.
#[test]
fn revalidate_prunes_a_stored_wav_rewritten_as_wavl_only_when_asked() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("rewritten.wav");
    std::fs::write(&path, build_wav()).unwrap();
    let db = musefs_db::Db::open_in_memory().unwrap();
    assert_eq!(crate::scan_directory(&db, dir.path()).unwrap().scanned, 1);

    std::fs::write(&path, wavl_wav(16, true)).unwrap();
    let stats = crate::revalidate(&db, dir.path()).unwrap();
    assert_eq!((stats.failed, stats.pruned), (1, 0));
    assert_eq!(db.list_tracks().unwrap().len(), 1, "kept unasked");

    let opts = ScanOptions {
        prune: true,
        ..ScanOptions::default()
    };
    let stats = crate::revalidate_with(&db, dir.path(), &opts).unwrap();
    assert_eq!((stats.failed, stats.pruned), (1, 1));
    assert!(db.list_tracks().unwrap().is_empty());
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
