//! The `musefs` command-line interface: `scan` (ingest a backing directory into a
//! SQLite store) and `mount` (serve a read-only FUSE view of that store).

use std::path::{Component, Path, PathBuf};
use std::process::ExitCode;
use std::time::Instant;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use indicatif::{HumanBytes, HumanDuration};
use musefs_core::{MountConfig, Musefs};
use musefs_db::{Db, PendingMigration};

use crate::progress::ScanReporter;

mod logging;
mod progress;
mod prompt;
mod signal;

pub use crate::logging::install_logger;

/// Mount content mode (CLI surface for `musefs_core::Mode`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
#[non_exhaustive]
pub enum CliMode {
    /// Synthesize a fresh metadata region in front of the audio (default).
    Synthesis,
    /// Serve the original backing file bytes unchanged.
    StructureOnly,
}

impl From<CliMode> for musefs_core::Mode {
    fn from(m: CliMode) -> musefs_core::Mode {
        match m {
            CliMode::Synthesis => musefs_core::Mode::Synthesis,
            CliMode::StructureOnly => musefs_core::Mode::StructureOnly,
        }
    }
}

/// CLI surface for `musefs_core::ChecksumTier`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
#[non_exhaustive]
pub enum ChecksumMode {
    /// No checksums.
    None,
    /// Cheap fingerprint only (default).
    Fingerprint,
    /// Fingerprint plus full-file SHA-256.
    Full,
}

impl From<ChecksumMode> for musefs_core::ChecksumTier {
    fn from(m: ChecksumMode) -> musefs_core::ChecksumTier {
        match m {
            ChecksumMode::None => musefs_core::ChecksumTier::None,
            ChecksumMode::Fingerprint => musefs_core::ChecksumTier::Fingerprint,
            ChecksumMode::Full => musefs_core::ChecksumTier::Full,
        }
    }
}

/// CLI surface for `musefs_core::MatchStrictness`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
#[non_exhaustive]
pub enum MatchMode {
    /// Confirm with a full hash when the matched row has one; otherwise trust
    /// the fingerprint (default).
    Auto,
    /// Trust a fingerprint match; never read the whole file.
    Fast,
    /// Require a full-hash match: a row with no stored hash is not retargeted.
    Strict,
}

impl From<MatchMode> for musefs_core::MatchStrictness {
    fn from(m: MatchMode) -> musefs_core::MatchStrictness {
        match m {
            MatchMode::Auto => musefs_core::MatchStrictness::Auto,
            MatchMode::Fast => musefs_core::MatchStrictness::Fast,
            MatchMode::Strict => musefs_core::MatchStrictness::Strict,
        }
    }
}

#[derive(Parser, Debug)]
#[command(
    name = "musefs",
    version,
    propagate_version = true,
    about = "Read-only re-tagging FUSE view of a music library"
)]
#[non_exhaustive]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
    /// Increase log verbosity: `-v` = info, `-vv` = debug, `-vvv` = trace
    /// (default: warn). An explicit `RUST_LOG` takes precedence over this.
    #[arg(short, long, action = clap::ArgAction::Count, global = true)]
    pub verbose: u8,
}

/// Flags for `musefs mount`, grouped so the mount plumbing passes one value
/// instead of ten ordering-fragile positional parameters.
#[derive(clap::Args, Debug)]
#[allow(clippy::struct_excessive_bools)] // independent CLI toggles, not a state machine
#[non_exhaustive]
pub struct MountArgs {
    /// Empty directory to mount at. Not required with `--dry-run`, which only
    /// previews the template and never touches a target (#555).
    #[arg(env = "MUSEFS_MOUNTPOINT", required_unless_present = "dry_run")]
    pub mountpoint: Option<PathBuf>,
    /// Path to the SQLite database (must already exist; unlike `scan`, mount
    /// never creates it).
    #[arg(long, env = "MUSEFS_DB")]
    pub db: PathBuf,
    /// Path template, e.g. "$albumartist/$album/$title". Supports ${a|b}
    /// fallback chains, [...] conditional sections ($[/$] for literal
    /// brackets), and $!{field} path fields that keep '/' as separators.
    #[arg(
        long,
        env = "MUSEFS_TEMPLATE",
        default_value = "$albumartist/$album/$title"
    )]
    pub template: String,
    /// Fallback value substituted for any missing template field.
    #[arg(long, env = "MUSEFS_DEFAULT_FALLBACK", default_value = "Unknown")]
    pub default_fallback: String,
    /// Per-field fallback `FIELD=VALUE`, overriding `--default-fallback` for
    /// just that field when it is missing. Repeatable, e.g. `--fallback
    /// albumartist="Unknown Artist" --fallback genre=Misc`.
    #[arg(long = "fallback", value_name = "FIELD=VALUE", value_parser = parse_fallback)]
    pub fallbacks: Vec<(String, String)>,
    /// Drop tracks whose path is missing a top-level template field instead of
    /// substituting `--default-fallback` (per-field `--fallback` chains and
    /// `[...]` sections still apply). Useful when an external writer only tags a
    /// subset of tracks, e.g. skipping tracks beets left without a `beets_path`.
    #[arg(long, env = "MUSEFS_SKIP_ON_MISSING", value_parser = clap::builder::BoolishValueParser::new())]
    pub skip_on_missing: bool,
    /// How file contents are served.
    #[arg(long, value_enum, env = "MUSEFS_MODE", default_value_t = CliMode::Synthesis)]
    pub mode: CliMode,
    /// Debounce window (ms) for picking up external DB edits.
    #[arg(long, env = "MUSEFS_POLL_INTERVAL_MS", default_value_t = 1000)]
    pub poll_interval_ms: u64,
    /// Entry/attr cache TTL (ms) the kernel may trust before re-validating.
    /// Higher cuts lookup/getattr traffic but slows visibility of DB edits.
    #[arg(long, env = "MUSEFS_ATTR_TTL_MS", default_value_t = 1000)]
    pub attr_ttl_ms: u64,
    /// Kernel read-ahead window (KiB). Larger hides HDD/NFS latency while
    /// streaming; clamped to the kernel maximum at mount.
    #[arg(long, env = "MUSEFS_MAX_READAHEAD_KIB", default_value_t = 512)]
    pub max_readahead_kib: u32,
    /// Global read-ahead RAM budget (MiB) shared across all active streams. 0 disables.
    #[arg(long, env = "MUSEFS_READ_AHEAD_BUDGET_MIB", default_value_t = 64)]
    pub read_ahead_budget_mib: u32,
    /// Enable Phase-2 background prefetch threads (advanced). Off by default;
    /// worth enabling on high-latency network backing, where it adds ~30% to
    /// single-stream throughput over a 200 ms-RTT NFS mount. On local or
    /// low-latency storage it instead reads the stream a second time
    /// speculatively for no gain. See the benchmarks docs: https://sohex.github.io/musefs/benchmarks.html
    // This doc comment is also the `--help` text, where `<…>` around the URL
    // would print verbatim, so the rustdoc lint is silenced rather than obeyed.
    #[allow(rustdoc::bare_urls)]
    #[arg(long, env = "MUSEFS_READ_AHEAD_PREFETCH", value_parser = clap::builder::BoolishValueParser::new())]
    pub read_ahead_prefetch: bool,
    /// Max outstanding background (readahead/async) requests the kernel queues.
    #[arg(long, env = "MUSEFS_MAX_BACKGROUND", default_value_t = 64)]
    pub max_background: u16,
    /// Worker threads for offloaded FUSE ops (reads, metadata synthesis).
    /// 0 = auto: 2× the CPU count, oversized because the work is I/O-bound.
    /// Each worker lazily opens its own read-only SQLite connection, so
    /// steady-state memory also scales with this — lower it on
    /// memory-constrained or many-core hosts (see the tuning guide).
    #[arg(long, env = "MUSEFS_WORKERS", default_value_t = 0)]
    pub workers: usize,
    /// Keep the kernel page cache across opens. On by default: it is the one
    /// measured storage win (~3× faster repeat-open on HDD/NFS, #432). External
    /// re-tags auto-invalidate the affected inodes on refresh, so cached bytes
    /// are dropped when content changes. Disable with `--keep-cache false`.
    #[arg(long, env = "MUSEFS_KEEP_CACHE", default_value_t = true, num_args = 0..=1, default_missing_value = "true", value_parser = clap::builder::BoolishValueParser::new())]
    pub keep_cache: bool,
    /// Skip the backing re-stat that `getattr` does on a metadata-cache hit,
    /// serving the cached size/mtime instead. Off by default. Worth setting only
    /// on high-latency backing (NFS, SMB, a spun-down array), where that stat is
    /// a round trip paid once per track on every traversal after the first.
    /// `open` and reads keep validating, so no stale bytes are ever served; what
    /// goes stale is the size/mtime a `stat` reports for a backing file changed
    /// without the store being updated.
    #[arg(long, env = "MUSEFS_TRUST_BACKING_MTIME", value_parser = clap::builder::BoolishValueParser::new())]
    pub trust_backing_mtime: bool,
    /// Compare filenames case-insensitively: case-variant directories merge and
    /// case-variant files are disambiguated. Defaults to true on macOS (whose
    /// volumes are usually case-insensitive), false on Linux/FreeBSD. Override
    /// with `--case-insensitive false` (e.g. a case-sensitive APFS volume).
    #[arg(long, env = "MUSEFS_CASE_INSENSITIVE", default_value_t = cfg!(target_os = "macos"), action = clap::ArgAction::Set, value_parser = clap::builder::BoolishValueParser::new())]
    pub case_insensitive: bool,
    /// Owning user for every entry: a username or numeric uid. Defaults to the
    /// launching process's uid.
    #[arg(long, env = "MUSEFS_OWNER", value_name = "NAME|UID", value_parser = parse_owner)]
    pub owner: Option<u32>,
    /// Owning group for every entry: a group name or numeric gid. Defaults to
    /// the launching process's gid.
    #[arg(long, env = "MUSEFS_GROUP", value_name = "NAME|GID", value_parser = parse_group)]
    pub group: Option<u32>,
    /// Permission bits for regular files, octal (e.g. 444). Defaults to 444.
    /// The mount is read-only, so write bits are advertised but inert.
    #[arg(long, env = "MUSEFS_FILE_MODE", value_name = "OCTAL", value_parser = parse_octal_mode)]
    pub file_mode: Option<u16>,
    /// Permission bits for directories, octal (e.g. 555). Defaults to 555.
    #[arg(long, env = "MUSEFS_DIR_MODE", value_name = "OCTAL", value_parser = parse_octal_mode)]
    pub dir_mode: Option<u16>,
    /// Mount with `allow_other` + `default_permissions` so accounts other than
    /// the mounting user can reach the mount and the presented owner/mode bits
    /// are kernel-enforced. Implied by `--owner`/`--group`. Non-root mounts also
    /// require `user_allow_other` in `/etc/fuse.conf`.
    #[arg(long, env = "MUSEFS_ALLOW_OTHER", value_parser = clap::builder::BoolishValueParser::new())]
    pub allow_other: bool,
    /// Expose a `/proc`-style `.musefs-metrics/metrics` file at the mount root
    /// for live observability (handles, read/dir-handle queues, caches, tree,
    /// allocator). Off by default. Distinct from the compile-time `metrics`
    /// cargo feature, which adds the syscall counters.
    #[arg(long, env = "MUSEFS_EXPOSE_METRICS", value_parser = clap::builder::BoolishValueParser::new())]
    pub expose_metrics: bool,
    /// Validate the template and config and print a sample of the paths the
    /// mount would expose, then exit without mounting. Use this to check a
    /// `--template` before committing to a mount.
    #[arg(long, default_value_t = false)]
    pub dry_run: bool,
}

