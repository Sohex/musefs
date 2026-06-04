//! Hands a read connection to whichever thread needs one.
//!
//! - File-backed DB → each thread lazily opens its own read-only connection
//!   (WAL makes concurrent readers contention-free; the worker pool is bounded,
//!   so the connection count is bounded).
//! - In-memory DB (tests) cannot be reopened by path, so a single connection is
//!   shared behind a mutex.
//!
//! Each thread opens a read connection per unique database path, so
//! multiple mounts (or test DBs) on the same thread don't collide.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use parking_lot::ReentrantMutex;

use musefs_db::{Db, ReadOnly};

use crate::error::{CoreError, Result};

static NEXT_POOL_ID: AtomicU64 = AtomicU64::new(1);

/// `with` and `with_poll` may nest freely, on any variant: `PerThread` reads
/// hand out thread-local `Rc` clones, and both mutexes are reentrant.
///
/// Lifetime contract (#127, accepted by design): `PerThread` read connections
/// are owned by the threads that opened them and live until those threads
/// exit. Dropping the pool clears only the dropping thread's connection, so a
/// caller that creates and drops many pools on long-lived shared threads
/// strands connections (one fd + SQLite cache each) until those threads exit.
/// Fine for musefs — a mount's worker pool lives exactly as long as the mount;
/// closing the gap would need a cross-thread registry, deliberately not built
/// (`Rc<Db>` is `!Send`, so another thread's entry is unreachable anyway).
pub enum DbPool {
    PerThread {
        id: u64,
        path: PathBuf,
        poll: ReentrantMutex<Db<ReadOnly>>,
    },
    Shared(Arc<ReentrantMutex<Db<ReadOnly>>>),
}

/// Clears only the *dropping thread's* thread-local connection for this pool —
/// see the lifetime contract on [`DbPool`].
impl Drop for DbPool {
    fn drop(&mut self) {
        if let DbPool::PerThread { id, path, .. } = self {
            let id = *id;
            let path = path.clone();
            PER_PATH.with(|cell| {
                cell.borrow_mut().remove(&(path, id));
            });
        }
    }
}

type PerPathMap = HashMap<(PathBuf, u64), Rc<Db<ReadOnly>>>;

thread_local! {
    static PER_PATH: RefCell<PerPathMap> = RefCell::new(HashMap::new());
}

impl DbPool {
    /// Build a pool from the DB used to construct the mount. File-backed DBs
    /// become per-thread pools (the passed connection is dropped — workers open
    /// their own); in-memory DBs are wrapped in a shared mutex.
    pub fn new(db: Db) -> Result<DbPool> {
        let db = db.into_read_only();
        match db.path() {
            Some(p) => Ok(DbPool::PerThread {
                id: NEXT_POOL_ID.fetch_add(1, Ordering::Relaxed),
                path: p.to_path_buf(),
                poll: ReentrantMutex::new(db),
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
            DbPool::PerThread { id, path, .. } => PER_PATH.with(|cell| {
                let db = {
                    let mut map = cell.borrow_mut();
                    if let std::collections::hash_map::Entry::Vacant(e) =
                        map.entry((path.clone(), *id))
                    {
                        let db = Db::open_readonly(path).map_err(|source| CoreError::DbOpen {
                            path: path.clone(),
                            source,
                        })?;
                        e.insert(Rc::new(db));
                    }
                    Rc::clone(map.get(&(path.clone(), *id)).unwrap())
                };
                f(&db)
            }),
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
            id: u64::MAX,
            path: bad.clone(),
            poll: ReentrantMutex::new(Db::open_in_memory().unwrap().into_read_only()),
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
