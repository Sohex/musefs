use std::collections::HashMap;
use std::path::{Path, PathBuf};

use musefs_db::{Db, Format, NewArt, NewTrack, Tag, TrackArt};
use musefs_format::{flac, mp3, mp4, ogg, wav, EmbeddedPicture, Extent};

use crate::error::Result;

/// Initial bounded-read window. Covers typical metadata + cover art; a larger
/// metadata region triggers a `NeedMore` widen.
const WINDOW: usize = 1 << 20; // 1 MiB
/// Cap on widen iterations before falling back to a whole-file read.
const MAX_WIDEN_RETRIES: usize = 8;

/// Skip embedded art whose image bytes exceed this. The binding limit is FLAC's
/// 24-bit PICTURE block length (~16 MiB for the whole block); reserve 64 KiB of
/// headroom so the block framing + mime + description can never push a near-cap
/// image past the limit at synthesis time. Real cover art is far smaller.
const MAX_ART_BYTES: usize = 16 * 1024 * 1024 - 64 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanStats {
    pub scanned: u64,
    pub skipped: u64,
    pub failed: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RevalidateStats {
    pub updated: u64,
    pub unchanged: u64,
    pub pruned: u64,
    pub failed: u64,
}

fn mtime_secs(meta: &std::fs::Metadata) -> i64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_secs() as i64)
}

fn has_ext(path: &Path, ext: &str) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case(ext))
}

/// True if `path` has an extension for a format the scanner can probe.
fn is_supported_audio(path: &Path) -> bool {
    has_ext(path, "flac")
        || has_ext(path, "mp3")
        || has_ext(path, "m4a")
        || has_ext(path, "m4b")
        || has_ext(path, "ogg")
        || has_ext(path, "oga")
        || has_ext(path, "opus")
        || has_ext(path, "wav")
}

fn collect_audio(root: &Path, out: &mut Vec<PathBuf>) -> std::io::Result<()> {
    for entry in std::fs::read_dir(root)? {
        let entry = entry?;
        let path = entry.path();
        let ftype = entry.file_type()?;
        if ftype.is_dir() {
            collect_audio(&path, out)?;
        } else if ftype.is_file() && is_supported_audio(&path) {
            out.push(path);
        }
    }
    Ok(())
}

/// A backing file parsed into the fields a track row needs, plus its raw
/// `(key, value)` tags to seed.
pub(crate) struct Probed {
    format: Format,
    audio_offset: u64,
    audio_length: u64,
    tags: Vec<(String, String)>,
    pictures: Vec<EmbeddedPicture>,
}

/// Full-buffer probe (legacy path). Retained as the reference implementation the
/// bounded path is checked against (see the equivalence property test).
pub(crate) fn probe_full(path: &Path, bytes: &[u8]) -> Option<Probed> {
    if has_ext(path, "flac") {
        let scan = flac::locate_audio(bytes).ok()?;
        Some(Probed {
            format: Format::Flac,
            audio_offset: scan.audio_offset,
            audio_length: scan.audio_length,
            tags: flac::read_vorbis_comments(bytes).unwrap_or_default(),
            pictures: flac::read_pictures(bytes).unwrap_or_default(),
        })
    } else if has_ext(path, "mp3") {
        let bounds = mp3::locate_audio(bytes).ok()?;
        Some(Probed {
            format: Format::Mp3,
            audio_offset: bounds.audio_offset,
            audio_length: bounds.audio_length,
            tags: mp3::read_tags(bytes),
            pictures: mp3::read_pictures(bytes),
        })
    } else if has_ext(path, "m4a") || has_ext(path, "m4b") {
        let bounds = mp4::locate_audio(bytes).ok()?;
        Some(Probed {
            format: Format::M4a,
            audio_offset: bounds.audio_offset,
            audio_length: bounds.audio_length,
            tags: mp4::read_tags(bytes),
            pictures: mp4::read_pictures(bytes),
        })
    } else if has_ext(path, "ogg") || has_ext(path, "oga") || has_ext(path, "opus") {
        let scan = ogg::locate_audio(bytes).ok()?;
        let format = match scan.codec {
            ogg::Codec::Opus => Format::Opus,
            ogg::Codec::Vorbis => Format::Vorbis,
            ogg::Codec::OggFlac => Format::OggFlac,
        };
        Some(Probed {
            format,
            audio_offset: scan.audio_offset,
            audio_length: scan.audio_length,
            tags: ogg::read_tags(bytes).unwrap_or_default(),
            pictures: ogg::read_pictures(bytes).unwrap_or_default(),
        })
    } else if has_ext(path, "wav") {
        let bounds = wav::locate_audio(bytes).ok()?;
        Some(Probed {
            format: Format::Wav,
            audio_offset: bounds.audio_offset,
            audio_length: bounds.audio_length,
            tags: wav::read_tags(bytes),
            pictures: wav::read_pictures(bytes),
        })
    } else {
        None
    }
}

