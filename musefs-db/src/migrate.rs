//! The gated store upgrade, held open for its pre-flight (#705).
//!
//! A gated migration is one nobody should get as a side effect of `mount` or
//! `scan`: it rewrites data the user did not ask to have rewritten, transiently
//! needs the store's size again in free disk, and ends compatibility with every
//! older musefs build. `musefs migrate` is where that happens deliberately, and
//! this module is what the command inspects the store through before it commits
//! to anything.

use std::cell::RefCell;
use std::collections::HashSet;
use std::marker::PhantomData;
use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::Connection;

use crate::{Db, Result, maintenance, schema};

/// The present participle every refusal raised through this module reports.
const OP: &str = "migrating";

/// How many rows of one rebuilt table the migrated schema would refuse.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableRejections {
    /// The table as the store names it.
    pub table: &'static str,
    /// Rows that would not survive the rebuild.
    pub rejected: u64,
}

/// Two `tracks` rows naming one backing path, once as TEXT and once as a BLOB.
///
/// Before 2.0.0 the column was TEXT, and `UNIQUE` never compares a TEXT value
/// equal to a BLOB, so a tool binding the path as bytes could add a second row
/// for a file musefs already had. The upgrade stores every path as bytes, which
/// makes the two one path, and only one row can keep it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct DuplicatePath {
    /// The row that keeps the path: the one carrying tags or art links, or the
    /// older one when neither does.
    pub kept: i64,
    /// The row refused for sharing it, which the `tracks` count includes.
    pub refused: i64,
    /// Both rows carry tags or art links, so [`PendingMigration::repair`] will
    /// not choose between them.
    pub ambiguous: bool,
}

/// What the migrated schema would refuse, per table. Empty is the normal case.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Rejections {
    tables: Vec<TableRejections>,
    duplicates: Vec<DuplicatePath>,
    relinked: u64,
}

impl Rejections {
    /// Every table with at least one refused row.
    pub fn tables(&self) -> &[TableRejections] {
        &self.tables
    }

    /// Every path two `tracks` rows share. Each refused row is also counted
    /// under `tracks`.
    pub fn duplicates(&self) -> &[DuplicatePath] {
        &self.duplicates
    }

    /// Picture links a repair moves onto a correctly filed `art` row holding the
    /// same bytes as the refused row they point at, instead of deleting them.
    /// They are not counted as refused.
    pub fn relinked(&self) -> u64 {
        self.relinked
    }

    /// Refused rows across every table.
    pub fn total(&self) -> u64 {
        self.tables.iter().map(|t| t.rejected).sum()
    }

    /// Whether the store migrates as it stands.
    pub fn is_empty(&self) -> bool {
        self.tables.is_empty()
    }
}

/// What one pass of the probe found, and what a repair does about it.
#[derive(Debug, Default)]
struct Findings {
    /// Rowids each rebuilt table refuses, in the order a repair deletes them.
    refused: Vec<(&'static str, Vec<i64>)>,
    /// `track_art` rowids, each with the correctly filed `art` id it moves onto.
    relinks: Vec<(i64, i64)>,
    duplicates: Vec<DuplicatePath>,
}

impl Findings {
    fn report(&self) -> Rejections {
        Rejections {
            tables: self
                .refused
                .iter()
                .map(|(table, rowids)| TableRejections {
                    table,
                    rejected: rowids.len() as u64,
                })
                .filter(|t| t.rejected > 0)
                .collect(),
            duplicates: self.duplicates.clone(),
            relinked: self.relinks.len() as u64,
        }
    }
}

/// Carry out a repair on `conn`: the relinks first, so that no link still
/// points at an `art` row when it goes, then the deletes, table by table in the
/// order the probe named them.
fn carry_out(conn: &Connection, plan: &Findings) -> Result<()> {
    let mut relink = conn.prepare("UPDATE track_art SET art_id = ?2 WHERE rowid = ?1")?;
    for (rowid, art_id) in &plan.relinks {
        relink.execute([rowid, art_id])?;
    }
    for (table, rowids) in &plan.refused {
        let mut delete = conn.prepare(&format!("DELETE FROM {table} WHERE rowid = ?1"))?;
        for rowid in rowids {
            delete.execute([rowid])?;
        }
    }
    Ok(())
}

/// A store opened for a gated schema upgrade, before the upgrade runs.
///
/// Deliberately not a [`Db`]: the schema on disk is by definition not the one
/// this build expects, so every row reader and writer in the crate would be
/// pointed at a shape it does not know. What is exposed instead is only what a
/// pre-flight needs — the versions, the steps, whether anyone else has the
/// store open, and a snapshot — plus [`PendingMigration::apply`], which runs
/// the migration and hands back an ordinary `Db`.
///
/// The connection carries no length limit (`crate::bound_lengths`) until the
/// upgrade has run, since the store's rows are not yet bounded by anything.
#[derive(Debug)]
pub struct PendingMigration {
    conn: Connection,
    path: PathBuf,
    current: i64,
    /// The repair [`PendingMigration::repair`] worked out, for
    /// [`PendingMigration::apply`] to carry out inside the migration.
    repair: RefCell<Option<Findings>>,
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
            repair: RefCell::new(None),
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

