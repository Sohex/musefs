//! End-to-end cover for `musefs migrate` (#705): the pre-flight refusals, the
//! snapshot, and the upgrade itself.
//!
//! Every test runs off a terminal, which is the shape a CI or a pipeline has:
//! the confirmation must come from `--yes` and the two offers must decline
//! themselves rather than block on an answer nobody is there to give.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::Parser;
use musefs_cli::{Cli, Command, MigrateArgs, run_migrate};
use musefs_db::{Db, LATEST_VERSION};

/// A store an older musefs build would have left behind: the earlier steps, run
/// for real, because a current store with its `user_version` rewound is not one.
fn gated_store(dir: &Path) -> PathBuf {
    let path = dir.join("library.db");
    musefs_db::seed_store_at_version(&path, LATEST_VERSION - 1).unwrap();
    path
}

fn user_version(path: &Path) -> i64 {
    rusqlite::Connection::open(path)
        .unwrap()
        .pragma_query_value(None, "user_version", |r| r.get(0))
        .unwrap()
}

/// Migrate args as the parser produces them from a command line: confirmed, one
/// probe worker, and both offers left unanswered so they decline off a terminal.
fn args(db: &Path) -> MigrateArgs {
    let db = db.to_str().expect("tempdir paths are UTF-8");
    let cli = Cli::parse_from(["musefs", "migrate", "--db", db, "--yes", "--jobs", "1"]);
    let Command::Migrate(args) = cli.command else {
        panic!("expected migrate");
    };
    args
}

#[test]
fn a_gated_store_is_snapshotted_and_upgraded() {
    let dir = tempfile::tempdir().unwrap();
    let db = gated_store(dir.path());

    assert_eq!(
        run_migrate(&args(&db)).unwrap(),
        0,
        "no tracks, so no revalidate and nothing failed"
    );

    assert_eq!(user_version(&db), LATEST_VERSION);
    // The snapshot is the remedy that makes this reversible, so it has to be
    // there, and it has to be the store as it was.
    let snapshot = dir
        .path()
        .join(format!("library.db.v{}.bak", LATEST_VERSION - 1));
    assert!(snapshot.exists(), "a snapshot must be taken by default");
    assert_eq!(user_version(&snapshot), LATEST_VERSION - 1);
}

#[test]
fn no_snapshot_skips_the_copy_and_still_upgrades() {
    let dir = tempfile::tempdir().unwrap();
    let db = gated_store(dir.path());

    let mut migrate_args = args(&db);
    migrate_args.no_snapshot = true;
    run_migrate(&migrate_args).unwrap();

    assert_eq!(user_version(&db), LATEST_VERSION);
    let default = dir
        .path()
        .join(format!("library.db.v{}.bak", LATEST_VERSION - 1));
    assert!(!default.exists(), "--no-snapshot must write no copy");
}

#[test]
fn a_named_snapshot_goes_where_it_was_asked_to() {
    let dir = tempfile::tempdir().unwrap();
    let db = gated_store(dir.path());
    let dest = dir.path().join("keep-me.db");

    let mut migrate_args = args(&db);
    migrate_args.snapshot = Some(dest.clone());
    run_migrate(&migrate_args).unwrap();

    assert_eq!(user_version(&dest), LATEST_VERSION - 1);
}

/// Overwriting a backup is exactly the thing a backup exists to prevent, and
/// the refusal has to name the way out.
#[test]
fn an_occupied_snapshot_destination_is_refused_before_anything_changes() {
    let dir = tempfile::tempdir().unwrap();
    let db = gated_store(dir.path());
    let dest = dir.path().join("taken.db");
    std::fs::write(&dest, b"not a database").unwrap();

    let mut migrate_args = args(&db);
    migrate_args.snapshot = Some(dest.clone());
    let err = run_migrate(&migrate_args).unwrap_err().to_string();

    assert!(err.contains("already exists"), "{err}");
    assert!(err.contains("--no-snapshot"), "{err}");
    assert_eq!(user_version(&db), LATEST_VERSION - 1, "store untouched");
    assert_eq!(std::fs::read(&dest).unwrap(), b"not a database");
}

/// Off a terminal there is nobody to ask, so the command must name the flag
/// rather than block on stdin.
#[test]
fn without_yes_and_without_a_terminal_it_refuses_and_names_the_flag() {
    let dir = tempfile::tempdir().unwrap();
    let db = gated_store(dir.path());

    let mut migrate_args = args(&db);
    migrate_args.yes = false;
    let err = run_migrate(&migrate_args).unwrap_err().to_string();

    assert!(err.contains("--yes"), "{err}");
    assert_eq!(user_version(&db), LATEST_VERSION - 1, "store untouched");
}

