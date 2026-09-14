use super::*;
use musefs_format::{EmbeddedPicture, PictureType};
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
/// Each widening takes what the probe asked for, but never less than twice
/// what it already had, and never more than the ceiling. The doubling is what
/// keeps a file whose metadata spans many blocks from walking toward the
/// ceiling one exact block at a time.
#[test]
fn widening_asks_for_at_least_double_and_stops_at_the_ceiling() {
    const CAP: u64 = 1 << 20;
    assert_eq!(
        widened(100, 110, CAP),
        200,
        "a small ask still doubles the window"
    );
    assert_eq!(
        widened(100, 5000, CAP),
        5000,
        "a large ask is met in one step"
    );
    assert_eq!(
        widened(CAP / 2 + 1, CAP / 2 + 2, CAP),
        CAP,
        "doubling is capped"
    );
    assert_eq!(widened(100, CAP * 4, CAP), CAP, "so is the ask");
}

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
        depth: 0,
        colors: 0,
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

/// #644: an oversize `covr` fails the whole file rather than being dropped from
/// an otherwise-stored track. The size check still happens before any copy, so
/// the image is described and refused, never materialized — that property is
/// what forces the verdict here at the probe rather than in `check_storable`.
#[test]
fn probe_file_fails_file_with_oversized_mp4_covr() {
    let oversized = vec![0xFFu8; MAX_ART_BYTES + 1];
    let bytes = mp4_with_covr(13, &oversized);
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("oversized_art.m4a");
    std::fs::write(&path, &bytes).unwrap();
    assert!(
        matches!(
            probe_file(
                &path,
                0,
                ChecksumTier::Fingerprint,
                &InodeKeeping::default()
            )
            .unwrap(),
            ProbeOutcome::Failed(_)
        ),
        "an oversized covr must fail the file, not yield a track without its art"
    );
}

#[test]
fn probe_file_fails_file_with_oversized_mp4_binary_freeform() {
    let oversized = vec![0xABu8; MAX_BINARY_TAG_BYTES + 1];
    let bytes = mp4_with_binary_freeform("com.serato.dj", "analysis", &oversized);
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("oversized_bin.m4a");
    std::fs::write(&path, &bytes).unwrap();
    assert!(
        matches!(
            probe_file(
                &path,
                0,
                ChecksumTier::Fingerprint,
                &InodeKeeping::default()
            )
            .unwrap(),
            ProbeOutcome::Failed(_)
        ),
        "an oversized `----` value must fail the file"
    );
}