#[derive(Subcommand, Debug)]
#[non_exhaustive]
pub enum Command {
    /// Walk backing files or directories, ingesting supported audio
    /// (FLAC, MP3, M4A/M4B, Ogg, WAV) into the SQLite store.
    Scan {
        /// One or more files or directories to scan (directories recurse).
        #[arg(required = true, num_args = 1..)]
        targets: Vec<PathBuf>,
        /// Path to the SQLite database (created if absent).
        #[arg(long, env = "MUSEFS_DB")]
        db: PathBuf,
        /// Re-ingest files already present in the DB, overwriting curated tags
        /// and art with the file's embedded metadata.
        #[arg(long, env = "MUSEFS_FORCE", value_parser = clap::builder::BoolishValueParser::new())]
        force: bool,
        /// Probe worker threads (0 = available parallelism). 1 = sequential.
        #[arg(long, env = "MUSEFS_JOBS", default_value_t = 0)]
        jobs: usize,
        /// Follow symlinks while walking directories. Off by default: symlinked
        /// files and directories are logged and skipped.
        #[arg(long, env = "MUSEFS_FOLLOW_SYMLINKS", value_parser = clap::builder::BoolishValueParser::new())]
        follow_symlinks: bool,
        /// Suppress the per-target summary on stdout (failures still surface via
        /// the `log` facade on stderr; raise detail with `RUST_LOG=info`).
        #[arg(long, short, env = "MUSEFS_QUIET", value_parser = clap::builder::BoolishValueParser::new())]
        quiet: bool,
        /// Which content checksums to compute and store (none|fingerprint|full).
        #[arg(long, value_enum, env = "MUSEFS_CHECKSUM", default_value_t = ChecksumMode::Fingerprint)]
        checksum: ChecksumMode,
        /// How a moved file's fingerprint match is confirmed before its row is
        /// retargeted (auto|fast|strict).
        #[arg(long = "match", value_enum, env = "MUSEFS_MATCH", default_value_t = MatchMode::Auto)]
        match_mode: MatchMode,
    },
    /// Refresh tracks already in the store: re-probe files whose backing bytes
    /// changed while preserving curated tags and art. Files not yet in the
    /// store are ignored. Never deletes anything unless `--prune`.
    Revalidate {
        /// One or more files or directories to revalidate (directories recurse).
        #[arg(required = true, num_args = 1..)]
        targets: Vec<PathBuf>,
        /// Path to the SQLite database.
        #[arg(long, env = "MUSEFS_DB")]
        db: PathBuf,
        /// Delete tracks whose backing file is gone, or whose file this build
        /// refuses as unsupported (a chained Ogg an older musefs stored), with
        /// their tags and art links; then GC orphaned art.
        #[arg(long, env = "MUSEFS_PRUNE", value_parser = clap::builder::BoolishValueParser::new())]
        prune: bool,
        /// Probe worker threads (0 = available parallelism). 1 = sequential.
        #[arg(long, env = "MUSEFS_JOBS", default_value_t = 0)]
        jobs: usize,
        /// Follow symlinks while walking directories. Off by default: symlinked
        /// files and directories are logged and skipped.
        #[arg(long, env = "MUSEFS_FOLLOW_SYMLINKS", value_parser = clap::builder::BoolishValueParser::new())]
        follow_symlinks: bool,
        /// Suppress the per-target summary on stdout (failures still surface via
        /// the `log` facade on stderr; raise detail with `RUST_LOG=info`).
        #[arg(long, short, env = "MUSEFS_QUIET", value_parser = clap::builder::BoolishValueParser::new())]
        quiet: bool,
        /// Which content checksums to compute and store (none|fingerprint|full).
        #[arg(long, value_enum, env = "MUSEFS_CHECKSUM", default_value_t = ChecksumMode::Fingerprint)]
        checksum: ChecksumMode,
    },
    /// Mount a read-only FUSE view of the store.
    Mount(MountArgs),
    /// Compact the SQLite store, reclaiming free pages left by deletions
    /// (prunes, orphan-art GC, the schema migration). Run while unmounted. A
    /// store needing a gated schema upgrade is refused: run `musefs migrate`
    /// first.
    Vacuum {
        /// Path to the SQLite database.
        #[arg(long, env = "MUSEFS_DB")]
        db: PathBuf,
    },
    /// Upgrade the store's schema to the version this build needs.
    ///
    /// Some schema changes are too invasive to apply as a side effect of
    /// opening the store: they rewrite data, transiently need the store's size
    /// again in free disk, and end compatibility with older musefs builds.
    /// Those are refused by `mount`, `scan`, `revalidate` and `vacuum`, and applied here,
    /// after reporting what they will do. Run it while unmounted. A snapshot is
    /// taken first unless `--no-snapshot`, so the upgrade stays reversible.
    Migrate(MigrateArgs),
}

#[derive(clap::Args, Debug)]
#[non_exhaustive]
pub struct MigrateArgs {
    /// Path to the SQLite database.
    #[arg(long, env = "MUSEFS_DB")]
    pub db: PathBuf,
    /// Upgrade without asking. Required when not running on a terminal.
    #[arg(long, short = 'y', env = "MUSEFS_YES", value_parser = clap::builder::BoolishValueParser::new())]
    pub yes: bool,
    /// Where to write the pre-upgrade snapshot. Default: the store's own path
    /// with `.v<version>.bak` appended, alongside it.
    #[arg(long, value_name = "PATH", conflicts_with = "no_snapshot")]
    pub snapshot: Option<PathBuf>,
    /// Delete rows the upgraded schema refuses, instead of stopping to report
    /// them. Nothing is deleted without this.
    ///
    /// Refuses alongside `--no-snapshot`: the rows are deleted for good, and the
    /// snapshot is the only copy they survive in.
    #[arg(long, conflicts_with = "no_snapshot")]
    pub repair: bool,
    /// Upgrade without taking a snapshot first. The upgrade is then not
    /// reversible.
    #[arg(long)]
    pub no_snapshot: bool,
    /// Compact the store afterwards (an upgrade grows it). Omit to be asked.
    #[arg(long, num_args = 0..=1, default_missing_value = "true", value_parser = clap::builder::BoolishValueParser::new())]
    pub vacuum: Option<bool>,
    /// Revalidate the library afterwards, recomputing what the upgrade retired.
    /// Omit to be asked.
    #[arg(long, num_args = 0..=1, default_missing_value = "true", value_parser = clap::builder::BoolishValueParser::new())]
    pub revalidate: Option<bool>,
    /// Probe worker threads for that revalidate (0 = available parallelism).
    #[arg(long, env = "MUSEFS_JOBS", default_value_t = 0)]
    pub jobs: usize,
}

/// Open (creating/migrating) the DB at `db_path` once, then scan each target in
/// `targets` (a file or a directory; directories recurse). With `quiet`,
/// suppress the per-target summary on stdout. Fails fast:
/// the first failing target aborts the batch; targets already scanned stay
/// committed (ingest is an idempotent upsert).
///
/// Returns the total per-file `failed` count across all targets. Per-file
/// failures (an unparseable/uningestible entry) do not abort the batch — only a
/// hard `Err` does — so the caller inspects the returned count to signal partial
/// or total ingest failure via the process exit code (#554).
#[allow(clippy::too_many_arguments)]
pub fn run_scan(
    db_path: &Path,
    targets: &[PathBuf],
    force: bool,
    jobs: usize,
    follow_symlinks: bool,
    quiet: bool,
    checksum: ChecksumMode,
    match_mode: MatchMode,
) -> Result<u64> {
    let db =
        Db::open(db_path).with_context(|| format!("opening database at {}", db_path.display()))?;
    let reporter = ScanReporter::new(quiet);
    let mut opts = musefs_core::ScanOptions::default();
    opts.jobs = jobs;
    opts.follow_symlinks = follow_symlinks;
    opts.progress = reporter.sink();
    opts.checksum = checksum.into();
    opts.strictness = match_mode.into();
    opts.force = force;
    let mut total_failed = 0u64;
    for target in targets {
        reporter.start_target();
        let start = Instant::now();
        let stats = musefs_core::scan_directory_with(&db, target, &opts)
            .with_context(|| format!("scanning {}", target.display()))?;
        total_failed += stats.failed;
        if !quiet {
            // Suspended like a log record: the bar is still ticking between
            // targets, and an un-lifted frame would swallow the summary line.
            progress::suspend(|| {
                println!(
                    "scanned {}: {} file(s), {} already present, skipped {}, failed {} in {}",
                    target.display(),
                    stats.scanned,
                    stats.already_present,
                    stats.skipped,
                    stats.failed,
                    HumanDuration(start.elapsed()),
                );
            });
        }
    }
    reporter.finish();
    warn_if_revalidate_owed(&db, db_path)?;
    Ok(total_failed)
}

