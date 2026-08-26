use crate::Result;
use rusqlite::{Connection, TransactionBehavior};

const MIGRATION_V1: &str = r"
CREATE TABLE tracks (
    id               INTEGER PRIMARY KEY,
    backing_path     TEXT NOT NULL UNIQUE,
    format           TEXT NOT NULL,
    audio_offset     INTEGER NOT NULL,
    audio_length     INTEGER NOT NULL,
    backing_size     INTEGER NOT NULL,
    backing_mtime_ns INTEGER NOT NULL,
    content_version  INTEGER NOT NULL DEFAULT 0,
    updated_at       INTEGER NOT NULL,
    backing_ctime_ns INTEGER NOT NULL DEFAULT 0 CHECK (backing_ctime_ns >= 0),
    CHECK (format IN ('flac','mp3','m4a','opus','vorbis','oggflac','wav')),
    CHECK (audio_offset >= 0),
    CHECK (audio_length >= 0),
    CHECK (backing_size >= 0),
    CHECK (backing_mtime_ns >= 0),
    CHECK (content_version >= 0),
    CHECK (updated_at >= 0),
    CHECK (audio_offset + audio_length <= backing_size)
);

CREATE TABLE tags (
    track_id   INTEGER NOT NULL REFERENCES tracks(id) ON DELETE CASCADE,
    key        TEXT NOT NULL,
    value      TEXT NOT NULL,
    ordinal    INTEGER NOT NULL DEFAULT 0,
    value_blob BLOB,
    PRIMARY KEY (track_id, key, ordinal),
    CHECK (ordinal >= 0),
    CHECK (value_blob IS NULL OR value = ''),
    CHECK (length(key) <= 256),
    CHECK (length(key) >= 1
           AND key NOT GLOB '*[' || char(1) || '-' || char(31) || ']*'),
    CHECK (length(value) <= 262144),
    CHECK (value_blob IS NULL OR length(value_blob) <= 16711680)
);

CREATE TABLE art (
    id       INTEGER PRIMARY KEY,
    sha256   TEXT NOT NULL UNIQUE,
    mime     TEXT NOT NULL,
    width    INTEGER,
    height   INTEGER,
    byte_len INTEGER NOT NULL,
    data     BLOB NOT NULL,
    CHECK (byte_len = length(data)),
    CHECK (length(sha256) = 64),
    CHECK (width IS NULL OR width >= 0),
    CHECK (height IS NULL OR height >= 0),
    CHECK (length(mime) <= 255),
    CHECK (byte_len <= 16711680)
);

CREATE TABLE track_art (
    track_id     INTEGER NOT NULL REFERENCES tracks(id) ON DELETE CASCADE,
    art_id       INTEGER NOT NULL REFERENCES art(id),
    picture_type INTEGER NOT NULL DEFAULT 3,
    description  TEXT NOT NULL DEFAULT '',
    ordinal      INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (track_id, ordinal),
    CHECK (picture_type BETWEEN 0 AND 20),
    CHECK (ordinal >= 0),
    CHECK (length(description) <= 1024)
);

-- Read-only, derived-from-file structural metadata (FLAC STREAMINFO/SEEKTABLE).
-- NOT part of the editable `tags` contract: external tools never touch it.
CREATE TABLE structural_blocks (
    track_id INTEGER NOT NULL REFERENCES tracks(id) ON DELETE CASCADE,
    kind     TEXT NOT NULL,
    ordinal  INTEGER NOT NULL DEFAULT 0,
    body     BLOB NOT NULL,
    PRIMARY KEY (track_id, kind, ordinal),
    CHECK (kind IN ('STREAMINFO','SEEKTABLE')),
    CHECK (ordinal >= 0),
    CHECK (length(body) <= 16777215)
);

-- Bounded changelog ring for O(changed) refresh. Every metadata edit funnels
-- through an UPDATE on the tracks row (the tags/track_art triggers), so
-- triggers on tracks alone capture all writers. Relies on SQLite nested
-- trigger activation (on by default; distinct from PRAGMA recursive_triggers).
CREATE TABLE track_changes (
    seq      INTEGER PRIMARY KEY AUTOINCREMENT,
    track_id INTEGER NOT NULL
);

-- Index the reverse art -> track_art edge so bulk orphan-GC and the art delete
-- trigger below do not scan the whole join table per deleted row.
CREATE INDEX track_art_art_id_idx ON track_art(art_id);

CREATE TRIGGER tags_ai AFTER INSERT ON tags BEGIN
    UPDATE tracks SET content_version = content_version + 1,
                      updated_at = CAST(strftime('%s','now') AS INTEGER)
    WHERE id = NEW.track_id;
END;
CREATE TRIGGER tags_au AFTER UPDATE ON tags BEGIN
    UPDATE tracks SET content_version = content_version + 1,
                      updated_at = CAST(strftime('%s','now') AS INTEGER)
    WHERE id = NEW.track_id;
END;
CREATE TRIGGER tags_ad AFTER DELETE ON tags BEGIN
    UPDATE tracks SET content_version = content_version + 1,
                      updated_at = CAST(strftime('%s','now') AS INTEGER)
    WHERE id = OLD.track_id;
END;

CREATE TRIGGER track_art_ai AFTER INSERT ON track_art BEGIN
    UPDATE tracks SET content_version = content_version + 1,
                      updated_at = CAST(strftime('%s','now') AS INTEGER)
    WHERE id = NEW.track_id;
END;
CREATE TRIGGER track_art_au AFTER UPDATE ON track_art BEGIN
    UPDATE tracks SET content_version = content_version + 1,
                      updated_at = CAST(strftime('%s','now') AS INTEGER)
    WHERE id = NEW.track_id;
END;
CREATE TRIGGER track_art_ad AFTER DELETE ON track_art BEGIN
    UPDATE tracks SET content_version = content_version + 1,
                      updated_at = CAST(strftime('%s','now') AS INTEGER)
    WHERE id = OLD.track_id;
END;

CREATE TRIGGER tracks_changelog_ai AFTER INSERT ON tracks BEGIN
    INSERT INTO track_changes (track_id) VALUES (NEW.id);
END;
CREATE TRIGGER tracks_changelog_au AFTER UPDATE ON tracks BEGIN
    INSERT INTO track_changes (track_id) VALUES (NEW.id);
END;
CREATE TRIGGER tracks_changelog_ad AFTER DELETE ON tracks BEGIN
    INSERT INTO track_changes (track_id) VALUES (OLD.id);
END;

-- Self-pruning ring: writers maintain it; the mount's read-only connections
-- never need to. Deletes only from the old end, so retained seqs stay contiguous.
CREATE TRIGGER track_changes_prune AFTER INSERT ON track_changes BEGIN
    DELETE FROM track_changes WHERE seq <= NEW.seq - 8192;
END;

-- art rows are content-addressed by sha256: once written, their content
-- columns are immutable. A writer needing different bytes/metadata inserts a
-- NEW row and relinks via track_art (which bumps content_version through the
-- track_art triggers). width/height use IS NOT (NULL-safe) because they are
-- nullable; the NOT NULL columns use <>.
CREATE TRIGGER art_reject_content_update
BEFORE UPDATE ON art
WHEN NEW.data   <> OLD.data
  OR NEW.sha256 <> OLD.sha256
  OR NEW.mime   <> OLD.mime
  OR NEW.byte_len <> OLD.byte_len
  OR NEW.width  IS NOT OLD.width
  OR NEW.height IS NOT OLD.height
BEGIN
    SELECT RAISE(ABORT,
        'art rows are immutable; insert a new content-addressed row and relink via track_art');
END;

-- Deleting an art row that still has track_art references (an orphan an
-- external writer can produce with foreign_keys OFF) bumps every referencing
-- track, so the mount rebuilds and serves a clean EIO on the orphan rather
-- than streaming stale bytes from an old cached layout. Inert on the normal
-- gc_orphan_art path, where the deleted row has no references.
CREATE TRIGGER art_ad AFTER DELETE ON art BEGIN
    UPDATE tracks SET content_version = content_version + 1,
                      updated_at = CAST(strftime('%s','now') AS INTEGER)
    WHERE id IN (SELECT track_id FROM track_art WHERE art_id = OLD.id);
END;

-- Scanner-owned geometry feeds the synthesized layout, but upsert_track does
-- not touch content_version. Bump it whenever a geometry column actually
-- changes, making content_version a true superset of served-byte inputs. The
-- WHEN guard is false on this trigger's own nested UPDATE (only content_version
-- changes), so the recursion terminates after exactly one bump.
CREATE TRIGGER tracks_geometry_au
AFTER UPDATE ON tracks
WHEN NEW.format        <> OLD.format
  OR NEW.audio_offset  <> OLD.audio_offset
  OR NEW.audio_length  <> OLD.audio_length
  OR NEW.backing_size  <> OLD.backing_size
  OR NEW.backing_mtime_ns <> OLD.backing_mtime_ns
BEGIN
    UPDATE tracks SET content_version = content_version + 1 WHERE id = NEW.id;
END;

-- FLAC structural blocks feed synthesized headers and flip the synthesis path
-- (legacy front-read fallback vs streamed fast path), so a change must bump.
-- set_structural_blocks is DELETE-then-INSERT (no UPDATE path exists), so these
-- fire on every rewrite; the resulting over-bump on a byte-identical re-probe
-- is harmless monotone churn (content_version is compared only for equality).
CREATE TRIGGER structural_blocks_ai AFTER INSERT ON structural_blocks BEGIN
    UPDATE tracks SET content_version = content_version + 1 WHERE id = NEW.track_id;
END;
CREATE TRIGGER structural_blocks_ad AFTER DELETE ON structural_blocks BEGIN
    UPDATE tracks SET content_version = content_version + 1 WHERE id = OLD.track_id;
END;
";

