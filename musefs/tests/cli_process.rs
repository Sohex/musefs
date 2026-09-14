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
fn scan_with_a_failing_file_exits_two() {
    // A per-file ingest failure does not abort the batch, but it must surface as a
    // non-zero exit so a pipeline like `scan && mount` can detect partial/total
    // ingest failure. Exit 2 is the chosen signal (#554).
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("library");
    std::fs::create_dir(&target).unwrap();
    std::fs::write(
        target.join("good.flac"),
        make_flac(&["TITLE=Good"], &[0xAB; 32]),
    )
    .unwrap();
    std::fs::write(target.join("bad.flac"), b"not a flac at all").unwrap();
    let db = dir.path().join("library.db");

    let out = musefs()
        .arg("scan")
        .arg(&target)
        .arg("--db")
        .arg(&db)
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(2),
        "a scan with a failing file should exit 2, stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    // The good file still ingested; the failure is a signal, not a rollback.
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("failed 1"),
        "expected the summary to report one failure, stdout: {stdout}"
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

/// #707: the alias deprecated in 1.2.0 is gone, and asking for it is a usage
/// error like any other unknown flag — not a scan that quietly ran instead.
#[test]
fn scan_revalidate_flag_is_a_usage_error() {
    let (_dir, target, db) = library_with_one_flac();
    let out = musefs()
        .arg("scan")
        .arg(&target)
        .arg("--db")
        .arg(&db)
        .arg("--revalidate")
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(2), "stderr: {stderr}");
    assert!(stderr.contains("--revalidate"), "stderr: {stderr}");
    assert!(
        !db.exists(),
        "a refused scan must not have created the store"
    );
}

/// #707: clap never reads a variable no flag declares, so the retired one is
/// refused explicitly. Ignoring it would turn a unit file's revalidate into a
/// full scan without a word.
#[test]
fn retired_revalidate_env_is_refused() {
    let (_dir, target, db) = library_with_one_flac();
    // Any non-empty value, `false` and `0` included: the refusal is about the
    // variable still being set, not about what it asks for. Reading `false` as
    // "off" would carry on silently for exactly the unit files that most need
    // telling that the variable no longer does anything.
    for value in ["true", "false", "0"] {
        let out = musefs()
            .arg("scan")
            .arg(&target)
            .arg("--db")
            .arg(&db)
            .env("MUSEFS_REVALIDATE", value)
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            !out.status.success(),
            "MUSEFS_REVALIDATE={value}, stderr: {stderr}"
        );
        assert!(
            stderr.contains("MUSEFS_REVALIDATE") && stderr.contains("`revalidate` subcommand"),
            "MUSEFS_REVALIDATE={value}: the refusal should name the variable and its \
             replacement, stderr: {stderr}"
        );
        assert!(
            !db.exists(),
            "MUSEFS_REVALIDATE={value}: a refused scan must not have created the store"
        );
    }

    // Empty is unset, as it is for every variable clap reads.
    let out = musefs()
        .arg("scan")
        .arg(&target)
        .arg("--db")
        .arg(&db)
        .env("MUSEFS_REVALIDATE", "")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// What `mount`, `scan` and `revalidate` print while a track awaits the
/// revalidate an upgrade left owed.
const OWED_REVALIDATE: &str = "have not been re-probed since the store was upgraded";

/// Run `musefs` with `args` and hand back its stderr, having checked it
/// succeeded.
fn stderr_of_success(args: &[&std::ffi::OsStr]) -> String {
    let out = musefs().args(args).output().unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(out.status.success(), "musefs {args:?}, stderr: {stderr}");
    stderr
}

