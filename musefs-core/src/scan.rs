use std::collections::HashMap;
use std::path::{Path, PathBuf};

use musefs_db::{Db, Format, NewArt, NewTrack, Tag, TrackArt};
use musefs_format::{flac, mp3, mp4, ogg, wav, EmbeddedBinaryTag, EmbeddedPicture, Extent};

use crate::byte_budget::ByteBudget;
use crate::error::Result;
use std::sync::mpsc::sync_channel;

const BATCH_FILES: usize = 256;
const BATCH_BYTES: u64 = 64 << 20; // 64 MiB

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

/// Per-frame cap for opaque binary tags, mirroring `MAX_ART_BYTES`. Oversize
/// payloads (e.g. a GEOB embedding a multi-MB file) are logged-and-skipped.
const MAX_BINARY_TAG_BYTES: usize = MAX_ART_BYTES;

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
    binary_tags: Vec<EmbeddedBinaryTag>,
    /// FLAC STREAMINFO/SEEKTABLE as (kind, body) pairs; empty for other formats.
    structural_blocks: Vec<(String, Vec<u8>)>,
}

/// Full-buffer probe (legacy path). Retained as the reference implementation the
/// bounded path is checked against (see the equivalence property test).
pub(crate) fn probe_full(path: &Path, bytes: &[u8]) -> Option<Probed> {
    if has_ext(path, "flac") {
        let scan = flac::locate_audio(bytes).ok()?;
        let (structural_blocks, binary_tags) = flac::split_preserved(&scan.preserved);
        Some(Probed {
            format: Format::Flac,
            audio_offset: scan.audio_offset,
            audio_length: scan.audio_length,
            tags: flac::read_vorbis_comments(bytes).unwrap_or_default(),
            pictures: flac::read_pictures(bytes).unwrap_or_default(),
            binary_tags,
            structural_blocks,
        })
    } else if has_ext(path, "mp3") {
        let bounds = mp3::locate_audio(bytes).ok()?;
        let (binary_tags, promoted) = mp3::read_binary_tags(bytes);
        let mut tags = mp3::read_tags(bytes);
        tags.extend(promoted);
        Some(Probed {
            format: Format::Mp3,
            audio_offset: bounds.audio_offset,
            audio_length: bounds.audio_length,
            tags,
            pictures: mp3::read_pictures(bytes),
            binary_tags,
            structural_blocks: Vec::new(),
        })
    } else if has_ext(path, "m4a") || has_ext(path, "m4b") {
        let bounds = mp4::locate_audio(bytes).ok()?;
        Some(Probed {
            format: Format::M4a,
            audio_offset: bounds.audio_offset,
            audio_length: bounds.audio_length,
            tags: mp4::read_tags(bytes),
            pictures: mp4::read_pictures(bytes),
            binary_tags: mp4::read_binary_tags(bytes),
            structural_blocks: Vec::new(),
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
            binary_tags: Vec::new(),
            structural_blocks: Vec::new(),
        })
    } else if has_ext(path, "wav") {
        let bounds = wav::locate_audio(bytes).ok()?;
        let (binary_tags, promoted) = wav::read_binary_tags(bytes);
        let mut tags = wav::read_tags(bytes);
        tags.extend(promoted);
        Some(Probed {
            format: Format::Wav,
            audio_offset: bounds.audio_offset,
            audio_length: bounds.audio_length,
            tags,
            pictures: wav::read_pictures(bytes),
            binary_tags,
            structural_blocks: Vec::new(),
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
        let scan = match mp4::read_structure_from(&mut f, file_len) {
            Ok(s) => s,
            Err(e) => {
                if matches!(e, mp4::Mp4ScanError::MetadataTooLarge { .. }) {
                    log::warn!("skipping {}: {e}", path.display());
                }
                return Ok(None);
            }
        };
        return Ok(Some(Probed {
            format: Format::M4a,
            audio_offset: scan.mdat_payload_offset,
            audio_length: scan.mdat_payload_len,
            tags: mp4::read_tags(&scan.moov),
            pictures: mp4::read_pictures(&scan.moov),
            binary_tags: mp4::read_binary_tags(&scan.moov),
            structural_blocks: Vec::new(),
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
            Ok(Extent::Complete(meta)) => {
                let (structural_blocks, binary_tags) = flac::split_preserved(&meta.preserved);
                Probe::Done(Probed {
                    format: Format::Flac,
                    audio_offset: meta.audio_offset,
                    audio_length: file_len - meta.audio_offset,
                    tags: flac::read_vorbis_comments(prefix).unwrap_or_default(),
                    pictures: flac::read_pictures(prefix).unwrap_or_default(),
                    binary_tags,
                    structural_blocks,
                })
            }
            Ok(Extent::NeedMore { up_to }) => Probe::NeedMore(up_to),
            Err(_) => Probe::Skip,
        }
    } else if has_ext(path, "mp3") {
        match mp3::locate_audio_bounded(prefix, file_len, tail) {
            Ok(Extent::Complete(b)) => {
                let (binary_tags, promoted) = mp3::read_binary_tags(prefix);
                let mut tags = mp3::read_tags(prefix);
                tags.extend(promoted);
                Probe::Done(Probed {
                    format: Format::Mp3,
                    audio_offset: b.audio_offset,
                    audio_length: b.audio_length,
                    tags,
                    pictures: mp3::read_pictures(prefix),
                    binary_tags,
                    structural_blocks: Vec::new(),
                })
            }
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
                    binary_tags: Vec::new(),
                    structural_blocks: Vec::new(),
                })
            }
            Ok(Extent::NeedMore { up_to }) => Probe::NeedMore(up_to),
            Err(_) => Probe::Skip,
        }
    } else if has_ext(path, "wav") {
        match wav::locate_audio_bounded(prefix, file_len) {
            Ok(Extent::Complete(b)) => {
                let (binary_tags, promoted) = wav::read_binary_tags(prefix);
                let mut tags = wav::read_tags(prefix);
                tags.extend(promoted);
                Probe::Done(Probed {
                    format: Format::Wav,
                    audio_offset: b.audio_offset,
                    audio_length: b.audio_length,
                    tags,
                    pictures: wav::read_pictures(prefix),
                    binary_tags,
                    structural_blocks: Vec::new(),
                })
            }
            Ok(Extent::NeedMore { up_to }) => Probe::NeedMore(up_to),
            Err(_) => Probe::Skip,
        }
    } else {
        Probe::Skip
    }
}

/// Knobs for a scan. `jobs == 0` means "use available parallelism".
#[derive(Debug, Clone, Default)]
pub struct ScanOptions {
    pub jobs: usize,
}

fn effective_jobs(jobs: usize) -> usize {
    if jobs != 0 {
        return jobs;
    }
    std::thread::available_parallelism().map_or(1, std::num::NonZero::get)
}

/// In-flight art-byte budget (and per-batch byte-flush threshold). Overridable via
/// `MUSEFS_BATCH_BYTES` so tests can exercise the backpressure path without 64 MiB
/// of fixture art; defaults to `BATCH_BYTES`.
fn batch_bytes_cap() -> u64 {
    std::env::var("MUSEFS_BATCH_BYTES")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .filter(|&n| n > 0)
        .unwrap_or(BATCH_BYTES)
}

/// One probed file ready to write, plus its art-byte weight for backpressure.
struct Unit {
    abs_path: String,
    meta_len: u64,
    meta_mtime: i64,
    probed: Probed,
    weight: u64,
}

/// In-memory byte weight of a `Probed`, used for batch backpressure
/// (`MUSEFS_BATCH_BYTES`). Counts every buffered payload — pictures plus FLAC
/// structural blocks and binary tags — so large preserved blocks can't slip the
/// budget the way picture-only accounting did.
fn payload_weight(p: &Probed) -> u64 {
    let pictures: u64 = p.pictures.iter().map(|pic| pic.data.len() as u64).sum();
    let binary: u64 = p.binary_tags.iter().map(|t| t.payload.len() as u64).sum();
    let structural: u64 = p
        .structural_blocks
        .iter()
        .map(|(_, body)| body.len() as u64)
        .sum();
    pictures + binary + structural
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

    let binary_tags: Vec<musefs_db::BinaryTag> = probed
        .binary_tags
        .into_iter()
        .filter(|b| !b.payload.is_empty() && b.payload.len() <= MAX_BINARY_TAG_BYTES)
        .enumerate()
        .map(|(ordinal, b)| musefs_db::BinaryTag {
            key: b.key,
            payload: b.payload,
            ordinal: ordinal as i64,
        })
        .collect();
    db.set_binary_tags(track_id, &binary_tags)?;

    let mut sb_ordinals: HashMap<String, i64> = HashMap::new();
    let structural_blocks: Vec<musefs_db::StructuralBlock> = probed
        .structural_blocks
        .into_iter()
        .map(|(kind, body)| {
            let ord = sb_ordinals.entry(kind.clone()).or_insert(0);
            let sb = musefs_db::StructuralBlock {
                kind,
                ordinal: *ord,
                body,
            };
            *ord += 1;
            sb
        })
        .collect();
    db.set_structural_blocks(track_id, &structural_blocks)?;

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

/// Like `ingest`, but writes through a batch `BulkWriter`.
fn ingest_bulk(
    bw: &mut musefs_db::BulkWriter<'_>,
    abs_path: &str,
    meta_len: u64,
    meta_mtime: i64,
    probed: &Probed,
) -> Result<()> {
    let track_id = bw.upsert_track(&NewTrack {
        backing_path: abs_path.to_string(),
        format: probed.format,
        audio_offset: probed.audio_offset as i64,
        audio_length: probed.audio_length as i64,
        backing_size: meta_len as i64,
        backing_mtime: meta_mtime,
    })?;

    let mut tags = Vec::new();
    let mut ordinals: HashMap<String, i64> = HashMap::new();
    for (key, value) in &probed.tags {
        let ord = ordinals.entry(key.clone()).or_insert(0);
        tags.push(Tag::new(key, value, *ord));
        *ord += 1;
    }
    bw.replace_tags(track_id, &tags)?;

    let binary_tags: Vec<musefs_db::BinaryTag> = probed
        .binary_tags
        .iter()
        .filter(|b| !b.payload.is_empty() && b.payload.len() <= MAX_BINARY_TAG_BYTES)
        .enumerate()
        .map(|(ordinal, b)| musefs_db::BinaryTag {
            key: b.key.clone(),
            payload: b.payload.clone(),
            ordinal: ordinal as i64,
        })
        .collect();
    bw.set_binary_tags(track_id, &binary_tags)?;

    let mut sb_ordinals: HashMap<String, i64> = HashMap::new();
    let structural_blocks: Vec<musefs_db::StructuralBlock> = probed
        .structural_blocks
        .iter()
        .map(|(kind, body)| {
            let ord = sb_ordinals.entry(kind.clone()).or_insert(0);
            let sb = musefs_db::StructuralBlock {
                kind: kind.clone(),
                ordinal: *ord,
                body: body.clone(),
            };
            *ord += 1;
            sb
        })
        .collect();
    bw.set_structural_blocks(track_id, &structural_blocks)?;

    let mut track_arts = Vec::new();
    let accepted = probed
        .pictures
        .iter()
        .filter(|p| p.data.len() <= MAX_ART_BYTES);
    for (ordinal, pic) in accepted.enumerate() {
        let art_id = bw.upsert_art(&NewArt {
            mime: pic.mime.clone(),
            width: (pic.width != 0).then_some(pic.width as i64),
            height: (pic.height != 0).then_some(pic.height as i64),
            data: pic.data.clone(),
        })?;
        let picture_type = if pic.picture_type <= 20 {
            pic.picture_type as i64
        } else {
            0
        };
        track_arts.push(TrackArt {
            art_id,
            picture_type,
            description: pic.description.clone(),
            ordinal: ordinal as i64,
        });
    }
    bw.set_track_art(track_id, &track_arts)?;
    Ok(())
}

/// Public entry: parallel-probe / single-writer scan of `root`.
///
/// Insert/update a track row for each supported audio file (FLAC, MP3, M4A,
/// Opus, Vorbis, FLAC-in-Ogg) under `root` (with audio bounds and validation
/// stamps), seeding its tags from the file's existing metadata. `root` may be
/// a single audio file (only that file is scanned) or a directory (walked
/// recursively). Unsupported-format files increment `ScanStats::skipped`; files
/// with a per-file I/O or parse error increment `ScanStats::failed` and do not
/// abort the scan.
pub fn scan_directory_with(db: &Db, root: &Path, opts: &ScanOptions) -> Result<ScanStats> {
    let mut files = Vec::new();
    if root.is_file() {
        if is_supported_audio(root) {
            files.push(root.to_path_buf());
        }
    } else {
        collect_audio(root, &mut files)?;
    }
    db.apply_bulk_pragmas_self()?; // scan-scoped tuning on the caller's connection
    let stats = run_pipeline(db, files, opts)?;
    Ok(stats)
}

/// Back-compat shim used by the CLI and existing tests.
pub fn scan_directory(db: &Db, root: &Path) -> Result<ScanStats> {
    scan_directory_with(db, root, &ScanOptions::default())
}

/// Probe `files` across `jobs` workers (no DB access) and write the results from a
/// single writer (this thread) in batched transactions. Per-file errors are
/// counted, not fatal.
fn run_pipeline(db: &Db, files: Vec<PathBuf>, opts: &ScanOptions) -> Result<ScanStats> {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Arc;

    let jobs = effective_jobs(opts.jobs);
    let cap = batch_bytes_cap();
    let budget = Arc::new(ByteBudget::new(cap));
    let skipped = Arc::new(AtomicU64::new(0));
    let failed = Arc::new(AtomicU64::new(0));

    // Work queue: a shared iterator behind a mutex (cheap; probing dominates).
    let work = Arc::new(std::sync::Mutex::new(files.into_iter()));
    let (tx, rx) = sync_channel::<Unit>(jobs * 2);

    let mut workers = Vec::with_capacity(jobs);
    for _ in 0..jobs {
        let work = Arc::clone(&work);
        let tx = tx.clone();
        let budget = Arc::clone(&budget);
        let skipped = Arc::clone(&skipped);
        let failed = Arc::clone(&failed);
        workers.push(std::thread::spawn(move || loop {
            let next = { work.lock().unwrap().next() };
            let Some(path) = next else { break };
            let Ok(meta) = std::fs::metadata(&path) else {
                failed.fetch_add(1, Ordering::Relaxed);
                continue;
            };
            match probe_file(&path, meta.len()) {
                Ok(Some(probed)) => {
                    let Ok(abs) = std::fs::canonicalize(&path) else {
                        failed.fetch_add(1, Ordering::Relaxed);
                        continue;
                    };
                    let weight = payload_weight(&probed);
                    budget.acquire(weight); // backpressure on in-flight art bytes
                    let unit = Unit {
                        abs_path: abs.to_string_lossy().into_owned(),
                        meta_len: meta.len(),
                        meta_mtime: mtime_secs(&meta),
                        probed,
                        weight,
                    };
                    if tx.send(unit).is_err() {
                        budget.release(weight);
                        break;
                    }
                }
                Ok(None) => {
                    skipped.fetch_add(1, Ordering::Relaxed);
                }
                Err(_) => {
                    failed.fetch_add(1, Ordering::Relaxed);
                }
            }
        }));
    }
    drop(tx); // close the channel once all clones (workers) finish

    // Writer: this thread. Batch by file count and accumulated art bytes.
    let mut scanned = 0u64;
    let mut batch: Vec<Unit> = Vec::new();
    let mut batch_bytes = 0u64;
    let flush = |batch: &mut Vec<Unit>, batch_bytes: &mut u64, scanned: &mut u64| -> Result<()> {
        if batch.is_empty() {
            return Ok(());
        }
        let mut bw = db.bulk_writer()?;
        for u in batch.iter() {
            ingest_bulk(&mut bw, &u.abs_path, u.meta_len, u.meta_mtime, &u.probed)?;
            *scanned += 1;
        }
        bw.commit()?;
        for u in batch.drain(..) {
            budget.release(u.weight);
        }
        *batch_bytes = 0;
        Ok(())
    };

    // Drain the channel, batching by file count and accumulated art bytes. The
    // budget cap equals the byte-flush threshold, so a worker calling
    // `budget.acquire` (which it does *before* `send`) could block while the
    // writer's pending batch sits just below the threshold — if the writer then
    // parked on a blocking `recv`, neither side could make progress (the held
    // budget is never released, the batch never reaches the threshold). To avoid
    // that, whenever the channel momentarily drains we flush the pending batch —
    // releasing the budget so blocked producers proceed — *before* blocking on the
    // next item.
    loop {
        match rx.try_recv() {
            Ok(unit) => {
                batch_bytes += unit.weight;
                batch.push(unit);
                if batch.len() >= BATCH_FILES || batch_bytes >= cap {
                    flush(&mut batch, &mut batch_bytes, &mut scanned)?;
                }
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => {
                flush(&mut batch, &mut batch_bytes, &mut scanned)?;
                match rx.recv() {
                    Ok(unit) => {
                        batch_bytes += unit.weight;
                        batch.push(unit);
                        if batch.len() >= BATCH_FILES || batch_bytes >= cap {
                            flush(&mut batch, &mut batch_bytes, &mut scanned)?;
                        }
                    }
                    Err(_) => break, // all workers finished; channel closed
                }
            }
            Err(std::sync::mpsc::TryRecvError::Disconnected) => break,
        }
    }
    flush(&mut batch, &mut batch_bytes, &mut scanned)?;
    // A fatal flush error above returns via `?` *before* this join, abandoning the
    // worker threads — acceptable because a DB-write failure aborts the whole scan.
    // On the success path every worker has already exited (the work queue drained
    // and `drop(tx)` closed the channel), so these joins return promptly.
    for w in workers {
        let _ = w.join();
    }

    Ok(ScanStats {
        scanned,
        skipped: skipped.load(Ordering::Relaxed),
        failed: failed.load(Ordering::Relaxed),
    })
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

/// Re-validate an already-scanned library root: re-probe only files whose
/// size/mtime changed since the last scan (skipping unchanged ones so external
/// tag edits in the DB are preserved), then delete tracks **under `root`** whose
/// backing file is gone (cascading tags/art links) and garbage-collect
/// now-unreferenced art. `root` may be a single audio file (only that file is
/// revalidated) or a directory (walked recursively). Pruning is scoped to
/// `root`, so revalidating one library root never removes tracks belonging to
/// another.
///
/// Uses `opts` to configure the probe pipeline (e.g. `jobs` for parallelism).
/// The skip-unchanged decision runs on the calling thread before workers are
/// dispatched, so workers remain DB-free. A `stat`/`canonicalize` failure on a
/// candidate during the skip pass is counted in `failed` (and the file is left
/// for the next revalidation) rather than re-probed or pruned.
pub fn revalidate_with(db: &Db, root: &Path, opts: &ScanOptions) -> Result<RevalidateStats> {
    let mut files = Vec::new();
    if root.is_file() {
        if is_supported_audio(root) {
            files.push(root.to_path_buf());
        }
    } else {
        collect_audio(root, &mut files)?;
    }
    db.apply_bulk_pragmas_self()?;

    // Main-thread pre-dispatch skip pass: load existing (path -> size,mtime,id,format) once,
    // stat each candidate, keep only changed files. Workers stay DB-free.
    let existing: HashMap<String, (i64, i64, i64, Format)> = db
        .list_tracks()?
        .into_iter()
        .map(|t| {
            (
                t.backing_path,
                (t.backing_size, t.backing_mtime, t.id, t.format),
            )
        })
        .collect();
    // Legacy backfill (spec §1): FLAC tracks scanned under V1 have no structural
    // blocks. Re-scan them even when the backing file is unchanged so the V2
    // structural store + binary tags get populated by the ingest path.
    let have_structural = db.track_ids_with_structural_blocks()?;

    let mut unchanged = 0u64;
    let mut skip_failed = 0u64;
    let mut changed: Vec<PathBuf> = Vec::new();
    for path in files {
        let Ok(meta) = std::fs::metadata(&path) else {
            skip_failed += 1;
            continue;
        };
        let Ok(abs) = std::fs::canonicalize(&path) else {
            skip_failed += 1;
            continue;
        };
        let key = abs.to_string_lossy().to_string();
        if let Some(&(size, mtime, id, format)) = existing.get(&key) {
            let needs_backfill = format == Format::Flac && !have_structural.contains(&id);
            if size == meta.len() as i64 && mtime == mtime_secs(&meta) && !needs_backfill {
                unchanged += 1;
                continue;
            }
        }
        changed.push(path);
    }

    let scan = run_pipeline(db, changed, opts)?;

    // Prune + GC on the writer connection (single-threaded), unchanged from before.
    let canon_root = std::fs::canonicalize(root)?;
    let mut pruned = 0u64;
    for track in db.list_tracks()? {
        if !Path::new(&track.backing_path).starts_with(&canon_root) {
            continue;
        }
        if let Err(e) = std::fs::metadata(&track.backing_path) {
            if e.kind() == std::io::ErrorKind::NotFound {
                db.delete_track(track.id)?;
                pruned += 1;
            }
        }
    }
    db.gc_orphan_art()?;

    Ok(RevalidateStats {
        updated: scan.scanned,
        unchanged,
        pruned,
        failed: scan.failed + skip_failed,
    })
}

/// Back-compat shim used by the CLI and existing tests.
pub fn revalidate(db: &Db, root: &Path) -> Result<RevalidateStats> {
    revalidate_with(db, root, &ScanOptions::default())
}

#[cfg(test)]
mod scan_unit_tests {
    use super::*;
    use std::io::Write;
    use std::sync::Mutex;

    /// Env is process-global: serialize the env-mutating tests so they never
    /// observe each other's `MUSEFS_*` vars.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    // --- scan_window() / WINDOW (lines 16, 149-154) ---

    // kills scan L16 WINDOW `<<`→`>>` (default must be 1<<20, not 1>>20==0)
    // kills scan L153 filter `>`→`>=`/`==`/`<`
    #[test]
    fn scan_window_default_and_env() {
        let _g = ENV_LOCK.lock().unwrap();
        // Default (unset): WINDOW == 1<<20. `1>>20` == 0 → distinguishes the shift.
        std::env::remove_var("MUSEFS_SCAN_WINDOW");
        assert_eq!(scan_window(), 1 << 20);
        assert_eq!(scan_window(), 1_048_576);

        // "0" is filtered out (`0 > 0` is false) → falls back to WINDOW.
        // Under `>=`/`==`, `0` would be kept and returned (wrong).
        std::env::set_var("MUSEFS_SCAN_WINDOW", "0");
        assert_eq!(
            scan_window(),
            1 << 20,
            "zero must be filtered → default window"
        );

        // "5" passes the filter (`5 > 0`) → returned verbatim. Under `<`,
        // `5 < 0` is false → 5 would be filtered → default (wrong).
        std::env::set_var("MUSEFS_SCAN_WINDOW", "5");
        assert_eq!(scan_window(), 5, "positive override must pass the filter");

        std::env::remove_var("MUSEFS_SCAN_WINDOW");
    }

    // --- read_tail_128() (lines 170-178) ---

    fn write_temp(name: &str, bytes: &[u8]) -> (tempfile::TempDir, std::fs::File) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(name);
        std::fs::File::create(&path)
            .unwrap()
            .write_all(bytes)
            .unwrap();
        let file = std::fs::File::open(&path).unwrap();
        (dir, file)
    }

    // kills scan L171 `<`→`<=` (128-byte file must be Some)
    // kills scan L172 Ok(None) constant, L178 Ok(Some) value
    // kills scan L176 `file_len - 128`→`/` (offset 0 vs 1 shifts the bytes)
    // kills scan L175 buf init [0;128]/[1;128] constants (exact bytes asserted)
    #[test]
    fn read_tail_128_exact_128_bytes() {
        // Distinct, position-sensitive pattern: byte[i] = i (0..=127).
        let pattern: Vec<u8> = (0u8..128).collect();
        let (_dir, file) = write_temp("tail128.bin", &pattern);

        let tail = read_tail_128(&file, 128).unwrap();
        let expected: [u8; 128] = pattern.clone().try_into().unwrap();
        // Exact equality kills:
        //  - Ok(None) (would be None, not Some)
        //  - [0;128]/[1;128] buf-init constants (would mismatch the pattern)
        //  - `<`→`<=` (128<=128 true → returns None for a 128-byte file)
        //  - `-`→`/` (offset 128/128==1 reads bytes[1..], shifting the pattern)
        assert_eq!(tail, Some(expected));
    }

    // kills scan L171 `<`→`<=` boundary the other way (127 bytes → None)
    #[test]
    fn read_tail_128_short_file_is_none() {
        let (_dir, file) = write_temp("tail127.bin", &[0xABu8; 127]);
        assert_eq!(read_tail_128(&file, 127).unwrap(), None);
    }

    // --- effective_jobs() (lines 313-318) ---

    // kills scan L314 effective_jobs body→1 (assuming parallelism > 1)
    #[test]
    fn effective_jobs_zero_uses_parallelism_and_nonzero_passes_through() {
        let par = std::thread::available_parallelism().map_or(1, std::num::NonZero::get);
        assert_eq!(effective_jobs(0), par);
        assert_eq!(effective_jobs(4), 4);
        assert_eq!(effective_jobs(1), 1);
    }

    // --- batch_bytes_cap() / BATCH_BYTES (lines 323-329) ---

    // kills scan L324 batch_bytes_cap body→0/→1 (default must be BATCH_BYTES)
    // kills scan L327 filter `>`→`>=`/`==`/`<`
    #[test]
    fn batch_bytes_cap_default_and_env() {
        let _g = ENV_LOCK.lock().unwrap();
        std::env::remove_var("MUSEFS_BATCH_BYTES");
        assert_eq!(batch_bytes_cap(), BATCH_BYTES);
        assert_eq!(batch_bytes_cap(), 64 << 20);
        assert_eq!(batch_bytes_cap(), 67_108_864);

        // "0" filtered (`0 > 0` false) → default. Kills `>=`/`==`.
        std::env::set_var("MUSEFS_BATCH_BYTES", "0");
        assert_eq!(batch_bytes_cap(), BATCH_BYTES);

        // "5" passes (`5 > 0`) → 5. Kills `<`.
        std::env::set_var("MUSEFS_BATCH_BYTES", "5");
        assert_eq!(batch_bytes_cap(), 5);

        std::env::remove_var("MUSEFS_BATCH_BYTES");
    }

    // --- payload_weight() ---

    // Sums picture + binary-tag + structural-block byte lengths (batch backpressure).
    #[test]
    fn payload_weight_sums_all_buffered_payloads() {
        let pic = |n: usize| EmbeddedPicture {
            mime: "image/png".to_string(),
            picture_type: 3,
            description: String::new(),
            width: 0,
            height: 0,
            data: vec![0u8; n],
        };
        let probed = Probed {
            format: Format::Flac,
            audio_offset: 0,
            audio_length: 0,
            tags: Vec::new(),
            pictures: vec![pic(3), pic(5)],
            binary_tags: vec![EmbeddedBinaryTag {
                key: "APPLICATION".into(),
                payload: vec![0u8; 4],
            }],
            structural_blocks: vec![("SEEKTABLE".into(), vec![0u8; 2])],
        };
        // 3 + 5 (pictures) + 4 (binary) + 2 (structural) = 14.
        assert_eq!(payload_weight(&probed), 14);

        // Empty → 0, distinguishes the →1 constant (which ignores the input).
        let empty = Probed {
            format: Format::Flac,
            audio_offset: 0,
            audio_length: 0,
            tags: Vec::new(),
            pictures: Vec::new(),
            binary_tags: Vec::new(),
            structural_blocks: Vec::new(),
        };
        assert_eq!(payload_weight(&empty), 0);
    }

    /// Minimal-but-valid m4a that `mp4::locate_audio` accepts (one `soun` trak),
    /// with a `udta/meta/ilst` carrying one binary `----` atom. `value` is the raw
    /// binary `data` payload (type code 0). Not synthesis-grade (no stco), but
    /// `probe_full` only locates audio + reads tags, never synthesizes.
    fn mp4_with_binary_freeform(mean: &str, name: &str, value: &[u8]) -> Vec<u8> {
        fn bx(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
            let mut v = ((8 + body.len()) as u32).to_be_bytes().to_vec();
            v.extend_from_slice(kind);
            v.extend_from_slice(body);
            v
        }
        // mdia/hdlr with handler type `soun` at payload offset 8..12 (FullBox
        // version/flags [0..4], pre_defined [4..8], handler_type [8..12]).
        let mut hdlr_body = vec![0u8; 8];
        hdlr_body.extend_from_slice(b"soun");
        hdlr_body.extend_from_slice(&[0u8; 12]); // reserved(12) + empty name
        let trak = bx(b"trak", &bx(b"mdia", &bx(b"hdlr", &hdlr_body)));

        // udta/meta/ilst with one binary `----` atom.
        let mut mean_body = 0u32.to_be_bytes().to_vec();
        mean_body.extend_from_slice(mean.as_bytes());
        let mut name_body = 0u32.to_be_bytes().to_vec();
        name_body.extend_from_slice(name.as_bytes());
        let mut data_body = 0u32.to_be_bytes().to_vec(); // type 0 = binary
        data_body.extend_from_slice(&0u32.to_be_bytes()); // locale
        data_body.extend_from_slice(value);
        let mut free = bx(b"mean", &mean_body);
        free.extend(bx(b"name", &name_body));
        free.extend(bx(b"data", &data_body));
        let ilst = bx(b"ilst", &bx(b"----", &free));
        let mut meta = 0u32.to_be_bytes().to_vec();
        meta.extend(bx(b"hdlr", &[0u8; 25]));
        meta.extend(ilst);
        let udta = bx(b"udta", &bx(b"meta", &meta));

        let moov = bx(b"moov", &[trak, udta].concat());
        [bx(b"ftyp", b"M4A "), moov, bx(b"mdat", b"AUDIODATA")].concat()
    }

    #[test]
    fn probe_full_surfaces_mp4_binary_freeform() {
        use musefs_format::mp4;
        let bytes = mp4_with_binary_freeform("com.serato.dj", "analysis", &[0x00, 0xAB, 0xCD]);
        let probed = probe_full(std::path::Path::new("/x.m4a"), &bytes).expect("probed");
        assert_eq!(probed.format, Format::M4a);
        let keys: Vec<&str> = probed.binary_tags.iter().map(|b| b.key.as_str()).collect();
        assert!(
            keys.contains(&"----:com.serato.dj:analysis"),
            "binary freeform not surfaced: {keys:?}"
        );
        let bt = probed
            .binary_tags
            .iter()
            .find(|b| b.key == "----:com.serato.dj:analysis")
            .unwrap();
        assert_eq!(bt.payload, vec![0x00, 0xAB, 0xCD]);
        let scan = mp4::read_structure(&bytes).unwrap();
        assert_eq!(probed.audio_offset, scan.mdat_payload_offset);
    }
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

    #[test]
    fn scan_ingests_binary_tags_and_promotes() {
        use id3::frame::{Content, Popularimeter, Unknown};
        use id3::{Encoder, Frame, Tag, TagLike, Version};

        let dir = tempfile::tempdir().unwrap();

        // Build an MP3 with a PRIV (opaque) + POPM (promoted) tag.
        let mut tag = Tag::new();
        tag.add_frame(Popularimeter {
            user: "u".into(),
            rating: 128,
            counter: 3,
        });
        tag.add_frame(Frame::with_content(
            "PRIV",
            Content::Unknown(Unknown {
                data: vec![1, 1, 2, 3, 5],
                version: Version::Id3v24,
            }),
        ));
        let mut bytes = Vec::new();
        Encoder::new()
            .version(Version::Id3v24)
            .encode(&tag, &mut bytes)
            .unwrap();
        // A real MP3 frame header is enough for locate_audio_bounded to find audio.
        bytes.extend_from_slice(&[0xFF, 0xFB, 0x90, 0x00, 0x00, 0x00, 0x00, 0x00]);
        std::fs::write(dir.path().join("a.mp3"), &bytes).unwrap();

        let db = musefs_db::Db::open_in_memory().unwrap();
        crate::scan::scan_directory(&db, dir.path()).unwrap();
        let track = db.list_tracks().unwrap().into_iter().next().unwrap();
        let tid = track.id;

        // Opaque PRIV survives as a binary row.
        let bin = db.get_binary_tags(tid).unwrap();
        assert!(
            bin.iter().any(|r| r.key == "PRIV" && r.byte_len == 5),
            "PRIV not ingested as binary row; got: {bin:?}"
        );

        // POPM promoted into editable text tags.
        let texts = db.get_tags(tid).unwrap();
        assert!(
            texts.iter().any(|t| t.key == "rating" && t.value == "128"),
            "rating not promoted; got: {texts:?}"
        );
        assert!(
            texts.iter().any(|t| t.key == "playcount" && t.value == "3"),
            "playcount not promoted; got: {texts:?}"
        );
    }

    /// Probed carrying a valid, an empty, and an oversize binary tag. Only the
    /// valid one is stored: the filter drops empty (`EmptySegment` would fail
    /// layout validation) and oversize (`> MAX_BINARY_TAG_BYTES`) payloads, with
    /// gap-free ordinals.
    fn probed_with_mixed_binary_tags() -> Probed {
        Probed {
            format: musefs_db::Format::Mp3,
            audio_offset: 0,
            audio_length: 0,
            tags: Vec::new(),
            pictures: Vec::new(),
            binary_tags: vec![
                EmbeddedBinaryTag {
                    key: "PRIV".into(),
                    payload: vec![1, 2, 3],
                },
                EmbeddedBinaryTag {
                    key: "GEOB".into(),
                    payload: Vec::new(),
                },
                EmbeddedBinaryTag {
                    key: "SYLT".into(),
                    payload: vec![0u8; MAX_BINARY_TAG_BYTES + 1],
                },
            ],
            structural_blocks: Vec::new(),
        }
    }

    #[test]
    fn ingest_filters_empty_and_oversize_binary_tags() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.mp3");
        std::fs::write(&path, b"x").unwrap();
        let meta = std::fs::metadata(&path).unwrap();
        let db = Db::open_in_memory().unwrap();

        ingest(
            &db,
            &path.to_string_lossy(),
            &meta,
            probed_with_mixed_binary_tags(),
        )
        .unwrap();

        let tid = db.list_tracks().unwrap()[0].id;
        let rows = db.get_binary_tags(tid).unwrap();
        assert_eq!(
            rows.len(),
            1,
            "only the valid binary tag survives: {rows:?}"
        );
        assert_eq!(rows[0].key, "PRIV");
        assert_eq!(rows[0].byte_len, 3);
    }

    #[test]
    fn ingest_bulk_filters_empty_and_oversize_binary_tags() {
        let db = Db::open_in_memory().unwrap();
        {
            let mut bw = db.bulk_writer().unwrap();
            ingest_bulk(&mut bw, "/a.mp3", 1, 0, &probed_with_mixed_binary_tags()).unwrap();
            bw.commit().unwrap();
        }
        let tid = db.list_tracks().unwrap()[0].id;
        let rows = db.get_binary_tags(tid).unwrap();
        assert_eq!(
            rows.len(),
            1,
            "only the valid binary tag survives: {rows:?}"
        );
        assert_eq!(rows[0].key, "PRIV");
        assert_eq!(rows[0].byte_len, 3);
    }

    /// Probed with two structural blocks of the SAME kind, to make the per-kind
    /// ordinal increment (`*ord += 1`) observable. A real FLAC carries only one
    /// STREAMINFO/SEEKTABLE, so a duplicate kind is the only input under which the
    /// second block's ordinal differs from the first; without it the increment's
    /// mutants survive.
    fn probed_with_duplicate_structural_kind() -> Probed {
        Probed {
            format: musefs_db::Format::Flac,
            audio_offset: 0,
            audio_length: 0,
            tags: Vec::new(),
            pictures: Vec::new(),
            binary_tags: Vec::new(),
            structural_blocks: vec![
                ("SEEKTABLE".to_string(), vec![0xA1]),
                ("SEEKTABLE".to_string(), vec![0xB2]),
            ],
        }
    }

    #[test]
    fn ingest_assigns_sequential_structural_ordinals_per_kind() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.flac");
        std::fs::write(&path, b"x").unwrap();
        let meta = std::fs::metadata(&path).unwrap();
        let db = Db::open_in_memory().unwrap();

        ingest(
            &db,
            &path.to_string_lossy(),
            &meta,
            probed_with_duplicate_structural_kind(),
        )
        .unwrap();

        let tid = db.list_tracks().unwrap()[0].id;
        let got = db.get_structural_blocks(tid).unwrap();
        // Rows come back ORDER BY kind, ordinal: the two same-kind blocks must hold
        // ordinals 0 then 1 (the `-=`/`*=` mutants collapse or invert this).
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].ordinal, 0);
        assert_eq!(got[0].body, vec![0xA1]);
        assert_eq!(got[1].ordinal, 1);
        assert_eq!(got[1].body, vec![0xB2]);
    }

    #[test]
    fn ingest_bulk_assigns_sequential_structural_ordinals_per_kind() {
        let db = Db::open_in_memory().unwrap();
        {
            let mut bw = db.bulk_writer().unwrap();
            ingest_bulk(
                &mut bw,
                "/a.flac",
                1,
                0,
                &probed_with_duplicate_structural_kind(),
            )
            .unwrap();
            bw.commit().unwrap();
        }
        let tid = db.list_tracks().unwrap()[0].id;
        let got = db.get_structural_blocks(tid).unwrap();
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].ordinal, 0);
        assert_eq!(got[0].body, vec![0xA1]);
        assert_eq!(got[1].ordinal, 1);
        assert_eq!(got[1].body, vec![0xB2]);
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

    #[test]
    fn revalidate_skips_unchanged_and_reprobes_changed() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("x.flac");
        let mk = |audio: &[u8]| {
            let mut b = b"fLaC".to_vec();
            b.push(0x80);
            b.extend_from_slice(&[0, 0, 34]);
            b.extend(std::iter::repeat_n(0u8, 34));
            b.extend_from_slice(audio);
            b
        };
        std::fs::write(&p, mk(b"AUDIO")).unwrap();
        let db = Db::open_in_memory().unwrap();
        scan_directory(&db, dir.path()).unwrap();

        // Unchanged → all unchanged.
        let s1 = revalidate_with(&db, dir.path(), &ScanOptions::default()).unwrap();
        assert_eq!(s1.unchanged, 1);
        assert_eq!(s1.updated, 0);

        // Rewrite with a different size → detected as changed and re-probed.
        std::fs::write(&p, mk(b"DIFFERENT-AUDIO")).unwrap();
        let s2 = revalidate_with(&db, dir.path(), &ScanOptions::default()).unwrap();
        assert_eq!(s2.updated, 1);
        assert_eq!(s2.unchanged, 0);
        // The track row now reflects the new (longer) audio length.
        let track = db
            .get_track_by_path(&std::fs::canonicalize(&p).unwrap().to_string_lossy())
            .unwrap()
            .unwrap();
        assert_eq!(track.audio_length as usize, b"DIFFERENT-AUDIO".len());
    }

    #[test]
    fn revalidate_accepts_a_single_file_target() {
        // The CLI advertises file targets for every scan, including --revalidate,
        // so revalidate_with must handle a bare file root (not just a directory).
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("x.flac");
        let mut bytes = b"fLaC".to_vec();
        bytes.push(0x80);
        bytes.extend_from_slice(&[0, 0, 34]);
        bytes.extend(std::iter::repeat_n(0u8, 34));
        bytes.extend_from_slice(b"AUDIO");
        std::fs::write(&p, &bytes).unwrap();
        let db = Db::open_in_memory().unwrap();
        scan_directory(&db, dir.path()).unwrap();

        // Revalidate the file path directly: must not error on read_dir and the
        // unchanged file is bucketed as unchanged (not pruned).
        let stats = revalidate_with(&db, &p, &ScanOptions::default()).unwrap();
        assert_eq!(stats.unchanged, 1);
        assert_eq!(stats.pruned, 0);
        assert_eq!(db.list_tracks().unwrap().len(), 1);
    }

    #[test]
    fn jobs1_and_jobs_n_produce_equivalent_state() {
        let dir = tempfile::tempdir().unwrap();
        // A handful of distinct FLACs.
        for i in 0..12 {
            let mut bytes = b"fLaC".to_vec();
            bytes.push(0x80);
            bytes.extend_from_slice(&[0, 0, 34]);
            bytes.extend(std::iter::repeat_n(0u8, 34));
            bytes.extend_from_slice(format!("AUDIO-{i}").as_bytes());
            std::fs::write(dir.path().join(format!("t{i}.flac")), &bytes).unwrap();
        }
        let norm = |jobs: usize| {
            let db = Db::open_in_memory().unwrap();
            scan_directory_with(&db, dir.path(), &ScanOptions { jobs }).unwrap();
            let mut rows: Vec<(String, i64, i64)> = db
                .list_tracks()
                .unwrap()
                .into_iter()
                .map(|t| (t.backing_path, t.audio_offset, t.audio_length))
                .collect();
            rows.sort();
            rows
        };
        assert_eq!(norm(1), norm(4));
        assert_eq!(norm(1).len(), 12);
    }
}
