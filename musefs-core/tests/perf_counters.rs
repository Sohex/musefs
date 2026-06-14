#![cfg(feature = "metrics")]

mod common;
use common::corpus::{ALL_FORMATS, CorpusParams, Format, format_token, prepare_format};
use musefs_core::{Mode, MountConfig, Musefs, VirtualTree, metrics, scan_directory};
use std::collections::BTreeMap;
use std::sync::Mutex;

/// The `metrics` counters are global statics; serialize every measured region.
static METRICS_LOCK: Mutex<()> = Mutex::new(());

/// `AUDIO_BYTES` as `usize` for `CorpusParams::bytes_per_track`.
const AUDIO_BYTES_USIZE: usize = 4 * 1024 * 1024;
/// 128 KiB read chunk (matching `read_throughput`).
const CHUNK: u64 = 128 * 1024;

fn config() -> MountConfig {
    MountConfig {
        template: "$artist/$album/$title".to_string(),
        fallbacks: BTreeMap::new(),
        default_fallback: "Unknown".to_string(),
        mode: Mode::Synthesis,
        poll_interval: std::time::Duration::ZERO,
        case_insensitive: false,
    }
}

/// Recursively collect every file inode (non-FLAC corpus tracks render under
/// the `Unknown/` fallback, so we discover by a format-agnostic tree walk).
fn collect_file_inodes(fs: &Musefs, dir: u64, out: &mut Vec<u64>) {
    for (_, ino, is_dir) in fs.readdir(dir).unwrap() {
        if is_dir {
            collect_file_inodes(fs, ino, out);
        } else {
            out.push(ino);
        }
    }
}

/// Generate a single-format AUDIO-ONLY corpus (fixed seed/size), scan + mount,
/// and return (fs, first-file-inode, tempdir-guard).
fn mount_one(fmt: Format, bytes_per_track: usize) -> (Musefs, u64, tempfile::TempDir) {
    let base = tempfile::tempdir().unwrap();
    let params = CorpusParams {
        albums: 1,
        tracks_per_album: 1,
        bytes_per_track,
        art_bytes_per_track: 0,
        format_mix: vec![fmt],
        seed: 42,
    };
    let target = prepare_format(&params, base.path(), fmt);
    let db = musefs_db::Db::open_in_memory().unwrap();
    scan_directory(&db, &target.corpus_dir).unwrap();
    let fs = Musefs::open(db, config()).unwrap();
    let mut inodes = Vec::new();
    collect_file_inodes(&fs, VirtualTree::ROOT, &mut inodes);
    assert!(!inodes.is_empty(), "no file inodes for {fmt:?}");
    (fs, inodes[0], base)
}

fn read_whole(fs: &Musefs, inode: u64) {
    let size = fs.getattr(inode).unwrap().size;
    let mut off = 0u64;
    while off < size {
        let got = fs.read(inode, None, off, CHUNK).unwrap();
        if got.is_empty() {
            break;
        }
        off += got.len() as u64;
    }
}

/// A whole-file sequential read of an audio-only file must emit zero art and
/// zero binary-tag chunks (guards against accidental art/tag re-emission on the
/// hot read path). Per-format byte/pread goldens live in Task 1.2.
#[test]
fn audio_only_read_emits_no_art_or_tag_chunks() {
    let _g = METRICS_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    for &fmt in ALL_FORMATS {
        let (fs, inode, _dir) = mount_one(fmt, AUDIO_BYTES_USIZE);
        metrics::reset();
        read_whole(&fs, inode);
        let s = metrics::snapshot();
        assert_eq!(
            s.art_chunks,
            0,
            "{}: audio-only must emit no art chunks",
            format_token(fmt)
        );
        assert_eq!(
            s.binary_tag_chunks,
            0,
            "{}: audio-only must emit no binary-tag chunks",
            format_token(fmt),
        );
    }
}
