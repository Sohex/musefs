mod common;

use common::corpus::{prepare, CorpusParams, Format, Tier};
use common::report::{peak_rss_kib, RunReport};
use common::write_m4a_moov_last;
use common::write_ogg;
use musefs_core::scan_directory;
use musefs_db::Db;

/// Serializes tests that mutate process-global `MUSEFS_BENCH_*` env vars —
/// cargo runs all tests in one binary across threads, so concurrent
/// set_var/remove_var would race. Every env-touching test locks this first.
static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

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
    let _g = ENV_LOCK.lock().unwrap();
    std::env::set_var("MUSEFS_BENCH_TIER", "ci");
    std::env::set_var("MUSEFS_BENCH_ALBUMS", "3");
    std::env::set_var("MUSEFS_BENCH_TRACKS_PER_ALBUM", "4");
    let p = CorpusParams::from_env();
    std::env::remove_var("MUSEFS_BENCH_ALBUMS");
    std::env::remove_var("MUSEFS_BENCH_TRACKS_PER_ALBUM");
    std::env::remove_var("MUSEFS_BENCH_TIER");
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
    let _g = ENV_LOCK.lock().unwrap();
    std::env::remove_var("MUSEFS_BENCH_LIBRARY");
    std::env::remove_var("MUSEFS_BENCH_DB");
    let scratch = tempfile::tempdir().unwrap();
    std::env::set_var("MUSEFS_BENCH_DIR", scratch.path());
    let p = CorpusParams {
        albums: 1,
        tracks_per_album: 2,
        bytes_per_track: 128,
        art_bytes_per_track: 0,
        format_mix: vec![Format::Flac],
        seed: 3,
    };
    let t = prepare(&p);
    std::env::remove_var("MUSEFS_BENCH_DIR");
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
        tier: "ci".into(),
        storage: "tempfs".into(),
        wall_ms: 1234,
        opens: 200,
        preads: 200,
        fsyncs: None,
        peak_rss_kib: Some(50_000),
    };
    let line = r.row();
    assert!(line.contains("scan"));
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
