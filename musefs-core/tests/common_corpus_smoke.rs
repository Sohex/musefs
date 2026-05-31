mod common;

use common::corpus::{CorpusParams, Tier};
use common::write_m4a_moov_last;
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
fn moov_last_m4a_scans_as_one_track() {
    let dir = tempfile::tempdir().unwrap();
    let (_off, _len) = write_m4a_moov_last(&dir.path().join("a.m4a"), &[0x11u8; 256]);
    let db = Db::open_in_memory().unwrap();
    let stats = scan_directory(&db, dir.path()).unwrap();
    assert_eq!(stats.scanned, 1, "moov-at-end M4A should probe & ingest");
    assert_eq!(stats.skipped, 0);
}