/// Effective initial window: `MUSEFS_SCAN_WINDOW` (bytes) if set, else `WINDOW`.
fn scan_window() -> usize {
    std::env::var("MUSEFS_SCAN_WINDOW")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .filter(|&n| n > 0)
        .unwrap_or(WINDOW)
}

/// Read `[0, len)` of `path` into a buffer, counting the read. A short read at
/// EOF is fine (`len` may exceed the file size).
fn read_window(file: &std::fs::File, len: usize) -> std::io::Result<Vec<u8>> {
    use std::os::unix::fs::FileExt;
    let mut buf = vec![0u8; len];
    let n = file.read_at(&mut buf, 0)?;
    buf.truncate(n);
    crate::metrics::on_scan_read(n as u64);
    Ok(buf)
}

/// Read the file's last 128 bytes (for the MP3 ID3v1 trailer check), or `None`
/// if the file is shorter than 128 bytes.
fn read_tail_128(file: &std::fs::File, file_len: u64) -> std::io::Result<Option<[u8; 128]>> {
    if file_len < 128 {
        return Ok(None);
    }
    use std::os::unix::fs::FileExt;
    let mut buf = [0u8; 128];
    file.read_exact_at(&mut buf, file_len - 128)?;
    crate::metrics::on_scan_read(128);
    Ok(Some(buf))
}

/// Bounded probe of one backing file: open once, read a bounded window, dispatch
/// per format, widening on `NeedMore`. Never reads the audio payload (M4A uses
/// the seek reader; front-anchored formats read only the metadata extent).
/// Returns `Ok(None)` for an unsupported/unparseable file (to be skipped).
///
/// Metrics note: `on_scan_read` counts the front-anchored prefix/widen/tail
/// reads only. The M4A seek reader does its own positioned reads internally, so
/// its bytes are not reflected in `SCAN_BYTES_READ` (only `on_scan_open` fires
/// for M4A); its win shows up in wall time and peak RSS instead.
fn probe_file(path: &Path, file_len: u64) -> std::io::Result<Option<Probed>> {
    let file = std::fs::File::open(path)?;
    crate::metrics::on_scan_open();

    // M4A: seek reader, never touches mdat.
    if has_ext(path, "m4a") || has_ext(path, "m4b") {
        let mut f = &file;
        let Ok(scan) = mp4::read_structure_from(&mut f, file_len) else {
            return Ok(None);
        };
        return Ok(Some(Probed {
            format: Format::M4a,
            audio_offset: scan.mdat_payload_offset,
            audio_length: scan.mdat_payload_len,
            tags: mp4::read_tags(&scan.moov),
            pictures: mp4::read_pictures(&scan.moov),
        }));
    }

    // Front-anchored formats: read a window, widen on NeedMore.
    let tail = read_tail_128(&file, file_len)?;
    let mut want = (scan_window() as u64).min(file_len) as usize;
    let mut prefix = read_window(&file, want)?;
    for _ in 0..MAX_WIDEN_RETRIES {
        match probe_prefix(path, &prefix, file_len, tail.as_ref()) {
            Probe::Done(p) => return Ok(Some(p)),
            Probe::Skip => return Ok(None),
            Probe::NeedMore(up_to) => {
                // Already at EOF? The prefix is the whole file; widening can't help.
                if want as u64 >= file_len {
                    break;
                }
                // Grow to at least `up_to` (capped at the file), always making
                // progress (`+1`), then retry.
                want = (up_to.min(file_len) as usize)
                    .max(want + 1)
                    .min(file_len as usize);
                prefix = read_window(&file, want)?;
            }
        }
    }
    // Fallback: read the whole file once and use the full-buffer probe.
    if (prefix.len() as u64) < file_len {
        prefix = read_window(&file, file_len as usize)?;
    }
    Ok(probe_full(path, &prefix))
}

