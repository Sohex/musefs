use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

use musefs_db::convert::usize_from;
use musefs_db::{Db, Format, NewArt, NewTrack, Tag, TrackArt};
use musefs_format::{EmbeddedBinaryTag, EmbeddedPicture, Extent, flac, mp3, mp4, ogg, wav};

use crate::byte_budget::ByteBudget;
use crate::error::Result;
use crate::freshness::BackingStamp;
use std::sync::mpsc::sync_channel;

const BATCH_FILES: usize = 256;
const BATCH_BYTES: u64 = 64 << 20; // 64 MiB

/// Initial bounded-read window. Covers typical metadata + cover art; a larger
/// metadata region triggers a `NeedMore` widen.
const WINDOW: usize = 1 << 20; // 1 MiB
/// Cap on widen iterations before falling back to a full-buffer read.
const MAX_WIDEN_RETRIES: usize = 8;
/// Hard ceiling on bytes read to probe one file. Real audio metadata fits far
/// below this, so a file still unparsed past the cap is treated as malformed
/// rather than read whole into RAM. Guards against a multi-GB file misnamed with
/// an audio extension, and against a corrupt header whose length field demands a
/// giant `NeedMore` widen.
pub(crate) const MAX_PROBE_BYTES: u64 = 64 << 20; // 64 MiB

/// The artwork-size ceiling. Enforced here at ingest (oversize scanned art is
/// dropped) and at resolve in `mapping::track_art_to_inputs` (oversize art from
/// any writer is rejected). Sized to clear FLAC's 24-bit block length with
/// headroom for the picture-block framing.
pub(crate) const MAX_ART_BYTES: usize = 16 * 1024 * 1024 - 64 * 1024;

/// Per-frame cap for opaque binary tags, mirroring `MAX_ART_BYTES`. Oversize
/// payloads (e.g. a GEOB embedding a multi-MB file) are logged-and-skipped.
const MAX_BINARY_TAG_BYTES: usize = MAX_ART_BYTES;

/// Outcome of probing one backing file. `Unparseable` is a supported-extension
/// file whose bytes did not parse (counted as a scan `failed`). `Raced` means
/// the file changed under us between the pre- and post-probe `fstat` — the probe
/// may be torn, so nothing is committed for it (#276).
#[derive(Debug)]
enum ProbeOutcome {
    Probed(Probed, BackingStamp),
    Unparseable,
    Raced,
}

