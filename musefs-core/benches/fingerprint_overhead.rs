use criterion::{Criterion, criterion_group, criterion_main};
use musefs_core::{ChecksumTier, ScanOptions, scan_directory_with};
use musefs_db::Db;

// Mirror the fixture recipe used by musefs-core/tests/common/mod.rs:
// streaminfo_body() + vorbis_comment_body() -> make_flac([(0,si),(4,vc)], audio).
// Block types are u8 (0 = STREAMINFO, 4 = VORBIS_COMMENT).
use musefs_format::fuzz_check::fixtures::{make_flac, streaminfo_body, vorbis_comment_body};

/// Build N minimal FLAC files in a tempdir (on $TMPDIR, which is tmpfs/RAM on
/// this host) and return the dir. Each file is ~200 bytes of FLAC metadata +
/// 4 KiB of audio — large enough for a realistic probe, small enough to keep
/// the bench runtime modest.
fn build_library(n: usize) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let si = streaminfo_body();
    let vc = vorbis_comment_body("bench", &["TITLE=Track"]);
    let audio = vec![0xABu8; 4096];
    let bytes = make_flac(&[(0u8, si), (4u8, vc)], &audio);
    for i in 0..n {
        std::fs::write(dir.path().join(format!("t{i}.flac")), &bytes).unwrap();
    }
    dir
}

fn bench_tiers(c: &mut Criterion) {
    let lib = build_library(200);
    let mut g = c.benchmark_group("scan_fingerprint_overhead");
    // Reduce sample count so the bench completes in reasonable time (200 files × N iters).
    g.sample_size(20);
    for tier in [ChecksumTier::None, ChecksumTier::Fingerprint] {
        g.bench_function(format!("{tier:?}"), |b| {
            b.iter(|| {
                let db = Db::open_in_memory().unwrap();
                let opts = ScanOptions {
                    jobs: 1,
                    checksum: tier,
                    ..Default::default()
                };
                scan_directory_with(&db, lib.path(), &opts).unwrap();
            });
        });
    }
    g.finish();
}

criterion_group!(benches, bench_tiers);
criterion_main!(benches);