    /// The tables V4 refills, **in the order a repair must delete from them**,
    /// with the projection the migration's own refill uses.
    ///
    /// The order is not cosmetic. `tracks` goes first so its cascade takes the
    /// children it owns. `art` goes last because `track_art.art_id` references
    /// it with no `ON DELETE` clause, so deleting a refused `art` row while a
    /// link to it survives fails outright.
    ///
    /// `tracks` selects no checksum column: the refill nulls both (#691,
    /// #689), so a V1 store, which has no such columns, needs no special case.
    ///
    /// `tracks` goes in with the rows carrying tags or art links first, then by
    /// rowid. Where the refill's CAST gives a TEXT path and a BLOB one the same
    /// bytes, `UNIQUE` refuses whichever arrives second, so that ordering is
    /// what decides which row keeps the path: the one with something a user
    /// wrote on it, or the older one when neither has.
    fn probe_projections() -> [(&'static str, String); 5] {
        [
            (
                "tracks",
                "(rowid, id, backing_path, format, audio_offset, audio_length, \
                  backing_size, backing_mtime_ns, content_version, updated_at, \
                  backing_ctime_ns, fingerprint, content_hash, backing_ino) \
                 SELECT rowid, id, CAST(backing_path AS BLOB), format, audio_offset, \
                  audio_length, backing_size, backing_mtime_ns, content_version, \
                  updated_at, backing_ctime_ns, NULL, NULL, 0 \
                 FROM main.tracks t \
                 ORDER BY EXISTS (SELECT 1 FROM main.tags WHERE track_id = t.id) \
                       OR EXISTS (SELECT 1 FROM main.track_art WHERE track_id = t.id) DESC, \
                          rowid"
                    .to_string(),
            ),
            (
                "tags",
                "(rowid, track_id, key, value, ordinal, value_blob) \
                 SELECT rowid, track_id, key, value, ordinal, value_blob \
                 FROM main.tags"
                    .to_string(),
            ),
            (
                "track_art",
                "(rowid, track_id, art_id, picture_type, description, \
                  mime, width, height, depth, colors, ordinal) \
                 SELECT h.rowid, h.track_id, h.art_id, h.picture_type, h.description, \
                  a.mime, a.width, a.height, 0, 0, h.ordinal \
                 FROM main.track_art h LEFT JOIN main.art a ON a.id = h.art_id"
                    .to_string(),
            ),
            (
                // Rebuilt like the rest (#732), and probed for the same reason:
                // a row the new storage-class checks refuse -- or one an older
                // writer smuggled past the checks this table always had --
                // fails the refill. Omitting it would let that row miss the
                // report, miss
                // --repair, and then fail the upgrade after the snapshot.
                "structural_blocks",
                "(rowid, track_id, kind, ordinal, body) \
                 SELECT rowid, track_id, kind, ordinal, body \
                 FROM main.structural_blocks"
                    .to_string(),
            ),
            (
                "art",
                // The probe table is the *new* shape and `main.art` is the old
                // one, so this is where the three relocated columns stop being
                // selected (#716). The `track_art` projection above still reads
                // them from `main.art`, which is correct: the store being
                // probed has not been migrated yet, so that is where they are.
                "(rowid, id, sha256, byte_len, data) \
                 SELECT rowid, id, sha256, byte_len, data \
                 FROM main.art"
                    .to_string(),
            ),
        ]
    }

    /// Child rows whose parent will not be there when the migration refills
    /// them, keyed by child table.
    ///
    /// The probe above runs with foreign keys off and so cannot see this: an
    /// orphan — a child an older foreign-keys-off writer left pointing at a row
    /// that is not there — satisfies every `CHECK` in its own table and fails
    /// only when the refill puts it back with enforcement on. `INSERT OR
    /// IGNORE` is no help either: conflict resolution does not apply to foreign
    /// keys, so a violation there aborts the statement rather than skipping the
    /// row (verified).
    ///
    /// "Will not be there" covers both shapes at once, because the probe tables
    /// hold exactly the rows that survive: a parent that never existed and a
    /// parent this repair is about to delete are the same question to a child.
    fn orphans(&self) -> Result<Vec<(&'static str, Vec<i64>)>> {
        let checks: [(&'static str, &'static str); 4] = [
            (
                "tags",
                "SELECT rowid FROM main.tags \
                 WHERE track_id NOT IN (SELECT id FROM probe.tracks)",
            ),
            (
                "track_art",
                "SELECT rowid FROM main.track_art \
                 WHERE track_id NOT IN (SELECT id FROM probe.tracks) \
                    OR art_id NOT IN (SELECT id FROM probe.art)",
            ),
            (
                "structural_blocks",
                "SELECT rowid FROM main.structural_blocks \
                 WHERE track_id NOT IN (SELECT id FROM probe.tracks)",
            ),
            // `art` has no parent; listed nowhere rather than as an empty case.
            ("", ""),
        ];
        let mut out = Vec::new();
        for (table, sql) in checks.iter().filter(|(t, _)| !t.is_empty()) {
            let mut stmt = self.conn.prepare(sql)?;
            let rowids: Vec<i64> = stmt
                .query_map([], |r| r.get(0))?
                .collect::<rusqlite::Result<_>>()?;
            out.push((*table, rowids));
        }
        Ok(out)
    }

    /// Build the target shapes in an attached scratch database and run the
    /// store's rows at them, returning the rowids each table would refuse.
    ///
    /// The reference schema is produced by the migration itself rather than
    /// described a second time here — a scratch store migrated to the target is
    /// exactly what this one is about to become — so there is no second copy of
    /// the constraints to drift. `INSERT OR IGNORE` then skips precisely the
    /// rows a `CHECK`, `NOT NULL` or `UNIQUE` would refuse, and what did not
    /// arrive is the answer.
    ///
    /// Foreign keys are off for the pass. The question is per row and per table
    /// — a child whose parent is refused would otherwise be counted for a
    /// reason of its own that it does not have.
    fn probe(&self) -> Result<Findings> {
        let reference = {
            let mut scratch = Connection::open_in_memory()?;
            schema::migrate_all(&mut scratch)?;
            let mut stmt = scratch.prepare(
                "SELECT name, sql FROM sqlite_master \
                 WHERE type = 'table' \
                   AND name IN ('tracks','tags','track_art','art','structural_blocks')",
            )?;
            let rows =
                stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        };

        self.conn.pragma_update(None, "foreign_keys", false)?;
        let out = self.probe_inner(&reference);
        // Restore the pragma whatever happened: the caller's next move may be
        // the migration, which relies on enforcement being on.
        let restored = self.conn.pragma_update(None, "foreign_keys", true);
        let out = out?;
        restored?;
        Ok(out)
    }

    /// Two `tracks` rows the refill would give one path: each refused row with
    /// the row that kept the path, and whether both carry tags or art links.
    fn duplicates(&self) -> Result<Vec<DuplicatePath>> {
        let mut stmt = self.conn.prepare(
            "SELECT p.id, m.id, \
                    (EXISTS (SELECT 1 FROM main.tags WHERE track_id = m.id) \
                     OR EXISTS (SELECT 1 FROM main.track_art WHERE track_id = m.id)) \
                    AND (EXISTS (SELECT 1 FROM main.tags WHERE track_id = p.id) \
                     OR EXISTS (SELECT 1 FROM main.track_art WHERE track_id = p.id)) \
             FROM main.tracks m \
             JOIN probe.tracks p ON p.backing_path = CAST(m.backing_path AS BLOB) \
             WHERE m.rowid NOT IN (SELECT rowid FROM probe.tracks)",
        )?;
        let found = stmt
            .query_map([], |r| {
                Ok(DuplicatePath {
                    kept: r.get(0)?,
                    refused: r.get(1)?,
                    ambiguous: r.get(2)?,
                })
            })?
            .collect::<rusqlite::Result<_>>()?;
        Ok(found)
    }

    /// Links to a refused `art` row that a correctly filed row holding the same
    /// bytes can take instead, each with that row's id. Only a link that
    /// survives on its own terms qualifies: one refused for itself, or whose
    /// track is refused, goes whatever it points at. The bytes are compared in
    /// SQL, lengths first, so no blob is read into Rust to decide.
    fn relinks(&self) -> Result<Vec<(i64, i64)>> {
        let mut stmt = self.conn.prepare(
            "SELECT h.rowid, \
                    (SELECT min(twin.id) FROM main.art twin \
                      WHERE twin.id IN (SELECT id FROM probe.art) \
                        AND twin.byte_len = bad.byte_len \
                        AND twin.data = bad.data) \
             FROM main.track_art h JOIN main.art bad ON bad.id = h.art_id \
             WHERE h.art_id NOT IN (SELECT id FROM probe.art) \
               AND h.rowid IN (SELECT rowid FROM probe.track_art) \
               AND h.track_id IN (SELECT id FROM probe.tracks)",
        )?;
        let rows: Vec<(i64, Option<i64>)> = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?;
        Ok(rows
            .into_iter()
            .filter_map(|(rowid, twin)| twin.map(|id| (rowid, id)))
            .collect())
    }

    fn probe_inner(&self, reference: &[(String, String)]) -> Result<Findings> {
        // An empty filename is SQLite's temporary on-disk database: it lives
        // beside the store's own temp files and is deleted on DETACH, so a
        // store far too large to probe in memory still works.
        self.conn.execute_batch("ATTACH DATABASE '' AS probe")?;
        let result = (|| -> Result<Findings> {
            for (name, sql) in reference {
                self.conn
                    .execute_batch(&sql.replacen("CREATE TABLE ", "CREATE TABLE probe.", 1))
                    .map_err(|e| {
                        crate::DbError::Sqlite(rusqlite::Error::SqliteFailure(
                            rusqlite::ffi::Error::new(1),
                            Some(format!("building the probe shape for {name}: {e}")),
                        ))
                    })?;
            }
            let mut out: Vec<(&'static str, Vec<i64>)> = Vec::new();
            for (table, projection) in Self::probe_projections() {
                self.conn
                    .execute_batch(&format!("INSERT OR IGNORE INTO probe.{table} {projection}"))?;
                let mut stmt = self.conn.prepare(&format!(
                    "SELECT rowid FROM main.{table} \
                     WHERE rowid NOT IN (SELECT rowid FROM probe.{table})"
                ))?;
                let rowids: Vec<i64> = stmt
                    .query_map([], |r| r.get(0))?
                    .collect::<rusqlite::Result<_>>()?;
                out.push((table, rowids));
            }
            // Second pass, once every probe table is populated: a child whose
            // parent did not survive. Merged into the same per-table answer,
            // because to the user it is one question -- what will not be there
            // afterwards -- and a row can fail both ways at once. A link that
            // moves onto a correctly filed row is not lost with the row it
            // pointed at, so it is left out.
            let relinks = self.relinks()?;
            let moving: HashSet<i64> = relinks.iter().map(|&(rowid, _)| rowid).collect();
            for (table, orphaned) in self.orphans()? {
                let slot = out
                    .iter_mut()
                    .find(|(t, _)| *t == table)
                    .expect("every orphan-checked table is probed");
                // A set beside the list, so a large orphan set is not quadratic,
                // and the list keeps the order the report and the repair use.
                let mut named: HashSet<i64> = slot.1.iter().copied().collect();
                for rowid in orphaned {
                    let moves = table == "track_art" && moving.contains(&rowid);
                    if !moves && named.insert(rowid) {
                        slot.1.push(rowid);
                    }
                }
            }
            Ok(Findings {
                refused: out,
                relinks,
                duplicates: self.duplicates()?,
            })
        })();
        let detached = self.conn.execute_batch("DETACH DATABASE probe");
        let out = result?;
        detached?;
        Ok(out)
    }

    /// Report the rows the migrated schema would refuse, without changing
    /// anything.
    ///
    /// A row reaches this state by being written before the constraint that
    /// now refuses it, or by a writer that turned the constraints off. The
    /// migration would abort on the first one it met — atomically, so nothing
    /// would be half-applied — but aborting part-way through a long upgrade
    /// with a raw `CHECK constraint failed` is a poor way to learn that.
    pub fn inspect_rejections(&self) -> Result<Rejections> {
        Ok(self.probe()?.report())
    }

    /// Work out how to repair every row the migrated schema would refuse, hold
    /// it for [`PendingMigration::apply`], and report what the upgrade will do.
    ///
    /// Dropping a row an external writer chose is exactly the class of thing
    /// that must not happen without being asked for, so nothing calls this
    /// except a command the user has told to repair.
    ///
    /// Nothing is deleted here. The repair runs inside the migration's own
    /// transaction, before its first step, so an upgrade that fails part-way —
    /// on a full disk, say — rolls the repair back with everything else and
    /// leaves the store as it was, rather than at its old version short the
    /// rows the repair took.
    ///
    /// Deletes run parent-first, so a refused `tracks` row takes its tags and
    /// art links with it through the cascade rather than leaving them to be
    /// deleted for a reason they do not have. The counts reported are the rows
    /// named by the probe; the cascade may take more.
    ///
    /// Two cases are decided rather than deleted wholesale. A picture link whose
    /// `art` row is refused moves onto a correctly filed row holding the same
    /// bytes, where one exists, since the picture it serves is the same one.
    /// And where two `tracks` rows share a path ([`DuplicatePath`]), the one
    /// carrying tags or art links keeps it; if both carry some, this refuses
    /// with [`crate::DbError::AmbiguousDuplicatePath`] and holds nothing.
    pub fn repair(&self) -> Result<Rejections> {
        let found = self.probe()?;
        if let Some(both) = found.duplicates.iter().find(|d| d.ambiguous) {
            return Err(crate::DbError::AmbiguousDuplicatePath {
                first: both.kept,
                second: both.refused,
            });
        }
        let report = found.report();
        self.repair.replace(Some(found));
        Ok(report)
    }

    /// Run every pending step, gated ones included, and hand back the migrated
    /// store. The identity check runs here, against the shape the migration was
    /// supposed to produce. A repair [`PendingMigration::repair`] held runs in
    /// the same transaction, first.
    ///
    /// This is where the connection stops being a migration handle and becomes
    /// an ordinary [`Db`], so it picks up what [`Db::open`] sets that the
    /// pre-flight had no use for. Write-ahead logging, which is what keeps a
    /// reader and a writer off each other's backs: a musefs store is already in
    /// WAL — the mode is persistent and every other open sets it — so this is
    /// belt and braces for a store that arrived some other way. And the length
    /// limit, which a store past V4's constraints can carry.
    pub fn apply(mut self) -> Result<Db> {
        let _: String = self
            .conn
            .query_row("PRAGMA journal_mode = WAL", [], |r| r.get(0))?;
        match self.repair.take() {
            Some(plan) => {
                schema::migrate_all_after(&mut self.conn, &|tx| carry_out(tx, &plan))?;
            }
            None => schema::migrate_all(&mut self.conn)?,
        }
        schema::validate_identity(&self.conn)?;
        crate::bound_lengths(&self.conn)?;
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
mod rejection_tests {
    use super::PendingMigration;
    use rusqlite::Connection;

    /// A V3 store with one clean track, one tag, one art row and one link.
    fn store_at_v3(path: &std::path::Path) -> Connection {
        crate::schema::seed_store_at_version(path, 3).unwrap();
        let conn = Connection::open(path).unwrap();
        conn.execute(
            "INSERT INTO tracks (backing_path, format, audio_offset, audio_length, \
             backing_size, backing_mtime_ns, backing_ctime_ns, updated_at) \
             VALUES ('/lib/a.flac', 'flac', 0, 0, 0, 0, 0, 0)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO art (sha256, mime, width, height, byte_len, data) \
             VALUES (?1, 'image/png', 1, 1, 1, X'00')",
            [&"a".repeat(64)],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO tags (track_id, key, value, ordinal) VALUES (1, 'artist', 'A', 0)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO track_art (track_id, art_id, picture_type, description, ordinal) \
             VALUES (1, 1, 3, 'cover', 0)",
            [],
        )
        .unwrap();
        conn
    }

    /// Write a row the pre-V4 schema accepts and V4 does not.
    fn plant_hostile(conn: &Connection, sql: &str, params: &[&dyn rusqlite::ToSql]) {
        conn.pragma_update(None, "ignore_check_constraints", true)
            .unwrap();
        conn.execute(sql, params).unwrap();
        conn.pragma_update(None, "ignore_check_constraints", false)
            .unwrap();
    }

    #[test]
    fn a_clean_store_has_nothing_to_report() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.db");
        drop(store_at_v3(&path));
        let pending = PendingMigration::open(&path).unwrap();
        let found = pending.inspect_rejections().unwrap();
        assert!(found.is_empty(), "{found:?}");
        assert_eq!(found.total(), 0);
        // And it still migrates, which is the claim the report is making.
        pending.apply().unwrap();
    }

    #[test]
    fn a_nul_bearing_tag_key_is_reported_then_repaired() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.db");
        let conn = store_at_v3(&path);
        plant_hostile(
            &conn,
            "INSERT INTO tags (track_id, key, value, ordinal) VALUES (1, ?1, 'v', 1)",
            &[&format!("k{}junk", '\0')],
        );
        drop(conn);

        let pending = PendingMigration::open(&path).unwrap();
        let found = pending.inspect_rejections().unwrap();
        assert!(!found.is_empty(), "a store with a refused row is not empty");
        assert_eq!(found.total(), 1);
        assert_eq!(found.tables()[0].table, "tags");

        // Without repair the migration is exactly as bad as the report says.
        let err = pending.apply().unwrap_err().to_string();
        assert!(err.contains("CHECK constraint failed"), "{err}");

        // With it, the row goes and the upgrade runs. The repair is held for the
        // upgrade to carry out, so until then the row is still there.
        let pending = PendingMigration::open(&path).unwrap();
        let removed = pending.repair().unwrap();
        assert_eq!(removed.total(), 1);
        assert_eq!(pending.inspect_rejections().unwrap().total(), 1);
        let db = pending.apply().unwrap();
        assert_eq!(db.get_tags(1).unwrap().len(), 1, "the clean tag survived");
    }

    /// A child whose parent will not survive is reported too.
    ///
    /// The first cut of this counted only the parent, on the grounds that a tag
    /// under a refused track has nothing wrong with it of its own. That is true
    /// and beside the point: the number exists to tell the user how many rows
    /// they are about to lose, and the cascade loses that one. Counting only
    /// the parent under-reported, and left the same query unable to see a
    /// genuine orphan -- a child pointing at a track that is not there at all,
    /// which fails the refill for the same reason and is invisible to a probe
    /// that runs with foreign keys off.
    #[test]
    fn a_child_of_a_refused_track_is_reported_with_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.db");
        let conn = store_at_v3(&path);
        plant_hostile(
            &conn,
            "INSERT INTO tracks (backing_path, format, audio_offset, audio_length, \
             backing_size, backing_mtime_ns, backing_ctime_ns, updated_at) \
             VALUES ('', 'flac', 0, 0, 0, 0, 0, 0)",
            &[],
        );
        let bad = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO tags (track_id, key, value, ordinal) VALUES (?1, 'artist', 'B', 0)",
            [bad],
        )
        .unwrap();
        drop(conn);

        let pending = PendingMigration::open(&path).unwrap();
        let found = pending.inspect_rejections().unwrap();
        let named: Vec<(&str, u64)> = found
            .tables()
            .iter()
            .map(|t| (t.table, t.rejected))
            .collect();
        assert_eq!(
            named,
            vec![("tracks", 1), ("tags", 1)],
            "the track and the tag that goes with it: {found:?}"
        );

        let removed = pending.repair().unwrap();
        assert_eq!(removed.total(), 2);
        let db = pending.apply().unwrap();
        assert_eq!(db.list_tracks().unwrap().len(), 1);
        assert_eq!(db.get_tags(1).unwrap().len(), 1);
    }

