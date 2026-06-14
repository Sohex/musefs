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

/// Frozen per-format read goldens:
/// `(seq_preads, seq_pread_bytes, seek_preads, seek_pread_bytes)`.
/// seq = whole-file sequential read (audio read exactly once).
/// seek = one 128 KiB read near EOF — must touch a BOUNDED window, never the
/// whole file/index. Filled by the characterization run in Step 3; a change here
/// means real read-path work changed — update in the same PR.
fn goldens(fmt: Format) -> (u64, u64, u64, u64) {
    match fmt {
        Format::Mp3 => (33, 4_194_306, 1, 131_072),
        Format::Ogg => (194, 4_221_658, 9, 262_250),
        Format::Flac | Format::M4aMoovFirst | Format::M4aMoovLast | Format::Wav => {
            (33, 4_194_304, 1, 131_072)
        }
    }
}

const SEEK_OFF: u64 = 3_500_000;

#[test]
fn read_preads_and_seek_match_goldens() {
    let _g = METRICS_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    for &fmt in ALL_FORMATS {
        let (exp_seq_preads, exp_seq_bytes, exp_seek_preads, exp_seek_bytes) = goldens(fmt);

        let (fs, inode, _dir) = mount_one(fmt, AUDIO_BYTES_USIZE);
        metrics::reset();
        read_whole(&fs, inode);
        let seq = metrics::snapshot();
        assert_eq!(
            seq.preads,
            exp_seq_preads,
            "{}: sequential preads",
            format_token(fmt)
        );
        assert_eq!(
            seq.pread_bytes,
            exp_seq_bytes,
            "{}: sequential pread_bytes (audio read once)",
            format_token(fmt)
        );
        // Format-agnostic slurp guard, independent of the frozen number.
        assert!(
            seq.pread_bytes < AUDIO_BYTES_USIZE as u64 * 2,
            "{}: sequential read {} bytes — looks like a double-read/slurp",
            format_token(fmt),
            seq.pread_bytes,
        );

        // Fresh mount → cold cache → single deep read.
        let (fs2, inode2, _dir2) = mount_one(fmt, AUDIO_BYTES_USIZE);
        metrics::reset();
        let _ = fs2.read(inode2, None, SEEK_OFF, CHUNK).unwrap();
        let seek = metrics::snapshot();
        assert_eq!(
            seek.preads,
            exp_seek_preads,
            "{}: seek preads",
            format_token(fmt)
        );
        assert_eq!(
            seek.pread_bytes,
            exp_seek_bytes,
            "{}: seek must read a bounded window, not the whole file/index",
            format_token(fmt),
        );
        assert!(
            seek.pread_bytes < AUDIO_BYTES_USIZE as u64 / 4,
            "{}: seek read {} bytes — not a bounded window",
            format_token(fmt),
            seek.pread_bytes,
        );
    }
}
