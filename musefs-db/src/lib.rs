mod art;
mod error;
mod models;
mod schema;
mod tags;
mod tracks;

pub use error::{DbError, Result};
pub use models::{Art, ArtMeta, Format, NewArt, NewTrack, Tag, Track, TrackArt};

use rusqlite::Connection;
use std::path::Path;
use std::time::Duration;

pub struct Db {
    conn: Connection,
}

impl Db {
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Db> {
        let mut conn = Connection::open(path)?;
        Self::configure(&mut conn, true)?;
        Ok(Db { conn })
    }

    pub fn open_in_memory() -> Result<Db> {
        let mut conn = Connection::open_in_memory()?;
        Self::configure(&mut conn, false)?;
        Ok(Db { conn })
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
}
