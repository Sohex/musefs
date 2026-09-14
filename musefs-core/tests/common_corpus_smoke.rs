mod common;

use common::corpus::{CorpusParams, Format, Tier, prepare};
use common::report::{RunReport, peak_rss_kib};
use common::write_m4a_moov_last;
use common::{VORBIS_PAGE_PAYLOAD, write_ogg, write_ogg_vorbis};
use musefs_core::scan_directory;
use musefs_db::Db;

/// Serializes tests that mutate process-global `MUSEFS_BENCH_*` env vars —
/// cargo runs all tests in one binary across threads, so concurrent
/// set_var/remove_var would race. Every env-touching test locks this first, and
/// recovers from poisoning (`PoisonError::into_inner`) so that a panic in one
/// locked test doesn't cascade into spurious `.lock()` failures in the others.
static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Set a process-global env var. `std::env::set_var` became `unsafe` in
/// edition 2024 (mutating the environment is unsound with concurrent readers).
/// This concentrates that single `unsafe` in one audited place; soundness rests
/// on the existing `ENV_LOCK` discipline — every env-touching test holds the
/// lock for the full read-modify span, so no other test thread touches the
/// environment concurrently.
fn set_env(key: &str, val: impl AsRef<std::ffi::OsStr>) {
    #[expect(
        unsafe_code,
        reason = "test-only env mutation, serialized by ENV_LOCK; std marked env mutation unsafe in edition 2024"
    )]
    unsafe {
        std::env::set_var(key, val);
    }
}

/// Remove a process-global env var. See [`set_env`] for the safety argument.
fn remove_env(key: &str) {
    #[expect(
        unsafe_code,
        reason = "test-only env mutation, serialized by ENV_LOCK; std marked env mutation unsafe in edition 2024"
    )]
    unsafe {
        std::env::remove_var(key);
    }
}

#[test]
fn tier_presets_have_expected_shape() {
    let ci = CorpusParams::for_tier(Tier::Ci);
    assert_eq!(ci.track_count(), 200);
    assert_eq!(ci.art_bytes_per_track, 0, "ci omits embedded art");

    let lc = CorpusParams::for_tier(Tier::LargeCompute);
    assert_eq!(lc.track_count(), 100_000);
    assert!(lc.art_bytes_per_track > 0, "large-compute embeds a cover");

    let bw = CorpusParams::for_tier(Tier::Bandwidth);
    assert!(
        bw.bytes_per_track >= 1_000_000,
        "bandwidth uses realistic payloads"
    );
}

#[test]
fn env_overrides_apply_over_tier() {
    let _g = ENV_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    set_env("MUSEFS_BENCH_TIER", "ci");
    set_env("MUSEFS_BENCH_ALBUMS", "3");
    set_env("MUSEFS_BENCH_TRACKS_PER_ALBUM", "4");
    let p = CorpusParams::from_env();
    remove_env("MUSEFS_BENCH_ALBUMS");
    remove_env("MUSEFS_BENCH_TRACKS_PER_ALBUM");
    remove_env("MUSEFS_BENCH_TIER");
    assert_eq!(p.albums, 3);
    assert_eq!(p.tracks_per_album, 4);
    assert_eq!(p.track_count(), 12);
}

#[test]
fn generate_is_deterministic_and_scans_all_tracks() {
    let p = CorpusParams {
        albums: 2,
        tracks_per_album: 3,
        bytes_per_track: 512,
        art_bytes_per_track: 64,
        format_mix: vec![Format::Flac],
        seed: 7,
    };
    let a = tempfile::tempdir().unwrap();
    let b = tempfile::tempdir().unwrap();
    let files_a = common::corpus::generate(a.path(), &p);
    let files_b = common::corpus::generate(b.path(), &p);
    assert_eq!(files_a.len(), 6);
    // Determinism: same relative names and identical bytes for the first file.
    let first_a = std::fs::read(&files_a[0]).unwrap();
    let first_b = std::fs::read(&files_b[0]).unwrap();
    assert_eq!(first_a, first_b, "same (params, seed) => identical bytes");

    let db = Db::open_in_memory().unwrap();
    let stats = musefs_core::scan_directory(&db, a.path()).unwrap();
    assert_eq!(stats.scanned, 6);
}