/// An at-cap `covr` is still stored: the boundary is inclusive, and a drop here
/// would be exactly the silent art loss #644 set out to remove.
#[test]
fn probe_file_keeps_mp4_covr_at_cap() {
    let at_cap = vec![0xFFu8; MAX_ART_BYTES];
    let bytes = mp4_with_covr(13, &at_cap);
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("at_cap_art.m4a");
    std::fs::write(&path, &bytes).unwrap();
    let probed = match probe_file(
        &path,
        0,
        ChecksumTier::Fingerprint,
        &InodeKeeping::default(),
    )
    .unwrap()
    {
        ProbeOutcome::Probed(p, _, _) => p,
        other => panic!("expected Probed, got {other:?}"),
    };
    assert_eq!(probed.format, Format::M4a);
    assert_eq!(probed.pictures.len(), 1);
    assert_eq!(probed.pictures[0].data.len(), MAX_ART_BYTES);
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
            ScanProgress::Failed { done, total } => format!("fail:{done}/{total}"),
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

/// #655: a file that fails to probe still has to advance the progress sequence,
/// or the bar stalls short of its length and a completed scan reads as aborted.
/// Three good files and two unparseable ones must produce a 1..=5 run of events
/// ending at 5/5, whatever order the two kinds interleave in.
#[test]
fn progress_reaches_the_total_when_files_fail() {
    use std::sync::Arc;
    use std::sync::Mutex;
    let dir = tempfile::tempdir().unwrap();
    for i in 0..3 {
        let mut bytes = b"fLaC".to_vec();
        bytes.push(0x80);
        bytes.extend_from_slice(&[0, 0, 34]);
        bytes.extend(std::iter::repeat_n(0u8, 34));
        bytes.extend_from_slice(format!("AUDIO-{i}").as_bytes());
        std::fs::write(dir.path().join(format!("good{i}.flac")), &bytes).unwrap();
    }
    // Supported extension, unparseable content: the walk queues these, and the
    // probe worker fails them — the case that used to leave the bar short.
    for i in 0..2 {
        std::fs::write(dir.path().join(format!("bad{i}.flac")), b"not a flac").unwrap();
    }

    let events = Arc::new(Mutex::new(Vec::<(u64, u64)>::new()));
    let recorder = Arc::clone(&events);
    let sink = ProgressSink::new(move |ev| match ev {
        ScanProgress::Ingested { done, total, .. } | ScanProgress::Failed { done, total } => {
            recorder.lock().unwrap().push((done, total));
        }
        _ => {}
    });

    let db = Db::open_in_memory().unwrap();
    let opts = ScanOptions {
        jobs: 1,
        progress: Some(sink),
        ..Default::default()
    };
    let stats = scan_directory_with(&db, dir.path(), &opts).unwrap();
    assert_eq!(stats.scanned, 3, "the three parseable files ingest");
    assert_eq!(
        stats.failed, 2,
        "the two unparseable files are counted failed"
    );

    let ev = events.lock().unwrap();
    // Every dispatched file reports exactly once, and `done` is a dense 1..=5
    // sequence — so the bar lands on its length rather than stopping at 3/5.
    assert_eq!(
        ev.iter().map(|(d, _)| *d).collect::<Vec<_>>(),
        vec![1, 2, 3, 4, 5],
        "events: {ev:?}"
    );
    assert!(ev.iter().all(|(_, t)| *t == 5), "events: {ev:?}");
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
    let audio = b"AUDIO".as_slice();
    assert_eq!(
        fingerprint_of(&p1, audio),
        fingerprint_of(&p2, audio),
        "same content => same fp"
    );

    let mut p3 = clone_probed(&p1);
    p3.audio_length = 101;
    assert_ne!(
        fingerprint_of(&p1, audio),
        fingerprint_of(&p3, audio),
        "length change => fp change"
    );

    let mut p4 = clone_probed(&p1);
    p4.tags = vec![("title".into(), "B".into())];
    assert_ne!(
        fingerprint_of(&p1, audio),
        fingerprint_of(&p4, audio),
        "tag change => fp change"
    );

    // The #691 half: two files agreeing on every parsed field still differ if
    // their sampled audio differs.
    assert_ne!(
        fingerprint_of(&p1, audio),
        fingerprint_of(&p1, b"OTHER"),
        "sampled audio change => fp change"
    );
}

#[test]
fn full_file_hash_matches_known_sha256() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("f.bin");
    std::fs::write(&path, b"abc").unwrap();
    // sha256("abc")
    assert_eq!(
        full_file_hash(&std::fs::File::open(&path).unwrap()).unwrap(),
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
}

/// The fingerprint's audio sampling: three bounded windows over the audio
/// region, and the whole region when it is shorter than three windows.
#[test]
fn audio_sample_reads_three_bounded_windows() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.bin");
    let span = usize_from(AUDIO_SAMPLE_BYTES);
    // Region long enough for three disjoint windows, with a distinct byte in
    // each so a dropped or mispositioned window is visible.
    let len = 10 * span;
    let mut bytes = vec![0u8; 16 + len];
    bytes[16] = 1; // first window
    bytes[16 + (len - span) / 2] = 2; // middle window
    bytes[16 + len - span] = 3; // last window
    std::fs::write(&path, &bytes).unwrap();
    let p = Probed {
        format: Format::Flac,
        audio_offset: 16,
        audio_length: len as u64,
        tags: Vec::new(),
        pictures: Vec::new(),
        binary_tags: Vec::new(),
        structural_blocks: Vec::new(),
    };
    let f = std::fs::File::open(&path).unwrap();
    let sample = audio_sample(&f, &p).unwrap();
    assert_eq!(sample.len(), 3 * span, "three windows, nothing more");
    assert_eq!(sample[0], 1);
    assert_eq!(sample[span], 2);
    assert_eq!(sample[2 * span], 3);

    // A region shorter than three windows is read whole, once.
    let short = Probed {
        audio_offset: 16,
        audio_length: 32,
        ..clone_probed(&p)
    };
    assert_eq!(audio_sample(&f, &short).unwrap(), bytes[16..48]);

    // The branch boundary sits at three windows, not one: a region of two
    // windows is still read whole (2 × span contiguous), not sampled as three
    // overlapping ones (3 × span).
    let two = Probed {
        audio_offset: 16,
        audio_length: 2 * AUDIO_SAMPLE_BYTES,
        ..clone_probed(&p)
    };
    assert_eq!(audio_sample(&f, &two).unwrap(), bytes[16..16 + 2 * span]);

    // A zero-length audio region reads nothing at all.
    let empty = Probed {
        audio_offset: 16,
        audio_length: 0,
        ..clone_probed(&p)
    };
    assert!(audio_sample(&f, &empty).unwrap().is_empty());

    // Nor does a malformed region running past u64: the sampler declines it
    // rather than overflowing its window arithmetic on untrusted geometry.
    let overflowing = Probed {
        audio_offset: u64::MAX - 4,
        audio_length: 8,
        ..clone_probed(&p)
    };
    assert!(audio_sample(&f, &overflowing).unwrap().is_empty());
}

