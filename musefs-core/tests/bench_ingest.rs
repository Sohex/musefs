mod common;

use std::time::Instant;

use common::corpus::{
    bench_base_dir, bench_formats, format_token, prepare, prepare_format, CorpusParams, Target,
};
use common::report::{peak_rss_kib, RunReport};
use musefs_core::{metrics, revalidate, scan_directory};
use musefs_db::Db;

/// Scan + revalidate one resolved target, printing a `scan` and a `revalidate`
/// row tagged with `format`/`storage`.
///
/// The `opens`/`preads` metrics instrument the *serve* path (reader.rs /
/// open_handle), not the scan path, so both rows print ~0 even under
/// `--features metrics`. The SP1-relevant signals are `wall_ms` and
/// `peak_rss_kib`. `peak_rss_kib()` reads VmHWM — a process-lifetime high-water
/// mark that only rises — so later rows show the same or a higher value than
/// earlier ones; read the first format's scan row for the pre-SP1 baseline.
fn run_one(target: &Target, tier: &str, format: &str, storage: &str) {
    let db = Db::open(&target.db_path).unwrap();

    metrics::reset();
    let t0 = Instant::now();
    let stats = scan_directory(&db, &target.corpus_dir).unwrap();
    let scan_ms = t0.elapsed().as_millis();
    let s = metrics::snapshot();

    metrics::reset();
    let t1 = Instant::now();
    let _ = revalidate(&db, &target.corpus_dir).unwrap();
    let reval_ms = t1.elapsed().as_millis();
    let r = metrics::snapshot();

    for (label, ms, snap) in [("scan", scan_ms, &s), ("revalidate", reval_ms, &r)] {
        println!(
            "{}",
            RunReport {
                label: label.into(),
                format: format.into(),
                tier: tier.into(),
                storage: storage.into(),
                wall_ms: ms,
                opens: snap.opens,
                preads: snap.preads,
                fsyncs: None,
                peak_rss_kib: peak_rss_kib(),
            }
            .row()
        );
    }
    assert!(stats.scanned > 0, "format {format}: scanned 0 tracks");
}

#[test]
#[ignore = "SP0 timing harness; run with --ignored --nocapture"]
fn bench_cold_scan_and_revalidate() {
    let params = CorpusParams::from_env();
    let tier = std::env::var("MUSEFS_BENCH_TIER").unwrap_or_else(|_| "ci".into());

    println!("\n{}", RunReport::header());

    // Real library: already mixed-format and never written to — a single scan
    // tagged "mixed" rather than a per-format sweep.
    if std::env::var("MUSEFS_BENCH_LIBRARY").is_ok() {
        let target = prepare(&params);
        run_one(&target, &tier, "mixed", "real-lib");
        return;
    }

    // Generated mode: one single-format corpus + cold DB per format under a
    // shared base dir (held for the loop's duration).
    let (base, _base_tempdir) = bench_base_dir();
    let storage = if std::env::var("MUSEFS_BENCH_DIR").is_ok() {
        "env-dir"
    } else {
        "tempfs"
    };
    for fmt in bench_formats() {
        let target = prepare_format(&params, &base, fmt);
        run_one(&target, &tier, format_token(fmt), storage);
    }
}
