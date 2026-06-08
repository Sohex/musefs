//! Hands a read connection to whichever thread needs one.
//!
//! - File-backed DB → the pool owns one read-only connection per thread in an
//!   internal map. Each thread lazily opens its own connection, and dropping
//!   the pool drops the map and closes every connection it owns.
//! - In-memory DB (tests) cannot be reopened by path, so a single connection is
//!   shared behind a mutex.
//!
//! Dropping the pool closes every connection it owns, from whatever thread
//! drops it (#127). A thread that dies while the pool lives leaves its
//! connection in the map until the pool is dropped; that bound is the pool's
//! lifetime, not the thread's. Each pool has its own map, so multiple mounts
//! (or test DBs) on the same thread don't collide.

use std::collections::{HashMap, hash_map::Entry};
use std::path::PathBuf;
use std::sync::Arc;
use std::thread::ThreadId;

use parking_lot::{Mutex, ReentrantMutex};

use musefs_db::{Db, ReadOnly};

use crate::error::{CoreError, Result};

/// `with` and `with_poll` may nest freely, on any variant: `PerThread` reads
/// hand out cloned `Arc`s from the pool-owned map, and the connection locks
/// are reentrant.
///
/// The `poll`/`conns` asymmetry is deliberate. `poll` is uniquely owned (the
/// `Box` only keeps the variant small) because `with_poll` locks it in place
/// and takes no other lock. `conns` values are `Arc`-wrapped so `with` can
/// clone a handle and release the map guard *before* running the caller's
/// closure — holding the (non-reentrant) map guard across it would deadlock
/// nested `with`. The inner `ReentrantMutex` is never contended (only its
/// owning thread locks it) but is load-bearing for the type system: `Db` is
/// `Send + !Sync`, so the mutex wrapper is what keeps the map field, and
/// therefore `DbPool`, `Send + Sync`.
pub enum DbPool {
    PerThread {
        path: PathBuf,
        poll: Box<ReentrantMutex<Db<ReadOnly>>>,
        conns: Mutex<HashMap<ThreadId, Arc<ReentrantMutex<Db<ReadOnly>>>>>,
    },
    Shared(Arc<ReentrantMutex<Db<ReadOnly>>>),
}

impl DbPool {
    /// Build a pool from the DB used to construct the mount. File-backed DBs
    /// become per-thread pools (the passed connection becomes the poll
    /// connection — workers open their own); in-memory DBs are wrapped in a
    /// shared mutex.
    pub fn new(db: Db) -> Result<DbPool> {
        let db = db.into_read_only();
        match db.path() {
            Some(p) => Ok(DbPool::PerThread {
                path: p.to_path_buf(),
                poll: Box::new(ReentrantMutex::new(db)),
                conns: Mutex::new(HashMap::new()),
            }),
            None => Ok(DbPool::Shared(Arc::new(ReentrantMutex::new(db)))),
        }
    }

    /// Run `f` with the persistent poll connection.
    ///
    /// For `PerThread` pools, `PRAGMA data_version` is connection-relative: a fresh
    /// thread-local connection starts at 0, so it can't detect changes that happened
    /// before it opened. The poll connection is the original writer Db, kept alive
    /// precisely so it can observe incremental changes from other connections.
    /// For `Shared` pools (in-memory), the single shared connection serves both roles.
    pub fn with_poll<R>(&self, f: impl FnOnce(&Db<ReadOnly>) -> Result<R>) -> Result<R> {
        match self {
            DbPool::PerThread { poll, .. } => f(&poll.lock()),
            DbPool::Shared(m) => f(&m.lock()),
        }
    }

