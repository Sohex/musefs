use super::*;
use musefs_format::PictureType;
use std::io::Write;

// --- ScanOptions defaults (WINDOW L16, BATCH_BYTES L12) ---

// kills the WINDOW `<<`→`>>` and BATCH_BYTES initializer mutants: the
// right-hand sides are decimal literals, so a mutated const/Default
// initializer cannot flow to both sides of the assertion.
#[test]
fn scan_options_defaults() {
    let d = ScanOptions::default();
    assert_eq!(d.jobs, 0, "jobs default = use available parallelism");
    assert_eq!(d.window, 65_536, "window default = 64 KiB");
    assert_eq!(d.batch_bytes, 67_108_864, "batch_bytes default = 64 MiB");
}

// --- read_tail_128() (lines 170-178) ---

fn write_temp(name: &str, bytes: &[u8]) -> (tempfile::TempDir, std::fs::File) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(name);
    std::fs::File::create(&path)
        .unwrap()
        .write_all(bytes)
        .unwrap();
    let file = std::fs::File::open(&path).unwrap();
    (dir, file)
}

// kills scan L171 `<`→`<=` (128-byte file must be Some)
// kills scan L172 Ok(None) constant, L178 Ok(Some) value
// kills scan L176 `file_len - 128`→`/` (offset 0 vs 1 shifts the bytes)
// kills scan L175 buf init [0;128]/[1;128] constants (exact bytes asserted)
#[test]
fn read_tail_128_exact_128_bytes() {
    // Distinct, position-sensitive pattern: byte[i] = i (0..=127).
    let pattern: Vec<u8> = (0u8..128).collect();
    let (_dir, file) = write_temp("tail128.bin", &pattern);

    let tail = read_tail_128(&file, 128).unwrap();
    let expected: [u8; 128] = pattern.clone().try_into().unwrap();
    // Exact equality kills:
    //  - Ok(None) (would be None, not Some)
    //  - [0;128]/[1;128] buf-init constants (would mismatch the pattern)
    //  - `<`→`<=` (128<=128 true → returns None for a 128-byte file)
    //  - `-`→`/` (offset 128/128==1 reads bytes[1..], shifting the pattern)
    assert_eq!(tail, Some(expected));
}

// kills scan L171 `<`→`<=` boundary the other way (127 bytes → None)
#[test]
fn read_tail_128_short_file_is_none() {
    let (_dir, file) = write_temp("tail127.bin", &[0xABu8; 127]);
    assert_eq!(read_tail_128(&file, 127).unwrap(), None);
}

// --- effective_jobs() (lines 313-318) ---

// kills scan L314 effective_jobs body→1 (assuming parallelism > 1)
#[test]
fn effective_jobs_zero_uses_parallelism_and_nonzero_passes_through() {
    let par = std::thread::available_parallelism().map_or(1, std::num::NonZero::get);
    assert_eq!(effective_jobs(0), par);
    assert_eq!(effective_jobs(4), 4);
    assert_eq!(effective_jobs(1), 1);
}

// --- payload_weight() ---

// Sums picture + binary-tag + structural-block byte lengths (batch backpressure).
#[test]
fn payload_weight_sums_all_buffered_payloads() {
    let pic = |n: usize| EmbeddedPicture {
        mime: "image/png".to_string(),
        picture_type: PictureType::new(3).unwrap(),
        description: String::new(),
        width: 0,
        height: 0,
        data: vec![0u8; n],
    };
    let probed = Probed {
        format: Format::Flac,
        audio_offset: 0,
        audio_length: 0,
        tags: Vec::new(),
        pictures: vec![pic(3), pic(5)],
        binary_tags: vec![EmbeddedBinaryTag {
            key: "APPLICATION".into(),
            payload: vec![0u8; 4],
        }],
        structural_blocks: vec![("SEEKTABLE".into(), vec![0u8; 2])],
    };
    // 3 + 5 (pictures) + 4 (binary) + 2 (structural) = 14.
    assert_eq!(payload_weight(&probed), 14);

    // Empty → 0, distinguishes the →1 constant (which ignores the input).
    let empty = Probed {
        format: Format::Flac,
        audio_offset: 0,
        audio_length: 0,
        tags: Vec::new(),
        pictures: Vec::new(),
        binary_tags: Vec::new(),
        structural_blocks: Vec::new(),
    };
    assert_eq!(payload_weight(&empty), 0);
}

