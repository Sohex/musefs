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
        _ => unreachable!(),
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
            assert_eq!(
                args.mountpoint.as_deref().and_then(|p| p.to_str()),
                Some("/mnt/x")
            );
            assert_eq!(args.db.to_str(), Some("/tmp/m.db"));
            assert_eq!(args.template, "$album/$title");
            assert_eq!(args.default_fallback, "Unknown"); // default applied
        }
        Command::Scan { .. } => panic!("expected mount"),
        _ => unreachable!(),
    }
}

#[test]
fn parses_mode_and_tuning_flags() {
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
        _ => unreachable!(),
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
            assert!(args.keep_cache); // #432: default on
        }
        Command::Scan { .. } => panic!("expected mount"),
        _ => unreachable!(),
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
        _ => unreachable!(),
    }
}

#[test]
fn scan_parses_checksum_and_strictness_flags() {
    let cli = Cli::parse_from([
        "musefs",
        "scan",
        "/lib",
        "--db",
        "/tmp/m.db",
        "--checksum",
        "full",
        "--match",
        "strict",
    ]);
    match cli.command {
        Command::Scan {
            checksum,
            match_mode,
            ..
        } => {
            assert_eq!(checksum, musefs_cli::ChecksumMode::Full);
            assert_eq!(match_mode, musefs_cli::MatchMode::Strict);
        }
        Command::Mount(..) => panic!("expected scan"),
        _ => unreachable!(),
    }

    // Unset is `auto`, the escalating default.
    let cli = Cli::parse_from(["musefs", "scan", "/lib", "--db", "/tmp/m.db"]);
    let Command::Scan { match_mode, .. } = cli.command else {
        panic!("expected scan");
    };
    assert_eq!(match_mode, musefs_cli::MatchMode::Auto);
}

#[test]
fn scan_help_lists_m4b_format() {
    use clap::CommandFactory;
    let cmd = Cli::command();
    let scan = cmd
        .get_subcommands()
        .find(|c| c.get_name() == "scan")
        .expect("scan subcommand exists");
    let about = scan.get_about().expect("scan has help text").to_string();
    assert!(
        about.contains("M4B"),
        "scan help should advertise M4B; got: {about}"
    );
}

#[test]
fn dry_run_does_not_require_a_mountpoint() {
    let cli = Cli::try_parse_from(["musefs", "mount", "--db", "/tmp/m.db", "--dry-run"])
        .expect("dry-run should parse without a mountpoint");
    match cli.command {
        Command::Mount(args) => {
            assert!(args.dry_run);
            assert_eq!(args.mountpoint, None);
        }
        Command::Scan { .. } => panic!("expected mount"),
        _ => unreachable!(),
    }
}

#[test]
fn mount_without_dry_run_still_requires_a_mountpoint() {
    let err = Cli::try_parse_from(["musefs", "mount", "--db", "/tmp/m.db"])
        .expect_err("a real mount must demand a mountpoint");
    assert_eq!(err.kind(), clap::error::ErrorKind::MissingRequiredArgument);
}

#[test]
fn boolish_mount_flags_work_as_bare_switches() {
    let cli = Cli::try_parse_from([
        "musefs",
        "mount",
        "/mnt/x",
        "--db",
        "/tmp/m.db",
        "--skip-on-missing",
        "--read-ahead-prefetch",
    ])
    .expect("bare boolish switches should still parse");
    match cli.command {
        Command::Mount(args) => {
            assert!(args.skip_on_missing);
            assert!(args.read_ahead_prefetch);
        }
        Command::Scan { .. } => panic!("expected mount"),
        _ => unreachable!(),
    }
}

use musefs_cli::parse_mount_config;
use musefs_core::Mode;
use std::time::Duration;

/// Mount args as the parser produces them from `flags`, after a fixed mountpoint
/// and store. Going through the parser keeps these tests on the flags a user
/// actually types; `MountArgs` cannot be built with a literal outside its crate.
fn mount_args(flags: &[&str]) -> MountArgs {
    let mut argv = vec!["musefs", "mount", "/mnt/x", "--db", "/tmp/x.db"];
    argv.extend_from_slice(flags);
    let Command::Mount(args) = Cli::parse_from(argv).command else {
        panic!("expected mount");
    };
    args
}