    /// Run `f` with a read connection.
    pub fn with<R>(&self, f: impl FnOnce(&Db<ReadOnly>) -> Result<R>) -> Result<R> {
        match self {
            DbPool::PerThread { path, conns, .. } => {
                let tid = std::thread::current().id();
                let db = {
                    let mut map = conns.lock();
                    match map.entry(tid) {
                        Entry::Occupied(entry) => Arc::clone(entry.get()),
                        Entry::Vacant(entry) => {
                            let db =
                                Db::open_readonly(path).map_err(|source| CoreError::DbOpen {
                                    path: path.clone(),
                                    source,
                                })?;
                            Arc::clone(entry.insert(Arc::new(ReentrantMutex::new(db))))
                        }
                    }
                };
                let guard = db.lock();
                f(&guard)
            }
            DbPool::Shared(m) => {
                let db = m.lock();
                f(&db)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use musefs_db::Db;

    /// Count this process's open fds whose target path starts with `db_path`.
    /// Prefix match deliberately: a WAL reader holds up to three fds
    /// (`db`, `db-wal`, `db-shm`). Linux-only: it reads `/proc/self/fd`, which
    /// FreeBSD has no equivalent for by default — so the two fd-leak tests that
    /// use it are gated to Linux as well.
    #[cfg(target_os = "linux")]
    fn db_fd_count(db_path: &std::path::Path) -> usize {
        let prefix = db_path.to_str().unwrap();
        std::fs::read_dir("/proc/self/fd")
            .unwrap()
            .filter_map(|e| std::fs::read_link(e.unwrap().path()).ok())
            .filter(|target| target.to_string_lossy().starts_with(prefix))
            .count()
    }

    // Linux-only: asserts fd closure via `db_fd_count` (/proc/self/fd).
    #[cfg(target_os = "linux")]
    #[test]
    fn drop_closes_connections_opened_by_live_threads() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("d.db");
        Db::open(&path).unwrap(); // create + migrate (writer, sets WAL)
        let baseline = db_fd_count(&path);

        let pool = Arc::new(DbPool::new(Db::open(&path).unwrap()).unwrap());
        // 2 workers + the main thread; workers park here until main has asserted.
        let barrier = Arc::new(std::sync::Barrier::new(3));
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let mut handles = Vec::new();
        for _ in 0..2 {
            let pool = Arc::clone(&pool);
            let barrier = Arc::clone(&barrier);
            let done = done_tx.clone();
            handles.push(std::thread::spawn(move || {
                pool.with(|db| Ok(db.data_version()?)).unwrap();
                drop(pool); // this thread's Arc clone; main's is then the last
                done.send(()).unwrap();
                barrier.wait();
            }));
        }
        // Count exactly two done-signals; don't drain-until-Err — the workers
        // still hold their sender clones while parked at the barrier.
        drop(done_tx);
        for _ in 0..2 {
            done_rx.recv().unwrap();
        }

        // Both workers are done using the pool but still alive (parked at, or
        // headed to, the barrier — they cannot pass it until main waits too).
        drop(pool);
        assert_eq!(
            db_fd_count(&path),
            baseline,
            "pool drop must close all threads' connections while those threads are alive"
        );

        barrier.wait();
        for h in handles {
            h.join().unwrap();
        }
    }

    // Linux-only: asserts fd closure via `db_fd_count` (/proc/self/fd).
    #[cfg(target_os = "linux")]
    #[test]
    fn drop_on_foreign_thread_closes_all_connections() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.db");
        Db::open(&path).unwrap();
        let baseline = db_fd_count(&path);

        let pool = DbPool::new(Db::open(&path).unwrap()).unwrap();
        pool.with(|db| Ok(db.data_version()?)).unwrap(); // opens this thread's connection

        // DbPool is Send: drop it on a thread that never opened a connection.
        std::thread::spawn(move || drop(pool)).join().unwrap();

        assert_eq!(
            db_fd_count(&path),
            baseline,
            "drop on a foreign thread must still close every connection"
        );
    }

    #[test]
    fn shared_pool_for_in_memory_db() {
        let pool = DbPool::new(Db::open_in_memory().unwrap()).unwrap();
        // NOTE: db.data_version() returns the DB crate's error type, so wrap with
        // Ok(...?) to convert it into the core Result the closure must return.
        let v = pool.with(|db| Ok(db.data_version()?)).unwrap();
        let v2 = pool.with(|db| Ok(db.data_version()?)).unwrap();
        assert_eq!(v, v2);
    }

    #[test]
    fn same_thread_two_pools_keyed_by_path() {
        let dir = tempfile::tempdir().unwrap();
        let path_a = dir.path().join("a.db");
        let path_b = dir.path().join("b.db");
        Db::open(&path_a).unwrap();
        Db::open(&path_b).unwrap();

        let pool_a = DbPool::new(Db::open(&path_a).unwrap()).unwrap();
        let pool_b = DbPool::new(Db::open(&path_b).unwrap()).unwrap();

        pool_a
            .with(|db| {
                assert_eq!(db.path().unwrap(), path_a);
                Ok(())
            })
            .unwrap();
        pool_b
            .with(|db| {
                assert_eq!(db.path().unwrap(), path_b);
                Ok(())
            })
            .unwrap();
    }

    #[test]
    fn per_thread_pool_for_file_db() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("m.db");
        Db::open(&path).unwrap(); // create + migrate (writer, sets WAL)
        let pool = DbPool::new(Db::open(&path).unwrap()).unwrap();
        // Used from a different thread: that thread opens its own read connection.
        let r = std::thread::scope(|s| {
            s.spawn(|| pool.with(|db| Ok(db.data_version()?)).unwrap())
                .join()
                .unwrap()
        });
        assert!(r >= 0);
    }

    #[test]
    fn reentrant_with_does_not_panic() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("re.db");
        Db::open(&path).unwrap();
        let pool = DbPool::new(Db::open(&path).unwrap()).unwrap();
        let r: Result<i64> = pool.with(|_outer| pool.with(|db| Ok(db.data_version()?)));
        assert!(r.is_ok(), "re-entrant with() must not panic or error");
    }

    #[test]
    fn with_open_failure_includes_path_in_error() {
        let bad = std::path::PathBuf::from("/nonexistent-musefs-dir/does-not-exist.db");
        let pool = DbPool::PerThread {
            path: bad.clone(),
            poll: Box::new(ReentrantMutex::new(
                Db::open_in_memory().unwrap().into_read_only(),
            )),
            conns: Mutex::new(HashMap::new()),
        };
        let msg = pool.with(|_db| Ok(())).unwrap_err().to_string();
        assert!(
            msg.contains("/nonexistent-musefs-dir/does-not-exist.db"),
            "open error must name the failing path, got: {msg}"
        );
    }

    #[test]
    fn nested_with_on_shared_pool() {
        let pool = DbPool::new(Db::open_in_memory().unwrap()).unwrap();
        let r: Result<i64> = pool.with(|_outer| pool.with(|db| Ok(db.data_version()?)));
        assert!(r.is_ok(), "nested with on Shared must not deadlock");
    }

    #[test]
    fn with_poll_inside_with_on_shared_pool() {
        let pool = DbPool::new(Db::open_in_memory().unwrap()).unwrap();
        let r: Result<i64> = pool.with(|_outer| pool.with_poll(|db| Ok(db.data_version()?)));
        assert!(
            r.is_ok(),
            "with_poll inside with on Shared must not deadlock"
        );
    }

    #[test]
    fn nested_with_poll_on_per_thread_pool() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("np.db");
        Db::open(&path).unwrap();
        let pool = DbPool::new(Db::open(&path).unwrap()).unwrap();
        let r: Result<i64> = pool.with_poll(|_outer| pool.with_poll(|db| Ok(db.data_version()?)));
        assert!(r.is_ok(), "nested with_poll on PerThread must not deadlock");
    }
}