/// Minimal-but-valid m4a that `mp4::locate_audio` accepts (one `soun` trak),
/// with a `udta/meta/ilst` carrying one binary `----` atom. `value` is the raw
/// binary `data` payload (type code 0). Not synthesis-grade (no stco), but
/// `probe_full` only locates audio + reads tags, never synthesizes.
fn mp4_with_binary_freeform(mean: &str, name: &str, value: &[u8]) -> Vec<u8> {
    fn bx(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut v = u32::try_from(8 + body.len())
            .unwrap()
            .to_be_bytes()
            .to_vec();
        v.extend_from_slice(kind);
        v.extend_from_slice(body);
        v
    }
    // mdia/hdlr with handler type `soun` at payload offset 8..12 (FullBox
    // version/flags [0..4], pre_defined [4..8], handler_type [8..12]).
    let mut hdlr_body = vec![0u8; 8];
    hdlr_body.extend_from_slice(b"soun");
    hdlr_body.extend_from_slice(&[0u8; 12]); // reserved(12) + empty name
    let trak = bx(b"trak", &bx(b"mdia", &bx(b"hdlr", &hdlr_body)));

    // udta/meta/ilst with one binary `----` atom.
    let mut mean_body = 0u32.to_be_bytes().to_vec();
    mean_body.extend_from_slice(mean.as_bytes());
    let mut name_body = 0u32.to_be_bytes().to_vec();
    name_body.extend_from_slice(name.as_bytes());
    let mut data_body = 0u32.to_be_bytes().to_vec(); // type 0 = binary
    data_body.extend_from_slice(&0u32.to_be_bytes()); // locale
    data_body.extend_from_slice(value);
    let mut free = bx(b"mean", &mean_body);
    free.extend(bx(b"name", &name_body));
    free.extend(bx(b"data", &data_body));
    let ilst = bx(b"ilst", &bx(b"----", &free));
    let mut meta = 0u32.to_be_bytes().to_vec();
    meta.extend(bx(b"hdlr", &[0u8; 25]));
    meta.extend(ilst);
    let udta = bx(b"udta", &bx(b"meta", &meta));

    let moov = bx(b"moov", &[trak, udta].concat());
    [bx(b"ftyp", b"M4A "), moov, bx(b"mdat", b"AUDIODATA")].concat()
}

#[test]
fn probe_full_surfaces_mp4_binary_freeform() {
    use musefs_format::mp4;
    let bytes = mp4_with_binary_freeform("com.serato.dj", "analysis", &[0x00, 0xAB, 0xCD]);
    let probed = probe_full(std::path::Path::new("/x.m4a"), &bytes).expect("probed");
    assert_eq!(probed.format, Format::M4a);
    let keys: Vec<&str> = probed.binary_tags.iter().map(|b| b.key.as_str()).collect();
    assert!(
        keys.contains(&"----:com.serato.dj:analysis"),
        "binary freeform not surfaced: {keys:?}"
    );
    let bt = probed
        .binary_tags
        .iter()
        .find(|b| b.key == "----:com.serato.dj:analysis")
        .unwrap();
    assert_eq!(bt.payload, vec![0x00, 0xAB, 0xCD]);
    let scan = mp4::read_structure(&bytes).unwrap();
    assert_eq!(probed.audio_offset, scan.mdat_payload_offset);
}