/// `records_same_bytes` is the whole Keep-vs-Clear decision (#689), so each of
/// the four facts it compares has to be able to say "not the same content" on
/// its own — an `||` slipped between them would let three agreeing fields vouch
/// for a fourth that does not.
#[test]
fn records_same_bytes_needs_every_field_to_agree() {
    let mut unit = unit_with("/m/a.flac", Some("a".repeat(64)));
    unit.stamp.ino = Some(7);
    // A real row to vary: `Track` is `#[non_exhaustive]`, so outside musefs-db
    // one comes from the store rather than a literal.
    let db = Db::open_in_memory().unwrap();
    let id = db
        .upsert_track(&NewTrack {
            backing_path: unit.abs_path.clone(),
            format: Format::Flac,
            audio_offset: 0,
            audio_length: 0,
            backing_size: unit.stamp.size,
            backing_mtime_ns: unit.stamp.mtime_ns,
            backing_ctime_ns: unit.stamp.ctime_ns,
            backing_ino: unit.stamp.ino,
        })
        .unwrap();
    let stored = db.get_track(id).unwrap().expect("the row just written");
    let row = |stamp: BackingStamp, format, offset, length| {
        let mut track = stored.clone();
        track.format = format;
        track.bounds = musefs_db::TrackBounds::new(offset, length, stamp.size).unwrap();
        track.backing_size = stamp.size;
        track.backing_mtime_ns = stamp.mtime_ns;
        track.backing_ctime_ns = stamp.ctime_ns;
        track.backing_ino = stamp.ino;
        track
    };
    let same = row(unit.stamp, Format::Flac, 0, 0);
    assert!(
        records_same_bytes(&unit, Some(&same)),
        "a row agreeing on stamp, format and geometry is the same content"
    );
    assert!(
        !records_same_bytes(&unit, None),
        "no stored row means nothing is known to be unchanged"
    );

    // One disagreement at a time, the rest agreeing.
    let other_stamp = BackingStamp {
        ctime_ns: unit.stamp.ctime_ns + 1,
        ..unit.stamp
    };
    for (label, t) in [
        ("stamp", row(other_stamp, Format::Flac, 0, 0)),
        ("format", row(unit.stamp, Format::Mp3, 0, 0)),
        ("audio_offset", row(unit.stamp, Format::Flac, 4, 0)),
        ("audio_length", row(unit.stamp, Format::Flac, 0, 4)),
        // A stored row with no recorded inode, everything else agreeing: the
        // wildcard that serving allows cannot vouch for a checksum (#689).
        (
            "unrecorded inode",
            row(
                BackingStamp {
                    ino: None,
                    ..unit.stamp
                },
                Format::Flac,
                0,
                0,
            ),
        ),
    ] {
        assert!(
            !records_same_bytes(&unit, Some(&t)),
            "a differing {label} must not read as the same content"
        );
    }
}

/// Where the filesystem keeps no inode numbers, the probe records none, so the
/// live stamp has no inode either — and then the other three fields plus the
/// geometry decide. A cheap pass over an unchanged file there must keep what
/// an expensive one computed ("a cheap pass never undoes an expensive one").
/// A stored row with no inode beside a live stamp that has one is still not
/// proof: that is the upgraded row the test above covers.
#[test]
fn records_same_bytes_needs_no_inode_where_neither_side_has_one() {
    let unit = unit_with("/m/b.flac", None);
    assert_eq!(unit.stamp.ino, None, "the probe recorded no inode");
    let db = Db::open_in_memory().unwrap();
    let id = db
        .upsert_track(&NewTrack {
            backing_path: unit.abs_path.clone(),
            format: Format::Flac,
            audio_offset: 0,
            audio_length: 0,
            backing_size: unit.stamp.size,
            backing_mtime_ns: unit.stamp.mtime_ns,
            backing_ctime_ns: unit.stamp.ctime_ns,
            backing_ino: None,
        })
        .unwrap();
    let stored = db.get_track(id).unwrap().expect("the row just written");
    assert!(
        records_same_bytes(&unit, Some(&stored)),
        "neither side records an inode, and everything else agrees"
    );

    let mut live_has_one = unit_with("/m/b.flac", None);
    live_has_one.stamp.ino = Some(7);
    assert!(
        !records_same_bytes(&live_has_one, Some(&stored)),
        "an unrecorded stored inode cannot vouch for a file whose filesystem keeps them"
    );

    let mut grown = unit_with("/m/b.flac", None);
    grown.stamp.size += 1;
    assert!(
        !records_same_bytes(&grown, Some(&stored)),
        "no inode on either side excuses nothing else"
    );
}

