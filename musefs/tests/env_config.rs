//! The `musefs` binary reads MUSEFS_* environment variables for scalar mount
//! and scan flags (clap's `env` feature). Each test spawns the real binary with
//! an isolated environment, so they are parallel-safe and need no /dev/fuse:
//! the children fail fast at arg-parse (exit 2) or at the mount runtime's
//! missing-db guard (exit 1), never reaching a mount.
//!
//! `env_clear()` is deliberate: it guarantees no ambient MUSEFS_* leaks in from
//! the developer's shell. It is safe here — the binary is launched by absolute
//! path (`CARGO_BIN_EXE_musefs`), and the assertions key on the
//! `database does not exist` stderr, which `main` emits via anyhow/eprintln,
//! not through env_logger — so a cleared `RUST_LOG` does not suppress it.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn musefs() -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_musefs"));
    cmd.env_clear();
    cmd
}

/// A DB path that does not exist (its parent directory is absent too), so the
/// mount runtime's missing-db guard rejects it deterministically — proving we
/// got *past* arg parsing into mount execution.
fn unopenable_db(dir: &Path, name: &str) -> PathBuf {
    dir.join("missing").join(name)
}

#[test]
fn env_satisfies_required_mount_args() {
    let dir = tempfile::tempdir().unwrap();
    let db = unopenable_db(dir.path(), "env.db");
    let out: Output = musefs()
        .arg("mount")
        .env("MUSEFS_MOUNTPOINT", dir.path())
        .env("MUSEFS_DB", &db)
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    // Not a usage error: env satisfied the required mountpoint and --db.
    assert_ne!(out.status.code(), Some(2), "stderr: {stderr}");
    // We reached the mount runtime's missing-db guard, which rejects the
    // non-existent path before any FUSE setup.
    assert!(
        stderr.contains("database does not exist"),
        "stderr: {stderr}"
    );
    assert!(
        stderr.contains(&db.display().to_string()),
        "stderr: {stderr}"
    );
}

#[test]
fn missing_required_mount_args_is_usage_error() {
    let out = musefs().arg("mount").output().unwrap();
    assert_eq!(out.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&out.stderr).to_lowercase();
    assert!(
        stderr.contains("--db") || stderr.contains("required"),
        "stderr: {stderr}"
    );
}

#[test]
fn explicit_db_flag_overrides_env_db() {
    let dir = tempfile::tempdir().unwrap();
    let env_db = unopenable_db(dir.path(), "env.db");
    let flag_db = unopenable_db(dir.path(), "flag.db");
    let out = musefs()
        .arg("mount")
        .arg(dir.path()) // positional mountpoint
        .arg("--db")
        .arg(&flag_db)
        .env("MUSEFS_DB", &env_db)
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains(&flag_db.display().to_string()),
        "expected flag db to win, stderr: {stderr}"
    );
    assert!(
        !stderr.contains(&env_db.display().to_string()),
        "env db should have been overridden, stderr: {stderr}"
    );
}

// Precedence on a value-bearing, non-required flag (the spec's --mode example).
// A bogus MUSEFS_MODE alone is rejected at parse (proves env is read); the same
// bogus env with an explicit --mode is accepted (proves the flag wins and env is
// not consulted). Observable purely via exit codes — no mount needed.
#[test]
fn invalid_mode_env_is_rejected_but_flag_overrides_it() {
    let dir = tempfile::tempdir().unwrap();
    let db = unopenable_db(dir.path(), "env.db");

    let env_only = musefs()
        .arg("mount")
        .arg(dir.path())
        .arg("--db")
        .arg(&db)
        .env("MUSEFS_MODE", "bogus")
        .output()
        .unwrap();
    assert_eq!(
        env_only.status.code(),
        Some(2),
        "bogus MUSEFS_MODE should be a usage error, stderr: {}",
        String::from_utf8_lossy(&env_only.stderr)
    );

    let flag_wins = musefs()
        .arg("mount")
        .arg(dir.path())
        .arg("--db")
        .arg(&db)
        .arg("--mode")
        .arg("synthesis")
        .env("MUSEFS_MODE", "bogus")
        .output()
        .unwrap();
    // --mode on the CLI wins; the bogus env is never parsed. We fall through to
    // the mount runtime's missing-db guard (exit 1), not a usage error.
    let stderr = String::from_utf8_lossy(&flag_wins.stderr);
    assert_ne!(flag_wins.status.code(), Some(2), "stderr: {stderr}");
    assert!(
        stderr.contains("database does not exist"),
        "stderr: {stderr}"
    );
}

// Locks the per-flag env wiring: clap's `env` feature renders `[env: NAME=]` in
// help for annotated args. Catches a dropped `env=` that the precedence tests
// might miss, and confirms the list-valued carve-out (--fallback) has no env.
#[test]
fn mount_help_lists_env_vars() {
    let out = musefs().args(["mount", "--help"]).output().unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("MUSEFS_DB"), "stdout: {stdout}");
    assert!(stdout.contains("MUSEFS_MODE"), "stdout: {stdout}");
    assert!(stdout.contains("MUSEFS_MOUNTPOINT"), "stdout: {stdout}");
    assert!(
        !stdout.contains("MUSEFS_FALLBACK"),
        "--fallback is flag-only and must not advertise an env var, stdout: {stdout}"
    );
}