fn mp4_with_covr(type_code: u32, value: &[u8]) -> Vec<u8> {
    fn bx(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut v = u32::try_from(8 + body.len())
            .unwrap()
            .to_be_bytes()
            .to_vec();
        v.extend_from_slice(kind);
        v.extend_from_slice(body);
        v
    }
    let mut hdlr_body = vec![0u8; 8];
    hdlr_body.extend_from_slice(b"soun");
    hdlr_body.extend_from_slice(&[0u8; 12]);
    let trak = bx(b"trak", &bx(b"mdia", &bx(b"hdlr", &hdlr_body)));

    let mut data_body = type_code.to_be_bytes().to_vec();
    data_body.extend_from_slice(&0u32.to_be_bytes());
    data_body.extend_from_slice(value);
    let ilst = bx(b"ilst", &bx(b"covr", &bx(b"data", &data_body)));
    let mut meta = 0u32.to_be_bytes().to_vec();
    meta.extend(bx(b"hdlr", &[0u8; 25]));
    meta.extend(ilst);
    let udta = bx(b"udta", &bx(b"meta", &meta));

    let moov = bx(b"moov", &[trak, udta].concat());
    [bx(b"ftyp", b"M4A "), moov, bx(b"mdat", b"AUDIODATA")].concat()
}

#[test]
fn probe_file_skips_oversized_mp4_covr() {
    let oversized = vec![0xFFu8; MAX_ART_BYTES + 1];
    let bytes = mp4_with_covr(13, &oversized);
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("oversized_art.m4a");
    std::fs::write(&path, &bytes).unwrap();
    let probed = match probe_file(&path, 0).unwrap() {
        ProbeOutcome::Probed(p, _) => p,
        other => panic!("expected Probed, got {other:?}"),
    };
    assert_eq!(probed.format, Format::M4a);
    assert!(
        probed.pictures.is_empty(),
        "oversized covr must be skipped at extraction, not materialized"
    );
}

#[test]
fn probe_file_skips_oversized_mp4_binary_freeform() {
    // A `----` value larger than MAX_BINARY_TAG_BYTES must be skipped at
    // extraction by the real seek-path scanner, so it is absent from Probed.
    let oversized = vec![0xABu8; MAX_BINARY_TAG_BYTES + 1];
    let bytes = mp4_with_binary_freeform("com.serato.dj", "analysis", &oversized);
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("oversized_bin.m4a");
    std::fs::write(&path, &bytes).unwrap();
    let probed = match probe_file(&path, 0).unwrap() {
        ProbeOutcome::Probed(p, _) => p,
        other => panic!("expected Probed, got {other:?}"),
    };
    assert_eq!(probed.format, Format::M4a);
    assert!(
        probed.binary_tags.is_empty(),
        "oversized binary freeform must be skipped at extraction, not materialized"
    );
}

#[test]
fn scan_options_debug_includes_progress_sink() {
    let opts = ScanOptions {
        progress: Some(ProgressSink::new(|_| {})),
        ..Default::default()
    };
    assert!(format!("{opts:?}").contains("ProgressSink"));
}

#[test]
fn scan_emits_discovered_walked_ingested_events() {
    use std::sync::Mutex;
    let dir = tempfile::tempdir().unwrap();
    for i in 0..5 {
        let mut bytes = b"fLaC".to_vec();
        bytes.push(0x80);
        bytes.extend_from_slice(&[0, 0, 34]);
        bytes.extend(std::iter::repeat_n(0u8, 34));
        bytes.extend_from_slice(format!("AUDIO-{i}").as_bytes());
        std::fs::write(dir.path().join(format!("t{i}.flac")), &bytes).unwrap();
    }

    let events = Arc::new(Mutex::new(Vec::<String>::new()));
    let recorder = Arc::clone(&events);
    let sink = ProgressSink::new(move |ev| {
        let line = match ev {
            ScanProgress::Discovered { found } => format!("disc:{found}"),
            ScanProgress::Walked { total } => format!("walk:{total}"),
            ScanProgress::Ingested { done, total, .. } => format!("ing:{done}/{total}"),
        };
        recorder.lock().unwrap().push(line);
    });

    let db = Db::open_in_memory().unwrap();
    let opts = ScanOptions {
        jobs: 1,
        progress: Some(sink),
        ..Default::default()
    };
    let stats = scan_directory_with(&db, dir.path(), &opts).unwrap();
    assert_eq!(stats.scanned, 5);

    let ev = events.lock().unwrap();
    // Discovery climbs to the full count.
    assert!(ev.iter().any(|e| e == "disc:5"), "events: {ev:?}");
    // Walk reports the total to ingest.
    assert!(ev.contains(&"walk:5".to_string()), "events: {ev:?}");
    // Ingest reports each committed file, done strictly 1..=total.
    let ing: Vec<&String> = ev.iter().filter(|e| e.starts_with("ing:")).collect();
    assert_eq!(
        ing,
        vec!["ing:1/5", "ing:2/5", "ing:3/5", "ing:4/5", "ing:5/5"],
    );
}

