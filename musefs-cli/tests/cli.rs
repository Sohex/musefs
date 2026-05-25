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
        _ => panic!("expected scan"),
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
        _ => panic!("expected mount"),
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
        _ => panic!("expected mount"),
    }

    // Mode defaults to synthesis.
    let cli = Cli::parse_from(["musefs", "mount", "/mnt/x", "--db", "/tmp/m.db"]);
    match cli.command {
        Command::Mount { mode, .. } => assert_eq!(mode, CliMode::Synthesis),
        _ => panic!("expected mount"),
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
        _ => panic!("expected scan"),
    }
    let cli = Cli::parse_from(["musefs", "scan", "/music", "--db", "/tmp/m.db"]);
    match cli.command {
        Command::Scan { revalidate, .. } => assert!(!revalidate),
        _ => panic!("expected scan"),
    }
}
