//! Process-boundary coverage for the packaged `musefs` binary: clap dispatch,
//! exit codes, stderr, `--version`, `scan`/`revalidate` wiring, and the
//! `MUSEFS_*` environment-variable contract (clap's `env` feature). The
//! library-level `musefs-cli` tests call `run_scan` directly and never exercise
//! `main`'s arg-parse → error-format → exit-status contract; these do, by
//! spawning the real binary (`CARGO_BIN_EXE_musefs`). CLI flags and env vars are
//! two triggers for the same contract, so they share one spawn fixture here.
//!
//! `env_clear()` is deliberate: it guarantees no ambient `MUSEFS_*` leaks in from
//! the developer's shell, keeping the cases parallel-safe. It is safe — the
//! binary is launched by absolute path (`CARGO_BIN_EXE_musefs`), and the
//! assertions key on stderr that `main` emits via anyhow/eprintln, not through
//! env_logger, so a cleared `RUST_LOG` does not suppress it. All cases are
//! non-FUSE, so they run in the default suite without `/dev/fuse`: the children
//! fail fast at arg-parse (exit 2) or at the mount runtime's missing-db guard
//! (exit 1), never reaching a mount.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn musefs() -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_musefs"));
    // No MUSEFS_* should leak in from the developer's shell.
    cmd.env_clear();
    cmd
}

fn flac_block(block_type: u8, body: &[u8], is_last: bool) -> Vec<u8> {
    let mut out = Vec::new();
    out.push((if is_last { 0x80 } else { 0 }) | (block_type & 0x7F));
    let len = body.len();
    out.push(u8::try_from((len >> 16) & 0xFF).unwrap());
    out.push(u8::try_from((len >> 8) & 0xFF).unwrap());
    out.push(u8::try_from(len & 0xFF).unwrap());
    out.extend_from_slice(body);
    out
}

fn streaminfo_body() -> Vec<u8> {
    let mut b = vec![
        0x10, 0x00, 0x10, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x0A, 0xC4, 0x42, 0xF0, 0x00,
        0x00, 0x00, 0x00,
    ];
    b.extend_from_slice(&[0u8; 16]);
    b
}

fn vorbis_comment_body(comments: &[&str]) -> Vec<u8> {
    let vendor = "orig";
    let mut out = Vec::new();
    out.extend_from_slice(&u32::try_from(vendor.len()).unwrap().to_le_bytes());
    out.extend_from_slice(vendor.as_bytes());
    out.extend_from_slice(&u32::try_from(comments.len()).unwrap().to_le_bytes());
    for c in comments {
        out.extend_from_slice(&u32::try_from(c.len()).unwrap().to_le_bytes());
        out.extend_from_slice(c.as_bytes());
    }
    out
}

fn make_flac(comments: &[&str], audio: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(b"fLaC");
    out.extend_from_slice(&flac_block(0, &streaminfo_body(), false));
    out.extend_from_slice(&flac_block(4, &vorbis_comment_body(comments), true));
    out.extend_from_slice(audio);
    out
}

/// A directory holding one ingestible FLAC, plus the DB path to scan it into.
fn library_with_one_flac() -> (tempfile::TempDir, std::path::PathBuf, std::path::PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("library");
    std::fs::create_dir(&target).unwrap();
    std::fs::write(
        target.join("a.flac"),
        make_flac(&["ARTIST=Alice", "TITLE=Song"], &[0xAB; 32]),
    )
    .unwrap();
    let db = dir.path().join("library.db");
    (dir, target, db)
}

/// A DB path that does not exist (its parent directory is absent too), so the
/// mount runtime's missing-db guard rejects it deterministically — proving we
/// got *past* arg parsing into mount execution.
fn unopenable_db(dir: &Path, name: &str) -> PathBuf {
    dir.join("missing").join(name)
}

#[test]
fn version_flag_reports_the_package_version() {
    for flag in ["--version", "-V"] {
        let out = musefs().arg(flag).output().unwrap();
        assert!(
            out.status.success(),
            "`musefs {flag}` should exit 0, stderr: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            stdout.contains(env!("CARGO_PKG_VERSION")),
            "`musefs {flag}` should print {}, stdout: {stdout}",
            env!("CARGO_PKG_VERSION")
        );
    }
}

