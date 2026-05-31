//! The `musefs` command-line interface: `scan` (ingest a backing directory into a
//! SQLite store) and `mount` (serve a read-only FUSE view of that store).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use musefs_core::{MountConfig, Musefs};
use musefs_db::Db;

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

#[derive(Parser, Debug)]
#[command(
    name = "musefs",
    about = "Read-only re-tagging FUSE view of a music library"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Walk a backing directory, ingesting FLAC/MP3 files into the SQLite store.
    Scan {
        /// Directory of backing audio files to scan recursively.
        backing_dir: PathBuf,
        /// Path to the SQLite database (created if absent).
        #[arg(long)]
        db: PathBuf,
        /// Re-validate: skip unchanged files, prune tracks whose backing file is
        /// gone, and garbage-collect orphaned art.
        #[arg(long)]
        revalidate: bool,
        /// Probe worker threads (0 = available parallelism). 1 = sequential.
        #[arg(long, default_value_t = 0)]
        jobs: usize,
    },
    /// Mount a read-only FUSE view of the store.
    Mount {
        /// Empty directory to mount at.
        mountpoint: PathBuf,
        /// Path to the SQLite database.
        #[arg(long)]
        db: PathBuf,
        /// Path template, e.g. "$albumartist/$album/$title".
        #[arg(long, default_value = "$artist/$title")]
        template: String,
        /// Fallback value substituted for any missing template field.
        #[arg(long, default_value = "Unknown")]
        default_fallback: String,
        /// How file contents are served.
        #[arg(long, value_enum, default_value_t = CliMode::Synthesis)]
        mode: CliMode,
        /// Debounce window (ms) for picking up external DB edits.
        #[arg(long, default_value_t = 1000)]
        poll_interval_ms: u64,
        /// Entry/attr cache TTL (ms) the kernel may trust before re-validating.
        /// Higher cuts lookup/getattr traffic but slows visibility of DB edits.
        #[arg(long, default_value_t = 1000)]
        attr_ttl_ms: u64,
        /// Kernel read-ahead window (KiB). Larger hides HDD/NFS latency while
        /// streaming; clamped to the kernel maximum at mount.
        #[arg(long, default_value_t = 512)]
        max_readahead_kib: u32,
        /// Max outstanding background (readahead/async) requests the kernel queues.
        #[arg(long, default_value_t = 64)]
        max_background: u16,
        /// Keep the kernel page cache across opens. External re-tags auto-invalidate
        /// the affected inodes on refresh, so cached bytes are dropped when content
        /// changes.
        #[arg(long)]
        keep_cache: bool,
    },
}

/// Open (creating/migrating) the DB at `db_path` and scan `backing_dir`. With
/// `revalidate`, run the maintenance pass (skip-unchanged, prune, GC) instead of
/// a full ingest.
pub fn run_scan(db_path: &Path, backing_dir: &Path, revalidate: bool, jobs: usize) -> Result<()> {
    let db =
        Db::open(db_path).with_context(|| format!("opening database at {}", db_path.display()))?;
    let opts = musefs_core::ScanOptions { jobs };
    if revalidate {
        let stats = musefs_core::revalidate_with(&db, backing_dir, &opts)
            .with_context(|| format!("revalidating {}", backing_dir.display()))?;
        println!(
            "revalidated: {} updated, {} unchanged, {} pruned, {} failed",
            stats.updated, stats.unchanged, stats.pruned, stats.failed
        );
    } else {
        let stats = musefs_core::scan_directory_with(&db, backing_dir, &opts)
            .with_context(|| format!("scanning {}", backing_dir.display()))?;
        println!(
            "scanned {} file(s), skipped {}, failed {}",
            stats.scanned, stats.skipped, stats.failed
        );
    }
    Ok(())
}

/// Parse mount CLI flags into `MountConfig` and `FuseConfig`. Pure function —
/// no DB access, no mounting. Exported for unit testing.
#[allow(clippy::too_many_arguments)]
pub fn parse_mount_config(
    template: String,
    default_fallback: String,
    mode: musefs_core::Mode,
    poll_interval_ms: u64,
    attr_ttl_ms: u64,
    max_readahead_kib: u32,
    max_background: u16,
    keep_cache: bool,
) -> (MountConfig, musefs_fuse::FuseConfig) {
    let config = MountConfig {
        template,
        fallbacks: BTreeMap::new(),
        default_fallback,
        mode,
        poll_interval: std::time::Duration::from_millis(poll_interval_ms),
    };
    let fuse_config = musefs_fuse::FuseConfig {
        ttl: std::time::Duration::from_millis(attr_ttl_ms),
        max_readahead: max_readahead_kib.saturating_mul(1024),
        max_background,
        keep_cache,
    };
    (config, fuse_config)
}

/// Build a `Musefs` from the DB at `db_path` and mount it (blocking) at
/// `mountpoint`.
#[allow(clippy::too_many_arguments)]
pub fn run_mount(
    db_path: &Path,
    mountpoint: &Path,
    template: String,
    default_fallback: String,
    mode: musefs_core::Mode,
    poll_interval_ms: u64,
    attr_ttl_ms: u64,
    max_readahead_kib: u32,
    max_background: u16,
    keep_cache: bool,
) -> Result<()> {
    let db =
        Db::open(db_path).with_context(|| format!("opening database at {}", db_path.display()))?;
    let (config, fuse_config) = parse_mount_config(
        template,
        default_fallback,
        mode,
        poll_interval_ms,
        attr_ttl_ms,
        max_readahead_kib,
        max_background,
        keep_cache,
    );
    let core = Musefs::open(db, config).context("building the virtual filesystem")?;
    musefs_fuse::mount_with(core, mountpoint, "musefs", fuse_config)
        .with_context(|| format!("mounting at {}", mountpoint.display()))?;
    Ok(())
}

/// Dispatch a parsed CLI invocation.
pub fn run(cli: Cli) -> Result<()> {
    match cli.command {
        Command::Scan {
            backing_dir,
            db,
            revalidate,
            jobs,
        } => run_scan(&db, &backing_dir, revalidate, jobs),
        Command::Mount {
            mountpoint,
            db,
            template,
            default_fallback,
            mode,
            poll_interval_ms,
            attr_ttl_ms,
            max_readahead_kib,
            max_background,
            keep_cache,
        } => run_mount(
            &db,
            &mountpoint,
            template,
            default_fallback,
            mode.into(),
            poll_interval_ms,
            attr_ttl_ms,
            max_readahead_kib,
            max_background,
            keep_cache,
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scan_command_parses_jobs_flag() {
        use clap::Parser;
        let cli = Cli::try_parse_from(["musefs", "scan", "/m", "--db", "/tmp/x.db", "--jobs", "3"])
            .unwrap();
        match cli.command {
            Command::Scan { jobs, .. } => assert_eq!(jobs, 3),
            Command::Mount { .. } => panic!("expected Scan"),
        }
    }
}
