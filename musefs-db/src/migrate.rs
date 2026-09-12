//! The gated store upgrade, held open for its pre-flight (#705).
//!
//! A gated migration is one nobody should get as a side effect of `mount` or
//! `scan`: it rewrites data the user did not ask to have rewritten, transiently
//! needs the store's size again in free disk, and ends compatibility with every
//! older musefs build. `musefs migrate` is where that happens deliberately, and
//! this module is what the command inspects the store through before it commits
//! to anything.

use std::marker::PhantomData;
use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::Connection;

use crate::{Db, Result, maintenance, schema};

/// The present participle every refusal raised through this module reports.
const OP: &str = "migrating";

/// A store opened for a gated schema upgrade, before the upgrade runs.
///
/// Deliberately not a [`Db`]: the schema on disk is by definition not the one
/// this build expects, so every row reader and writer in the crate would be
/// pointed at a shape it does not know. What is exposed instead is only what a
/// pre-flight needs — the versions, the steps, whether anyone else has the
/// store open, and a snapshot — plus [`PendingMigration::apply`], which runs
/// the migration and hands back an ordinary `Db`.
#[derive(Debug)]
pub struct PendingMigration {
    conn: Connection,
    path: PathBuf,
    current: i64,
}

impl PendingMigration {
    /// Open the store at `path` **without** migrating it.
    ///
    /// The identity check every other open performs is skipped for the same
    /// reason the migration is: the schema is not yet the one this build
    /// validates against. A store from a *newer* build is still refused, since
    /// no amount of migrating fixes that direction.
    ///
    /// Opened without `SQLITE_OPEN_CREATE`, which is the one way these flags
    /// differ from the default every other constructor in this crate takes.
    /// There is nothing to migrate about a store that does not exist, and
    /// creating one here would turn a typo'd `--db` into a brand new empty
    /// library reported as a successful upgrade.
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self> {
        use rusqlite::OpenFlags;

        let path = path.as_ref().to_path_buf();
        // Spelled as the default minus one flag rather than as a list, so this
        // says the same thing the paragraph above does and cannot drift if
        // rusqlite's default gains a flag.
        let conn = Connection::open_with_flags(
            &path,
            OpenFlags::default().difference(OpenFlags::SQLITE_OPEN_CREATE),
        )?;
        conn.busy_timeout(Duration::from_secs(5))?;
        conn.pragma_update(None, "foreign_keys", true)?;
        let current: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
        if current > schema::LATEST_VERSION {
            return Err(crate::DbError::StoreTooNew {
                found: current,
                supported: schema::LATEST_VERSION,
            });
        }
        Ok(Self {
            conn,
            path,
            current,
        })
    }

    /// The `user_version` the store carries now.
    pub fn current_version(&self) -> i64 {
        self.current
    }

    /// The `user_version` it carries once [`PendingMigration::apply`] returns.
    pub fn target_version(&self) -> i64 {
        schema::LATEST_VERSION
    }

    /// Every step this store has yet to receive, in the order they run.
    pub fn pending(&self) -> Vec<schema::PendingStep> {
        schema::pending(self.current)
    }

    /// Whether there is nothing to do.
    pub fn is_current(&self) -> bool {
        self.pending().is_empty()
    }

    /// The store file this was opened from.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Take the store for this handle alone, refusing with
    /// [`crate::DbError::StoreInUse`] if any other connection — a mount, a
    /// scan, another `musefs migrate` — has it open, idle or not.
    ///
    /// The claim is held until the handle is dropped, which for `musefs
    /// migrate` means through the snapshot and the migration. Nothing can
    /// attach to the store in between, and the connection
    /// [`PendingMigration::apply`] hands on carries the claim with it, so the
    /// command holds the store until it exits.
    pub fn claim_exclusive(&self) -> Result<()> {
        maintenance::claim_exclusive(&self.conn, OP)
    }

    /// Write a compacted, consistent copy of the store to `dest`, which must
    /// not already exist.
    ///
    /// This is the remedy that makes the upgrade reversible: one `VACUUM INTO`
    /// against a read transaction, which on a reference-shaped library measures
    /// under two seconds.
    pub fn snapshot_to(&self, dest: &Path) -> Result<()> {
        maintenance::snapshot_into(&self.conn, dest, OP)
    }

