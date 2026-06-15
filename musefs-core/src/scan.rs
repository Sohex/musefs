use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

use musefs_db::convert::usize_from;
use musefs_db::{Db, Format, NewArt, NewTrack, Tag, TrackArt};
use musefs_format::{EmbeddedBinaryTag, EmbeddedPicture, Extent, flac, mp3, mp4, ogg, wav};

use crate::byte_budget::ByteBudget;
use crate::error::Result;
use crate::freshness::BackingStamp;
use std::fmt;
use std::sync::Arc;
use std::sync::mpsc::sync_channel;

const BATCH_FILES: usize = 256;
const BATCH_BYTES: u64 = 64 << 20; // 64 MiB

/// Initial bounded-read window. Sized to cover most files' metadata in one read;
/// larger metadata (e.g. embedded cover art) triggers a precise `NeedMore` widen.
const WINDOW: usize = 1 << 16; // 64 KiB
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

/// A progress event emitted during a scan or revalidate. Borrows the current
/// path to avoid a per-file allocation in the writer; the saved allocation is
/// negligible next to the existing per-file `to_string_lossy` + DB write, so do
/// not contort the API to preserve the borrow.
#[derive(Debug, Clone, Copy)]
pub enum ScanProgress<'a> {
    /// A supported-audio file was found during the walk; `found` is the running
    /// count of collected files.
    Discovered { found: u64 },
    /// The walk (and, for revalidate, the skip-unchanged pass) finished;
    /// `total` files will be ingested and tracked by the determinate bar.
    Walked { total: u64 },
    /// A file was committed. `done` runs 1..=total; `path` is its absolute path.
    Ingested {
        done: u64,
        total: u64,
        path: &'a str,
    },
}

/// UI-agnostic progress callback for [`ScanOptions`]. Invoked only from the
/// caller's thread (the walk and the single writer), never from probe workers.
/// The `Send + Sync` bound is not required by today's code; it is deliberate
/// future-proofing and free here (`indicatif::ProgressBar` is `Send + Sync`).
#[derive(Clone)]
pub struct ProgressSink(Arc<dyn for<'a> Fn(ScanProgress<'a>) + Send + Sync>);

impl ProgressSink {
    pub fn new(f: impl for<'a> Fn(ScanProgress<'a>) + Send + Sync + 'static) -> Self {
        ProgressSink(Arc::new(f))
    }

    fn emit(&self, ev: ScanProgress<'_>) {
        (self.0)(ev);
    }
}

impl fmt::Debug for ProgressSink {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ProgressSink")
    }
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
    collect_audio_with(root, out, follow_symlinks, None)
}