    /// A genuine orphan: a child pointing at a parent that is not there at all.
    ///
    /// It satisfies every `CHECK` in its own table, and the probe runs with
    /// foreign keys off, so nothing about the row itself gives it away —
    /// `INSERT OR IGNORE` cannot help either, since conflict resolution does
    /// not apply to foreign keys and a violation aborts the statement rather
    /// than skipping the row. Without the second pass this reached `apply()`
    /// and failed the refill *after* the snapshot had been written.
    #[test]
    fn an_orphan_left_by_a_foreign_keys_off_writer_is_reported() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.db");
        let conn = store_at_v3(&path);
        conn.pragma_update(None, "foreign_keys", false).unwrap();
        conn.execute(
            "INSERT INTO tags (track_id, key, value, ordinal) VALUES (99, 'artist', 'ghost', 0)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO structural_blocks (track_id, kind, ordinal, body) \
             VALUES (99, 'STREAMINFO', 0, X'00')",
            [],
        )
        .unwrap();
        drop(conn);

        let pending = PendingMigration::open(&path).unwrap();
        let found = pending.inspect_rejections().unwrap();
        let named: Vec<(&str, u64)> = found
            .tables()
            .iter()
            .map(|t| (t.table, t.rejected))
            .collect();
        assert_eq!(
            named,
            vec![("tags", 1), ("structural_blocks", 1)],
            "{found:?}"
        );

