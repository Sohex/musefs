use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

use musefs_db::convert::usize_from;
use musefs_db::{Db, Format, NewArt, NewTrack, Tag, TrackArt};
use musefs_format::{EmbeddedBinaryTag, EmbeddedPicture, Extent, flac, mp3, mp4, ogg, wav};

use crate::byte_budget::ByteBudget;
use crate::error::Result;
use crate::freshness::BackingStamp;
use musefs_db::limits::{
    MAX_ART_DESCRIPTION_LEN, MAX_ART_MIME_LEN, MAX_TAG_KEY_LEN, MAX_TAG_VALUE_LEN,
};
use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
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

/// The artwork-size ceiling. Enforced here at ingest (a file carrying oversize
/// art fails, see [`check_storable`]) and at resolve in
/// `mapping::track_art_to_inputs` (oversize art from any writer is rejected).
/// Sized to clear FLAC's 24-bit block length with headroom for the
/// picture-block framing.
pub(crate) const MAX_ART_BYTES: usize = 16 * 1024 * 1024 - 64 * 1024;

/// Per-frame cap for opaque binary tags, mirroring `MAX_ART_BYTES`. A file with
/// an oversize payload (e.g. a GEOB embedding a multi-MB file) fails.
const MAX_BINARY_TAG_BYTES: usize = MAX_ART_BYTES;

/// Outcome of probing one backing file. `Failed` carries the reason and the
/// line to log for a file that was supported and then lost — unparseable bytes,
/// oversize metadata, a caught parser panic — and is counted as a scan `failed`;
/// the caller tallies and logs it rather than the probe, so the warns can be
/// capped per reason (#651). `Raced` means the file changed under us between the
/// pre- and post-probe `fstat` — the probe may be torn, so nothing is committed
/// for it (#276).
#[derive(Debug)]
enum ProbeOutcome {
    Probed(Probed, BackingStamp),
    Failed(Failure),
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
    /// A dispatched file finished without being committed — it failed to probe
    /// or raced. `done` shares the same 1..=total sequence as [`Self::Ingested`],
    /// so the two together always reach `total` however many files fail (#655).
    ///
    /// There is no `path`: the failure was recorded on a probe worker, and only
    /// the count crosses back to the writer thread a [`ProgressSink`] may be
    /// invoked from. The `warn` line naming the file is the record of *which*.
    Failed { done: u64, total: u64 },
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
    /// Files skipped because a track already exists at that path.
    pub already_present: u64,
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

/// Warn budget per skip reason, per scan. The first `SCAN_WARN_BURST` skips of
/// a kind are logged individually at their natural level; the rest fall to
/// `debug` and are carried by the end-of-scan breakdown instead. Without a cap,
/// an unreadable subtree or a share that vanished mid-scan emits one line per
/// file (#651). Mirrors the serve path's `rate_limited_warn` burst
/// (`musefs-fuse`), but a scan is a bounded operation rather than a long-lived
/// mount, so the budget is a plain per-reason count and not a sliding window.
const SCAN_WARN_BURST: u64 = 10;

/// Why one entry was passed over. Buckets the end-of-scan breakdowns and keys
/// the per-reason warn budget.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SkipReason {
    /// Supported extension, but the bytes did not parse.
    Unparseable,
    /// Metadata over a storage cap — art, tag field, or binary frame (#644).
    Oversize,
    /// `open`/`stat`/`canonicalize` failed on a file the walk had accepted.
    Io,
    /// A parser panic caught by [`probe_file_caught`] (#425).
    Panicked,
    /// The file changed under the probe, so nothing was committed (#276).
    Raced,
    /// A directory or directory entry the walk could not read or classify.
    WalkUnreadable,
    /// A symlink the walk could not resolve, or one closing a cycle.
    WalkSymlink,
}

impl SkipReason {
    /// Every reason, in [`FailureTally`]'s array order (each reason indexes that
    /// array by its discriminant).
    const ALL: [SkipReason; 7] = [
        SkipReason::Unparseable,
        SkipReason::Oversize,
        SkipReason::Io,
        SkipReason::Panicked,
        SkipReason::Raced,
        SkipReason::WalkUnreadable,
        SkipReason::WalkSymlink,
    ];

    /// The reasons that increment `ScanStats::failed`. They partition it
    /// exactly, which is what makes the `failed N: ...` breakdown trustworthy.
    const FAILED: [SkipReason; 4] = [
        SkipReason::Unparseable,
        SkipReason::Oversize,
        SkipReason::Io,
        SkipReason::Panicked,
    ];

    /// Walk-time entries, counted in no `ScanStats` field: no file was ever
    /// queued for them, so they are neither `skipped` (the unsupported-extension
    /// tally) nor `failed` (a probe or ingest that ran and lost).
    const WALK: [SkipReason; 2] = [SkipReason::WalkUnreadable, SkipReason::WalkSymlink];

    fn label(self) -> &'static str {
        match self {
            SkipReason::Unparseable => "unparseable",
            SkipReason::Oversize => "oversize",
            SkipReason::Io => "io",
            SkipReason::Panicked => "panicked",
            SkipReason::Raced => "changed-during-probe",
            SkipReason::WalkUnreadable => "unreadable",
            SkipReason::WalkSymlink => "symlink",
        }
    }

    /// The level the in-budget lines for this reason are logged at. A caught
    /// parser panic keeps `error`: it is a musefs bug, not a property of the
    /// library being scanned.
    fn level(self) -> log::Level {
        match self {
            SkipReason::Panicked => log::Level::Error,
            _ => log::Level::Warn,
        }
    }
}

/// What the warn budget says about one skip. Split out of [`FailureTally::record`]
/// so the policy is testable without a capturing logger, mirroring
/// `WarnLimiter::decide` on the serve path.
#[derive(Debug, PartialEq, Eq)]
enum WarnBudget {
    /// In budget: log the line at the reason's own level.
    Spend(log::Level),
    /// The first line over budget: say once that the rest are going to `debug`.
    Exhausted,
    /// Over budget: `debug` only.
    Over,
}

/// Per-reason tally of everything a scan passed over, doing two jobs off one
/// counter: it caps the per-file warns (the first [`SCAN_WARN_BURST`] of each
/// reason at their natural level, the rest at `debug`) and it backs the
/// end-of-scan breakdown that explains the otherwise bare `failed N` — the
/// number the CLI reports and the one that drives the exit-2 partial-failure
/// signal, so the one users most need explained (#651).
///
/// Distinct from [`SkipTally`], which buckets *unsupported-extension* files by
/// extension: that breakdown is log-only and must never reach `ScanStats`
/// (#341), whereas these failure counts partition `ScanStats::failed` exactly.
///
/// Shared across probe workers through `&self` — relaxed counters, no lock. The
/// summaries are read after every worker has joined, and the warn budget only
/// needs each caller to draw a distinct sequence number.
#[derive(Debug, Default)]
struct FailureTally {
    counts: [AtomicU64; SkipReason::ALL.len()],
}

impl FailureTally {
    /// Count one skip and decide its log level, spending this reason's warn
    /// budget. Counting and deciding are the same atomic step: two callers can
    /// never draw the same sequence number, so the "rest are at debug" notice is
    /// emitted exactly once per reason however many workers race here.
    fn decide(&self, reason: SkipReason) -> WarnBudget {
        let prior = self.counts[reason as usize].fetch_add(1, Ordering::Relaxed);
        // Fully qualified: the atomic `Ordering` is what this module imports.
        match prior.cmp(&SCAN_WARN_BURST) {
            std::cmp::Ordering::Less => WarnBudget::Spend(reason.level()),
            std::cmp::Ordering::Equal => WarnBudget::Exhausted,
            std::cmp::Ordering::Greater => WarnBudget::Over,
        }
    }

