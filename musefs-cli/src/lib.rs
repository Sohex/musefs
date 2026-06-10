//! The `musefs` command-line interface: `scan` (ingest a backing directory into a
//! SQLite store) and `mount` (serve a read-only FUSE view of that store).

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use musefs_core::{MountConfig, Musefs};
use musefs_db::Db;

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

#[derive(Parser, Debug)]
#[command(
    name = "musefs",
    about = "Read-only re-tagging FUSE view of a music library"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

/// Flags for `musefs mount`, grouped so the mount plumbing passes one value
/// instead of ten ordering-fragile positional parameters.
#[derive(clap::Args, Debug)]
pub struct MountArgs {
    /// Empty directory to mount at.
    pub mountpoint: PathBuf,
    /// Path to the SQLite database.
    #[arg(long)]
    pub db: PathBuf,
    /// Path template, e.g. "$albumartist/$album/$title". Supports ${a|b}
    /// fallback chains, [...] conditional sections ($[/$] for literal
    /// brackets), and $!{field} path fields that keep '/' as separators.
    #[arg(long, default_value = "$artist/$title")]
    pub template: String,
    /// Fallback value substituted for any missing template field.
    #[arg(long, default_value = "Unknown")]
    pub default_fallback: String,
    /// Per-field fallback `FIELD=VALUE`, overriding `--default-fallback` for
    /// just that field when it is missing. Repeatable, e.g. `--fallback
    /// albumartist="Unknown Artist" --fallback genre=Misc`.
    #[arg(long = "fallback", value_name = "FIELD=VALUE", value_parser = parse_fallback)]
    pub fallbacks: Vec<(String, String)>,
    /// How file contents are served.
    #[arg(long, value_enum, default_value_t = CliMode::Synthesis)]
    pub mode: CliMode,
    /// Debounce window (ms) for picking up external DB edits.
    #[arg(long, default_value_t = 1000)]
    pub poll_interval_ms: u64,
    /// Entry/attr cache TTL (ms) the kernel may trust before re-validating.
    /// Higher cuts lookup/getattr traffic but slows visibility of DB edits.
    #[arg(long, default_value_t = 1000)]
    pub attr_ttl_ms: u64,
    /// Kernel read-ahead window (KiB). Larger hides HDD/NFS latency while
    /// streaming; clamped to the kernel maximum at mount.
    #[arg(long, default_value_t = 512)]
    pub max_readahead_kib: u32,
    /// Max outstanding background (readahead/async) requests the kernel queues.
    #[arg(long, default_value_t = 64)]
    pub max_background: u16,
    /// Keep the kernel page cache across opens. External re-tags auto-invalidate
    /// the affected inodes on refresh, so cached bytes are dropped when content
    /// changes.
    #[arg(long)]
    pub keep_cache: bool,
    /// Compare filenames case-insensitively: case-variant directories merge and
    /// case-variant files are disambiguated. Defaults to true on macOS (whose
    /// volumes are usually case-insensitive), false on Linux/FreeBSD. Override
    /// with `--case-insensitive false` (e.g. a case-sensitive APFS volume).
    #[arg(long, default_value_t = cfg!(target_os = "macos"), action = clap::ArgAction::Set)]
    pub case_insensitive: bool,
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
        #[arg(long)]
        db: PathBuf,
        /// Re-validate: skip unchanged files, prune tracks whose backing file is
        /// gone, and garbage-collect orphaned art.
        #[arg(long)]
        revalidate: bool,
        /// Probe worker threads (0 = available parallelism). 1 = sequential.
        #[arg(long, default_value_t = 0)]
        jobs: usize,
        /// Follow symlinks while walking directories. Off by default: symlinked
        /// files and directories are logged and skipped.
        #[arg(long)]
        follow_symlinks: bool,
        /// Suppress the per-target summary on stdout (failures still surface via
        /// the `log` facade on stderr; raise detail with `RUST_LOG=info`).
        #[arg(long, short)]
        quiet: bool,
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
pub fn run_scan(
    db_path: &Path,
    targets: &[PathBuf],
    revalidate: bool,
    jobs: usize,
    follow_symlinks: bool,
    quiet: bool,
) -> Result<()> {
    let db =
        Db::open(db_path).with_context(|| format!("opening database at {}", db_path.display()))?;
    let opts = musefs_core::ScanOptions {
        jobs,
        follow_symlinks,
        ..Default::default()
    };
    for target in targets {
        if revalidate {
            let stats = musefs_core::revalidate_with(&db, target, &opts)
                .with_context(|| format!("revalidating {}", target.display()))?;
            if !quiet {
                println!(
                    "revalidated {}: {} updated, {} unchanged, {} pruned, {} failed",
                    target.display(),
                    stats.updated,
                    stats.unchanged,
                    stats.pruned,
                    stats.failed
                );
            }
        } else {
            let stats = musefs_core::scan_directory_with(&db, target, &opts)
                .with_context(|| format!("scanning {}", target.display()))?;
            if !quiet {
                println!(
                    "scanned {}: {} file(s), skipped {}, failed {}",
                    target.display(),
                    stats.scanned,
                    stats.skipped,
                    stats.failed
                );
            }
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

/// Parse mount CLI flags into `MountConfig` and `FuseConfig`. Pure function —
/// no DB access, no mounting. Exported for unit testing.
pub fn parse_mount_config(args: &MountArgs) -> (MountConfig, musefs_fuse::FuseConfig) {
    let config = MountConfig {
        template: args.template.clone(),
        fallbacks: args.fallbacks.iter().cloned().collect(),
        default_fallback: args.default_fallback.clone(),
        mode: args.mode.into(),
        poll_interval: std::time::Duration::from_millis(args.poll_interval_ms),
        case_insensitive: args.case_insensitive,
    };
    let fuse_config = musefs_fuse::FuseConfig {
        ttl: std::time::Duration::from_millis(args.attr_ttl_ms),
        max_readahead: args.max_readahead_kib.saturating_mul(1024),
        max_background: args.max_background,
        keep_cache: args.keep_cache,
    };
    (config, fuse_config)
}

/// Build a `Musefs` from the DB at `args.db` and mount it (blocking) at
/// `args.mountpoint`.
pub fn run_mount(args: &MountArgs) -> Result<()> {
    let db =
        Db::open(&args.db).with_context(|| format!("opening database at {}", args.db.display()))?;
    let (config, fuse_config) = parse_mount_config(args);
    let core = Musefs::open(db, config).context("building the virtual filesystem")?;
    signal::install_unmount_on_signal(args.mountpoint.clone())
        .context("installing the stop-signal unmount handler")?;
    musefs_fuse::mount_with(core, &args.mountpoint, "musefs", fuse_config)
        .with_context(|| format!("mounting at {}", args.mountpoint.display()))?;
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
        } => run_scan(&db, &targets, revalidate, jobs, follow_symlinks, quiet),
        Command::Mount(args) => run_mount(&args),
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
        ])
        .unwrap();
        let Command::Mount(args) = cli.command else {
            panic!("expected Mount");
        };
        let (config, fuse_config) = parse_mount_config(&args);
        // Defaults survive the move into the struct.
        assert_eq!(config.template, "$artist/$title");
        assert_eq!(config.default_fallback, "Unknown");
        assert_eq!(config.mode, musefs_core::Mode::Synthesis);
        assert!(!fuse_config.keep_cache);
        assert_eq!(config.case_insensitive, cfg!(target_os = "macos"));
        // ms → Duration.
        assert_eq!(config.poll_interval, std::time::Duration::from_millis(250));
        assert_eq!(fuse_config.ttl, std::time::Duration::from_millis(750));
        // KiB → bytes.
        assert_eq!(fuse_config.max_readahead, 64 * 1024);
        assert_eq!(fuse_config.max_background, 32);
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
        for (val, want) in [("true", true), ("false", false)] {
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
}
