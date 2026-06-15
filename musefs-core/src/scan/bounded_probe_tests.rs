use super::*;
use musefs_db::Db;

/// Minimal FLAC: marker + a single last STREAMINFO (34-byte body) + audio.
/// FLAC has no frame-sync check at the audio offset, so any payload works.
fn flac_fixture() -> Vec<u8> {
    let mut bytes = b"fLaC".to_vec();
    bytes.push(0x80); // last-block flag set, type 0 (STREAMINFO)
    bytes.extend_from_slice(&[0, 0, 34]); // 24-bit length = 34
    bytes.extend(std::iter::repeat_n(0u8, 34));
    bytes.extend_from_slice(b"AUDIOPAYLOAD");
    bytes
}

#[test]
fn scan_counts_unreadable_file_as_failed_and_continues() {
    let dir = tempfile::tempdir().unwrap();
    // One good FLAC + one zero-byte ".flac" that cannot parse.
    let good = dir.path().join("good.flac");
    let mut bytes = b"fLaC".to_vec();
    bytes.push(0x80);
    bytes.extend_from_slice(&[0, 0, 34]);
    bytes.extend(std::iter::repeat_n(0u8, 34));
    bytes.extend_from_slice(b"AUDIO");
    std::fs::write(&good, &bytes).unwrap();
    std::fs::write(dir.path().join("bad.flac"), b"").unwrap();

    let db = Db::open_in_memory().unwrap();
    let stats = scan_directory(&db, dir.path()).unwrap();
    assert_eq!(stats.scanned, 1);
    assert_eq!(stats.skipped + stats.failed, 1);
}

#[test]
fn scan_directory_bounded_matches_full_for_flac() {
    // A FLAC fixture written to a temp dir, scanned with the (default) bounded
    // path, yields a track with the same audio bounds as a full-file probe.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.flac");
    let bytes = flac_fixture();
    std::fs::write(&path, &bytes).unwrap();

    let full = probe_full(&path, &bytes).expect("full probe");

    let db = Db::open_in_memory().unwrap();
    let stats = scan_directory(&db, dir.path()).unwrap();
    assert_eq!(stats.scanned, 1);
    let track = db
        .get_track_by_path(&std::fs::canonicalize(&path).unwrap().to_string_lossy())
        .unwrap()
        .unwrap();
    assert_eq!(track.bounds.audio_offset(), full.audio_offset);
    assert_eq!(track.bounds.audio_length(), full.audio_length);
}

#[test]
fn revalidate_skips_unchanged_and_reprobes_changed() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("x.flac");
    let mk = |audio: &[u8]| {
        let mut b = b"fLaC".to_vec();
        b.push(0x80);
        b.extend_from_slice(&[0, 0, 34]);
        b.extend(std::iter::repeat_n(0u8, 34));
        b.extend_from_slice(audio);
        b
    };
    std::fs::write(&p, mk(b"AUDIO")).unwrap();
    let db = Db::open_in_memory().unwrap();
    scan_directory(&db, dir.path()).unwrap();

    // Unchanged → all unchanged.
    let s1 = revalidate_with(&db, dir.path(), &ScanOptions::default()).unwrap();
    assert_eq!(s1.unchanged, 1);
    assert_eq!(s1.updated, 0);

    // Rewrite with a different size → detected as changed and re-probed.
    std::fs::write(&p, mk(b"DIFFERENT-AUDIO")).unwrap();
    let s2 = revalidate_with(&db, dir.path(), &ScanOptions::default()).unwrap();
    assert_eq!(s2.updated, 1);
    assert_eq!(s2.unchanged, 0);
    // The track row now reflects the new (longer) audio length.
    let track = db
        .get_track_by_path(&std::fs::canonicalize(&p).unwrap().to_string_lossy())
        .unwrap()
        .unwrap();
    assert_eq!(
        usize_from(track.bounds.audio_length()),
        b"DIFFERENT-AUDIO".len()
    );
}

#[test]
fn revalidate_accepts_a_single_file_target() {
    // The CLI advertises file targets for every scan, including --revalidate,
    // so revalidate_with must handle a bare file root (not just a directory).
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("x.flac");
    let mut bytes = b"fLaC".to_vec();
    bytes.push(0x80);
    bytes.extend_from_slice(&[0, 0, 34]);
    bytes.extend(std::iter::repeat_n(0u8, 34));
    bytes.extend_from_slice(b"AUDIO");
    std::fs::write(&p, &bytes).unwrap();
    let db = Db::open_in_memory().unwrap();
    scan_directory(&db, dir.path()).unwrap();

    // Revalidate the file path directly: must not error on read_dir and the
    // unchanged file is bucketed as unchanged (not pruned).
    let stats = revalidate_with(&db, &p, &ScanOptions::default()).unwrap();
    assert_eq!(stats.unchanged, 1);
    assert_eq!(stats.pruned, 0);
    assert_eq!(db.list_tracks().unwrap().len(), 1);
}

