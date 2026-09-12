//! Store maintenance operations: compaction (`VACUUM` + WAL checkpoint),
//! snapshots, and the exclusive-use probe the invasive operations pre-flight
//! with.

use std::path::Path;

use rusqlite::Connection;

use crate::{Db, DbError, ReadWrite, Result};

/// Take the store for `conn` alone, refusing with [`DbError::StoreInUse`] if
/// any other connection has it open. The claim is held until `conn` is dropped.
///
/// `PRAGMA locking_mode = EXCLUSIVE` is what does it, and it is a stronger
/// check than "is anyone holding a lock right now": in WAL mode it needs the
/// shared-memory index to itself, so a mount sitting *idle* between reads is
/// caught too, where a busy-lock probe would wave it through. Holding the claim
/// rather than releasing it also closes the window between the check and the
/// work — nothing can attach to the store part-way through a rewrite of it.
///
/// What it cannot see is a connection that has been opened and never used:
/// SQLite attaches the index on the first statement, not at open. That is a
/// process about to use the store rather than one using it, and it is excluded
/// from the moment it tries.
///
/// The pragma alone is lazy: SQLite takes the locks on the next transaction, so
/// one is forced here. Without that the refusal would land somewhere later,
/// after the user had already been asked to agree to the upgrade.
pub(crate) fn claim_exclusive(conn: &Connection, op: &'static str) -> Result<()> {
    conn.pragma_update(None, "locking_mode", "exclusive")
        .map_err(|e| map_busy(e, op))?;
    conn.execute_batch("BEGIN IMMEDIATE; COMMIT")
        .map_err(|e| map_busy(e, op))?;
    Ok(())
}

/// Write a compacted, consistent copy of the store to `dest` in one statement.
///
/// `VACUUM INTO` runs against a read transaction, so it neither blocks a
/// serving mount nor needs one to stop. `dest` must not exist; SQLite refuses
/// to overwrite, which is the behaviour a backup wants.
pub(crate) fn snapshot_into(conn: &Connection, dest: &Path, op: &'static str) -> Result<()> {
    conn.execute("VACUUM INTO ?1", [&*dest.to_string_lossy()])
        .map_err(|e| map_busy(e, op))?;
    Ok(())
}

impl Db<ReadWrite> {
    /// Compact the store: reclaim free pages left by deletions, then truncate
    /// the WAL. Runs a full `VACUUM` (rewrites the whole database — transiently
    /// needs free disk roughly equal to the store size) followed by
    /// `PRAGMA wal_checkpoint(TRUNCATE)`. The TRUNCATE checkpoint *after* VACUUM
    /// is what actually shrinks the main `.db` file on disk and zeroes the
    /// `-wal`. A busy/locked store (e.g. a live mount) maps to
    /// [`DbError::StoreInUse`].
    pub fn vacuum(&self) -> Result<()> {
        self.conn
            .execute_batch("VACUUM")
            .map_err(|e| map_busy(e, "vacuuming"))?;
        self.conn
            .execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")
            .map_err(|e| map_busy(e, "vacuuming"))?;
        Ok(())
    }
}

/// Translate a maintenance error: a SQLite busy/locked failure means the store
/// is open elsewhere (a mount or scan), surfaced as the actionable
/// [`DbError::StoreInUse`] naming `op`; everything else flows through the
/// transparent rusqlite variant.
pub(crate) fn map_busy(err: rusqlite::Error, op: &'static str) -> DbError {
    if let rusqlite::Error::SqliteFailure(e, _) = &err
        && matches!(
            e.code,
            rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked
        )
    {
        return DbError::StoreInUse { op, source: err };
    }
    DbError::Sqlite(err)
}

#[cfg(test)]
mod tests {
    use super::map_busy;
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
    fn map_busy_maps_busy_and_locked_to_store_in_use() {
        use rusqlite::{Error, ffi};
        let busy = Error::SqliteFailure(ffi::Error::new(ffi::SQLITE_BUSY), None);
        assert!(matches!(
            map_busy(busy, "vacuuming"),
            DbError::StoreInUse {
                op: "vacuuming",
                ..
            }
        ));
        let locked = Error::SqliteFailure(ffi::Error::new(ffi::SQLITE_LOCKED), None);
        assert!(matches!(
            map_busy(locked, "migrating"),
            DbError::StoreInUse {
                op: "migrating",
                ..
            }
        ));
    }

    /// One variant, one message per operation: the remedy is the same but the
    /// verb has to be the one the user just typed.
    #[test]
    fn the_in_use_message_names_the_operation_it_refused() {
        use rusqlite::{Error, ffi};
        let err = map_busy(
            Error::SqliteFailure(ffi::Error::new(ffi::SQLITE_BUSY), None),
            "migrating",
        );
        assert_eq!(
            err.to_string(),
            "the store is in use — unmount the filesystem or stop any scan before migrating"
        );
    }

    #[test]
    fn map_busy_passes_through_other_errors() {
        use rusqlite::{Error, ffi};
        let corrupt = Error::SqliteFailure(ffi::Error::new(ffi::SQLITE_CORRUPT), None);
        assert!(matches!(map_busy(corrupt, "vacuuming"), DbError::Sqlite(_)));
    }
}
