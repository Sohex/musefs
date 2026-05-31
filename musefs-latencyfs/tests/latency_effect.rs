use std::time::Instant;

use musefs_latencyfs::LatencyMount;

fn open_read_close(path: &std::path::Path) {
    use std::io::Read;
    let mut s = Vec::new();
    std::fs::File::open(path)
        .unwrap()
        .read_to_end(&mut s)
        .unwrap();
}

#[test]
#[ignore = "requires /dev/fuse; run with --ignored"]
fn nonzero_profile_is_slower_than_ssd() {
    let backing = tempfile::tempdir().unwrap();
    std::fs::write(backing.path().join("f.bin"), vec![0u8; 64 * 1024]).unwrap();

    // ssd (~0) baseline.
    let fast = LatencyMount::new(backing.path(), "ssd").unwrap();
    let t0 = Instant::now();
    for _ in 0..20 {
        open_read_close(&fast.path().join("f.bin"));
    }
    let fast_ms = t0.elapsed().as_millis();
    drop(fast);

    // hdd profile: each open+read sleeps multiple ms, so 20 iterations are far slower.
    let slow = LatencyMount::new(backing.path(), "hdd").unwrap();
    let t1 = Instant::now();
    for _ in 0..20 {
        open_read_close(&slow.path().join("f.bin"));
    }
    let slow_ms = t1.elapsed().as_millis();

    println!("ssd={fast_ms}ms hdd={slow_ms}ms");
    assert!(
        slow_ms > fast_ms + 50,
        "hdd profile ({slow_ms}ms) should be clearly slower than ssd ({fast_ms}ms)"
    );
}

#[test]
#[ignore = "requires /dev/fuse; run with --ignored"]
fn fsync_count_rises_with_more_commits() {
    use rusqlite::Connection;
    let backing = tempfile::tempdir().unwrap();
    let mount = LatencyMount::new(backing.path(), "ssd").unwrap();

    let conn = Connection::open(mount.path().join("c.db")).unwrap();
    let _: String = conn
        .query_row("PRAGMA journal_mode = WAL", [], |r| r.get(0))
        .unwrap();
    conn.execute("CREATE TABLE t (id INTEGER PRIMARY KEY)", [])
        .unwrap();

    let before = mount.fsyncs();
    // Each checkpoint forces fsyncs; more checkpoints => strictly more fsyncs.
    for _ in 0..10 {
        conn.execute("INSERT INTO t DEFAULT VALUES", []).unwrap();
        conn.execute_batch("PRAGMA wal_checkpoint(FULL);").unwrap();
    }
    let after = mount.fsyncs();
    assert!(
        after > before,
        "fsync count must rise with commits/checkpoints"
    );
}