/// The warning `mount`, `scan` and `revalidate` print while tracks still await
/// the revalidate an upgrade leaves owed, or `None` when none do (#705).
/// `migrate` reports the number once, but that line scrolls away and the gap
/// does not: until a row is re-probed, a moved file is not recognised and a
/// file replaced in place is caught on fewer fields.
fn revalidate_owed_warning(db: &Db, db_path: &Path) -> Result<Option<String>> {
    let owed = db.count_tracks_awaiting_revalidate()?;
    Ok((owed > 0).then(|| {
        format!(
            "{owed} track(s) have not been re-probed since the store was upgraded, so a moved \
             file is not recognised and a file replaced in place is caught on fewer fields; run \
             `musefs revalidate <library> --db {}` (with --follow-symlinks if the library is \
             reached through symlinks). A file it cannot re-probe stays counted until \
             `revalidate --prune` removes it",
            db_path.display()
        )
    }))
}

fn warn_if_revalidate_owed(db: &Db, db_path: &Path) -> Result<()> {
    if let Some(w) = revalidate_owed_warning(db, db_path)? {
        eprintln!("warning: {w}");
    }
    Ok(())
}

/// Open the DB once and revalidate each target, preserving curated metadata.
#[allow(clippy::too_many_arguments)]
pub fn run_revalidate(
    db_path: &Path,
    targets: &[PathBuf],
    prune: bool,
    jobs: usize,
    follow_symlinks: bool,
    quiet: bool,
    checksum: ChecksumMode,
) -> Result<u64> {
    let db =
        Db::open(db_path).with_context(|| format!("opening database at {}", db_path.display()))?;
    let reporter = ScanReporter::new(quiet);
    let mut opts = musefs_core::ScanOptions::default();
    opts.jobs = jobs;
    opts.follow_symlinks = follow_symlinks;
    opts.progress = reporter.sink();
    opts.checksum = checksum.into();
    opts.prune = prune;
    let mut total_failed = 0u64;
    for target in targets {
        reporter.start_target();
        let start = Instant::now();
        let stats = musefs_core::revalidate_with(&db, target, &opts)
            .with_context(|| format!("revalidating {}", target.display()))?;
        total_failed += stats.failed;
        if !quiet {
            progress::suspend(|| {
                println!(
                    "revalidated {}: {} updated, {} unchanged, {} pruned, {} failed in {}",
                    target.display(),
                    stats.updated,
                    stats.unchanged,
                    stats.pruned,
                    stats.failed,
                    HumanDuration(start.elapsed()),
                );
            });
        }
    }
    reporter.finish();
    warn_if_revalidate_owed(&db, db_path)?;
    Ok(total_failed)
}

/// Environment variables for `scan` flags that 2.0.0 removed, each with what to
/// use instead. clap rejects a removed flag, but it never reads a variable no
/// flag declares, so without this a unit file still setting one would carry on
/// doing something different from what it asks for, and say nothing.
const RETIRED_SCAN_ENV: &[(&str, &str)] = &[
    (
        "MUSEFS_REVALIDATE",
        "run the `revalidate` subcommand instead",
    ),
    ("MUSEFS_FAST", "set `MUSEFS_MATCH=fast` instead"),
    ("MUSEFS_STRICT", "set `MUSEFS_MATCH=strict` instead"),
];

/// Refuse to run while any of `retired` is set. An empty value counts as unset,
/// which is how clap treats a declared variable too.
fn refuse_retired_env(retired: &[(&str, &str)]) -> Result<()> {
    for (var, instead) in retired {
        if std::env::var_os(var).is_some_and(|v| !v.is_empty()) {
            anyhow::bail!("{var} was removed in musefs 2.0.0; {instead}");
        }
    }
    Ok(())
}

/// Split a `--fallback FIELD=VALUE` argument. The value may contain '=' (only
/// the first one separates); the field name must be non-empty.
fn parse_fallback(s: &str) -> Result<(String, String), String> {
    let (field, value) = s
        .split_once('=')
        .ok_or_else(|| format!("expected FIELD=VALUE, got `{s}`"))?;
    if field.is_empty() {
        return Err(format!("empty field name in `{s}`"));
    }
    Ok((field.to_string(), value.to_string()))
}

/// Resolve a `--owner`/`--group` value: an all-numeric string is taken as a raw
/// id (never a name, matching `chown`); otherwise `lookup` resolves it by name,
/// with `noun` naming the entity in the not-found error.
fn parse_id(s: &str, lookup: impl FnOnce(&str) -> Option<u32>, noun: &str) -> Result<u32, String> {
    if let Ok(id) = s.parse::<u32>() {
        return Ok(id);
    }
    lookup(s).ok_or_else(|| format!("no such {noun}: {s}"))
}

/// Resolve `--owner`: a numeric uid is used directly; anything else is looked
/// up as a username. An all-numeric string is always treated as an id (never a
/// name), matching `chown`.
fn parse_owner(s: &str) -> Result<u32, String> {
    parse_id(s, |n| uzers::get_user_by_name(n).map(|u| u.uid()), "user")
}

/// Resolve `--group`: a numeric gid is used directly; anything else is looked
/// up as a group name.
fn parse_group(s: &str) -> Result<u32, String> {
    parse_id(s, |n| uzers::get_group_by_name(n).map(|g| g.gid()), "group")
}

/// Parse a bare octal permission word (e.g. `644`, `0755`) — NOT decimal, and
/// without an `0o` prefix. Range-checked to `0o7777`.
fn parse_octal_mode(s: &str) -> Result<u16, String> {
    let mode = u16::from_str_radix(s, 8).map_err(|_| format!("invalid octal mode: {s}"))?;
    if mode > 0o7777 {
        return Err(format!("octal mode out of range (max 7777): {s}"));
    }
    Ok(mode)
}

/// Warning text when a read-only mount is given a mode with write bits set;
/// the bits are applied as requested, this only informs.
fn write_bit_warning(flag: &str, mode: u16) -> Option<String> {
    (mode & 0o222 != 0).then(|| {
        format!(
            "--{flag} {mode:o} sets write bits, but the mount is read-only; writes will fail with EROFS"
        )
    })
}

/// Effective `allow_other`: the explicit flag, or implied by a presented
/// owner/group (the cross-user use case is unreachable without it). Auto-enable
/// wins over an explicit `--allow-other false` (only reachable via the env var).
fn effective_allow_other(flag: bool, owner: Option<u32>, group: Option<u32>) -> bool {
    flag || owner.is_some() || group.is_some()
}

/// Parse mount CLI flags into `MountConfig` and `FuseConfig`. Pure function —
/// no DB access, no mounting. Exported for unit testing.
pub fn parse_mount_config(args: &MountArgs) -> (MountConfig, musefs_fuse::FuseConfig) {
    let mut config = MountConfig::default();
    config.template.clone_from(&args.template);
    // Field names are case-insensitive everywhere else (the template parser
    // and `tags_to_fields` ASCII-lowercase them), so a fallback keyed under
    // any uppercase letter would never match at render time (#504). Normalize
    // the key the same way; later duplicates win, matching `collect`'s prior
    // last-write semantics.
    config.fallbacks = args
        .fallbacks
        .iter()
        .map(|(field, value)| (field.to_ascii_lowercase(), value.clone()))
        .collect();
    config.default_fallback.clone_from(&args.default_fallback);
    config.mode = args.mode.into();
    config.poll_interval = std::time::Duration::from_millis(args.poll_interval_ms);
    config.case_insensitive = args.case_insensitive;
    config.read_ahead_budget = u64::from(args.read_ahead_budget_mib).saturating_mul(1024 * 1024);
    config.read_ahead_prefetch = args.read_ahead_prefetch;
    config.skip_on_missing = args.skip_on_missing;
    config.trust_backing_mtime = args.trust_backing_mtime;
    // Starts from the defaults, so an unset owner, group or mode keeps its default.
    let mut fuse_config = musefs_fuse::FuseConfig::default();
    fuse_config.ttl = std::time::Duration::from_millis(args.attr_ttl_ms);
    fuse_config.max_readahead = args.max_readahead_kib.saturating_mul(1024);
    fuse_config.max_background = args.max_background;
    fuse_config.keep_cache = args.keep_cache;
    fuse_config.uid = args.owner.unwrap_or(fuse_config.uid);
    fuse_config.gid = args.group.unwrap_or(fuse_config.gid);
    fuse_config.file_mode = args.file_mode.unwrap_or(fuse_config.file_mode);
    fuse_config.dir_mode = args.dir_mode.unwrap_or(fuse_config.dir_mode);
    fuse_config.allow_other = effective_allow_other(args.allow_other, args.owner, args.group);
    fuse_config.expose_metrics = args.expose_metrics;
    fuse_config.workers = args.workers;
    (config, fuse_config)
}

/// Actionable hint appended to a permission-denied mount failure. Covers the
/// common AppArmor case (Ubuntu 24.04+ / libfuse >= 3.17 restrict unprivileged
/// FUSE mounts to whitelisted prefixes); mirrors `ALLOW_OTHER_HELP`'s role for
/// the `user_allow_other` denial. See `docs/src/guide/mounting.md`.
const MOUNT_DENIED_HELP: &str = "the mount was denied; on Ubuntu 24.04+ / libfuse >= 3.17 the fusermount3 \
AppArmor profile only permits unprivileged FUSE mounts under whitelisted prefixes ($HOME, /mnt, /media, /tmp, ...). \
Mount under a permitted prefix, or whitelist yours in /etc/apparmor.d/local/fusermount3 (check the kernel audit \
log for an apparmor=\"DENIED\" ... profile=\"fusermount3\" line). See the mounting guide for details.";