#[test]
fn moov_last_m4a_scans_as_one_track() {
    let dir = tempfile::tempdir().unwrap();
    let (_off, _len) = write_m4a_moov_last(&dir.path().join("a.m4a"), &[0x11u8; 256]);
    let db = Db::open_in_memory().unwrap();
    let stats = scan_directory(&db, dir.path()).unwrap();
    assert_eq!(stats.scanned, 1, "moov-at-end M4A should probe & ingest");
    assert_eq!(stats.skipped, 0);
}

#[test]
fn prepare_generates_when_no_library_set() {
    let _g = ENV_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    remove_env("MUSEFS_BENCH_LIBRARY");
    remove_env("MUSEFS_BENCH_DB");
    let scratch = tempfile::tempdir().unwrap();
    set_env("MUSEFS_BENCH_DIR", scratch.path());
    let p = CorpusParams {
        albums: 1,
        tracks_per_album: 2,
        bytes_per_track: 128,
        art_bytes_per_track: 0,
        format_mix: vec![Format::Flac],
        seed: 3,
    };
    let t = prepare(&p);
    remove_env("MUSEFS_BENCH_DIR");
    assert!(t.corpus_dir.exists());
    assert!(!t.is_real_library);
    // DB path is separate from the corpus dir.
    assert_ne!(t.db_path, t.corpus_dir);
    let db = Db::open(&t.db_path).unwrap();
    let stats = musefs_core::scan_directory(&db, &t.corpus_dir).unwrap();
    assert_eq!(stats.scanned, 2);
}

#[test]
fn report_renders_a_row() {
    let r = RunReport {
        label: "scan".into(),
        format: "flac".into(),
        tier: "ci".into(),
        storage: "tempfs".into(),
        wall_ms: 1234,
        opens: 200,
        preads: 200,
        fsyncs: None,
        bytes_read: 0,
        peak_rss_kib: Some(50_000),
    };
    let line = r.row();
    assert!(line.contains("scan"));
    assert!(line.contains("flac"), "format column is rendered");
    assert!(line.contains("ci"));
    assert!(line.contains("n/a"), "fsyncs None renders as n/a");
    // RSS is readable and positive on Linux.
    assert!(peak_rss_kib().unwrap_or(1) > 0);
}

#[test]
fn write_ogg_scans_as_one_track() {
    let dir = tempfile::tempdir().unwrap();
    write_ogg(&dir.path().join("a.ogg"), &[0x22u8; 256]);
    let db = Db::open_in_memory().unwrap();
    let stats = scan_directory(&db, dir.path()).unwrap();
    assert_eq!(stats.scanned, 1, "minimal Ogg Opus should probe & ingest");
    assert_eq!(stats.skipped, 0);
}

#[test]
fn write_ogg_is_deterministic() {
    let dir = tempfile::tempdir().unwrap();
    let a = dir.path().join("a.ogg");
    let b = dir.path().join("b.ogg");
    write_ogg(&a, &[0x33u8; 300]);
    write_ogg(&b, &[0x33u8; 300]);
    assert_eq!(
        std::fs::read(&a).unwrap(),
        std::fs::read(&b).unwrap(),
        "same audio bytes => identical Ogg file"
    );
}

/// Sequence numbers of every page in `data`, with each page's payload length.
fn page_geometry(data: &[u8]) -> Vec<(u32, usize)> {
    let mut out = Vec::new();
    let mut pos = 0usize;
    while pos < data.len() {
        let h = musefs_format::ogg::parse_page(data, pos).unwrap();
        out.push((h.seq, h.total_len() - h.header_len));
        pos += h.total_len();
    }
    out
}