/// `(fingerprint, content_hash)` for the one track in `db`.
fn stored_checksums(db: &Db) -> (Option<String>, Option<String>) {
    let t = db.list_tracks().unwrap().remove(0);
    (t.fingerprint, t.content_hash)
}

/// A pass that answers `keeps` for the filesystem `path` is on, standing in for
/// one that keeps no inode numbers (or does) whatever the suite runs on. Unlike
/// `pretend_no_inodes`, the answer reaches the scan's probe workers.
fn pass_answering(path: &std::path::Path, keeps: bool) -> Arc<InodeKeeping> {
    use std::os::unix::fs::MetadataExt;
    let inodes = Arc::new(InodeKeeping::default());
    inodes.pretend(std::fs::metadata(path).unwrap().dev(), keeps);
    inodes
}

/// #689's rule, on a filesystem that keeps no inode numbers: a `scan --force`
/// below the full tier used to clear the `content_hash` a `--checksum full`
/// pass computed, and `--checksum none` the fingerprint too — disabling move
/// recovery — because keeping a checksum required a recorded inode that no
/// pass there can ever record.
#[test]
fn a_cheap_rescan_keeps_the_checksums_of_an_unchanged_file_where_no_inode_is_recorded() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("unchanged-no-inode.m4a");
    std::fs::write(&path, mp4_with_covr(13, &[0xFF; 8])).unwrap();
    let db = Db::open_in_memory().unwrap();
    let pass = |checksum: ChecksumTier, force: bool| {
        let opts = ScanOptions {
            checksum,
            force,
            ..ScanOptions::default()
        };
        scan_directory_in(&db, dir.path(), &opts, &pass_answering(&path, false)).unwrap()
    };

    pass(ChecksumTier::Full, false);
    assert_eq!(db.list_tracks().unwrap()[0].backing_ino, None);
    let full = stored_checksums(&db);
    assert!(full.0.is_some() && full.1.is_some(), "{full:?}");

    assert_eq!(pass(ChecksumTier::Fingerprint, true).scanned, 1);
    assert_eq!(
        stored_checksums(&db),
        full,
        "a fingerprint-tier rescan of an unchanged file keeps its full hash"
    );
    assert_eq!(pass(ChecksumTier::None, true).scanned, 1);
    assert_eq!(
        stored_checksums(&db),
        full,
        "and a none-tier rescan keeps its fingerprint too"
    );
}