/// Running it twice is what a provisioning script does. The second run is a
/// report, not an error.
#[test]
fn a_store_already_at_the_latest_version_is_a_no_op() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("library.db");
    Db::open(&path).unwrap();

    run_migrate(&args(&path)).unwrap();

    assert_eq!(user_version(&path), LATEST_VERSION);
    assert!(
        !dir.path()
            .join(format!("library.db.v{LATEST_VERSION}.bak"))
            .exists(),
        "nothing to migrate means nothing to snapshot"
    );
}

#[test]
fn a_missing_store_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let err = run_migrate(&args(&dir.path().join("nope.db")))
        .unwrap_err()
        .to_string();
    assert!(err.contains("database not found"), "{err}");
}

/// A mount or a scan holding the store means it is not ours to rewrite, and
/// the refusal has to land before the snapshot, not after.
#[test]
fn a_store_another_connection_is_using_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let db = gated_store(dir.path());

    let other = rusqlite::Connection::open(&db).unwrap();
    other
        .busy_timeout(std::time::Duration::from_millis(50))
        .unwrap();
    other
        .query_row("SELECT count(*) FROM tracks", [], |r| r.get::<_, i64>(0))
        .unwrap();

    let err = run_migrate(&args(&db)).unwrap_err().to_string();

    assert!(err.contains("in use"), "{err}");
    assert_eq!(user_version(&db), LATEST_VERSION - 1, "store untouched");
    assert!(
        !dir.path()
            .join(format!("library.db.v{}.bak", LATEST_VERSION - 1))
            .exists(),
        "the refusal must come before the snapshot"
    );
}

/// A gated store holding one clean tag and one the upgraded schema refuses.
/// The hostile row goes in with the constraints off, which is the only way it
/// can exist: the shape on disk here is already the target's.
fn store_with_a_refused_row(dir: &Path) -> PathBuf {
    let path = gated_store(dir);
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute(
        "INSERT INTO tracks (backing_path, format, audio_offset, audio_length, \
         backing_size, backing_mtime_ns, backing_ctime_ns, updated_at) \
         VALUES (CAST('/lib/a.flac' AS BLOB), 'flac', 0, 0, 0, 0, 0, 0)",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO tags (track_id, key, value, ordinal) VALUES (1, 'artist', 'A', 0)",
        [],
    )
    .unwrap();
    conn.pragma_update(None, "ignore_check_constraints", true)
        .unwrap();
    conn.execute(
        "INSERT INTO tags (track_id, key, value, ordinal) VALUES (1, ?1, 'v', 1)",
        [&format!("k{}junk", '\0')],
    )
    .unwrap();
    path
}

#[test]
fn a_store_with_a_refused_row_is_not_upgraded_without_repair() {
    let dir = tempfile::tempdir().unwrap();
    let db = store_with_a_refused_row(dir.path());
    let before = user_version(&db);

    let err = run_migrate(&args(&db)).unwrap_err().to_string();
    assert!(
        err.contains("--repair"),
        "the refusal must name the way out: {err}"
    );
    assert!(err.contains("1 row(s)"), "{err}");
    assert_eq!(
        user_version(&db),
        before,
        "the store is untouched: the refusal comes before anything is written"
    );
    assert!(
        !dir.path()
            .join(format!("library.db.v{before}.bak"))
            .exists(),
        "and before the snapshot, so nothing was copied either"
    );
}

#[test]
fn repair_deletes_the_refused_row_and_upgrades() {
    let dir = tempfile::tempdir().unwrap();
    let db = store_with_a_refused_row(dir.path());
    let mut migrate_args = args(&db);
    migrate_args.repair = true;
    run_migrate(&migrate_args).unwrap();

    assert_eq!(user_version(&db), LATEST_VERSION);
    let store = Db::open(&db).unwrap();
    let tags = store.get_tags(1).unwrap();
    assert_eq!(tags.len(), 1, "the clean tag survived: {tags:?}");
    assert_eq!(tags[0].key, "artist");
}

/// #750: the revalidate `migrate` runs on request counts a file it cannot
/// process as failed, and `migrate` hands that count back so the command exits
/// `2`, as `revalidate` would. The store is upgraded all the same.
#[test]
fn a_revalidate_with_failures_is_reported_to_the_caller() {
    let dir = tempfile::tempdir().unwrap();
    let db = store_with_an_unparseable_track(dir.path());

    let mut migrate_args = args(&db);
    migrate_args.revalidate = Some(true);
    assert_eq!(run_migrate(&migrate_args).unwrap(), 1);
    assert_eq!(
        user_version(&db),
        LATEST_VERSION,
        "the upgrade itself landed"
    );
}