/// Outcome of a single bounded dispatch attempt against the current `prefix`.
enum Probe {
    Done(Probed),
    NeedMore(u64),
    Skip,
}

/// Dispatch the front-anchored formats against `prefix` + `file_len`.
fn probe_prefix(path: &Path, prefix: &[u8], file_len: u64, tail: Option<&[u8; 128]>) -> Probe {
    if has_ext(path, "flac") {
        match flac::read_metadata_bounded(prefix) {
            Ok(Extent::Complete(meta)) => Probe::Done(Probed {
                format: Format::Flac,
                audio_offset: meta.audio_offset,
                audio_length: file_len - meta.audio_offset,
                tags: flac::read_vorbis_comments(prefix).unwrap_or_default(),
                pictures: flac::read_pictures(prefix).unwrap_or_default(),
            }),
            Ok(Extent::NeedMore { up_to }) => Probe::NeedMore(up_to),
            Err(_) => Probe::Skip,
        }
    } else if has_ext(path, "mp3") {
        match mp3::locate_audio_bounded(prefix, file_len, tail) {
            Ok(Extent::Complete(b)) => Probe::Done(Probed {
                format: Format::Mp3,
                audio_offset: b.audio_offset,
                audio_length: b.audio_length,
                tags: mp3::read_tags(prefix),
                pictures: mp3::read_pictures(prefix),
            }),
            Ok(Extent::NeedMore { up_to }) => Probe::NeedMore(up_to),
            Err(_) => Probe::Skip,
        }
    } else if has_ext(path, "ogg") || has_ext(path, "oga") || has_ext(path, "opus") {
        match ogg::read_metadata_bounded(prefix, file_len) {
            Ok(Extent::Complete(header)) => {
                let format = match header.codec {
                    ogg::Codec::Opus => Format::Opus,
                    ogg::Codec::Vorbis => Format::Vorbis,
                    ogg::Codec::OggFlac => Format::OggFlac,
                };
                Probe::Done(Probed {
                    format,
                    audio_offset: header.audio_offset,
                    audio_length: file_len - header.audio_offset,
                    tags: ogg::read_tags(prefix).unwrap_or_default(),
                    pictures: ogg::read_pictures(prefix).unwrap_or_default(),
                })
            }
            Ok(Extent::NeedMore { up_to }) => Probe::NeedMore(up_to),
            Err(_) => Probe::Skip,
        }
    } else if has_ext(path, "wav") {
        match wav::locate_audio_bounded(prefix, file_len) {
            Ok(Extent::Complete(b)) => Probe::Done(Probed {
                format: Format::Wav,
                audio_offset: b.audio_offset,
                audio_length: b.audio_length,
                tags: wav::read_tags(prefix),
                pictures: wav::read_pictures(prefix),
            }),
            Ok(Extent::NeedMore { up_to }) => Probe::NeedMore(up_to),
            Err(_) => Probe::Skip,
        }
    } else {
        Probe::Skip
    }
}

/// Upsert a track from a probed backing file: write the track row, replace its
/// seeded tags, and ingest its embedded art (capped, deduped, clamped).
fn ingest(db: &Db, abs_path: &str, meta: &std::fs::Metadata, probed: Probed) -> Result<()> {
    let track_id = db.upsert_track(&NewTrack {
        backing_path: abs_path.to_string(),
        format: probed.format,
        audio_offset: probed.audio_offset as i64,
        audio_length: probed.audio_length as i64,
        backing_size: meta.len() as i64,
        backing_mtime: mtime_secs(meta),
    })?;

    let mut tags = Vec::new();
    let mut ordinals: HashMap<String, i64> = HashMap::new();
    for (key, value) in probed.tags {
        let ord = ordinals.entry(key.clone()).or_insert(0);
        tags.push(Tag::new(&key, &value, *ord));
        *ord += 1;
    }
    db.replace_tags(track_id, &tags)?;

    let mut track_arts = Vec::new();
    // Filter before enumerating so skipped (oversized) art doesn't leave gaps
    // in the stored ordinals.
    let accepted = probed
        .pictures
        .into_iter()
        .filter(|p| p.data.len() <= MAX_ART_BYTES);
    for (ordinal, pic) in accepted.enumerate() {
        let art_id = db.upsert_art(&NewArt {
            mime: pic.mime,
            width: (pic.width != 0).then_some(pic.width as i64),
            height: (pic.height != 0).then_some(pic.height as i64),
            data: pic.data,
        })?;
        // Valid ID3/FLAC picture types are 0..=20; clamp anything out of range.
        let picture_type = if pic.picture_type <= 20 {
            pic.picture_type as i64
        } else {
            0
        };
        track_arts.push(TrackArt {
            art_id,
            picture_type,
            description: pic.description,
            ordinal: ordinal as i64,
        });
    }
    db.set_track_art(track_id, &track_arts)?;
    Ok(())
}

