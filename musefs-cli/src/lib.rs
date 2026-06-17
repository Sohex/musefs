//! The `musefs` command-line interface: `scan` (ingest a backing directory into a
//! SQLite store) and `mount` (serve a read-only FUSE view of that store).

use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use indicatif::HumanDuration;
use musefs_core::{MountConfig, Musefs};
use musefs_db::Db;

use crate::progress::ScanReporter;

mod progress;
mod signal;

/// Mount content mode (CLI surface for `musefs_core::Mode`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
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

#[derive(Parser, Debug)]
#[command(
    name = "musefs",
    version,
    propagate_version = true,
    about = "Read-only re-tagging FUSE view of a music library"
)]
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
pub struct MountArgs {
    /// Empty directory to mount at.
    #[arg(env = "MUSEFS_MOUNTPOINT")]
    pub mountpoint: PathBuf,
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
    #[arg(long, env = "MUSEFS_SKIP_ON_MISSING", default_value_t = false)]
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
    /// Enable Phase-2 background prefetch threads (advanced). Off by default:
    /// read amplification alone carries the read-ahead win; the threads add
    /// overhead without benefit on tested backends (NFS, SSD). See the benchmarks docs: https://sohex.github.io/musefs/benchmarks.html
    #[arg(long, env = "MUSEFS_READ_AHEAD_PREFETCH", default_value_t = false)]
    pub read_ahead_prefetch: bool,
    /// Max outstanding background (readahead/async) requests the kernel queues.
    #[arg(long, env = "MUSEFS_MAX_BACKGROUND", default_value_t = 64)]
    pub max_background: u16,
    /// Keep the kernel page cache across opens. On by default: it is the one
    /// measured storage win (~3× faster repeat-open on HDD/NFS, #432). External
    /// re-tags auto-invalidate the affected inodes on refresh, so cached bytes
    /// are dropped when content changes. Disable with `--keep-cache false`.
    #[arg(long, env = "MUSEFS_KEEP_CACHE", default_value_t = true, num_args = 0..=1, default_missing_value = "true", value_parser = clap::builder::BoolishValueParser::new())]
    pub keep_cache: bool,
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
pub enum Command {
    /// Walk backing files or directories, ingesting supported audio
    /// (FLAC, MP3, M4A, Ogg, WAV) into the SQLite store.
    Scan {
        /// One or more files or directories to scan (directories recurse).
        #[arg(required = true, num_args = 1..)]
        targets: Vec<PathBuf>,
        /// Path to the SQLite database (created if absent).
        #[arg(long, env = "MUSEFS_DB")]
        db: PathBuf,
        /// Re-validate: skip unchanged files, prune tracks whose backing file is
        /// gone, and garbage-collect orphaned art.
        #[arg(long, env = "MUSEFS_REVALIDATE", value_parser = clap::builder::BoolishValueParser::new())]
        revalidate: bool,
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
        /// Confirm a move only by fingerprint, never reading the full file.
        #[arg(long, env = "MUSEFS_FAST", value_parser = clap::builder::BoolishValueParser::new())]
        fast: bool,
        /// Require a full-hash match to retarget a moved file.
        #[arg(long, env = "MUSEFS_STRICT", value_parser = clap::builder::BoolishValueParser::new())]
        strict: bool,
    },
    /// Mount a read-only FUSE view of the store.
    Mount(MountArgs),
}

/// Open (creating/migrating) the DB at `db_path` once, then scan each target in
/// `targets` (a file or a directory; directories recurse). With `revalidate`,
/// run the maintenance pass (skip-unchanged, prune, GC) instead of a full
/// ingest. With `quiet`, suppress the per-target summary on stdout. Fails fast:
/// the first failing target aborts the batch; targets already scanned stay
/// committed (ingest is an idempotent upsert).
#[allow(clippy::too_many_arguments, clippy::fn_params_excessive_bools)]
pub fn run_scan(
    db_path: &Path,
    targets: &[PathBuf],
    revalidate: bool,
    jobs: usize,
    follow_symlinks: bool,
    quiet: bool,
    checksum: ChecksumMode,
    fast: bool,
    strict: bool,
) -> Result<()> {
    let strictness = match (fast, strict) {
        (true, true) => anyhow::bail!("--fast and --strict are mutually exclusive"),
        (true, false) => musefs_core::MatchStrictness::Fast,
        (false, true) => musefs_core::MatchStrictness::Strict,
        (false, false) => musefs_core::MatchStrictness::Auto,
    };
    let db =
        Db::open(db_path).with_context(|| format!("opening database at {}", db_path.display()))?;
    let reporter = ScanReporter::new(quiet);
    let opts = musefs_core::ScanOptions {
        jobs,
        follow_symlinks,
        progress: reporter.sink(),
        checksum: checksum.into(),
        strictness,
        ..Default::default()
    };
    for target in targets {
        reporter.start_target();
        let start = Instant::now();
        if revalidate {
            let stats = musefs_core::revalidate_with(&db, target, &opts)
                .with_context(|| format!("revalidating {}", target.display()))?;
            if !quiet {
                println!(
                    "revalidated {}: {} updated, {} unchanged, {} pruned, {} failed in {}",
                    target.display(),
                    stats.updated,
                    stats.unchanged,
                    stats.pruned,
                    stats.failed,
                    HumanDuration(start.elapsed()),
                );
            }
        } else {
            let stats = musefs_core::scan_directory_with(&db, target, &opts)
                .with_context(|| format!("scanning {}", target.display()))?;
            if !quiet {
                println!(
                    "scanned {}: {} file(s), skipped {}, failed {} in {}",
                    target.display(),
                    stats.scanned,
                    stats.skipped,
                    stats.failed,
                    HumanDuration(start.elapsed()),
                );
            }
        }
    }
    reporter.finish();
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
    let config = MountConfig {
        template: args.template.clone(),
        // Field names are case-insensitive everywhere else (the template parser
        // and `tags_to_fields` ASCII-lowercase them), so a fallback keyed under
        // any uppercase letter would never match at render time (#504). Normalize
        // the key the same way; later duplicates win, matching `collect`'s prior
        // last-write semantics.
        fallbacks: args
            .fallbacks
            .iter()
            .map(|(field, value)| (field.to_ascii_lowercase(), value.clone()))
            .collect(),
        default_fallback: args.default_fallback.clone(),
        mode: args.mode.into(),
        poll_interval: std::time::Duration::from_millis(args.poll_interval_ms),
        case_insensitive: args.case_insensitive,
        read_ahead_budget: u64::from(args.read_ahead_budget_mib).saturating_mul(1024 * 1024),
        read_ahead_prefetch: args.read_ahead_prefetch,
        skip_on_missing: args.skip_on_missing,
    };
    let defaults = musefs_fuse::FuseConfig::default();
    let fuse_config = musefs_fuse::FuseConfig {
        ttl: std::time::Duration::from_millis(args.attr_ttl_ms),
        max_readahead: args.max_readahead_kib.saturating_mul(1024),
        max_background: args.max_background,
        keep_cache: args.keep_cache,
        uid: args.owner.unwrap_or(defaults.uid),
        gid: args.group.unwrap_or(defaults.gid),
        file_mode: args.file_mode.unwrap_or(defaults.file_mode),
        dir_mode: args.dir_mode.unwrap_or(defaults.dir_mode),
        allow_other: effective_allow_other(args.allow_other, args.owner, args.group),
        expose_metrics: args.expose_metrics,
    };
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
    if !args.dry_run && !args.mountpoint.is_dir() {
        if args.mountpoint.exists() {
            anyhow::bail!(
                "mountpoint is not a directory: {}",
                args.mountpoint.display()
            );
        }
        anyhow::bail!(
            "mountpoint does not exist (create it first): {}",
            args.mountpoint.display()
        );
    }
    // The mountpoint help says "Empty directory", but FUSE happily mounts over a
    // populated one and shadows its contents for the mount's lifetime. Warn so a
    // typo (or reusing a real music folder) doesn't silently hide files (#508).
    if !args.dry_run && mountpoint_is_nonempty(&args.mountpoint) {
        eprintln!(
            "warning: mountpoint {} is not empty; its existing contents will be \
             hidden behind the virtual tree until you unmount",
            args.mountpoint.display()
        );
    }
    let db =
        Db::open(&args.db).with_context(|| format!("opening database at {}", args.db.display()))?;
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
    signal::install_unmount_on_signal(args.mountpoint.clone())
        .context("installing the stop-signal unmount handler")?;
    // The "is it serving the right library?" context, alongside the mount-success
    // line `mount_with` emits with the file/dir counts (#522).
    log::info!(
        "serving database {} at {} (template {:?})",
        args.db.display(),
        args.mountpoint.display(),
        template,
    );
    musefs_fuse::mount_with(core, &args.mountpoint, "musefs", fuse_config).map_err(|e| {
        // A bare EACCES from fusermount3 (e.g. an AppArmor-denied prefix) is
        // otherwise opaque. Append actionable guidance — but not when the
        // allow_other preflight already produced its own self-contained message
        // (it names /etc/fuse.conf), to avoid stacking two different hints (#509).
        let add_hint = e.kind() == std::io::ErrorKind::PermissionDenied
            && !e.to_string().contains("/etc/fuse.conf");
        let err =
            anyhow::Error::new(e).context(format!("mounting at {}", args.mountpoint.display()));
        if add_hint {
            err.context(MOUNT_DENIED_HELP)
        } else {
            err
        }
    })?;
    Ok(())
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

/// Dispatch a parsed CLI invocation.
pub fn run(cli: Cli) -> Result<()> {
    match cli.command {
        Command::Scan {
            targets,
            db,
            revalidate,
            jobs,
            follow_symlinks,
            quiet,
            checksum,
            fast,
            strict,
        } => run_scan(
            &db,
            &targets,
            revalidate,
            jobs,
            follow_symlinks,
            quiet,
            checksum,
            fast,
            strict,
        ),
        Command::Mount(args) => run_mount(&args),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        }
    }

    #[test]
    fn scan_command_quiet_flag_defaults_off_and_parses() {
        use clap::Parser;
        let cli = Cli::try_parse_from(["musefs", "scan", "/m", "--db", "/tmp/x.db"]).unwrap();
        match cli.command {
            Command::Scan { quiet, .. } => assert!(!quiet),
            Command::Mount(..) => panic!("expected Scan"),
        }
        for arg in ["--quiet", "-q"] {
            let cli =
                Cli::try_parse_from(["musefs", "scan", "/m", "--db", "/tmp/x.db", arg]).unwrap();
            match cli.command {
                Command::Scan { quiet, .. } => assert!(quiet),
                Command::Mount(..) => panic!("expected Scan"),
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
        }
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
}