#[test]
fn jobs1_and_jobs_n_produce_equivalent_state() {
    let dir = tempfile::tempdir().unwrap();
    // A handful of distinct FLACs.
    for i in 0..12 {
        let mut bytes = b"fLaC".to_vec();
        bytes.push(0x80);
        bytes.extend_from_slice(&[0, 0, 34]);
        bytes.extend(std::iter::repeat_n(0u8, 34));
        bytes.extend_from_slice(format!("AUDIO-{i}").as_bytes());
        std::fs::write(dir.path().join(format!("t{i}.flac")), &bytes).unwrap();
    }
    let norm = |jobs: usize| {
        let db = Db::open_in_memory().unwrap();
        scan_directory_with(
            &db,
            dir.path(),
            &ScanOptions {
                jobs,
                ..Default::default()
            },
        )
        .unwrap();
        let mut rows: Vec<(String, u64, u64)> = db
            .list_tracks()
            .unwrap()
            .into_iter()
            .map(|t| {
                (
                    t.backing_path,
                    t.bounds.audio_offset(),
                    t.bounds.audio_length(),
                )
            })
            .collect();
        rows.sort();
        rows
    };
    assert_eq!(norm(1), norm(4));
    assert_eq!(norm(1).len(), 12);
}

#[test]
fn oversize_unparseable_file_is_skipped_not_read_whole() {
    // A file far larger than the probe ceiling, with a valid FLAC marker but
    // a metadata block that never terminates, must be skipped rather than
    // allocated whole into RAM (the misnamed-multi-GB-file OOM guard).
    use std::io::Write;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("huge.flac");
    let mut f = std::fs::File::create(&path).unwrap();
    // Marker + a non-last VORBIS_COMMENT block claiming the max 24-bit
    // length, so the bounded reader keeps asking for more.
    f.write_all(b"fLaC").unwrap();
    f.write_all(&[0x04, 0xFF, 0xFF, 0xFF]).unwrap();
    let len = MAX_PROBE_BYTES + 4096;
    f.set_len(len).unwrap();
    drop(f);

    assert!(matches!(
        probe_file(&path, WINDOW).unwrap(),
        ProbeOutcome::Unparseable
    ));
}

#[test]
fn oversize_wav_is_served_via_data_header() {
    // A valid WAV whose `data` payload exceeds the probe ceiling (any
    // recording more than a few minutes long) must still be ingested: the
    // `data` chunk header sits at the front, so the declared audio bounds
    // are known without reading the payload. Skipping it would drop every
    // sufficiently long WAV in the library.
    use std::io::Write;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("long.wav");

    let data_len: u64 = MAX_PROBE_BYTES + (16 << 20); // 80 MiB payload
    let mut fmt = Vec::new();
    fmt.extend_from_slice(&1u16.to_le_bytes());
    fmt.extend_from_slice(&1u16.to_le_bytes());
    fmt.extend_from_slice(&44_100u32.to_le_bytes());
    fmt.extend_from_slice(&88_200u32.to_le_bytes());
    fmt.extend_from_slice(&2u16.to_le_bytes());
    fmt.extend_from_slice(&16u16.to_le_bytes());

    let mut front = b"RIFF".to_vec();
    // form: WAVE(4) + fmt chunk(24) + data header(8) + data payload
    let riff_size = 36u32 + u32::try_from(data_len).unwrap();
    front.extend_from_slice(&riff_size.to_le_bytes());
    front.extend_from_slice(b"WAVE");
    front.extend_from_slice(b"fmt ");
    front.extend_from_slice(&u32::try_from(fmt.len()).unwrap().to_le_bytes());
    front.extend_from_slice(&fmt);
    front.extend_from_slice(b"data");
    front.extend_from_slice(&u32::try_from(data_len).unwrap().to_le_bytes());
    let audio_offset = front.len() as u64;
    let file_len = audio_offset + data_len;

    let mut f = std::fs::File::create(&path).unwrap();
    f.write_all(&front).unwrap();
    f.set_len(file_len).unwrap();
    drop(f);

    let probed = match probe_file(&path, WINDOW).unwrap() {
        ProbeOutcome::Probed(p, _) => p,
        other => panic!("expected Probed, got {other:?}"),
    };
    assert_eq!(probed.format, Format::Wav);
    assert_eq!(probed.audio_offset, audio_offset);
    assert_eq!(probed.audio_length, data_len);
}

#[test]
fn probe_file_reports_raced_on_mid_probe_mutation() {
    use std::io::Write;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.wav");

    // Minimal valid WAV the probe accepts (fmt + tiny data).
    let mut fmt = Vec::new();
    for v in [1u16, 1, 0, 0, 0, 16] {
        fmt.extend_from_slice(&v.to_le_bytes());
    }
    let mut front = b"RIFF".to_vec();
    // form: WAVE(4) + fmt chunk(8+len) + data header(8) + data payload(64)
    let riff_size = 4 + 8 + u32::try_from(fmt.len()).unwrap() + 8 + 64;
    front.extend_from_slice(&riff_size.to_le_bytes());
    front.extend_from_slice(b"WAVE");
    front.extend_from_slice(b"fmt ");
    front.extend_from_slice(&u32::try_from(fmt.len()).unwrap().to_le_bytes());
    front.extend_from_slice(&fmt);
    front.extend_from_slice(b"data");
    front.extend_from_slice(&64u32.to_le_bytes());
    let mut f = std::fs::File::create(&path).unwrap();
    f.write_all(&front).unwrap();
    f.set_len(front.len() as u64 + 64).unwrap();
    drop(f);

    let pc = path.clone();
    set_after_s1_hook(move || {
        let mut g = std::fs::OpenOptions::new().append(true).open(&pc).unwrap();
        g.write_all(&[0u8; 4096]).unwrap(); // size moves -> S2 != S1
    });
    let out = probe_file(&path, WINDOW);
    clear_after_s1_hook();
    assert!(matches!(out, Ok(ProbeOutcome::Raced)), "got {out:?}");
}