/// #750: `run` is what turns that count into the process's exit status, and a
/// script chaining on it must be able to tell a partial revalidate from a clean
/// one. Exit 2 for the failure, success for the same upgrade without one.
#[test]
fn run_exits_two_only_when_the_offered_revalidate_counts_failures() {
    let dir = tempfile::tempdir().unwrap();
    let db = store_with_an_unparseable_track(dir.path());
    let failing = musefs_cli::run(Cli::parse_from(migrate_argv(&db, &["--revalidate"]))).unwrap();
    assert_eq!(failing, ExitCode::from(2));
    assert_eq!(user_version(&db), LATEST_VERSION, "the upgrade landed");

    let dir = tempfile::tempdir().unwrap();
    let db = gated_store(dir.path());
    let clean = musefs_cli::run(Cli::parse_from(migrate_argv(&db, &["--revalidate"]))).unwrap();
    assert_eq!(clean, ExitCode::SUCCESS);
}

/// `--vacuum` compacts the upgraded store: no free page is left behind, and the
/// file is smaller than the same upgrade leaves it without the flag.
#[test]
fn vacuum_compacts_the_upgraded_store() {
    let upgraded = |vacuum: &str| {
        let dir = tempfile::tempdir().unwrap();
        let db = gated_store(dir.path());
        // Free pages for the vacuum to reclaim, beyond the ones the upgrade's
        // own table rebuilds leave.
        rusqlite::Connection::open(&db)
            .unwrap()
            .execute_batch(
                "CREATE TABLE bloat (b BLOB); \
                 INSERT INTO bloat VALUES (zeroblob(4 * 1024 * 1024)); \
                 DROP TABLE bloat;",
            )
            .unwrap();
        let code = musefs_cli::run(Cli::parse_from(migrate_argv(
            &db,
            &["--no-snapshot", vacuum],
        )))
        .unwrap();
        assert_eq!(code, ExitCode::SUCCESS);
        let conn = rusqlite::Connection::open(&db).unwrap();
        let free: i64 = conn
            .pragma_query_value(None, "freelist_count", |r| r.get(0))
            .unwrap();
        drop(conn);
        (free, std::fs::metadata(&db).unwrap().len(), dir)
    };

    let (free_kept, size_kept, _dir) = upgraded("--vacuum=false");
    let (free_vacuumed, size_vacuumed, _dir) = upgraded("--vacuum");
    assert!(free_kept > 0, "the unvacuumed upgrade leaves free pages");
    assert_eq!(free_vacuumed, 0, "--vacuum leaves none");
    assert!(
        size_vacuumed < size_kept,
        "--vacuum shrinks the store: {size_vacuumed} vs {size_kept} bytes"
    );
}

/// #706: every command that opens a store for ordinary work refuses one a gated
/// step stands in front of, names the remedy, and leaves the store as it found
/// it. `mount` is driven through `--dry-run`, which opens the store exactly as a
/// mount does and never reaches FUSE.
///
/// Compared byte for byte, which is only meaningful with the WAL checkpointed
/// into the file first: a refusing open that closes the last connection
/// checkpoints whatever frames were pending, which changes the bytes without
/// changing the data. Checkpointed, nothing is pending, so any change is one the
/// refusal made.
#[test]
fn ordinary_commands_refuse_a_gated_store_and_leave_it_as_it_was() {
    let dir = tempfile::tempdir().unwrap();
    let library = dir.path().join("lib");
    std::fs::create_dir(&library).unwrap();
    let db = gated_store(dir.path());
    rusqlite::Connection::open(&db)
        .unwrap()
        .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))
        .unwrap();
    let before = std::fs::read(&db).unwrap();

    let (db_arg, lib_arg) = (db.to_str().unwrap(), library.to_str().unwrap());
    for argv in [
        vec!["musefs", "mount", "--db", db_arg, "--dry-run"],
        vec!["musefs", "scan", lib_arg, "--db", db_arg, "--jobs", "1"],
        vec![
            "musefs",
            "revalidate",
            lib_arg,
            "--db",
            db_arg,
            "--jobs",
            "1",
        ],
        vec!["musefs", "vacuum", "--db", db_arg],
    ] {
        let command = argv[1];
        let err = musefs_cli::run(Cli::parse_from(&argv))
            .expect_err(&format!("`{command}` must refuse a gated store"));
        let message = format!("{err:#}");
        assert!(
            message.contains("run `musefs migrate --db <store>`"),
            "`{command}` must name the remedy: {message}"
        );
        assert_eq!(
            user_version(&db),
            LATEST_VERSION - 1,
            "`{command}` left the version where it was"
        );
        assert!(
            std::fs::read(&db).unwrap() == before,
            "`{command}` left the store byte-identical"
        );
    }
}

