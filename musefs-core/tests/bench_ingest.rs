mod common;

use std::time::Instant;

use common::corpus::{prepare, CorpusParams};
use common::report::{peak_rss_kib, RunReport};
use musefs_core::{metrics, revalidate, scan_directory};
use musefs_db::Db;

fn storage_label(t: &common::corpus::Target) -> String {
    if t.is_real_library {
        "real-lib".into()
    } else if std::env::var("MUSEFS_BENCH_DIR").is_ok() {
        "env-dir".into()
    } else {
        "tempfs".into()
    }
}

#[test]
#[ignore = "SP0 timing harness; run with --ignored --nocapture"]
fn bench_cold_scan_and_revalidate() {
    let params = CorpusParams::from_env();
    let tier = std::env::var("MUSEFS_BENCH_TIER").unwrap_or_else(|_| "ci".into());
    let target = prepare(&params);
    let storage = storage_label(&target);

    let db = Db::open(&target.db_path).unwrap();

    metrics::reset();
    let t0 = Instant::now();
    let stats = scan_directory(&db, &target.corpus_dir).unwrap();
    let scan_ms = t0.elapsed().as_millis();
    let s = metrics::snapshot();

    // Second pass: revalidate should skip unchanged files (cheap).
    metrics::reset();
    let t1 = Instant::now();
    let _ = revalidate(&db, &target.corpus_dir).unwrap();
    let reval_ms = t1.elapsed().as_millis();

    println!("\n{}", RunReport::header());
    println!(
        "{}",
        RunReport {
            label: "scan".into(),
            tier: tier.clone(),
            storage: storage.clone(),
            wall_ms: scan_ms,
            opens: s.opens,
            preads: s.preads,
            fsyncs: None,
            peak_rss_kib: peak_rss_kib(),
        }
        .row()
    );
    println!(
        "{}",
        RunReport {
            label: "revalidate".into(),
            tier,
            storage,
            wall_ms: reval_ms,
            opens: 0,
            preads: 0,
            fsyncs: None,
            peak_rss_kib: peak_rss_kib(),
        }
        .row()
    );
    println!("scanned={} skipped={}\n", stats.scanned, stats.skipped);
    assert!(stats.scanned > 0);
}