const MIGRATION_V2: &str = r"
-- fingerprint/content_hash are scanner-owned content identities. Neither is
-- UNIQUE and the index is NON-unique BY DESIGN: duplicate-content tracks (same
-- album in two places, genuine dupes) legitimately share both values, and a
-- UNIQUE constraint would abort the scan batch on the second copy. Correctness
-- comes from the refind logic (unique-missing candidate + confirmation), not
-- from DB uniqueness. Both columns carry a length(x) = 64 CHECK locking them
-- to SHA-256 hex (Task E2 benchmark locked the hash to SHA-256: under a
-- realistic SSD/HDD I/O profile the fingerprint adds ~8.6%; the RAM
-- microbench's higher ratio is an I/O-elimination artifact — see
-- the benchmarks docs). Hash function is now fixed, so the CHECK is added here.
ALTER TABLE tracks ADD COLUMN fingerprint  TEXT
    CHECK (fingerprint IS NULL OR length(fingerprint) = 64);
ALTER TABLE tracks ADD COLUMN content_hash TEXT
    CHECK (content_hash IS NULL OR length(content_hash) = 64);
CREATE INDEX tracks_fingerprint_idx ON tracks(fingerprint);

-- Rebuild `tags` with a byte-accurate value cap (#505). SQLite's length() on
-- TEXT counts characters, so the V1 `CHECK (length(value) <= 262144)` was up to
-- ~4x looser than the documented 256 KiB byte bound; length(CAST(value AS BLOB))
-- counts bytes. SQLite cannot alter a CHECK in place, so recreate the table
-- (V2 is unreleased — this is folded in rather than added as a new migration).
-- Pre-existing over-cap rows (only reachable on an upgraded store) are dropped:
-- the read-time guard already counts bytes, so they were unreadable anyway, and
-- carrying them would abort the rebuild on the new CHECK.
CREATE TABLE tags_new (
    track_id   INTEGER NOT NULL REFERENCES tracks(id) ON DELETE CASCADE,
    key        TEXT NOT NULL,
    value      TEXT NOT NULL,
    ordinal    INTEGER NOT NULL DEFAULT 0,
    value_blob BLOB,
    PRIMARY KEY (track_id, key, ordinal),
    CHECK (ordinal >= 0),
    CHECK (value_blob IS NULL OR value = ''),
    CHECK (length(key) <= 256),
    CHECK (length(key) >= 1
           AND key NOT GLOB '*[' || char(1) || '-' || char(31) || ']*'),
    CHECK (length(CAST(value AS BLOB)) <= 262144),
    CHECK (value_blob IS NULL OR length(value_blob) <= 16711680)
);
INSERT INTO tags_new (track_id, key, value, ordinal, value_blob)
    SELECT track_id, key, value, ordinal, value_blob FROM tags
    WHERE length(CAST(value AS BLOB)) <= 262144;
DROP TABLE tags;
ALTER TABLE tags_new RENAME TO tags;

-- DROP TABLE tags dropped its INSERT/UPDATE/DELETE triggers; recreate them
-- verbatim so the content_version/updated_at bump contract is unchanged.
CREATE TRIGGER tags_ai AFTER INSERT ON tags BEGIN
    UPDATE tracks SET content_version = content_version + 1,
                      updated_at = CAST(strftime('%s','now') AS INTEGER)
    WHERE id = NEW.track_id;
END;
CREATE TRIGGER tags_au AFTER UPDATE ON tags BEGIN
    UPDATE tracks SET content_version = content_version + 1,
                      updated_at = CAST(strftime('%s','now') AS INTEGER)
    WHERE id = NEW.track_id;
END;
CREATE TRIGGER tags_ad AFTER DELETE ON tags BEGIN
    UPDATE tracks SET content_version = content_version + 1,
                      updated_at = CAST(strftime('%s','now') AS INTEGER)
    WHERE id = OLD.track_id;
END;
";

const MIGRATION_V3: &str = r"
-- Widen the two caps musefs invented rather than inherited (#644).
--
-- `tags.value` moves 256 KiB -> 16 MiB - 1 (FLAC's 24-bit metadata-block
-- ceiling, the largest tag synthesis could ever serve) and
-- `track_art.description` moves 1 KiB -> 8 KiB. Both are *widenings*, so the
-- refills need no WHERE filter and drop no rows -- unlike V2's narrowing, which
-- had to shed over-cap rows to avoid aborting on its own new CHECK.
--
-- SQLite cannot alter a CHECK in place, so both tables are recreated. V1/V2
-- text is left untouched: they must stay replayable for a V1 -> V2 -> V3
-- upgrade, and their literals are frozen history, not the current caps.

-- `art_ad`'s body reads `track_art`. ALTER TABLE ... RENAME reparses the whole
-- schema and would fail with 'error in trigger art_ad: no such table' while
-- track_art is momentarily absent, so drop it up front and recreate it verbatim
-- below. (V2's `tags` rebuild needed no such dance: nothing referenced `tags`.)
DROP TRIGGER art_ad;

CREATE TABLE tags_v3 (
    track_id   INTEGER NOT NULL REFERENCES tracks(id) ON DELETE CASCADE,
    key        TEXT NOT NULL,
    value      TEXT NOT NULL,
    ordinal    INTEGER NOT NULL DEFAULT 0,
    value_blob BLOB,
    PRIMARY KEY (track_id, key, ordinal),
    CHECK (ordinal >= 0),
    CHECK (value_blob IS NULL OR value = ''),
    CHECK (length(key) <= 256),
    CHECK (length(key) >= 1
           AND key NOT GLOB '*[' || char(1) || '-' || char(31) || ']*'),
    CHECK (length(CAST(value AS BLOB)) <= 16777215),
    CHECK (value_blob IS NULL OR length(value_blob) <= 16711680)
);
INSERT INTO tags_v3 (track_id, key, value, ordinal, value_blob)
    SELECT track_id, key, value, ordinal, value_blob FROM tags;
DROP TABLE tags;
ALTER TABLE tags_v3 RENAME TO tags;

CREATE TABLE track_art_v3 (
    track_id     INTEGER NOT NULL REFERENCES tracks(id) ON DELETE CASCADE,
    art_id       INTEGER NOT NULL REFERENCES art(id),
    picture_type INTEGER NOT NULL DEFAULT 3,
    description  TEXT NOT NULL DEFAULT '',
    ordinal      INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (track_id, ordinal),
    CHECK (picture_type BETWEEN 0 AND 20),
    CHECK (ordinal >= 0),
    CHECK (length(description) <= 8192)
);
INSERT INTO track_art_v3 (track_id, art_id, picture_type, description, ordinal)
    SELECT track_id, art_id, picture_type, description, ordinal FROM track_art;
DROP TABLE track_art;
ALTER TABLE track_art_v3 RENAME TO track_art;

-- DROP TABLE took each table's triggers (and track_art's index) with it;
-- recreate them verbatim so the content_version/updated_at bump contract and
-- the reverse art -> track_art edge are unchanged.
CREATE INDEX track_art_art_id_idx ON track_art(art_id);

CREATE TRIGGER tags_ai AFTER INSERT ON tags BEGIN
    UPDATE tracks SET content_version = content_version + 1,
                      updated_at = CAST(strftime('%s','now') AS INTEGER)
    WHERE id = NEW.track_id;
END;
CREATE TRIGGER tags_au AFTER UPDATE ON tags BEGIN
    UPDATE tracks SET content_version = content_version + 1,
                      updated_at = CAST(strftime('%s','now') AS INTEGER)
    WHERE id = NEW.track_id;
END;
CREATE TRIGGER tags_ad AFTER DELETE ON tags BEGIN
    UPDATE tracks SET content_version = content_version + 1,
                      updated_at = CAST(strftime('%s','now') AS INTEGER)
    WHERE id = OLD.track_id;
END;

CREATE TRIGGER track_art_ai AFTER INSERT ON track_art BEGIN
    UPDATE tracks SET content_version = content_version + 1,
                      updated_at = CAST(strftime('%s','now') AS INTEGER)
    WHERE id = NEW.track_id;
END;
CREATE TRIGGER track_art_au AFTER UPDATE ON track_art BEGIN
    UPDATE tracks SET content_version = content_version + 1,
                      updated_at = CAST(strftime('%s','now') AS INTEGER)
    WHERE id = NEW.track_id;
END;
CREATE TRIGGER track_art_ad AFTER DELETE ON track_art BEGIN
    UPDATE tracks SET content_version = content_version + 1,
                      updated_at = CAST(strftime('%s','now') AS INTEGER)
    WHERE id = OLD.track_id;
END;

CREATE TRIGGER art_ad AFTER DELETE ON art BEGIN
    UPDATE tracks SET content_version = content_version + 1,
                      updated_at = CAST(strftime('%s','now') AS INTEGER)
    WHERE id IN (SELECT track_id FROM track_art WHERE art_id = OLD.id);
END;
";

/// Ring capacity of the `track_changes` changelog. Must match the literal in
/// MIGRATION_V1 (guarded by `changelog_cap_constant_matches_migration_sql`).
#[allow(dead_code)]
pub const CHANGELOG_CAP: i64 = 8192;

const MIGRATIONS: &[&str] = &[MIGRATION_V1, MIGRATION_V2, MIGRATION_V3];

/// The `user_version` a fully-migrated store carries. Exported so callers and
/// tests assert "the latest schema" rather than a literal that has to be chased
/// through every test file each time a migration is appended.
pub const LATEST_VERSION: i64 = 3;
const _: () = assert!(
    MIGRATIONS.len() == 3,
    "LATEST_VERSION must match MIGRATIONS"
);

pub fn migrate(conn: &mut Connection) -> Result<()> {
    let latest = LATEST_VERSION;
    let current = conn.pragma_query_value::<i64, _>(None, "user_version", |r| r.get(0))?;
    // A store at a user_version past anything this binary knows about was written
    // by a newer (or third-party) tool that bumped the schema. Refuse it loudly
    // rather than treating it as already-migrated and silently misreading the
    // external-writer contract.
    if current > latest {
        return Err(crate::error::DbError::StoreTooNew {
            found: current,
            supported: latest,
        });
    }
    // Fast path: already at the latest version, no transaction needed.
    if current >= latest {
        return Ok(());
    }
    // Use an IMMEDIATE transaction so the write lock is acquired up front. The
    // user_version read below is then authoritative: a second process opening
    // the same database concurrently blocks here until the first commits, then
    // sees the updated version and skips re-applying the migration.
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let current: i64 = tx.pragma_query_value(None, "user_version", |r| r.get(0))?;
    // Announce only an upgrade this call actually performs: another process may
    // have migrated the store while we waited for the write lock, in which case
    // the version read under the lock is already the latest and the loop below
    // applies nothing.
    let mut work = None;
    if current < latest {
        // `Connection::path` is `Some("")` for an in-memory or temporary store.
        let at = match tx.path().filter(|p| !p.is_empty()) {
            Some(path) => format!(" at {path}"),
            None => String::new(),
        };
        if current == 0 {
            // A store this binary is creating from scratch — nothing is being
            // taken anywhere it cannot come back from, so this stays quiet at
            // the default filter.
            log::info!("creating store schema{at} at version {latest}");
        } else {
            // A pre-existing store is about to be rewritten in place, one way.
            // The user gets this once per store, and wants it in their
            // scrollback if they ever try to roll musefs back.
            log::warn!(
                "upgrading store schema{at} from version {current} to version {latest}; \
                 this is irreversible and the store will no longer open with musefs \
                 builds older than this one"
            );
        }
        work = Some((at, std::time::Instant::now()));
    }
    for (target, sql) in (1i64..).zip(MIGRATIONS) {
        if current < target {
            tx.execute_batch(sql)?;
            tx.pragma_update(None, "user_version", target)?;
        }
    }
    tx.commit()?;
    if let Some((at, started)) = work {
        let secs = started.elapsed().as_secs_f64();
        log::info!("store schema{at} is now at version {latest} (took {secs:.1}s)");
    }
    Ok(())
}

#[cfg(test)]
mod migration_logging_tests {
    use log::{Level, LevelFilter, Log, Metadata, Record};
    use rusqlite::Connection;
    use std::sync::{Mutex, MutexGuard, OnceLock, PoisonError};
    use std::thread::ThreadId;

    /// Records emitted on the capturing thread, newest last.
    static RECORDS: Mutex<Vec<(Level, String)>> = Mutex::new(Vec::new());
    /// Serializes the capturing tests: `log::set_logger` installs one logger per
    /// process, so they share a single buffer.
    static SERIAL: Mutex<()> = Mutex::new(());
    /// The thread whose records are being captured. Everything the rest of the
    /// (parallel) test binary logs is dropped, so a capture only ever sees what
    /// the test itself provoked.
    static CAPTURING: Mutex<Option<ThreadId>> = Mutex::new(None);

    struct Capture;
    static CAPTURE: Capture = Capture;

    impl Log for Capture {
        fn enabled(&self, _: &Metadata) -> bool {
            true
        }
        fn log(&self, record: &Record) {
            if *CAPTURING.lock().unwrap() == Some(std::thread::current().id()) {
                RECORDS
                    .lock()
                    .unwrap()
                    .push((record.level(), record.args().to_string()));
            }
        }
        fn flush(&self) {}
    }

    /// Holds the capture open; the buffer is reachable only through the guard,
    /// so no test can read records it does not own.
    struct Captured {
        records: &'static Mutex<Vec<(Level, String)>>,
        _serial: MutexGuard<'static, ()>,
    }

    impl Captured {
        fn records(&self) -> Vec<(Level, String)> {
            self.records.lock().unwrap().clone()
        }
        /// Drop everything logged so far, so the next assertion sees only what
        /// follows this call.
        fn clear(&self) {
            self.records.lock().unwrap().clear();
        }
    }

    impl Drop for Captured {
        fn drop(&mut self) {
            *CAPTURING.lock().unwrap() = None;
            RECORDS.lock().unwrap().clear();
        }
    }

    /// Start capturing this thread's log records until the returned guard drops.
    fn capture() -> Captured {
        let serial = SERIAL.lock().unwrap_or_else(PoisonError::into_inner);
        static INIT: OnceLock<()> = OnceLock::new();
        INIT.get_or_init(|| {
            log::set_logger(&CAPTURE).expect("no other logger is installed in this test binary");
            log::set_max_level(LevelFilter::Trace);
        });
        RECORDS.lock().unwrap().clear();
        *CAPTURING.lock().unwrap() = Some(std::thread::current().id());
        Captured {
            records: &RECORDS,
            _serial: serial,
        }
    }

    /// A store stamped at v1 with only MIGRATION_V1 applied — what an older
    /// musefs build left behind.
    fn store_at_v1(conn: &Connection) {
        conn.execute_batch(super::MIGRATION_V1).unwrap();
        conn.pragma_update(None, "user_version", 1i64).unwrap();
    }

    #[test]
    fn upgrading_an_existing_store_warns_with_both_versions() {
        let captured = capture();
        let mut conn = Connection::open_in_memory().unwrap();
        store_at_v1(&conn);
        captured.clear();

        super::migrate(&mut conn).unwrap();

        let records = captured.records();
        let warnings: Vec<&String> = records
            .iter()
            .filter(|(level, _)| *level == Level::Warn)
            .map(|(_, msg)| msg)
            .collect();
        assert_eq!(
            warnings.len(),
            1,
            "an in-place upgrade must announce itself exactly once at warn, \
             which is the default filter level; got {records:?}"
        );
        let warning = warnings[0];
        assert!(
            warning.contains("from version 1")
                && warning.contains(&format!("to version {}", super::LATEST_VERSION)),
            "the warning must name the version found and the version reached: {warning}"
        );
        assert!(
            records.iter().any(|(level, msg)| *level == Level::Info
                && msg.contains(&format!("version {}", super::LATEST_VERSION))),
            "a completion line must follow the upgrade: {records:?}"
        );
    }

    #[test]
    fn the_warning_names_the_store_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("library.db");
        {
            let conn = Connection::open(&path).unwrap();
            store_at_v1(&conn);
        }
        let captured = capture();
        let mut conn = Connection::open(&path).unwrap();
        super::migrate(&mut conn).unwrap();

        let records = captured.records();
        let path = path.to_str().unwrap();
        assert!(
            records
                .iter()
                .any(|(level, msg)| *level == Level::Warn && msg.contains(path)),
            "the upgrade warning must identify which store was upgraded: {records:?}"
        );
    }

    #[test]
    fn opening_an_already_current_store_logs_nothing() {
        let captured = capture();
        let mut conn = Connection::open_in_memory().unwrap();
        super::migrate(&mut conn).unwrap();
        captured.clear();

        // The fast path, taken on every open and every mount.
        super::migrate(&mut conn).unwrap();

        assert!(
            captured.records().is_empty(),
            "a store already at the latest version must stay silent: {:?}",
            captured.records()
        );
    }

    #[test]
    fn creating_a_fresh_store_does_not_warn() {
        let captured = capture();
        let mut conn = Connection::open_in_memory().unwrap();
        super::migrate(&mut conn).unwrap();

        let records = captured.records();
        assert!(
            records.iter().all(|(level, _)| *level != Level::Warn),
            "creating a store is not an irreversible upgrade of the user's data, \
             so nothing may reach the default filter: {records:?}"
        );
        assert!(
            records.iter().any(|(level, _)| *level == Level::Info),
            "creating a store should still be visible under -v: {records:?}"
        );
    }
}

fn reference_objects() -> &'static std::collections::BTreeMap<(String, String), String> {
    static REF: std::sync::OnceLock<std::collections::BTreeMap<(String, String), String>> =
        std::sync::OnceLock::new();
    REF.get_or_init(|| {
        let mut conn =
            Connection::open_in_memory().expect("in-memory connection for schema reference");
        migrate(&mut conn).expect("reference migration must succeed on a fresh DB");
        read_schema_objects(&conn).expect("reading reference schema must succeed")
    })
}