/// A gated store with one track whose file parses as nothing, so the revalidate
/// `migrate` offers counts it as failed.
fn store_with_an_unparseable_track(dir: &Path) -> PathBuf {
    let library = dir.join("lib");
    std::fs::create_dir(&library).unwrap();
    // A supported extension over bytes that parse as nothing: the revalidate's
    // probe refuses it, which is a failure, not a crash.
    let broken = library.join("broken.flac");
    std::fs::write(&broken, b"not a flac at all").unwrap();
    // Canonical, as a scan stores it: revalidate keys the files it walks on
    // their canonical paths, and on macOS the temp directory is behind the
    // `/var` -> `/private/var` symlink.
    let broken = std::fs::canonicalize(&broken).unwrap();

    let db = gated_store(dir);
    rusqlite::Connection::open(&db)
        .unwrap()
        .execute(
            "INSERT INTO tracks (backing_path, format, audio_offset, audio_length, \
             backing_size, backing_mtime_ns, backing_ctime_ns, updated_at) \
             VALUES (CAST(?1 AS BLOB), 'flac', 0, 0, 0, 0, 0, 0)",
            [broken.to_str().expect("tempdir paths are UTF-8")],
        )
        .unwrap();
    db
}

/// The command line `musefs migrate` gets in a script: confirmed, one probe
/// worker, plus `extra`.
fn migrate_argv<'a>(db: &'a Path, extra: &[&'a str]) -> Vec<&'a str> {
    let mut argv = vec![
        "musefs",
        "migrate",
        "--db",
        db.to_str().expect("tempdir paths are UTF-8"),
        "--yes",
        "--jobs",
        "1",
    ];
    argv.extend_from_slice(extra);
    argv
}

/// `run_migrate` is public and its arguments are plain fields, so the parser's
/// refusal of `--repair` with `--no-snapshot` cannot be the only guard. Called
/// directly with both, it must refuse before deleting anything, rather than
/// repair the store without a snapshot and then panic.
#[test]
fn repair_without_a_snapshot_is_refused_by_the_function_itself() {
    let dir = tempfile::tempdir().unwrap();
    let db = store_with_a_refused_row(dir.path());
    let before = user_version(&db);
    let tags_before: i64 = rusqlite::Connection::open(&db)
        .unwrap()
        .query_row("SELECT count(*) FROM tags", [], |r| r.get(0))
        .unwrap();

    let mut migrate_args = args(&db);
    migrate_args.repair = true;
    migrate_args.no_snapshot = true;
    let err = run_migrate(&migrate_args).unwrap_err().to_string();
    assert!(err.contains("--repair requires a snapshot"), "{err}");

    let tags_after: i64 = rusqlite::Connection::open(&db)
        .unwrap()
        .query_row("SELECT count(*) FROM tags", [], |r| r.get(0))
        .unwrap();
    assert_eq!(tags_after, tags_before, "the refused row is still there");
    assert_eq!(user_version(&db), before, "and nothing was migrated");

    let mut migrate_args = args(&db);
    migrate_args.snapshot = Some(dir.path().join("elsewhere.bak"));
    migrate_args.no_snapshot = true;
    let err = run_migrate(&migrate_args).unwrap_err().to_string();
    assert!(err.contains("--snapshot"), "{err}");
    assert_eq!(user_version(&db), before);
}

/// An explicit `--revalidate` that cannot run, because the stored tracks share
/// no directory below `/`, fails the command: the store is upgraded, but the
/// revalidate the caller asked for did not happen, and exit 0 would say it did.
#[test]
fn an_explicit_revalidate_that_cannot_run_fails_after_upgrading() {
    let dir = tempfile::tempdir().unwrap();
    let db = gated_store(dir.path());
    let conn = rusqlite::Connection::open(&db).unwrap();
    for path in ["/home/u/music/a.flac", "/srv/music/b.flac"] {
        conn.execute(
            "INSERT INTO tracks (backing_path, format, audio_offset, audio_length, \
             backing_size, backing_mtime_ns, backing_ctime_ns, updated_at) \
             VALUES (CAST(?1 AS BLOB), 'flac', 0, 0, 0, 0, 0, 0)",
            [path],
        )
        .unwrap();
    }
    drop(conn);

    let mut migrate_args = args(&db);
    migrate_args.revalidate = Some(true);
    let err = run_migrate(&migrate_args).unwrap_err().to_string();
    assert!(err.contains("--revalidate was not run"), "{err}");
    assert_eq!(
        user_version(&db),
        LATEST_VERSION,
        "the upgrade itself stands"
    );
}
