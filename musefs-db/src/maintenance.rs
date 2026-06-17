//! Store maintenance operations: compaction (`VACUUM` + WAL checkpoint).

use crate::{Db, DbError, ReadWrite, Result};

impl Db<ReadWrite> {
    /// Compact the store: reclaim free pages left by deletions, then truncate
    /// the WAL. Runs a full `VACUUM` (rewrites the whole database — transiently
    /// needs free disk roughly equal to the store size) followed by
    /// `PRAGMA wal_checkpoint(TRUNCATE)`. The TRUNCATE checkpoint *after* VACUUM
    /// is what actually shrinks the main `.db` file on disk and zeroes the
    /// `-wal`. A busy/locked store (e.g. a live mount) maps to
    /// [`DbError::StoreInUse`].
    pub fn vacuum(&self) -> Result<()> {
        self.conn.execute_batch("VACUUM").map_err(map_vacuum_err)?;
        self.conn
            .execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")
            .map_err(map_vacuum_err)?;
        Ok(())
    }
}

/// Translate a VACUUM/checkpoint error: a SQLite busy/locked failure means the
/// store is open elsewhere (a mount or scan), surfaced as the actionable
/// [`DbError::StoreInUse`]; everything else flows through the transparent
/// rusqlite variant.
fn map_vacuum_err(err: rusqlite::Error) -> DbError {
    if let rusqlite::Error::SqliteFailure(e, _) = &err
        && matches!(
            e.code,
            rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked
        )
    {
        return DbError::StoreInUse(err);
    }
    DbError::Sqlite(err)
}

#[cfg(test)]
mod tests {
    use super::map_vacuum_err;
    use crate::models::NewArt;
    use crate::{Db, DbError};

    #[test]
    fn vacuum_shrinks_file_and_truncates_wal_after_deletion() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.db");
        let db = Db::open(&path).unwrap();

        // Allocate many pages: 16 distinct 256 KiB art blobs (~4 MiB).
        for i in 0..16u8 {
            db.upsert_art(&NewArt {
                mime: "image/png".into(),
                width: None,
                height: None,
                data: vec![i; 256 * 1024],
            })
            .unwrap();
        }
        // None are linked to a track, so they are all orphan: free their pages.
        assert_eq!(db.gc_orphan_art().unwrap(), 16);

        // Settle the WAL so the pre-vacuum main-file size reflects the deletes.
        db.conn
            .execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")
            .unwrap();
        let before = std::fs::metadata(&path).unwrap().len();

        db.vacuum().unwrap();

        let after = std::fs::metadata(&path).unwrap().len();
        assert!(after < before, "expected shrink: {before} -> {after}");

        let freelist: i64 = db
            .conn
            .query_row("PRAGMA freelist_count", [], |r| r.get(0))
            .unwrap();
        assert_eq!(freelist, 0, "vacuum must leave no free pages");

        // The TRUNCATE checkpoint inside vacuum() must drain the WAL: a
        // subsequent checkpoint reports 0 frames in the log (column 1 of
        // `PRAGMA wal_checkpoint` is the WAL frame count). Deterministic, and
        // unlike a `-wal` file-size check it does not depend on WAL internals.
        // Without the in-method checkpoint, VACUUM's frames are still pending
        // here, so this is non-zero and the checkpoint-removal mutant dies.
        let wal_frames: i64 = db
            .conn
            .query_row("PRAGMA wal_checkpoint(PASSIVE)", [], |r| r.get(1))
            .unwrap();
        assert_eq!(wal_frames, 0, "vacuum must checkpoint the WAL");
    }

    #[test]
    fn vacuum_on_empty_store_is_ok() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(dir.path().join("t.db")).unwrap();
        db.vacuum().unwrap();
    }

    #[test]
    fn map_vacuum_err_maps_busy_and_locked_to_store_in_use() {
        use rusqlite::{Error, ffi};
        let busy = Error::SqliteFailure(ffi::Error::new(ffi::SQLITE_BUSY), None);
        assert!(matches!(map_vacuum_err(busy), DbError::StoreInUse(_)));
        let locked = Error::SqliteFailure(ffi::Error::new(ffi::SQLITE_LOCKED), None);
        assert!(matches!(map_vacuum_err(locked), DbError::StoreInUse(_)));
    }

    #[test]
    fn map_vacuum_err_passes_through_other_errors() {
        use rusqlite::{Error, ffi};
        let corrupt = Error::SqliteFailure(ffi::Error::new(ffi::SQLITE_CORRUPT), None);
        assert!(matches!(map_vacuum_err(corrupt), DbError::Sqlite(_)));
    }
}
