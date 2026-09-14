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

/// Hand a claim back: return `conn` to `NORMAL` locking. SQLite drops the
/// exclusive locks only on the next access to the database file, so one is made
/// here rather than left to whatever the caller does next.
fn release_exclusive(conn: &Connection) -> Result<()> {
    conn.pragma_update(None, "locking_mode", "normal")?;
    conn.query_row("SELECT count(*) FROM sqlite_master", [], |r| {
        r.get::<_, i64>(0)
    })?;
    Ok(())
}

/// Write a compacted, consistent copy of the store to `dest` in one statement.
///
/// `VACUUM INTO` runs against a read transaction, so it neither blocks a
/// serving mount nor needs one to stop. `dest` must not exist; SQLite refuses
/// to overwrite, which is the behaviour a backup wants.
///
/// A destination that is not valid UTF-8 is refused rather than lossily
/// converted: `to_string_lossy` would substitute U+FFFD and SQLite would write
/// a perfectly good backup to a path the caller never named, which the caller
/// would then report as the snapshot it can fall back on.
pub(crate) fn snapshot_into(conn: &Connection, dest: &Path, op: &'static str) -> Result<()> {
    let Some(dest_str) = dest.to_str() else {
        return Err(DbError::Sqlite(rusqlite::Error::InvalidPath(
            dest.to_path_buf(),
        )));
    };
    conn.execute("VACUUM INTO ?1", [dest_str])
        .map_err(|e| map_busy(e, op))?;
    Ok(())
}

impl Db<ReadWrite> {
    /// Compact the store: reclaim free pages left by deletions, then truncate
    /// the WAL. Runs a full `VACUUM` (rewrites the whole database — transiently
    /// needs free disk roughly equal to the store size) followed by
    /// `PRAGMA wal_checkpoint(TRUNCATE)`. The TRUNCATE checkpoint *after* VACUUM
    /// is what actually shrinks the main `.db` file on disk and zeroes the
    /// `-wal`.
    ///
    /// The store is claimed first, exactly as `migrate` claims it
    /// (`claim_exclusive`), and refused with [`DbError::StoreInUse`] if anything
    /// else has it open (#721). Mapping a busy `VACUUM` alone was not that check:
    /// in WAL mode a mount idle between reads holds no lock, so the rewrite ran
    /// underneath it. The claim is held through the vacuum and the checkpoint,
    /// which is also why the checkpoint's result row can be discarded — with no
    /// other connection attached, nothing can leave it unable to finish.
    ///
    /// Afterwards the connection goes back to the locking mode it was in, so a
    /// long-lived caller does not keep every other reader locked out for as long
    /// as the `Db` stays open. A caller that already held the store — `musefs
    /// migrate` vacuums under its own claim — keeps it.
    pub fn vacuum(&self) -> Result<()> {
        let mode: String = self
            .conn
            .pragma_query_value(None, "locking_mode", |r| r.get(0))?;
        claim_exclusive(&self.conn, "vacuuming")?;
        let vacuumed = self
            .conn
            .execute_batch("VACUUM")
            .and_then(|()| self.conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)"))
            .map_err(|e| map_busy(e, "vacuuming"));
        let released = if mode.eq_ignore_ascii_case("exclusive") {
            Ok(())
        } else {
            release_exclusive(&self.conn)
        };
        vacuumed.and(released)
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

    /// #721: a store another connection has open is refused — including one
    /// that did a read and went idle, which is what a mount looks like between
    /// serving two files, and a read-only one, which is what most of a mount's
    /// connections are. The vacuum used to run to completion under both.
    /// A vacuum hands the store back when it is done: another connection can
    /// take a write lock afterwards, rather than finding the store held for as
    /// long as this `Db` stays open.
    #[test]
    fn vacuum_releases_the_store_when_it_is_done() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.db");
        let db = Db::open(&path).unwrap();
        db.vacuum().unwrap();

        let mode: String = db
            .conn
            .pragma_query_value(None, "locking_mode", |r| r.get(0))
            .unwrap();
        assert!(mode.eq_ignore_ascii_case("normal"), "locking_mode {mode}");
        let other = rusqlite::Connection::open(&path).unwrap();
        other
            .busy_timeout(std::time::Duration::from_millis(200))
            .unwrap();
        other
            .execute_batch("BEGIN IMMEDIATE; COMMIT")
            .expect("the vacuuming connection no longer holds the store");
    }

    /// Under a claim its caller already holds, as `migrate`'s is, the vacuum
    /// leaves the claim in place.
    #[test]
    fn vacuum_keeps_a_claim_the_caller_already_held() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.db");
        let db = Db::open(&path).unwrap();
        super::claim_exclusive(&db.conn, "migrating").unwrap();
        db.vacuum().unwrap();

        let other = rusqlite::Connection::open(&path).unwrap();
        other
            .busy_timeout(std::time::Duration::from_millis(50))
            .unwrap();
        assert!(
            other.execute_batch("BEGIN IMMEDIATE; COMMIT").is_err(),
            "the caller's claim must survive the vacuum"
        );
    }

    #[test]
    fn vacuum_refuses_a_store_another_connection_has_open() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.db");
        let db = Db::open(&path).unwrap();
        db.conn
            .busy_timeout(std::time::Duration::from_millis(50))
            .unwrap();

        let idle = rusqlite::Connection::open(&path).unwrap();
        idle.query_row("SELECT count(*) FROM tracks", [], |r| r.get::<_, i64>(0))
            .unwrap();
        let err = db.vacuum().unwrap_err();
        assert!(
            matches!(
                err,
                DbError::StoreInUse {
                    op: "vacuuming",
                    ..
                }
            ),
            "{err:?}"
        );
        drop(idle);

        let reader = Db::open_readonly(&path).unwrap();
        let err = db.vacuum().unwrap_err();
        assert!(
            matches!(
                err,
                DbError::StoreInUse {
                    op: "vacuuming",
                    ..
                }
            ),
            "{err:?}"
        );
        drop(reader);

        db.vacuum().expect("nobody else has the store now");
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
