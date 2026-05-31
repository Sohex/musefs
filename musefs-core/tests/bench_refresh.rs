mod common;

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use common::corpus::{prepare, CorpusParams};
use common::report::RunReport;
use musefs_core::{scan_directory, Mode, MountConfig, Musefs};
use musefs_db::Db;

fn config() -> MountConfig {
    MountConfig {
        template: "$artist/$album/$title".to_string(),
        fallbacks: BTreeMap::new(),
        default_fallback: "Unknown".to_string(),
        mode: Mode::Synthesis,
        poll_interval: Duration::ZERO, // no debounce: each poll actually polls
    }
}

/// Replace all tags for `count` tracks via a separate connection, then time
/// the refresh.  Using `replace_tags` (not an append) is sufficient: it bumps
/// both `content_version` (per-track trigger) and `data_version` (whole-DB),
/// which is all `poll_refresh` needs to observe in order to rebuild.
fn time_refresh(db_path: &std::path::Path, fs: &Musefs, count: usize) -> u128 {
    let writer = Db::open(db_path).unwrap();
    let tracks = writer.list_tracks().unwrap();
    for t in tracks.iter().take(count) {
        writer
            .replace_tags(t.id, &[musefs_db::Tag::new("COMMENT", "bench-touch", 0)])
            .unwrap();
    }
    let t0 = Instant::now();
    // poll_refresh returns Ok(true) when a rebuild actually happened. Asserting it
    // guards against silently timing a no-op (e.g. data_version not observed).
    let rebuilt = fs.poll_refresh().unwrap();
    let ms = t0.elapsed().as_millis();
    assert!(
        rebuilt,
        "expected a rebuild after re-tagging {count} track(s)"
    );
    ms
}

#[test]
#[ignore = "SP0 timing harness; run with --ignored --nocapture"]
fn bench_refresh_one_vs_many() {
    let params = CorpusParams::from_env();
    let tier = std::env::var("MUSEFS_BENCH_TIER").unwrap_or_else(|_| "ci".into());
    let target = prepare(&params);

    let db = Db::open(&target.db_path).unwrap();
    scan_directory(&db, &target.corpus_dir).unwrap();
    let fs = Musefs::open(db, config()).unwrap();

    // Both measurements run on the same `fs`. After the first poll_refresh the
    // tree is freshly built; the second call starts from that warm state. Because
    // today's rebuild is unconditionally full (every track, regardless of the
    // change-set size), the two wall times should be roughly equal — that
    // equality is the SP2 baseline. When SP2 makes rebuild cost scale with the
    // changed set, refresh-N should diverge from refresh-1.
    let one_ms = time_refresh(&target.db_path, &fs, 1);
    // Cap the touch count: today's rebuild is full regardless of how many tracks
    // changed, so a bounded sample represents "many changed" without a huge
    // un-batched-write setup on the heavy tiers (the slow path SP1 fixes). SP2
    // will make rebuild cost scale with the changed set, at which point this cap
    // is revisited.
    let many = (params.track_count() / 2).clamp(1, 1000);
    let many_ms = time_refresh(&target.db_path, &fs, many);

    // `poll_refresh` is pure CPU + DB work — independent of both the corpus
    // storage class and the backing audio format — so those columns are fixed
    // rather than derived from the target. Per-format rows would be pure noise.
    println!("\n{}", RunReport::header());
    for (label, ms) in [("refresh-1", one_ms), ("refresh-N", many_ms)] {
        println!(
            "{}",
            RunReport {
                label: label.into(),
                format: "flac".into(),
                tier: tier.clone(),
                storage: "tempfs".into(),
                wall_ms: ms,
                opens: 0,
                preads: 0,
                fsyncs: None,
                bytes_read: 0,
                peak_rss_kib: None,
            }
            .row()
        );
    }
    println!("touched_many={many}\n");
}