/// The other direction, which the rule must keep: on a filesystem that keeps
/// inode numbers, a row with none recorded is what an upgraded store holds, and
/// a stamp agreeing on the other three fields cannot vouch for its hash.
#[test]
fn a_cheap_rescan_clears_the_checksums_an_unrecorded_inode_cannot_vouch_for() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("upgraded-row.m4a");
    std::fs::write(&path, mp4_with_covr(13, &[0xFF; 8])).unwrap();
    let db = Db::open_in_memory().unwrap();
    let pass = |checksum: ChecksumTier, force: bool| {
        let opts = ScanOptions {
            checksum,
            force,
            ..ScanOptions::default()
        };
        scan_directory_in(&db, dir.path(), &opts, &pass_answering(&path, true)).unwrap()
    };

    pass(ChecksumTier::Full, false);
    let t = db.list_tracks().unwrap().remove(0);
    assert!(t.backing_ino.is_some(), "the inode is recorded");
    db.upsert_track(&NewTrack {
        backing_path: t.backing_path,
        format: t.format,
        audio_offset: t.bounds.audio_offset(),
        audio_length: t.bounds.audio_length(),
        backing_size: t.backing_size,
        backing_mtime_ns: t.backing_mtime_ns,
        backing_ctime_ns: t.backing_ctime_ns,
        backing_ino: None,
    })
    .unwrap();

    pass(ChecksumTier::Fingerprint, true);
    let (fingerprint, content_hash) = stored_checksums(&db);
    assert!(fingerprint.is_some(), "the pass computed a fingerprint");
    assert_eq!(
        content_hash, None,
        "a hash an unrecorded inode cannot vouch for is cleared"
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
        abs_path: std::path::PathBuf::from(abs_path),
        stamp: BackingStamp {
            size: 10,
            mtime_ns: 1,
            ctime_ns: 2,
            ino: None,
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
    ingest_unit(&db, unit, MatchStrictness::Auto, WritePolicy::Full).unwrap();

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
            backing_path: std::path::PathBuf::from(orphan),
            format: Format::Flac,
            audio_offset: 0,
            audio_length: 10,
            backing_size: 10,
            backing_mtime_ns: 0,
            backing_ctime_ns: 0,
            backing_ino: None,
        })
        .unwrap();
    db.set_track_checksums(id, ChecksumWrite::Set(&fp), ChecksumWrite::Keep)
        .unwrap();

    let new_path = "/moved/here.flac";
    let unit = unit_with(new_path, Some(fp.clone()));
    ingest_unit(&db, unit, MatchStrictness::Auto, WritePolicy::Full).unwrap();

    let tracks = db.list_tracks().unwrap();
    assert_eq!(tracks.len(), 1, "orphan retargeted, not duplicated");
    assert_eq!(tracks[0].id, id, "retarget keeps the id");
    assert_eq!(tracks[0].backing_path, std::path::PathBuf::from(new_path));
    // Retarget must refresh the stamp + audio bounds too, not just the path — the
    // orphan was inserted with mtime/ctime 0 and audio_length 10, so a path-only
    // regression would leave these stale.
    assert_eq!(tracks[0].backing_size, 10);
    assert_eq!(tracks[0].backing_mtime_ns, 1);
    assert_eq!(tracks[0].backing_ctime_ns, 2);
    assert_eq!(tracks[0].bounds.audio_offset(), 0);
    assert_eq!(tracks[0].bounds.audio_length(), 0);
    assert_eq!(tracks[0].fingerprint.as_deref(), Some(fp.as_str()));
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
            backing_ino: None,
        })
        .unwrap();
    db.set_track_checksums(id, ChecksumWrite::Set(&fp), ChecksumWrite::Keep)
        .unwrap();

    let unit = unit_with("/fresh/new.flac", Some(fp));
    ingest_unit(&db, unit, MatchStrictness::Auto, WritePolicy::Full).unwrap();

    let tracks = db.list_tracks().unwrap();
    assert_eq!(
        tracks.len(),
        2,
        "unstatable candidate must not be retargeted"
    );
}

/// #746: a structural refresh restores what the file declares about its own
/// picture through a plain `Db` sink too, not only the pipeline's bulk writer.
#[test]
fn refresh_structural_into_restores_the_files_picture_metadata_through_a_db() {
    let db = Db::open_in_memory().unwrap();
    let stamp = BackingStamp {
        size: 10,
        mtime_ns: 1,
        ctime_ns: 1,
        ino: None,
    };
    let probed = |mime: &str, depth: u32| Probed {
        format: Format::Flac,
        audio_offset: 4,
        audio_length: 6,
        tags: Vec::new(),
        pictures: vec![EmbeddedPicture {
            mime: mime.into(),
            picture_type: PictureType::new(3).unwrap(),
            description: String::new(),
            width: 8,
            height: 8,
            depth,
            colors: 0,
            data: vec![7, 7, 7],
        }],
        binary_tags: Vec::new(),
        structural_blocks: Vec::new(),
    };
    let path = Path::new("/m/a.flac");
    ingest_into(
        &db,
        path,
        stamp,
        probed("image/png", 0),
        ChecksumWrite::Keep,
        ChecksumWrite::Keep,
    )
    .unwrap();
    let id = db.list_tracks().unwrap()[0].id;

    refresh_structural_into(
        &db,
        path,
        stamp,
        probed("image/jpeg", 24),
        ChecksumWrite::Keep,
        ChecksumWrite::Keep,
    )
    .unwrap();

    let link = &db.get_track_art(id).unwrap()[0];
    assert_eq!((link.mime.as_str(), link.depth), ("image/jpeg", 24));
}

