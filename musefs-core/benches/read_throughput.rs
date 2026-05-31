use std::collections::BTreeMap;
use std::sync::Arc;
use std::thread;

use criterion::{criterion_group, criterion_main, Criterion, Throughput};
use musefs_core::{scan_directory, Mode, MountConfig, Musefs, VirtualTree};

#[path = "../tests/common/mod.rs"]
mod common;
use common::corpus::{generate, CorpusParams, Format};

fn config() -> MountConfig {
    MountConfig {
        template: "$artist/$album/$title".to_string(),
        fallbacks: BTreeMap::new(),
        default_fallback: "Unknown".to_string(),
        mode: Mode::Synthesis,
        poll_interval: std::time::Duration::ZERO,
    }
}

/// A small generated corpus with a few MB of audio per track, scanned into an
/// in-memory DB and mounted. Returns the fs plus all file inodes.
fn fixture(bytes_per_track: usize, tracks: usize) -> (Arc<Musefs>, Vec<u64>) {
    let p = CorpusParams {
        albums: 1,
        tracks_per_album: tracks,
        bytes_per_track,
        art_bytes_per_track: 0,
        format_mix: vec![Format::Flac],
        seed: 42,
    };
    let dir = tempfile::tempdir().unwrap();
    generate(dir.path(), &p);
    let db = musefs_db::Db::open_in_memory().unwrap();
    scan_directory(&db, dir.path()).unwrap();
    let fs = Arc::new(Musefs::open(db, config()).unwrap());

    // Walk the single album dir to collect file inodes.
    let artist = fs.lookup(VirtualTree::ROOT, "Artist 00000").unwrap();
    let sub = fs.readdir(artist).unwrap()[0].1; // Album 00000 dir
    let inodes: Vec<u64> = fs
        .readdir(sub)
        .unwrap()
        .into_iter()
        .map(|(_, ino, _)| ino)
        .collect();
    // Keep the tempdir alive for the duration of the bench by leaking it.
    std::mem::forget(dir);
    (fs, inodes)
}

fn bench_sequential_read(c: &mut Criterion) {
    let (fs, inodes) = fixture(4 * 1024 * 1024, 1);
    let inode = inodes[0];
    let size = fs.getattr(inode).unwrap().size;
    let mut group = c.benchmark_group("sequential_read");
    group.throughput(Throughput::Bytes(size));
    let chunk = 128 * 1024u64;
    group.bench_function("flac_128k_chunks", |b| {
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
    group.finish();
}

fn bench_concurrent_read_and_walk(c: &mut Criterion) {
    // M reader threads streaming distinct files + one metadata walker, sharing
    // one Arc<Musefs>. Exercises handles/size_cache mutex contention (SP3).
    let m = std::env::var("MUSEFS_BENCH_READERS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(num_streams);
    let (fs, inodes) = fixture(1024 * 1024, m.max(2));

    let mut group = c.benchmark_group("concurrent_read_walk");
    group.bench_function(format!("m{m}_plus_walker"), |b| {
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
            for r in readers {
                r.join().unwrap();
            }
            stop.store(true, std::sync::atomic::Ordering::Relaxed);
            walker.join().unwrap();
        });
    });
    group.finish();
}

fn num_streams() -> usize {
    std::thread::available_parallelism().map_or(4, |n| n.get() * 2)
}

criterion_group!(
    benches,
    bench_sequential_read,
    bench_concurrent_read_and_walk
);
criterion_main!(benches);