/// Insert/update a track row for each supported audio file (FLAC, MP3, M4A,
/// Opus, Vorbis, FLAC-in-Ogg) under `root` (with audio bounds and validation
/// stamps), seeding its tags from the file's existing metadata. `root` may be
/// a single audio file (only that file is scanned) or a directory (walked
/// recursively). Files that fail to parse are skipped.
pub fn scan_directory(db: &Db, root: &Path) -> Result<ScanStats> {
    let mut files = Vec::new();
    if root.is_file() {
        if is_supported_audio(root) {
            files.push(root.to_path_buf());
        }
    } else {
        collect_audio(root, &mut files)?;
    }

    let mut stats = ScanStats {
        scanned: 0,
        skipped: 0,
        failed: 0,
    };
    for path in files {
        let Ok(meta) = std::fs::metadata(&path) else {
            stats.failed += 1;
            continue;
        };
        match probe_file(&path, meta.len()) {
            Ok(Some(probed)) => {
                let abs = std::fs::canonicalize(&path)?;
                ingest(db, &abs.to_string_lossy(), &meta, probed)?;
                stats.scanned += 1;
            }
            Ok(None) => stats.skipped += 1,
            Err(_) => stats.failed += 1,
        }
    }
    Ok(stats)
}

/// Test/oracle only: scan using the legacy whole-file probe (`probe_full`). The
/// equivalence property compares this against the bounded `scan_directory`.
#[doc(hidden)]
pub fn scan_directory_full_oracle(db: &Db, root: &Path) -> Result<ScanStats> {
    let mut files = Vec::new();
    if root.is_file() {
        if is_supported_audio(root) {
            files.push(root.to_path_buf());
        }
    } else {
        collect_audio(root, &mut files)?;
    }
    let mut stats = ScanStats {
        scanned: 0,
        skipped: 0,
        failed: 0,
    };
    for path in files {
        let bytes = std::fs::read(&path)?;
        let Some(probed) = probe_full(&path, &bytes) else {
            stats.skipped += 1;
            continue;
        };
        let meta = std::fs::metadata(&path)?;
        let abs = std::fs::canonicalize(&path)?;
        ingest(db, &abs.to_string_lossy(), &meta, probed)?;
        stats.scanned += 1;
    }
    Ok(stats)
}

