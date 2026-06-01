use std::collections::BTreeMap;
use std::sync::Arc;
use std::thread;

use criterion::{criterion_group, criterion_main, Criterion, Throughput};
use musefs_core::{scan_directory, Mode, MountConfig, Musefs, VirtualTree};

#[path = "../tests/common/mod.rs"]
mod common;
use common::corpus::{bench_formats, format_token, generate, CorpusParams, Format};

fn config() -> MountConfig {
    MountConfig {
        template: "$artist/$album/$title".to_string(),
        fallbacks: BTreeMap::new(),
        default_fallback: "Unknown".to_string(),
        mode: Mode::Synthesis,
        poll_interval: std::time::Duration::ZERO,
    }
}

/// Recursively collect every non-directory inode reachable from `dir`. Used
/// instead of a name-based lookup because non-FLAC corpus builders embed no tags,
/// so their tracks render under the `default_fallback` ("Unknown/…") path.
fn collect_file_inodes(fs: &Musefs, dir: u64, out: &mut Vec<u64>) {
    for (_, ino, is_dir) in fs.readdir(dir).unwrap() {
        if is_dir {
            collect_file_inodes(fs, ino, out);
        } else {
            out.push(ino);
        }
    }
}

/// A small single-format generated corpus, scanned into an in-memory DB and
/// mounted. Returns the fs plus all file inodes (discovered by a format-agnostic
/// tree walk).
fn fixture(format: Format, bytes_per_track: usize, tracks: usize) -> (Arc<Musefs>, Vec<u64>) {
    let p = CorpusParams {
        albums: 1,
        tracks_per_album: tracks,
        bytes_per_track,
        art_bytes_per_track: 0,
        format_mix: vec![format],
        seed: 42,
    };
    let dir = tempfile::tempdir().unwrap();
    generate(dir.path(), &p);
    let db = musefs_db::Db::open_in_memory().unwrap();
    scan_directory(&db, dir.path()).unwrap();
    let fs = Arc::new(Musefs::open(db, config()).unwrap());

    let mut inodes = Vec::new();
    collect_file_inodes(&fs, VirtualTree::ROOT, &mut inodes);
    assert!(!inodes.is_empty(), "fixture: no file inodes for {format:?}");
    // Keep the tempdir alive for the duration of the bench by leaking it. Each
    // fixture call leaks one; a full run accumulates at most ALL_FORMATS.len()+1
    // (sequential per-format + concurrent), all reclaimed when the process exits.
    std::mem::forget(dir);
    (fs, inodes)
}

fn bench_sequential_read(c: &mut Criterion) {
    let mut group = c.benchmark_group("sequential_read");
    let chunk = 128 * 1024u64;
    for fmt in bench_formats() {
        let (fs, inodes) = fixture(fmt, 4 * 1024 * 1024, 1);
        let inode = inodes[0];
        let size = fs.getattr(inode).unwrap().size;
        group.throughput(Throughput::Bytes(size));
        // fh=0 takes the no-handle path: each read resolves the inode via the
        // HeaderCache rather than reusing a registered fd.
        group.bench_function(format_token(fmt), |b| {
            b.iter(|| {
                let mut off = 0u64;
                while off < size {
                    let got = std::hint::black_box(fs.read(inode, 0, off, chunk).unwrap());
                    if got.is_empty() {
                        break;
                    }
                    off += got.len() as u64;
                }
            });
        });
    }
    group.finish();
}

fn bench_concurrent_read_and_walk(c: &mut Criterion) {
    // M reader threads streaming distinct files + one metadata walker, sharing
    // one Arc<Musefs>. Exercises handles/size_cache mutex contention (SP3).
    let m = std::env::var("MUSEFS_BENCH_READERS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(num_streams);
    let (fs, inodes) = fixture(Format::Flac, 1024 * 1024, m.max(2));

    // Each iteration streams `m` whole files (reader i reads inodes[i]); the
    // walker does metadata-only ops and contributes no bytes.
    let total_bytes: u64 = (0..m)
        .map(|i| fs.getattr(inodes[i % inodes.len()]).unwrap().size)
        .sum();

    let mut group = c.benchmark_group("concurrent_read_walk");
    group.throughput(Throughput::Bytes(total_bytes));
    group.bench_function(format!("m{m}_plus_walker"), |b| {
        // Thread spawn/join overhead is included in each iteration, so this
        // measures burst concurrency rather than steady-state throughput; the
        // signal of interest is how that wall time scales with `m` (SP3 lock
        // contention), not its absolute value.
        b.iter(|| {
            let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
            // Walker thread: loop lookups/getattrs over the inodes.
            let walker = {
                let fs = Arc::clone(&fs);
                let inodes = inodes.clone();
                let stop = Arc::clone(&stop);
                thread::spawn(move || {
                    while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                        for &ino in &inodes {
                            let _ = std::hint::black_box(fs.getattr(ino));
                        }
                    }
                })
            };
            let readers: Vec<_> = (0..m)
                .map(|i| {
                    let fs = Arc::clone(&fs);
                    let ino = inodes[i % inodes.len()];
                    thread::spawn(move || {
                        // open_handle + per-handle reads exercise the `handles`
                        // mutex (SP3); the walker's getattr exercises `size_cache`.
                        let fh = fs.open_handle(ino).unwrap();
                        let size = fs.getattr(ino).unwrap().size;
                        let mut off = 0u64;
                        while off < size {
                            let got = fs.read(ino, fh, off, 128 * 1024).unwrap();
                            if got.is_empty() {
                                break;
                            }
                            off += got.len() as u64;
                        }
                        fs.release_handle(fh);
                    })
                })
                .collect();
            // Join all readers first, then stop the walker — collecting results
            // before unwrapping so a reader panic can't leave the walker spinning
            // (stop is always set before we re-raise the panic).
            let reader_results: Vec<_> =
                readers.into_iter().map(thread::JoinHandle::join).collect();
            stop.store(true, std::sync::atomic::Ordering::Relaxed);
            walker.join().unwrap();
            for r in reader_results {
                r.unwrap();
            }
        });
    });
    group.finish();
}