const MOUNT_NO_FUSE_HELP: &str = "the FUSE userspace appears to be missing; mounting needs the `fuse3` package \
(fusermount3 on PATH), the `fuse` kernel module loaded, and /dev/fuse present. Install it (e.g. `apt install fuse3` \
/ `apk add fuse3` / `dnf install fuse3`) and ensure /dev/fuse exists. See the installation guide for runtime requirements.";

/// True if `mountpoint` is a directory containing at least one entry. A read
/// failure is treated as "empty" — the warning is advisory, and the mount will
/// surface any real access error itself.
fn mountpoint_is_nonempty(mountpoint: &std::path::Path) -> bool {
    std::fs::read_dir(mountpoint).is_ok_and(|mut entries| entries.next().is_some())
}

/// Build a `Musefs` from the DB at `args.db` and mount it (blocking) at
/// `args.mountpoint`. Unlike `scan`, mount never creates the store: a missing
/// database path is a configuration error (a typo would otherwise silently
/// mount an empty view), so it is rejected before any FUSE setup.
pub fn run_mount(args: &MountArgs) -> Result<()> {
    if !args.db.exists() {
        anyhow::bail!("database does not exist: {}", args.db.display());
    }
    // `--dry-run` previews a template and never mounts, so it needs no
    // mountpoint (#555); clap guarantees one for every other invocation.
    if !args.dry_run {
        let mountpoint = args
            .mountpoint
            .as_deref()
            .expect("clap requires a mountpoint unless --dry-run");
        if !mountpoint.is_dir() {
            if mountpoint.exists() {
                anyhow::bail!("mountpoint is not a directory: {}", mountpoint.display());
            }
            anyhow::bail!(
                "mountpoint does not exist (create it first): {}",
                mountpoint.display()
            );
        }
        // The mountpoint help says "Empty directory", but FUSE happily mounts over a
        // populated one and shadows its contents for the mount's lifetime. Warn so a
        // typo (or reusing a real music folder) doesn't silently hide files (#508).
        if mountpoint_is_nonempty(mountpoint) {
            eprintln!(
                "warning: mountpoint {} is not empty; its existing contents will be \
                 hidden behind the virtual tree until you unmount",
                mountpoint.display()
            );
        }
    }
    let db =
        Db::open(&args.db).with_context(|| format!("opening database at {}", args.db.display()))?;
    warn_if_revalidate_owed(&db, &args.db)?;
    let (config, fuse_config) = parse_mount_config(args);
    let template = config.template.clone();
    for (flag, mode) in [("file-mode", args.file_mode), ("dir-mode", args.dir_mode)] {
        if let Some(w) = mode.and_then(|m| write_bit_warning(flag, m)) {
            eprintln!("warning: {w}");
        }
    }
    let core = Musefs::open(db, config).context("building the virtual filesystem")?;
    if args.dry_run {
        return print_dry_run(&core);
    }
    let mountpoint = args
        .mountpoint
        .as_deref()
        .expect("clap requires a mountpoint unless --dry-run");
    signal::install_unmount_on_signal(mountpoint.to_path_buf())
        .context("installing the stop-signal unmount handler")?;
    // The "is it serving the right library?" context, logged before the blocking
    // mount call; `mount_with` emits the success line with the file/dir counts once
    // the session is actually serving (#522).
    log::info!(
        "mounting database {} at {} (template {:?})",
        args.db.display(),
        mountpoint.display(),
        template,
    );
    musefs_fuse::mount_with(core, mountpoint, "musefs", fuse_config).map_err(|e| {
        // A bare EACCES from fusermount3 (e.g. an AppArmor-denied prefix) is
        // otherwise opaque. Append actionable guidance — but not when the
        // allow_other preflight already produced its own self-contained message
        // (it names /etc/fuse.conf), to avoid stacking two different hints (#509).
        let denied_hint = e.kind() == std::io::ErrorKind::PermissionDenied
            && !e.to_string().contains("/etc/fuse.conf");
        // A missing FUSE userspace (no fusermount3 on PATH, kernel module not
        // loaded, or /dev/fuse absent) surfaces as NotFound with an opaque
        // message — the most common first-run failure on a fresh host/container.
        let no_fuse_hint = !denied_hint && e.kind() == std::io::ErrorKind::NotFound;
        let err = anyhow::Error::new(e).context(format!("mounting at {}", mountpoint.display()));
        if denied_hint {
            err.context(MOUNT_DENIED_HELP)
        } else if no_fuse_hint {
            err.context(MOUNT_NO_FUSE_HELP)
        } else {
            err
        }
    })?;
    Ok(())
}

/// Total on-disk footprint of the store: the main `.db` plus its `-wal`/`-shm`
/// sidecars (a missing sidecar counts as 0). Summing the WAL/SHM keeps the
/// reclaimed figure honest — a TRUNCATE checkpoint reclaims WAL bytes too.
fn store_footprint(db: &Path) -> u64 {
    // `.map_or`, not `.map(..).unwrap_or(..)`: the latter trips
    // `clippy::map_unwrap_or` (pedantic), which the `-D warnings` gate rejects.
    let main = std::fs::metadata(db).map_or(0, |m| m.len());
    let side: u64 = ["-wal", "-shm"]
        .iter()
        .map(|suffix| {
            let mut p = db.as_os_str().to_os_string();
            p.push(suffix);
            std::fs::metadata(p).map_or(0, |m| m.len())
        })
        .sum();
    main + side
}

/// One-line summary for a completed vacuum. `after >= before` (VACUUM can grow
/// an already-compact file slightly) reports `(already compact)` rather than a
/// zero or negative delta.
fn vacuum_summary(path: &Path, before: u64, after: u64) -> String {
    let reclaimed = before.saturating_sub(after);
    if reclaimed == 0 {
        format!(
            "vacuumed {}: {} (already compact)",
            path.display(),
            HumanBytes(after)
        )
    } else {
        format!(
            "vacuumed {}: {} → {} (reclaimed {})",
            path.display(),
            HumanBytes(before),
            HumanBytes(after),
            HumanBytes(reclaimed)
        )
    }
}

/// Compact the SQLite store at `db`. A store anything else has open — a mount,
/// even one idle between reads, or a scan — is refused with
/// `DbError::StoreInUse`'s actionable message before anything is rewritten.
pub fn run_vacuum(db: &Path) -> Result<()> {
    if !db.exists() {
        anyhow::bail!("database not found: {} (nothing to vacuum)", db.display());
    }
    let before = store_footprint(db);
    let store = Db::open(db).with_context(|| format!("opening store {}", db.display()))?;
    store.vacuum()?;
    let after = store_footprint(db);
    println!("{}", vacuum_summary(db, before, after));
    Ok(())
}

/// The peak free space an upgrade of a `footprint`-byte store needs on one
/// filesystem, as `copies` whole copies of it.
///
/// SQLite stages a rewritten page in the write-ahead log before committing it,
/// so a migration that touches every row transiently has the store on disk
/// twice; a snapshot alongside it is another whole copy. Both are estimates
/// from the store's current size, which is the only number available before the
/// work is done — deliberately not padded, since the point is to refuse a run
/// that would fail part-way through rather than to reserve headroom.
fn space_needed(footprint: u64, copies: u64) -> u64 {
    footprint.saturating_mul(copies)
}

/// Free space on the filesystem holding `path`'s directory. `path` itself need
/// not exist; its parent must.
fn free_space_for(path: &Path) -> Result<u64> {
    let dir = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    fs4::available_space(dir).with_context(|| format!("checking free space on {}", dir.display()))
}

/// Whether `a` and `b` would be written to the same filesystem, judged by the
/// device of the directories they sit in. Where either directory cannot be
/// stat'd, or on a platform without device ids, it falls back to the two being
/// the same directory.
fn same_filesystem(a: &Path, b: &Path) -> bool {
    let dir = |p: &Path| {
        p.parent()
            .filter(|d| !d.as_os_str().is_empty())
            .unwrap_or(Path::new("."))
            .to_path_buf()
    };
    let (dir_a, dir_b) = (dir(a), dir(b));
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if let (Ok(meta_a), Ok(meta_b)) = (std::fs::metadata(&dir_a), std::fs::metadata(&dir_b)) {
            return meta_a.dev() == meta_b.dev();
        }
    }
    dir_a == dir_b
}

/// Where a snapshot goes when the user did not say: the store's own path with
/// the version it is being taken from appended, so two upgrades of one store
/// never collide and the file says what it is.
fn default_snapshot_path(db: &Path, from_version: i64) -> PathBuf {
    let mut p = db.as_os_str().to_os_string();
    p.push(format!(".v{from_version}.bak"));
    PathBuf::from(p)
}

/// The deepest directory containing every stored backing file — the one target
/// a `revalidate` has to walk to reach the whole library.
///
/// `None` when there is no such place worth walking: an empty store, or paths
/// that share nothing above the filesystem root. Revalidating from `/` is never
/// what anyone meant, so the caller prints the command instead of offering to
/// run it.
fn common_library_root(paths: &[PathBuf]) -> Option<PathBuf> {
    let mut dirs = paths.iter().filter_map(|p| p.parent());
    let mut common: Vec<Component<'_>> = dirs.next()?.components().collect();
    for dir in dirs {
        let shared = common
            .iter()
            .zip(dir.components())
            .take_while(|(a, b)| **a == *b)
            .count();
        common.truncate(shared);
        if common.is_empty() {
            return None;
        }
    }
    // A prefix or a bare root separator is not a library.
    common
        .iter()
        .any(|c| matches!(c, Component::Normal(_)))
        .then(|| common.iter().collect())
}