#[test]
fn write_ogg_vorbis_uses_encoder_realistic_page_sizes() {
    // `write_ogg` laces a whole track as one packet, so every page is max-size.
    // This fixture exists to cover the other regime — the ~1 890 B pages real
    // Vorbis streams carry — because the algebraic CRC patch costs scale with
    // page payload length (#666).
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.ogg");
    let audio = vec![0x5Au8; 200 * 1024];
    let (audio_offset, _) = write_ogg_vorbis(&path, &["ARTIST=A"], None, &audio);
    let bytes = std::fs::read(&path).unwrap();

    let audio_start = usize::try_from(audio_offset).unwrap();
    let header_pages = page_geometry(&bytes[..audio_start]);
    assert_eq!(
        header_pages.len(),
        2,
        "libvorbis packs comment+setup behind the BOS page: {header_pages:?}"
    );

    let audio_pages = page_geometry(&bytes[audio_start..]);
    assert!(audio_pages.len() > 100, "expected many small pages");
    for (seq, payload) in &audio_pages[..audio_pages.len() - 1] {
        assert_eq!(
            *payload, VORBIS_PAGE_PAYLOAD,
            "page {seq} should carry one realistic page of audio"
        );
    }
}

#[test]
fn write_ogg_vorbis_renumbers_every_audio_page_on_serve() {
    // The property the fixture exists for: synthesis gives each header packet its
    // own page, so the served header outgrows the original and every audio page's
    // sequence number shifts. Without that shift `crc32(DELTA)` is zero and the
    // algebraic patch short-circuits — which is exactly what the Opus fixture does.
    use musefs_core::{MountConfig, Musefs, VirtualTree};

    let dir = tempfile::tempdir().unwrap();
    let audio = vec![0x5Au8; 64 * 1024];
    let (audio_offset, _) =
        write_ogg_vorbis(&dir.path().join("a.ogg"), &["ARTIST=A"], None, &audio);
    let original = std::fs::read(dir.path().join("a.ogg")).unwrap();

    let db = Db::open_in_memory().unwrap();
    scan_directory(&db, dir.path()).unwrap();
    let mut config = MountConfig::default();
    config.template = "$artist/$album/$title".to_string();
    config.poll_interval = std::time::Duration::ZERO;
    config.case_insensitive = false;
    config.read_ahead_budget = 0;
    let fs = Musefs::open(db, config).unwrap();

    let mut inodes = Vec::new();
    collect_file_inodes(&fs, VirtualTree::ROOT, &mut inodes);
    let inode = inodes[0];
    let size = fs.getattr(inode).unwrap().size;
    let mut served = Vec::new();
    while (served.len() as u64) < size {
        let got = fs
            .read(inode, None, served.len() as u64, 128 * 1024)
            .unwrap();
        assert!(!got.is_empty());
        served.extend_from_slice(&got);
    }

    let audio_start = usize::try_from(audio_offset).unwrap();
    let original_seqs: Vec<u32> = page_geometry(&original[audio_start..])
        .into_iter()
        .map(|(seq, _)| seq)
        .collect();
    // Locate the served audio region the way the serve path does. Trimming the
    // served pages down to the original count instead would make the length check
    // below vacuous, and a dropped or duplicated audio page would pass.
    let served_header = musefs_format::ogg::read_header(&served).unwrap();
    let original_header = musefs_format::ogg::read_header(&original).unwrap();
    assert!(
        served_header.header_pages > original_header.header_pages,
        "synthesis must lengthen the header ({} -> {}); that is what shifts the audio pages",
        original_header.header_pages,
        served_header.header_pages
    );
    let served_start = usize::try_from(served_header.audio_offset).unwrap();
    let served_seqs: Vec<u32> = page_geometry(&served[served_start..])
        .into_iter()
        .map(|(seq, _)| seq)
        .collect();
    assert_eq!(
        served_seqs.len(),
        original_seqs.len(),
        "synthesis must carry every audio page through"
    );
    let deltas: std::collections::BTreeSet<i64> = served_seqs
        .iter()
        .zip(&original_seqs)
        .map(|(new, old)| i64::from(*new) - i64::from(*old))
        .collect();
    assert_eq!(
        deltas.len(),
        1,
        "every audio page shifts by the same amount: {deltas:?}"
    );
    assert_ne!(
        *deltas.iter().next().unwrap(),
        0,
        "audio pages must be renumbered, or the CRC patch never does work"
    );
}

