//! Process-boundary coverage for the packaged `musefs` binary: clap dispatch,
//! exit codes, stderr, `--version`, and `scan`/`revalidate` wiring. The
//! library-level `musefs-cli` tests call `run_scan` directly and never exercise
//! `main`'s arg-parse → error-format → exit-status contract; these do, by
//! spawning the real binary (`CARGO_BIN_EXE_musefs`). All cases are non-FUSE, so
//! they run in the default suite without `/dev/fuse`.

use std::process::Command;

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