    /// Record one skip and log `message` under this reason's warn budget: the
    /// first [`SCAN_WARN_BURST`] go out at [`SkipReason::level`], then one line
    /// says where the rest went, and the remainder are `debug`.
    fn record(&self, reason: SkipReason, message: fmt::Arguments<'_>) {
        match self.decide(reason) {
            WarnBudget::Spend(level) => log::log!(level, "{message}"),
            WarnBudget::Exhausted => {
                log::warn!(
                    "further \"{}\" skips are logged at debug ({SCAN_WARN_BURST} already reported); \
                     the end-of-scan summary has the totals",
                    reason.label()
                );
                log::debug!("{message}");
            }
            WarnBudget::Over => log::debug!("{message}"),
        }
    }

    fn count(&self, reason: SkipReason) -> u64 {
        self.counts[reason as usize].load(Ordering::Relaxed)
    }

    /// Everything counted toward `ScanStats::failed`.
    fn failed_total(&self) -> u64 {
        SkipReason::FAILED.iter().map(|r| self.count(*r)).sum()
    }

    /// `failed 37: unparseable=30, io=5, oversize=2` — the breakdown of
    /// `ScanStats::failed`. `None` when nothing failed.
    fn failed_summary(&self) -> Option<String> {
        self.summary("failed", &SkipReason::FAILED)
    }

    /// `walk errors 12: unreadable=9, symlink=3` — entries the walk could not
    /// read or classify, counted in no `ScanStats` field. `None` when the walk
    /// was clean.
    fn walk_summary(&self) -> Option<String> {
        self.summary("walk errors", &SkipReason::WALK)
    }

    /// One summary line over `group`: `<label> <total>: reason=n, ...`, buckets
    /// ordered by descending count with ties broken by reason name (matching
    /// [`SkipTally::summary`]) and empty buckets omitted. `None` when the group
    /// is empty, so there is no line to emit.
    ///
    /// [`SkipReason::Raced`] belongs to no group: it has exactly one cause, so a
    /// breakdown would only repeat its own name, and its total is already
    /// reported as `ScanStats::raced`.
    fn summary(&self, label: &str, group: &[SkipReason]) -> Option<String> {
        let mut buckets: Vec<(&'static str, u64)> = group
            .iter()
            .map(|r| (r.label(), self.count(*r)))
            .filter(|(_, n)| *n > 0)
            .collect();
        let total: u64 = buckets.iter().map(|(_, n)| *n).sum();
        if total == 0 {
            return None;
        }
        buckets.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(b.0)));
        let breakdown = buckets
            .iter()
            .map(|(reason, n)| format!("{reason}={n}"))
            .collect::<Vec<_>>()
            .join(", ");
        Some(format!("{label} {total}: {breakdown}"))
    }
}

/// Outcome of [`probe_body`]'s per-format dispatch: a parse, or the [`Failure`]
/// for the caller to tally and log.
#[derive(Debug)]
enum ProbeBody {
    Parsed(Probed),
    Failed(Failure),
}

/// One per-file failure on its way from the probe to the tally: the bucket it
/// counts toward and the line to log for it. Carried out of the probe rather
/// than logged where it happens so the tally — which lives with the worker
/// pool — sees every failure and can cap the warns (#651).
#[derive(Debug)]
struct Failure {
    reason: SkipReason,
    message: String,
}

impl Failure {
    fn new(reason: SkipReason, message: String) -> Failure {
        Failure { reason, message }
    }
}

/// The tallies threaded through the directory walk: the owned per-extension skip
/// breakdown handed back to the caller, plus a borrow of the scan-wide
/// [`FailureTally`] (shared with the probe workers) for entries the walk itself
/// cannot read. Bundled into one parameter so the recursion's argument list
/// stays inside clippy's limit.
struct WalkTally<'a> {
    skips: SkipTally,
    failures: &'a FailureTally,
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

/// Walk `root` with a throwaway failure tally — the callers that do not
/// aggregate walk errors (the legacy oracle scan, unit tests).
fn collect_audio(
    root: &Path,
    out: &mut Vec<PathBuf>,
    follow_symlinks: bool,
) -> std::io::Result<SkipTally> {
    collect_audio_with(root, out, follow_symlinks, None, &FailureTally::default())
}

fn collect_audio_with(
    root: &Path,
    out: &mut Vec<PathBuf>,
    follow_symlinks: bool,
    progress: Option<&ProgressSink>,
    failures: &FailureTally,
) -> std::io::Result<SkipTally> {
    let mut visited = HashSet::new();
    let mut files_visited = HashSet::new();
    let mut tally = WalkTally {
        skips: SkipTally::default(),
        failures,
    };
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
    Ok(tally.skips)
}