#[test]
fn refresh_structural_into_preserves_tags_and_art() {
    let db = Db::open_in_memory().unwrap();
    let stamp = BackingStamp {
        size: 10,
        mtime_ns: 1,
        ctime_ns: 1,
        ino: None,
    };
    let seeded = Probed {
        format: Format::Flac,
        audio_offset: 4,
        audio_length: 6,
        tags: vec![("title".into(), "Original".into())],
        pictures: vec![EmbeddedPicture {
            mime: "image/jpeg".into(),
            picture_type: PictureType::new(3).unwrap(),
            description: "Original art".into(),
            width: 1,
            height: 1,
            depth: 0,
            colors: 0,
            data: vec![1, 2, 3],
        }],
        binary_tags: vec![EmbeddedBinaryTag {
            key: "APPLICATION".into(),
            payload: vec![1, 2, 3],
        }],
        structural_blocks: vec![("STREAMINFO".into(), vec![1, 2, 3])],
    };
    ingest_into(
        &db,
        Path::new("/m/a.flac"),
        stamp,
        seeded,
        ChecksumWrite::Keep,
        ChecksumWrite::Keep,
    )
    .unwrap();
    let id = db.list_tracks().unwrap()[0].id;

    let changed = Probed {
        format: Format::Flac,
        audio_offset: 8,
        audio_length: 12,
        tags: vec![("title".into(), "CLOBBERED".into())],
        pictures: vec![EmbeddedPicture {
            mime: "image/jpeg".into(),
            picture_type: PictureType::new(3).unwrap(),
            description: "Changed art".into(),
            width: 2,
            height: 2,
            depth: 0,
            colors: 0,
            data: vec![9, 9, 9],
        }],
        binary_tags: vec![EmbeddedBinaryTag {
            key: "APPLICATION".into(),
            payload: vec![9, 9, 9, 9, 9],
        }],
        structural_blocks: vec![("STREAMINFO".into(), vec![9, 9, 9])],
    };
    let stamp2 = BackingStamp {
        size: 20,
        mtime_ns: 2,
        ctime_ns: 2,
        ino: None,
    };
    refresh_structural_into(
        &db,
        Path::new("/m/a.flac"),
        stamp2,
        changed,
        ChecksumWrite::Keep,
        ChecksumWrite::Keep,
    )
    .unwrap();

    let track = &db.list_tracks().unwrap()[0];
    assert_eq!(track.id, id, "same row upserted, not replaced");
    assert_eq!(track.bounds.audio_offset(), 8, "Layer A bounds refreshed");
    assert_eq!(track.bounds.audio_length(), 12, "Layer A bounds refreshed");
    assert_eq!(track.backing_size, 20, "Layer A stamp refreshed");
    assert_eq!(track.backing_mtime_ns, 2);
    assert_eq!(track.backing_ctime_ns, 2);

    let tags = db.get_tags(id).unwrap();
    assert_eq!(tags, vec![Tag::new("title", "Original", 0)]);

    let structural = db.get_structural_blocks(id).unwrap();
    assert_eq!(
        structural,
        vec![musefs_db::StructuralBlock {
            kind: "STREAMINFO".into(),
            ordinal: 0,
            body: vec![9, 9, 9],
        }]
    );

    let art = db.get_track_art(id).unwrap();
    assert_eq!(art.len(), 1);
    assert_eq!(art[0].description, "Original art");
    assert_eq!(art[0].ordinal, 0);

    // Curated binary tags survive untouched: the original 3-byte APPLICATION
    // payload, not the 5-byte one in `changed`.
    let binary = db.get_binary_tags(id).unwrap();
    assert_eq!(binary.len(), 1, "binary tag preserved");
    assert_eq!(binary[0].key, "APPLICATION");
    assert_eq!(
        binary[0].byte_len, 3,
        "original binary payload kept, not rewritten"
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
        depth: 0,
        colors: 0,
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
        fingerprint_of(&base, b""),
        fingerprint_of(&other, b""),
        "picture description change => fp change"
    );
}

// --- #690: checksums come from the probe's descriptor, inside its sandwich ---

/// The point of `full_file_hash` taking a `&File`: once the caller has stamped
/// this inode the hash describes it, not whatever later takes its name.
/// Reopening the pathname is how a row came to pair one generation's stamp,
/// geometry and tags with another generation's hash (#690).
#[test]
fn full_file_hash_follows_the_descriptor_not_the_path() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("f.bin");
    std::fs::write(&path, b"abc").unwrap();
    let held = std::fs::File::open(&path).unwrap();

    // Replace the name with different content, atomically, the way an external
    // tool rewriting a file in place does.
    let other = dir.path().join("g.bin");
    std::fs::write(&other, b"zzz").unwrap();
    std::fs::rename(&other, &path).unwrap();

    assert_eq!(
        full_file_hash(&held).unwrap(),
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
        "sha256(\"abc\") — the descriptor's bytes, not the path's"
    );
}