/// #705: while any track awaits its re-probe, each of the three commands that
/// open the store for ordinary work says so on stderr — `migrate`'s one report
/// scrolls away and the gap does not. Once a revalidate has re-probed the track,
/// none of them does.
///
/// `mount` is reached through `--dry-run`, which opens the store and warns
/// exactly as a mount does and stops before FUSE, so this needs no `/dev/fuse`.
/// `scan` and `revalidate` are pointed at an empty directory, so they leave the
/// owed row as it is.
#[test]
fn every_store_command_warns_of_an_owed_revalidate_until_it_has_run() {
    let (dir, library, db) = library_with_one_flac();
    let elsewhere = dir.path().join("elsewhere");
    std::fs::create_dir(&elsewhere).unwrap();
    // The row an upgrade leaves: neither a fingerprint nor an inode. Canonical,
    // as a scan stores it, so a revalidate of `library` reaches it.
    let track = std::fs::canonicalize(library.join("a.flac")).unwrap();
    musefs_db::Db::open(&db)
        .unwrap()
        .upsert_track(&musefs_db::NewTrack {
            backing_path: track,
            format: musefs_db::Format::Flac,
            audio_offset: 0,
            audio_length: 1,
            backing_size: 1,
            backing_mtime_ns: 0,
            backing_ctime_ns: 0,
            backing_ino: None,
        })
        .unwrap();

    let (db, elsewhere, library) = (db.as_os_str(), elsewhere.as_os_str(), library.as_os_str());
    let commands: [(&str, Vec<&std::ffi::OsStr>); 3] = [
        (
            "mount",
            vec!["mount".as_ref(), "--db".as_ref(), db, "--dry-run".as_ref()],
        ),
        (
            "scan",
            vec!["scan".as_ref(), elsewhere, "--db".as_ref(), db],
        ),
        (
            "revalidate",
            vec!["revalidate".as_ref(), elsewhere, "--db".as_ref(), db],
        ),
    ];

    for (name, args) in &commands {
        let stderr = stderr_of_success(args);
        assert!(
            stderr.contains(OWED_REVALIDATE) && stderr.contains("1 track(s)"),
            "`{name}` must warn while a revalidate is owed, stderr: {stderr}"
        );
    }

    // The revalidate that re-probes the track does not warn about it afterwards.
    let stderr = stderr_of_success(&["revalidate".as_ref(), library, "--db".as_ref(), db]);
    assert!(
        !stderr.contains(OWED_REVALIDATE),
        "the revalidate that re-probed every track must not warn, stderr: {stderr}"
    );
    for (name, args) in &commands {
        let stderr = stderr_of_success(args);
        assert!(
            !stderr.contains(OWED_REVALIDATE),
            "`{name}` must not warn once every track is re-probed, stderr: {stderr}"
        );
    }
}