fn collect_audio_inner(
    root: &Path,
    out: &mut Vec<PathBuf>,
    follow_symlinks: bool,
    visited: &mut HashSet<(u64, u64)>,
    files_visited: &mut HashSet<(u64, u64)>,
    tally: &mut WalkTally<'_>,
    progress: Option<&ProgressSink>,
) -> std::io::Result<()> {
    // A single unreadable subtree or vanished entry must drop only that entry,
    // not abort the whole ingest — matching the log-and-continue resilience of
    // the symlink arm below and `probe_file` (#534). The top-level root is
    // validated upstream by `scan_directory_with`'s canonicalize, so a genuine
    // bad root is still reported there.
    let entries = match std::fs::read_dir(root) {
        Ok(entries) => entries,
        Err(e) => {
            tally.failures.record(
                SkipReason::WalkUnreadable,
                format_args!("skipping directory {}: {e}", root.display()),
            );
            return Ok(());
        }
    };
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(e) => {
                tally.failures.record(
                    SkipReason::WalkUnreadable,
                    format_args!("skipping unreadable entry in {}: {e}", root.display()),
                );
                continue;
            }
        };
        let path = entry.path();
        let ftype = match entry.file_type() {
            Ok(ftype) => ftype,
            Err(e) => {
                tally.failures.record(
                    SkipReason::WalkUnreadable,
                    format_args!("skipping {}: {e}", path.display()),
                );
                continue;
            }
        };
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
                tally.skips.record(&path);
            }
        } else if ftype.is_symlink() {
            if !follow_symlinks {
                // Routine and expected (symlinks are off by default); a library
                // sitting next to symlinked dirs would otherwise flood stderr at
                // the default `warn` floor. The end-of-scan skip tally still
                // surfaces what was passed over.
                log::debug!(
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
                        tally.skips.record(&path);
                    }
                }
                Ok(_) => {}
                Err(e) => {
                    tally.failures.record(
                        SkipReason::WalkSymlink,
                        format_args!("skipping broken symlink {}: {e}", path.display()),
                    );
                }
            }
        } else {
            // A direct special file (FIFO, char/block device, socket) — not a
            // file, dir, or symlink. The audio invariant is unaffected (it is
            // never opened), but tally it so it surfaces in the skip breakdown
            // rather than vanishing without a trace, matching unsupported
            // regular files above (#544).
            tally.skips.record(&path);
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
    tally: &mut WalkTally<'_>,
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
            // `path` is a directory or a symlink to one; a failed `stat` cannot
            // tell them apart, so it buckets with the walk's other unreadables.
            tally.failures.record(
                SkipReason::WalkUnreadable,
                format_args!("skipping directory {}: {e}", path.display()),
            );
            return Ok(());
        }
    };
    if !visited.insert(dir_key(&meta)) {
        tally.failures.record(
            SkipReason::WalkSymlink,
            format_args!("skipping symlink cycle at {}", path.display()),
        );
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

/// Assemble the FLAC `Probed` for an already-located audio region, reading tags
/// and pictures out of `prefix`.
///
/// A FLAC carrying a leading ID3v2 tag (#602) has that tag's contents ingested
/// too, as a *fallback*: the Vorbis comments are authoritative, and ID3 frames
/// only fill keys the Vorbis block does not define. So a file tagged both ways
/// keeps what a FLAC-aware editor wrote, while a file whose tags live only in
/// the ID3 header still lands in the store instead of being skipped. Pictures
/// follow the same rule — `PICTURE` blocks win outright, ID3 `APIC` frames are
/// the fallback.
///
/// The ID3 tag itself is not preserved: like MP3's, it is metadata, and the
/// synthesized file is regenerated metadata in front of untouched audio.
fn flac_probed(prefix: &[u8], scan: &flac::FlacScan) -> Probed {
    let (structural_blocks, binary_tags) = flac::split_preserved(&scan.preserved);
    let mut tags = flac::read_vorbis_comments(prefix).unwrap_or_default();
    let mut pictures = flac::read_pictures(prefix).unwrap_or_default();
    if flac::has_leading_id3(prefix) {
        fill_absent_keys(&mut tags, mp3::read_tags(prefix));
        if pictures.is_empty() {
            pictures = mp3::read_pictures(prefix);
        }
    }
    Probed {
        format: Format::Flac,
        audio_offset: scan.audio_offset,
        audio_length: scan.audio_length,
        tags,
        pictures,
        binary_tags,
        structural_blocks,
    }
}

/// Append the `fallback` pairs whose key is absent from `tags`, comparing keys
/// case-insensitively as the store does.
///
/// All-or-nothing per key: a key already in `tags` keeps every one of its values
/// and takes none from `fallback`, so two sources never interleave values for one
/// key (which would silently double a multi-value field like `artist`).
fn fill_absent_keys(tags: &mut Vec<(String, String)>, fallback: Vec<(String, String)>) {
    if fallback.is_empty() {
        return;
    }
    let present: HashSet<String> = tags.iter().map(|(k, _)| k.to_ascii_lowercase()).collect();
    tags.extend(
        fallback
            .into_iter()
            .filter(|(k, _)| !present.contains(&k.to_ascii_lowercase())),
    );
}

/// Full-buffer probe (legacy path). Retained as the reference implementation the
/// bounded path is checked against (see the equivalence property test).
pub(crate) fn probe_full(path: &Path, bytes: &[u8]) -> Option<Probed> {
    if has_ext(path, "flac") {
        Some(flac_probed(bytes, &flac::locate_audio(bytes).ok()?))
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
        // Oversize `covr`/`----` payloads fail the file (#644). This oracle path
        // has no error channel, so it reports the same verdict as `None` — which
        // its caller already counts as `failed`, matching the bounded probe.
        if let Some(e) = mp4_oversize_error(path, &art_drops, &bin_drops) {
            log::warn!("skipping {e}");
            return None;
        }
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
/// Returns `ProbeOutcome::Failed` for a supported-extension file that does not
/// parse or cannot be stored (counted as `failed`) and `ProbeOutcome::Raced` if
/// the file changed under us — a race outranks a failure, since a torn probe
/// says nothing about whether the settled file would parse.
fn probe_file(path: &Path, window: usize) -> std::io::Result<ProbeOutcome> {
    let file = std::fs::File::open(path)?;
    crate::metrics::on_scan_open();
    let s1 = BackingStamp::from_metadata(&file.metadata()?);
    #[cfg(test)]
    fire_after_s1();

    let probed = probe_body(path, &file, s1.size, window)?;

    let s2 = BackingStamp::from_metadata(&file.metadata()?);
    if s1 != s2 {
        return Ok(ProbeOutcome::Raced);
    }
    Ok(match probed {
        ProbeBody::Parsed(p) => ProbeOutcome::Probed(p, s1),
        ProbeBody::Failed(f) => ProbeOutcome::Failed(f),
    })
}

/// Run [`probe_file`] under a panic boundary so a residual parser panic — one
/// the format-layer alloc guards (`id3v2_alloc_safe` and friends) don't catch —
/// drops just that file instead of unwinding the scan worker thread. An unwound
/// worker would skip its `failed.fetch_add`, and a crafted directory could kill
/// every worker, closing the channel so the writer reports success while
/// silently truncating the rest of the library (#425). A caught panic is folded
/// into a `Panicked` `ProbeOutcome::Failed`, which the worker already counts as
/// `failed` — and which keeps its `error` level when the caller logs it, since a
/// panic is a musefs bug rather than a property of the library being scanned.
/// Mirrors the read path's `read_outcome` boundary (#359).
fn probe_file_caught(path: &Path, window: usize) -> std::io::Result<ProbeOutcome> {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| probe_file(path, window))) {
        Ok(res) => res,
        Err(payload) => {
            let msg = payload
                .downcast_ref::<&str>()
                .copied()
                .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
                .unwrap_or("<non-string panic>");
            Ok(ProbeOutcome::Failed(Failure::new(
                SkipReason::Panicked,
                format!(
                    "scan worker panicked probing {}: {msg}; counting as failed",
                    path.display()
                ),
            )))
        }
    }
}

/// The per-format metadata dispatch for one already-opened backing file, over
/// its first `file_len` bytes. Split out of `probe_file` so the fstat-sandwich
/// wrapper stays legible. Never reads the audio payload (M4A uses the seek
/// reader; front-anchored formats read only the metadata extent).
/// `ProbeBody::Failed` is an unsupported/unparseable/oversize file — a verdict
/// for the caller to tally and log, distinct from the outer `Err`, which is an
/// I/O error on the already-open file.
fn probe_body(
    path: &Path,
    file: &std::fs::File,
    file_len: u64,
    window: usize,
) -> std::io::Result<ProbeBody> {
    // M4A: seek reader, never touches mdat.
    if has_ext(path, "m4a") || has_ext(path, "m4b") {
        let mut f = file;
        let scan = match mp4::read_structure_from(&mut f, file_len) {
            Ok(s) => s,
            Err(e) => {
                return Ok(ProbeBody::Failed(Failure::new(
                    SkipReason::Unparseable,
                    format!("skipping {}: {e}", path.display()),
                )));
            }
        };
        let (pictures, art_drops) = mp4::read_pictures_reporting(&scan.moov, MAX_ART_BYTES);
        let (binary_tags, bin_drops) =
            mp4::read_binary_tags_reporting(&scan.moov, MAX_BINARY_TAG_BYTES);
        // Oversize `covr`/`----` payloads fail the file (#644). Reported as
        // "no probe result", which the caller counts as `failed` — the same
        // verdict `check_storable` produces for every other format, reached one
        // layer earlier because the payload is never materialized here.
        if let Some(e) = mp4_oversize_error(path, &art_drops, &bin_drops) {
            return Ok(ProbeBody::Failed(Failure::new(
                SkipReason::Oversize,
                format!("skipping {e}"),
            )));
        }
        return Ok(ProbeBody::Parsed(Probed {
            format: Format::M4a,
            audio_offset: scan.mdat_payload_offset,
            audio_length: scan.mdat_payload_len,
            tags: mp4::read_tags(&scan.moov),
            pictures,
            binary_tags,
            structural_blocks: Vec::new(),
        }));
    }

    // Front-anchored formats: read a window, widen on NeedMore.
    // Never read past the probe ceiling, however large the file or whatever a
    // (possibly corrupt) header asks for via `NeedMore`.
    let probe_cap = file_len.min(MAX_PROBE_BYTES);
    let mut want = usize_from((window as u64).min(probe_cap));
    let mut prefix = read_window(file, want)?;
    // Only the MP3 arm of probe_prefix consumes the ID3v1 tail, plus the FLAC arm
    // for the rare file that puts an ID3v2 tag in front of the `fLaC` marker
    // (#602) — a stock .flac still pays no tail read (#67), and .ogg/.wav never
    // do. The `ID3` magic sits in the first 3 bytes, so this verdict does not
    // change as the window widens below.
    let tail = if has_ext(path, "mp3") || (has_ext(path, "flac") && flac::has_leading_id3(&prefix))
    {
        read_tail_128(file, file_len)?
    } else {
        None
    };
    for _ in 0..MAX_WIDEN_RETRIES {
        match probe_prefix(path, &prefix, file_len, tail.as_ref()) {
            Probe::Done(p) => return Ok(ProbeBody::Parsed(p)),
            Probe::Skip => {
                return Ok(ProbeBody::Failed(Failure::new(
                    SkipReason::Unparseable,
                    format!("skipping {}: no parseable audio metadata", path.display()),
                )));
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
        return Ok(ProbeBody::Parsed(p));
    }
    // A WAV whose `data` payload runs past the probe ceiling fails the strict
    // full-buffer parse (the payload isn't present to bound), yet its `fmt `/`data`
    // headers sit at the front: trust the declared bounds and serve the audio,
    // accepting the loss of any tag chunks trailing the payload.
    if has_ext(path, "wav")
        && file_len > MAX_PROBE_BYTES
        && let Ok(bounds) = wav::locate_audio_at_ceiling(&prefix, file_len)
    {
        return Ok(ProbeBody::Parsed(wav_probed(&prefix, &bounds)));
    }
    Ok(ProbeBody::Failed(unparseable(path, file_len)))
}

/// The "nothing parsed" verdict for one file, naming the probe ceiling when the
/// file is large enough that the ceiling is the likely reason.
fn unparseable(path: &Path, file_len: u64) -> Failure {
    let message = if file_len > MAX_PROBE_BYTES {
        format!(
            "skipping {}: no parseable metadata within first {MAX_PROBE_BYTES} bytes",
            path.display()
        )
    } else {
        format!("skipping {}: no parseable audio metadata", path.display())
    };
    Failure::new(SkipReason::Unparseable, message)
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
        match flac::locate_audio_bounded(prefix, file_len, tail) {
            Ok(Extent::Complete(scan)) => Probe::Done(flac_probed(prefix, &scan)),
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

/// Whether the writer overwrites curated metadata or only refreshes structural
/// serving facts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WritePolicy {
    /// Full upsert: track row, checksums, tags, binary tags, structural blocks,
    /// and art.
    Full,
    /// Layer-A-only refresh: track row, checksums, and structural blocks.
    StructuralOnly,
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
    /// Scan only: re-ingest files already present in the DB, overwriting
    /// curated metadata. Off by default; bare scan is additive.
    pub force: bool,
    /// Revalidate only: delete tracks whose backing file is gone and GC
    /// orphaned art. Off by default.
    pub prune: bool,
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
            force: false,
            prune: false,
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
    fingerprint: Option<String>,
    content_hash: Option<String>,
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

/// Every store cap the scanner can trip on an honest read of a legal file,
/// checked in one place before anything is written (#644).
///
/// A violation fails **that file**, not the scan. The caps are DB `CHECK`
/// constraints, so before this existed an over-cap tag surfaced as
/// `CHECK constraint failed: ...` from inside a batch commit — fatal, and with
/// the per-file context already gone. Checking here instead means the caller
/// counts one `failed` file, names it, and keeps going.
///
/// It also replaces the older drop-and-warn treatment of oversize art and
/// binary tags (#284): silently omitting a payload from the store leaves the
/// user with a mount that is quietly missing data, and a `warn` buried in a
/// scan of ten thousand files is easy to miss. Refusing the file is louder and
/// leaves no state nobody asked for.
///
/// Units are not uniform and the messages say which applies: SQLite's `length()`
/// counts **characters** on a TEXT column but **bytes** on a blob, and the
/// schema deliberately uses `length(CAST(value AS BLOB))` for `tags.value` so
/// its cap is a real memory bound (#505).
fn check_storable(abs_path: &str, probed: &Probed) -> Result<()> {
    // The two scanner-owned caps are `usize` consts; the DB's are `i64` because
    // they are compared against SQLite `length()`. Convert once here so the
    // per-field checks below all read the same way.
    let art_cap = i64::try_from(MAX_ART_BYTES).expect("art cap fits i64");
    let binary_cap = i64::try_from(MAX_BINARY_TAG_BYTES).expect("binary tag cap fits i64");
    let too_large = |item: String, len: usize, cap: i64, unit: &'static str| {
        crate::error::CoreError::TrackFieldTooLarge {
            path: abs_path.to_string(),
            item,
            len: len as u64,
            cap: cap.unsigned_abs(),
            unit,
        }
    };

    for (key, value) in &probed.tags {
        // Skipped keys are not stored, so they cannot violate anything.
        if !key_passes_floor(key) {
            continue;
        }
        let key_chars = key.chars().count();
        if i64::try_from(key_chars).unwrap_or(i64::MAX) > MAX_TAG_KEY_LEN {
            return Err(too_large(
                format!("tag key {key:?}"),
                key_chars,
                MAX_TAG_KEY_LEN,
                "characters",
            ));
        }
        if i64::try_from(value.len()).unwrap_or(i64::MAX) > MAX_TAG_VALUE_LEN {
            return Err(too_large(
                format!("tag {key:?}"),
                value.len(),
                MAX_TAG_VALUE_LEN,
                "bytes",
            ));
        }
    }

    for b in &probed.binary_tags {
        if b.payload.len() > MAX_BINARY_TAG_BYTES {
            return Err(too_large(
                format!("binary tag {:?}", b.key),
                b.payload.len(),
                binary_cap,
                "bytes",
            ));
        }
    }

    for p in &probed.pictures {
        if p.data.len() > MAX_ART_BYTES {
            return Err(too_large(
                format!("embedded {} art", p.mime),
                p.data.len(),
                art_cap,
                "bytes",
            ));
        }
        let mime_chars = p.mime.chars().count();
        if i64::try_from(mime_chars).unwrap_or(i64::MAX) > MAX_ART_MIME_LEN {
            return Err(too_large(
                "art MIME type".to_string(),
                mime_chars,
                MAX_ART_MIME_LEN,
                "characters",
            ));
        }
        let desc_chars = p.description.chars().count();
        if i64::try_from(desc_chars).unwrap_or(i64::MAX) > MAX_ART_DESCRIPTION_LEN {
            return Err(too_large(
                format!("description of the embedded {} art", p.mime),
                desc_chars,
                MAX_ART_DESCRIPTION_LEN,
                "characters",
            ));
        }
    }

    check_metadata_fits_format(abs_path, probed)
}

/// Reject a file whose tags, taken together, cannot fit the metadata container
/// its format synthesizes into.
///
/// Only the FLAC family needs this, and the reachable case is narrower than it
/// looks. A stock FLAC cannot trip it: its whole `VORBIS_COMMENT` body is
/// length-prefixed with 24 bits, so the comments inside can never sum past the
/// ceiling they were read from. What breaks that arithmetic is a **second tag
/// source merged into the first** — a FLAC carrying a leading ID3v2 tag (#602),
/// whose absent keys `fill_absent_keys` appends. ID3v2's synchsafe tag size is
/// 256 MiB, so the merged set can exceed what a FLAC comment block can hold.
///
/// Left unchecked, such a file scans clean and then fails synthesis with
/// `FormatError::TooLarge`, serving `EIO` on every read — a file that is
/// present in the mount and unreadable, with nothing in the scan log to explain
/// it. That is precisely the unanticipated state the fail-the-file policy
/// exists to prevent, so the arithmetic runs here, where it can still name the
/// file.
///
/// Conservative by construction: this is the *lower bound* on the synthesized
/// body (real synthesis maps and may add comments, never shrinks below the raw
/// pairs), so a file over the ceiling here provably cannot be served. Under it,
/// nothing is claimed.
///
/// The remaining formats need no equivalent. Ogg Opus/Vorbis comment packets
/// carry 32-bit lengths and span pages, so they have no ceiling worth checking;
/// MP4 and WAV use 32-bit box/chunk lengths; and MP3 synthesizes back into the
/// same 256 MiB ID3v2 container it was read from.
fn check_metadata_fits_format(abs_path: &str, probed: &Probed) -> Result<()> {
    let (format_name, cap) = match probed.format {
        Format::Flac => ("FLAC", musefs_format::flac::MAX_BLOCK_BODY),
        Format::OggFlac => ("Ogg FLAC", musefs_format::flac::MAX_BLOCK_BODY),
        _ => return Ok(()),
    };
    // VORBIS_COMMENT body: 4-byte vendor length + vendor string + 4-byte comment
    // count + per comment (4-byte length + "KEY=VALUE"). The vendor string is
    // synthesis's own and unknown here; omitting it only makes the bound more
    // conservative, which is the safe direction.
    let mut total: u64 = 8;
    for (key, value) in &probed.tags {
        if !key_passes_floor(key) {
            continue;
        }
        total = total.saturating_add(4 + key.len() as u64 + 1 + value.len() as u64);
    }
    if total > cap {
        return Err(crate::error::CoreError::TrackMetadataTooLarge {
            path: abs_path.to_string(),
            format: format_name,
            len: total,
            cap,
        });
    }
    Ok(())
}

/// Assign storage ordinals to the binary tags worth keeping. Empty payloads are
/// dropped silently: they carry nothing to serve, so unlike an oversize payload
/// their absence costs the user nothing. Size is not decided here — that is
/// [`check_storable`]'s job, and by the time this runs the file has passed it.
fn storable_binary_tags(tags: Vec<EmbeddedBinaryTag>) -> Vec<musefs_db::BinaryTag> {
    tags.into_iter()
        .filter(|b| !b.payload.is_empty())
        .enumerate()
        .map(|(ordinal, b)| musefs_db::BinaryTag {
            key: b.key,
            payload: b.payload,
            ordinal: ordinal as u64,
        })
        .collect()
}

/// Build the [`CoreError`](crate::error::CoreError) for an mp4 payload the
/// format layer skipped before materialization (#343).
///
/// These drops happen inside `mp4::read_pictures_reporting` /
/// `mp4::read_binary_tags_reporting` — deliberately earlier than
/// [`check_storable`], to avoid building a large image out of a large `moov`
/// just to reject it. Only the *report* reaches here, never the payload, so the
/// decision is made at the probe site instead; routing the message through this
/// one helper keeps it identical to the ingest-time refusals.
fn mp4_oversize_error(
    path: &Path,
    art: &[mp4::OversizeDrop],
    binary: &[mp4::OversizeDrop],
) -> Option<crate::error::CoreError> {
    let (item, bytes, cap) = if let Some(d) = art.first() {
        (
            format!("embedded {} art", d.descriptor),
            d.bytes,
            MAX_ART_BYTES,
        )
    } else {
        let d = binary.first()?;
        (
            format!("binary tag {:?}", d.descriptor),
            d.bytes,
            MAX_BINARY_TAG_BYTES,
        )
    };
    Some(crate::error::CoreError::TrackFieldTooLarge {
        path: path.display().to_string(),
        item,
        len: bytes as u64,
        cap: cap as u64,
        unit: "bytes",
    })
}

fn structural_blocks_from(blocks: Vec<(String, Vec<u8>)>) -> Vec<musefs_db::StructuralBlock> {
    let mut ordinals: HashMap<String, u64> = HashMap::new();
    blocks
        .into_iter()
        .map(|(kind, body)| {
            let ord = ordinals.entry(kind.clone()).or_insert(0);
            let block = musefs_db::StructuralBlock {
                kind,
                ordinal: *ord,
                body,
            };
            *ord += 1;
            block
        })
        .collect()
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
    fn set_track_checksums(
        &mut self,
        track_id: i64,
        fingerprint: Option<&str>,
        content_hash: Option<&str>,
    ) -> musefs_db::Result<()>;
    fn track_exists_at(&mut self, path: &str) -> musefs_db::Result<bool>;
    fn tracks_by_fingerprint(&mut self, fp: &str) -> musefs_db::Result<Vec<musefs_db::Track>>;
    #[allow(clippy::too_many_arguments)]
    fn retarget_track(
        &mut self,
        id: i64,
        new_backing_path: &str,
        stamp: BackingStamp,
        audio_offset: u64,
        audio_length: u64,
        fingerprint: Option<&str>,
        content_hash: Option<&str>,
    ) -> musefs_db::Result<()>;
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
    fn set_track_checksums(
        &mut self,
        track_id: i64,
        fingerprint: Option<&str>,
        content_hash: Option<&str>,
    ) -> musefs_db::Result<()> {
        Db::set_track_checksums(self, track_id, fingerprint, content_hash)
    }
    fn track_exists_at(&mut self, path: &str) -> musefs_db::Result<bool> {
        Ok(Db::get_track_by_path(self, path)?.is_some())
    }
    fn tracks_by_fingerprint(&mut self, fp: &str) -> musefs_db::Result<Vec<musefs_db::Track>> {
        Db::tracks_by_fingerprint(self, fp)
    }
    fn retarget_track(
        &mut self,
        id: i64,
        new_backing_path: &str,
        stamp: BackingStamp,
        audio_offset: u64,
        audio_length: u64,
        fingerprint: Option<&str>,
        content_hash: Option<&str>,
    ) -> musefs_db::Result<()> {
        Db::retarget_track(
            self,
            id,
            new_backing_path,
            stamp.size,
            stamp.mtime_ns,
            stamp.ctime_ns,
            audio_offset,
            audio_length,
            fingerprint,
            content_hash,
        )
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
    fn set_track_checksums(
        &mut self,
        track_id: i64,
        fingerprint: Option<&str>,
        content_hash: Option<&str>,
    ) -> musefs_db::Result<()> {
        musefs_db::BulkWriter::set_track_checksums(self, track_id, fingerprint, content_hash)
    }
    fn track_exists_at(&mut self, path: &str) -> musefs_db::Result<bool> {
        Ok(musefs_db::BulkWriter::get_track_by_path(self, path)?.is_some())
    }
    fn tracks_by_fingerprint(&mut self, fp: &str) -> musefs_db::Result<Vec<musefs_db::Track>> {
        musefs_db::BulkWriter::tracks_by_fingerprint(self, fp)
    }
    fn retarget_track(
        &mut self,
        id: i64,
        new_backing_path: &str,
        stamp: BackingStamp,
        audio_offset: u64,
        audio_length: u64,
        fingerprint: Option<&str>,
        content_hash: Option<&str>,
    ) -> musefs_db::Result<()> {
        musefs_db::BulkWriter::retarget_track(
            self,
            id,
            new_backing_path,
            stamp.size,
            stamp.mtime_ns,
            stamp.ctime_ns,
            audio_offset,
            audio_length,
            fingerprint,
            content_hash,
        )
    }
}

/// Upsert a track from a probed backing file into `w`: write the track row,
/// replace its seeded tags, and ingest its embedded art (capped, deduped,
/// clamped). The single source of the ingest body shared by `ingest` (direct
/// `&Db`), `ingest_unit` (production batch path), and `ingest_bulk` (test-only
/// `BulkWriter` wrapper). Takes `probed` by value so
/// picture/binary-tag/structural-block bytes are moved, not cloned (#68).
fn ingest_into(
    mut w: impl TrackSink,
    abs_path: &str,
    stamp: BackingStamp,
    probed: Probed,
    fingerprint: Option<&str>,
    content_hash: Option<&str>,
) -> Result<()> {
    // The pipeline already rejected an over-cap file in the worker, before the
    // payload was ever buffered. Re-checking here is what makes the direct
    // `ingest` / `ingest_bulk` entry points honour the same contract instead of
    // writing a row the `CHECK` will reject mid-transaction.
    check_storable(abs_path, &probed)?;

    let track_id = w.upsert_track(&NewTrack {
        backing_path: abs_path.to_string(),
        format: probed.format,
        audio_offset: probed.audio_offset,
        audio_length: probed.audio_length,
        backing_size: stamp.size,
        backing_mtime_ns: stamp.mtime_ns,
        backing_ctime_ns: stamp.ctime_ns,
    })?;
    w.set_track_checksums(track_id, fingerprint, content_hash)?;

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

    let binary_tags = storable_binary_tags(probed.binary_tags);
    w.set_binary_tags(track_id, &binary_tags)?;

    let structural_blocks = structural_blocks_from(probed.structural_blocks);
    w.set_structural_blocks(track_id, &structural_blocks)?;

    let mut track_arts = Vec::new();
    for (ordinal, pic) in probed.pictures.into_iter().enumerate() {
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

/// Refresh only the structural serving facts for an already-probed file.
/// Leaves curated tags, binary tags, and art untouched.
fn refresh_structural_into(
    mut w: impl TrackSink,
    abs_path: &str,
    stamp: BackingStamp,
    probed: Probed,
    fingerprint: Option<&str>,
    content_hash: Option<&str>,
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
    w.set_track_checksums(track_id, fingerprint, content_hash)?;
    let structural_blocks = structural_blocks_from(probed.structural_blocks);
    w.set_structural_blocks(track_id, &structural_blocks)?;
    Ok(())
}

/// Decide how to ingest one probed unit: retarget a relocated row when a unique
/// fingerprint match exists whose backing file is gone, otherwise ingest fresh.
/// The strict/auto confirm hash, if computed here, is persisted on the retarget
/// (so a fingerprint-tier strict move doesn't re-read the file next scan).
fn ingest_unit(
    mut w: impl TrackSink,
    unit: Unit,
    strictness: MatchStrictness,
    policy: WritePolicy,
) -> Result<()> {
    if policy == WritePolicy::StructuralOnly {
        return refresh_structural_into(
            w,
            &unit.abs_path,
            unit.stamp,
            unit.probed,
            unit.fingerprint.as_deref(),
            unit.content_hash.as_deref(),
        );
    }
    // Known path => ordinary upsert (re-scan of an in-place file).
    if w.track_exists_at(&unit.abs_path)? {
        return ingest_into(
            w,
            &unit.abs_path,
            unit.stamp,
            unit.probed,
            unit.fingerprint.as_deref(),
            unit.content_hash.as_deref(),
        );
    }
    if let Some(fp) = unit.fingerprint.as_deref() {
        let candidates: Vec<musefs_db::Track> = w
            .tracks_by_fingerprint(fp)?
            .into_iter()
            .filter(|t| match std::fs::metadata(&t.backing_path) {
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => true,
                Ok(_) => false,
                Err(e) => {
                    log::warn!(
                        "skipping retarget candidate {}: cannot stat backing path ({e})",
                        t.backing_path
                    );
                    false
                }
            })
            .collect();
        if candidates.len() == 1 {
            let cand = &candidates[0];
            // Does this strictness need a full-hash confirm against this candidate?
            let needs_full = match strictness {
                MatchStrictness::Fast => false,
                MatchStrictness::Auto | MatchStrictness::Strict => cand.content_hash.is_some(),
            };
            // The new file's full hash: worker-computed if present, else read now
            // (the file is present — it's the move destination). A read error here
            // must not abort the whole scan — log it and fall through with `None`,
            // which fails the confirm and inserts this unit fresh.
            let new_hash: Option<String> = match (&unit.content_hash, needs_full) {
                (Some(h), _) => Some(h.clone()),
                (None, true) => match full_file_hash(std::path::Path::new(&unit.abs_path)) {
                    Ok(h) => Some(h),
                    Err(e) => {
                        log::warn!(
                            "hash confirm failed for {}: {e}; inserting fresh",
                            unit.abs_path
                        );
                        None
                    }
                },
                (None, false) => None,
            };
            let confirmed = match strictness {
                MatchStrictness::Fast => true,
                MatchStrictness::Auto | MatchStrictness::Strict => match &cand.content_hash {
                    // Strict with no stored hash => refuse; Auto with none => fingerprint is enough.
                    None => matches!(strictness, MatchStrictness::Auto),
                    Some(stored) => new_hash.as_deref() == Some(stored.as_str()),
                },
            };
            if confirmed && !w.track_exists_at(&unit.abs_path)? {
                w.retarget_track(
                    cand.id,
                    &unit.abs_path,
                    unit.stamp,
                    unit.probed.audio_offset,
                    unit.probed.audio_length,
                    unit.fingerprint.as_deref(),
                    new_hash.as_deref(),
                )?;
                return Ok(());
            }
            if !confirmed {
                log::warn!(
                    "fingerprint match for {} not confirmed (strictness {:?}); inserting fresh",
                    unit.abs_path,
                    strictness,
                );
            }
        } else if candidates.len() > 1 {
            log::warn!(
                "ambiguous fingerprint match for {} ({} missing candidates); inserting fresh",
                unit.abs_path,
                candidates.len(),
            );
        }
    }
    ingest_into(
        w,
        &unit.abs_path,
        unit.stamp,
        unit.probed,
        unit.fingerprint.as_deref(),
        unit.content_hash.as_deref(),
    )
}

/// Upsert a track from a probed backing file through a direct `&Db`. Thin
/// wrapper over [`ingest_into`]; the `oracle`/non-bulk scan path.
fn ingest(db: &Db, abs_path: &str, meta: &std::fs::Metadata, probed: Probed) -> Result<()> {
    ingest_into(
        db,
        abs_path,
        BackingStamp::from_metadata(meta),
        probed,
        None,
        None,
    )
}

/// Like [`ingest`], but writes through a batch `BulkWriter`. Thin wrapper over
/// [`ingest_into`]; the `stamp` is captured once by the caller's `fstat`. The
/// production batch path inlines `ingest_into` (it threads per-unit checksums),
/// so this wrapper now only serves the hardening tests' bulk-writer coverage.
#[cfg(test)]
fn ingest_bulk(
    bw: &mut musefs_db::BulkWriter<'_>,
    abs_path: &str,
    stamp: BackingStamp,
    probed: Probed,
) -> Result<()> {
    ingest_into(bw, abs_path, stamp, probed, None, None)
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
/// the scan. Those failures are tallied by reason and summarized at the end too
/// (#651), and their per-file warns are capped per reason so a whole unreadable
/// subtree cannot scale the log with the library.
pub fn scan_directory_with(db: &Db, root: &Path, opts: &ScanOptions) -> Result<ScanStats> {
    // Canonicalize the root once. With symlinks unfollowed (the default) every
    // path the walk yields is then already absolute and symlink-free — i.e.
    // canonical — so the workers need not canonicalize each probed file (#440).
    let canon = std::fs::canonicalize(root)?;
    let root = canon.as_path();
    let mut files = Vec::new();
    let mut tally = SkipTally::default();
    let failures = Arc::new(FailureTally::default());
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
            &failures,
        )?;
    }
    let mut already_present = 0u64;
    if !opts.force {
        // Projected to the one column this set needs: a full `list_tracks` would
        // materialize every row's checksum strings just to drop them (#621).
        let existing: HashSet<String> = db.list_backing_paths()?.into_iter().collect();
        let before = files.len();
        files.retain(|path| {
            let key = if opts.follow_symlinks {
                match std::fs::canonicalize(path) {
                    Ok(abs) => abs.to_string_lossy().into_owned(),
                    Err(_) => return true,
                }
            } else {
                path.to_string_lossy().into_owned()
            };
            !existing.contains(&key)
        });
        already_present = (before - files.len()) as u64;
    }
    if let Some(p) = &opts.progress {
        p.emit(ScanProgress::Walked {
            total: files.len() as u64,
        });
    }
    db.apply_bulk_pragmas_self()?; // scan-scoped tuning on the caller's connection
    let mut stats = run_pipeline(db, files, opts, WritePolicy::Full, &failures)?;
    // skipped is tallied during the walk, not the pipeline
    stats.skipped = tally.total;
    stats.already_present = already_present;
    // Per-extension breakdown of the skip count, so a large `skipped` is
    // diagnosable (#341). Log-only: never folded into `stats`/the CLI summary.
    //
    // `info`, not `warn`: every ordinary library has cover art, `.cue` and
    // `.log` sidecars, so a `warn` here fires on every healthy scan and teaches
    // operators to tune warnings out. The count itself is never hidden — the CLI
    // prints `skipped N` regardless of log level — so `-v` gates only the
    // breakdown, which is a diagnostic you go looking for (#651).
    if let Some(summary) = tally.summary() {
        log::info!("{summary}");
    }
    log_failure_summaries(&failures);
    Ok(stats)
}

/// Emit the end-of-scan breakdowns of everything that went wrong, at `warn`:
/// unlike the skip summary these do not fire on a healthy library, and `failed`
/// is what drives the CLI's exit-2 partial-failure signal, so it must be legible
/// at the default log floor (#651).
fn log_failure_summaries(failures: &FailureTally) {
    if let Some(summary) = failures.failed_summary() {
        log::warn!("{summary}");
    }
    if let Some(summary) = failures.walk_summary() {
        log::warn!("{summary}");
    }
}

/// Dispatched files that finished without being committed: probe failures plus
/// races. Both leave the writer with nothing to persist, so both have to
/// advance the progress sequence for it to reach the walked total (#655).
///
/// A free function rather than an inline sum so the arithmetic is reachable
/// from a unit test: the pipeline cannot produce `raced > 0` under test at all,
/// because the probe's mid-read race hook is a `thread_local!` and the probes
/// run on worker threads the test never touches. Left inline, the sum would be
/// indistinguishable from a difference in every test that can be written.
fn uncommitted_total(failed: u64, raced: u64) -> u64 {
    failed + raced
}

/// Back-compat shim used by the CLI and existing tests.
pub fn scan_directory(db: &Db, root: &Path) -> Result<ScanStats> {
    scan_directory_with(db, root, &ScanOptions::default())
}

/// Probe `files` across `jobs` workers (no DB access) and write the results from a
/// single writer (this thread) in batched transactions. Per-file errors are
/// counted, not fatal: every one of them is recorded in `failures` (which also
/// caps their warns) so the reason breakdown its caller logs partitions
/// `ScanStats::failed` exactly.
fn run_pipeline(
    db: &Db,
    files: Vec<PathBuf>,
    opts: &ScanOptions,
    policy: WritePolicy,
    failures: &Arc<FailureTally>,
) -> Result<ScanStats> {
    use std::sync::atomic::AtomicUsize;

    let jobs = effective_jobs(opts.jobs);
    let total = files.len() as u64;
    let progress = opts.progress.as_ref();
    let window = opts.window;
    let follow_symlinks = opts.follow_symlinks;
    let tier = opts.checksum;
    let strictness = opts.strictness;
    let cap = opts.batch_bytes;
    let budget = Arc::new(ByteBudget::new(cap));
    let failed = Arc::new(AtomicU64::new(0));
    let raced = Arc::new(AtomicU64::new(0));
    let failed_before = failures.failed_total();

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
        let failures = Arc::clone(failures);
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
                                    failures.record(
                                        SkipReason::Io,
                                        format_args!("skipping {}: {e}", path.display()),
                                    );
                                    failed.fetch_add(1, Ordering::Relaxed);
                                    continue;
                                }
                            }
                        } else {
                            path.to_string_lossy().into_owned()
                        };
                        // Reject an over-cap file here, before its payload is
                        // charged to the budget and buffered into a batch: a
                        // `CHECK` violation discovered at commit time is fatal
                        // to the whole scan and has lost the path by then
                        // (#644). Full-write policy only — `StructuralOnly`
                        // (revalidate) writes neither tags nor art, so failing
                        // a stored track for them would be inventing a failure.
                        if policy == WritePolicy::Full
                            && let Err(e) = check_storable(&abs_path, &probed)
                        {
                            failures.record(SkipReason::Oversize, format_args!("skipping {e}"));
                            failed.fetch_add(1, Ordering::Relaxed);
                            continue;
                        }
                        let weight = payload_weight(&probed);
                        budget.acquire(weight); // backpressure on in-flight art bytes
                        let fingerprint = match tier {
                            ChecksumTier::None => None,
                            ChecksumTier::Fingerprint | ChecksumTier::Full => {
                                Some(fingerprint_of(&probed))
                            }
                        };
                        let content_hash = match tier {
                            ChecksumTier::Full => {
                                match full_file_hash(std::path::Path::new(&abs_path)) {
                                    Ok(h) => Some(h),
                                    Err(e) => {
                                        log::warn!("content hash failed for {abs_path}: {e}");
                                        None
                                    }
                                }
                            }
                            _ => None,
                        };
                        let unit = Unit {
                            abs_path,
                            stamp,
                            probed,
                            weight,
                            fingerprint,
                            content_hash,
                        };
                        if tx.send(unit).is_err() {
                            budget.release(weight);
                            break;
                        }
                    }
                    Ok(ProbeOutcome::Failed(f)) => {
                        failures.record(f.reason, format_args!("{}", f.message));
                        failed.fetch_add(1, Ordering::Relaxed);
                    }
                    Err(e) => {
                        failures.record(
                            SkipReason::Io,
                            format_args!("skipping {}: {e}", path.display()),
                        );
                        failed.fetch_add(1, Ordering::Relaxed);
                    }
                    Ok(ProbeOutcome::Raced) => {
                        // Its own bucket, capped like the rest but left out of the
                        // failure breakdown: a race has exactly one cause and is
                        // already reported whole as `ScanStats::raced`.
                        failures.record(
                            SkipReason::Raced,
                            format_args!("skipping {}: changed during probe", path.display()),
                        );
                        raced.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
        }));
    }
    drop(tx); // close the channel once all clones (workers) finish

    // Writer: this thread. Batch by file count and accumulated art bytes.
    let mut scanned = 0u64;
    // Files the pipeline is finished with, committed *or* failed. Both progress
    // events report it, so the bar converges on `total` however many files fail
    // (#655): a bar stalled at 93% reads as "this was aborted", which is the
    // wrong story for a scan that completed and is about to report `failed N`.
    let mut finished = 0u64;
    // Failures already folded into `finished`. Workers tally on their own
    // threads; only the counts cross back here, because a `ProgressSink` may be
    // invoked from the writer and the walk but never from a probe worker.
    let mut failures_seen = 0u64;
    let mut batch: Vec<Unit> = Vec::new();
    let mut batch_bytes = 0u64;
    let flush = |batch: &mut Vec<Unit>,
                 batch_bytes: &mut u64,
                 scanned: &mut u64,
                 finished: &mut u64|
     -> Result<()> {
        if batch.is_empty() {
            return Ok(());
        }
        let mut bw = db.bulk_writer()?;
        // Budget weights are released only after commit, and ingest_unit consumes
        // the Probed — capture each unit's weight before the move (#68).
        let mut released = 0u64;
        // `Ingested` reports committed files, so buffer the paths and emit only
        // after `bw.commit()` succeeds — a failed commit aborts the scan without
        // having advanced the progress bar past unpersisted files.
        let mut committed: Vec<String> = Vec::new();
        for unit in batch.drain(..) {
            released += unit.weight;
            committed.push(unit.abs_path.clone());
            // A write failure here is still fatal (the store, not the file, is
            // the problem), but it must say which file it died on — issue #644
            // was reported as an unattributed `CHECK constraint failed`.
            let abs_path = unit.abs_path.clone();
            ingest_unit(&mut bw, unit, strictness, policy).map_err(|e| {
                log::error!("aborting scan while ingesting {abs_path}: {e}");
                e
            })?;
        }
        bw.commit()?;
        for abs_path in committed {
            *scanned += 1;
            *finished += 1;
            if let Some(p) = progress {
                p.emit(ScanProgress::Ingested {
                    done: *finished,
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

    // Fold any failures the workers have tallied since the last check into the
    // progress sequence. Called at the top of each writer iteration (so the bar
    // tracks failures as they happen) and once more after the final flush (so a
    // tail of failures with no commit behind them still lands).
    let catch_up = |finished: &mut u64, failures_seen: &mut u64| {
        let tallied = uncommitted_total(
            failed.load(Ordering::Relaxed),
            raced.load(Ordering::Relaxed),
        );
        // A bounded `for` rather than a `while` advancing its own cursor: the
        // loop cannot outrun the tally, and it holds no increment whose failure
        // to advance would spin forever.
        for _ in *failures_seen..tallied {
            *finished += 1;
            if let Some(p) = progress {
                p.emit(ScanProgress::Failed {
                    done: *finished,
                    total,
                });
            }
        }
        *failures_seen = tallied;
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
    // A fatal flush error leaves the loop through `fatal` rather than `?`: the
    // pipeline must be torn down (below) before it propagates.
    let mut fatal: Option<crate::error::CoreError> = None;
    loop {
        catch_up(&mut finished, &mut failures_seen);
        match rx.try_recv() {
            Ok(unit) => {
                batch_bytes += unit.weight;
                batch.push(unit);
                if (batch.len() >= BATCH_FILES || batch_bytes >= cap)
                    && let Err(e) = flush(&mut batch, &mut batch_bytes, &mut scanned, &mut finished)
                {
                    fatal = Some(e);
                    break;
                }
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => {
                if let Err(e) = flush(&mut batch, &mut batch_bytes, &mut scanned, &mut finished) {
                    fatal = Some(e);
                    break;
                }
                match rx.recv() {
                    Ok(unit) => {
                        batch_bytes += unit.weight;
                        batch.push(unit);
                        if (batch.len() >= BATCH_FILES || batch_bytes >= cap)
                            && let Err(e) =
                                flush(&mut batch, &mut batch_bytes, &mut scanned, &mut finished)
                        {
                            fatal = Some(e);
                            break;
                        }
                    }
                    Err(_) => break, // all workers finished; channel closed
                }
            }
            Err(std::sync::mpsc::TryRecvError::Disconnected) => break,
        }
    }
    let outcome = match fatal {
        Some(e) => Err(e),
        None => flush(&mut batch, &mut batch_bytes, &mut scanned, &mut finished),
    };
    if outcome.is_ok() {
        // Every worker has exited by now, so the tallies are final: this is the
        // catch-up that matters when the tail of the run is failures with no
        // commit behind them to drive the loop (the whole-target failure case,
        // where nothing was ever sent and the loop ran once).
        catch_up(&mut finished, &mut failures_seen);
    }
    if outcome.is_err() {
        // A DB-write failure aborts the whole scan, but the workers must still be
        // wound down before the error propagates: `scan_directory_with` /
        // `revalidate_with` are public API, and an embedder that catches the error
        // and carries on would otherwise leak a thread — plus its in-flight art
        // bytes — per worker parked in `budget.acquire`, waiting on a release the
        // failed batch will never make (#618). Dropping the receiver unblocks
        // `send`; closing the budget unblocks `acquire`; both make the workers exit.
        drop(rx);
        budget.close();
    }
    // On the success path every worker has already exited (the work queue drained
    // and `drop(tx)` closed the channel), so these joins return promptly.
    for w in workers {
        let _ = w.join();
    }
    outcome?;

    let stats = ScanStats {
        scanned,
        skipped: 0, // counted at walk time; filled in by scan_directory_with
        already_present: 0,
        failed: failed.load(Ordering::Relaxed),
        raced: raced.load(Ordering::Relaxed),
    };
    // Every `failed` increment must be paired with a `failures.record`, or the
    // breakdown would silently understate the number it exists to explain.
    // Checked as a delta: the tally may already carry the walk's errors, or
    // revalidate's pre-dispatch skip-pass failures.
    debug_assert_eq!(
        failures.failed_total() - failed_before,
        stats.failed,
        "every scan failure must be tallied by reason"
    );
    // The companion invariant for the progress bar: every dispatched file is
    // committed, failed, or raced, so the two progress events must together
    // account for the walked total. If this ever trips, the bar has silently
    // gone back to stalling short of 100% (#655).
    debug_assert_eq!(
        finished, total,
        "progress must reach the walked total (scanned {} + failed {} + raced {})",
        stats.scanned, stats.failed, stats.raced
    );
    Ok(stats)
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
        already_present: 0,
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
    let failures = Arc::new(FailureTally::default());
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
            &failures,
        )?;
    }
    db.apply_bulk_pragmas_self()?;

    // Main-thread pre-dispatch skip pass: load existing
    // (path -> stamp, id, format, has_fingerprint, has_content_hash) once,
    // stat each candidate, keep only changed files. Workers stay DB-free.
    let existing: HashMap<String, (crate::freshness::BackingStamp, i64, Format, bool, bool)> = db
        .list_tracks()?
        .into_iter()
        .map(|t| {
            (
                t.backing_path.clone(),
                (
                    crate::freshness::BackingStamp::from_track(&t),
                    t.id,
                    t.format,
                    t.fingerprint.is_some(),
                    t.content_hash.is_some(),
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
                failures.record(
                    SkipReason::Io,
                    format_args!("skipping {}: {e}", path.display()),
                );
                skip_failed += 1;
                continue;
            }
        };
        let key = if opts.follow_symlinks {
            match std::fs::canonicalize(&path) {
                Ok(abs) => abs.to_string_lossy().into_owned(),
                Err(e) => {
                    failures.record(
                        SkipReason::Io,
                        format_args!("skipping {}: {e}", path.display()),
                    );
                    skip_failed += 1;
                    continue;
                }
            }
        } else {
            path.to_string_lossy().into_owned()
        };
        if let Some((stamp, id, format, has_fingerprint, has_content_hash)) =
            existing.get(&key).copied()
        {
            let needs_backfill = format == Format::Flac && !have_structural.contains(&id);
            let needs_checksum = match opts.checksum {
                ChecksumTier::None => false,
                ChecksumTier::Fingerprint => !has_fingerprint,
                ChecksumTier::Full => !has_fingerprint || !has_content_hash,
            };
            if crate::freshness::BackingStamp::from_metadata(&meta) == stamp
                && !needs_backfill
                && !needs_checksum
            {
                unchanged += 1;
                continue;
            }
            changed.push(path);
        }
    }

    if let Some(p) = &opts.progress {
        p.emit(ScanProgress::Walked {
            total: changed.len() as u64,
        });
    }

    let mut pruned = 0u64;
    let scan = run_pipeline(db, changed, opts, WritePolicy::StructuralOnly, &failures)?;

    if opts.prune {
        let canon_root = root;
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
    }

    log_failure_summaries(&failures);
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
        feed(&mut h, pic.description.as_bytes());
        h.update(u64::from(pic.width).to_le_bytes());
        h.update(u64::from(pic.height).to_le_bytes());
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
