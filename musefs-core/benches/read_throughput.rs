use std::collections::BTreeMap;

use criterion::{criterion_group, criterion_main, Criterion, Throughput};
use musefs_core::{scan_directory, MountConfig, Musefs, VirtualTree};

#[path = "../tests/common/mod.rs"]
mod common;
use common::{make_flac, streaminfo_body, vorbis_comment_body};

fn config() -> MountConfig {
    MountConfig {
        template: "$artist/$title".to_string(),
        fallbacks: BTreeMap::new(),
        default_fallback: "Unknown".to_string(),
        mode: musefs_core::Mode::Synthesis,
    }
}

fn bench_sequential_read(c: &mut Criterion) {
    let audio_len = 4 * 1024 * 1024usize; // 4 MiB
    // Bind the buffer so the slice passed to make_flac is not an inline `&vec![...]`
    // (avoids clippy::useless_vec while keeping the 4 MiB off the stack).
    let audio = vec![0x7E_u8; audio_len];
    let dir = tempfile::tempdir().unwrap();
    let flac = make_flac(
        &[
            (0, streaminfo_body()),
            (4, vorbis_comment_body("v", &["ARTIST=Alice", "TITLE=Song"])),
        ],
        &audio,
    );
    std::fs::write(dir.path().join("a.flac"), &flac).unwrap();

    let db = musefs_db::Db::open_in_memory().unwrap();
    scan_directory(&db, dir.path()).unwrap();
    let mut fs = Musefs::open(db, config()).unwrap();
    let artist = fs.lookup(VirtualTree::ROOT, "Alice").unwrap();
    let (_, file_inode, _) = fs.readdir(artist).unwrap().into_iter().next().unwrap();
    let size = fs.getattr(file_inode).unwrap().size;

    let mut group = c.benchmark_group("sequential_read");
    group.throughput(Throughput::Bytes(size));
    group.bench_function("flac_128k_chunks", |b| {
        b.iter(|| {
            let chunk = 128 * 1024u64;
            let mut off = 0u64;
            while off < size {
                let got = fs.read(file_inode, off, chunk).unwrap();
                if got.is_empty() {
                    break;
                }
                off += got.len() as u64;
            }
        });
    });
    group.finish();
}

criterion_group!(benches, bench_sequential_read);
criterion_main!(benches);