/// Re-validate an already-scanned library subtree: re-probe only files whose
/// size/mtime changed since the last scan (skipping unchanged ones so external
/// tag edits in the DB are preserved), then delete tracks **under `root`** whose
/// backing file is gone (cascading tags/art links) and garbage-collect
/// now-unreferenced art. Pruning is scoped to `root`, so revalidating one library
/// root never removes tracks belonging to another.
pub fn revalidate(db: &Db, root: &Path) -> Result<RevalidateStats> {
    let mut files = Vec::new();
    collect_audio(root, &mut files)?;

    let mut stats = RevalidateStats {
        updated: 0,
        unchanged: 0,
        pruned: 0,
        failed: 0,
    };
    for path in files {
        let Ok(meta) = std::fs::metadata(&path) else {
            stats.failed += 1;
            continue;
        };
        let Ok(abs) = std::fs::canonicalize(&path) else {
            stats.failed += 1;
            continue;
        };
        let abs_str = abs.to_string_lossy().to_string();

        if let Some(existing) = db.get_track_by_path(&abs_str)? {
            if existing.backing_size == meta.len() as i64
                && existing.backing_mtime == mtime_secs(&meta)
            {
                stats.unchanged += 1;
                continue;
            }
        }

        match probe_file(&path, meta.len()) {
            Ok(Some(probed)) => {
                ingest(db, &abs_str, &meta, probed)?;
                stats.updated += 1;
            }
            Ok(None) => {}
            Err(_) => {
                stats.failed += 1;
            }
        }
    }

    // Prune tracks under `root` whose backing file is gone. Scoped to `root` so a
    // targeted revalidate never touches tracks from a different library root, and
    // gated on `NotFound` so a transient I/O error (an unreadable mount, a denied
    // permission) does not delete a track whose file still exists.
    let canon_root = std::fs::canonicalize(root)?;
    for track in db.list_tracks()? {
        if !Path::new(&track.backing_path).starts_with(&canon_root) {
            continue;
        }
        match std::fs::metadata(&track.backing_path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                db.delete_track(track.id)?;
                stats.pruned += 1;
            }
            _ => {}
        }
    }
    db.gc_orphan_art()?;

    Ok(stats)
}

#[cfg(test)]
mod ogg_probe_tests {
    use super::*;
    use musefs_format::ogg::page_test_support::{
        build_header_pub, lace_packet_pub, vorbis_body_empty,
    };
    use std::io::Write;

    #[test]
    fn probe_detects_opus_and_seeds_tags() {
        let head = b"OpusHead\x01\x02\x38\x01\x80\xbb\x00\x00\x00\x00\x00".to_vec();
        let mut tags = b"OpusTags".to_vec();
        tags.extend_from_slice(&vorbis_body_empty());
        let (mut bytes, _) = build_header_pub(0x1234, &[&head, &tags]);
        let (audio, _) = lace_packet_pub(0x1234, 2, false, 960, &[0u8; 100]);
        bytes.extend_from_slice(&audio);

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("song.opus");
        std::fs::File::create(&path)
            .unwrap()
            .write_all(&bytes)
            .unwrap();

        let probed = probe_full(&path, &bytes).expect("opus should probe");
        assert_eq!(probed.format, Format::Opus);
        assert_eq!(probed.audio_offset, (bytes.len() - audio.len()) as u64);
    }

    #[test]
    fn scan_single_opus_file_ingests_it() {
        let head = b"OpusHead\x01\x02\x38\x01\x80\xbb\x00\x00\x00\x00\x00".to_vec();
        let mut tags = b"OpusTags".to_vec();
        tags.extend_from_slice(&vorbis_body_empty());
        let (mut bytes, _) = build_header_pub(0x1234, &[&head, &tags]);
        let (audio, _) = lace_packet_pub(0x1234, 2, false, 960, &[0u8; 100]);
        bytes.extend_from_slice(&audio);

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("single.opus");
        std::io::Write::write_all(&mut std::fs::File::create(&path).unwrap(), &bytes).unwrap();

        let db = musefs_db::Db::open_in_memory().unwrap();
        // Pass the FILE path directly (not the directory).
        let stats = crate::scan_directory(&db, &path).unwrap();
        assert_eq!(stats.scanned, 1);
        assert_eq!(stats.skipped, 0);
    }

    #[test]
    fn probe_recognizes_oga_alias() {
        let head = b"OpusHead\x01\x02\x38\x01\x80\xbb\x00\x00\x00\x00\x00".to_vec();
        let mut tags = b"OpusTags".to_vec();
        tags.extend_from_slice(&vorbis_body_empty());
        let (mut bytes, _) = build_header_pub(0x1234, &[&head, &tags]);
        let (audio, _) = lace_packet_pub(0x1234, 2, false, 960, &[0u8; 100]);
        bytes.extend_from_slice(&audio);

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("song.oga");
        std::fs::File::create(&path)
            .unwrap()
            .write_all(&bytes)
            .unwrap();

        let probed = probe_full(&path, &bytes).expect("oga should probe");
        assert_eq!(probed.format, Format::Opus);
    }
}

#[cfg(test)]
mod wav_probe_tests {
    use super::*;
    use std::io::Write;