/// Upgrade the store at `args.db` to the schema this build needs.
///
/// The order is the point: everything that can refuse the run does so before
/// anything is written, the user is told what is about to happen and agrees to
/// it, a snapshot makes it reversible, and only then does the store change.
///
/// Returns how many files the follow-up revalidate counted as failed, zero when
/// none ran, so the caller can exit `2` the way `revalidate` itself does
/// (#750): the store is upgraded either way, but a script chaining on the exit
/// status must be able to tell a partial revalidate from a clean one.
pub fn run_migrate(args: &MigrateArgs) -> Result<u64> {
    // The parser refuses these combinations, but this function is public and
    // its arguments are plain fields, so it enforces them itself. The first one
    // guards the only destructive step: `--repair` deletes rows, and the
    // snapshot is the only copy they survive in.
    if args.repair && args.no_snapshot {
        anyhow::bail!("--repair requires a snapshot; it cannot be combined with --no-snapshot");
    }
    if args.snapshot.is_some() && args.no_snapshot {
        anyhow::bail!("--snapshot names a snapshot path; it cannot be combined with --no-snapshot");
    }
    let db = args.db.as_path();
    if !db.exists() {
        anyhow::bail!(
            "database not found: {} (nothing to migrate; `musefs scan` creates one)",
            db.display()
        );
    }
    let pending =
        PendingMigration::open(db).with_context(|| format!("opening store {}", db.display()))?;
    let from = pending.current_version();
    let to = pending.target_version();
    if pending.is_current() {
        println!(
            "{} is already at schema version {to}; nothing to migrate.",
            db.display()
        );
        return Ok(0);
    }

    // Refuse a store somebody else is using before reporting anything, so the
    // report is never a description of work that was never on the table.
    pending.claim_exclusive()?;

    println!(
        "store {} is at schema version {from}; this build needs {to}.",
        db.display()
    );
    for step in pending.pending() {
        let mark = if step.gated {
            "  [needs this command]"
        } else {
            ""
        };
        println!(
            "  v{} (musefs {}) — {}{mark}",
            step.version, step.since, step.summary
        );
    }
    println!(
        "This rewrites the store in place. Once it is done, musefs builds older \
         than this one will no longer open it."
    );

    // The rows the new shapes refuse, before anything is copied or written.
    // Ordered here deliberately: a user who is going to be stopped should be
    // stopped before being asked about disk, snapshots or confirmation.
    let refused = pending.inspect_rejections()?;
    if !refused.is_empty() {
        println!(
            "{} rows in this store are not valid under the new schema:",
            refused.total()
        );
        for t in refused.tables() {
            println!("  {}: {} row(s)", t.table, t.rejected);
        }
        println!(
            "They were written before the constraint that now refuses them, or by a \
             writer with the constraints turned off."
        );
        if !args.repair {
            anyhow::bail!(
                "refusing to upgrade {}: {} row(s) would be rejected. Pass --repair to \
                 delete them, or fix them yourself first. The upgrade changes nothing \
                 until this is resolved",
                db.display(),
                refused.total()
            );
        }
    }

    let footprint = store_footprint(db);
    let snapshot = if args.no_snapshot {
        None
    } else {
        Some(
            args.snapshot
                .clone()
                .unwrap_or_else(|| default_snapshot_path(db, from)),
        )
    };
    if let Some(dest) = &snapshot
        && dest.exists()
    {
        anyhow::bail!(
            "snapshot destination already exists: {} (move it, or pass --snapshot PATH, \
             or --no-snapshot to skip the snapshot)",
            dest.display()
        );
    }

    // The store's own filesystem carries the rewrite, and the snapshot too when
    // the snapshot lands on that same filesystem. That is decided by device, not
    // by directory: a `--snapshot` elsewhere on the same disk draws on the same
    // free space. A snapshot on another filesystem is checked on its own.
    let snapshot_shares_store_fs = snapshot.as_ref().is_some_and(|d| same_filesystem(d, db));
    let copies = 1 + u64::from(snapshot_shares_store_fs);
    let needed = space_needed(footprint, copies);
    let available = free_space_for(db)?;
    println!(
        "store is {}; the upgrade needs about {} free and has {}.",
        HumanBytes(footprint),
        HumanBytes(needed),
        HumanBytes(available)
    );
    if available < needed {
        anyhow::bail!(
            "not enough free space on {}: need about {}, have {}. Free some space, \
             or pass --no-snapshot to skip the copy",
            db.parent().unwrap_or(Path::new(".")).display(),
            HumanBytes(needed),
            HumanBytes(available)
        );
    }
    if let Some(dest) = &snapshot
        && !snapshot_shares_store_fs
    {
        let there = free_space_for(dest)?;
        if there < footprint {
            anyhow::bail!(
                "not enough free space for the snapshot at {}: need about {}, have {}",
                dest.display(),
                HumanBytes(footprint),
                HumanBytes(there)
            );
        }
    }
    match &snapshot {
        Some(dest) => println!("a snapshot will be written to {} first.", dest.display()),
        None => println!("no snapshot will be taken (--no-snapshot): this is not reversible."),
    }

    if !args.yes {
        if !prompt::interactive() {
            anyhow::bail!(
                "refusing to upgrade {} without confirmation; pass --yes to proceed \
                 (there is no terminal here to ask on)",
                db.display()
            );
        }
        if !prompt::confirm(&format!("Upgrade {} now?", db.display()), false)? {
            println!("aborted; the store is unchanged.");
            return Ok(0);
        }
    }

    if let Some(dest) = &snapshot {
        pending
            .snapshot_to(dest)
            .with_context(|| format!("writing the snapshot to {}", dest.display()))?;
        println!("snapshot written to {}", dest.display());
    }

    // After the snapshot, so the deleted rows are in the copy the user can go
    // back to, and after the confirmation, so --repair alone never deletes.
    if !refused.is_empty() {
        let removed = pending.repair()?;
        println!(
            "repaired: deleted {} row(s) the new schema refuses",
            removed.total()
        );
        println!(
            "  they are in the snapshot at {}, if you want them back.",
            snapshot
                .as_ref()
                .expect("--repair refuses --no-snapshot, so there is always one")
                .display()
        );
    }

    let started = Instant::now();
    let store = pending.apply()?;
    println!(
        "migrated {} from schema version {from} to {to} in {}",
        db.display(),
        HumanDuration(started.elapsed())
    );

    let grown = store_footprint(db);
    if grown > footprint {
        println!(
            "the store grew from {} to {}; a vacuum reclaims the difference.",
            HumanBytes(footprint),
            HumanBytes(grown)
        );
    }
    if prompt::decide(args.vacuum, "Compact the store now?", grown > footprint)? {
        store.vacuum()?;
        println!("{}", vacuum_summary(db, grown, store_footprint(db)));
    } else if grown > footprint {
        println!("  run later: musefs vacuum --db {}", db.display());
    }

    // Every read of the store has to happen before the handle is dropped, and
    // the handle has to be dropped before a revalidate can open its own: the
    // exclusive claim taken above is held for as long as it lives.
    let owed = store.count_tracks_without_fingerprint()?;
    let root = if owed > 0 {
        common_library_root(&store.list_backing_paths()?)
    } else {
        None
    };
    drop(store);

    let mut failed = 0u64;
    if owed > 0 {
        println!(
            "{owed} track(s) now carry no fingerprint; a revalidate recomputes them and \
             restores each file's own picture metadata, and until it runs those tracks \
             cannot be recovered by a move."
        );
        let offer = root
            .as_ref()
            .map(|r| format!("Revalidate {} now?", r.display()));
        match (root, offer) {
            (Some(root), Some(question)) if prompt::decide(args.revalidate, &question, false)? => {
                failed = run_revalidate(
                    db,
                    &[root],
                    false,
                    args.jobs,
                    false,
                    false,
                    ChecksumMode::Fingerprint,
                )?;
                if failed > 0 {
                    println!(
                        "the store is upgraded, but the revalidate counted {failed} failed \
                         file(s); this run exits 2, as `musefs revalidate` would."
                    );
                }
            }
            (Some(root), _) => println!(
                "  run later: musefs revalidate {} --db {}",
                root.display(),
                db.display()
            ),
            (None, _) => {
                println!(
                    "  run later: musefs revalidate <library path> --db {}",
                    db.display()
                );
                if args.revalidate == Some(true) {
                    // Asked for explicitly and not done: fail, rather than let a
                    // script read the missing revalidate as a clean one. The
                    // upgrade itself stands, and the message says so.
                    anyhow::bail!(
                        "the store is upgraded, but --revalidate was not run: the stored \
                         tracks share no directory below the filesystem root to \
                         revalidate from; run `musefs revalidate` over each library root"
                    );
                }
            }
        }
    }
    Ok(failed)
}

/// Print a sample of the paths a `mount --dry-run` would expose, walking the
/// already-built virtual tree so the preview reflects the exact rendering the
/// real mount uses.
fn print_dry_run(core: &Musefs) -> Result<()> {
    // The virtual tree root is inode 1 (the FUSE root id).
    const ROOT_INODE: u64 = 1;
    const SAMPLE: usize = 30;
    let mut sample: Vec<String> = Vec::new();
    let (mut files, mut dirs) = (0u64, 0u64);
    walk_preview(
        core,
        ROOT_INODE,
        "",
        SAMPLE,
        &mut sample,
        &mut files,
        &mut dirs,
    )?;
    if files == 0 {
        println!(
            "dry run: this template produces no files (the store is empty, or every track was dropped by --skip-on-missing)."
        );
        return Ok(());
    }
    println!("dry run: {files} files across {dirs} directories. Sample paths:");
    for path in &sample {
        println!("  {path}");
    }
    if files > sample.len() as u64 {
        println!("  ... and {} more", files - sample.len() as u64);
    }
    Ok(())
}