#[test]
fn invalid_boolean_env_is_usage_error() {
    let dir = tempfile::tempdir().unwrap();
    let db = unopenable_db(dir.path(), "env.db");
    let out = musefs()
        .arg("mount")
        .arg(dir.path())
        .arg("--db")
        .arg(&db)
        .env("MUSEFS_KEEP_CACHE", "enabled") // not a boolish value
        .output()
        .unwrap();
    // Hard error at parse time, not a silent false — and pinned to the boolean
    // parse failure, not any exit-2.
    assert_eq!(
        out.status.code(),
        Some(2),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8_lossy(&out.stderr).to_lowercase();
    assert!(
        stderr.contains("keep-cache") || stderr.contains("invalid value"),
        "expected a keep-cache boolean parse error, stderr: {stderr}"
    );
}

#[test]
fn valid_boolean_env_is_accepted() {
    let dir = tempfile::tempdir().unwrap();
    let db = unopenable_db(dir.path(), "env.db");
    let out = musefs()
        .arg("mount")
        .arg(dir.path())
        .arg("--db")
        .arg(&db)
        .env("MUSEFS_KEEP_CACHE", "true")
        .output()
        .unwrap();
    // Got past parse for the right reason (reached the mount runtime's
    // missing-db guard), not merely "not exit 2". Proves a valid boolish env
    // value is accepted, not silently dropped.
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_ne!(out.status.code(), Some(2), "stderr: {stderr}");
    assert!(
        stderr.contains("database does not exist"),
        "stderr: {stderr}"
    );
}

#[test]
fn scan_reads_db_from_env() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("library");
    std::fs::create_dir(&target).unwrap();
    let db = dir.path().join("scan-env.db");
    let out = musefs()
        .arg("scan")
        .arg(&target) // targets stay command-line only
        .env("MUSEFS_DB", &db)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        db.exists(),
        "scan should create the DB at the MUSEFS_DB path"
    );
}

#[test]
fn scan_help_lists_env_vars() {
    let out = musefs().args(["scan", "--help"]).output().unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("MUSEFS_DB"), "stdout: {stdout}");
    assert!(stdout.contains("MUSEFS_JOBS"), "stdout: {stdout}");
}

// #370: the SetTrue bools parse the full boolish set from env (case-insensitive
// true/false, t/f, yes/no, y/n, on/off, 1/0), not just literal `true`/`false`.
// Each accepted value gets past clap parsing (exit != 2) and reaches the mount
// runtime's missing-db guard.
#[test]
fn boolish_boolean_env_values_are_accepted() {
    let dir = tempfile::tempdir().unwrap();
    let db = unopenable_db(dir.path(), "env.db");
    for val in ["1", "0", "yes", "no", "on", "off", "t", "f", "TRUE", "Off"] {
        let out = musefs()
            .arg("mount")
            .arg(dir.path())
            .arg("--db")
            .arg(&db)
            .env("MUSEFS_KEEP_CACHE", val)
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert_ne!(
            out.status.code(),
            Some(2),
            "MUSEFS_KEEP_CACHE={val} should parse, stderr: {stderr}"
        );
        assert!(
            stderr.contains("database does not exist"),
            "MUSEFS_KEEP_CACHE={val} should reach the missing-db guard, stderr: {stderr}"
        );
    }
}

// #370: a boolish MUSEFS_REVALIDATE actually flips scan into its revalidate
// pass (observable on stdout), proving the parsed bool reaches run_scan — not
// merely that parsing succeeds.
#[test]
fn boolish_revalidate_env_selects_the_revalidate_pass() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("library");
    std::fs::create_dir(&target).unwrap();
    let db = dir.path().join("reval.db");

    let out = musefs()
        .arg("scan")
        .arg(&target)
        .env("MUSEFS_DB", &db)
        .env("MUSEFS_REVALIDATE", "on")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("revalidated"), "stdout: {stdout}");

    // Default (no env) takes the full-ingest path.
    let out = musefs()
        .arg("scan")
        .arg(&target)
        .env("MUSEFS_DB", &db)
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("scanned"), "stdout: {stdout}");
}

// #370: a boolish MUSEFS_QUIET (1/0) is honoured — `1` suppresses the summary,
// `0` keeps it. The bare-`bool` parser would reject `0` outright, so this only
// passes once BoolishValueParser is attached.
#[test]
fn boolish_quiet_env_toggles_the_summary() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("library");
    std::fs::create_dir(&target).unwrap();
    let db = dir.path().join("quiet.db");

    let out = musefs()
        .arg("scan")
        .arg(&target)
        .env("MUSEFS_DB", &db)
        .env("MUSEFS_QUIET", "1")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        String::from_utf8_lossy(&out.stdout).trim().is_empty(),
        "MUSEFS_QUIET=1 should suppress the summary, stdout: {}",
        String::from_utf8_lossy(&out.stdout)
    );

    let out = musefs()
        .arg("scan")
        .arg(&target)
        .env("MUSEFS_DB", &db)
        .env("MUSEFS_QUIET", "0")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("scanned"),
        "MUSEFS_QUIET=0 should keep the summary, stdout: {}",
        String::from_utf8_lossy(&out.stdout)
    );
}