    fn build_wav() -> Vec<u8> {
        let mut fmt = Vec::new();
        fmt.extend_from_slice(&1u16.to_le_bytes());
        fmt.extend_from_slice(&1u16.to_le_bytes());
        fmt.extend_from_slice(&44_100u32.to_le_bytes());
        fmt.extend_from_slice(&88_200u32.to_le_bytes());
        fmt.extend_from_slice(&2u16.to_le_bytes());
        fmt.extend_from_slice(&16u16.to_le_bytes());

        let data = vec![0u8; 16];
        let mut body = Vec::new();
        for (id, payload) in [(b"fmt ", &fmt), (b"data", &data)] {
            body.extend_from_slice(id);
            body.extend_from_slice(&(payload.len() as u32).to_le_bytes());
            body.extend_from_slice(payload);
        }
        let mut out = b"RIFF".to_vec();
        out.extend_from_slice(&((body.len() + 4) as u32).to_le_bytes());
        out.extend_from_slice(b"WAVE");
        out.extend_from_slice(&body);
        out
    }

    #[test]
    fn probe_detects_wav() {
        let bytes = build_wav();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("song.wav");
        std::fs::File::create(&path)
            .unwrap()
            .write_all(&bytes)
            .unwrap();

        let probed = probe_full(&path, &bytes).expect("wav should probe");
        assert_eq!(probed.format, Format::Wav);
        assert_eq!(probed.audio_length, 16);
    }

    #[test]
    fn scan_single_wav_file_ingests_it() {
        let bytes = build_wav();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("single.wav");
        std::fs::File::create(&path)
            .unwrap()
            .write_all(&bytes)
            .unwrap();

        let db = musefs_db::Db::open_in_memory().unwrap();
        let stats = crate::scan_directory(&db, &path).unwrap();
        assert_eq!(stats.scanned, 1);
        assert_eq!(stats.skipped, 0);
    }
}

#[cfg(test)]
mod hardening_tests {
    use super::*;

    #[test]
    fn max_art_bytes_is_16_mib_minus_64_kib() {
        assert_eq!(MAX_ART_BYTES, 16_711_680);
    }

    #[test]
    fn is_supported_audio_accepts_known_and_rejects_unknown() {
        for ok in [
            "a.flac", "a.mp3", "a.m4a", "a.m4b", "a.ogg", "a.oga", "a.opus", "a.wav",
        ] {
            assert!(
                is_supported_audio(std::path::Path::new(ok)),
                "{ok} should be supported"
            );
        }
        for bad in ["a.txt", "a.png", "a", "a.flacx"] {
            assert!(
                !is_supported_audio(std::path::Path::new(bad)),
                "{bad} must be rejected"
            );
        }
    }