fn read_schema_objects(
    conn: &Connection,
) -> crate::Result<std::collections::BTreeMap<(String, String), String>> {
    let mut stmt = conn.prepare(
        "SELECT type, name, COALESCE(sql, '') FROM sqlite_master \
         WHERE name NOT LIKE 'sqlite_%'",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok((
            (r.get::<_, String>(0)?, r.get::<_, String>(1)?),
            r.get::<_, String>(2)?,
        ))
    })?;
    let mut map = std::collections::BTreeMap::new();
    for row in rows {
        let ((ty, name), sql) = row?;
        map.insert((ty, name), sql);
    }
    Ok(map)
}

fn schema_mismatch(key: &(String, String), what: &str) -> crate::error::DbError {
    crate::error::DbError::SchemaMismatch {
        object: format!("{} {} ({what})", key.0, key.1),
    }
}

pub(crate) fn validate_identity(conn: &Connection) -> crate::Result<()> {
    let reference = reference_objects();
    let actual = read_schema_objects(conn)?;

    let mut keys: Vec<&(String, String)> = reference.keys().chain(actual.keys()).collect();
    keys.sort();
    keys.dedup();
    for key in keys {
        match (reference.get(key), actual.get(key)) {
            (Some(r), Some(a)) if r != a => return Err(schema_mismatch(key, "altered")),
            (Some(_), None) => return Err(schema_mismatch(key, "missing")),
            (None, Some(_)) => return Err(schema_mismatch(key, "unexpected")),
            _ => {}
        }
    }

    let mut fk = conn.prepare("PRAGMA foreign_key_check")?;
    let mut rows = fk.query([])?;
    if let Some(row) = rows.next()? {
        let table: String = row.get(0)?;
        return Err(crate::error::DbError::SchemaMismatch {
            object: format!("foreign key violation in table {table}"),
        });
    }
    Ok(())
}

#[cfg(test)]
mod baseline_tests {
    use rusqlite::Connection;