/// A fresh fixture that does NOT leak its tempdir: the caller holds the returned
/// `TempDir` for the lifetime of the (timed) read, so each iteration sees a cold
/// HeaderCache. This is the realistic "open and read a file once" workload, unlike
/// `sequential_read` which re-reads one warmed/cached file in a tight loop.
fn cold_fixture(format: Format, bytes_per_track: usize) -> (Arc<Musefs>, u64, tempfile::TempDir) {
    let p = CorpusParams {
        albums: 1,
        tracks_per_album: 1,
        bytes_per_track,
        art_bytes_per_track: 0,
        format_mix: vec![format],
        seed: 42,
    };
    let dir = tempfile::tempdir().unwrap();
    generate(dir.path(), &p);
    let db = musefs_db::Db::open_in_memory().unwrap();
    scan_directory(&db, dir.path()).unwrap();
    let fs = Arc::new(Musefs::open(db, config()).unwrap());
    let mut inodes = Vec::new();
    collect_file_inodes(&fs, VirtualTree::ROOT, &mut inodes);
    assert!(!inodes.is_empty(), "cold_fixture: no file inodes for {format:?}");
    let inode = inodes[0];
    (fs, inode, dir)
}

fn read_whole(fs: &Musefs, inode: u64) {
    let size = fs.getattr(inode).unwrap().size;
    let chunk = 128 * 1024u64;
    let mut off = 0u64;
    while off < size {
        let got = std::hint::black_box(fs.read(inode, 0, off, chunk).unwrap());
        if got.is_empty() {
            break;
        }
        off += got.len() as u64;
    }
}

/// Cold first read: a fresh mount per iteration, so the read pays whatever
/// per-file setup the strategy needs (the old eager index build; SP4 nothing).
/// This is the "play a track once" workload — the eager-index amortization that
/// `sequential_read` rewards never applies here.
fn bench_cold_first_read(c: &mut Criterion) {
    let mut group = c.benchmark_group("cold_first_read");
    let bytes = 4 * 1024 * 1024usize;
    for fmt in bench_formats() {
        group.throughput(Throughput::Bytes(bytes as u64));
        group.bench_function(format_token(fmt), |b| {
            b.iter_batched(
                || cold_fixture(fmt, bytes),
                |(fs, inode, _dir)| read_whole(&fs, inode),
                criterion::BatchSize::PerIteration,
            );
        });
    }
    group.finish();
}

/// Arbitrary deep seek: a fresh mount per iteration, then ONE 128 KiB read near
/// the end of the file. The old code builds the entire page index just to serve
/// this one chunk; SP4 scans ~65 KiB backward. The case the design targets.
fn bench_seek_read(c: &mut Criterion) {
    let mut group = c.benchmark_group("seek_read");
    let bytes = 4 * 1024 * 1024usize;
    let seek_off = 3_500_000u64;
    for fmt in bench_formats() {
        group.throughput(Throughput::Bytes(128 * 1024));
        group.bench_function(format_token(fmt), |b| {
            b.iter_batched(
                || cold_fixture(fmt, bytes),
                |(fs, inode, _dir)| {
                    let _ = std::hint::black_box(fs.read(inode, 0, seek_off, 128 * 1024).unwrap());
                },
                criterion::BatchSize::PerIteration,
            );
        });
    }
    group.finish();
}

fn num_streams() -> usize {
    std::thread::available_parallelism().map_or(4, |n| n.get() * 2)
}

criterion_group!(
    benches,
    bench_sequential_read,
    bench_cold_first_read,
    bench_seek_read,
    bench_concurrent_read_and_walk
);
criterion_main!(benches);