    #[test]
    fn collect_audio_skips_unsupported_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("keep.flac"), b"x").unwrap();
        std::fs::write(dir.path().join("skip.txt"), b"x").unwrap();
        let mut out = Vec::new();
        collect_audio(dir.path(), &mut out).unwrap();
        assert_eq!(out.len(), 1);
        assert!(out[0].ends_with("keep.flac"));
    }

    #[test]
    fn probe_returns_none_for_supported_ext_with_garbage_contents() {
        let dir = tempfile::tempdir().unwrap();
        for name in ["bad.flac", "bad.mp3", "bad.m4a", "bad.wav", "bad.opus"] {
            let path = dir.path().join(name);
            std::fs::write(&path, b"not a real audio file").unwrap();
            assert!(
                probe_full(&path, b"not a real audio file").is_none(),
                "{name} must skip"
            );
        }
    }

    fn flac_block(bt: u8, body: &[u8], last: bool) -> Vec<u8> {
        let mut v = vec![(if last { 0x80 } else { 0 }) | (bt & 0x7F)];
        let n = body.len();
        v.extend_from_slice(&[(n >> 16) as u8, (n >> 8) as u8, n as u8]);
        v.extend_from_slice(body);
        v
    }
    fn streaminfo() -> Vec<u8> {
        let mut si = vec![
            0x10, 0x00, 0x10, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x0A, 0xC4, 0x42, 0xF0,
            0x00, 0x00, 0x00, 0x00,
        ];
        si.extend_from_slice(&[0u8; 16]);
        si
    }
    fn vorbis_comment(entries: &[&str]) -> Vec<u8> {
        let mut vc = Vec::new();
        let vendor = b"x";
        vc.extend_from_slice(&(vendor.len() as u32).to_le_bytes());
        vc.extend_from_slice(vendor);
        vc.extend_from_slice(&(entries.len() as u32).to_le_bytes());
        for e in entries {
            vc.extend_from_slice(&(e.len() as u32).to_le_bytes());
            vc.extend_from_slice(e.as_bytes());
        }
        vc
    }
    fn picture(width: u32, height: u32, data: &[u8]) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(&3u32.to_be_bytes());
        let mime = "image/png";
        b.extend_from_slice(&(mime.len() as u32).to_be_bytes());
        b.extend_from_slice(mime.as_bytes());
        b.extend_from_slice(&0u32.to_be_bytes());
        b.extend_from_slice(&width.to_be_bytes());
        b.extend_from_slice(&height.to_be_bytes());
        b.extend_from_slice(&0u32.to_be_bytes());
        b.extend_from_slice(&0u32.to_be_bytes());
        b.extend_from_slice(&(data.len() as u32).to_be_bytes());
        b.extend_from_slice(data);
        b
    }
    fn write_flac(path: &std::path::Path, entries: &[&str], pic: Option<(u32, u32)>) {
        let mut out = b"fLaC".to_vec();
        out.extend(flac_block(0, &streaminfo(), false));
        let last_is_vc = pic.is_none();
        out.extend(flac_block(4, &vorbis_comment(entries), last_is_vc));
        if let Some((w, h)) = pic {
            out.extend(flac_block(6, &picture(w, h, &[0xAB; 64]), true));
        }
        out.extend_from_slice(&[0xCD; 128]);
        std::fs::write(path, &out).unwrap();
    }

    #[test]
    fn ingest_assigns_sequential_ordinals_per_key() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("multi.flac");
        write_flac(&path, &["ARTIST=A1", "ARTIST=A2"], None);
        let db = musefs_db::Db::open_in_memory().unwrap();
        crate::scan_directory(&db, &path).unwrap();
        let track = db.list_tracks().unwrap().into_iter().next().unwrap();
        let mut artists: Vec<(i64, String)> = db
            .get_tags(track.id)
            .unwrap()
            .into_iter()
            .filter(|t| t.key.eq_ignore_ascii_case("artist"))
            .map(|t| (t.ordinal, t.value))
            .collect();
        artists.sort();
        assert_eq!(artists, vec![(0, "A1".to_string()), (1, "A2".to_string())]);
    }

    #[test]
    fn ingest_stores_nonzero_art_dimensions() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("art.flac");
        write_flac(&path, &["ARTIST=A", "TITLE=T"], Some((10, 20)));
        let db = musefs_db::Db::open_in_memory().unwrap();
        crate::scan_directory(&db, &path).unwrap();
        let track = db.list_tracks().unwrap().into_iter().next().unwrap();
        let ta = db.get_track_art(track.id).unwrap();
        assert_eq!(ta.len(), 1);
        let meta = db.get_art_meta(ta[0].art_id).unwrap().unwrap();
        assert_eq!(meta.width, Some(10));
        assert_eq!(meta.height, Some(20));
    }

    #[test]
    fn scan_directory_counts_scanned_and_skipped() {
        let dir = tempfile::tempdir().unwrap();
        write_flac(
            &dir.path().join("ok1.flac"),
            &["ARTIST=A", "TITLE=T1"],
            None,
        );
        write_flac(
            &dir.path().join("ok2.flac"),
            &["ARTIST=A", "TITLE=T2"],
            None,
        );
        std::fs::write(dir.path().join("bad.flac"), b"garbage").unwrap();
        let db = musefs_db::Db::open_in_memory().unwrap();
        let stats = crate::scan_directory(&db, dir.path()).unwrap();
        assert_eq!(stats.scanned, 2);
        assert_eq!(stats.skipped, 1);
    }

    #[test]
    fn revalidate_buckets_unchanged_and_prunes_missing() {
        let dir = tempfile::tempdir().unwrap();
        let keep = dir.path().join("keep.flac");
        write_flac(&keep, &["ARTIST=A", "TITLE=T"], None);
        let db = musefs_db::Db::open_in_memory().unwrap();
        crate::scan_directory(&db, dir.path()).unwrap();

        let s1 = crate::revalidate(&db, dir.path()).unwrap();
        assert_eq!(s1.unchanged, 1);
        assert_eq!(s1.updated, 0);
        assert_eq!(s1.pruned, 0);

        std::fs::remove_file(&keep).unwrap();
        let s2 = crate::revalidate(&db, dir.path()).unwrap();
        assert_eq!(s2.pruned, 1);
        assert!(db.list_tracks().unwrap().is_empty());
    }

    #[test]
    fn revalidate_does_not_prune_on_non_notfound_error() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("real.flac");
        write_flac(&file, &["ARTIST=A", "TITLE=T"], None);
        let db = musefs_db::Db::open_in_memory().unwrap();
        crate::scan_directory(&db, dir.path()).unwrap();

        use musefs_db::{Format, NewTrack};
        let track = db.list_tracks().unwrap().into_iter().next().unwrap();
        db.delete_track(track.id).unwrap();
        let canon = std::fs::canonicalize(dir.path()).unwrap();
        let ghost = canon.join("real.flac").join("ghost.flac");
        db.upsert_track(&NewTrack {
            backing_path: ghost.to_string_lossy().to_string(),
            format: Format::Flac,
            audio_offset: 0,
            audio_length: 0,
            backing_size: 0,
            backing_mtime: 0,
        })
        .unwrap();

        let stats = crate::revalidate(&db, dir.path()).unwrap();
        assert_eq!(stats.pruned, 0, "ENOTDIR is not NotFound → must not prune");
        assert!(
            db.list_tracks()
                .unwrap()
                .iter()
                .any(|t| t.backing_path == ghost.to_string_lossy()),
            "ghost track must still exist"
        );
    }
}