fn collect_audio_with(
    root: &Path,
    out: &mut Vec<PathBuf>,
    follow_symlinks: bool,
    progress: Option<&ProgressSink>,
) -> std::io::Result<SkipTally> {
    let mut visited = HashSet::new();
    let mut files_visited = HashSet::new();
    let mut tally = SkipTally::default();
    if follow_symlinks && let Ok(meta) = std::fs::metadata(root) {
        visited.insert(dir_key(&meta));
    }
    collect_audio_inner(
        root,
        out,
        follow_symlinks,
        &mut visited,
        &mut files_visited,
        &mut tally,
        progress,
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
    progress: Option<&ProgressSink>,
) -> std::io::Result<()> {
    for entry in std::fs::read_dir(root)? {
        let entry = entry?;
        let path = entry.path();
        let ftype = entry.file_type()?;
        if ftype.is_dir() {
            descend(
                &path,
                out,
                follow_symlinks,
                visited,
                files_visited,
                tally,
                progress,
            )?;
        } else if ftype.is_file() {
            if is_supported_audio(&path) {
                push_file(&path, out, follow_symlinks, files_visited, None, progress);
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
                    descend(
                        &path,
                        out,
                        follow_symlinks,
                        visited,
                        files_visited,
                        tally,
                        progress,
                    )?;
                }
                Ok(meta) if meta.is_file() => {
                    if is_supported_audio(&path) {
                        push_file(
                            &path,
                            out,
                            follow_symlinks,
                            files_visited,
                            Some(&meta),
                            progress,
                        );
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
    progress: Option<&ProgressSink>,
) -> std::io::Result<()> {
    if !follow_symlinks {
        return collect_audio_inner(
            path,
            out,
            follow_symlinks,
            visited,
            files_visited,
            tally,
            progress,
        );
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
    collect_audio_inner(
        path,
        out,
        follow_symlinks,
        visited,
        files_visited,
        tally,
        progress,
    )
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
    progress: Option<&ProgressSink>,
) {
    if !follow_symlinks {
        out.push(path.to_path_buf());
        if let Some(p) = progress {
            p.emit(ScanProgress::Discovered {
                found: out.len() as u64,
            });
        }
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
        _ => {
            out.push(path.to_path_buf());
            if let Some(p) = progress {
                p.emit(ScanProgress::Discovered {
                    found: out.len() as u64,
                });
            }
        }
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

/// Run [`probe_file`] under a panic boundary so a residual parser panic — one
/// the format-layer alloc guards (`id3v2_alloc_safe` and friends) don't catch —
/// drops just that file instead of unwinding the scan worker thread. An unwound
/// worker would skip its `failed.fetch_add`, and a crafted directory could kill
/// every worker, closing the channel so the writer reports success while
/// silently truncating the rest of the library (#425). A caught panic is logged
/// and folded into `ProbeOutcome::Unparseable`, which the worker already counts
/// as `failed`. Mirrors the read path's `read_outcome` boundary (#359).
fn probe_file_caught(path: &Path, window: usize) -> std::io::Result<ProbeOutcome> {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| probe_file(path, window))) {
        Ok(res) => res,
        Err(payload) => {
            let msg = payload
                .downcast_ref::<&str>()
                .copied()
                .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
                .unwrap_or("<non-string panic>");
            log::error!(
                "scan worker panicked probing {}: {msg}; counting as failed",
                path.display()
            );
            Ok(ProbeOutcome::Unparseable)
        }
    }
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

/// How much checksum work a scan does per file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChecksumTier {
    /// No checksums (legacy behavior).
    None,
    /// Compute the cheap fingerprint only (rides the probe).
    Fingerprint,
    /// Fingerprint plus an eager full-file SHA-256.
    Full,
}

/// How a fingerprint match is confirmed before a retarget.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MatchStrictness {
    /// Confirm with the full hash when the candidate has one; else trust the
    /// fingerprint.
    Auto,
    /// Fingerprint match is always sufficient; never read the full file.
    Fast,
    /// Require a full-hash match; refuse the retarget if the candidate has no
    /// stored content_hash.
    Strict,
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
    /// Optional progress callback. `None` (the default) disables reporting.
    pub progress: Option<ProgressSink>,
    /// Which checksums to compute and store this scan.
    pub checksum: ChecksumTier,
    /// How a refind fingerprint match is confirmed before retargeting.
    pub strictness: MatchStrictness,
}

impl Default for ScanOptions {
    fn default() -> Self {
        Self {
            jobs: 0,
            window: WINDOW,
            batch_bytes: BATCH_BYTES,
            follow_symlinks: false,
            progress: None,
            checksum: ChecksumTier::Fingerprint,
            strictness: MatchStrictness::Auto,
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

/// The write surface `ingest_into` drives: satisfied by both a direct `&Db`
/// (its methods take `&self`) and a batched `&mut BulkWriter` (`&mut self`), so
/// the upsert body lives in exactly one place. Each method delegates through the
/// concrete type path (`Db::`/`BulkWriter::`), which names the inherent method
/// unambiguously so the same-named trait method can't recurse into itself.
trait TrackSink {
    fn upsert_track(&mut self, t: &NewTrack) -> musefs_db::Result<i64>;
    fn replace_tags(&mut self, track_id: i64, tags: &[Tag]) -> musefs_db::Result<()>;
    fn set_binary_tags(
        &mut self,
        track_id: i64,
        tags: &[musefs_db::BinaryTag],
    ) -> musefs_db::Result<()>;
    fn set_structural_blocks(
        &mut self,
        track_id: i64,
        blocks: &[musefs_db::StructuralBlock],
    ) -> musefs_db::Result<()>;
    fn upsert_art(&mut self, a: &NewArt) -> musefs_db::Result<i64>;
    fn set_track_art(&mut self, track_id: i64, items: &[TrackArt]) -> musefs_db::Result<()>;
}

impl TrackSink for &Db {
    fn upsert_track(&mut self, t: &NewTrack) -> musefs_db::Result<i64> {
        Db::upsert_track(self, t)
    }
    fn replace_tags(&mut self, track_id: i64, tags: &[Tag]) -> musefs_db::Result<()> {
        Db::replace_tags(self, track_id, tags)
    }
    fn set_binary_tags(
        &mut self,
        track_id: i64,
        tags: &[musefs_db::BinaryTag],
    ) -> musefs_db::Result<()> {
        Db::set_binary_tags(self, track_id, tags)
    }
    fn set_structural_blocks(
        &mut self,
        track_id: i64,
        blocks: &[musefs_db::StructuralBlock],
    ) -> musefs_db::Result<()> {
        Db::set_structural_blocks(self, track_id, blocks)
    }
    fn upsert_art(&mut self, a: &NewArt) -> musefs_db::Result<i64> {
        Db::upsert_art(self, a)
    }
    fn set_track_art(&mut self, track_id: i64, items: &[TrackArt]) -> musefs_db::Result<()> {
        Db::set_track_art(self, track_id, items)
    }
}

impl TrackSink for &mut musefs_db::BulkWriter<'_> {
    fn upsert_track(&mut self, t: &NewTrack) -> musefs_db::Result<i64> {
        musefs_db::BulkWriter::upsert_track(self, t)
    }
    fn replace_tags(&mut self, track_id: i64, tags: &[Tag]) -> musefs_db::Result<()> {
        musefs_db::BulkWriter::replace_tags(self, track_id, tags)
    }
    fn set_binary_tags(
        &mut self,
        track_id: i64,
        tags: &[musefs_db::BinaryTag],
    ) -> musefs_db::Result<()> {
        musefs_db::BulkWriter::set_binary_tags(self, track_id, tags)
    }
    fn set_structural_blocks(
        &mut self,
        track_id: i64,
        blocks: &[musefs_db::StructuralBlock],
    ) -> musefs_db::Result<()> {
        musefs_db::BulkWriter::set_structural_blocks(self, track_id, blocks)
    }
    fn upsert_art(&mut self, a: &NewArt) -> musefs_db::Result<i64> {
        musefs_db::BulkWriter::upsert_art(self, a)
    }
    fn set_track_art(&mut self, track_id: i64, items: &[TrackArt]) -> musefs_db::Result<()> {
        musefs_db::BulkWriter::set_track_art(self, track_id, items)
    }
}

/// Upsert a track from a probed backing file into `w`: write the track row,
/// replace its seeded tags, and ingest its embedded art (capped, deduped,
/// clamped). The single source of the ingest body shared by `ingest` (direct
/// `&Db`) and `ingest_bulk` (batched `BulkWriter`). Takes `probed` by value so
/// picture/binary-tag/structural-block bytes are moved, not cloned (#68).
fn ingest_into(
    mut w: impl TrackSink,
    abs_path: &str,
    stamp: BackingStamp,
    probed: Probed,
) -> Result<()> {
    let track_id = w.upsert_track(&NewTrack {
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
    for (key, value) in probed.tags {
        if !key_passes_floor(&key) {
            continue;
        }
        let ord = ordinals.entry(key.clone()).or_insert(0);
        tags.push(Tag::new(&key, &value, *ord));
        *ord += 1;
    }
    w.replace_tags(track_id, &tags)?;

    let binary_tags = accept_binary_tags(abs_path, probed.binary_tags);
    w.set_binary_tags(track_id, &binary_tags)?;

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
    w.set_structural_blocks(track_id, &structural_blocks)?;

    let mut track_arts = Vec::new();
    for (ordinal, pic) in accept_pictures(abs_path, probed.pictures)
        .into_iter()
        .enumerate()
    {
        let art_id = w.upsert_art(&NewArt {
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
    w.set_track_art(track_id, &track_arts)?;
    Ok(())
}

/// Upsert a track from a probed backing file through a direct `&Db`. Thin
/// wrapper over [`ingest_into`]; the `oracle`/non-bulk scan path.
fn ingest(db: &Db, abs_path: &str, meta: &std::fs::Metadata, probed: Probed) -> Result<()> {
    ingest_into(db, abs_path, BackingStamp::from_metadata(meta), probed)
}

/// Like [`ingest`], but writes through a batch `BulkWriter`. Thin wrapper over
/// [`ingest_into`]; the `stamp` is captured once by the caller's `fstat`.
fn ingest_bulk(
    bw: &mut musefs_db::BulkWriter<'_>,
    abs_path: &str,
    stamp: BackingStamp,
    probed: Probed,
) -> Result<()> {
    ingest_into(bw, abs_path, stamp, probed)
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
    // Canonicalize the root once. With symlinks unfollowed (the default) every
    // path the walk yields is then already absolute and symlink-free — i.e.
    // canonical — so the workers need not canonicalize each probed file (#440).
    let canon = std::fs::canonicalize(root)?;
    let root = canon.as_path();
    let mut files = Vec::new();
    let mut tally = SkipTally::default();
    if root.is_file() {
        if is_supported_audio(root) {
            files.push(root.to_path_buf());
        } else {
            tally.record(root);
        }
    } else {
        tally = collect_audio_with(
            root,
            &mut files,
            opts.follow_symlinks,
            opts.progress.as_ref(),
        )?;
    }
    if let Some(p) = &opts.progress {
        p.emit(ScanProgress::Walked {
            total: files.len() as u64,
        });
    }
    db.apply_bulk_pragmas_self()?; // scan-scoped tuning on the caller's connection
    let mut stats = run_pipeline(db, files, opts)?;
    // skipped is tallied during the walk, not the pipeline
    stats.skipped = tally.total;
    // Per-extension breakdown of the skip count, so a large `skipped` is
    // diagnosable (#341). Log-only: never folded into `stats`/the CLI summary.
    if let Some(summary) = tally.summary() {
        log::warn!("{summary}");
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
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

    let jobs = effective_jobs(opts.jobs);
    let total = files.len() as u64;
    let progress = opts.progress.as_ref();
    let window = opts.window;
    let follow_symlinks = opts.follow_symlinks;
    let cap = opts.batch_bytes;
    let budget = Arc::new(ByteBudget::new(cap));
    let failed = Arc::new(AtomicU64::new(0));
    let raced = Arc::new(AtomicU64::new(0));

    // Work queue: a shared slice with an atomic cursor — each worker claims the
    // next index with a single relaxed `fetch_add`, no per-file lock contention.
    let files = Arc::new(files);
    let cursor = Arc::new(AtomicUsize::new(0));
    let (tx, rx) = sync_channel::<Unit>(jobs * 2);

    let mut workers = Vec::with_capacity(jobs);
    for _ in 0..jobs {
        let files = Arc::clone(&files);
        let cursor = Arc::clone(&cursor);
        let tx = tx.clone();
        let budget = Arc::clone(&budget);
        let failed = Arc::clone(&failed);
        let raced = Arc::clone(&raced);
        workers.push(std::thread::spawn(move || {
            loop {
                let i = cursor.fetch_add(1, Ordering::Relaxed);
                let Some(path) = files.get(i) else { break };
                match probe_file_caught(path, window) {
                    Ok(ProbeOutcome::Probed(probed, stamp)) => {
                        // No-follow paths are canonical by construction (the root
                        // was canonicalized up front); only the opt-in symlink walk
                        // can yield a path with a symlink component to resolve (#440).
                        let abs_path = if follow_symlinks {
                            match std::fs::canonicalize(path) {
                                Ok(abs) => abs.to_string_lossy().into_owned(),
                                Err(e) => {
                                    log::warn!("skipping {}: {e}", path.display());
                                    failed.fetch_add(1, Ordering::Relaxed);
                                    continue;
                                }
                            }
                        } else {
                            path.to_string_lossy().into_owned()
                        };
                        let weight = payload_weight(&probed);
                        budget.acquire(weight); // backpressure on in-flight art bytes
                        let unit = Unit {
                            abs_path,
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
        let mut released = 0u64;
        // `Ingested` reports committed files, so buffer the paths and emit only
        // after `bw.commit()` succeeds — a failed commit aborts the scan without
        // having advanced the progress bar past unpersisted files.
        let mut committed: Vec<String> = Vec::new();
        for Unit {
            abs_path,
            stamp,
            probed,
            weight,
        } in batch.drain(..)
        {
            released += weight;
            ingest_bulk(&mut bw, &abs_path, stamp, probed)?;
            committed.push(abs_path);
        }
        bw.commit()?;
        for abs_path in committed {
            *scanned += 1;
            if let Some(p) = progress {
                p.emit(ScanProgress::Ingested {
                    done: *scanned,
                    total,
                    path: &abs_path,
                });
            }
        }
        // Coalesce into one wakeup: the commit frees the whole batch, so a single
        // release avoids waking every blocked producer once per committed file.
        budget.release(released);
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
    // Canonicalize once; see scan_directory_with (#440). The prune pass below reuses
    // this canonical root for its `starts_with` scope check.
    let canon = std::fs::canonicalize(root)?;
    let root = canon.as_path();
    let mut files = Vec::new();
    if root.is_file() {
        if is_supported_audio(root) {
            files.push(root.to_path_buf());
        }
    } else {
        collect_audio_with(
            root,
            &mut files,
            opts.follow_symlinks,
            opts.progress.as_ref(),
        )?;
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
        let key = if opts.follow_symlinks {
            match std::fs::canonicalize(&path) {
                Ok(abs) => abs.to_string_lossy().into_owned(),
                Err(e) => {
                    log::warn!("skipping {}: {e}", path.display());
                    skip_failed += 1;
                    continue;
                }
            }
        } else {
            path.to_string_lossy().into_owned()
        };
        if let Some((stamp, id, format)) = existing.get(&key).copied() {
            let needs_backfill = format == Format::Flac && !have_structural.contains(&id);
            if crate::freshness::BackingStamp::from_metadata(&meta) == stamp && !needs_backfill {
                unchanged += 1;
                continue;
            }
        }
        changed.push(path);
    }

    if let Some(p) = &opts.progress {
        p.emit(ScanProgress::Walked {
            total: changed.len() as u64,
        });
    }

    let scan = run_pipeline(db, changed, opts)?;

    // Prune + GC on the writer connection (single-threaded), unchanged from before.
    let canon_root = root;
    let mut pruned = 0u64;
    for track in db.list_tracks()? {
        if !Path::new(&track.backing_path).starts_with(canon_root) {
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

/// SHA-256 of the probe's parsed output, hex-encoded. This is the cheap content
/// fingerprint: deterministic per file (the parsed `Probed` is window- and
/// format-independent), and excludes every filesystem-stamp field. Length-prefix
/// every variable-length field so concatenation can't alias.
#[allow(dead_code)]
pub(crate) fn fingerprint_of(p: &Probed) -> String {
    use sha2::{Digest, Sha256};
    // Inner fn (not a closure) so it doesn't hold a borrow of `h` across the
    // direct `h.update(...)` calls below.
    fn feed(h: &mut Sha256, bytes: &[u8]) {
        h.update((bytes.len() as u64).to_le_bytes());
        h.update(bytes);
    }
    let mut h = Sha256::new();
    feed(&mut h, p.format.as_str().as_bytes());
    h.update(p.audio_offset.to_le_bytes());
    h.update(p.audio_length.to_le_bytes());
    h.update((p.tags.len() as u64).to_le_bytes());
    for (k, v) in &p.tags {
        feed(&mut h, k.as_bytes());
        feed(&mut h, v.as_bytes());
    }
    h.update((p.pictures.len() as u64).to_le_bytes());
    for pic in &p.pictures {
        feed(&mut h, pic.mime.as_bytes());
        h.update(u64::from(pic.picture_type.get()).to_le_bytes());
        feed(&mut h, &pic.data);
    }
    h.update((p.binary_tags.len() as u64).to_le_bytes());
    for bt in &p.binary_tags {
        feed(&mut h, bt.key.as_bytes());
        feed(&mut h, &bt.payload);
    }
    h.update((p.structural_blocks.len() as u64).to_le_bytes());
    for (kind, body) in &p.structural_blocks {
        feed(&mut h, kind.as_bytes());
        feed(&mut h, body);
    }
    format!("{:x}", base16ct::HexDisplay(&h.finalize()))
}

/// Streaming SHA-256 of an entire backing file, hex-encoded. The authoritative
/// content identity; reads the whole file, so callers gate it on the `Full` tier
/// or a strict-confirmation need.
#[allow(dead_code)]
pub(crate) fn full_file_hash(path: &std::path::Path) -> std::io::Result<String> {
    use sha2::{Digest, Sha256};
    let mut f = std::fs::File::open(path)?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 16];
    loop {
        let n = std::io::Read::read(&mut f, &mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(format!("{:x}", base16ct::HexDisplay(&h.finalize())))
}

#[cfg(test)]
mod bounded_probe_tests;
#[cfg(test)]
mod hardening_tests;
#[cfg(test)]
mod ogg_probe_tests;
#[cfg(test)]
mod scan_unit_tests;
#[cfg(test)]
mod wav_probe_tests;