// --- fingerprint_of() / full_file_hash() / ChecksumTier / MatchStrictness ---

fn clone_probed(p: &Probed) -> Probed {
    Probed {
        format: p.format,
        audio_offset: p.audio_offset,
        audio_length: p.audio_length,
        tags: p.tags.clone(),
        pictures: Vec::new(),
        binary_tags: Vec::new(),
        structural_blocks: p.structural_blocks.clone(),
    }
}

#[test]
fn fingerprint_is_deterministic_and_sensitive_to_content() {
    let p1 = Probed {
        format: Format::Flac,
        audio_offset: 8,
        audio_length: 100,
        tags: vec![("title".into(), "A".into())],
        pictures: Vec::new(),
        binary_tags: Vec::new(),
        structural_blocks: vec![("STREAMINFO".into(), vec![1, 2, 3])],
    };
    let p2 = Probed {
        tags: vec![("title".into(), "A".into())],
        structural_blocks: vec![("STREAMINFO".into(), vec![1, 2, 3])],
        ..clone_probed(&p1)
    };
    assert_eq!(
        fingerprint_of(&p1),
        fingerprint_of(&p2),
        "same content => same fp"
    );

    let mut p3 = clone_probed(&p1);
    p3.audio_length = 101;
    assert_ne!(
        fingerprint_of(&p1),
        fingerprint_of(&p3),
        "length change => fp change"
    );

    let mut p4 = clone_probed(&p1);
    p4.tags = vec![("title".into(), "B".into())];
    assert_ne!(
        fingerprint_of(&p1),
        fingerprint_of(&p4),
        "tag change => fp change"
    );
}

#[test]
fn full_file_hash_matches_known_sha256() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("f.bin");
    std::fs::write(&path, b"abc").unwrap();
    // sha256("abc")
    assert_eq!(
        full_file_hash(&path).unwrap(),
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
}

#[test]
fn checksum_tier_defaults_to_fingerprint() {
    assert_eq!(ScanOptions::default().checksum, ChecksumTier::Fingerprint);
    assert_eq!(ScanOptions::default().strictness, MatchStrictness::Auto);
}

// --- ingest_unit through the `&Db` TrackSink path ---

fn empty_probed() -> Probed {
    Probed {
        format: Format::Flac,
        audio_offset: 0,
        audio_length: 0,
        tags: Vec::new(),
        pictures: Vec::new(),
        binary_tags: Vec::new(),
        structural_blocks: Vec::new(),
    }
}

fn unit_with(abs_path: &str, fingerprint: Option<String>) -> Unit {
    Unit {
        abs_path: abs_path.to_string(),
        stamp: BackingStamp {
            size: 10,
            mtime_ns: 1,
            ctime_ns: 2,
        },
        probed: empty_probed(),
        weight: 0,
        fingerprint,
        content_hash: None,
    }
}

// Exercises the `&Db: TrackSink` ingest path: a fresh insert must persist the
// unit's fingerprint via `Db::set_track_checksums` (kills the `&Db`
// `set_track_checksums -> Ok(())` mutant — without the write the row's
// fingerprint stays NULL).
#[test]
fn ingest_unit_db_path_sets_checksums_on_fresh_insert() {
    let db = Db::open_in_memory().unwrap();
    let fp = "a".repeat(64);
    let unit = unit_with("/brand/new.flac", Some(fp.clone()));
    ingest_unit(&db, unit, MatchStrictness::Auto).unwrap();

    let tracks = db.list_tracks().unwrap();
    assert_eq!(tracks.len(), 1);
    assert_eq!(tracks[0].fingerprint.as_deref(), Some(fp.as_str()));
}

