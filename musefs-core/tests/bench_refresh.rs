mod common;

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use common::corpus::{prepare, prepare_format, CorpusParams, Format};
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

#[test]
#[ignore = "SP2 timing harness; run with --ignored --nocapture"]
fn bench_refresh_one_across_library_sizes() {
    let tier = std::env::var("MUSEFS_BENCH_TIER").unwrap_or_else(|_| "ci".into());
    println!("\n{}", RunReport::header());
    for n in [100usize, 1000, 5000, 20000] {
        // Each size gets its own tempdir + DB so the corpora never collide. We use
        // `prepare_format` (explicit base, ignores `MUSEFS_BENCH_DB`) rather than
        // `prepare` (env-driven, returns the same DB path when `MUSEFS_BENCH_DB`
        // is set) precisely so a multi-size sweep stays independent.
        let tmp = tempfile::tempdir().unwrap();
        let params = CorpusParams::single(Format::Flac, 1, n);
        let target = prepare_format(&params, tmp.path(), params.format_mix[0]);

        let db = Db::open(&target.db_path).unwrap();
        scan_directory(&db, &target.corpus_dir).unwrap();
        let fs = Musefs::open(db, config()).unwrap();

        // `open` already built the tree; this single `poll_refresh` (in
        // time_refresh) observes the changed data_version and does the one
        // incremental rebuild we time — the quantity the sweep tracks vs N.
        // `tmp` owns the corpus tempdir and must outlive the scan + open +
        // refresh; it drops at the end of this iteration's scope.
        let one_ms = time_refresh(&target.db_path, &fs, 1);
        println!(
            "{}",
            RunReport {
                label: format!("refresh-1@{n}"),
                format: "flac".into(),
                tier: tier.clone(),
                storage: "tempfs".into(),
                wall_ms: one_ms,
                opens: 0,
                preads: 0,
                fsyncs: None,
                bytes_read: 0,
                peak_rss_kib: None,
            }
            .row()
        );
    }
    println!();
}