    /// Run every pending step, gated ones included, and hand back the migrated
    /// store. The identity check runs here, against the shape the migration was
    /// supposed to produce.
    ///
    /// This is where the connection stops being a migration handle and becomes
    /// an ordinary [`Db`], so it picks up the one pragma [`Db::open`] sets that
    /// the pre-flight had no use for: write-ahead logging, which is what keeps
    /// a reader and a writer off each other's backs. A musefs store is already
    /// in WAL — the mode is persistent and every other open sets it — so this
    /// is belt and braces for a store that arrived some other way.
    pub fn apply(mut self) -> Result<Db> {
        let _: String = self
            .conn
            .query_row("PRAGMA journal_mode = WAL", [], |r| r.get(0))?;
        schema::migrate_all(&mut self.conn)?;
        schema::validate_identity(&self.conn)?;
        Ok(Db::from_migrated(self.conn, self.path))
    }
}

impl Db {
    /// Adopt the connection a completed [`PendingMigration`] leaves behind.
    /// Private to the crate: every other route to a `Db` goes through an open
    /// that migrates and validates.
    pub(crate) fn from_migrated(conn: Connection, path: PathBuf) -> Db {
        Db {
            conn,
            path: Some(path),
            _mode: PhantomData,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::PendingMigration;
    use crate::{Db, DbError, LATEST_VERSION};

    /// Rewind a freshly-created store to the version before the gated step —
    /// the state an older musefs build leaves behind. The gated step rewrites
    /// column values rather than the schema's shape, so the stamp is the whole
    /// difference.
    fn gated_store(path: &std::path::Path) {
        Db::open(path).unwrap();
        let conn = rusqlite::Connection::open(path).unwrap();
        conn.pragma_update(None, "user_version", LATEST_VERSION - 1)
            .unwrap();
    }

    #[test]
    fn opening_does_not_migrate_and_applying_does() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("library.db");
        gated_store(&path);

        let pending = PendingMigration::open(&path).unwrap();
        assert_eq!(pending.current_version(), LATEST_VERSION - 1);
        assert_eq!(pending.target_version(), LATEST_VERSION);
        assert!(!pending.is_current());
        assert_eq!(pending.path(), path);

        // Still unmigrated on disk: the pre-flight has to be able to report and
        // refuse before anything is written.
        let seen: i64 = rusqlite::Connection::open(&path)
            .unwrap()
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap();
        assert_eq!(seen, LATEST_VERSION - 1);

        let db = pending.apply().unwrap();
        assert_eq!(db.user_version().unwrap(), LATEST_VERSION);
    }

    /// The step list is what `musefs migrate` prints before asking to proceed,
    /// so it has to name the gated step as gated.
    #[test]
    fn pending_reports_the_gated_step() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("library.db");
        gated_store(&path);

        let pending = PendingMigration::open(&path).unwrap();
        let steps = pending.pending();
        assert_eq!(steps.len(), 1);
        assert_eq!(steps[0].version, LATEST_VERSION);
        assert!(steps[0].gated);
        assert!(!steps[0].summary.is_empty());
    }

    #[test]
    fn a_store_already_current_has_nothing_pending() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("library.db");
        Db::open(&path).unwrap();

