use clap::Parser;
use musefs_cli::{Cli, Command};

#[test]
fn parses_scan_and_mount_invocations() {
    let cli = Cli::parse_from(["musefs", "scan", "/music", "--db", "/tmp/m.db"]);
    match cli.command {
        Command::Scan {
            backing_dir, db, ..
        } => {
            assert_eq!(backing_dir.to_str(), Some("/music"));
            assert_eq!(db.to_str(), Some("/tmp/m.db"));
        }
        Command::Mount { .. } => panic!("expected scan"),
    }

    let cli = Cli::parse_from([
        "musefs",
        "mount",
        "/mnt/x",
        "--db",
        "/tmp/m.db",
        "--template",
        "$album/$title",
    ]);
    match cli.command {
        Command::Mount {
            mountpoint,
            db,
            template,
            default_fallback,
            ..
        } => {
            assert_eq!(mountpoint.to_str(), Some("/mnt/x"));
            assert_eq!(db.to_str(), Some("/tmp/m.db"));
            assert_eq!(template, "$album/$title");
            assert_eq!(default_fallback, "Unknown"); // default applied
        }
        Command::Scan { .. } => panic!("expected mount"),
    }
}

#[test]
fn parses_mode_and_revalidate_flags() {
    use musefs_cli::CliMode;

    let cli = Cli::parse_from([
        "musefs",
        "mount",
        "/mnt/x",
        "--db",
        "/tmp/m.db",
        "--mode",
        "structure-only",
    ]);
    match cli.command {
        Command::Mount { mode, .. } => assert_eq!(mode, CliMode::StructureOnly),
        Command::Scan { .. } => panic!("expected mount"),
    }

    // Mode defaults to synthesis; tuning knobs have conservative defaults.
    let cli = Cli::parse_from(["musefs", "mount", "/mnt/x", "--db", "/tmp/m.db"]);
    match cli.command {
        Command::Mount {
            mode,
            poll_interval_ms,
            attr_ttl_ms,
            max_readahead_kib,
            max_background,
            keep_cache,
            ..
        } => {
            assert_eq!(mode, CliMode::Synthesis);
            assert_eq!(poll_interval_ms, 1000); // default
            assert_eq!(attr_ttl_ms, 1000); // default
            assert_eq!(max_readahead_kib, 512); // default
            assert_eq!(max_background, 64); // default
            assert!(!keep_cache); // default off
        }
        Command::Scan { .. } => panic!("expected mount"),
    }

    // Tuning flags parse to their given values.
    let cli = Cli::parse_from([
        "musefs",
        "mount",
        "/mnt/x",
        "--db",
        "/tmp/m.db",
        "--poll-interval-ms",
        "500",
        "--attr-ttl-ms",
        "2000",
        "--max-readahead-kib",
        "1024",
        "--max-background",
        "128",
        "--keep-cache",
    ]);
    match cli.command {
        Command::Mount {
            poll_interval_ms,
            attr_ttl_ms,
            max_readahead_kib,
            max_background,
            keep_cache,
            ..
        } => {
            assert_eq!(poll_interval_ms, 500);
            assert_eq!(attr_ttl_ms, 2000);
            assert_eq!(max_readahead_kib, 1024);
            assert_eq!(max_background, 128);
            assert!(keep_cache);
        }
        Command::Scan { .. } => panic!("expected mount"),
    }

    // Scan --revalidate flag.
    let cli = Cli::parse_from([
        "musefs",
        "scan",
        "/music",
        "--db",
        "/tmp/m.db",
        "--revalidate",
    ]);
    match cli.command {
        Command::Scan { revalidate, .. } => assert!(revalidate),
        Command::Mount { .. } => panic!("expected scan"),
    }
    let cli = Cli::parse_from(["musefs", "scan", "/music", "--db", "/tmp/m.db"]);
    match cli.command {
        Command::Scan { revalidate, .. } => assert!(!revalidate),
        Command::Mount { .. } => panic!("expected scan"),
    }
}