        pending.repair().unwrap();
        let db = pending.apply().unwrap();
        assert_eq!(db.get_tags(1).unwrap().len(), 1, "the real tag survived");
    }

    /// `structural_blocks` keeps its shape across V4, but the migration still
    /// copies it out and back — so a row smuggled past its own constraints
    /// fails the refill like any other, and has to be in the report.
    #[test]
    fn a_structural_block_the_refill_would_refuse_is_reported() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.db");
        let conn = store_at_v3(&path);
        plant_hostile(
            &conn,
            "INSERT INTO structural_blocks (track_id, kind, ordinal, body) \
             VALUES (1, 'NOT_A_KIND', 0, X'00')",
            &[],
        );
        drop(conn);

        let pending = PendingMigration::open(&path).unwrap();
        let found = pending.inspect_rejections().unwrap();
        assert_eq!(found.total(), 1, "{found:?}");
        assert_eq!(found.tables()[0].table, "structural_blocks");
        pending.repair().unwrap();
        pending.apply().unwrap();
    }

    /// Two tables refusing the same planted row, and the order the report and
    /// the repair have to use.
    ///
    /// The geometry half is #718's bound reached through the link's backfill:
    /// the value still lives on `art` in the store being probed but the
    /// constraint that refuses it is on `track_art`, so the probe has to run the
    /// refill's own join to see it at all. The blob half is a storage class
    /// `art` keeps for itself. Together they pin the delete order, which the
    /// foreign key makes load-bearing rather than cosmetic.
    #[test]
    fn a_refused_blob_and_its_link_are_reported_link_first() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.db");
        let conn = store_at_v3(&path);
        // One planted row, refused twice over and in two different tables. Its
        // `data` is text, which is V4's storage-class check on `art` itself;
        // its width is past `u32::MAX`, which V4 checks on `track_art` now that
        // the geometry lives there (#716) and the probe reads it across the
        // join. That is what makes the delete order observable at all.
        //
        // Inserted rather than updated: `art_reject_content_update` is a
        // trigger, and `ignore_check_constraints` does not bypass triggers --
        // which is the immutability guard doing its job.
        plant_hostile(
            &conn,
            "INSERT INTO art (sha256, mime, width, height, byte_len, data) \
             VALUES (?1, 'image/png', 1099511627776, 1, 1, 'x')",
            &[&"b".repeat(64)],
        );
        conn.execute(
            "INSERT INTO track_art (track_id, art_id, picture_type, description, ordinal) \
             VALUES (1, 2, 3, 'back', 1)",
            [],
        )
        .unwrap();
        drop(conn);

        let pending = PendingMigration::open(&path).unwrap();
        let found = pending.inspect_rejections().unwrap();
        let named: Vec<&str> = found.tables().iter().map(|t| t.table).collect();
        assert_eq!(
            named,
            vec!["track_art", "art"],
            "reported -- and deleted -- link before blob: `track_art.art_id` \
             references `art(id)` with no ON DELETE clause, so the other order \
             fails on the foreign key: {found:?}"
        );
        // Which is the half that was never exercised before: repair it.
        let removed = pending.repair().unwrap();
        assert_eq!(removed.total(), 2);
        pending.apply().unwrap();
    }

    /// An art digest that is not lowercase hex (#761). 1.3.0 accepted any 64
    /// characters, so this takes no hostile writer -- only one that uppercased.
    /// Lowercasing it is not a safe sanitize, since a canonical row for the same
    /// bytes may already exist, so it takes the remediation every other refused
    /// row does: reported with the link that points at it, and deleted with it.
    #[test]
    fn a_non_canonical_art_digest_is_reported_with_its_link_then_repaired() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.db");
        let conn = store_at_v3(&path);
        conn.execute(
            "INSERT INTO art (sha256, mime, width, height, byte_len, data) \
             VALUES (?1, 'image/png', 1, 1, 1, X'01')",
            [&"A".repeat(64)],
        )
        .unwrap();
        let uppercased = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO track_art (track_id, art_id, picture_type, description, ordinal) \
             VALUES (1, ?1, 4, 'back', 1)",
            [uppercased],
        )
        .unwrap();
        drop(conn);

        let pending = PendingMigration::open(&path).unwrap();
        let found = pending.inspect_rejections().unwrap();
        let named: Vec<(&str, u64)> = found
            .tables()
            .iter()
            .map(|t| (t.table, t.rejected))
            .collect();
        assert_eq!(
            named,
            vec![("track_art", 1), ("art", 1)],
            "the row and the link it would orphan: {found:?}"
        );
        assert_eq!(found.relinked(), 0, "no correctly filed twin to move onto");

        let removed = pending.repair().unwrap();
        assert_eq!(removed.total(), 2);
        let db = pending.apply().unwrap();
        let links = db.get_track_art(1).unwrap();
        assert_eq!(links.len(), 1, "the canonical cover survives: {links:?}");
        assert_eq!(links[0].description, "cover");
    }

    /// A `backing_path` past the 64 KiB ceiling (#758). 1.3.0 set none, so a
    /// store can hold one, and the pre-flight sees it only because its reference
    /// tables come from the migration itself. One exactly at the ceiling is not
    /// refused.
    #[test]
    fn a_backing_path_over_the_ceiling_is_reported_and_one_at_it_is_not() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.db");
        let conn = store_at_v3(&path);
        let cap = usize::try_from(crate::limits::MAX_BACKING_PATH_BYTES).unwrap();
        for (dir_name, len) in [("at", cap), ("over", cap + 1)] {
            let stored = format!("/{dir_name}/{}", "a".repeat(len - dir_name.len() - 2));
            assert_eq!(stored.len(), len);
            plant_hostile(
                &conn,
                "INSERT INTO tracks (backing_path, format, audio_offset, audio_length, \
                 backing_size, backing_mtime_ns, backing_ctime_ns, updated_at) \
                 VALUES (?1, 'flac', 0, 0, 0, 0, 0, 0)",
                &[&stored],
            );
        }
        drop(conn);

        let pending = PendingMigration::open(&path).unwrap();
        let found = pending.inspect_rejections().unwrap();
        let named: Vec<(&str, u64)> = found
            .tables()
            .iter()
            .map(|t| (t.table, t.rejected))
            .collect();
        assert_eq!(named, vec![("tracks", 1)], "only the one over: {found:?}");

        let removed = pending.repair().unwrap();
        assert_eq!(removed.total(), 1);
        let db = pending.apply().unwrap();
        let mut lens: Vec<usize> = db
            .list_backing_paths()
            .unwrap()
            .iter()
            .map(|p| p.as_os_str().len())
            .collect();
        lens.sort_unstable();
        assert_eq!(
            lens,
            vec!["/lib/a.flac".len(), cap],
            "the clean row and the one at the cap"
        );
    }

    /// A row refused on its own and orphaned by a refused parent as well is one
    /// row the user loses, so it is counted once.
    #[test]
    fn a_row_refused_twice_over_is_counted_once() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.db");
        let conn = store_at_v3(&path);
        plant_hostile(
            &conn,
            "INSERT INTO tracks (backing_path, format, audio_offset, audio_length, \
             backing_size, backing_mtime_ns, backing_ctime_ns, updated_at) \
             VALUES ('', 'flac', 0, 0, 0, 0, 0, 0)",
            &[],
        );
        let bad = conn.last_insert_rowid();
        plant_hostile(
            &conn,
            "INSERT INTO tags (track_id, key, value, ordinal) VALUES (?1, ?2, 'v', 0)",
            &[&bad, &format!("k{}junk", '\0')],
        );
        drop(conn);

        let found = PendingMigration::open(&path)
            .unwrap()
            .inspect_rejections()
            .unwrap();
        let named: Vec<(&str, u64)> = found
            .tables()
            .iter()
            .map(|t| (t.table, t.rejected))
            .collect();
        assert_eq!(named, vec![("tracks", 1), ("tags", 1)], "{found:?}");
    }

    /// #761's repair when a correctly filed row already holds the same bytes:
    /// the link moves onto that row instead of going with the one it pointed at,
    /// since the picture it serves is byte for byte the same. A non-canonical
    /// row with no such twin still takes its links with it, which
    /// `a_non_canonical_art_digest_is_reported_with_its_link_then_repaired` pins.
    #[test]
    fn a_non_canonical_digest_with_a_canonical_twin_is_relinked_not_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.db");
        let conn = store_at_v3(&path);
        // The same one byte `store_at_v3` files, correctly, as art 1.
        conn.execute(
            "INSERT INTO art (sha256, mime, width, height, byte_len, data) \
             VALUES (?1, 'image/png', 1, 1, 1, X'00')",
            [&"A".repeat(64)],
        )
        .unwrap();
        let uppercased = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO track_art (track_id, art_id, picture_type, description, ordinal) \
             VALUES (1, ?1, 4, 'back', 1)",
            [uppercased],
        )
        .unwrap();
        drop(conn);

        let pending = PendingMigration::open(&path).unwrap();
        let found = pending.inspect_rejections().unwrap();
        let named: Vec<(&str, u64)> = found
            .tables()
            .iter()
            .map(|t| (t.table, t.rejected))
            .collect();
        assert_eq!(
            named,
            vec![("art", 1)],
            "the row goes, its link stays: {found:?}"
        );
        assert_eq!(found.relinked(), 1, "and the link is reported as moving");

        assert_eq!(pending.repair().unwrap().relinked(), 1);
        let db = pending.apply().unwrap();
        let links: Vec<(i64, String)> = db
            .get_track_art(1)
            .unwrap()
            .into_iter()
            .map(|l| (l.art_id, l.description))
            .collect();
        assert_eq!(
            links,
            vec![(1, "cover".to_string()), (1, "back".to_string())],
            "both pictures, from the one correctly filed row"
        );
    }

    /// A V3 store holding `/lib/a.flac` twice: track 1 as TEXT, the way every
    /// pre-2.0 writer bound it, and track 2 as the bytes a byte-binding tool
    /// bound. V3's UNIQUE never compared the two spellings, and V4's refill casts
    /// both to one BLOB, so only one of them can survive. `curated` says which of
    /// the two carry a tag.
    fn store_with_one_path_twice(path: &std::path::Path, curated: [bool; 2]) {
        let conn = store_at_v3(path);
        conn.execute("DELETE FROM track_art", []).unwrap();
        conn.execute("DELETE FROM tags", []).unwrap();
        conn.execute(
            "INSERT INTO tracks (backing_path, format, audio_offset, audio_length, \
             backing_size, backing_mtime_ns, backing_ctime_ns, updated_at) \
             VALUES (CAST('/lib/a.flac' AS BLOB), 'flac', 0, 0, 0, 0, 0, 0)",
            [],
        )
        .unwrap();
        for (id, tagged) in [1i64, 2].into_iter().zip(curated) {
            if tagged {
                conn.execute(
                    "INSERT INTO tags (track_id, key, value, ordinal) \
                     VALUES (?1, 'title', 'curated', 0)",
                    [id],
                )
                .unwrap();
            }
        }
    }

    /// Which of the two survives is decided, not left to rowid order: the one
    /// carrying tags or art links, since the other has nothing a user wrote on
    /// it to lose. With neither carrying any, nothing is lost either way, and
    /// the older row stays.
    #[test]
    fn a_path_stored_twice_keeps_the_row_carrying_curated_data() {
        for (curated, kept) in [([false, true], 2), ([true, false], 1), ([false, false], 1)] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("s.db");
            store_with_one_path_twice(&path, curated);

            let pending = PendingMigration::open(&path).unwrap();
            let found = pending.inspect_rejections().unwrap();
            let named: Vec<(&str, u64)> = found
                .tables()
                .iter()
                .map(|t| (t.table, t.rejected))
                .collect();
            assert_eq!(named, vec![("tracks", 1)], "{curated:?}: {found:?}");
            let refused = 3 - kept;
            assert_eq!(
                found.duplicates(),
                [super::DuplicatePath {
                    kept,
                    refused,
                    ambiguous: false,
                }],
                "{curated:?}"
            );

            pending.repair().unwrap();
            let db = pending.apply().unwrap();
            let ids: Vec<i64> = db.list_tracks().unwrap().iter().map(|t| t.id).collect();
            assert_eq!(ids, vec![kept], "{curated:?}");
            let tags = db.get_tags(kept).unwrap().len();
            assert_eq!(tags, usize::from(curated.contains(&true)), "{curated:?}");
        }
    }

    /// With tags on both rows there is no telling which the user wants, so the
    /// repair refuses rather than guess, and deletes nothing.
    #[test]
    fn a_path_stored_twice_with_curated_data_on_both_is_not_repaired() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.db");
        store_with_one_path_twice(&path, [true, true]);

        let pending = PendingMigration::open(&path).unwrap();
        let found = pending.inspect_rejections().unwrap();
        assert_eq!(
            found.duplicates(),
            [super::DuplicatePath {
                kept: 1,
                refused: 2,
                ambiguous: true,
            }],
            "reported, both ids named"
        );
        let err = pending
            .repair()
            .expect_err("two curated rows are not the repair's to choose between");
        assert!(
            matches!(
                err,
                crate::DbError::AmbiguousDuplicatePath {
                    first: 1,
                    second: 2
                }
            ),
            "{err:?}"
        );
        drop(pending);
        let tracks: i64 = Connection::open(&path)
            .unwrap()
            .query_row("SELECT count(*) FROM tracks", [], |r| r.get(0))
            .unwrap();
        assert_eq!(tracks, 2, "nothing was deleted");
    }

    /// The repair belongs to the migration's transaction, so an upgrade that
    /// fails after it puts the repaired rows back along with everything else.
    /// It used to commit on its own, and a failed upgrade left the store at its
    /// old version short the rows the repair took. The failure here is a store
    /// with no room to grow, which the rebuild's holding tables need.
    #[test]
    fn a_failed_upgrade_rolls_back_the_repair_with_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.db");
        let conn = store_at_v3(&path);
        plant_hostile(
            &conn,
            "INSERT INTO tags (track_id, key, value, ordinal) VALUES (1, ?1, 'v', 1)",
            &[&format!("k{}junk", '\0')],
        );
        drop(conn);

        let pending = PendingMigration::open(&path).unwrap();
        pending.repair().unwrap();
        let pages: i64 = pending
            .conn
            .query_row("PRAGMA page_count", [], |r| r.get(0))
            .unwrap();
        let _: i64 = pending
            .conn
            .query_row(&format!("PRAGMA max_page_count = {pages}"), [], |r| {
                r.get(0)
            })
            .unwrap();
        let err = pending.apply().unwrap_err().to_string();
        assert!(err.contains("full"), "{err}");

        let conn = Connection::open(&path).unwrap();
        let version: i64 = conn
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap();
        assert_eq!(version, 3, "the upgrade did not happen");
        let tags: i64 = conn
            .query_row("SELECT count(*) FROM tags", [], |r| r.get(0))
            .unwrap();
        assert_eq!(tags, 2, "and neither did the repair");
    }

    /// The pre-V4 schema bounds no row: `backing_path` has no cap, and the text
    /// caps stop counting at a NUL. So the migration handle carries no length
    /// limit, and a legacy row past it is reported and repaired like any other
    /// refused row instead of failing the pre-flight. The store it hands on has
    /// been through V4's constraints, and carries the limit.
    #[test]
    fn a_legacy_row_past_the_length_limit_is_reported_not_fatal() {
        use rusqlite::limits::Limit;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.db");
        let conn = store_at_v3(&path);
        let long = crate::limits::MAX_ROW_BYTES + 1;
        conn.execute(
            "INSERT INTO tracks (backing_path, format, audio_offset, audio_length, \
             backing_size, backing_mtime_ns, backing_ctime_ns, updated_at) \
             VALUES ('/' || substr(replace(hex(zeroblob(?1)), '0', 'a'), 1, ?1), \
                     'flac', 0, 0, 0, 0, 0, 0)",
            [long],
        )
        .unwrap();
        drop(conn);

        let pending = PendingMigration::open(&path).unwrap();
        let limit = |conn: &Connection| i64::from(conn.limit(Limit::SQLITE_LIMIT_LENGTH).unwrap());
        assert!(limit(&pending.conn) > long);
        let found = pending.inspect_rejections().unwrap();
        assert_eq!(found.total(), 1, "{found:?}");
        pending.repair().unwrap();
        let db = pending.apply().unwrap();
        assert_eq!(limit(&db.conn), crate::limits::MAX_ROW_BYTES);
        assert_eq!(db.list_tracks().unwrap().len(), 1);
    }

    /// A V1 store has no checksum columns at all, so the probe's `tracks`
    /// projection cannot name them. Upgrading from the oldest released shape is
    /// the arm that catches a projection written against the newest one.
    #[test]
    fn a_v1_store_probes_without_the_checksum_columns() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.db");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(crate::schema::migration_sql()[0])
            .unwrap();
        conn.pragma_update(None, "user_version", 1i64).unwrap();
        conn.execute(
            "INSERT INTO tracks (backing_path, format, audio_offset, audio_length, \
             backing_size, backing_mtime_ns, updated_at) \
             VALUES ('/lib/a.flac', 'flac', 0, 0, 0, 0, 0)",
            [],
        )
        .unwrap();
        drop(conn);

        let pending = PendingMigration::open(&path).unwrap();
        assert!(pending.inspect_rejections().unwrap().is_empty());
        pending.apply().unwrap();
    }
}

#[cfg(test)]
mod tests {
    use super::PendingMigration;
    use crate::{Db, DbError, LATEST_VERSION};

    /// A store at the version before the gated step — the state an older musefs
    /// build leaves behind. See [`crate::schema::seed_store_at_version`] for why
    /// it is built rather than stamped.
    fn gated_store(path: &std::path::Path) {
        crate::schema::seed_store_at_version(path, LATEST_VERSION - 1).unwrap();
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
    ///
    /// Alone among these, this one builds its pending store by rewinding a
    /// current one's stamp rather than by running the earlier migrations. It
    /// never migrates: what it needs is a store an ordinary reader can open
    /// *and* that `claim_exclusive` has something to refuse, and only the
    /// current shape passes `Db::open_readonly`'s identity check.
    #[test]
    fn a_store_another_connection_has_open_is_refused() {
        fn gated_store(path: &std::path::Path) {
            Db::open(path).unwrap();
            let conn = rusqlite::Connection::open(path).unwrap();
            conn.pragma_update(None, "user_version", LATEST_VERSION - 1)
                .unwrap();
        }
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