        let pending = PendingMigration::open(&path).unwrap();
        assert!(pending.is_current());
        assert!(pending.pending().is_empty());
        // Applying anyway is a no-op, not an error: the command reports
        // "nothing to do" rather than failing a scripted upgrade twice run.
        assert_eq!(
            pending.apply().unwrap().user_version().unwrap(),
            LATEST_VERSION
        );
    }

    /// Migrating cannot fix a store written by a newer build, so this direction
    /// is refused at open rather than after the user has agreed to an upgrade.
    #[test]
    fn a_store_from_a_newer_build_is_refused_at_open() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("library.db");
        Db::open(&path).unwrap();
        let future = LATEST_VERSION + 1;
        {
            let conn = rusqlite::Connection::open(&path).unwrap();
            conn.pragma_update(None, "user_version", future).unwrap();
        }

        let err = PendingMigration::open(&path).unwrap_err();
        assert!(
            matches!(err, DbError::StoreTooNew { found, .. } if found == future),
            "{err:?}"
        );
    }

    /// There is nothing to migrate about a store that does not exist, and a
    /// typo'd path must not become a brand new empty library that `apply` then
    /// reports as a successful upgrade.
    #[test]
    fn a_missing_store_is_refused_and_not_created() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nope.db");

        PendingMigration::open(&path).expect_err("there is no store here");

        assert!(!path.exists(), "opening must not create the store");
    }

    /// A lossy conversion would hand SQLite a path the caller never named, and
    /// the caller would then report that as the snapshot it can fall back on.
    #[cfg(unix)]
    #[test]
    fn a_snapshot_destination_that_is_not_utf8_is_refused() {
        use std::os::unix::ffi::OsStrExt as _;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("library.db");
        gated_store(&path);

        let dest = dir
            .path()
            .join(std::ffi::OsStr::from_bytes(b"snap\xff\xfe.bak"));
        let pending = PendingMigration::open(&path).unwrap();
        let err = pending.snapshot_to(&dest).unwrap_err();

        assert!(
            matches!(
                err,
                DbError::Sqlite(rusqlite::Error::InvalidPath(ref p)) if *p == dest
            ),
            "{err:?}"
        );
        assert!(!dest.exists(), "nothing may be written under another name");
    }

    #[test]
    fn the_snapshot_is_a_usable_store_and_refuses_to_overwrite() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("library.db");
        gated_store(&path);
        let backup = dir.path().join("library.db.bak");

        let pending = PendingMigration::open(&path).unwrap();
        pending.snapshot_to(&backup).unwrap();

        // A second snapshot to the same path must not clobber the first.
        pending
            .snapshot_to(&backup)
            .expect_err("VACUUM INTO must refuse an existing destination");

        // The copy is the pre-migration store, openable at the old version.
        let copy = PendingMigration::open(&backup).unwrap();
        assert_eq!(copy.current_version(), LATEST_VERSION - 1);
    }

    /// A mount, a scan, or a second `musefs migrate` holding the store means it
    /// is not ours to rewrite, and the pre-flight has to say so before the user
    /// is asked to agree to anything. Holding no lock is not enough to pass — a
    /// mount idle between reads is still a mount.
    #[test]
    fn a_store_another_connection_has_open_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("library.db");
        gated_store(&path);

        let other = rusqlite::Connection::open(&path).unwrap();
        other
            .busy_timeout(std::time::Duration::from_millis(50))
            .unwrap();
        // One read, then idle: no transaction and no lock held, which is what a
        // mount looks like between serving two files. Caught all the same.
        other
            .query_row("SELECT count(*) FROM tracks", [], |r| r.get::<_, i64>(0))
            .unwrap();

        let pending = PendingMigration::open(&path).unwrap();
        pending
            .conn
            .busy_timeout(std::time::Duration::from_millis(50))
            .unwrap();
        let err = pending.claim_exclusive().unwrap_err();
        assert!(
            matches!(
                err,
                DbError::StoreInUse {
                    op: "migrating",
                    ..
                }
            ),
            "{err:?}"
        );

        drop(other);

        // The connections a mount actually holds most of are read-only, and a
        // WAL reader needs the same shared-memory index, so those are caught
        // too. This is the case the whole check exists for: rewriting the
        // schema under a mount that is serving from it.
        let reader = Db::open_readonly(&path).unwrap();
        let err = pending.claim_exclusive().unwrap_err();
        assert!(
            matches!(
                err,
                DbError::StoreInUse {
                    op: "migrating",
                    ..
                }
            ),
            "{err:?}"
        );
        drop(reader);

        pending
            .claim_exclusive()
            .expect("nobody else has the store now");

        // And the claim is held, not released: nothing may attach while the
        // migration is in flight.
        let late = rusqlite::Connection::open(&path).unwrap();
        late.busy_timeout(std::time::Duration::from_millis(50))
            .unwrap();
        late.query_row("SELECT count(*) FROM tracks", [], |r| r.get::<_, i64>(0))
            .expect_err("the claim must exclude a connection made after it");
    }

    /// The claim has to survive everything the command does under it, or it
    /// would have to be released and retaken at the worst possible moment.
    #[test]
    fn the_claim_survives_the_snapshot_and_the_migration() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("library.db");
        gated_store(&path);

        let pending = PendingMigration::open(&path).unwrap();
        pending.claim_exclusive().unwrap();
        pending
            .snapshot_to(&dir.path().join("library.db.bak"))
            .unwrap();
        let db = pending.apply().unwrap();
        assert_eq!(db.user_version().unwrap(), LATEST_VERSION);
    }
}
