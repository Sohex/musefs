use clap::Parser;
use musefs_cli::{Cli, Command, MountArgs};

#[test]
fn parses_scan_and_mount_invocations() {
    let cli = Cli::parse_from(["musefs", "scan", "/music", "--db", "/tmp/m.db"]);
    match cli.command {
        Command::Scan { targets, db, .. } => {
            assert_eq!(targets, vec![std::path::PathBuf::from("/music")]);
            assert_eq!(db.to_str(), Some("/tmp/m.db"));
        }
        Command::Mount(..) => panic!("expected scan"),
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
        Command::Mount(args) => {
            assert_eq!(args.mountpoint.to_str(), Some("/mnt/x"));
            assert_eq!(args.db.to_str(), Some("/tmp/m.db"));
            assert_eq!(args.template, "$album/$title");
            assert_eq!(args.default_fallback, "Unknown"); // default applied
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
        Command::Mount(args) => assert_eq!(args.mode, CliMode::StructureOnly),
        Command::Scan { .. } => panic!("expected mount"),
    }

    // Mode defaults to synthesis; tuning knobs have conservative defaults.
    let cli = Cli::parse_from(["musefs", "mount", "/mnt/x", "--db", "/tmp/m.db"]);
    match cli.command {
        Command::Mount(args) => {
            assert_eq!(args.mode, CliMode::Synthesis);
            assert_eq!(args.poll_interval_ms, 1000); // default
            assert_eq!(args.attr_ttl_ms, 1000); // default
            assert_eq!(args.max_readahead_kib, 512); // default
            assert_eq!(args.max_background, 64); // default
            assert!(!args.keep_cache); // default off
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
        Command::Mount(args) => {
            assert_eq!(args.poll_interval_ms, 500);
            assert_eq!(args.attr_ttl_ms, 2000);
            assert_eq!(args.max_readahead_kib, 1024);
            assert_eq!(args.max_background, 128);
            assert!(args.keep_cache);
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
        Command::Mount(..) => panic!("expected scan"),
    }
    let cli = Cli::parse_from(["musefs", "scan", "/music", "--db", "/tmp/m.db"]);
    match cli.command {
        Command::Scan { revalidate, .. } => assert!(!revalidate),
        Command::Mount(..) => panic!("expected scan"),
    }
}

use musefs_cli::parse_mount_config;
use musefs_core::Mode;
use std::time::Duration;

#[test]
fn parse_mount_config_defaults_are_sensible() {
    let args = MountArgs {
        mountpoint: "/mnt/x".into(),
        db: "/tmp/x.db".into(),
        template: "$artist/$title".to_string(),
        default_fallback: "Unknown".to_string(),
        fallbacks: vec![],
        mode: musefs_cli::CliMode::Synthesis,
        poll_interval_ms: 1000,
        attr_ttl_ms: 1000,
        max_readahead_kib: 512,
        max_background: 64,
        keep_cache: false,
        case_insensitive: false,
        owner: None,
        group: None,
        file_mode: None,
        dir_mode: None,
        allow_other: false,
        read_ahead_budget_mib: 64,
        read_ahead_prefetch: false,
    };
    let (config, fuse_config) = parse_mount_config(&args);
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
    let args = MountArgs {
        mountpoint: "/mnt/x".into(),
        db: "/tmp/x.db".into(),
        template: "$title".to_string(),
        default_fallback: "Unknown".to_string(),
        fallbacks: vec![],
        mode: musefs_cli::CliMode::StructureOnly,
        poll_interval_ms: 250,
        attr_ttl_ms: 5000,
        max_readahead_kib: 256,
        max_background: 32,
        keep_cache: true,
        case_insensitive: false,
        owner: None,
        group: None,
        file_mode: None,
        dir_mode: None,
        allow_other: false,
        read_ahead_budget_mib: 64,
        read_ahead_prefetch: false,
    };
    let (config, fuse_config) = parse_mount_config(&args);
    assert_eq!(config.mode, Mode::StructureOnly);
    assert_eq!(config.poll_interval, Duration::from_millis(250));
    assert!(fuse_config.keep_cache);
    assert_eq!(fuse_config.ttl, Duration::from_secs(5));
    assert_eq!(fuse_config.max_background, 32);
}

#[test]
fn parse_mount_config_saturating_readahead() {
    let args = MountArgs {
        mountpoint: "/mnt/x".into(),
        db: "/tmp/x.db".into(),
        template: "$title".to_string(),
        default_fallback: "Unknown".to_string(),
        fallbacks: vec![],
        mode: musefs_cli::CliMode::Synthesis,
        poll_interval_ms: 1000,
        attr_ttl_ms: 1000,
        max_readahead_kib: u32::MAX,
        max_background: 64,
        keep_cache: false,
        case_insensitive: false,
        owner: None,
        group: None,
        file_mode: None,
        dir_mode: None,
        allow_other: false,
        read_ahead_budget_mib: 64,
        read_ahead_prefetch: false,
    };
    let (_, fuse_config) = parse_mount_config(&args);
    assert_eq!(fuse_config.max_readahead, u32::MAX);
}

#[test]
fn parses_repeatable_fallback_flag() {
    let cli = Cli::parse_from([
        "musefs",
        "mount",
        "/mnt/x",
        "--db",
        "/tmp/m.db",
        "--fallback",
        "albumartist=Unknown Artist",
        "--fallback",
        "genre=Misc",
    ]);
    match cli.command {
        Command::Mount(args) => assert_eq!(
            args.fallbacks,
            vec![
                ("albumartist".to_string(), "Unknown Artist".to_string()),
                ("genre".to_string(), "Misc".to_string()),
            ]
        ),
        Command::Scan { .. } => panic!("expected mount"),
    }
}

#[test]
fn parse_mount_config_populates_per_field_fallbacks() {
    let args = MountArgs {
        mountpoint: "/mnt/x".into(),
        db: "/tmp/x.db".into(),
        template: "$albumartist/$title".to_string(),
        default_fallback: "Unknown".to_string(),
        fallbacks: vec![
            ("albumartist".to_string(), "Unknown Artist".to_string()),
            ("genre".to_string(), "Misc".to_string()),
        ],
        mode: musefs_cli::CliMode::Synthesis,
        poll_interval_ms: 1000,
        attr_ttl_ms: 1000,
        max_readahead_kib: 512,
        max_background: 64,
        keep_cache: false,
        case_insensitive: false,
        owner: None,
        group: None,
        file_mode: None,
        dir_mode: None,
        allow_other: false,
        read_ahead_budget_mib: 64,
        read_ahead_prefetch: false,
    };
    let (config, _) = parse_mount_config(&args);
    assert_eq!(
        config.fallbacks.get("albumartist").map(String::as_str),
        Some("Unknown Artist")
    );
    assert_eq!(
        config.fallbacks.get("genre").map(String::as_str),
        Some("Misc")
    );
}

#[test]
fn fallback_value_may_contain_equals_and_last_duplicate_wins() {
    let cli = Cli::parse_from([
        "musefs",
        "mount",
        "/mnt/x",
        "--db",
        "/tmp/m.db",
        "--fallback",
        "comment=a=b",
        "--fallback",
        "artist=first",
        "--fallback",
        "artist=second",
    ]);
    let args = match cli.command {
        Command::Mount(args) => args,
        Command::Scan { .. } => panic!("expected mount"),
    };
    // Only the first '=' separates; the value keeps the rest verbatim.
    assert_eq!(
        args.fallbacks[0],
        ("comment".to_string(), "a=b".to_string())
    );
    let (config, _) = parse_mount_config(&args);
    // Duplicate field: the last value wins in the resulting map.
    assert_eq!(
        config.fallbacks.get("artist").map(String::as_str),
        Some("second")
    );
    assert_eq!(
        config.fallbacks.get("comment").map(String::as_str),
        Some("a=b")
    );
}

#[test]
fn fallback_without_equals_is_rejected() {
    let err = Cli::try_parse_from([
        "musefs",
        "mount",
        "/mnt/x",
        "--db",
        "/tmp/m.db",
        "--fallback",
        "noequals",
    ])
    .unwrap_err();
    assert!(err.to_string().contains("FIELD=VALUE"), "{err}");
}

#[test]
fn mount_fails_on_missing_db_without_creating_it() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("missing.db");
    let mount_dir = tempfile::tempdir().unwrap();

    let cli = Cli::parse_from([
        "musefs",
        "mount",
        mount_dir.path().to_str().unwrap(),
        "--db",
        db_path.to_str().unwrap(),
    ]);
    let args = match cli.command {
        Command::Mount(args) => args,
        Command::Scan { .. } => panic!("expected mount"),
    };

    let err = musefs_cli::run_mount(&args).unwrap_err();
    assert!(
        err.to_string().contains("does not exist"),
        "expected a missing-db error, got: {err}"
    );
    assert!(
        !db_path.exists(),
        "mount must not create the database when it is absent"
    );
}
