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

/// One FLAC metadata block: the last-block flag and type, the 24-bit length,
/// then the body.
fn flac_block(block_type: u8, body: &[u8], last: bool) -> Vec<u8> {
    let mut out = vec![(u8::from(last) << 7) | block_type];
    out.extend_from_slice(&u32::try_from(body.len()).unwrap().to_be_bytes()[1..]);
    out.extend_from_slice(body);
    out
}

/// The metadata region of a FLAC that carries `pictures` PICTURE blocks of
/// `image_bytes` each after its STREAMINFO: the shape of a file holding several
/// large cover scans, whose metadata runs far past the first probe window.
fn flac_front_with_pictures(pictures: usize, image_bytes: usize) -> Vec<u8> {
    let mut out = b"fLaC".to_vec();
    out.extend(flac_block(0, &[0u8; 34], false));
    for i in 0..pictures {
        let mut body = Vec::new();
        body.extend_from_slice(&3u32.to_be_bytes()); // front cover
        body.extend_from_slice(&9u32.to_be_bytes());
        body.extend_from_slice(b"image/png");
        body.extend_from_slice(&0u32.to_be_bytes()); // no description
        for field in [1u32, 1, 24, 0] {
            body.extend_from_slice(&field.to_be_bytes());
        }
        body.extend_from_slice(&u32::try_from(image_bytes).unwrap().to_be_bytes());
        body.extend(std::iter::repeat_n(u8::try_from(i).unwrap(), image_bytes));
        out.extend(flac_block(6, &body, i + 1 == pictures));
    }
    out
}

/// Write `front` at the start of a sparse file `len` bytes long, and `tail` at
/// its very end. Nothing in between is ever written, so the file costs what its
/// two ends do, however far past the probe ceiling it runs.
fn write_sparse(path: &std::path::Path, front: &[u8], len: u64, tail: &[u8]) {
    use std::os::unix::fs::FileExt;
    let f = std::fs::File::create(path).unwrap();
    f.set_len(len).unwrap();
    f.write_all_at(front, 0).unwrap();
    f.write_all_at(tail, len - tail.len() as u64).unwrap();
}

/// The stored `(audio_offset, audio_length)` of the one track a scan of `path`
/// ingests.
fn scanned_bounds(path: &std::path::Path) -> (u64, u64) {
    let db = Db::open_in_memory().unwrap();
    let stats = scan_directory(&db, path).unwrap();
    assert_eq!((stats.scanned, stats.failed), (1, 0), "{stats:?}");
    let t = db.list_tracks().unwrap().remove(0);
    (t.bounds.audio_offset(), t.bounds.audio_length())
}

/// A FLAC larger than the probe ceiling whose metadata takes more than a few
/// widening steps to cover. The bounded probe used to run out of retries and
/// fall back to a whole-buffer parse of the first 64 MiB, which took that
/// buffer's length for the file's: the stored audio region ended at the
/// ceiling, and the mount served a file cut short with nothing to say so.
#[test]
fn a_flac_past_the_probe_ceiling_keeps_its_whole_audio_region() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("large-art.flac");
    let front = flac_front_with_pictures(5, 100 << 10);
    let len = 100 << 20;
    write_sparse(&path, &front, len, &[0xFF, 0xF8]);

    assert_eq!(
        scanned_bounds(&path),
        (front.len() as u64, len - front.len() as u64),
        "the audio runs to the end of the file, not to the probe ceiling"
    );
}

/// The same file with a leading ID3v2 tag (#602), which makes an ID3v1 trailer
/// plausible. Whether one is there is a question about the file's real last 128
/// bytes, which the probe already reads; the whole-buffer fallback asked the
/// last 128 bytes of its truncated buffer instead.
#[test]
fn a_flac_past_the_probe_ceiling_trims_the_trailer_at_its_real_end() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("large-art-id3.flac");
    // An empty ID3v2.4 tag: the 10-byte header, declaring a 10-byte body of
    // padding.
    let mut front = b"ID3\x04\x00\x00\x00\x00\x00\x0A".to_vec();
    front.extend_from_slice(&[0u8; 10]);
    front.extend(flac_front_with_pictures(5, 100 << 10));
    let mut trailer = b"TAG".to_vec();
    trailer.resize(128, b' ');
    let len = 100 << 20;
    write_sparse(&path, &front, len, &trailer);

    assert_eq!(
        scanned_bounds(&path),
        (front.len() as u64, len - 128 - front.len() as u64),
        "the ID3v1 trailer at the end of the file is not audio"
    );
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
        .get_track_by_path(&std::fs::canonicalize(&path).unwrap())
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
        .get_track_by_path(&std::fs::canonicalize(&p).unwrap())
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
        let mut rows: Vec<(std::path::PathBuf, u64, u64)> = db
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
        probe_file(&path, WINDOW, ChecksumTier::Fingerprint).unwrap(),
        ProbeOutcome::Failed(_)
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

    let probed = match probe_file(&path, WINDOW, ChecksumTier::Fingerprint).unwrap() {
        ProbeOutcome::Probed(p, _, _) => p,
        other => panic!("expected Probed, got {other:?}"),
    };
    assert_eq!(probed.format, Format::Wav);
    assert_eq!(probed.audio_offset, audio_offset);
    assert_eq!(probed.audio_length, data_len);
}

/// Write a minimal valid WAV the probe accepts (fmt + 64 bytes of data).
fn write_tiny_wav(path: &std::path::Path) {
    use std::io::Write;
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
    let mut f = std::fs::File::create(path).unwrap();
    f.write_all(&front).unwrap();
    f.set_len(front.len() as u64 + 64).unwrap();
}

/// Append 4096 bytes to `path`, so its size, and with it the stamp, moves.
fn grow(path: &std::path::Path) {
    use std::io::Write;
    let mut g = std::fs::OpenOptions::new().append(true).open(path).unwrap();
    g.write_all(&[0u8; 4096]).unwrap();
}

#[test]
fn probe_file_reports_raced_on_mid_probe_mutation() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.wav");
    write_tiny_wav(&path);

    // Clear the shared hook even if `probe_file` panics, so a failure here can't
    // contaminate sibling tests that observe the global hook.
    struct HookGuard;
    impl Drop for HookGuard {
        fn drop(&mut self) {
            clear_after_s1_hook();
        }
    }

    let pc = path.clone();
    set_after_s1_hook(move || grow(&pc)); // size moves -> S2 != S1
    let _guard = HookGuard;
    let out = probe_file(&path, WINDOW, ChecksumTier::Fingerprint);
    assert!(matches!(out, Ok(ProbeOutcome::Raced)), "got {out:?}");
}

/// #690: the full-file hash is taken inside the probe's fstat sandwich, so a
/// write landing while it is under way is a race, not a row pairing the old
/// stamp with a hash that covers the new bytes. The hook fires once the hash
/// has read its first chunk. A hash moved back outside the sandwich, before
/// the first stat or after the second, returns `Probed` here.
#[test]
fn probe_file_reports_raced_when_the_file_changes_mid_hash() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.wav");
    write_tiny_wav(&path);

    struct HookGuard;
    impl Drop for HookGuard {
        fn drop(&mut self) {
            clear_hook(&DURING_FULL_HASH_HOOK);
        }
    }

    let pc = path.clone();
    set_hook(&DURING_FULL_HASH_HOOK, move || grow(&pc));
    let _guard = HookGuard;
    let out = probe_file(&path, WINDOW, ChecksumTier::Full);
    assert!(matches!(out, Ok(ProbeOutcome::Raced)), "got {out:?}");
}
