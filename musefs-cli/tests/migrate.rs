//! End-to-end cover for `musefs migrate` (#705): the pre-flight refusals, the
//! snapshot, and the upgrade itself.
//!
//! Every test runs off a terminal, which is the shape a CI or a pipeline has:
//! the confirmation must come from `--yes` and the two offers must decline
//! themselves rather than block on an answer nobody is there to give.

use std::path::{Path, PathBuf};

use musefs_cli::{MigrateArgs, run_migrate};
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

fn args(db: &Path) -> MigrateArgs {
    MigrateArgs {
        db: db.to_path_buf(),
        yes: true,
        repair: false,
        snapshot: None,
        no_snapshot: false,
        vacuum: None,
        revalidate: None,
        jobs: 1,
    }
}

#[test]
fn a_gated_store_is_snapshotted_and_upgraded() {
    let dir = tempfile::tempdir().unwrap();
    let db = gated_store(dir.path());

    run_migrate(&args(&db)).unwrap();

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

    run_migrate(&MigrateArgs {
        no_snapshot: true,
        ..args(&db)
    })
    .unwrap();

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

    run_migrate(&MigrateArgs {
        snapshot: Some(dest.clone()),
        ..args(&db)
    })
    .unwrap();

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

    let err = run_migrate(&MigrateArgs {
        snapshot: Some(dest.clone()),
        ..args(&db)
    })
    .unwrap_err()
    .to_string();

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

    let err = run_migrate(&MigrateArgs {
        yes: false,
        ..args(&db)
    })
    .unwrap_err()
    .to_string();

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
    run_migrate(&MigrateArgs {
        repair: true,
        ..args(&db)
    })
    .unwrap();

    assert_eq!(user_version(&db), LATEST_VERSION);
    let store = Db::open(&db).unwrap();
    let tags = store.get_tags(1).unwrap();
    assert_eq!(tags.len(), 1, "the clean tag survived: {tags:?}");
    assert_eq!(tags[0].key, "artist");
}
