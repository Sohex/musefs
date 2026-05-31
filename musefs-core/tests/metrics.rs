#![cfg(feature = "metrics")]

mod common;
use common::{make_flac, streaminfo_body, vorbis_comment_body};
use musefs_core::{metrics, scan_directory, MountConfig, Musefs, VirtualTree};
use std::collections::BTreeMap;
use std::sync::Mutex;

/// Serialise every test that calls `metrics::reset()` / `metrics::snapshot()`.
/// The counters are global statics; parallel threads would corrupt each other's
/// measurements without this lock.
static METRICS_LOCK: Mutex<()> = Mutex::new(());

fn config() -> MountConfig {
    MountConfig {
        template: "$artist/$title".to_string(),
        fallbacks: BTreeMap::new(),
        default_fallback: "Unknown".to_string(),
        mode: musefs_core::Mode::Synthesis,
        poll_interval: std::time::Duration::ZERO,
    }
}

#[test]
fn baseline_one_open_per_read_call() {
    let _guard = METRICS_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let dir = tempfile::tempdir().unwrap();
    let flac = make_flac(
        &[
            (0, streaminfo_body()),
            (4, vorbis_comment_body("v", &["ARTIST=Alice", "TITLE=Song"])),
        ],
        &vec![0xCD_u8; 64 * 1024],
    );
    std::fs::write(dir.path().join("a.flac"), &flac).unwrap();

    let db = musefs_db::Db::open_in_memory().unwrap();
    scan_directory(&db, dir.path()).unwrap();
    let fs = Musefs::open(db, config()).unwrap();

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
        let got = fs.read(file_inode, 0, off, chunk).unwrap();
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

#[test]
fn handle_reuses_one_open_and_no_per_read_stat() {
    let _guard = METRICS_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let dir = tempfile::tempdir().unwrap();
    let flac = make_flac(
        &[
            (0, streaminfo_body()),
            (4, vorbis_comment_body("v", &["ARTIST=Alice", "TITLE=Song"])),
        ],
        &vec![0xCD_u8; 64 * 1024],
    );
    std::fs::write(dir.path().join("a.flac"), &flac).unwrap();

    let db = musefs_db::Db::open_in_memory().unwrap();
    scan_directory(&db, dir.path()).unwrap();
    let fs = Musefs::open(db, config()).unwrap();

    let artist = fs.lookup(VirtualTree::ROOT, "Alice").unwrap();
    let (_, file_inode, _) = fs.readdir(artist).unwrap().into_iter().next().unwrap();
    let size = fs.getattr(file_inode).unwrap().size;

    let fh = fs.open_handle(file_inode).unwrap();
    metrics::reset(); // measure only the reads, not open_handle's resolve+open
    let chunk = 16 * 1024u64;
    let mut off = 0u64;
    let mut reads = 0u64;
    while off < size {
        let got = fs.read(file_inode, fh, off, chunk).unwrap();
        if got.is_empty() {
            break;
        }
        off += got.len() as u64;
        reads += 1;
    }
    let s = metrics::snapshot();
    fs.release_handle(fh);

    assert!(reads >= 2, "expected a multi-chunk read, got {reads}");
    // The whole point of Phase 2: reads reuse the handle's fd and never stat.
    assert_eq!(s.opens, 0, "no per-read open() on the handle path");
    assert_eq!(s.stats, 0, "no per-read stat() on the handle path");
    assert_eq!(s.pread_bytes, 64 * 1024, "audio body read exactly once");
}

#[test]
fn getattr_size_cache_hit_skips_stat() {
    let _guard = METRICS_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let dir = tempfile::tempdir().unwrap();
    let flac = make_flac(
        &[
            (0, streaminfo_body()),
            (4, vorbis_comment_body("v", &["ARTIST=Alice", "TITLE=Song"])),
        ],
        &[0xCD; 4096],
    );
    std::fs::write(dir.path().join("a.flac"), &flac).unwrap();
    let db = musefs_db::Db::open_in_memory().unwrap();
    scan_directory(&db, dir.path()).unwrap();
    let fs = Musefs::open(db, config()).unwrap();
    let artist = fs.lookup(VirtualTree::ROOT, "Alice").unwrap();
    let (_, file_inode, _) = fs.readdir(artist).unwrap().into_iter().next().unwrap();

    let first = fs.getattr(file_inode).unwrap(); // miss → resolve → stat
    metrics::reset();
    let second = fs.getattr(file_inode).unwrap(); // hit → size cache, no stat
    let s = metrics::snapshot();

    assert_eq!(first.size, second.size);
    assert_eq!(s.stats, 0, "a warm getattr must not stat the backing file");
}

#[test]
fn layout_cache_survives_unrelated_refresh() {
    use musefs_db::{Format, NewTrack, Tag};
    let _guard = METRICS_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);

    let dir = tempfile::tempdir().unwrap();
    let bytes = make_flac(
        &[
            (0, streaminfo_body()),
            (4, vorbis_comment_body("v", &["ARTIST=Alice", "TITLE=Song"])),
        ],
        &[0xCD; 8192],
    );
    std::fs::write(dir.path().join("a.flac"), &bytes).unwrap();
    let db_path = dir.path().join("m.db");
    {
        let db = musefs_db::Db::open(&db_path).unwrap();
        scan_directory(&db, dir.path()).unwrap();
    }
    let fs = Musefs::open(musefs_db::Db::open(&db_path).unwrap(), config()).unwrap();
    let artist = fs.lookup(VirtualTree::ROOT, "Alice").unwrap();
    let (_, inode, _) = fs.readdir(artist).unwrap().into_iter().next().unwrap();

    let fh = fs.open_handle(inode).unwrap(); // warms the layout cache
    fs.release_handle(fh);

    // Unrelated external commit + refresh (does NOT touch Alice's track).
    {
        let db2 = musefs_db::Db::open(&db_path).unwrap();
        let id = db2
            .upsert_track(&NewTrack {
                backing_path: "/x/ghost.mp3".to_string(),
                format: Format::Mp3,
                audio_offset: 0,
                audio_length: 0,
                backing_size: 0,
                backing_mtime: 0,
            })
            .unwrap();
        db2.replace_tags(
            id,
            &[Tag::new("artist", "Ghost", 0), Tag::new("title", "G", 0)],
        )
        .unwrap();
    }
    assert!(fs.poll_refresh().unwrap());

    // Re-open: the layout cache entry survived (content_version unchanged), so
    // resolve hits the cache — no FLAC front-read re-synthesis open. Only the
    // single handle fd open should be counted.
    metrics::reset();
    let fh2 = fs.open_handle(inode).unwrap();
    let s = metrics::snapshot();
    fs.release_handle(fh2);

    // (resolve still fires one on_stat even on a cache hit — backing validation; not asserted here)
    assert_eq!(
        s.opens, 1,
        "warm cache: only the handle fd open, no re-synthesis open"
    );
}
