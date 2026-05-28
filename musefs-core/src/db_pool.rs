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
use std::sync::{Arc, Mutex};

use musefs_db::Db;

use crate::error::Result;

pub enum DbPool {
    PerThread { path: PathBuf, poll: Mutex<Db> },
    Shared(Arc<Mutex<Db>>),
}

thread_local! {
    static PER_PATH: RefCell<HashMap<PathBuf, Db>> = RefCell::new(HashMap::new());
}

impl DbPool {
    /// Build a pool from the DB used to construct the mount. File-backed DBs
    /// become per-thread pools (the passed connection is dropped — workers open
    /// their own); in-memory DBs are wrapped in a shared mutex.
    pub fn new(db: Db) -> Result<DbPool> {
        match db.path() {
            Some(p) => Ok(DbPool::PerThread {
                path: p.to_path_buf(),
                poll: Mutex::new(db),
            }),
            None => Ok(DbPool::Shared(Arc::new(Mutex::new(db)))),
        }
    }

    /// Run `f` with the persistent poll connection.
    ///
    /// For `PerThread` pools, `PRAGMA data_version` is connection-relative: a fresh
    /// thread-local connection starts at 0, so it can't detect changes that happened
    /// before it opened. The poll connection is the original writer Db, kept alive
    /// precisely so it can observe incremental changes from other connections.
    /// For `Shared` pools (in-memory), the single shared connection serves both roles.
    ///
    /// Note: `std::sync::Mutex` is not reentrant — do not call `with_poll` from
    /// inside a `with` closure on the `Shared` variant (it would deadlock).
    pub fn with_poll<R>(&self, f: impl FnOnce(&Db) -> Result<R>) -> Result<R> {
        match self {
            DbPool::PerThread { poll, .. } => f(&poll
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)),
            DbPool::Shared(m) => f(&m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)),
        }
    }

    /// Run `f` with a read connection.
    pub fn with<R>(&self, f: impl FnOnce(&Db) -> Result<R>) -> Result<R> {
        match self {
            DbPool::PerThread { path, .. } => PER_PATH.with(|cell| {
                let mut map = cell.borrow_mut();
                if !map.contains_key(path) {
                    let db = Db::open_readonly(path)?;
                    map.insert(path.clone(), db);
                }
                f(map.get(path).unwrap())
            }),
            DbPool::Shared(m) => {
                let db = m.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
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
}
