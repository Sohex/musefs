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
    },
}

/// Open (creating/migrating) the DB at `db_path` and scan `backing_dir`. With
/// `revalidate`, run the maintenance pass (skip-unchanged, prune, GC) instead of
/// a full ingest.
pub fn run_scan(db_path: &Path, backing_dir: &Path, revalidate: bool) -> Result<()> {
    let db =
        Db::open(db_path).with_context(|| format!("opening database at {}", db_path.display()))?;
    if revalidate {
        let stats = musefs_core::revalidate(&db, backing_dir)
            .with_context(|| format!("revalidating {}", backing_dir.display()))?;
        println!(
            "revalidated: {} updated, {} unchanged, {} pruned",
            stats.updated, stats.unchanged, stats.pruned
        );
    } else {
        let stats = musefs_core::scan_directory(&db, backing_dir)
            .with_context(|| format!("scanning {}", backing_dir.display()))?;
        println!(
            "scanned {} file(s), skipped {}",
            stats.scanned, stats.skipped
        );
    }
    Ok(())
}

/// Build a `Musefs` from the DB at `db_path` and mount it (blocking) at
/// `mountpoint`.
pub fn run_mount(
    db_path: &Path,
    mountpoint: &Path,
    template: String,
    default_fallback: String,
    mode: musefs_core::Mode,
) -> Result<()> {
    let db =
        Db::open(db_path).with_context(|| format!("opening database at {}", db_path.display()))?;
    let config = MountConfig {
        template,
        fallbacks: BTreeMap::new(),
        default_fallback,
        mode,
    };
    let core = Musefs::open(db, config).context("building the virtual filesystem")?;
    musefs_fuse::mount(core, mountpoint, "musefs")
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
        } => run_scan(&db, &backing_dir, revalidate),
        Command::Mount {
            mountpoint,
            db,
            template,
            default_fallback,
            mode,
        } => run_mount(&db, &mountpoint, template, default_fallback, mode.into()),
    }
}
