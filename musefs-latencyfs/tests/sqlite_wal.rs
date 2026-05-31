use musefs_latencyfs::LatencyMount;
use rusqlite::Connection;

#[test]
#[ignore = "requires /dev/fuse; run with --ignored"]
fn sqlite_wal_cycle_through_the_mount() {
    let backing = tempfile::tempdir().unwrap();
    let mount = LatencyMount::new(backing.path(), "ssd").unwrap();
    let db_path = mount.path().join("test.db");

    {
        let conn = Connection::open(&db_path).unwrap();
        // WAL mode exercises -wal/-shm create, write, fsync, and truncate.
        let mode: String = conn
            .query_row("PRAGMA journal_mode = WAL", [], |r| r.get(0))
            .unwrap();
        assert_eq!(mode.to_lowercase(), "wal");
        conn.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)", [])
            .unwrap();
        for i in 0..200 {
            conn.execute("INSERT INTO t (v) VALUES (?1)", [format!("row-{i}")])
                .unwrap();
        }
        // Force a checkpoint (truncates the WAL).
        conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")
            .unwrap();
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM t", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 200);
    }

    // Reopen and re-read to confirm durability through the mount.
    let conn = Connection::open(&db_path).unwrap();
    let n: i64 = conn
        .query_row("SELECT COUNT(*) FROM t", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 200);

    assert!(mount.fsyncs() > 0, "WAL writes must have triggered fsyncs");
}