/// Depth-first walk of the virtual tree. `readdir` returns name-sorted children,
/// so `sample` collects the first `cap` file paths in lexicographic order while
/// `files`/`dirs` accumulate the totals.
fn walk_preview(
    core: &Musefs,
    inode: u64,
    prefix: &str,
    cap: usize,
    sample: &mut Vec<String>,
    files: &mut u64,
    dirs: &mut u64,
) -> Result<()> {
    for (name, child, is_dir) in core.readdir(inode)? {
        let path = if prefix.is_empty() {
            name
        } else {
            format!("{prefix}/{name}")
        };
        if is_dir {
            *dirs += 1;
            walk_preview(core, child, &path, cap, sample, files, dirs)?;
        } else {
            *files += 1;
            if sample.len() < cap {
                sample.push(path);
            }
        }
    }
    Ok(())
}

pub fn run(cli: Cli) -> Result<ExitCode> {
    match cli.command {
        Command::Scan {
            targets,
            db,
            force,
            jobs,
            follow_symlinks,
            quiet,
            checksum,
            match_mode,
        } => {
            refuse_retired_env(RETIRED_SCAN_ENV)?;
            let failed = run_scan(
                &db,
                &targets,
                force,
                jobs,
                follow_symlinks,
                quiet,
                checksum,
                match_mode,
            )?;
            // Per-file ingest failures are not a hard error (they don't abort the
            // batch), but a pipeline like `scan && mount` needs a machine-detectable
            // signal that not everything ingested. Exit 2 marks any per-file failure
            // (partial or total); it overlaps clap's usage-error code, but only ever
            // fires after a successful parse + run (#554).
            Ok(if failed > 0 {
                ExitCode::from(2)
            } else {
                ExitCode::SUCCESS
            })
        }
        Command::Revalidate {
            targets,
            db,
            prune,
            jobs,
            follow_symlinks,
            quiet,
            checksum,
        } => {
            let failed =
                run_revalidate(&db, &targets, prune, jobs, follow_symlinks, quiet, checksum)?;
            Ok(if failed > 0 {
                ExitCode::from(2)
            } else {
                ExitCode::SUCCESS
            })
        }
        Command::Mount(args) => run_mount(&args).map(|()| ExitCode::SUCCESS),
        Command::Vacuum { db } => run_vacuum(&db).map(|()| ExitCode::SUCCESS),
        Command::Migrate(args) => {
            // The store upgrade succeeded if this returns at all; a failure
            // count comes from the revalidate it offered, and marks the run
            // partial exactly as `revalidate`'s own does (#750).
            let failed = run_migrate(&args)?;
            Ok(if failed > 0 {
                ExitCode::from(2)
            } else {
                ExitCode::SUCCESS
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// #705: the warning names the count and the command while any track lacks
    /// both values a probe writes, and says nothing once none does.
    #[test]
    fn the_owed_revalidate_warning_lasts_until_every_track_is_reprobed() {
        let db = Db::open_in_memory().unwrap();
        let store = Path::new("/srv/library.db");
        assert_eq!(revalidate_owed_warning(&db, store).unwrap(), None);

        let mut track = musefs_db::NewTrack {
            backing_path: PathBuf::from("/lib/a.flac"),
            format: musefs_db::Format::Flac,
            audio_offset: 0,
            audio_length: 1,
            backing_size: 1,
            backing_mtime_ns: 0,
            backing_ctime_ns: 0,
            backing_ino: None,
        };
        db.upsert_track(&track).unwrap();
        let warning = revalidate_owed_warning(&db, store)
            .unwrap()
            .expect("an upgraded row is owed a revalidate");
        assert!(warning.starts_with("1 track(s) "), "{warning}");
        assert!(
            warning.contains("`musefs revalidate <library> --db /srv/library.db`"),
            "{warning}"
        );

        track.backing_ino = Some(7);
        db.upsert_track(&track).unwrap();
        assert_eq!(revalidate_owed_warning(&db, store).unwrap(), None);
    }

    #[test]
    fn space_needed_scales_with_the_copies_and_saturates() {
        assert_eq!(space_needed(100, 1), 100);
        assert_eq!(space_needed(100, 2), 200);
        // A nonsense footprint must not wrap the estimate to a small number and
        // wave through a run that cannot fit.
        assert_eq!(space_needed(u64::MAX, 2), u64::MAX);
    }

    #[test]
    fn default_snapshot_path_names_the_version_it_came_from() {
        assert_eq!(
            default_snapshot_path(Path::new("/srv/library.db"), 3),
            PathBuf::from("/srv/library.db.v3.bak")
        );
        // Two upgrades of one store must not collide on the same backup file.
        assert_ne!(
            default_snapshot_path(Path::new("/srv/library.db"), 3),
            default_snapshot_path(Path::new("/srv/library.db"), 4)
        );
    }

    /// A `--snapshot` in another directory on the store's filesystem draws on
    /// the same free space as the rewrite, so the pre-flight has to count both
    /// against it; one on another filesystem does not.
    #[test]
    fn same_filesystem_is_decided_by_device_not_directory() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("db")).unwrap();
        std::fs::create_dir(dir.path().join("backups")).unwrap();
        assert!(
            same_filesystem(
                &dir.path().join("backups/library.db.v3.bak"),
                &dir.path().join("db/library.db")
            ),
            "different directories on one filesystem"
        );
        #[cfg(target_os = "linux")]
        assert!(
            !same_filesystem(
                Path::new("/proc/library.db.v3.bak"),
                &dir.path().join("db/library.db")
            ),
            "procfs is another filesystem"
        );
    }

    #[test]
    fn common_library_root_is_the_deepest_shared_directory() {
        let paths = [
            PathBuf::from("/srv/music/a/one.flac"),
            PathBuf::from("/srv/music/b/two.mp3"),
            PathBuf::from("/srv/music/b/c/three.mp3"),
        ];
        assert_eq!(
            common_library_root(&paths),
            Some(PathBuf::from("/srv/music"))
        );
        // One track's own directory is the whole library.
        assert_eq!(
            common_library_root(&[PathBuf::from("/srv/music/a/one.flac")]),
            Some(PathBuf::from("/srv/music/a"))
        );
    }

    /// Revalidating from the filesystem root is never what anyone meant, so a
    /// library that spans unrelated trees gets the command printed instead of
    /// an offer to walk everything.
    #[test]
    fn common_library_root_declines_to_offer_the_filesystem_root() {
        let split = [
            PathBuf::from("/srv/music/one.flac"),
            PathBuf::from("/home/u/music/two.mp3"),
        ];
        assert_eq!(common_library_root(&split), None);
        assert_eq!(common_library_root(&[PathBuf::from("/one.flac")]), None);
        assert_eq!(common_library_root(&[]), None);
    }

    /// An explicit flag is answered without consulting a terminal, which is
    /// what keeps a script from ever being stuck on a prompt.
    #[test]
    fn an_explicit_answer_is_taken_as_given() {
        assert!(prompt::decide(Some(true), "unused", false).unwrap());
        assert!(!prompt::decide(Some(false), "unused", true).unwrap());
        // No flag and no terminal (the shape a test runs in): decline, leaving
        // the user something they can still run by hand.
        assert!(!prompt::decide(None, "unused", true).unwrap());
    }

    #[test]
    fn migrate_command_parses_its_flags() {
        use clap::Parser;
        let cli = Cli::try_parse_from([
            "musefs",
            "migrate",
            "--db",
            "/tmp/x.db",
            "--yes",
            "--snapshot",
            "/tmp/backup.db",
            "--vacuum",
            "--revalidate=false",
            "--jobs",
            "4",
        ])
        .unwrap();
        match cli.command {
            Command::Migrate(args) => {
                assert_eq!(args.db, PathBuf::from("/tmp/x.db"));
                assert!(args.yes);
                assert_eq!(args.snapshot, Some(PathBuf::from("/tmp/backup.db")));
                assert!(!args.no_snapshot);
                assert_eq!(args.vacuum, Some(true));
                assert_eq!(args.revalidate, Some(false));
                assert_eq!(args.jobs, 4);
            }
            _ => panic!("expected Migrate"),
        }
    }

    /// Unset is distinct from `false`: it means "ask", and off a terminal it
    /// means "skip and print the command". Collapsing the two would make a
    /// scripted run silently do work nobody asked for.
    #[test]
    fn migrate_offers_default_to_unset_not_false() {
        use clap::Parser;
        let cli = Cli::try_parse_from(["musefs", "migrate", "--db", "/tmp/x.db"]).unwrap();
        match cli.command {
            Command::Migrate(args) => {
                assert_eq!(args.vacuum, None);
                assert_eq!(args.revalidate, None);
                assert!(!args.yes);
                assert_eq!(args.snapshot, None);
            }
            _ => panic!("expected Migrate"),
        }
    }

    /// Naming a snapshot and refusing to take one are contradictory, and clap
    /// says so rather than silently honouring one of them.
    /// `--repair` deletes rows for good, and the snapshot is the only copy they
    /// survive in — so asking for one without the other is refused rather than
    /// quietly honoured.
    #[test]
    fn migrate_rejects_repair_alongside_no_snapshot() {
        use clap::Parser;
        assert!(
            Cli::try_parse_from([
                "musefs",
                "migrate",
                "--db",
                "/tmp/x.db",
                "--repair",
                "--no-snapshot",
            ])
            .is_err()
        );
        // Either alone is fine.
        for flag in ["--repair", "--no-snapshot"] {
            Cli::try_parse_from(["musefs", "migrate", "--db", "/tmp/x.db", flag])
                .unwrap_or_else(|e| panic!("{flag} alone must parse: {e}"));
        }
    }

    #[test]
    fn migrate_rejects_a_named_snapshot_alongside_no_snapshot() {
        use clap::Parser;
        assert!(
            Cli::try_parse_from([
                "musefs",
                "migrate",
                "--db",
                "/tmp/x.db",
                "--snapshot",
                "/tmp/b.db",
                "--no-snapshot",
            ])
            .is_err()
        );
    }

    #[test]
    fn read_ahead_budget_flag_maps_to_mount_config() {
        use clap::Parser;
        let cli = Cli::try_parse_from([
            "musefs",
            "mount",
            "/mnt",
            "--db",
            "/tmp/x.db",
            "--read-ahead-budget-mib",
            "128",
        ])
        .unwrap();
        let Command::Mount(args) = cli.command else {
            panic!("expected Mount");
        };
        let (config, _) = parse_mount_config(&args);
        assert_eq!(config.read_ahead_budget, 128 * 1024 * 1024);
    }

    #[test]
    fn mountpoint_is_nonempty_detects_contents() {
        let dir = tempfile::tempdir().unwrap();
        assert!(
            !mountpoint_is_nonempty(dir.path()),
            "fresh tempdir is empty"
        );
        std::fs::write(dir.path().join("stray.mp3"), b"x").unwrap();
        assert!(
            mountpoint_is_nonempty(dir.path()),
            "a populated dir is non-empty (#508)"
        );
    }

    #[test]
    fn mountpoint_is_nonempty_is_false_on_unreadable_path() {
        // A nonexistent path can't be read; the advisory check fails safe to
        // "empty" rather than erroring.
        assert!(!mountpoint_is_nonempty(std::path::Path::new(
            "/nonexistent/musefs/mountpoint"
        )));
    }

    #[test]
    fn mount_denied_help_points_at_apparmor_and_the_fix() {
        assert!(MOUNT_DENIED_HELP.contains("AppArmor"));
        assert!(MOUNT_DENIED_HELP.contains("/etc/apparmor.d/local/fusermount3"));
    }

    #[test]
    fn read_ahead_budget_zero_disables() {
        use clap::Parser;
        let cli = Cli::try_parse_from([
            "musefs",
            "mount",
            "/mnt",
            "--db",
            "/tmp/x.db",
            "--read-ahead-budget-mib",
            "0",
        ])
        .unwrap();
        let Command::Mount(args) = cli.command else {
            panic!("expected Mount");
        };
        let (config, _) = parse_mount_config(&args);
        assert_eq!(config.read_ahead_budget, 0);
    }

    #[test]
    fn read_ahead_prefetch_defaults_off_and_opts_in() {
        use clap::Parser;
        let base = ["musefs", "mount", "/mnt", "--db", "/tmp/x.db"];
        let off = Cli::try_parse_from(base).unwrap();
        let Command::Mount(args) = off.command else {
            panic!("expected Mount");
        };
        assert!(
            !parse_mount_config(&args).0.read_ahead_prefetch,
            "Phase-2 prefetch must default off"
        );

        let on = Cli::try_parse_from(base.iter().chain(["--read-ahead-prefetch"].iter())).unwrap();
        let Command::Mount(args) = on.command else {
            panic!("expected Mount");
        };
        assert!(
            parse_mount_config(&args).0.read_ahead_prefetch,
            "flag opts in"
        );
    }

    #[test]
    fn trust_backing_mtime_defaults_off_and_opts_in() {
        use clap::Parser;
        let base = ["musefs", "mount", "/mnt", "--db", "/tmp/x.db"];
        let off = Cli::try_parse_from(base).unwrap();
        let Command::Mount(args) = off.command else {
            panic!("expected Mount");
        };
        assert!(
            !parse_mount_config(&args).0.trust_backing_mtime,
            "the getattr re-stat must stay the default"
        );

        let on = Cli::try_parse_from(base.iter().chain(["--trust-backing-mtime"].iter())).unwrap();
        let Command::Mount(args) = on.command else {
            panic!("expected Mount");
        };
        assert!(
            parse_mount_config(&args).0.trust_backing_mtime,
            "flag opts in"
        );
    }

    #[test]
    fn skip_on_missing_defaults_off_and_opts_in() {
        use clap::Parser;
        let base = ["musefs", "mount", "/mnt", "--db", "/tmp/x.db"];
        let off = Cli::try_parse_from(base).unwrap();
        let Command::Mount(args) = off.command else {
            panic!("expected Mount");
        };
        assert!(
            !parse_mount_config(&args).0.skip_on_missing,
            "skip-on-missing must default off"
        );

        let on = Cli::try_parse_from(base.iter().chain(["--skip-on-missing"].iter())).unwrap();
        let Command::Mount(args) = on.command else {
            panic!("expected Mount");
        };
        assert!(parse_mount_config(&args).0.skip_on_missing, "flag opts in");
    }

    #[test]
    fn scan_command_parses_jobs_flag() {
        use clap::Parser;
        let cli = Cli::try_parse_from(["musefs", "scan", "/m", "--db", "/tmp/x.db", "--jobs", "3"])
            .unwrap();
        match cli.command {
            Command::Scan { jobs, targets, .. } => {
                assert_eq!(jobs, 3);
                assert_eq!(targets, vec![PathBuf::from("/m")]);
            }
            Command::Mount(..) => panic!("expected Scan"),
            Command::Revalidate { .. } | Command::Vacuum { .. } | Command::Migrate(..) => {
                unreachable!()
            }
        }
    }

    #[test]
    fn scan_command_quiet_flag_defaults_off_and_parses() {
        use clap::Parser;
        let cli = Cli::try_parse_from(["musefs", "scan", "/m", "--db", "/tmp/x.db"]).unwrap();
        match cli.command {
            Command::Scan { quiet, .. } => assert!(!quiet),
            Command::Mount(..) => panic!("expected Scan"),
            Command::Revalidate { .. } | Command::Vacuum { .. } | Command::Migrate(..) => {
                unreachable!()
            }
        }
        for arg in ["--quiet", "-q"] {
            let cli =
                Cli::try_parse_from(["musefs", "scan", "/m", "--db", "/tmp/x.db", arg]).unwrap();
            match cli.command {
                Command::Scan { quiet, .. } => assert!(quiet),
                Command::Mount(..) => panic!("expected Scan"),
                Command::Revalidate { .. } | Command::Vacuum { .. } | Command::Migrate(..) => {
                    unreachable!()
                }
            }
        }
    }

    #[test]
    fn scan_command_parses_follow_symlinks_flag() {
        use clap::Parser;
        let cli = Cli::try_parse_from([
            "musefs",
            "scan",
            "/m",
            "--db",
            "/tmp/x.db",
            "--follow-symlinks",
        ])
        .unwrap();
        match cli.command {
            Command::Scan {
                follow_symlinks, ..
            } => assert!(follow_symlinks),
            Command::Mount(..) => panic!("expected scan command"),
            Command::Revalidate { .. } | Command::Vacuum { .. } | Command::Migrate(..) => {
                unreachable!()
            }
        }
    }

    #[test]
    fn scan_command_follow_symlinks_defaults_off() {
        use clap::Parser;
        let cli = Cli::try_parse_from(["musefs", "scan", "/m", "--db", "/tmp/x.db"]).unwrap();
        match cli.command {
            Command::Scan {
                follow_symlinks, ..
            } => assert!(!follow_symlinks),
            Command::Mount(..) => panic!("expected scan command"),
            Command::Revalidate { .. } | Command::Vacuum { .. } | Command::Migrate(..) => {
                unreachable!()
            }
        }
    }

    #[test]
    fn scan_command_parses_multiple_paths() {
        use clap::Parser;
        let cli =
            Cli::try_parse_from(["musefs", "scan", "/a", "/b", "/c", "--db", "/tmp/x.db"]).unwrap();
        match cli.command {
            Command::Scan { targets, .. } => {
                assert_eq!(
                    targets,
                    vec![
                        PathBuf::from("/a"),
                        PathBuf::from("/b"),
                        PathBuf::from("/c")
                    ]
                );
            }
            Command::Mount(..) => panic!("expected Scan"),
            Command::Revalidate { .. } | Command::Vacuum { .. } | Command::Migrate(..) => {
                unreachable!()
            }
        }
    }

    #[test]
    fn scan_accepts_force() {
        use clap::Parser;
        let cli = Cli::try_parse_from(["musefs", "scan", "/m", "--db", "/d", "--force"]).unwrap();
        let Command::Scan { force, .. } = cli.command else {
            panic!("expected Scan");
        };
        assert!(force);
    }

    #[test]
    fn revalidate_subcommand_parses_with_prune() {
        use clap::Parser;
        let cli =
            Cli::try_parse_from(["musefs", "revalidate", "/m", "--db", "/d", "--prune"]).unwrap();
        let Command::Revalidate { prune, targets, .. } = cli.command else {
            panic!("expected Revalidate");
        };
        assert!(prune);
        assert_eq!(targets, vec![PathBuf::from("/m")]);
    }

    #[test]
    fn scan_rejects_prune() {
        use clap::Parser;
        assert!(Cli::try_parse_from(["musefs", "scan", "/m", "--db", "/d", "--prune"]).is_err());
    }

    #[test]
    fn revalidate_rejects_force() {
        use clap::Parser;
        assert!(
            Cli::try_parse_from(["musefs", "revalidate", "/m", "--db", "/d", "--force"]).is_err()
        );
    }

    #[test]
    fn mount_args_parse_into_configs() {
        use clap::Parser;
        let cli = Cli::try_parse_from([
            "musefs",
            "mount",
            "/mnt/muse",
            "--db",
            "/tmp/x.db",
            "--poll-interval-ms",
            "250",
            "--attr-ttl-ms",
            "750",
            "--max-readahead-kib",
            "64",
            "--max-background",
            "32",
            "--expose-metrics",
        ])
        .unwrap();
        let Command::Mount(args) = cli.command else {
            panic!("expected Mount");
        };
        let (config, fuse_config) = parse_mount_config(&args);
        // Defaults survive the move into the struct.
        assert_eq!(config.template, "$albumartist/$album/$title");
        assert_eq!(config.default_fallback, "Unknown");
        assert_eq!(config.mode, musefs_core::Mode::Synthesis);
        // #432: keep-cache defaults on when the flag is absent.
        assert!(fuse_config.keep_cache);
        assert_eq!(config.case_insensitive, cfg!(target_os = "macos"));
        // ms → Duration.
        assert_eq!(config.poll_interval, std::time::Duration::from_millis(250));
        assert_eq!(fuse_config.ttl, std::time::Duration::from_millis(750));
        // KiB → bytes.
        assert_eq!(fuse_config.max_readahead, 64 * 1024);
        assert_eq!(fuse_config.max_background, 32);
        assert!(fuse_config.expose_metrics);
    }

    #[test]
    fn workers_flag_maps_through_with_auto_default() {
        use clap::Parser;
        let parse = |extra: &[&str]| {
            let mut argv = vec!["musefs", "mount", "/mnt/muse", "--db", "/tmp/x.db"];
            argv.extend_from_slice(extra);
            let cli = Cli::try_parse_from(argv).unwrap();
            let Command::Mount(args) = cli.command else {
                panic!("expected Mount");
            };
            parse_mount_config(&args).1.workers
        };
        // Absent → 0, the "auto: 2× CPUs" sentinel resolved in musefs-fuse.
        assert_eq!(parse(&[]), 0);
        assert_eq!(parse(&["--workers", "8"]), 8);
    }

    #[test]
    fn keep_cache_flag_forms() {
        use clap::Parser;
        let parse = |extra: &[&str]| {
            let mut argv = vec!["musefs", "mount", "/mnt/muse", "--db", "/tmp/x.db"];
            argv.extend_from_slice(extra);
            let cli = Cli::try_parse_from(argv).unwrap();
            let Command::Mount(args) = cli.command else {
                panic!("expected Mount");
            };
            parse_mount_config(&args).1.keep_cache
        };
        // Absent → default on (#432).
        assert!(parse(&[]));
        // Bare flag stays a backward-compatible "on".
        assert!(parse(&["--keep-cache"]));
        // Explicit value opts out / in.
        assert!(!parse(&["--keep-cache", "false"]));
        assert!(parse(&["--keep-cache", "true"]));
    }

    #[test]
    fn case_insensitive_defaults_to_os() {
        use clap::Parser;
        let cli = Cli::try_parse_from(["musefs", "mount", "/mnt", "--db", "/tmp/x.db"]).unwrap();
        let Command::Mount(args) = cli.command else {
            panic!("expected Mount");
        };
        let (config, _) = parse_mount_config(&args);
        assert_eq!(config.case_insensitive, cfg!(target_os = "macos"));
    }

    #[test]
    fn case_insensitive_is_overridable() {
        use clap::Parser;
        // The boolish set (1/0, yes/no, on/off, t/f) parses, not just true/false.
        for (val, want) in [
            ("true", true),
            ("false", false),
            ("1", true),
            ("0", false),
            ("yes", true),
            ("no", false),
            ("on", true),
            ("off", false),
        ] {
            let cli = Cli::try_parse_from([
                "musefs",
                "mount",
                "/mnt",
                "--db",
                "/tmp/x.db",
                "--case-insensitive",
                val,
            ])
            .unwrap();
            let Command::Mount(args) = cli.command else {
                panic!("expected Mount");
            };
            assert_eq!(args.case_insensitive, want);
        }
    }

    #[test]
    fn octal_mode_parses_as_octal_not_decimal() {
        assert_eq!(parse_octal_mode("644").unwrap(), 0o644);
        assert_eq!(parse_octal_mode("644").unwrap(), 420);
        assert_eq!(parse_octal_mode("0755").unwrap(), 0o755);
    }

    #[test]
    fn octal_mode_rejects_out_of_range_and_non_octal() {
        assert!(parse_octal_mode("10000").is_err());
        assert!(parse_octal_mode("8").is_err());
        assert!(parse_octal_mode("xyz").is_err());
    }

    #[test]
    fn write_bit_warning_fires_only_for_write_bits() {
        assert!(write_bit_warning("file-mode", 0o444).is_none());
        assert!(write_bit_warning("dir-mode", 0o555).is_none());
        assert!(write_bit_warning("file-mode", 0o664).is_some());
        assert!(write_bit_warning("dir-mode", 0o775).is_some());
    }

    #[test]
    fn owner_and_modes_flow_into_fuse_config() {
        use clap::Parser;
        let cli = Cli::try_parse_from([
            "musefs",
            "mount",
            "/mnt",
            "--db",
            "/tmp/x.db",
            "--owner",
            "0",
            "--group",
            "0",
            "--file-mode",
            "640",
            "--dir-mode",
            "750",
        ])
        .unwrap();
        let Command::Mount(args) = cli.command else {
            panic!("expected Mount");
        };
        let (_config, fuse_config) = parse_mount_config(&args);
        assert_eq!(fuse_config.uid, 0);
        assert_eq!(fuse_config.gid, 0);
        assert_eq!(fuse_config.file_mode, 0o640);
        assert_eq!(fuse_config.dir_mode, 0o750);
    }

    #[test]
    fn owner_flags_default_to_process_identity() {
        use clap::Parser;
        let cli = Cli::try_parse_from(["musefs", "mount", "/mnt", "--db", "/tmp/x.db"]).unwrap();
        let Command::Mount(args) = cli.command else {
            panic!("expected Mount");
        };
        let (_config, fuse_config) = parse_mount_config(&args);
        let defaults = musefs_fuse::FuseConfig::default();
        assert_eq!(fuse_config.uid, defaults.uid);
        assert_eq!(fuse_config.gid, defaults.gid);
        assert_eq!(fuse_config.file_mode, 0o444);
        assert_eq!(fuse_config.dir_mode, 0o555);
    }

    #[test]
    fn owner_accepts_numeric_and_rejects_unknown_name() {
        assert_eq!(parse_owner("1234").unwrap(), 1234);
        assert!(parse_owner("").is_err());
        assert!(parse_owner("definitely-no-such-user-xyzzy").is_err());
    }

    #[test]
    fn group_accepts_numeric_and_rejects_unknown_name() {
        assert_eq!(parse_group("1234").unwrap(), 1234);
        assert!(parse_group("").is_err());
        assert!(parse_group("definitely-no-such-group-xyzzy").is_err());
    }

    #[test]
    fn owner_or_group_auto_enables_allow_other() {
        use clap::Parser;
        for arg in [["--owner", "0"], ["--group", "0"]] {
            let cli = Cli::try_parse_from([
                "musefs",
                "mount",
                "/mnt",
                "--db",
                "/tmp/x.db",
                arg[0],
                arg[1],
            ])
            .unwrap();
            let Command::Mount(args) = cli.command else {
                panic!("expected Mount");
            };
            let (_config, fuse_config) = parse_mount_config(&args);
            assert!(fuse_config.allow_other, "{arg:?} should enable allow_other");
        }
    }

    #[test]
    fn allow_other_defaults_off_without_owner_group() {
        use clap::Parser;
        let cli = Cli::try_parse_from(["musefs", "mount", "/mnt", "--db", "/tmp/x.db"]).unwrap();
        let Command::Mount(args) = cli.command else {
            panic!("expected Mount");
        };
        let (_config, fuse_config) = parse_mount_config(&args);
        assert!(!fuse_config.allow_other);
    }

    #[test]
    fn expose_metrics_defaults_off() {
        use clap::Parser;
        let cli = Cli::try_parse_from(["musefs", "mount", "/mnt", "--db", "/tmp/x.db"]).unwrap();
        let Command::Mount(args) = cli.command else {
            panic!("expected Mount")
        };
        let (_c, fuse_config) = parse_mount_config(&args);
        assert!(!fuse_config.expose_metrics);
    }

    #[test]
    fn effective_allow_other_combines_flag_and_owner_group() {
        assert!(!effective_allow_other(false, None, None));
        assert!(effective_allow_other(true, None, None));
        // Auto-enable wins even when the flag is explicitly false (env path).
        assert!(effective_allow_other(false, Some(0), None));
        assert!(effective_allow_other(false, None, Some(0)));
    }

    #[test]
    fn explicit_allow_other_flag_enables_it() {
        use clap::Parser;
        let cli = Cli::try_parse_from([
            "musefs",
            "mount",
            "/mnt",
            "--db",
            "/tmp/x.db",
            "--allow-other",
        ])
        .unwrap();
        let Command::Mount(args) = cli.command else {
            panic!("expected Mount");
        };
        let (_config, fuse_config) = parse_mount_config(&args);
        assert!(fuse_config.allow_other);
    }

    #[test]
    fn vacuum_command_parses_db_path() {
        use clap::Parser;
        let cli = Cli::try_parse_from(["musefs", "vacuum", "--db", "/tmp/x.db"]).unwrap();
        let Command::Vacuum { db } = cli.command else {
            panic!("expected Vacuum");
        };
        assert_eq!(db, PathBuf::from("/tmp/x.db"));
    }

    #[test]
    fn run_vacuum_errors_on_missing_db() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("nope.db");
        assert!(run_vacuum(&missing).is_err());
    }

    #[test]
    fn run_vacuum_compacts_a_bloated_store() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("lib.db");
        {
            let db = musefs_db::Db::open(&path).unwrap();
            for i in 0..16u8 {
                db.upsert_art(&musefs_db::NewArt {
                    data: vec![i; 256 * 1024],
                })
                .unwrap();
            }
            assert_eq!(db.gc_orphan_art().unwrap(), 16);
        }
        let before = std::fs::metadata(&path).unwrap().len();
        run_vacuum(&path).unwrap();
        let after = std::fs::metadata(&path).unwrap().len();
        assert!(after < before, "expected shrink: {before} -> {after}");
    }

    #[test]
    fn vacuum_summary_reports_reclaimed_then_compact() {
        let p = Path::new("/x.db");
        assert!(vacuum_summary(p, 1000, 400).contains("reclaimed"));
        // Equal sizes => already compact.
        assert!(vacuum_summary(p, 400, 400).contains("already compact"));
        // VACUUM can grow a tiny file; saturating delta must not go negative.
        assert!(vacuum_summary(p, 400, 410).contains("already compact"));
    }
}