#[cfg(test)]
mod bounded_probe_tests {
    use super::*;
    use musefs_db::Db;

    /// Minimal FLAC: marker + a single last STREAMINFO (34-byte body) + audio.
    /// FLAC has no frame-sync check at the audio offset, so any payload works.
    fn flac_fixture() -> Vec<u8> {
        let mut bytes = b"fLaC".to_vec();
        bytes.push(0x80); // last-block flag set, type 0 (STREAMINFO)
        bytes.extend_from_slice(&[0, 0, 34]); // 24-bit length = 34
        bytes.extend(std::iter::repeat_n(0u8, 34));
        bytes.extend_from_slice(b"AUDIOPAYLOAD");
        bytes
    }

    #[test]
    fn scan_counts_unreadable_file_as_failed_and_continues() {
        let dir = tempfile::tempdir().unwrap();
        // One good FLAC + one zero-byte ".flac" that cannot parse.
        let good = dir.path().join("good.flac");
        let mut bytes = b"fLaC".to_vec();
        bytes.push(0x80);
        bytes.extend_from_slice(&[0, 0, 34]);
        bytes.extend(std::iter::repeat_n(0u8, 34));
        bytes.extend_from_slice(b"AUDIO");
        std::fs::write(&good, &bytes).unwrap();
        std::fs::write(dir.path().join("bad.flac"), b"").unwrap();

        let db = Db::open_in_memory().unwrap();
        let stats = scan_directory(&db, dir.path()).unwrap();
        assert_eq!(stats.scanned, 1);
        assert_eq!(stats.skipped + stats.failed, 1);
    }

    #[test]
    fn scan_directory_bounded_matches_full_for_flac() {
        // A FLAC fixture written to a temp dir, scanned with the (default) bounded
        // path, yields a track with the same audio bounds as a full-file probe.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.flac");
        let bytes = flac_fixture();
        std::fs::write(&path, &bytes).unwrap();

        let full = probe_full(&path, &bytes).expect("full probe");

        let db = Db::open_in_memory().unwrap();
        let stats = scan_directory(&db, dir.path()).unwrap();
        assert_eq!(stats.scanned, 1);
        let track = db
            .get_track_by_path(&std::fs::canonicalize(&path).unwrap().to_string_lossy())
            .unwrap()
            .unwrap();
        assert_eq!(track.audio_offset as u64, full.audio_offset);
        assert_eq!(track.audio_length as u64, full.audio_length);
    }
}
