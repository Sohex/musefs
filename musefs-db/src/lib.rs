mod art;
mod bulk;
mod error;
mod models;
mod schema;
mod structural;
mod tags;
mod tracks;

pub use bulk::BulkWriter;
pub use error::{DbError, Result};
pub use models::{
    Art, ArtMeta, BinaryTag, BinaryTagRow, Format, NewArt, NewTrack, StructuralBlock, Tag, Track,
    TrackArt,
};

use rusqlite::Connection;
use std::path::{Path, PathBuf};
use std::time::Duration;

pub struct Db {
    conn: Connection,
    path: Option<PathBuf>,
}

impl Db {
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Db> {
        let p = path.as_ref().to_path_buf();
        let mut conn = Connection::open(&p)?;
        Self::configure(&mut conn, true)?;
        Ok(Db {
            conn,
            path: Some(p),
        })
    }

    pub fn open_in_memory() -> Result<Db> {
        let mut conn = Connection::open_in_memory()?;
        Self::configure(&mut conn, false)?;
        Ok(Db { conn, path: None })
    }

    /// Apply shared connection pragmas, then migrate. `wal` enables write-ahead
    /// logging (file-backed DBs only) so a reader (the FUSE mount) and a writer
    /// (e.g. a beets-plugin sync) don't block each other; the busy timeout lets
    /// brief lock contention retry instead of failing immediately with
    /// SQLITE_BUSY.
    fn configure(conn: &mut Connection, wal: bool) -> Result<()> {
        conn.busy_timeout(Duration::from_secs(5))?;
        conn.pragma_update(None, "foreign_keys", true)?;
        if wal {
            // journal_mode returns the resulting mode; query_row consumes it
            // (pragma_update would error on a result-returning pragma).
            let _: String = conn.query_row("PRAGMA journal_mode = WAL", [], |r| r.get(0))?;
        }
        schema::migrate(conn)?;
        Ok(())
    }

    pub fn user_version(&self) -> Result<i64> {
        Ok(self
            .conn
            .pragma_query_value(None, "user_version", |r| r.get(0))?)
    }

    pub fn data_version(&self) -> Result<i64> {
        Ok(self
            .conn
            .pragma_query_value(None, "data_version", |r| r.get(0))?)
    }

    /// The backing file path, or `None` for an in-memory database.
    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// Open an additional read-only connection to an existing file-backed DB.
    /// WAL (set by the writer) lets these run concurrently without blocking.
    /// No migration is run — the schema already exists and the connection is RO.
    /// Note: even with `SQLITE_OPEN_READ_ONLY`, SQLite needs write access to the
    /// directory (to create/use the `-shm` wal-index) when the DB is in WAL mode;
    /// a strictly read-only DB directory will make this fail.
    pub fn open_readonly<P: AsRef<Path>>(path: P) -> Result<Db> {
        let p = path.as_ref().to_path_buf();
        let conn = Connection::open_with_flags(&p, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        // No configure()/migrate and no foreign_keys pragma: the schema already
        // exists and no writes are possible on a read-only connection.
        conn.busy_timeout(Duration::from_secs(5))?;
        Ok(Db {
            conn,
            path: Some(p),
        })
    }
}

#[cfg(feature = "mutants")]
impl Default for Db {
    /// Test-only (the `mutants` feature). An in-memory, **unmigrated** connection
    /// (so `user_version == 0`, distinct from the always-migrated `1`). Sets the
    /// FK/busy-timeout pragmas like a real connection, but runs no migration, so it
    /// has **no schema**. Use only for the version-0 kill and to let
    /// `Ok(Default::default())` mutants compile; behavioral tests use
    /// `open_in_memory()`.
    fn default() -> Self {
        let conn = Connection::open_in_memory().expect("in-memory sqlite open");
        conn.busy_timeout(Duration::from_secs(5))
            .expect("set busy_timeout");
        conn.pragma_update(None, "foreign_keys", true)
            .expect("enable foreign_keys");
        Db { conn, path: None }
    }
}

#[cfg(test)]
mod tests {
    use super::Db;

    #[test]
    fn open_uses_wal_and_busy_timeout() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(dir.path().join("t.db")).unwrap();
        let mode: String = db
            .conn
            .pragma_query_value(None, "journal_mode", |r| r.get(0))
            .unwrap();
        assert_eq!(mode.to_lowercase(), "wal");
        let timeout: i64 = db
            .conn
            .pragma_query_value(None, "busy_timeout", |r| r.get(0))
            .unwrap();
        assert_eq!(timeout, 5000);
    }

    #[test]
    fn in_memory_sets_busy_timeout_without_wal() {
        let db = Db::open_in_memory().unwrap();
        let timeout: i64 = db
            .conn
            .pragma_query_value(None, "busy_timeout", |r| r.get(0))
            .unwrap();
        assert_eq!(timeout, 5000);
        let mode: String = db
            .conn
            .pragma_query_value(None, "journal_mode", |r| r.get(0))
            .unwrap();
        assert_ne!(mode.to_lowercase(), "wal");
    }

    #[test]
    fn open_readonly_can_read_a_file_db() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("m.db");
        {
            let w = Db::open(&path).unwrap();
            assert!(w.path().is_some());
        }
        let r = Db::open_readonly(&path).unwrap();
        // A read-only connection can run a read pragma without error.
        assert!(r.data_version().is_ok());
        assert_eq!(r.path().unwrap(), path.as_path());
    }

    #[test]
    fn in_memory_has_no_path() {
        let db = Db::open_in_memory().unwrap();
        assert!(db.path().is_none());
    }
}