    #[test]
    fn baseline_creates_value_blob_and_structural_blocks_and_is_idempotent() {
        let mut conn = Connection::open_in_memory().unwrap();
        super::migrate(&mut conn).unwrap();
        let uv: i64 = conn
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap();
        assert_eq!(
            uv,
            super::LATEST_VERSION,
            "migrate() must reach the latest migration"
        );

        // value_blob exists on tags and defaults to NULL.
        conn.execute(
            "INSERT INTO tracks (backing_path, format, audio_offset, audio_length, \
             backing_size, backing_mtime_ns, updated_at) \
             VALUES ('/a.flac','flac',0,1,1,0,0)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO tags (track_id, key, value, ordinal) VALUES (1,'artist','A',0)",
            [],
        )
        .unwrap();
        let blob_is_null: bool = conn
            .query_row(
                "SELECT value_blob IS NULL FROM tags WHERE track_id=1 AND key='artist'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(blob_is_null);

        // structural_blocks table accepts a row.
        conn.execute(
            "INSERT INTO structural_blocks (track_id, kind, ordinal, body) \
             VALUES (1,'STREAMINFO',0,X'00')",
            [],
        )
        .unwrap();

        // Re-running migrate is a no-op (idempotent).
        super::migrate(&mut conn).unwrap();
        let uv2: i64 = conn
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap();
        assert_eq!(uv2, super::LATEST_VERSION);
    }

    #[test]
    fn migration_v2_adds_fingerprint_and_content_hash_columns() {
        let mut conn = Connection::open_in_memory().unwrap();
        super::migrate(&mut conn).unwrap();
        assert_eq!(
            conn.pragma_query_value::<i64, _>(None, "user_version", |r| r.get(0))
                .unwrap(),
            super::LATEST_VERSION,
            "migrate() must stamp user_version with the latest migration index"
        );
        // Both columns exist, are nullable, and default to NULL.
        conn.execute(
            "INSERT INTO tracks
                (backing_path, format, audio_offset, audio_length, backing_size,
                 backing_mtime_ns, backing_ctime_ns, updated_at)
             VALUES ('/x.flac','flac',0,10,10,0,0,0)",
            [],
        )
        .unwrap();
        let (fp, ch): (Option<String>, Option<String>) = conn
            .query_row(
                "SELECT fingerprint, content_hash FROM tracks WHERE backing_path='/x.flac'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(fp, None);
        assert_eq!(ch, None);
    }

    /// The SQL literal and the exported constant must not drift.
    #[test]
    fn changelog_cap_constant_matches_migration_sql() {
        assert!(super::MIGRATION_V1.contains(&format!("NEW.seq - {}", super::CHANGELOG_CAP)));
    }

    /// The caps a later migration has since widened live in V1/V2 as *frozen
    /// history* — those steps must stay replayable byte-for-byte for the
    /// V1 -> V2 -> V3 upgrade path, so their literals are pinned to the values
    /// they shipped with and deliberately NOT to `crate::limits`. Binding them
    /// to the live constants (as this test once did) makes any future cap
    /// change look like it must be back-edited into released migration text.
    #[test]
    fn superseded_migration_literals_are_frozen() {
        assert!(super::MIGRATION_V1.contains("length(value) <= 262144"));
        assert!(super::MIGRATION_V1.contains("length(description) <= 1024"));
        assert!(
            super::MIGRATION_V2.contains("length(CAST(value AS BLOB)) <= 262144"),
            "V2's byte-accurate rebuild (#505) shipped at the 256 KiB cap"
        );
    }

    /// The literals in the *latest* definition of each table are what a fresh
    /// `migrate()` leaves behind, so those are the ones that must track
    /// `crate::limits`. V3 owns `tags` and `track_art`; V1 still owns `art` and
    /// `structural_blocks`.
    #[test]
    fn check_literals_match_limits_constants() {
        use crate::limits::*;
        let v1 = super::MIGRATION_V1;
        let v3 = super::MIGRATION_V3;
        // V3 rebuilds `tags` at FLAC's block ceiling and `track_art` at 8 KiB (#644).
        assert!(v3.contains(&format!("length(key) <= {MAX_TAG_KEY_LEN}")));
        assert!(v3.contains(&format!(
            "length(CAST(value AS BLOB)) <= {MAX_TAG_VALUE_LEN}"
        )));
        assert!(v3.contains(&format!("length(value_blob) <= {MAX_BINARY_TAG_BYTES}")));
        assert!(v3.contains(&format!("length(description) <= {MAX_ART_DESCRIPTION_LEN}")));
        // Still V1-owned: no later migration recreates `art` or `structural_blocks`.
        assert!(v1.contains(&format!("length(mime) <= {MAX_ART_MIME_LEN}")));
        assert!(v1.contains(&format!("byte_len <= {MAX_ART_BYTES}")));
        assert!(v1.contains(&format!("length(body) <= {MAX_STRUCTURAL_BODY_LEN}")));
        let kinds = STRUCTURAL_KINDS
            .iter()
            .map(|k| format!("'{k}'"))
            .collect::<Vec<_>>()
            .join(",");
        assert!(v1.contains(&format!("kind IN ({kinds})")));
    }
}

#[cfg(test)]
mod changelog_tests {
    use rusqlite::Connection;

    fn count_changes(conn: &Connection) -> i64 {
        conn.query_row("SELECT COUNT(*) FROM track_changes", [], |r| r.get(0))
            .unwrap()
    }

    fn insert_track(conn: &Connection, path: &str) {
        conn.execute(
            "INSERT INTO tracks (backing_path, format, audio_offset, audio_length, \
             backing_size, backing_mtime_ns, updated_at) \
             VALUES (?1,'flac',0,1,1,0,0)",
            [path],
        )
        .unwrap();
    }

    #[test]
    fn v3_changelog_records_insert_update_delete() {
        let mut conn = Connection::open_in_memory().unwrap();
        super::migrate(&mut conn).unwrap();
        let uv: i64 = conn
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap();
        assert_eq!(uv, super::LATEST_VERSION);

        insert_track(&conn, "/a.flac"); // tracks AI -> 1 row
        assert_eq!(count_changes(&conn), 1);

        conn.execute(
            "UPDATE tracks SET backing_mtime_ns = 1 WHERE id = 1", // tracks AU -> 2 rows (geometry trigger nested UPDATE)
            [],
        )
        .unwrap();
        assert_eq!(count_changes(&conn), 3);

        conn.execute("DELETE FROM tracks WHERE id = 1", []).unwrap(); // tracks AD -> 1 row
        assert_eq!(count_changes(&conn), 4);

        let ids: Vec<i64> = conn
            .prepare("SELECT track_id FROM track_changes ORDER BY seq")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(ids, vec![1, 1, 1, 1]);
    }

    /// Load-bearing nested-trigger dependency (see spec): a bare tag write fires
    /// tags_ai -> UPDATE tracks -> tracks changelog trigger. If this fails, nested
    /// activation is off in this SQLite build; the fix is PRAGMA-level, not schema.
    #[test]
    fn v3_bare_tag_insert_produces_changelog_row_via_nested_trigger() {
        let mut conn = Connection::open_in_memory().unwrap();
        super::migrate(&mut conn).unwrap();
        insert_track(&conn, "/a.flac");
        let before = count_changes(&conn);
        conn.execute(
            "INSERT INTO tags (track_id, key, value, ordinal) VALUES (1,'artist','A',0)",
            [],
        )
        .unwrap();
        assert_eq!(
            count_changes(&conn),
            before + 1,
            "tags_ai's UPDATE tracks must fire the changelog trigger (nested activation)"
        );
        let last_id: i64 = conn
            .query_row(
                "SELECT track_id FROM track_changes ORDER BY seq DESC LIMIT 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(last_id, 1);
    }

    #[test]
    fn v3_prune_keeps_ring_bounded_and_contiguous() {
        let mut conn = Connection::open_in_memory().unwrap();
        super::migrate(&mut conn).unwrap();
        insert_track(&conn, "/a.flac");
        // Drive CAP + 100 changelog inserts via track updates.
        for i in 0..(super::CHANGELOG_CAP + 100) {
            conn.execute("UPDATE tracks SET backing_mtime_ns = ?1 WHERE id = 1", [i])
                .unwrap();
        }
        let (min_seq, max_seq, rows): (i64, i64, i64) = conn
            .query_row(
                "SELECT MIN(seq), MAX(seq), COUNT(*) FROM track_changes",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(
            rows,
            super::CHANGELOG_CAP,
            "ring must hold exactly CAP rows"
        );
        assert_eq!(min_seq, max_seq - super::CHANGELOG_CAP + 1, "contiguous");
    }

    #[test]
    fn v4_metadata_edit_bumps_version_and_appends_one_changelog_row() {
        let mut conn = Connection::open_in_memory().unwrap();
        super::migrate(&mut conn).unwrap();
        insert_track(&conn, "/a.flac");
        let cv_before: i64 = conn
            .query_row("SELECT content_version FROM tracks WHERE id=1", [], |r| {
                r.get(0)
            })
            .unwrap();
        let changes_before = count_changes(&conn);

        conn.execute(
            "INSERT INTO tags (track_id, key, value, ordinal) VALUES (1,'artist','A',0)",
            [],
        )
        .unwrap();

        let cv_after: i64 = conn
            .query_row("SELECT content_version FROM tracks WHERE id=1", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(cv_after, cv_before + 1, "content_version must bump by one");
        assert_eq!(
            count_changes(&conn),
            changes_before + 1,
            "exactly one changelog row from the edit (nested trigger)"
        );
    }
}

#[cfg(test)]
mod schema_py_tests {
    use std::fmt::Write as _;

    use rusqlite::Connection;

    use super::MIGRATIONS;

    /// Canonical SQL text: each migration verbatim, preceded by a banner and
    /// followed by the user_version stamp `migrate()` applies after that step.
    /// Equivalent to `migrate()` on a fresh DB only — no fast-path/partial-
    /// upgrade logic — which is what `schema_sql_matches_migrate` proves.
    fn render_schema_sql() -> String {
        let mut sql = String::new();
        for (i, migration) in MIGRATIONS.iter().enumerate() {
            let n = i + 1;
            if i > 0 {
                sql.push('\n');
            }
            // write!/writeln! (not push_str(&format!(..))): the workspace's
            // pedantic clippy lints deny format_push_string, and a bare
            // write! ending in '\n' would trip write_with_newline.
            let _ = write!(sql, "-- ── MIGRATION_V{n} ──");
            sql.push_str(migration); // every MIGRATION_Vn starts and ends with '\n'
            let _ = writeln!(sql, "PRAGMA user_version = {n};");
        }
        sql
    }

    /// Full content of the generated musefs_common/schema.py. Must stay
    /// `ruff format --check`-clean (comment header + two assignments is).
    fn render_schema_py() -> String {
        format!(
            "# GENERATED from musefs-db/src/schema.rs — do not edit.\n\
             # Regenerate: MUSEFS_REGEN_SCHEMA_PY=1 cargo test -p musefs-db schema_py\n\
             # Re-vendor:  python contrib/python-musefs/vendor_to_picard.py\n\
             \n\
             SCHEMA_SQL = \"\"\"\\\n\
             {sql}\"\"\"\n\
             \n\
             USER_VERSION = {version}\n\
             \n\
             # Byte cap on `tags.value`, mirrored so an external writer can check a\n\
             # value before the `CHECK` does. Generated from the Rust constant: it\n\
             # moved once already (#644) and a hand-kept copy would silently rot.\n\
             MAX_TAG_VALUE_LEN = {max_tag_value_len}\n",
            sql = render_schema_sql(),
            version = MIGRATIONS.len(),
            max_tag_value_len = crate::limits::MAX_TAG_VALUE_LEN
        )
    }

    fn dump_master(conn: &Connection) -> Vec<(String, String, String, Option<String>)> {
        conn.prepare("SELECT type, name, tbl_name, sql FROM sqlite_master ORDER BY type, name")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
    }

    fn user_version(conn: &Connection) -> i64 {
        conn.pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap()
    }

    /// The rendering must stay semantically identical to migrate() on a fresh
    /// DB — guards against migrate() ever growing a non-SQL step the
    /// concatenation cannot represent.
    #[test]
    fn schema_sql_matches_migrate() {
        let rendered = Connection::open_in_memory().unwrap();
        rendered.execute_batch(&render_schema_sql()).unwrap();

        let mut migrated = Connection::open_in_memory().unwrap();
        super::migrate(&mut migrated).unwrap();

        assert_eq!(dump_master(&rendered), dump_master(&migrated));
        assert_eq!(user_version(&rendered), user_version(&migrated));
        assert_eq!(user_version(&rendered), super::LATEST_VERSION);
    }

    #[test]
    fn migrate_does_not_reapply_an_already_applied_step() {
        // A DB sitting at version 1 (only V1 applied) must receive ONLY the
        // remaining steps on the next migrate. `current < target` skips the
        // already-applied V1; `<=` would re-run V1 (`CREATE TABLE tracks` ->
        // "table already exists"), so a clean upgrade to the latest version proves
        // the loop never re-applies a step it already ran. (The current==latest
        // case can't exercise this — migrate fast-paths out before the loop.)
        let mut conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(MIGRATIONS[0]).unwrap(); // apply V1 only
        conn.pragma_update(None, "user_version", 1i64).unwrap();
        super::migrate(&mut conn).expect("upgrading from v1 must apply only the remaining steps");
        assert_eq!(user_version(&conn), super::LATEST_VERSION);
    }

    #[test]
    fn v2_rebuild_enforces_byte_cap_and_drops_oversize_rows() {
        // #505: V2 rebuilds `tags` with a byte-accurate value cap. Simulate a v1
        // store, plant an over-cap multibyte value (legal under V1's char-counting
        // CHECK: 150_000 chars / 300_000 bytes) plus a normal one, then upgrade.
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(MIGRATIONS[0]).unwrap(); // V1 only
        conn.pragma_update(None, "user_version", 1i64).unwrap();
        conn.execute(
            "INSERT INTO tracks (backing_path, format, audio_offset, audio_length, \
             backing_size, backing_mtime_ns, backing_ctime_ns, updated_at) \
             VALUES ('/a.flac','flac',0,0,0,0,0,0)",
            [],
        )
        .unwrap();
        let big = "é".repeat(150_000);
        conn.execute(
            "INSERT INTO tags (track_id, key, value, ordinal) VALUES (1,'big',?1,0)",
            rusqlite::params![big],
        )
        .expect("V1 char-counting CHECK accepts a 150_000-char value");
        conn.execute(
            "INSERT INTO tags (track_id, key, value, ordinal) VALUES (1,'ok','fine',0)",
            [],
        )
        .unwrap();

        // V2 only, applied directly. `migrate()` would run on through V3, whose
        // widened cap (#644) accepts this value — that is the later step's
        // business, asserted separately below. This test is about what V2 did.
        conn.execute_batch(MIGRATIONS[1]).unwrap();
        conn.pragma_update(None, "user_version", 2i64).unwrap();

        // The over-cap row is dropped; the valid row survives.
        let keys: Vec<String> = {
            let mut stmt = conn.prepare("SELECT key FROM tags ORDER BY key").unwrap();
            let rows = stmt.query_map([], |r| r.get(0)).unwrap();
            rows.collect::<rusqlite::Result<_>>().unwrap()
        };
        assert_eq!(keys, vec!["ok".to_string()]);

        // The rebuilt CHECK rejects an over-cap multibyte value at write.
        assert!(
            conn.execute(
                "INSERT INTO tags (track_id, key, value, ordinal) VALUES (1,'big2',?1,0)",
                rusqlite::params![big],
            )
            .is_err(),
            "byte-accurate CHECK must reject the write"
        );

        // The tag triggers were recreated: an insert still bumps content_version.
        let cv = |c: &Connection| -> i64 {
            c.query_row("SELECT content_version FROM tracks WHERE id=1", [], |r| {
                r.get(0)
            })
            .unwrap()
        };
        let before = cv(&conn);
        conn.execute(
            "INSERT INTO tags (track_id, key, value, ordinal) VALUES (1,'extra','v',0)",
            [],
        )
        .unwrap();
        assert!(
            cv(&conn) > before,
            "tags_ai trigger must survive the rebuild"
        );
    }

    /// #644: V3 widens `tags.value` to FLAC's block ceiling and
    /// `track_art.description` to 8 KiB. Both are widenings, so unlike V2's
    /// narrowing the rebuild must carry every existing row across — losing user
    /// tags to a migration that only relaxes a limit would be gratuitous.
    #[test]
    fn v3_rebuild_widens_caps_and_preserves_rows() {
        use crate::limits::{MAX_ART_DESCRIPTION_LEN, MAX_TAG_VALUE_LEN};
        let mut conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(MIGRATIONS[0]).unwrap();
        conn.execute_batch(MIGRATIONS[1]).unwrap();
        conn.pragma_update(None, "user_version", 2i64).unwrap();
        conn.execute(
            "INSERT INTO tracks (backing_path, format, audio_offset, audio_length, \
             backing_size, backing_mtime_ns, backing_ctime_ns, updated_at) \
             VALUES ('/a.flac','flac',0,0,0,0,0,0)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO art (sha256, mime, width, height, byte_len, data) \
             VALUES (?1,'image/png',1,1,1,X'00')",
            [&"a".repeat(64)],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO tags (track_id, key, value, ordinal) VALUES (1,'artist','A',0)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO track_art (track_id, art_id, picture_type, description, ordinal) \
             VALUES (1,1,3,'cover',0)",
            [],
        )
        .unwrap();

        super::migrate(&mut conn).expect("upgrade to v3");

        assert_eq!(
            conn.pragma_query_value::<i64, _>(None, "user_version", |r| r.get(0))
                .unwrap(),
            super::LATEST_VERSION
        );
        // Rows survive: a widening must never drop data.
        let (key, value): (String, String) = conn
            .query_row("SELECT key, value FROM tags", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        assert_eq!((key.as_str(), value.as_str()), ("artist", "A"));
        let desc: String = conn
            .query_row("SELECT description FROM track_art", [], |r| r.get(0))
            .unwrap();
        assert_eq!(desc, "cover");

        // A value the V2 cap rejected now writes cleanly — the point of #644.
        let over_v2 = "é".repeat(150_000);
        assert!(
            over_v2.len() > 262_144 && i64::try_from(over_v2.len()).unwrap() < MAX_TAG_VALUE_LEN
        );
        conn.execute(
            "INSERT INTO tags (track_id, key, value, ordinal) VALUES (1,'lyrics',?1,0)",
            rusqlite::params![over_v2],
        )
        .expect("V3 accepts a value the 256 KiB cap rejected");
        conn.execute(
            "INSERT INTO track_art (track_id, art_id, picture_type, description, ordinal) \
             VALUES (1,1,3,?1,1)",
            rusqlite::params!["d".repeat(2048)],
        )
        .expect("V3 accepts a description the 1 KiB cap rejected");

        // The triggers and the reverse-edge index came back with the rebuild.
        let objects: Vec<String> = {
            let mut stmt = conn
                .prepare(
                    "SELECT name FROM sqlite_master \
                     WHERE name IN ('tags_ai','tags_au','tags_ad','track_art_ai', \
                                    'track_art_au','track_art_ad','art_ad', \
                                    'track_art_art_id_idx') ORDER BY name",
                )
                .unwrap();
            let rows = stmt.query_map([], |r| r.get(0)).unwrap();
            rows.collect::<rusqlite::Result<_>>().unwrap()
        };
        assert_eq!(
            objects,
            vec![
                "art_ad",
                "tags_ad",
                "tags_ai",
                "tags_au",
                "track_art_ad",
                "track_art_ai",
                "track_art_art_id_idx",
                "track_art_au",
            ],
            "V3 must restore every object its two DROP TABLEs took with them"
        );
        // `art_ad` reads `track_art`; a rebuild that renamed the table out from
        // under that trigger leaves a body that errors at *fire* time, not at
        // migration time, so the object-name check above would not catch it.
        // Fire it, in the exact shape it was written for: an art row deleted
        // while track_art still references it (only reachable with FKs off).
        conn.pragma_update(None, "foreign_keys", false).unwrap();
        conn.execute("DELETE FROM art WHERE id = 1", [])
            .expect("art_ad must still resolve track_art after the V3 rebuild");
        let bumped: i64 = conn
            .query_row("SELECT content_version FROM tracks WHERE id=1", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert!(bumped > 0, "art_ad must bump the referencing track");
        assert_eq!(MAX_ART_DESCRIPTION_LEN, 8192);
    }

    /// NOT #[ignore]d on purpose: the compare path must run under plain
    /// `cargo test` or the CI drift gate doesn't exist. Only the write
    /// behavior is env-gated.
    #[test]
    fn schema_py_fixture_is_fresh() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../contrib/python-musefs/src/musefs_common/schema.py");
        let rendered = render_schema_py();
        if std::env::var_os("MUSEFS_REGEN_SCHEMA_PY").is_some() {
            std::fs::write(&path, &rendered).expect("write schema.py");
            return;
        }
        let on_disk = std::fs::read_to_string(&path).expect(
            "musefs_common/schema.py missing — regenerate with \
             MUSEFS_REGEN_SCHEMA_PY=1 cargo test -p musefs-db schema_py",
        );
        assert_eq!(
            on_disk, rendered,
            "musefs_common/schema.py is stale. Regenerate: \
             MUSEFS_REGEN_SCHEMA_PY=1 cargo test -p musefs-db schema_py, \
             then: python contrib/python-musefs/vendor_to_picard.py"
        );
    }
}

#[cfg(test)]
mod constraint_tests {
    use rusqlite::Connection;

    /// A fresh, fully-migrated DB with foreign_keys ON — mirrors how
    /// `Db::configure` opens the real connection (lib.rs:78).
    fn fresh(conn: &mut Connection) {
        conn.pragma_update(None, "foreign_keys", true).unwrap();
        super::migrate(conn).unwrap();
    }

    fn insert_track(conn: &Connection, path: &str) {
        conn.execute(
            "INSERT INTO tracks (backing_path, format, audio_offset, audio_length, \
             backing_size, backing_mtime_ns, updated_at) \
             VALUES (?1,'flac',0,1,1,0,0)",
            [path],
        )
        .unwrap();
    }

    /// A complete, valid row across all four tables migrates and reads back.
    #[test]
    fn v4_valid_rows_migrate_and_read_cleanly() {
        let mut conn = Connection::open_in_memory().unwrap();
        fresh(&mut conn);
        let uv: i64 = conn
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap();
        assert_eq!(uv, super::LATEST_VERSION);

        insert_track(&conn, "/a.flac");
        conn.execute(
            "INSERT INTO art (sha256, mime, width, height, byte_len, data) \
             VALUES (?1,'image/png',1,1,1,X'00')",
            [&"a".repeat(64)],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO tags (track_id, key, value, ordinal) VALUES (1,'artist','A',0)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO track_art (track_id, art_id, picture_type, ordinal) \
             VALUES (1,1,3,0)",
            [],
        )
        .unwrap();

        let (off, len, sz): (i64, i64, i64) = conn
            .query_row(
                "SELECT audio_offset, audio_length, backing_size FROM tracks WHERE id=1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!((off, len, sz), (0, 1, 1));
        let pic: i64 = conn
            .query_row(
                "SELECT picture_type FROM track_art WHERE track_id=1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(pic, 3);
    }

    /// SQL expression yielding `n` copies of `c`, for cap-boundary inserts.
    /// `hex(zeroblob(k))` is `2k` ASCII '0's, so one `replace` plus a `substr`
    /// builds any length without materializing a multi-MiB Rust string just to
    /// interpolate it into a statement.
    fn repeated_char_sql(c: char, n: i64) -> String {
        let half = n / 2 + n % 2;
        format!("substr(replace(hex(zeroblob({half})), '0', '{c}'), 1, {n})")
    }

    fn rejected(conn: &Connection, sql: &str) {
        assert!(
            conn.execute(sql, []).is_err(),
            "expected rejection for: {sql}"
        );
    }

    #[test]
    fn v4_tracks_rejects_unknown_format() {
        let mut conn = Connection::open_in_memory().unwrap();
        fresh(&mut conn);
        rejected(
            &conn,
            "INSERT INTO tracks (backing_path, format, audio_offset, audio_length, \
             backing_size, backing_mtime_ns, updated_at) \
             VALUES ('/x','aiff',0,0,0,0,0)",
        );
    }

    #[test]
    fn v4_tracks_accepts_every_pinned_format() {
        let mut conn = Connection::open_in_memory().unwrap();
        fresh(&mut conn);
        for (i, fmt) in ["flac", "mp3", "m4a", "opus", "vorbis", "oggflac", "wav"]
            .iter()
            .enumerate()
        {
            conn.execute(
                "INSERT INTO tracks (backing_path, format, audio_offset, audio_length, \
                 backing_size, backing_mtime_ns, updated_at) \
                 VALUES (?1, ?2, 0, 0, 0, 0, 0)",
                rusqlite::params![format!("/t{i}"), fmt],
            )
            .unwrap();
        }
    }

    #[test]
    fn v4_tracks_rejects_negative_audio_offset() {
        let mut conn = Connection::open_in_memory().unwrap();
        fresh(&mut conn);
        rejected(
            &conn,
            "INSERT INTO tracks (backing_path, format, audio_offset, audio_length, \
             backing_size, backing_mtime_ns, updated_at) \
             VALUES ('/x','flac',-1,0,0,0,0)",
        );
    }

    #[test]
    fn v4_tracks_rejects_negative_audio_length() {
        let mut conn = Connection::open_in_memory().unwrap();
        fresh(&mut conn);
        rejected(
            &conn,
            "INSERT INTO tracks (backing_path, format, audio_offset, audio_length, \
             backing_size, backing_mtime_ns, updated_at) \
             VALUES ('/x','flac',0,-1,0,0,0)",
        );
    }

    #[test]
    fn v4_tracks_rejects_negative_backing_size() {
        let mut conn = Connection::open_in_memory().unwrap();
        fresh(&mut conn);
        rejected(
            &conn,
            "INSERT INTO tracks (backing_path, format, audio_offset, audio_length, \
             backing_size, backing_mtime_ns, updated_at) \
             VALUES ('/x','flac',0,0,-1,0,0)",
        );
    }

    #[test]
    fn v4_tracks_rejects_negative_backing_mtime_ns() {
        let mut conn = Connection::open_in_memory().unwrap();
        fresh(&mut conn);
        rejected(
            &conn,
            "INSERT INTO tracks (backing_path, format, audio_offset, audio_length, \
             backing_size, backing_mtime_ns, updated_at) \
             VALUES ('/x','flac',0,0,0,-1,0)",
        );
    }

    #[test]
    fn v4_tracks_rejects_negative_content_version() {
        let mut conn = Connection::open_in_memory().unwrap();
        fresh(&mut conn);
        rejected(
            &conn,
            "INSERT INTO tracks (backing_path, format, audio_offset, audio_length, \
             backing_size, backing_mtime_ns, content_version, updated_at) \
             VALUES ('/x','flac',0,0,0,0,-1,0)",
        );
    }

    #[test]
    fn v4_tracks_rejects_negative_updated_at() {
        let mut conn = Connection::open_in_memory().unwrap();
        fresh(&mut conn);
        rejected(
            &conn,
            "INSERT INTO tracks (backing_path, format, audio_offset, audio_length, \
             backing_size, backing_mtime_ns, updated_at) \
             VALUES ('/x','flac',0,0,0,0,-1)",
        );
    }

    #[test]
    fn v4_tracks_rejects_audio_range_past_backing_size() {
        let mut conn = Connection::open_in_memory().unwrap();
        fresh(&mut conn);
        rejected(
            &conn,
            "INSERT INTO tracks (backing_path, format, audio_offset, audio_length, \
             backing_size, backing_mtime_ns, updated_at) \
             VALUES ('/x','flac',5,10,14,0,0)",
        );
        conn.execute(
            "INSERT INTO tracks (backing_path, format, audio_offset, audio_length, \
             backing_size, backing_mtime_ns, updated_at) \
             VALUES ('/ok','flac',5,10,15,0,0)",
            [],
        )
        .unwrap();
    }

    #[test]
    fn v4_tracks_rejects_update_pushing_audio_past_backing() {
        let mut conn = Connection::open_in_memory().unwrap();
        fresh(&mut conn);
        insert_track(&conn, "/x.flac");
        rejected(&conn, "UPDATE tracks SET backing_size = 0 WHERE id = 1");
    }

    fn seed_track_and_art(conn: &Connection) {
        insert_track(conn, "/seed.flac");
        conn.execute(
            "INSERT INTO art (sha256, mime, byte_len, data) VALUES (?1,'image/png',1,X'00')",
            [&"c".repeat(64)],
        )
        .unwrap();
    }

    #[test]
    fn v4_tags_rejects_negative_ordinal() {
        let mut conn = Connection::open_in_memory().unwrap();
        fresh(&mut conn);
        seed_track_and_art(&conn);
        rejected(
            &conn,
            "INSERT INTO tags (track_id, key, value, ordinal) VALUES (1,'artist','A',-1)",
        );
    }

    #[test]
    fn v4_tags_rejects_blob_with_nonempty_value() {
        let mut conn = Connection::open_in_memory().unwrap();
        fresh(&mut conn);
        seed_track_and_art(&conn);
        rejected(
            &conn,
            "INSERT INTO tags (track_id, key, value, ordinal, value_blob) \
             VALUES (1,'cover','nonempty',0,X'00')",
        );
    }

    #[test]
    fn v4_tags_accepts_blob_with_empty_value() {
        let mut conn = Connection::open_in_memory().unwrap();
        fresh(&mut conn);
        seed_track_and_art(&conn);
        conn.execute(
            "INSERT INTO tags (track_id, key, value, ordinal, value_blob) \
             VALUES (1,'cover','',0,X'00')",
            [],
        )
        .unwrap();
    }

    #[test]
    fn v4_tags_accepts_empty_text_value_without_blob() {
        let mut conn = Connection::open_in_memory().unwrap();
        fresh(&mut conn);
        seed_track_and_art(&conn);
        conn.execute(
            "INSERT INTO tags (track_id, key, value, ordinal) VALUES (1,'comment','',0)",
            [],
        )
        .unwrap();
    }

    #[test]
    fn v4_art_rejects_byte_len_mismatch() {
        let mut conn = Connection::open_in_memory().unwrap();
        fresh(&mut conn);
        rejected(
            &conn,
            "INSERT INTO art (sha256, mime, byte_len, data) \
             VALUES ('aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa',\
             'image/png',5,X'00')",
        );
    }

    #[test]
    fn v4_art_rejects_sha256_wrong_length() {
        let mut conn = Connection::open_in_memory().unwrap();
        fresh(&mut conn);
        rejected(
            &conn,
            "INSERT INTO art (sha256, mime, byte_len, data) \
             VALUES ('tooshort','image/png',1,X'00')",
        );
    }

    #[test]
    fn v4_art_rejects_negative_width() {
        let mut conn = Connection::open_in_memory().unwrap();
        fresh(&mut conn);
        rejected(
            &conn,
            "INSERT INTO art (sha256, mime, width, byte_len, data) \
             VALUES ('aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa',\
             'image/png',-1,1,X'00')",
        );
    }

    #[test]
    fn v4_art_rejects_negative_height() {
        let mut conn = Connection::open_in_memory().unwrap();
        fresh(&mut conn);
        rejected(
            &conn,
            "INSERT INTO art (sha256, mime, height, byte_len, data) \
             VALUES ('aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa',\
             'image/png',-1,1,X'00')",
        );
    }

    #[test]
    fn v4_art_accepts_null_dimensions() {
        let mut conn = Connection::open_in_memory().unwrap();
        fresh(&mut conn);
        conn.execute(
            "INSERT INTO art (sha256, mime, width, height, byte_len, data) \
             VALUES ('aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa',\
             'image/png',NULL,NULL,1,X'00')",
            [],
        )
        .unwrap();
    }

    #[test]
    fn v4_track_art_rejects_picture_type_above_range() {
        let mut conn = Connection::open_in_memory().unwrap();
        fresh(&mut conn);
        seed_track_and_art(&conn);
        rejected(
            &conn,
            "INSERT INTO track_art (track_id, art_id, picture_type, ordinal) \
             VALUES (1,1,21,0)",
        );
    }

    #[test]
    fn v4_track_art_rejects_negative_picture_type() {
        let mut conn = Connection::open_in_memory().unwrap();
        fresh(&mut conn);
        seed_track_and_art(&conn);
        rejected(
            &conn,
            "INSERT INTO track_art (track_id, art_id, picture_type, ordinal) \
             VALUES (1,1,-1,0)",
        );
    }

    #[test]
    fn v4_track_art_accepts_picture_type_bounds() {
        let mut conn = Connection::open_in_memory().unwrap();
        fresh(&mut conn);
        seed_track_and_art(&conn);
        conn.execute(
            "INSERT INTO track_art (track_id, art_id, picture_type, ordinal) \
             VALUES (1,1,0,0)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO track_art (track_id, art_id, picture_type, ordinal) \
             VALUES (1,1,20,1)",
            [],
        )
        .unwrap();
    }

    #[test]
    fn v4_track_art_rejects_negative_ordinal() {
        let mut conn = Connection::open_in_memory().unwrap();
        fresh(&mut conn);
        seed_track_and_art(&conn);
        rejected(
            &conn,
            "INSERT INTO track_art (track_id, art_id, picture_type, ordinal) \
             VALUES (1,1,3,-1)",
        );
    }

    #[test]
    fn v4_tags_rejects_oversize_key() {
        let mut conn = Connection::open_in_memory().unwrap();
        fresh(&mut conn);
        insert_track(&conn, "/a.flac");
        let key = "k".repeat(257);
        rejected(
            &conn,
            &format!(
                "INSERT INTO tags (track_id, key, value, ordinal) VALUES (1, '{key}', 'v', 0)"
            ),
        );
    }

    #[test]
    fn v4_tags_accepts_key_at_cap() {
        let mut conn = Connection::open_in_memory().unwrap();
        fresh(&mut conn);
        insert_track(&conn, "/a.flac");
        let key = "k".repeat(256);
        conn.execute(
            &format!(
                "INSERT INTO tags (track_id, key, value, ordinal) VALUES (1, '{key}', 'v', 0)"
            ),
            [],
        )
        .unwrap();
    }

    #[test]
    fn v4_tags_rejects_oversize_value() {
        use crate::limits::MAX_TAG_VALUE_LEN;
        let mut conn = Connection::open_in_memory().unwrap();
        fresh(&mut conn);
        insert_track(&conn, "/a.flac");
        // Built in SQL rather than Rust: at the post-#644 cap the string is
        // 16 MiB, and interpolating one into a statement costs twice that for a
        // test whose whole point is the `<=` boundary.
        let over = repeated_char_sql('v', MAX_TAG_VALUE_LEN + 1);
        rejected(
            &conn,
            &format!("INSERT INTO tags (track_id, key, value, ordinal) VALUES (1, 'k', {over}, 0)"),
        );
    }

    /// The widened cap (#644) accepts exactly at the boundary, so the pair pins
    /// the `CHECK`'s `<=` against an off-by-one in either direction.
    #[test]
    fn v4_tags_accepts_value_at_cap() {
        use crate::limits::MAX_TAG_VALUE_LEN;
        let mut conn = Connection::open_in_memory().unwrap();
        fresh(&mut conn);
        insert_track(&conn, "/a.flac");
        let at = repeated_char_sql('v', MAX_TAG_VALUE_LEN);
        conn.execute_batch(&format!(
            "INSERT INTO tags (track_id, key, value, ordinal) VALUES (1, 'k', {at}, 0)"
        ))
        .unwrap();
        let len: i64 = conn
            .query_row("SELECT length(CAST(value AS BLOB)) FROM tags", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(len, MAX_TAG_VALUE_LEN);
    }

    #[test]
    fn v4_structural_rejects_unknown_kind_and_negative_ordinal_and_oversize_body() {
        let mut conn = Connection::open_in_memory().unwrap();
        fresh(&mut conn);
        insert_track(&conn, "/a.flac");
        rejected(
            &conn,
            "INSERT INTO structural_blocks (track_id, kind, ordinal, body) VALUES (1, 'APPLICATION', 0, X'00')",
        );
        rejected(
            &conn,
            "INSERT INTO structural_blocks (track_id, kind, ordinal, body) VALUES (1, 'STREAMINFO', -1, X'00')",
        );
        // length(body) cap: a blob of MAX+1 zero bytes via zeroblob().
        rejected(
            &conn,
            "INSERT INTO structural_blocks (track_id, kind, ordinal, body) VALUES (1, 'STREAMINFO', 0, zeroblob(16777216))",
        );
    }

    #[test]
    fn v4_structural_accepts_body_at_cap() {
        let mut conn = Connection::open_in_memory().unwrap();
        fresh(&mut conn);
        insert_track(&conn, "/a.flac");
        conn.execute(
            "INSERT INTO structural_blocks (track_id, kind, ordinal, body) VALUES (1, 'STREAMINFO', 0, zeroblob(16777215))",
            [],
        )
        .unwrap();
    }

    #[test]
    fn v4_art_rejects_oversize_mime_and_byte_len() {
        let mut conn = Connection::open_in_memory().unwrap();
        fresh(&mut conn);
        let mime = "x".repeat(256);
        rejected(
            &conn,
            &format!(
                "INSERT INTO art (sha256, mime, byte_len, data) VALUES ('{}', '{mime}', 1, X'00')",
                "a".repeat(64)
            ),
        );
        // byte_len cap (byte_len must equal length(data), so use a zeroblob).
        rejected(
            &conn,
            &format!(
                "INSERT INTO art (sha256, mime, byte_len, data) VALUES ('{}', 'image/png', 16711681, zeroblob(16711681))",
                "b".repeat(64)
            ),
        );
    }

    #[test]
    fn v4_track_art_rejects_oversize_description() {
        use crate::limits::MAX_ART_DESCRIPTION_LEN;
        let mut conn = Connection::open_in_memory().unwrap();
        fresh(&mut conn);
        seed_track_and_art(&conn);
        let over = repeated_char_sql('d', MAX_ART_DESCRIPTION_LEN + 1);
        rejected(
            &conn,
            &format!(
                "INSERT INTO track_art (track_id, art_id, picture_type, description, ordinal) VALUES (1, 1, 3, {over}, 0)"
            ),
        );
    }

    #[test]
    fn v4_track_art_accepts_description_at_cap() {
        use crate::limits::MAX_ART_DESCRIPTION_LEN;
        let mut conn = Connection::open_in_memory().unwrap();
        fresh(&mut conn);
        seed_track_and_art(&conn);
        let at = repeated_char_sql('d', MAX_ART_DESCRIPTION_LEN);
        conn.execute_batch(&format!(
            "INSERT INTO track_art (track_id, art_id, picture_type, description, ordinal) VALUES (1, 1, 3, {at}, 0)"
        ))
        .unwrap();
        let len: i64 = conn
            .query_row("SELECT length(description) FROM track_art", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(len, MAX_ART_DESCRIPTION_LEN);
    }

    #[test]
    fn fresh_db_has_all_baseline_triggers() {
        let mut conn = Connection::open_in_memory().unwrap();
        fresh(&mut conn);
        let names: Vec<String> = conn
            .prepare("SELECT name FROM sqlite_master WHERE type='trigger' ORDER BY name")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        for expected in [
            "tags_ai",
            "tags_au",
            "tags_ad",
            "track_art_ai",
            "track_art_au",
            "track_art_ad",
            "tracks_changelog_ai",
            "tracks_changelog_au",
            "tracks_changelog_ad",
            "track_changes_prune",
            "art_reject_content_update",
            "art_ad",
            "tracks_geometry_au",
            "structural_blocks_ai",
            "structural_blocks_ad",
        ] {
            assert!(
                names.iter().any(|n| n == expected),
                "missing trigger on fresh DB: {expected}"
            );
        }
        assert_eq!(names.len(), 15, "unexpected trigger count: {names:?}");
    }

    #[test]
    fn v4_tags_rejects_empty_and_control_char_keys() {
        let mut conn = Connection::open_in_memory().unwrap();
        fresh(&mut conn);
        conn.execute(
            "INSERT INTO tracks (backing_path, format, audio_offset, audio_length, \
             backing_size, backing_mtime_ns, updated_at) \
             VALUES ('/x','flac',0,0,0,0,0)",
            [],
        )
        .unwrap();
        rejected(
            &conn,
            "INSERT INTO tags (track_id, key, value, ordinal) VALUES (1,'','v',0)",
        );
        rejected(
            &conn,
            "INSERT INTO tags (track_id, key, value, ordinal) VALUES (1,char(7),'v',0)",
        );
        // '=' is NOT a DB-floor violation — only Vorbis synthesis bars it.
        conn.execute(
            "INSERT INTO tags (track_id, key, value, ordinal) VALUES (1,'a=b','c',0)",
            [],
        )
        .unwrap();
    }

    #[test]
    fn v2_fingerprint_check_rejects_wrong_length_and_accepts_null_and_64_chars() {
        let mut conn = Connection::open_in_memory().unwrap();
        fresh(&mut conn);
        insert_track(&conn, "/fp.flac");

        // NULL is accepted (no fingerprint yet).
        conn.execute(
            "UPDATE tracks SET fingerprint = NULL WHERE backing_path = '/fp.flac'",
            [],
        )
        .unwrap();

        // A valid 64-char SHA-256 hex string is accepted.
        conn.execute(
            &format!(
                "UPDATE tracks SET fingerprint = '{}' WHERE backing_path = '/fp.flac'",
                "a".repeat(64)
            ),
            [],
        )
        .unwrap();

        // A too-short fingerprint (1 char) is rejected.
        rejected(
            &conn,
            "UPDATE tracks SET fingerprint = 'x' WHERE backing_path = '/fp.flac'",
        );

        // A too-long fingerprint (65 chars) is also rejected.
        rejected(
            &conn,
            &format!(
                "UPDATE tracks SET fingerprint = '{}' WHERE backing_path = '/fp.flac'",
                "a".repeat(65)
            ),
        );
    }
}

#[cfg(test)]
mod identity_tests {
    use super::*;
    use crate::error::DbError;

    fn migrated() -> Connection {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.pragma_update(None, "foreign_keys", true).unwrap();
        migrate(&mut conn).unwrap();
        conn
    }

    #[test]
    fn honest_schema_passes() {
        let conn = migrated();
        validate_identity(&conn).unwrap();
    }

    #[test]
    fn honest_schema_with_rows_passes() {
        let conn = migrated();
        conn.execute(
            "INSERT INTO tracks (backing_path, format, audio_offset, audio_length, \
             backing_size, backing_mtime_ns, updated_at) VALUES ('/a.flac','flac',0,1,1,0,0)",
            [],
        )
        .unwrap();
        let has_seq: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE name = 'sqlite_sequence'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(has_seq, 1, "precondition: insert created sqlite_sequence");
        validate_identity(&conn).unwrap();
    }

    #[test]
    fn missing_trigger_is_rejected() {
        let conn = migrated();
        conn.execute_batch("DROP TRIGGER tags_ai").unwrap();
        let err = validate_identity(&conn).unwrap_err();
        match err {
            DbError::SchemaMismatch { object } => {
                assert!(object.contains("tags_ai"), "names the object: {object}");
                assert!(object.contains("missing"), "classifies it: {object}");
            }
            other => panic!("expected SchemaMismatch, got {other:?}"),
        }
    }

    #[test]
    fn extra_object_is_rejected() {
        let conn = migrated();
        conn.execute_batch("CREATE TABLE sneaky (x)").unwrap();
        let err = validate_identity(&conn).unwrap_err();
        assert!(matches!(err, DbError::SchemaMismatch { .. }));
    }

    #[test]
    fn altered_table_is_rejected() {
        let conn = migrated();
        conn.execute_batch(
            "PRAGMA foreign_keys=OFF; \
             DROP TABLE tags; \
             CREATE TABLE tags (track_id INTEGER NOT NULL, key TEXT, value TEXT, \
                ordinal INTEGER, value_blob BLOB, PRIMARY KEY (track_id, key, ordinal));",
        )
        .unwrap();
        let err = validate_identity(&conn).unwrap_err();
        match err {
            DbError::SchemaMismatch { object } => assert!(object.contains("tags")),
            other => panic!("expected SchemaMismatch, got {other:?}"),
        }
    }

    #[test]
    fn altered_object_with_no_other_diffs_is_rejected() {
        // `art` has no triggers and (when empty) no FK children to cascade, so
        // recreating it with a different shape makes the *altered* table the
        // ONLY schema difference — isolating the `r != a` guard so a
        // `r != a -> false` mutant cannot survive on the back of an unrelated
        // missing/extra object.
        let conn = migrated();
        conn.execute_batch(
            "PRAGMA foreign_keys=OFF; \
             DROP TABLE art; \
             CREATE TABLE art (id INTEGER PRIMARY KEY, sha256 TEXT, mime TEXT, \
                width INTEGER, height INTEGER, byte_len INTEGER, data BLOB);",
        )
        .unwrap();
        let err = validate_identity(&conn).unwrap_err();
        match err {
            DbError::SchemaMismatch { object } => {
                assert!(object.contains("art"), "names the object: {object}");
                assert!(
                    object.contains("altered"),
                    "classifies it as altered: {object}"
                );
            }
            other => panic!("expected SchemaMismatch (altered), got {other:?}"),
        }
    }

    #[test]
    fn foreign_key_violation_is_rejected() {
        let conn = migrated();
        conn.execute_batch(
            "PRAGMA foreign_keys=OFF; \
             INSERT INTO art (sha256, mime, byte_len, data) \
             VALUES ('aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa', \
                     'image/png', 1, X'00'); \
             INSERT INTO track_art (track_id, art_id, picture_type, ordinal) VALUES (999, 1, 3, 0);",
        )
        .unwrap();
        let err = validate_identity(&conn).unwrap_err();
        match err {
            DbError::SchemaMismatch { object } => assert!(object.contains("foreign key")),
            other => panic!("expected SchemaMismatch (fk), got {other:?}"),
        }
    }

    #[test]
    fn first_offender_is_deterministic_in_type_name_order() {
        let conn = migrated();
        conn.execute_batch(
            "PRAGMA foreign_keys=OFF; DROP TRIGGER track_art_ai; DROP TRIGGER tags_ai;",
        )
        .unwrap();
        let err = validate_identity(&conn).unwrap_err();
        match err {
            DbError::SchemaMismatch { object } => assert!(object.contains("tags_ai"), "{object}"),
            other => panic!("expected SchemaMismatch, got {other:?}"),
        }
    }
}

#[cfg(test)]
mod art_immutability_tests {
    use rusqlite::{Connection, params};

    /// A fresh, fully-migrated DB with `foreign_keys` OFF — that is what lets
    /// `deleting_referenced_art_bumps_tracks` produce the orphan case.
    fn migrated() -> Connection {
        let mut conn = Connection::open_in_memory().unwrap();
        super::migrate(&mut conn).unwrap();
        conn.pragma_update(None, "foreign_keys", false).unwrap();
        conn
    }

    fn insert_track(conn: &Connection, path: &str) -> i64 {
        conn.execute(
            "INSERT INTO tracks (backing_path, format, audio_offset, audio_length, \
             backing_size, backing_mtime_ns, updated_at) \
             VALUES (?1,'flac',0,1,1,0,0)",
            [path],
        )
        .unwrap();
        conn.last_insert_rowid()
    }

    fn insert_art(conn: &Connection, sha: &str, data: &[u8]) -> i64 {
        conn.execute(
            "INSERT INTO art (sha256, mime, width, height, byte_len, data) \
             VALUES (?1,'image/png',NULL,NULL,?2,?3)",
            params![sha, i64::try_from(data.len()).unwrap(), data],
        )
        .unwrap();
        conn.last_insert_rowid()
    }

    #[test]
    fn migration_reaches_latest_user_version() {
        let conn = migrated();
        let uv: i64 = conn
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap();
        assert_eq!(uv, super::LATEST_VERSION);
    }

    #[test]
    fn art_content_update_is_rejected() {
        let conn = migrated();
        let a = insert_art(&conn, &"a".repeat(64), &[1, 2, 3]);
        assert!(
            conn.execute("UPDATE art SET mime='image/jpeg' WHERE id=?1", [a])
                .is_err()
        );
        assert!(
            conn.execute("UPDATE art SET byte_len=99 WHERE id=?1", [a])
                .is_err()
        );
        assert!(
            conn.execute("UPDATE art SET data=X'04050607' WHERE id=?1", [a])
                .is_err()
        );
        assert!(
            conn.execute("UPDATE art SET width=10 WHERE id=?1", [a])
                .is_err()
        );
        assert!(
            conn.execute(
                "UPDATE art SET sha256=?1 WHERE id=?2",
                params![&"b".repeat(64), a],
            )
            .is_err()
        );
    }

    #[test]
    fn art_noop_update_is_allowed() {
        let conn = migrated();
        let a = insert_art(&conn, &"a".repeat(64), &[1, 2, 3]);
        conn.execute("UPDATE art SET mime=mime WHERE id=?1", [a])
            .unwrap();
    }

    #[test]
    fn deleting_referenced_art_bumps_tracks() {
        let conn = migrated();
        let t = insert_track(&conn, "/a.flac");
        let a = insert_art(&conn, &"a".repeat(64), &[1, 2, 3]);
        conn.execute(
            "INSERT INTO track_art (track_id, art_id, picture_type, ordinal) \
             VALUES (?1,?2,3,0)",
            [t, a],
        )
        .unwrap();
        let cv0: i64 = conn
            .query_row("SELECT content_version FROM tracks WHERE id=?1", [t], |r| {
                r.get(0)
            })
            .unwrap();
        conn.execute("DELETE FROM art WHERE id=?1", [a]).unwrap();
        let cv1: i64 = conn
            .query_row("SELECT content_version FROM tracks WHERE id=?1", [t], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(cv1, cv0 + 1, "art delete must bump the referencing track");
    }

    #[test]
    fn deleting_unreferenced_art_bumps_nothing() {
        let conn = migrated();
        let t = insert_track(&conn, "/a.flac");
        let a = insert_art(&conn, &"a".repeat(64), &[1, 2, 3]);
        let cv0: i64 = conn
            .query_row("SELECT content_version FROM tracks WHERE id=?1", [t], |r| {
                r.get(0)
            })
            .unwrap();
        conn.execute("DELETE FROM art WHERE id=?1", [a]).unwrap();
        let cv1: i64 = conn
            .query_row("SELECT content_version FROM tracks WHERE id=?1", [t], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(cv1, cv0, "deleting an unreferenced art row must not bump");
    }
}
