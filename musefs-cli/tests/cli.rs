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

use musefs_cli::parse_mount_config;
use musefs_core::Mode;
use std::time::Duration;

#[test]
fn parse_mount_config_defaults_are_sensible() {
    let (config, fuse_config) = parse_mount_config(
        "$artist/$title".to_string(),
        "Unknown".to_string(),
        Mode::Synthesis,
        1000,
        1000,
        512,
        64,
        false,
    );
    assert_eq!(config.template, "$artist/$title");
    assert_eq!(config.default_fallback, "Unknown");
    assert_eq!(config.mode, Mode::Synthesis);
    assert_eq!(config.poll_interval, Duration::from_secs(1));
    assert!(config.fallbacks.is_empty());
    assert!(!fuse_config.keep_cache);
    assert_eq!(fuse_config.ttl, Duration::from_secs(1));
    assert_eq!(fuse_config.max_readahead, 512 * 1024);
    assert_eq!(fuse_config.max_background, 64);
}

#[test]
fn parse_mount_config_keep_cache_sets_flag() {
    let (config, fuse_config) = parse_mount_config(
        "$title".to_string(),
        "Unknown".to_string(),
        Mode::StructureOnly,
        250,
        5000,
        256,
        32,
        true,
    );
    assert_eq!(config.mode, Mode::StructureOnly);
    assert_eq!(config.poll_interval, Duration::from_millis(250));
    assert!(fuse_config.keep_cache);
    assert_eq!(fuse_config.ttl, Duration::from_secs(5));
    assert_eq!(fuse_config.max_background, 32);
}

#[test]
fn parse_mount_config_saturating_readahead() {
    let (_, fuse_config) = parse_mount_config(
        "$title".to_string(),
        "Unknown".to_string(),
        Mode::Synthesis,
        1000,
        1000,
        u32::MAX,
        64,
        false,
    );
    assert_eq!(fuse_config.max_readahead, u32::MAX);
}