// Exercises the `&Db` retarget path: an orphan (backing file gone) with a unique
// fingerprint match must be retargeted in place, not duplicated. Auto with a
// candidate that has no content_hash needs no full-file read, so neither path
// need exist on disk except the orphan's, which must NOT (so it passes the
// copy-vs-move filter). Kills `&Db track_exists_at -> Ok(true)`,
// `tracks_by_fingerprint -> Ok(vec![])`, and `retarget_track -> Ok(())`.
#[test]
fn ingest_unit_db_path_retargets_orphan() {
    let db = Db::open_in_memory().unwrap();
    let fp = "b".repeat(64);
    let orphan = "/gone/missing-orphan.flac";
    let id = db
        .upsert_track(&NewTrack {
            backing_path: orphan.to_string(),
            format: Format::Flac,
            audio_offset: 0,
            audio_length: 10,
            backing_size: 10,
            backing_mtime_ns: 0,
            backing_ctime_ns: 0,
        })
        .unwrap();
    db.set_track_checksums(id, Some(&fp), None).unwrap();

    let new_path = "/moved/here.flac";
    let unit = unit_with(new_path, Some(fp.clone()));
    ingest_unit(&db, unit, MatchStrictness::Auto).unwrap();

    let tracks = db.list_tracks().unwrap();
    assert_eq!(tracks.len(), 1, "orphan retargeted, not duplicated");
    assert_eq!(tracks[0].id, id, "retarget keeps the id");
    assert_eq!(tracks[0].backing_path, new_path);
}

// A candidate whose backing path can't be statted with a NON-NotFound error
// must NOT be treated as a missing move source — it stays a real (present-or-
// inaccessible) row, so the new unit inserts fresh instead of stealing its id.
// Kills the copy-vs-move filter's match-guard `... == NotFound with true` mutant
// (which would treat every stat error, not just NotFound, as missing).
#[test]
fn ingest_unit_db_path_skips_unstatable_candidate() {
    let db = Db::open_in_memory().unwrap();
    let fp = "c".repeat(64);
    // backing_path has a regular file as a path component, so metadata() returns
    // a non-NotFound error (ENOTDIR / NotADirectory), not NotFound.
    let dir = tempfile::tempdir().unwrap();
    let blocker = dir.path().join("not_a_dir");
    std::fs::write(&blocker, b"x").unwrap();
    let bad_path = blocker.join("under_a_file.flac");
    let bad_path = bad_path.to_string_lossy().into_owned();
    // Sanity: the candidate path is unstatable for a reason other than NotFound.
    let kind = std::fs::metadata(&bad_path).unwrap_err().kind();
    assert_ne!(
        kind,
        std::io::ErrorKind::NotFound,
        "must be a non-NotFound error"
    );

    let id = db
        .upsert_track(&NewTrack {
            backing_path: bad_path,
            format: Format::Flac,
            audio_offset: 0,
            audio_length: 10,
            backing_size: 10,
            backing_mtime_ns: 0,
            backing_ctime_ns: 0,
        })
        .unwrap();
    db.set_track_checksums(id, Some(&fp), None).unwrap();

    let unit = unit_with("/fresh/new.flac", Some(fp));
    ingest_unit(&db, unit, MatchStrictness::Auto).unwrap();

    let tracks = db.list_tracks().unwrap();
    assert_eq!(
        tracks.len(),
        2,
        "unstatable candidate must not be retargeted"
    );
}

#[test]
fn fingerprint_changes_with_picture_description() {
    let pic = |desc: &str| EmbeddedPicture {
        mime: "image/jpeg".into(),
        picture_type: PictureType::new(3).unwrap(),
        description: desc.into(),
        width: 10,
        height: 10,
        data: vec![1, 2, 3],
    };
    let base = Probed {
        format: Format::Flac,
        audio_offset: 8,
        audio_length: 100,
        tags: Vec::new(),
        pictures: vec![pic("front")],
        binary_tags: Vec::new(),
        structural_blocks: Vec::new(),
    };
    let other = Probed {
        pictures: vec![pic("back")],
        ..clone_probed(&base)
    };
    assert_ne!(
        fingerprint_of(&base),
        fingerprint_of(&other),
        "picture description change => fp change"
    );
}