#[cfg(test)]
thread_local! {
    static AFTER_S1_HOOK: std::cell::RefCell<Option<Box<dyn FnMut()>>> =
        const { std::cell::RefCell::new(None) };
}
#[cfg(test)]
fn fire_after_s1() {
    AFTER_S1_HOOK.with(|h| {
        if let Some(f) = h.borrow_mut().as_mut() {
            f();
        }
    });
}
#[cfg(test)]
fn set_after_s1_hook(f: impl FnMut() + 'static) {
    AFTER_S1_HOOK.with(|h| *h.borrow_mut() = Some(Box::new(f)));
}
#[cfg(test)]
fn clear_after_s1_hook() {
    AFTER_S1_HOOK.with(|h| *h.borrow_mut() = None);
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanStats {
    pub scanned: u64,
    pub skipped: u64,
    pub failed: u64,
    pub raced: u64,
}

/// Per-extension tally of files skipped during the directory walk because their
/// extension is not a supported audio format. Backs the end-of-scan summary log
/// line (#341) that breaks the single `skipped` count down by extension, so an
/// operator can tell expected sidecars (cover art, `.cue`, `.log`, `.nfo`) from
/// genuinely unexpected files. Not part of `ScanStats`: the breakdown is
/// log-only and does not affect the CLI summary.
#[derive(Debug, Default)]
struct SkipTally {
    total: u64,
    by_ext: BTreeMap<String, u64>,
}

impl SkipTally {
    /// Record one skipped file, bucketed by its lowercased extension
    /// (`<none>` when the file has no extension or a non-UTF-8 one).
    fn record(&mut self, path: &Path) {
        self.total += 1;
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map_or_else(|| "<none>".to_string(), str::to_ascii_lowercase);
        *self.by_ext.entry(ext).or_insert(0) += 1;
    }

    /// The end-of-scan summary line, e.g. `skipped 42: jpg=20, cue=10, log=8,
    /// <none>=4` — buckets ordered by descending count, ties broken by extension
    /// name. `None` when nothing was skipped, so there is no line to emit.
    fn summary(&self) -> Option<String> {
        if self.total == 0 {
            return None;
        }
        let mut buckets: Vec<(&String, &u64)> = self.by_ext.iter().collect();
        buckets.sort_by(|a, b| b.1.cmp(a.1).then_with(|| a.0.cmp(b.0)));
        let breakdown = buckets
            .iter()
            .map(|(ext, n)| format!("{ext}={n}"))
            .collect::<Vec<_>>()
            .join(", ");
        Some(format!("skipped {}: {breakdown}", self.total))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RevalidateStats {
    pub updated: u64,
    pub unchanged: u64,
    pub pruned: u64,
    pub failed: u64,
    pub raced: u64,
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

fn collect_audio(
    root: &Path,
    out: &mut Vec<PathBuf>,
    follow_symlinks: bool,
) -> std::io::Result<SkipTally> {
    let mut visited = HashSet::new();
    let mut files_visited = HashSet::new();
    let mut tally = SkipTally::default();
    if follow_symlinks {
        // Seed with the root's identity so a symlink pointing back to it is
        // caught as a cycle on the first descent.
        if let Ok(meta) = std::fs::metadata(root) {
            visited.insert(dir_key(&meta));
        }
    }
    collect_audio_inner(
        root,
        out,
        follow_symlinks,
        &mut visited,
        &mut files_visited,
        &mut tally,
    )?;
    Ok(tally)
}

fn collect_audio_inner(
    root: &Path,
    out: &mut Vec<PathBuf>,
    follow_symlinks: bool,
    visited: &mut HashSet<(u64, u64)>,
    files_visited: &mut HashSet<(u64, u64)>,
    tally: &mut SkipTally,
) -> std::io::Result<()> {
    for entry in std::fs::read_dir(root)? {
        let entry = entry?;
        let path = entry.path();
        let ftype = entry.file_type()?;
        if ftype.is_dir() {
            descend(&path, out, follow_symlinks, visited, files_visited, tally)?;
        } else if ftype.is_file() {
            if is_supported_audio(&path) {
                push_file(&path, out, follow_symlinks, files_visited, None);
            } else {
                tally.record(&path);
            }
        } else if ftype.is_symlink() {
            if !follow_symlinks {
                log::warn!(
                    "skipping symlink {} (pass --follow-symlinks to scan it)",
                    path.display()
                );
                continue;
            }
            match std::fs::metadata(&path) {
                Ok(meta) if meta.is_dir() => {
                    descend(&path, out, follow_symlinks, visited, files_visited, tally)?;
                }
                Ok(meta) if meta.is_file() => {
                    if is_supported_audio(&path) {
                        push_file(&path, out, follow_symlinks, files_visited, Some(&meta));
                    } else {
                        tally.record(&path);
                    }
                }
                Ok(_) => {}
                Err(e) => {
                    log::warn!("skipping broken symlink {}: {e}", path.display());
                }
            }
        }
    }
    Ok(())
}

fn descend(
    path: &Path,
    out: &mut Vec<PathBuf>,
    follow_symlinks: bool,
    visited: &mut HashSet<(u64, u64)>,
    files_visited: &mut HashSet<(u64, u64)>,
    tally: &mut SkipTally,
) -> std::io::Result<()> {
    if !follow_symlinks {
        return collect_audio_inner(path, out, follow_symlinks, visited, files_visited, tally);
    }
    let meta = match std::fs::metadata(path) {
        Ok(m) => m,
        Err(e) => {
            log::warn!("skipping directory {}: {e}", path.display());
            return Ok(());
        }
    };
    if !visited.insert(dir_key(&meta)) {
        log::warn!("skipping symlink cycle at {}", path.display());
        return Ok(());
    }
    collect_audio_inner(path, out, follow_symlinks, visited, files_visited, tally)
}

fn dir_key(meta: &std::fs::Metadata) -> (u64, u64) {
    use std::os::unix::fs::MetadataExt;
    (meta.dev(), meta.ino())
}

/// Collect one supported-extension file into `out`, deduplicating by target
/// identity when following symlinks so a real file and a symlink to it (or a
/// file reached via two symlink paths) are ingested once. `known_meta` is the
/// already-resolved target metadata when the caller has it (the symlink arm),
/// avoiding a second `stat`. Dedup is best-effort: if the target cannot be
/// `stat`ed we push it and let the probe pipeline count it rather than dropping
/// it silently.
fn push_file(
    path: &Path,
    out: &mut Vec<PathBuf>,
    follow_symlinks: bool,
    files_visited: &mut HashSet<(u64, u64)>,
    known_meta: Option<&std::fs::Metadata>,
) {
    if !follow_symlinks {
        out.push(path.to_path_buf());
        return;
    }
    let key = match known_meta {
        Some(m) => Some(dir_key(m)),
        None => std::fs::metadata(path).ok().map(|m| dir_key(&m)),
    };
    match key {
        Some(k) if !files_visited.insert(k) => {
            log::debug!("skipping duplicate backing target {}", path.display());
        }
        _ => out.push(path.to_path_buf()),
    }
}

/// A backing file parsed into the fields a track row needs, plus its raw
/// `(key, value)` tags to seed.
#[derive(Debug)]
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

/// Assemble a WAV [`Probed`] from located audio bounds, reading tags and pictures
/// from `prefix`. Shared by the bounded, full-buffer, and ceiling probe paths.
fn wav_probed(prefix: &[u8], bounds: &wav::WavBounds) -> Probed {
    let (binary_tags, promoted) = wav::read_binary_tags(prefix);
    let mut tags = wav::read_tags(prefix);
    tags.extend(promoted);
    Probed {
        format: Format::Wav,
        audio_offset: bounds.audio_offset,
        audio_length: bounds.audio_length,
        tags,
        pictures: wav::read_pictures(prefix),
        binary_tags,
        structural_blocks: Vec::new(),
    }
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
        let (pictures, art_drops) = mp4::read_pictures_reporting(bytes, MAX_ART_BYTES);
        let (binary_tags, bin_drops) = mp4::read_binary_tags_reporting(bytes, MAX_BINARY_TAG_BYTES);
        log_mp4_oversize_drops(path, &art_drops, &bin_drops);
        Some(Probed {
            format: Format::M4a,
            audio_offset: bounds.audio_offset,
            audio_length: bounds.audio_length,
            tags: mp4::read_tags(bytes),
            pictures,
            binary_tags,
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
        Some(wav_probed(bytes, &bounds))
    } else {
        None
    }
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

/// Bounded probe of one backing file: open once, fstat before and after the
/// probe, and report `Raced` when the file moved mid-probe — so the stored
/// stamp and the probed bytes provably share one inode held still across the
/// probe. Never reads the audio payload (M4A uses the seek reader;
/// front-anchored formats read only the metadata extent).
///
/// Returns `ProbeOutcome::Unparseable` for a supported-extension file that does
/// not parse (counted as `failed`) and `ProbeOutcome::Raced` if the file
/// changed under us.
fn probe_file(path: &Path, window: usize) -> std::io::Result<ProbeOutcome> {
    let file = std::fs::File::open(path)?;
    crate::metrics::on_scan_open();
    let s1 = BackingStamp::from_metadata(&file.metadata()?);
    #[cfg(test)]
    fire_after_s1();

    let probed = probe_body(path, &file, s1.size, window)?;

    let s2 = BackingStamp::from_metadata(&file.metadata()?);
    if s1 != s2 {
        log::warn!("skipping {}: changed during probe", path.display());
        return Ok(ProbeOutcome::Raced);
    }
    Ok(match probed {
        Some(p) => ProbeOutcome::Probed(p, s1),
        None => ProbeOutcome::Unparseable,
    })
}

/// The per-format metadata dispatch for one already-opened backing file, over
/// its first `file_len` bytes. Split out of `probe_file` so the fstat-sandwich
/// wrapper stays legible. Never reads the audio payload (M4A uses the seek
/// reader; front-anchored formats read only the metadata extent). Returns
/// `Ok(None)` for an unsupported/unparseable file.
fn probe_body(
    path: &Path,
    file: &std::fs::File,
    file_len: u64,
    window: usize,
) -> std::io::Result<Option<Probed>> {
    // M4A: seek reader, never touches mdat.
    if has_ext(path, "m4a") || has_ext(path, "m4b") {
        let mut f = file;
        let scan = match mp4::read_structure_from(&mut f, file_len) {
            Ok(s) => s,
            Err(e) => {
                log::warn!("skipping {}: {e}", path.display());
                return Ok(None);
            }
        };
        let (pictures, art_drops) = mp4::read_pictures_reporting(&scan.moov, MAX_ART_BYTES);
        let (binary_tags, bin_drops) =
            mp4::read_binary_tags_reporting(&scan.moov, MAX_BINARY_TAG_BYTES);
        log_mp4_oversize_drops(path, &art_drops, &bin_drops);
        return Ok(Some(Probed {
            format: Format::M4a,
            audio_offset: scan.mdat_payload_offset,
            audio_length: scan.mdat_payload_len,
            tags: mp4::read_tags(&scan.moov),
            pictures,
            binary_tags,
            structural_blocks: Vec::new(),
        }));
    }

    // Front-anchored formats: read a window, widen on NeedMore. Only the MP3
    // arm of probe_prefix consumes the ID3v1 tail, and dispatch is by
    // extension — so only .mp3 pays the tail read (#67).
    let tail = if has_ext(path, "mp3") {
        read_tail_128(file, file_len)?
    } else {
        None
    };
    // Never read past the probe ceiling, however large the file or whatever a
    // (possibly corrupt) header asks for via `NeedMore`.
    let probe_cap = file_len.min(MAX_PROBE_BYTES);
    let mut want = usize_from((window as u64).min(probe_cap));
    let mut prefix = read_window(file, want)?;
    for _ in 0..MAX_WIDEN_RETRIES {
        match probe_prefix(path, &prefix, file_len, tail.as_ref()) {
            Probe::Done(p) => return Ok(Some(p)),
            Probe::Skip => {
                log::warn!("skipping {}: no parseable audio metadata", path.display());
                return Ok(None);
            }
            Probe::NeedMore(up_to) => {
                // Read everything we're willing to probe? Widening can't help.
                if want as u64 >= probe_cap {
                    break;
                }
                // Grow to at least `up_to` (capped at `probe_cap`), always making
                // progress (`+1`), then retry.
                want = usize_from(up_to.min(probe_cap))
                    .max(want + 1)
                    .min(usize_from(probe_cap));
                prefix = read_window(file, want)?;
            }
        }
    }
    // Fallback: full-buffer probe over the bytes we were willing to read.
    if (prefix.len() as u64) < probe_cap {
        prefix = read_window(file, usize_from(probe_cap))?;
    }
    if let Some(p) = probe_full(path, &prefix) {
        return Ok(Some(p));
    }
    // A WAV whose `data` payload runs past the probe ceiling fails the strict
    // full-buffer parse (the payload isn't present to bound), yet its `fmt `/`data`
    // headers sit at the front: trust the declared bounds and serve the audio,
    // accepting the loss of any tag chunks trailing the payload.
    if has_ext(path, "wav")
        && file_len > MAX_PROBE_BYTES
        && let Ok(bounds) = wav::locate_audio_at_ceiling(&prefix, file_len)
    {
        return Ok(Some(wav_probed(&prefix, &bounds)));
    }
    if file_len > MAX_PROBE_BYTES {
        log::warn!(
            "skipping {}: no parseable metadata within first {MAX_PROBE_BYTES} bytes",
            path.display()
        );
    } else {
        log::warn!("skipping {}: no parseable audio metadata", path.display());
    }
    Ok(None)
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
            Ok(Extent::Complete(b)) => Probe::Done(wav_probed(prefix, &b)),
            Ok(Extent::NeedMore { up_to }) => Probe::NeedMore(up_to),
            Err(_) => Probe::Skip,
        }
    } else {
        Probe::Skip
    }
}

/// Knobs for a scan. `jobs == 0` means "use available parallelism".
#[derive(Debug, Clone)]
pub struct ScanOptions {
    pub jobs: usize,
    /// Initial probe read window in bytes; widened on `NeedMore`.
    pub window: usize,
    /// In-flight art-byte budget and per-batch byte-flush threshold.
    pub batch_bytes: u64,
    /// Follow symlinks during collection. Off by default: symlinks are logged
    /// and skipped, which keeps the walk immune to directory-symlink cycles.
    pub follow_symlinks: bool,
}

impl Default for ScanOptions {
    fn default() -> Self {
        Self {
            jobs: 0,
            window: WINDOW,
            batch_bytes: BATCH_BYTES,
            follow_symlinks: false,
        }
    }
}

fn effective_jobs(jobs: usize) -> usize {
    if jobs != 0 {
        return jobs;
    }
    std::thread::available_parallelism().map_or(1, std::num::NonZero::get)
}

/// One probed file ready to write, plus its art-byte weight for backpressure.
struct Unit {
    abs_path: String,
    stamp: BackingStamp,
    probed: Probed,
    weight: u64,
}

/// In-memory byte weight of a `Probed`, used for batch backpressure
/// (`ScanOptions::batch_bytes`). Counts every buffered payload — pictures plus FLAC
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

/// The universal `tags.key` floor, mirrored from the DB `CHECK` exactly: a key
/// must be non-empty and contain no byte below 0x20 (the control chars the DB
/// rejects via its GLOB range; NUL also fails here, the DB's documented blind
/// spot). DEL (0x7F) and high/non-ASCII bytes are accepted, matching the DB.
/// Distinct from the strict Vorbis `is_valid_key` (which also bars `=`, 0x7E,
/// 0x7F, and non-ASCII) — applying that here would wrongly drop legal MP3/M4A
/// custom keys containing `=`/`:`/space.
fn key_passes_floor(key: &str) -> bool {
    !key.is_empty() && key.bytes().all(|b| b >= 0x20)
}

/// Drops embedded pictures over [`MAX_ART_BYTES`], logging each so a cover that
/// vanishes from the synthesized view is explained rather than silent (#284).
/// Filtering here, before the caller enumerates, keeps stored art ordinals
/// gap-free. Note: the mp4 `covr` path caps oversize art earlier, inside
/// `mp4::read_pictures`, so those drops never reach this filter.
fn accept_pictures(abs_path: &str, pictures: Vec<EmbeddedPicture>) -> Vec<EmbeddedPicture> {
    pictures
        .into_iter()
        .filter(|p| {
            if p.data.len() > MAX_ART_BYTES {
                log::warn!(
                    "{abs_path}: dropping embedded {} art ({} bytes), over the {MAX_ART_BYTES}-byte cap",
                    p.mime,
                    p.data.len(),
                );
                return false;
            }
            true
        })
        .collect()
}

/// Filters embedded binary tags to those worth storing, logging oversize drops
/// (#284). Empty payloads carry nothing to serve, so they are dropped silently;
/// payloads over [`MAX_BINARY_TAG_BYTES`] are a lossy drop and get a warning.
fn accept_binary_tags(abs_path: &str, tags: Vec<EmbeddedBinaryTag>) -> Vec<musefs_db::BinaryTag> {
    tags.into_iter()
        .filter(|b| {
            if b.payload.len() > MAX_BINARY_TAG_BYTES {
                log::warn!(
                    "{abs_path}: dropping binary tag {} ({} bytes), over the {MAX_BINARY_TAG_BYTES}-byte cap",
                    b.key,
                    b.payload.len(),
                );
                return false;
            }
            !b.payload.is_empty()
        })
        .enumerate()
        .map(|(ordinal, b)| musefs_db::BinaryTag {
            key: b.key,
            payload: b.payload,
            ordinal: ordinal as u64,
        })
        .collect()
}

/// Logs each oversized mp4 `covr` image / binary `----` value that the format
/// layer skipped before materialization (#343). These drops happen inside
/// `mp4::read_pictures` / `mp4::read_binary_tags` — earlier than the `accept_*`
/// ingest filters that log the lossy drops for the other formats (#284), and
/// deliberately so, to avoid building a large image out of a large `moov` — so
/// they are surfaced here at probe time, mirroring the `accept_*` message shape.
fn log_mp4_oversize_drops(path: &Path, art: &[mp4::OversizeDrop], binary: &[mp4::OversizeDrop]) {
    for d in art {
        log::warn!(
            "{}: dropping embedded {} art ({} bytes), over the {MAX_ART_BYTES}-byte cap",
            path.display(),
            d.descriptor,
            d.bytes,
        );
    }
    for d in binary {
        log::warn!(
            "{}: dropping binary tag {} ({} bytes), over the {MAX_BINARY_TAG_BYTES}-byte cap",
            path.display(),
            d.descriptor,
            d.bytes,
        );
    }
}

/// Upsert a track from a probed backing file: write the track row, replace its
/// seeded tags, and ingest its embedded art (capped, deduped, clamped).
fn ingest(db: &Db, abs_path: &str, meta: &std::fs::Metadata, probed: Probed) -> Result<()> {
    let stamp = BackingStamp::from_metadata(meta);
    let track_id = db.upsert_track(&NewTrack {
        backing_path: abs_path.to_string(),
        format: probed.format,
        audio_offset: probed.audio_offset,
        audio_length: probed.audio_length,
        backing_size: meta.len(),
        backing_mtime_ns: stamp.mtime_ns,
        backing_ctime_ns: stamp.ctime_ns,
    })?;

    let mut tags = Vec::new();
    let mut ordinals: HashMap<String, u64> = HashMap::new();
    for (key, value) in probed.tags {
        if !key_passes_floor(&key) {
            continue;
        }
        let ord = ordinals.entry(key.clone()).or_insert(0);
        tags.push(Tag::new(&key, &value, *ord));
        *ord += 1;
    }
    db.replace_tags(track_id, &tags)?;

    let binary_tags = accept_binary_tags(abs_path, probed.binary_tags);
    db.set_binary_tags(track_id, &binary_tags)?;

    let mut sb_ordinals: HashMap<String, u64> = HashMap::new();
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
    for (ordinal, pic) in accept_pictures(abs_path, probed.pictures)
        .into_iter()
        .enumerate()
    {
        let art_id = db.upsert_art(&NewArt {
            mime: pic.mime,
            width: (pic.width != 0).then_some(pic.width),
            height: (pic.height != 0).then_some(pic.height),
            data: pic.data,
        })?;
        let picture_type = pic.picture_type.get();
        track_arts.push(TrackArt {
            art_id,
            picture_type,
            description: pic.description,
            ordinal: ordinal as u64,
        });
    }
    db.set_track_art(track_id, &track_arts)?;
    Ok(())
}

/// Like `ingest`, but writes through a batch `BulkWriter`. Takes `probed` by
/// value so picture/binary-tag/structural-block bytes are moved, not cloned (#68).
fn ingest_bulk(
    bw: &mut musefs_db::BulkWriter<'_>,
    abs_path: &str,
    stamp: BackingStamp,
    probed: Probed,
) -> Result<()> {
    let track_id = bw.upsert_track(&NewTrack {
        backing_path: abs_path.to_string(),
        format: probed.format,
        audio_offset: probed.audio_offset,
        audio_length: probed.audio_length,
        backing_size: stamp.size,
        backing_mtime_ns: stamp.mtime_ns,
        backing_ctime_ns: stamp.ctime_ns,
    })?;

    let mut tags = Vec::new();
    let mut ordinals: HashMap<String, u64> = HashMap::new();
    for (key, value) in &probed.tags {
        if !key_passes_floor(key) {
            continue;
        }
        let ord = ordinals.entry(key.clone()).or_insert(0);
        tags.push(Tag::new(key, value, *ord));
        *ord += 1;
    }
    bw.replace_tags(track_id, &tags)?;

    let binary_tags = accept_binary_tags(abs_path, probed.binary_tags);
    bw.set_binary_tags(track_id, &binary_tags)?;

    let mut sb_ordinals: HashMap<String, u64> = HashMap::new();
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
    bw.set_structural_blocks(track_id, &structural_blocks)?;

    let mut track_arts = Vec::new();
    for (ordinal, pic) in accept_pictures(abs_path, probed.pictures)
        .into_iter()
        .enumerate()
    {
        let art_id = bw.upsert_art(&NewArt {
            mime: pic.mime,
            width: (pic.width != 0).then_some(pic.width),
            height: (pic.height != 0).then_some(pic.height),
            data: pic.data,
        })?;
        let picture_type = pic.picture_type.get();
        track_arts.push(TrackArt {
            art_id,
            picture_type,
            description: pic.description,
            ordinal: ordinal as u64,
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
/// recursively). Files whose extension is not a supported audio format
/// increment `ScanStats::skipped` and are tallied by extension for the
/// end-of-scan summary log line (#341); supported-extension files with a
/// per-file I/O or parse error increment `ScanStats::failed` and do not abort
/// the scan.
pub fn scan_directory_with(db: &Db, root: &Path, opts: &ScanOptions) -> Result<ScanStats> {
    let mut files = Vec::new();
    let mut tally = SkipTally::default();
    if root.is_file() {
        if is_supported_audio(root) {
            files.push(root.to_path_buf());
        } else {
            tally.record(root);
        }
    } else {
        tally = collect_audio(root, &mut files, opts.follow_symlinks)?;
    }
    db.apply_bulk_pragmas_self()?; // scan-scoped tuning on the caller's connection
    let mut stats = run_pipeline(db, files, opts)?;
    // skipped is tallied during the walk, not the pipeline
    stats.skipped = tally.total;
    // Per-extension breakdown of the skip count, so a large `skipped` is
    // diagnosable (#341). Log-only: never folded into `stats`/the CLI summary.
    if let Some(summary) = tally.summary() {
        log::info!("{summary}");
    }
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
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};

    let jobs = effective_jobs(opts.jobs);
    let window = opts.window;
    let cap = opts.batch_bytes;
    let budget = Arc::new(ByteBudget::new(cap));
    let failed = Arc::new(AtomicU64::new(0));
    let raced = Arc::new(AtomicU64::new(0));

    // Work queue: a shared iterator behind a mutex (cheap; probing dominates).
    let work = Arc::new(std::sync::Mutex::new(files.into_iter()));
    let (tx, rx) = sync_channel::<Unit>(jobs * 2);

    let mut workers = Vec::with_capacity(jobs);
    for _ in 0..jobs {
        let work = Arc::clone(&work);
        let tx = tx.clone();
        let budget = Arc::clone(&budget);
        let failed = Arc::clone(&failed);
        let raced = Arc::clone(&raced);
        workers.push(std::thread::spawn(move || {
            loop {
                let next = { work.lock().unwrap().next() };
                let Some(path) = next else { break };
                match probe_file(&path, window) {
                    Ok(ProbeOutcome::Probed(probed, stamp)) => {
                        let abs = match std::fs::canonicalize(&path) {
                            Ok(abs) => abs,
                            Err(e) => {
                                log::warn!("skipping {}: {e}", path.display());
                                failed.fetch_add(1, Ordering::Relaxed);
                                continue;
                            }
                        };
                        let weight = payload_weight(&probed);
                        budget.acquire(weight); // backpressure on in-flight art bytes
                        let unit = Unit {
                            abs_path: abs.to_string_lossy().into_owned(),
                            stamp,
                            probed,
                            weight,
                        };
                        if tx.send(unit).is_err() {
                            budget.release(weight);
                            break;
                        }
                    }
                    Ok(ProbeOutcome::Unparseable) => {
                        failed.fetch_add(1, Ordering::Relaxed);
                    }
                    Err(e) => {
                        log::warn!("skipping {}: {e}", path.display());
                        failed.fetch_add(1, Ordering::Relaxed);
                    }
                    Ok(ProbeOutcome::Raced) => {
                        raced.fetch_add(1, Ordering::Relaxed);
                    }
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
        // Budget weights are released only after commit, and ingest_bulk consumes
        // the Probed — capture each unit's weight before the move (#68).
        let mut weights = Vec::with_capacity(batch.len());
        for Unit {
            abs_path,
            stamp,
            probed,
            weight,
        } in batch.drain(..)
        {
            weights.push(weight);
            ingest_bulk(&mut bw, &abs_path, stamp, probed)?;
            *scanned += 1;
        }
        bw.commit()?;
        for w in weights {
            budget.release(w);
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
        skipped: 0, // counted at walk time; filled in by scan_directory_with
        failed: failed.load(Ordering::Relaxed),
        raced: raced.load(Ordering::Relaxed),
    })
}

/// Test/oracle only: scan using the legacy whole-file probe (`probe_full`). The
/// equivalence property compares this against the bounded `scan_directory`.
#[doc(hidden)]
pub fn scan_directory_full_oracle(db: &Db, root: &Path) -> Result<ScanStats> {
    let mut files = Vec::new();
    let mut skipped = 0u64;
    if root.is_file() {
        if is_supported_audio(root) {
            files.push(root.to_path_buf());
        } else {
            skipped += 1;
        }
    } else {
        skipped += collect_audio(root, &mut files, false)?.total;
    }
    let mut stats = ScanStats {
        scanned: 0,
        skipped,
        failed: 0,
        raced: 0,
    };
    for path in files {
        let bytes = std::fs::read(&path)?;
        let Some(probed) = probe_full(&path, &bytes) else {
            stats.failed += 1;
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
/// size/mtime/ctime changed since the last scan (skipping unchanged ones so external
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
        collect_audio(root, &mut files, opts.follow_symlinks)?;
    }
    db.apply_bulk_pragmas_self()?;

    // Main-thread pre-dispatch skip pass: load existing (path -> stamp,id,format) once,
    // stat each candidate, keep only changed files. Workers stay DB-free.
    let existing: HashMap<String, (crate::freshness::BackingStamp, i64, Format)> = db
        .list_tracks()?
        .into_iter()
        .map(|t| {
            (
                t.backing_path.clone(),
                (
                    crate::freshness::BackingStamp::from_track(&t),
                    t.id,
                    t.format,
                ),
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
        let meta = match std::fs::metadata(&path) {
            Ok(meta) => meta,
            Err(e) => {
                log::warn!("skipping {}: {e}", path.display());
                skip_failed += 1;
                continue;
            }
        };
        let abs = match std::fs::canonicalize(&path) {
            Ok(abs) => abs,
            Err(e) => {
                log::warn!("skipping {}: {e}", path.display());
                skip_failed += 1;
                continue;
            }
        };
        let key = abs.to_string_lossy().into_owned();
        if let Some((stamp, id, format)) = existing.get(&key).copied() {
            let needs_backfill = format == Format::Flac && !have_structural.contains(&id);
            if crate::freshness::BackingStamp::from_metadata(&meta) == stamp && !needs_backfill {
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
        if let Err(e) = std::fs::metadata(&track.backing_path)
            && e.kind() == std::io::ErrorKind::NotFound
        {
            db.delete_track(track.id)?;
            pruned += 1;
        }
    }
    db.gc_orphan_art()?;

    Ok(RevalidateStats {
        updated: scan.scanned,
        unchanged,
        pruned,
        failed: scan.failed + skip_failed,
        raced: scan.raced,
    })
}

/// Back-compat shim used by the CLI and existing tests.
pub fn revalidate(db: &Db, root: &Path) -> Result<RevalidateStats> {
    revalidate_with(db, root, &ScanOptions::default())
}

#[cfg(test)]
mod scan_unit_tests {
    use super::*;
    use musefs_format::PictureType;
    use std::io::Write;

    // --- ScanOptions defaults (WINDOW L16, BATCH_BYTES L12) ---

    // kills the WINDOW `<<`→`>>` and BATCH_BYTES initializer mutants: the
    // right-hand sides are decimal literals, so a mutated const/Default
    // initializer cannot flow to both sides of the assertion.
    #[test]
    fn scan_options_defaults() {
        let d = ScanOptions::default();
        assert_eq!(d.jobs, 0, "jobs default = use available parallelism");
        assert_eq!(d.window, 1_048_576, "window default = 1 MiB");
        assert_eq!(d.batch_bytes, 67_108_864, "batch_bytes default = 64 MiB");
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

    // --- payload_weight() ---

    // Sums picture + binary-tag + structural-block byte lengths (batch backpressure).
    #[test]
    fn payload_weight_sums_all_buffered_payloads() {
        let pic = |n: usize| EmbeddedPicture {
            mime: "image/png".to_string(),
            picture_type: PictureType::new(3).unwrap(),
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
            let mut v = u32::try_from(8 + body.len())
                .unwrap()
                .to_be_bytes()
                .to_vec();
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

    fn mp4_with_covr(type_code: u32, value: &[u8]) -> Vec<u8> {
        fn bx(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
            let mut v = u32::try_from(8 + body.len())
                .unwrap()
                .to_be_bytes()
                .to_vec();
            v.extend_from_slice(kind);
            v.extend_from_slice(body);
            v
        }
        let mut hdlr_body = vec![0u8; 8];
        hdlr_body.extend_from_slice(b"soun");
        hdlr_body.extend_from_slice(&[0u8; 12]);
        let trak = bx(b"trak", &bx(b"mdia", &bx(b"hdlr", &hdlr_body)));

        let mut data_body = type_code.to_be_bytes().to_vec();
        data_body.extend_from_slice(&0u32.to_be_bytes());
        data_body.extend_from_slice(value);
        let ilst = bx(b"ilst", &bx(b"covr", &bx(b"data", &data_body)));
        let mut meta = 0u32.to_be_bytes().to_vec();
        meta.extend(bx(b"hdlr", &[0u8; 25]));
        meta.extend(ilst);
        let udta = bx(b"udta", &bx(b"meta", &meta));

        let moov = bx(b"moov", &[trak, udta].concat());
        [bx(b"ftyp", b"M4A "), moov, bx(b"mdat", b"AUDIODATA")].concat()
    }

    #[test]
    fn probe_file_skips_oversized_mp4_covr() {
        let oversized = vec![0xFFu8; MAX_ART_BYTES + 1];
        let bytes = mp4_with_covr(13, &oversized);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("oversized_art.m4a");
        std::fs::write(&path, &bytes).unwrap();
        let probed = match probe_file(&path, 0).unwrap() {
            ProbeOutcome::Probed(p, _) => p,
            other => panic!("expected Probed, got {other:?}"),
        };
        assert_eq!(probed.format, Format::M4a);
        assert!(
            probed.pictures.is_empty(),
            "oversized covr must be skipped at extraction, not materialized"
        );
    }

    #[test]
    fn probe_file_skips_oversized_mp4_binary_freeform() {
        // A `----` value larger than MAX_BINARY_TAG_BYTES must be skipped at
        // extraction by the real seek-path scanner, so it is absent from Probed.
        let oversized = vec![0xABu8; MAX_BINARY_TAG_BYTES + 1];
        let bytes = mp4_with_binary_freeform("com.serato.dj", "analysis", &oversized);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("oversized_bin.m4a");
        std::fs::write(&path, &bytes).unwrap();
        let probed = match probe_file(&path, 0).unwrap() {
            ProbeOutcome::Probed(p, _) => p,
            other => panic!("expected Probed, got {other:?}"),
        };
        assert_eq!(probed.format, Format::M4a);
        assert!(
            probed.binary_tags.is_empty(),
            "oversized binary freeform must be skipped at extraction, not materialized"
        );
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
            body.extend_from_slice(&u32::try_from(payload.len()).unwrap().to_le_bytes());
            body.extend_from_slice(payload);
        }
        let mut out = b"RIFF".to_vec();
        out.extend_from_slice(&u32::try_from(body.len() + 4).unwrap().to_le_bytes());
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
    fn scan_caps_match_db_limits() {
        assert_eq!(
            i64::try_from(MAX_ART_BYTES).unwrap(),
            musefs_db::limits::MAX_ART_BYTES
        );
        assert_eq!(
            i64::try_from(MAX_BINARY_TAG_BYTES).unwrap(),
            musefs_db::limits::MAX_BINARY_TAG_BYTES
        );
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
        collect_audio(dir.path(), &mut out, false).unwrap();
        assert_eq!(out.len(), 1);
        assert!(out[0].ends_with("keep.flac"));
    }

    #[test]
    fn scan_options_default_does_not_follow_symlinks() {
        assert!(!ScanOptions::default().follow_symlinks);
    }

    #[test]
    fn collect_audio_follows_symlinked_file_when_enabled() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real.flac");
        std::fs::write(&real, b"x").unwrap();
        let lib = dir.path().join("lib");
        std::fs::create_dir(&lib).unwrap();
        std::os::unix::fs::symlink(&real, lib.join("link.flac")).unwrap();

        let mut on = Vec::new();
        collect_audio(&lib, &mut on, true).unwrap();
        assert_eq!(
            on.len(),
            1,
            "symlinked file should be collected when following"
        );

        let mut off = Vec::new();
        collect_audio(&lib, &mut off, false).unwrap();
        assert!(
            off.is_empty(),
            "symlinked file should be skipped by default"
        );
    }

    #[test]
    fn collect_audio_follows_symlinked_dir_when_enabled() {
        let dir = tempfile::tempdir().unwrap();
        let real_dir = dir.path().join("music");
        std::fs::create_dir(&real_dir).unwrap();
        std::fs::write(real_dir.join("song.flac"), b"x").unwrap();
        let root = dir.path().join("root");
        std::fs::create_dir(&root).unwrap();
        std::os::unix::fs::symlink(&real_dir, root.join("linkdir")).unwrap();

        let mut on = Vec::new();
        collect_audio(&root, &mut on, true).unwrap();
        assert_eq!(
            on.len(),
            1,
            "files under a symlinked dir should be collected"
        );

        let mut off = Vec::new();
        collect_audio(&root, &mut off, false).unwrap();
        assert!(off.is_empty(), "symlinked dir should be skipped by default");
    }

    #[test]
    fn collect_audio_terminates_on_symlink_cycle() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a");
        std::fs::create_dir(&a).unwrap();
        std::fs::write(a.join("song.flac"), b"x").unwrap();
        std::os::unix::fs::symlink(dir.path(), a.join("loop")).unwrap();

        let mut out = Vec::new();
        collect_audio(dir.path(), &mut out, true).unwrap();
        assert_eq!(
            out.iter().filter(|p| p.ends_with("song.flac")).count(),
            1,
            "each real file collected at most once despite the cycle"
        );
    }

    #[test]
    fn collect_audio_skips_broken_symlink_when_following() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("real.flac"), b"x").unwrap();
        std::os::unix::fs::symlink(dir.path().join("nonexistent"), dir.path().join("dangling"))
            .unwrap();

        let mut out = Vec::new();
        let result = collect_audio(dir.path(), &mut out, true);
        assert!(
            result.is_ok(),
            "a dangling symlink must not abort collection"
        );
        assert_eq!(out.len(), 1);
        assert!(out[0].ends_with("real.flac"));
    }

    #[test]
    fn collect_audio_does_not_follow_symlinks_by_default() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("real.flac"), b"x").unwrap();
        let other = dir.path().join("other.flac");
        std::fs::write(&other, b"x").unwrap();
        std::os::unix::fs::symlink(&other, dir.path().join("link.flac")).unwrap();

        let mut out = Vec::new();
        collect_audio(dir.path(), &mut out, false).unwrap();
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn collect_audio_ignores_symlink_to_non_file_target_when_following() {
        use std::os::unix::ffi::OsStrExt;

        let dir = tempfile::tempdir().unwrap();
        // A FIFO is neither a regular file nor a directory, and mkfifo works in
        // restricted sandboxes that deny Unix-socket bind (issue #277).
        let fifo = dir.path().join("fifo");
        let c_path = std::ffi::CString::new(fifo.as_os_str().as_bytes()).unwrap();
        #[expect(unsafe_code, reason = "libc::mkfifo FFI; no std equivalent")]
        let rc = unsafe { libc::mkfifo(c_path.as_ptr(), 0o644) };
        assert_eq!(rc, 0, "mkfifo failed: {}", std::io::Error::last_os_error());

        // Name the link with a supported audio extension so the only thing
        // keeping it out of `out` is the resolved target's is_file() check.
        std::os::unix::fs::symlink(&fifo, dir.path().join("link.flac")).unwrap();

        let mut out = Vec::new();
        collect_audio(dir.path(), &mut out, true).unwrap();
        assert!(
            out.is_empty(),
            "a symlink to a non-file, non-dir target must not be collected"
        );
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
        let n: u32 = u32::try_from(body.len()).unwrap();
        v.extend_from_slice(&[
            u8::try_from(n >> 16).unwrap(),
            u8::try_from(n >> 8).unwrap(),
            u8::try_from(n).unwrap(),
        ]);
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
        vc.extend_from_slice(&u32::try_from(vendor.len()).unwrap().to_le_bytes());
        vc.extend_from_slice(vendor);
        vc.extend_from_slice(&u32::try_from(entries.len()).unwrap().to_le_bytes());
        for e in entries {
            vc.extend_from_slice(&u32::try_from(e.len()).unwrap().to_le_bytes());
            vc.extend_from_slice(e.as_bytes());
        }
        vc
    }
    fn picture(width: u32, height: u32, data: &[u8]) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(&3u32.to_be_bytes());
        let mime = "image/png";
        b.extend_from_slice(&u32::try_from(mime.len()).unwrap().to_be_bytes());
        b.extend_from_slice(mime.as_bytes());
        b.extend_from_slice(&0u32.to_be_bytes());
        b.extend_from_slice(&width.to_be_bytes());
        b.extend_from_slice(&height.to_be_bytes());
        b.extend_from_slice(&0u32.to_be_bytes());
        b.extend_from_slice(&0u32.to_be_bytes());
        b.extend_from_slice(&u32::try_from(data.len()).unwrap().to_be_bytes());
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
        let mut artists: Vec<(u64, String)> = db
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
    fn ingest_oracle_path_stores_nonzero_art_dimensions() {
        // Drives the single-file `ingest` (not `ingest_bulk`) so the
        // `(pic.width != 0).then_some(..)` dimension guards there are pinned.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("art.flac");
        write_flac(&path, &["ARTIST=A", "TITLE=T"], Some((10, 20)));
        let db = musefs_db::Db::open_in_memory().unwrap();
        crate::scan_directory_full_oracle(&db, &path).unwrap();
        let track = db.list_tracks().unwrap().into_iter().next().unwrap();
        let ta = db.get_track_art(track.id).unwrap();
        assert_eq!(ta.len(), 1);
        let meta = db.get_art_meta(ta[0].art_id).unwrap().unwrap();
        assert_eq!(meta.width, Some(10));
        assert_eq!(meta.height, Some(20));
    }

    #[test]
    fn scan_directory_counts_scanned_failed_and_skipped() {
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
        // Supported extension, unparseable bytes → a scan failure.
        std::fs::write(dir.path().join("bad.flac"), b"garbage").unwrap();
        // Unsupported extension → skipped at collection, never probed.
        std::fs::write(dir.path().join("notes.txt"), b"hello").unwrap();
        let db = musefs_db::Db::open_in_memory().unwrap();
        let stats = crate::scan_directory(&db, dir.path()).unwrap();
        assert_eq!(stats.scanned, 2);
        assert_eq!(stats.failed, 1);
        assert_eq!(stats.skipped, 1);
    }

    #[test]
    fn skip_tally_summary_orders_by_descending_count() {
        let mut tally = super::SkipTally::default();
        for _ in 0..20 {
            tally.record(std::path::Path::new("art/cover.jpg"));
        }
        for _ in 0..10 {
            tally.record(std::path::Path::new("disc.cue"));
        }
        for _ in 0..8 {
            tally.record(std::path::Path::new("rip.log"));
        }
        for _ in 0..4 {
            tally.record(std::path::Path::new("README"));
        }
        assert_eq!(tally.total, 42);
        assert_eq!(
            tally.summary().unwrap(),
            "skipped 42: jpg=20, cue=10, log=8, <none>=4"
        );
    }

    #[test]
    fn skip_tally_lowercases_extension_and_buckets_extensionless() {
        let mut tally = super::SkipTally::default();
        tally.record(std::path::Path::new("a.JPG"));
        tally.record(std::path::Path::new("b.jpg"));
        tally.record(std::path::Path::new("noext"));
        assert_eq!(tally.summary().unwrap(), "skipped 3: jpg=2, <none>=1");
    }

    #[test]
    fn skip_tally_ties_break_by_extension_name() {
        let mut tally = super::SkipTally::default();
        tally.record(std::path::Path::new("a.nfo"));
        tally.record(std::path::Path::new("b.cue"));
        assert_eq!(tally.summary().unwrap(), "skipped 2: cue=1, nfo=1");
    }

    #[test]
    fn skip_tally_empty_has_no_summary() {
        assert!(super::SkipTally::default().summary().is_none());
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
            backing_path: ghost.to_string_lossy().into_owned(),
            format: Format::Flac,
            audio_offset: 0,
            audio_length: 0,
            backing_size: 0,
            backing_mtime_ns: 0,
            backing_ctime_ns: 0,
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
            ingest_bulk(
                &mut bw,
                "/a.mp3",
                BackingStamp {
                    size: 1,
                    mtime_ns: 0,
                    ctime_ns: 0,
                },
                probed_with_mixed_binary_tags(),
            )
            .unwrap();
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

    #[test]
    fn accept_pictures_keeps_at_cap_and_drops_over_cap() {
        let mk = |len: usize| EmbeddedPicture {
            mime: "image/jpeg".to_string(),
            picture_type: musefs_format::PictureType::new(3).unwrap(),
            description: String::new(),
            width: 0,
            height: 0,
            data: vec![0u8; len],
        };
        // A picture exactly at the cap is kept; one byte over is dropped. The
        // boundary pins `>` against `>=` (an at-cap drop would be silent loss).
        let kept = accept_pictures("/x.flac", vec![mk(MAX_ART_BYTES), mk(MAX_ART_BYTES + 1)]);
        assert_eq!(kept.len(), 1, "exactly the at-cap picture survives");
        assert_eq!(kept[0].data.len(), MAX_ART_BYTES);
    }

    #[test]
    fn accept_binary_tags_keeps_at_cap_and_drops_over_cap() {
        let mk = |len: usize| EmbeddedBinaryTag {
            key: "PRIV".to_string(),
            payload: vec![0u8; len],
        };
        let kept = accept_binary_tags(
            "/x.mp3",
            vec![mk(MAX_BINARY_TAG_BYTES), mk(MAX_BINARY_TAG_BYTES + 1)],
        );
        assert_eq!(kept.len(), 1, "exactly the at-cap binary tag survives");
        assert_eq!(kept[0].payload.len(), MAX_BINARY_TAG_BYTES);
    }

    fn probed_with_text_tags(tags: &[(&str, &str)]) -> Probed {
        Probed {
            format: musefs_db::Format::Mp3,
            audio_offset: 0,
            audio_length: 0,
            tags: tags
                .iter()
                .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
                .collect(),
            pictures: Vec::new(),
            binary_tags: Vec::new(),
            structural_blocks: Vec::new(),
        }
    }

    #[test]
    fn ingest_skips_empty_and_control_char_keys() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.mp3");
        std::fs::write(&path, b"x").unwrap();
        let meta = std::fs::metadata(&path).unwrap();
        let db = Db::open_in_memory().unwrap();

        ingest(
            &db,
            &path.to_string_lossy(),
            &meta,
            probed_with_text_tags(&[
                ("artist", "Alice"),
                ("", "dropped"),        // empty key
                ("a\u{7}b", "dropped"), // control char
                ("a\u{0}b", "dropped"), // embedded NUL — DB CHECK can't see it, the floor can
                ("a=b", "kept"),        // '=' is NOT a floor violation
            ]),
        )
        .unwrap();

        let tid = db.list_tracks().unwrap()[0].id;
        let keys: Vec<String> = db
            .get_tags(tid)
            .unwrap()
            .into_iter()
            .map(|t| t.key)
            .collect();
        // get_tags is ORDER BY key, ordinal: '=' (0x3D) sorts before 'a' (0x61).
        assert_eq!(keys, vec!["a=b".to_string(), "artist".to_string()]);
    }

    #[test]
    fn ingest_bulk_skips_empty_and_control_char_keys() {
        let db = Db::open_in_memory().unwrap();
        {
            let mut bw = db.bulk_writer().unwrap();
            ingest_bulk(
                &mut bw,
                "/a.mp3",
                BackingStamp {
                    size: 1,
                    mtime_ns: 0,
                    ctime_ns: 0,
                },
                probed_with_text_tags(&[
                    ("artist", "Alice"),
                    ("", "dropped"),
                    ("a\u{7}b", "dropped"),
                    ("a\u{0}b", "dropped"), // embedded NUL — floor drops it
                    ("a=b", "kept"),
                ]),
            )
            .unwrap();
            bw.commit().unwrap();
        }
        let tid = db.list_tracks().unwrap()[0].id;
        let keys: Vec<String> = db
            .get_tags(tid)
            .unwrap()
            .into_iter()
            .map(|t| t.key)
            .collect();
        assert_eq!(keys, vec!["a=b".to_string(), "artist".to_string()]);
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

    /// Probed with two tags of the SAME key, to make the per-key ordinal
    /// increment (`*ord += 1` in the tag loop) observable. The production
    /// `ingest_bulk` path is exercised with a multi-value tag elsewhere, but the
    /// oracle-only `ingest` is not, so without this its tag-ordinal mutants
    /// survive. Distinct values under one key: a collapsed ordinal (the `-=`/`*=`
    /// mutants) either underflows or duplicates the `(track_id, key, ordinal)`
    /// primary key — both observable.
    fn probed_with_duplicate_tag_key() -> Probed {
        Probed {
            format: musefs_db::Format::Flac,
            audio_offset: 0,
            audio_length: 0,
            tags: vec![
                ("ARTIST".to_string(), "A".to_string()),
                ("ARTIST".to_string(), "B".to_string()),
            ],
            pictures: Vec::new(),
            binary_tags: Vec::new(),
            structural_blocks: Vec::new(),
        }
    }

    #[test]
    fn ingest_assigns_sequential_tag_ordinals_per_key() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.flac");
        std::fs::write(&path, b"x").unwrap();
        let meta = std::fs::metadata(&path).unwrap();
        let db = Db::open_in_memory().unwrap();

        ingest(
            &db,
            &path.to_string_lossy(),
            &meta,
            probed_with_duplicate_tag_key(),
        )
        .unwrap();

        let tid = db.list_tracks().unwrap()[0].id;
        let got = db.get_tags(tid).unwrap();
        // get_tags is ORDER BY key, ordinal: the two same-key tags must hold
        // ordinals 0 then 1 (the `-=`/`*=` mutants collapse or invert this).
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].ordinal, 0);
        assert_eq!(got[0].value, "A");
        assert_eq!(got[1].ordinal, 1);
        assert_eq!(got[1].value, "B");
    }

    #[test]
    fn ingest_bulk_assigns_sequential_structural_ordinals_per_kind() {
        let db = Db::open_in_memory().unwrap();
        {
            let mut bw = db.bulk_writer().unwrap();
            ingest_bulk(
                &mut bw,
                "/a.flac",
                BackingStamp {
                    size: 1,
                    mtime_ns: 0,
                    ctime_ns: 0,
                },
                probed_with_duplicate_structural_kind(),
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
        assert_eq!(track.bounds.audio_offset(), full.audio_offset);
        assert_eq!(track.bounds.audio_length(), full.audio_length);
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
        assert_eq!(
            usize_from(track.bounds.audio_length()),
            b"DIFFERENT-AUDIO".len()
        );
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
            scan_directory_with(
                &db,
                dir.path(),
                &ScanOptions {
                    jobs,
                    ..Default::default()
                },
            )
            .unwrap();
            let mut rows: Vec<(String, u64, u64)> = db
                .list_tracks()
                .unwrap()
                .into_iter()
                .map(|t| {
                    (
                        t.backing_path,
                        t.bounds.audio_offset(),
                        t.bounds.audio_length(),
                    )
                })
                .collect();
            rows.sort();
            rows
        };
        assert_eq!(norm(1), norm(4));
        assert_eq!(norm(1).len(), 12);
    }

    #[test]
    fn oversize_unparseable_file_is_skipped_not_read_whole() {
        // A file far larger than the probe ceiling, with a valid FLAC marker but
        // a metadata block that never terminates, must be skipped rather than
        // allocated whole into RAM (the misnamed-multi-GB-file OOM guard).
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("huge.flac");
        let mut f = std::fs::File::create(&path).unwrap();
        // Marker + a non-last VORBIS_COMMENT block claiming the max 24-bit
        // length, so the bounded reader keeps asking for more.
        f.write_all(b"fLaC").unwrap();
        f.write_all(&[0x04, 0xFF, 0xFF, 0xFF]).unwrap();
        let len = MAX_PROBE_BYTES + 4096;
        f.set_len(len).unwrap();
        drop(f);

        assert!(matches!(
            probe_file(&path, WINDOW).unwrap(),
            ProbeOutcome::Unparseable
        ));
    }

    #[test]
    fn oversize_wav_is_served_via_data_header() {
        // A valid WAV whose `data` payload exceeds the probe ceiling (any
        // recording more than a few minutes long) must still be ingested: the
        // `data` chunk header sits at the front, so the declared audio bounds
        // are known without reading the payload. Skipping it would drop every
        // sufficiently long WAV in the library.
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("long.wav");

        let data_len: u64 = MAX_PROBE_BYTES + (16 << 20); // 80 MiB payload
        let mut fmt = Vec::new();
        fmt.extend_from_slice(&1u16.to_le_bytes());
        fmt.extend_from_slice(&1u16.to_le_bytes());
        fmt.extend_from_slice(&44_100u32.to_le_bytes());
        fmt.extend_from_slice(&88_200u32.to_le_bytes());
        fmt.extend_from_slice(&2u16.to_le_bytes());
        fmt.extend_from_slice(&16u16.to_le_bytes());

        let mut front = b"RIFF".to_vec();
        // form: WAVE(4) + fmt chunk(24) + data header(8) + data payload
        let riff_size = 36u32 + u32::try_from(data_len).unwrap();
        front.extend_from_slice(&riff_size.to_le_bytes());
        front.extend_from_slice(b"WAVE");
        front.extend_from_slice(b"fmt ");
        front.extend_from_slice(&u32::try_from(fmt.len()).unwrap().to_le_bytes());
        front.extend_from_slice(&fmt);
        front.extend_from_slice(b"data");
        front.extend_from_slice(&u32::try_from(data_len).unwrap().to_le_bytes());
        let audio_offset = front.len() as u64;
        let file_len = audio_offset + data_len;

        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(&front).unwrap();
        f.set_len(file_len).unwrap();
        drop(f);

        let probed = match probe_file(&path, WINDOW).unwrap() {
            ProbeOutcome::Probed(p, _) => p,
            other => panic!("expected Probed, got {other:?}"),
        };
        assert_eq!(probed.format, Format::Wav);
        assert_eq!(probed.audio_offset, audio_offset);
        assert_eq!(probed.audio_length, data_len);
    }

    #[test]
    fn probe_file_reports_raced_on_mid_probe_mutation() {
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.wav");

        // Minimal valid WAV the probe accepts (fmt + tiny data).
        let mut fmt = Vec::new();
        for v in [1u16, 1, 0, 0, 0, 16] {
            fmt.extend_from_slice(&v.to_le_bytes());
        }
        let mut front = b"RIFF".to_vec();
        // form: WAVE(4) + fmt chunk(8+len) + data header(8) + data payload(64)
        let riff_size = 4 + 8 + u32::try_from(fmt.len()).unwrap() + 8 + 64;
        front.extend_from_slice(&riff_size.to_le_bytes());
        front.extend_from_slice(b"WAVE");
        front.extend_from_slice(b"fmt ");
        front.extend_from_slice(&u32::try_from(fmt.len()).unwrap().to_le_bytes());
        front.extend_from_slice(&fmt);
        front.extend_from_slice(b"data");
        front.extend_from_slice(&64u32.to_le_bytes());
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(&front).unwrap();
        f.set_len(front.len() as u64 + 64).unwrap();
        drop(f);

        let pc = path.clone();
        set_after_s1_hook(move || {
            let mut g = std::fs::OpenOptions::new().append(true).open(&pc).unwrap();
            g.write_all(&[0u8; 4096]).unwrap(); // size moves -> S2 != S1
        });
        let out = probe_file(&path, WINDOW);
        clear_after_s1_hook();
        assert!(matches!(out, Ok(ProbeOutcome::Raced)), "got {out:?}");
    }
}