/// The retarget confirm decides an identity question, so it must refuse to
/// answer rather than compare a hash of bytes the probe never stamped (#690).
#[test]
fn hash_confirm_refuses_a_file_that_no_longer_matches_the_stamp() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("f.bin");
    std::fs::write(&path, b"abc").unwrap();
    let stamp = BackingStamp::from_metadata(&std::fs::metadata(&path).unwrap());
    assert_eq!(
        hash_confirm(&path, stamp).unwrap().as_deref(),
        Some("ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"),
        "the stamped file confirms"
    );

    // The file moves on; the stamp the probe committed to no longer describes it.
    std::fs::write(&path, b"abcd").unwrap();
    assert_eq!(
        hash_confirm(&path, stamp).unwrap(),
        None,
        "a changed file must not be confirmed against a stale stamp"
    );
}

/// #757: the retarget confirm compares against the stamp the probe recorded, so
/// it has to record the same way — as that stamp says, without asking the
/// filesystem again. Otherwise every confirm where no inode is recorded would
/// compare a stamp without an inode against one with, and refuse. No seam: the
/// filesystem this runs on may keep inode numbers, and the confirm must accept
/// the stamp anyway.
#[test]
fn hash_confirm_accepts_a_stamp_recorded_without_the_inode() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("f.bin");
    std::fs::write(&path, b"abc").unwrap();
    let stamp = BackingStamp::from_metadata(&std::fs::metadata(&path).unwrap()).recordable(false);
    assert_eq!(
        hash_confirm(&path, stamp).unwrap().as_deref(),
        Some("ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"),
        "a file stamped where no inode is kept still confirms"
    );
}

/// #757: a probe records the inode it read, except on a filesystem that keeps
/// none, where it records nothing — and does not read the file as raced for it.
#[test]
fn probe_records_the_inode_only_where_the_filesystem_keeps_one() {
    use std::os::unix::fs::MetadataExt;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.m4a");
    std::fs::write(&path, mp4_with_covr(13, &[0xFF; 8])).unwrap();
    let recorded_ino = || match probe_file(
        &path,
        0,
        ChecksumTier::Fingerprint,
        &InodeKeeping::default(),
    )
    .unwrap()
    {
        ProbeOutcome::Probed(_, stamp, _) => stamp.ino,
        other => panic!("expected Probed, got {other:?}"),
    };

    // Decided the way a scan decides it, for the filesystem this test runs on.
    let expected = crate::freshness::filesystem_keeps_inodes_for_test(dir.path())
        .then(|| std::fs::metadata(&path).unwrap().ino());
    assert_eq!(
        recorded_ino(),
        expected,
        "the inode is recorded exactly where the filesystem keeps inode numbers"
    );
    let _fat = crate::freshness::pretend_no_inodes();
    assert_eq!(
        recorded_ino(),
        None,
        "a filesystem that renumbers files on every mount gets none"
    );
}

/// A checksum the tier asked for and could not produce fails that file under
/// its own reason, instead of committing a row one tier below what the flag
/// promised behind a warn nothing counted (#690). Being in `SkipReason::FAILED`
/// is what puts it in `ScanStats::failed` and so in the exit-2 contract.
#[test]
fn a_checksum_that_cannot_be_produced_fails_the_file() {
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::OpenOptionsExt;

    let dir = tempfile::tempdir().unwrap();
    let fifo = dir.path().join("p");
    let c = std::ffi::CString::new(fifo.as_os_str().as_bytes()).unwrap();
    #[expect(unsafe_code, reason = "libc::mkfifo FFI; no std equivalent")]
    let rc = unsafe { libc::mkfifo(c.as_ptr(), 0o600) };
    assert_eq!(rc, 0, "mkfifo");
    // A FIFO's read end opens without a writer under O_NONBLOCK, and `pread` on
    // it fails with ESPIPE — the shape of any I/O error the checksum reads can
    // hit on a descriptor the probe already opened and parsed successfully.
    let f = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(&fifo)
        .unwrap();
    let probed = Probed {
        format: Format::Flac,
        audio_offset: 0,
        audio_length: 4096,
        tags: Vec::new(),
        pictures: Vec::new(),
        binary_tags: Vec::new(),
        structural_blocks: Vec::new(),
    };

    let err = checksums_of(&f, &probed, ChecksumTier::Full).expect_err("pread on a FIFO fails");
    assert_eq!(checksum_failure(&fifo, &err).reason, SkipReason::Checksum);
    assert!(
        SkipReason::FAILED.contains(&SkipReason::Checksum),
        "a checksum failure must count in ScanStats::failed"
    );
    // The `none` tier asks for nothing, so it has nothing to fail on.
    assert_eq!(
        checksums_of(&f, &probed, ChecksumTier::None).unwrap(),
        Checksums::default()
    );
}