#[test]
fn parse_mount_config_defaults_are_sensible() {
    let args = mount_args(&[
        "--template",
        "$artist/$title",
        "--keep-cache",
        "false",
        "--case-insensitive",
        "false",
    ]);
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
    let args = mount_args(&[
        "--template",
        "$title",
        "--mode",
        "structure-only",
        "--poll-interval-ms",
        "250",
        "--attr-ttl-ms",
        "5000",
        "--max-readahead-kib",
        "256",
        "--max-background",
        "32",
        "--case-insensitive",
        "false",
    ]);
    let (config, fuse_config) = parse_mount_config(&args);
    assert_eq!(config.mode, Mode::StructureOnly);
    assert_eq!(config.poll_interval, Duration::from_millis(250));
    assert!(fuse_config.keep_cache);
    assert_eq!(fuse_config.ttl, Duration::from_secs(5));
    assert_eq!(fuse_config.max_background, 32);
}

#[test]
fn parse_mount_config_saturating_readahead() {
    let args = mount_args(&[
        "--template",
        "$title",
        "--max-readahead-kib",
        &u32::MAX.to_string(),
        "--keep-cache",
        "false",
        "--case-insensitive",
        "false",
    ]);
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
        _ => unreachable!(),
    }
}

#[test]
fn parse_mount_config_populates_per_field_fallbacks() {
    let args = mount_args(&[
        "--template",
        "$albumartist/$title",
        "--fallback",
        "albumartist=Unknown Artist",
        "--fallback",
        "genre=Misc",
        "--keep-cache",
        "false",
        "--case-insensitive",
        "false",
    ]);
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
fn fallback_keys_are_lowercased_to_match_template_fields() {
    // Regression for #504: template field names are case-insensitive (the
    // parser lowercases `$AlbumArtist` to `albumartist`), so a fallback keyed
    // with uppercase letters must be lowercased too or it could never match.
    let cli = Cli::parse_from([
        "musefs",
        "mount",
        "/mnt/x",
        "--db",
        "/tmp/m.db",
        "--fallback",
        "AlbumArtist=Unknown Artist",
        "--fallback",
        "GENRE=Misc",
    ]);
    let args = match cli.command {
        Command::Mount(args) => args,
        Command::Scan { .. } => panic!("expected mount"),
        _ => unreachable!(),
    };
    let (config, _) = parse_mount_config(&args);
    assert_eq!(
        config.fallbacks.get("albumartist").map(String::as_str),
        Some("Unknown Artist"),
        "uppercase fallback key must normalize to the lowercased field name"
    );
    assert_eq!(
        config.fallbacks.get("genre").map(String::as_str),
        Some("Misc")
    );
    // The verbatim uppercase key must NOT be present.
    assert!(!config.fallbacks.contains_key("AlbumArtist"));
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
        _ => unreachable!(),
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
        _ => unreachable!(),
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

/// `MountConfig::default()` and `FuseConfig::default()` promise to be what a bare
/// `musefs mount` parses to, so code outside their crates can start from them and
/// assign only what it changes. Hold both to that, field by field.
#[test]
fn config_defaults_are_what_a_bare_mount_parses_to() {
    let (config, fuse_config) = parse_mount_config(&mount_args(&[]));

    let default = musefs_core::MountConfig::default();
    assert_eq!(config.template, default.template);
    assert_eq!(config.fallbacks, default.fallbacks);
    assert_eq!(config.default_fallback, default.default_fallback);
    assert_eq!(config.mode, default.mode);
    assert_eq!(config.poll_interval, default.poll_interval);
    assert_eq!(config.case_insensitive, default.case_insensitive);
    assert_eq!(config.read_ahead_budget, default.read_ahead_budget);
    assert_eq!(config.read_ahead_prefetch, default.read_ahead_prefetch);
    assert_eq!(config.skip_on_missing, default.skip_on_missing);
    assert_eq!(config.trust_backing_mtime, default.trust_backing_mtime);

    let default = musefs_fuse::FuseConfig::default();
    assert_eq!(fuse_config.ttl, default.ttl);
    assert_eq!(fuse_config.max_readahead, default.max_readahead);
    assert_eq!(fuse_config.max_background, default.max_background);
    assert_eq!(fuse_config.keep_cache, default.keep_cache);
    assert_eq!(fuse_config.uid, default.uid);
    assert_eq!(fuse_config.gid, default.gid);
    assert_eq!(fuse_config.file_mode, default.file_mode);
    assert_eq!(fuse_config.dir_mode, default.dir_mode);
    assert_eq!(fuse_config.allow_other, default.allow_other);
    assert_eq!(fuse_config.expose_metrics, default.expose_metrics);
    assert_eq!(fuse_config.workers, default.workers);
}
