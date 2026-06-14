mod common;

use std::time::Instant;

use common::corpus::{
    CorpusParams, Target, bench_base_dir, bench_formats, format_token, prepare, prepare_format,
};
use common::report::{RunReport, peak_rss_kib};
use musefs_core::{
    Mode, MountConfig, Musefs, ScanOptions, VirtualTree, metrics, revalidate_with,
    scan_directory_with,
};
use musefs_db::Db;

/// Scan + revalidate one resolved target, printing a `scan` and a `revalidate`
/// row tagged with `format`/`storage`. The `bytes_read` column reports
/// `scan_bytes_read` (the SP1 bounded-read signal: front-anchored prefix, widen,
/// and MP3 tail reads). M4A's seek-reader bytes are not counted here (they live in
/// musefs-format); M4A's win shows in `wall_ms` and `peak_rss_kib`. `opens`/`preads`
/// remain serve-path counters and stay ~0 on the scan path.
fn run_one(target: &Target, tier: &str, format: &str, storage: &str) {
    let db = Db::open(&target.db_path).unwrap();

    let jobs = std::env::var("MUSEFS_BENCH_JOBS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let opts = ScanOptions {
        jobs,
        ..Default::default()
    };

    metrics::reset();
    let t0 = Instant::now();
    let stats = scan_directory_with(&db, &target.corpus_dir, &opts).unwrap();
    let scan_ms = t0.elapsed().as_millis();
    let s = metrics::snapshot();

    metrics::reset();
    let t1 = Instant::now();
    let _ = revalidate_with(&db, &target.corpus_dir, &opts).unwrap();
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
                bytes_read: snap.scan_bytes_read,
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

#[test]
#[ignore = "needs /dev/fuse + MUSEFS_BENCH_LATENCY_PROFILE; run with --ignored --nocapture"]
fn bench_scan_under_latency() {
    use musefs_latencyfs::LatencyMount;

    let Ok(profile) = std::env::var("MUSEFS_BENCH_LATENCY_PROFILE") else {
        println!("set MUSEFS_BENCH_LATENCY_PROFILE=ssd|hdd|nfs-ssd|nfs-hdd to run");
        return;
    };
    let params = CorpusParams::from_env();
    let tier = std::env::var("MUSEFS_BENCH_TIER").unwrap_or_else(|_| "ci".into());

    // Label the row with the corpus's actual format (matching the file's
    // per-format / "mixed" convention) rather than assuming FLAC.
    let format = if params.format_mix.len() == 1 {
        format_token(params.format_mix[0]).to_string()
    } else {
        "mixed".to_string()
    };

    // Generate the corpus on a real backing dir, then mount the latency FS over
    // it so the scan and its SQLite writes traverse the injected-latency layer.
    let backing = tempfile::tempdir().unwrap();
    common::corpus::generate(backing.path(), &params);
    let mount = LatencyMount::new(backing.path(), &profile).unwrap();

    let db = Db::open(mount.path().join("musefs-bench.db")).unwrap();
    // `metrics::reset()` clears the in-process counters but NOT the mount's
    // fsync counter, so snapshot the mount's count here to subtract Db::open's
    // migration/WAL-setup fsyncs; the reported value then covers scan_directory
    // only.
    let fsyncs_before_scan = mount.fsyncs();
    metrics::reset();
    let t0 = Instant::now();
    let stats = scan_directory_with(
        &db,
        &mount.path(),
        &ScanOptions {
            jobs: std::env::var("MUSEFS_BENCH_JOBS")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(0),
            ..Default::default()
        },
    )
    .unwrap();
    let scan_ms = t0.elapsed().as_millis();
    let s = metrics::snapshot();

    println!("\n{}", RunReport::header());
    println!(
        "{}",
        RunReport {
            label: "scan".into(),
            format,
            tier,
            storage: profile.clone(),
            wall_ms: scan_ms,
            opens: s.opens,
            preads: s.preads,
            fsyncs: Some(mount.fsyncs().saturating_sub(fsyncs_before_scan)),
            bytes_read: s.scan_bytes_read,
            peak_rss_kib: None, // FS runs in-process here, but RSS attribution is mixed; omit.
        }
        .row()
    );
    println!("scanned={} skipped={}\n", stats.scanned, stats.skipped);
    assert!(
        stats.scanned > 0,
        "scanned 0 tracks under {profile} latency"
    );
}

/// Latency-injected READ profile (SP4 storage-aware validation). Mounts the
/// corpus under `musefs-latencyfs`, scans through it so backing reads traverse the
/// injected-latency layer, then times two cold operations each on a FRESH mount:
/// a whole-file read and a single deep seek. The decisive column is `preads`/
/// `bytes_read`: the old eager index reads the whole prefix to serve a seek, while
/// SP4 reads only a ~65 KB backward window — so under per-pread latency the seek
/// row separates the two strategies. Run with MUSEFS_BENCH_LATENCY_PROFILE set.
#[test]
#[ignore = "needs /dev/fuse + MUSEFS_BENCH_LATENCY_PROFILE; run with --ignored --nocapture"]
fn bench_read_under_latency() {
    use musefs_latencyfs::LatencyMount;

    let Ok(profile) = std::env::var("MUSEFS_BENCH_LATENCY_PROFILE") else {
        println!("set MUSEFS_BENCH_LATENCY_PROFILE=ssd|hdd|nfs-ssd|nfs-hdd to run");
        return;
    };
    let tier = std::env::var("MUSEFS_BENCH_TIER").unwrap_or_else(|_| "ci".into());
    // Sweep the read-ahead budget off (0) vs on (default 64 MiB) to isolate the
    // backing read-ahead win at each latency profile.
    let ra_mib: u64 = std::env::var("MUSEFS_BENCH_READAHEAD_MIB")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(64);
    // Opt into Phase-2 prefetch threads (off by default) for the bench sweep.
    let ra_prefetch = std::env::var("MUSEFS_READ_AHEAD_PREFETCH")
        .is_ok_and(|v| matches!(v.trim(), "1" | "true" | "yes" | "on"));

    let cfg = || MountConfig {
        template: "$artist/$album/$title".to_string(),
        fallbacks: std::collections::BTreeMap::new(),
        default_fallback: "Unknown".to_string(),
        mode: Mode::Synthesis,
        poll_interval: std::time::Duration::ZERO,
        case_insensitive: false,
        read_ahead_budget: ra_mib * 1024 * 1024,
        read_ahead_prefetch: ra_prefetch,
    };
    fn first_inode(fs: &Musefs, dir: u64) -> Option<u64> {
        for (_, ino, is_dir) in fs.readdir(dir).unwrap() {
            if is_dir {
                if let Some(f) = first_inode(fs, ino) {
                    return Some(f);
                }
            } else {
                return Some(ino);
            }
        }
        None
    }

    let mut params = CorpusParams::from_env();
    params.format_mix = vec![common::corpus::Format::Ogg];
    params.tracks_per_album = params.tracks_per_album.max(1);

    println!("\n{}", RunReport::header());
    // Each row rebuilds a fresh mount + scan so the read is genuinely cold (no
    // warmed HeaderCache / page index from a prior op).
    for (label, whole) in [("read_whole_cold", true), ("read_seek_cold", false)] {
        let backing = tempfile::tempdir().unwrap();
        common::corpus::generate(backing.path(), &params);
        let mount = LatencyMount::new(backing.path(), &profile).unwrap();
        let db = Db::open_in_memory().unwrap();
        scan_directory_with(&db, &mount.path(), &ScanOptions::default()).unwrap();
        let fs = Musefs::open(db, cfg()).unwrap();
        let inode = first_inode(&fs, VirtualTree::ROOT).expect("an ogg inode");
        let size = fs.getattr(inode).unwrap().size;
        // Read through a real handle: `None` would take the fallback path (a
        // fresh disabled-pool reader), bypassing the per-handle read-ahead this
        // bench measures. Open outside the timed region so only reads are timed.
        let fh = fs.open_handle(inode).unwrap();

        metrics::reset();
        let t0 = Instant::now();
        if whole {
            let mut off = 0u64;
            while off < size {
                let got = fs.read(inode, Some(fh), off, 128 * 1024).unwrap();
                if got.is_empty() {
                    break;
                }
                off += got.len() as u64;
            }
        } else {
            let off = (size * 7 / 8).min(size.saturating_sub(128 * 1024));
            let _ = fs.read(inode, Some(fh), off, 128 * 1024).unwrap();
        }
        let ms = t0.elapsed().as_millis();
        let s = metrics::snapshot();
        fs.release_handle(fh);
        println!(
            "{}",
            RunReport {
                label: label.into(),
                format: "ogg".into(),
                tier: tier.clone(),
                storage: format!("{profile}/ra{ra_mib}"),
                wall_ms: ms,
                opens: s.opens,
                preads: s.preads,
                fsyncs: None,
                bytes_read: s.pread_bytes,
                peak_rss_kib: None,
            }
            .row()
        );
    }
}
