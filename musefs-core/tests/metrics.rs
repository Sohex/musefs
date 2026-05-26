#![cfg(feature = "metrics")]

mod common;
use common::{make_flac, streaminfo_body, vorbis_comment_body};
use musefs_core::{metrics, scan_directory, MountConfig, Musefs, VirtualTree};
use std::collections::BTreeMap;

fn config() -> MountConfig {
    MountConfig {
        template: "$artist/$title".to_string(),
        fallbacks: BTreeMap::new(),
        default_fallback: "Unknown".to_string(),
        mode: musefs_core::Mode::Synthesis,
    }
}

#[test]
fn baseline_one_open_per_read_call() {
    let dir = tempfile::tempdir().unwrap();
    let flac = make_flac(
        &[
            (0, streaminfo_body()),
            (4, vorbis_comment_body("v", &["ARTIST=Alice", "TITLE=Song"])),
        ],
        &[0xCD; 64 * 1024],
    );
    std::fs::write(dir.path().join("a.flac"), &flac).unwrap();

    let db = musefs_db::Db::open_in_memory().unwrap();
    scan_directory(&db, dir.path()).unwrap();
    let mut fs = Musefs::open(db, config()).unwrap();

    let artist = fs.lookup(VirtualTree::ROOT, "Alice").unwrap();
    let (name, file_inode, _) = fs.readdir(artist).unwrap().into_iter().next().unwrap();
    assert_eq!(name, "Song.flac");
    let size = fs.getattr(file_inode).unwrap().size;

    metrics::reset();
    // Read the file in 16 KiB chunks (the access pattern a streaming player produces).
    let chunk = 16 * 1024u64;
    let mut off = 0u64;
    let mut reads = 0u64;
    while off < size {
        let got = fs.read(file_inode, off, chunk).unwrap();
        if got.is_empty() {
            break;
        }
        off += got.len() as u64;
        reads += 1;
    }
    let s = metrics::snapshot();

    // BASELINE (pre-handle-lifecycle): the backing file is reopened on every
    // read() call. A later phase will reduce this to ~1 open per file.
    assert!(reads >= 2, "expected a multi-chunk read, got {reads}");
    assert_eq!(s.opens, reads, "currently one open() per read() call");
    // The 64 KiB audio body is read exactly once across the chunked reads; the
    // inline FLAC header is an Inline segment and is not pread-counted.
    assert_eq!(s.pread_bytes, 64 * 1024, "audio body read exactly once");
}