#[test]
fn scan_prune_and_revalidate_force_are_usage_errors() {
    let (_dir, target, db) = library_with_one_flac();
    for argv in [
        [
            "scan",
            target.to_str().unwrap(),
            "--db",
            db.to_str().unwrap(),
            "--prune",
        ],
        [
            "revalidate",
            target.to_str().unwrap(),
            "--db",
            db.to_str().unwrap(),
            "--force",
        ],
    ] {
        let out = musefs().args(argv).output().unwrap();
        assert_eq!(
            out.status.code(),
            Some(2),
            "usage error should exit 2, stderr: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
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

/// `MUSEFS_YES` reads like every other boolean variable: `0`/`1`, `no`/`yes`,
/// `off`/`on`, `false`/`true`. It confirms `migrate`'s one-way upgrade, and a
/// unit file writing `0` for "ask me" used to be a usage error instead. Each
/// value must parse and reach the command, which then refuses the missing store;
/// a non-boolean value is still a usage error.
#[test]
fn migrate_yes_env_takes_boolish_values() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("absent.db");
    for value in ["0", "no", "off", "false", "1", "yes", "on", "true"] {
        let out = musefs()
            .arg("migrate")
            .arg("--db")
            .arg(&missing)
            .env("MUSEFS_YES", value)
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert_eq!(
            out.status.code(),
            Some(1),
            "MUSEFS_YES={value} must parse; stderr: {stderr}"
        );
        assert!(
            stderr.contains("database not found"),
            "MUSEFS_YES={value}: {stderr}"
        );
    }
    let out = musefs()
        .arg("migrate")
        .arg("--db")
        .arg(&missing)
        .env("MUSEFS_YES", "enabled")
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(2),
        "a non-boolean MUSEFS_YES is a usage error; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// A store 1.x left behind, at the schema version before the gated upgrade: one
/// clean track `/lib/a.flac` with a tag, a correctly filed picture and a link to
/// it. Returned open, for the caller to plant what it needs.
fn store_before_the_upgrade(dir: &Path) -> (PathBuf, rusqlite::Connection) {
    let path = dir.join("library.db");
    musefs_db::seed_store_at_version(&path, musefs_db::LATEST_VERSION - 1).unwrap();
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute_batch(&format!(
        "INSERT INTO tracks (backing_path, format, audio_offset, audio_length, \
           backing_size, backing_mtime_ns, backing_ctime_ns, updated_at) \
         VALUES ('/lib/a.flac', 'flac', 0, 0, 0, 0, 0, 0); \
         INSERT INTO tags (track_id, key, value, ordinal) VALUES (1, 'artist', 'A', 0); \
         INSERT INTO art (sha256, mime, width, height, byte_len, data) \
         VALUES ('{}', 'image/png', 1, 1, 1, X'00'); \
         INSERT INTO track_art (track_id, art_id, picture_type, description, ordinal) \
         VALUES (1, 1, 3, 'cover', 0);",
        "a".repeat(64)
    ))
    .unwrap();
    (path, conn)
}

fn user_version(path: &Path) -> i64 {
    rusqlite::Connection::open(path)
        .unwrap()
        .pragma_query_value(None, "user_version", |r| r.get(0))
        .unwrap()
}

fn count(path: &Path, table: &str) -> i64 {
    rusqlite::Connection::open(path)
        .unwrap()
        .query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
        .unwrap()
}

/// `musefs migrate` over `db` with `extra`, confirmed and declining both offers.
fn migrate(db: &Path, extra: &[&str]) -> Output {
    musefs()
        .args([
            "migrate",
            "--yes",
            "--vacuum=false",
            "--revalidate=false",
            "--db",
        ])
        .arg(db)
        .args(extra)
        .output()
        .unwrap()
}

/// The pre-flight names what `--repair` decides rather than deletes: a path
/// stored twice, with both track ids and the one kept, and the picture links it
/// moves onto an identical, correctly filed copy. With `--repair`, it says the
/// rows will be deleted as part of the upgrade, and only once the upgrade has
/// run does it say they were.
#[test]
fn migrate_reports_duplicates_and_relinks_then_what_the_repair_did() {
    let dir = tempfile::tempdir().unwrap();
    let (db, conn) = store_before_the_upgrade(dir.path());
    // The same path as bytes: a second track row the old schema never compared.
    conn.execute_batch(&format!(
        "INSERT INTO tracks (backing_path, format, audio_offset, audio_length, \
           backing_size, backing_mtime_ns, backing_ctime_ns, updated_at) \
         VALUES (CAST('/lib/a.flac' AS BLOB), 'flac', 0, 0, 0, 0, 0, 0); \
         INSERT INTO art (sha256, mime, width, height, byte_len, data) \
         VALUES ('{}', 'image/png', 1, 1, 1, X'00'); \
         INSERT INTO track_art (track_id, art_id, picture_type, description, ordinal) \
         VALUES (1, 2, 4, 'back', 1);",
        "A".repeat(64)
    ))
    .unwrap();
    drop(conn);

    let out = migrate(&db, &[]);
    let (stdout, stderr) = (
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    assert_eq!(
        out.status.code(),
        Some(1),
        "stdout: {stdout} stderr: {stderr}"
    );
    assert!(
        stdout.contains(
            "  /lib/a.flac is stored twice, as tracks 1 and 2: --repair keeps track 1 and \
             deletes track 2"
        ),
        "{stdout}"
    );
    assert!(
        stdout.contains(
            "  1 picture link(s) point at a refused art row with a correctly filed copy of \
             the same image"
        ),
        "{stdout}"
    );
    assert!(stderr.contains("Pass --repair"), "{stderr}");

    let out = migrate(&db, &["--repair"]);
    let (stdout, stderr) = (
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    assert!(out.status.success(), "stdout: {stdout} stderr: {stderr}");
    let position = |needle: &str| {
        stdout
            .find(needle)
            .unwrap_or_else(|| panic!("no {needle:?} in: {stdout}"))
    };
    let planned = position(
        "--repair: 2 row(s) the new schema refuses will be deleted as part of the upgrade, \
         and 1 picture link(s) moved onto a correctly filed copy of the same image; if the \
         upgrade fails, they are kept.",
    );
    let migrated = position("migrated ");
    let done = position(
        "repaired: deleted 2 row(s) the new schema refused, and 1 picture link(s) moved onto \
         a correctly filed copy of the same image",
    );
    assert!(planned < migrated && migrated < done, "{stdout}");
    assert_eq!(user_version(&db), musefs_db::LATEST_VERSION);
    assert_eq!(count(&db, "tracks"), 1, "the duplicate row went");
    assert_eq!(count(&db, "track_art"), 2, "and both pictures stayed");
}

/// Both rows of a path stored twice carry curated data, so there is nothing
/// `--repair` can safely keep. It refuses before the snapshot, naming the path,
/// both track ids and the decision to make, and changes nothing.
#[test]
fn migrate_refuses_to_choose_between_two_curated_rows_for_one_path() {
    let dir = tempfile::tempdir().unwrap();
    let (db, conn) = store_before_the_upgrade(dir.path());
    conn.execute_batch(
        "INSERT INTO tracks (backing_path, format, audio_offset, audio_length, \
           backing_size, backing_mtime_ns, backing_ctime_ns, updated_at) \
         VALUES (CAST('/lib/a.flac' AS BLOB), 'flac', 0, 0, 0, 0, 0, 0); \
         INSERT INTO tags (track_id, key, value, ordinal) VALUES (2, 'artist', 'B', 0);",
    )
    .unwrap();
    drop(conn);
    let before = user_version(&db);

    let out = migrate(&db, &["--repair"]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "{stderr}");
    assert!(
        stderr.contains(
            "/lib/a.flac is stored twice, as tracks 1 and 2, and both carry tags or picture \
             links, so --repair cannot choose which to keep"
        ) && stderr.contains("delete the other yourself"),
        "{stderr}"
    );
    assert_eq!(user_version(&db), before, "nothing was upgraded");
    assert_eq!(count(&db, "tracks"), 2, "nothing was deleted");
    let names: Vec<String> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    assert!(
        !names.iter().any(|n| n.contains(".bak")),
        "refused before any snapshot was written: {names:?}"
    );
}

/// An upgrade that fails after `--repair` rolls the repair back with it, and
/// the error says so and names the snapshot. The failure is a table of the name
/// the upgrade builds its first holding table under, which nothing in the
/// pre-flight looks at.
#[test]
fn a_failed_upgrade_after_a_repair_says_it_rolled_back_and_names_the_snapshot() {
    let dir = tempfile::tempdir().unwrap();
    let (db, conn) = store_before_the_upgrade(dir.path());
    conn.pragma_update(None, "ignore_check_constraints", true)
        .unwrap();
    conn.execute(
        "INSERT INTO tags (track_id, key, value, ordinal) VALUES (1, ?1, 'v', 1)",
        [format!("k{}junk", '\0')],
    )
    .unwrap();
    conn.execute_batch("CREATE TABLE tracks_hold_v4 (x)")
        .unwrap();
    drop(conn);
    let before = user_version(&db);
    let snapshot = dir.path().join(format!("library.db.v{before}.bak"));

    let out = migrate(&db, &["--repair"]);
    let (stdout, stderr) = (
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    assert_eq!(
        out.status.code(),
        Some(1),
        "stdout: {stdout} stderr: {stderr}"
    );
    assert!(
        stderr.contains(&format!(
            "failed: the upgrade was rolled back, and the store is unchanged at schema \
             version {before}, the rows --repair was to delete included; the snapshot taken \
             before it is at {}",
            snapshot.display()
        )),
        "{stderr}"
    );
    assert!(!stdout.contains("repaired: deleted"), "{stdout}");
    assert_eq!(user_version(&db), before);
    assert_eq!(count(&db, "tags"), 2, "the refused tag is back");
    assert_eq!(user_version(&snapshot), before, "and the snapshot is there");
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
    assert!(stdout.contains("MUSEFS_MATCH"), "stdout: {stdout}");
    // #709: the two booleans the value replaced must not still be advertised.
    assert!(!stdout.contains("MUSEFS_FAST"), "stdout: {stdout}");
    assert!(!stdout.contains("MUSEFS_STRICT"), "stdout: {stdout}");
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

// #370: a boolish MUSEFS_QUIET is honoured in every spelling — a true one
// suppresses the summary, a false one keeps it, and the empty value is unset, so
// the default (the summary) applies. The bare-`bool` parser would reject `0`
// outright, so this only passes once BoolishValueParser is attached. A value that
// is not a boolean is still a usage error.
#[test]
fn boolish_quiet_env_toggles_the_summary() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("library");
    std::fs::create_dir(&target).unwrap();
    let db = dir.path().join("quiet.db");
    let scan = |value: &str| {
        musefs()
            .arg("scan")
            .arg(&target)
            .env("MUSEFS_DB", &db)
            .env("MUSEFS_QUIET", value)
            .output()
            .unwrap()
    };

    for (value, quiet) in [
        ("1", true),
        ("true", true),
        ("yes", true),
        ("on", true),
        ("0", false),
        ("false", false),
        ("no", false),
        ("off", false),
        ("", false),
    ] {
        let out = scan(value);
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            out.status.success(),
            "MUSEFS_QUIET={value:?}, stderr: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(
            stdout.trim().is_empty(),
            quiet,
            "MUSEFS_QUIET={value:?} should {} the summary, stdout: {stdout}",
            if quiet { "suppress" } else { "keep" }
        );
    }

    let out = scan("enabled");
    assert_eq!(
        out.status.code(),
        Some(2),
        "a non-boolean MUSEFS_QUIET is a usage error; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Every boolean flag's variable, with the subcommand that reads it.
const BOOLEAN_ENV: &[(&str, &str)] = &[
    ("mount", "MUSEFS_SKIP_ON_MISSING"),
    ("mount", "MUSEFS_READ_AHEAD_PREFETCH"),
    ("mount", "MUSEFS_KEEP_CACHE"),
    ("mount", "MUSEFS_TRUST_BACKING_MTIME"),
    ("mount", "MUSEFS_CASE_INSENSITIVE"),
    ("mount", "MUSEFS_ALLOW_OTHER"),
    ("mount", "MUSEFS_EXPOSE_METRICS"),
    ("scan", "MUSEFS_FORCE"),
    ("scan", "MUSEFS_FOLLOW_SYMLINKS"),
    ("scan", "MUSEFS_QUIET"),
    ("revalidate", "MUSEFS_PRUNE"),
    ("revalidate", "MUSEFS_FOLLOW_SYMLINKS"),
    ("revalidate", "MUSEFS_QUIET"),
    ("migrate", "MUSEFS_YES"),
];

/// A boolean flag's variable set to the empty string — how a systemd unit or an
/// env file blanks one — is unset: the command runs with the flag's default
/// instead of stopping on a usage error. Each is checked on the subcommand that
/// reads it, where the default is observable: the dry-run still lists the track
/// `--skip-on-missing` would drop, the summaries are still printed, nothing is
/// pruned, and `migrate` still asks for its confirmation.
#[test]
fn an_empty_boolean_variable_leaves_the_flag_at_its_default() {
    // Every boolean flag with a variable is covered, so a new one cannot be
    // added without this test naming it.
    let mut declared: Vec<(String, String)> = Vec::new();
    let boolean = clap::builder::ValueParser::bool().type_id();
    for sub in musefs_cli::command().get_subcommands() {
        for arg in sub.get_arguments() {
            if let Some(env) = arg.get_env()
                && arg.get_value_parser().type_id() == boolean
            {
                declared.push((
                    sub.get_name().to_owned(),
                    env.to_string_lossy().into_owned(),
                ));
            }
        }
    }
    let mut listed: Vec<(String, String)> = BOOLEAN_ENV
        .iter()
        .map(|(sub, var)| ((*sub).to_owned(), (*var).to_owned()))
        .collect();
    declared.sort();
    listed.sort();
    assert_eq!(
        declared, listed,
        "BOOLEAN_ENV must list every boolean variable"
    );

    let (dir, library, db) = library_with_one_flac();
    let scanned = musefs()
        .arg("scan")
        .arg(&library)
        .arg("--db")
        .arg(&db)
        .output()
        .unwrap();
    assert!(scanned.status.success());
    let gated = dir.path().join("gated.db");
    musefs_db::seed_store_at_version(&gated, musefs_db::LATEST_VERSION - 1).unwrap();

    for (sub, var) in BOOLEAN_ENV {
        let mut cmd = musefs();
        cmd.env(var, "");
        match *sub {
            "mount" => cmd.args(["mount", "--dry-run", "--db"]).arg(&db),
            "scan" | "revalidate" => cmd.arg(sub).arg(&library).arg("--db").arg(&db),
            "migrate" => cmd.args(["migrate", "--db"]).arg(&gated),
            other => panic!("no invocation for {other}"),
        };
        let out = cmd.output().unwrap();
        let (stdout, stderr) = (
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr),
        );
        let context = format!("{var}= on {sub}; stdout: {stdout} stderr: {stderr}");
        assert_ne!(out.status.code(), Some(2), "not a usage error: {context}");
        match *sub {
            "mount" => {
                assert!(out.status.success(), "{context}");
                assert!(stdout.contains("dry run: 1 files"), "{context}");
            }
            "scan" => {
                assert!(out.status.success(), "{context}");
                assert!(stdout.contains("scanned"), "{context}");
            }
            "revalidate" => {
                assert!(out.status.success(), "{context}");
                assert!(stdout.contains(" 0 pruned"), "{context}");
            }
            _ => {
                assert_eq!(out.status.code(), Some(1), "{context}");
                assert!(stderr.contains("pass --yes"), "{context}");
            }
        }
    }
}

/// #709: `--match` is the one way to ask, and it reaches the scan.
#[test]
fn scan_match_takes_a_value_and_rejects_anything_else() {
    let (_dir, target, db) = library_with_one_flac();
    let out = musefs()
        .arg("scan")
        .arg(&target)
        .arg("--db")
        .arg(&db)
        .args(["--match", "strict"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let out = musefs()
        .arg("scan")
        .arg(&target)
        .arg("--db")
        .arg(&db)
        .args(["--match", "paranoid"])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(2), "stderr: {stderr}");
    assert!(
        stderr.contains("auto") && stderr.contains("fast") && stderr.contains("strict"),
        "the usage error should list the accepted values, stderr: {stderr}"
    );
}

/// #709: the flags `--match` replaced are gone, so they are usage errors, and
/// their variables are refused with the value to use. Ignoring
/// `MUSEFS_STRICT=true` would quietly weaken how a moved file is confirmed.
#[test]
fn retired_fast_and_strict_are_refused() {
    let (_dir, target, db) = library_with_one_flac();
    for flag in ["--fast", "--strict"] {
        let out = musefs()
            .arg("scan")
            .arg(&target)
            .arg("--db")
            .arg(&db)
            .arg(flag)
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert_eq!(out.status.code(), Some(2), "{flag}, stderr: {stderr}");
        assert!(stderr.contains(flag), "{flag}, stderr: {stderr}");
    }
    for (var, value) in [("MUSEFS_FAST", "fast"), ("MUSEFS_STRICT", "strict")] {
        let out = musefs()
            .arg("scan")
            .arg(&target)
            .arg("--db")
            .arg(&db)
            .env(var, "true")
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(!out.status.success(), "{var}, stderr: {stderr}");
        assert!(
            stderr.contains(var) && stderr.contains(&format!("MUSEFS_MATCH={value}")),
            "the refusal should name {var} and its replacement, stderr: {stderr}"
        );
    }
    assert!(
        !db.exists(),
        "a refused scan must not have created the store"
    );
}