/// Recursively collect every file inode reachable from `dir`.
fn collect_file_inodes(fs: &musefs_core::Musefs, dir: u64, out: &mut Vec<u64>) {
    for (_, ino, is_dir) in fs.readdir(dir).unwrap() {
        if is_dir {
            collect_file_inodes(fs, ino, out);
        } else {
            out.push(ino);
        }
    }
}

#[test]
fn generate_with_all_formats_scans_all() {
    // One track per supported format (round-robin over ALL_FORMATS), so every
    // `generate_one` arm — including both M4A layouts and Ogg — is exercised
    // through `generate()` + `scan_directory` in the default suite.
    let mix = common::corpus::ALL_FORMATS.to_vec();
    let n = mix.len();
    let p = CorpusParams {
        albums: 1,
        tracks_per_album: n,
        bytes_per_track: 256,
        art_bytes_per_track: 0,
        format_mix: mix,
        seed: 9,
    };
    let dir = tempfile::tempdir().unwrap();
    common::corpus::generate(dir.path(), &p);
    let db = Db::open_in_memory().unwrap();
    let stats = scan_directory(&db, dir.path()).unwrap();
    assert_eq!(stats.scanned, n as u64, "every supported format ingests");
}

#[test]
fn bench_formats_defaults_to_all_when_unset() {
    let _g = ENV_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    remove_env("MUSEFS_BENCH_FORMAT_MIX");
    assert_eq!(
        common::corpus::bench_formats(),
        common::corpus::ALL_FORMATS.to_vec()
    );
}

#[test]
fn bench_formats_filters_and_never_empty() {
    let _g = ENV_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    set_env("MUSEFS_BENCH_FORMAT_MIX", "ogg,wav");
    let filtered = common::corpus::bench_formats();
    set_env("MUSEFS_BENCH_FORMAT_MIX", "garbagetoken");
    let fallback = common::corpus::bench_formats();
    remove_env("MUSEFS_BENCH_FORMAT_MIX");
    assert_eq!(filtered, vec![Format::Ogg, Format::Wav]);
    assert_eq!(
        fallback,
        common::corpus::ALL_FORMATS.to_vec(),
        "all-unrecognized must fall back to full coverage, never empty"
    );
}

#[test]
fn all_formats_round_trip_through_tokens() {
    for &f in common::corpus::ALL_FORMATS {
        assert_eq!(
            common::corpus::format_from_token(common::corpus::format_token(f)),
            Some(f),
            "ALL_FORMATS member {f:?} must round-trip through its token"
        );
    }
}

#[test]
fn prepare_format_generates_scannable_single_format_corpus() {
    let base = tempfile::tempdir().unwrap();
    let p = CorpusParams {
        albums: 1,
        tracks_per_album: 2,
        bytes_per_track: 256,
        // format_mix is overridden by prepare_format; set something different
        // to prove the override.
        art_bytes_per_track: 0,
        format_mix: vec![Format::Flac],
        seed: 5,
    };
    let t = common::corpus::prepare_format(&p, base.path(), Format::Ogg);
    assert!(
        t.corpus_dir.ends_with("ogg"),
        "per-format subdir named by token"
    );
    assert_ne!(t.db_path, t.corpus_dir);
    let db = Db::open(&t.db_path).unwrap();
    let stats = scan_directory(&db, &t.corpus_dir).unwrap();
    assert_eq!(stats.scanned, 2, "two Ogg tracks generated and scanned");
}

#[test]
fn bench_base_dir_defaults_to_held_tempdir() {
    let _g = ENV_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    remove_env("MUSEFS_BENCH_DIR");
    let (base, scratch) = common::corpus::bench_base_dir();
    assert!(base.exists(), "base dir exists");
    assert!(
        scratch.is_some(),
        "unset MUSEFS_BENCH_DIR yields a caller-held tempdir"
    );
}