#[test]
fn version_flag_propagates_to_subcommands() {
    // propagate_version: `musefs scan --version` reports the same version rather
    // than erroring on an unexpected argument.
    let out = musefs().args(["scan", "--version"]).output().unwrap();
    assert!(
        out.status.success(),
        "`musefs scan --version` should exit 0, stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        String::from_utf8_lossy(&out.stdout).contains(env!("CARGO_PKG_VERSION")),
        "stdout: {}",
        String::from_utf8_lossy(&out.stdout)
    );
}

#[test]
fn scan_succeeds_and_ingests_through_the_binary() {
    let (_dir, target, db) = library_with_one_flac();
    let out = musefs()
        .arg("scan")
        .arg(&target)
        .arg("--db")
        .arg(&db)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "scan should exit 0, stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    // The DB landed at the --db path (db-path wiring) ...
    assert!(db.exists(), "scan should create the DB at --db");
    // ... and the library target was actually walked and ingested (the summary
    // reports one scanned file).
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("1 file(s)"),
        "expected one ingested file in the summary, stdout: {stdout}"
    );
}

#[test]
fn scan_with_checksum_full_exits_zero() {
    let (_dir, target, db) = library_with_one_flac();
    let out = musefs()
        .arg("scan")
        .arg(&target)
        .arg("--db")
        .arg(&db)
        .arg("--checksum")
        .arg("full")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "scan --checksum full should exit 0, stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(db.exists(), "scan should create the DB at --db");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("1 file(s)"),
        "expected one ingested file in the summary, stdout: {stdout}"
    );
}

#[test]
fn scan_with_revalidate_flag_runs_the_revalidate_pass() {
    let (_dir, target, db) = library_with_one_flac();
    // Seed the store first so revalidate has something to re-check.
    let seed = musefs()
        .arg("scan")
        .arg(&target)
        .arg("--db")
        .arg(&db)
        .output()
        .unwrap();
    assert!(seed.status.success());

    let out = musefs()
        .arg("scan")
        .arg(&target)
        .arg("--db")
        .arg(&db)
        .arg("--revalidate")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "revalidate should exit 0, stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("revalidated"),
        "the --revalidate flag should select the revalidate summary, stdout: {stdout}"
    );
}

#[test]
fn scan_missing_target_fails_with_nonzero_exit_and_stderr() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("scan.db");
    let missing = dir.path().join("does-not-exist");
    let out = musefs()
        .arg("scan")
        .arg(&missing)
        .arg("--db")
        .arg(&db)
        .output()
        .unwrap();
    // A runtime failure (not a usage error) exits 1 and explains itself on
    // stderr via `main`'s `musefs: {e:#}` formatting.
    assert_eq!(
        out.status.code(),
        Some(1),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("musefs:"), "stderr: {stderr}");
    assert!(
        stderr.contains(&missing.display().to_string()),
        "stderr should name the failing target, stderr: {stderr}"
    );
}

#[test]
fn scan_without_targets_is_a_usage_error() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("scan.db");
    let out = musefs().arg("scan").arg("--db").arg(&db).output().unwrap();
    assert_eq!(
        out.status.code(),
        Some(2),
        "missing required targets should be a usage error, stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        !db.exists(),
        "a usage error must not create the DB, it never reached run_scan"
    );
}

#[test]
fn unknown_subcommand_is_a_usage_error() {
    let out = musefs().arg("frobnicate").output().unwrap();
    assert_eq!(out.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&out.stderr).to_lowercase();
    assert!(
        stderr.contains("unrecognized") || stderr.contains("unexpected"),
        "stderr: {stderr}"
    );
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
    assert!(stdout.contains("MUSEFS_CHECKSUM"), "stdout: {stdout}");
}

// #370: the SetTrue bools parse the full boolish set from env (case-insensitive
// true/false, t/f, yes/no, y/n, on/off, 1/0), not just literal `true`/`false`.
// Each accepted value gets past clap parsing (exit != 2) and reaches the mount
// runtime's missing-db guard. The `"TRUE"` case subsumes a plain `"true"`.
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
