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

    // The `opens`/`preads` metrics instrument the *serve* path (reader.rs /
    // open_handle), not the scan path, so both rows print ≈0 even under
    // `--features metrics`. The SP1-relevant signals here are `wall_ms` and
    // `peak_rss_kib` (the latter captures the whole-file `fs::read` memory spike
    // SP1 eliminates). Per-file scan I/O counting arrives in SP1 with scan.rs.
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
    let r = metrics::snapshot();

    // `peak_rss_kib()` reads VmHWM: a process-lifetime high-water mark, not a
    // per-phase sample. Both rows therefore show the same value (revalidate's
    // working set never exceeds the scan's) — the meaningful figure is the scan
    // row's; the revalidate row repeats it for table completeness.
    println!("\n{}", RunReport::header());
    println!(
        "{}",
        RunReport {
            label: "scan".into(),
            format: "flac".into(),
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
            format: "flac".into(),
            tier,
            storage,
            wall_ms: reval_ms,
            opens: r.opens,
            preads: r.preads,
            fsyncs: None,
            peak_rss_kib: peak_rss_kib(),
        }
        .row()
    );
    println!("scanned={} skipped={}\n", stats.scanned, stats.skipped);
    assert!(stats.scanned > 0);
}
