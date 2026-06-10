//! The `musefs` binary reads MUSEFS_* environment variables for scalar mount
//! and scan flags (clap's `env` feature). Each test spawns the real binary with
//! an isolated environment, so they are parallel-safe and need no /dev/fuse:
//! the children fail fast at arg-parse (exit 2) or DB-open (exit 1), never
//! reaching a mount.
//!
//! `env_clear()` is deliberate: it guarantees no ambient MUSEFS_* leaks in from
//! the developer's shell. It is safe here — the binary is launched by absolute
//! path (`CARGO_BIN_EXE_musefs`), and the assertions key on the
//! `opening database at <path>` stderr, which `main` emits via anyhow/eprintln,
//! not through env_logger — so a cleared `RUST_LOG` does not suppress it.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn musefs() -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_musefs"));
    cmd.env_clear();
    cmd
}

/// A DB path whose parent directory does not exist, so opening it fails
/// deterministically — proving we got *past* arg parsing to the DB-open step.
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
    // We reached DB-open, which fails on the missing parent directory.
    assert!(stderr.contains("opening database"), "stderr: {stderr}");
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
    // DB-open (exit 1), not a usage error.
    let stderr = String::from_utf8_lossy(&flag_wins.stderr);
    assert_ne!(flag_wins.status.code(), Some(2), "stderr: {stderr}");
    assert!(stderr.contains("opening database"), "stderr: {stderr}");
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