/// #690's routing end to end, which the test above checks only in pieces: a
/// file that parses and then cannot be hashed leaves `probe_file` as a
/// `Checksum` failure carrying its stamp, and a scan counts it in
/// `ScanStats::failed` — the number the CLI turns into exit status 2 — with no
/// row written for it.
#[test]
fn a_file_that_parses_and_cannot_be_hashed_fails_the_scan_for_that_file() {
    crate::warn_limit::log_capture::install();
    let dir = tempfile::tempdir().unwrap();
    let path = std::fs::canonicalize(dir.path())
        .unwrap()
        .join("checksum-fault.m4a");
    std::fs::write(&path, mp4_with_covr(13, &[0xFF; 8])).unwrap();

    struct FaultGuard;
    impl Drop for FaultGuard {
        fn drop(&mut self) {
            set_checksum_fault(None);
        }
    }
    set_checksum_fault(Some(path.clone()));
    let _guard = FaultGuard;

    match probe_file(&path, WINDOW, ChecksumTier::Full, &InodeKeeping::default()).unwrap() {
        ProbeOutcome::Failed(f) => {
            assert_eq!(f.reason, SkipReason::Checksum);
            assert!(f.message.contains("checksum failed"), "{}", f.message);
            assert!(f.stamp.is_some(), "the verdict came from a held file");
        }
        other => panic!("expected a checksum failure, got {other:?}"),
    }

    let db = Db::open_in_memory().unwrap();
    let stats = scan_directory_with(
        &db,
        dir.path(),
        &ScanOptions {
            checksum: ChecksumTier::Full,
            ..ScanOptions::default()
        },
    )
    .unwrap();
    assert_eq!((stats.scanned, stats.failed), (0, 1), "{stats:?}");
    assert!(db.list_tracks().unwrap().is_empty(), "nothing is stored");
    let logged = crate::warn_limit::log_capture::messages_containing("checksum-fault.m4a");
    assert!(
        logged.iter().any(|m| m.contains("checksum failed")),
        "{logged:?}"
    );
}

/// The `&Db` sink's known-path arm: a unit whose path already has a row must be
/// upserted through `ingest_into`, and a pass that computed no full hash over
/// unchanged bytes must leave the stored one alone (#689). "Unchanged" needs the
/// row's inode recorded: an unrecorded one cannot vouch for the hash, which
/// `tests/checksums.rs` covers from the other side.
///
/// Also the only coverage of `<&Db>::existing_track` returning a row — the
/// other `&Db` ingest tests all use paths the store has never seen, so a sink
/// that always answers "no row here" is invisible to them.
#[test]
fn ingest_unit_db_path_keeps_the_hash_of_unchanged_bytes() {
    let db = Db::open_in_memory().unwrap();
    let fp = "a".repeat(64);
    let hash = "d".repeat(64);
    let mut unit = unit_with("/exists.flac", Some(fp.clone()));
    unit.stamp.ino = Some(7);
    let id = db
        .upsert_track(&NewTrack {
            backing_path: unit.abs_path.clone(),
            format: unit.probed.format,
            audio_offset: unit.probed.audio_offset,
            audio_length: unit.probed.audio_length,
            backing_size: unit.stamp.size,
            backing_mtime_ns: unit.stamp.mtime_ns,
            backing_ctime_ns: unit.stamp.ctime_ns,
            backing_ino: unit.stamp.ino,
        })
        .unwrap();
    db.set_track_checksums(id, ChecksumWrite::Keep, ChecksumWrite::Set(&hash))
        .unwrap();

    // The unit carries a fingerprint but no content hash, over bytes the row
    // already describes.
    ingest_unit(&db, unit, MatchStrictness::Auto, WritePolicy::Full).unwrap();

    let tracks = db.list_tracks().unwrap();
    assert_eq!(
        tracks.len(),
        1,
        "the known path is upserted, not duplicated"
    );
    assert_eq!(tracks[0].id, id, "same row");
    assert_eq!(tracks[0].fingerprint.as_deref(), Some(fp.as_str()));
    assert_eq!(
        tracks[0].content_hash.as_deref(),
        Some(hash.as_str()),
        "an unchanged file's hash is still true of it"
    );
}
