use musefs_db::Db;

#[test]
fn open_in_memory_runs_migration_to_latest() {
    let db = Db::open_in_memory().expect("open");
    assert_eq!(db.user_version().expect("user_version"), 1);
}

#[test]
fn migration_is_idempotent_across_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("musefs.db");

    let db = Db::open(&path).unwrap();
    assert_eq!(db.user_version().unwrap(), 1);
    drop(db);

    // Reopening must not error and must not advance the version.
    let db2 = Db::open(&path).unwrap();
    assert_eq!(db2.user_version().unwrap(), 1);
}

#[cfg(feature = "mutants")]
#[test]
fn default_db_is_unmigrated_version_zero() {
    // Kills `Db::user_version -> Ok(1)`: Default opens an UNMIGRATED in-memory
    // connection, so user_version is 0, distinct from the always-migrated 1.
    let db = Db::default();
    assert_eq!(db.user_version().unwrap(), 0);
}
