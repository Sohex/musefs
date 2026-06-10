use crate::Result;
use rusqlite::{Connection, TransactionBehavior};

const MIGRATION_V1: &str = r"
CREATE TABLE tracks (
    id              INTEGER PRIMARY KEY,
    backing_path    TEXT NOT NULL UNIQUE,
    format          TEXT NOT NULL,
    audio_offset    INTEGER NOT NULL,
    audio_length    INTEGER NOT NULL,
    backing_size    INTEGER NOT NULL,
    backing_mtime   INTEGER NOT NULL,
    content_version INTEGER NOT NULL DEFAULT 0,
    updated_at      INTEGER NOT NULL
);

CREATE TABLE tags (
    track_id INTEGER NOT NULL REFERENCES tracks(id) ON DELETE CASCADE,
    key      TEXT NOT NULL,
    value    TEXT NOT NULL,
    ordinal  INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (track_id, key, ordinal)
);

CREATE TABLE art (
    id       INTEGER PRIMARY KEY,
    sha256   TEXT NOT NULL UNIQUE,
    mime     TEXT NOT NULL,
    width    INTEGER,
    height   INTEGER,
    byte_len INTEGER NOT NULL,
    data     BLOB NOT NULL
);

CREATE TABLE track_art (
    track_id     INTEGER NOT NULL REFERENCES tracks(id) ON DELETE CASCADE,
    art_id       INTEGER NOT NULL REFERENCES art(id),
    picture_type INTEGER NOT NULL DEFAULT 3,
    description  TEXT NOT NULL DEFAULT '',
    ordinal      INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (track_id, ordinal)
);

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
";

const MIGRATION_V2: &str = r"
-- Binary tag payloads live alongside text tags. A row is binary iff
-- value_blob IS NOT NULL; binary rows store '' in value.
ALTER TABLE tags ADD COLUMN value_blob BLOB;

-- Read-only, derived-from-file structural metadata (FLAC STREAMINFO/SEEKTABLE).
-- NOT part of the editable `tags` contract: external tools never touch it.
CREATE TABLE structural_blocks (
    track_id INTEGER NOT NULL REFERENCES tracks(id) ON DELETE CASCADE,
    kind     TEXT NOT NULL,
    ordinal  INTEGER NOT NULL DEFAULT 0,
    body     BLOB NOT NULL,
    PRIMARY KEY (track_id, kind, ordinal)
);
";

/// Ring capacity of the `track_changes` changelog. Must match the literal in
/// MIGRATION_V3 (guarded by `changelog_cap_constant_matches_migration_sql`).
#[allow(dead_code)]
pub const CHANGELOG_CAP: i64 = 8192;

const MIGRATION_V3: &str = r"
-- Bounded changelog ring for O(changed) refresh. Every metadata edit funnels
-- through an UPDATE on the tracks row (the V1 tags/track_art triggers), so
-- triggers on tracks alone capture all writers. Relies on SQLite nested
-- trigger activation (on by default; distinct from PRAGMA recursive_triggers).
CREATE TABLE track_changes (
    seq      INTEGER PRIMARY KEY AUTOINCREMENT,
    track_id INTEGER NOT NULL
);

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
";

// V4 adds CHECK constraints to the four editable/contract tables. SQLite
// cannot ALTER ADD CONSTRAINT, so each table is rebuilt. Order is load-bearing:
//   1. Stash every row into a TEMP table (TEMP tables carry no FK, so the
//      drops below cannot cascade through them).
//   2. DROP the four real tables. This is done with foreign_keys ON (the
//      migrate() transaction cannot toggle the pragma), so dropping a parent
//      would ON DELETE CASCADE its children — but the children are dropped
//      first and their data already lives in the stash, so nothing is lost.
//   3. CREATE the four tables WITH the CHECK constraints.
//   4. Refill from the stash. The INSERT..SELECT copies enforce every new
//      CHECK, so any pre-existing violating row aborts the migration.
//   5. Recreate the triggers AFTER the refill. Dropping/renaming a table
//      destroys triggers defined ON it: the V1 tags_ai/au/ad, the V1
//      track_art_ai/au/ad, and the V3 tracks_changelog_ai/au/ad. Recreating
//      them only after step 4 means the bulk refill of `tracks` does NOT fire
//      tracks_changelog_ai and pump the self-pruning track_changes ring.
//      track_changes_prune (ON track_changes) and structural_blocks are NOT
//      rebuilt and keep their triggers/data untouched.
const MIGRATION_V4: &str = r"
CREATE TEMP TABLE _m4_tracks AS SELECT * FROM tracks;
CREATE TEMP TABLE _m4_tags AS SELECT * FROM tags;
CREATE TEMP TABLE _m4_art AS SELECT * FROM art;
CREATE TEMP TABLE _m4_track_art AS SELECT * FROM track_art;

DROP TABLE track_art;
DROP TABLE tags;
DROP TABLE art;
DROP TABLE tracks;

CREATE TABLE tracks (
    id              INTEGER PRIMARY KEY,
    backing_path    TEXT NOT NULL UNIQUE,
    format          TEXT NOT NULL,
    audio_offset    INTEGER NOT NULL,
    audio_length    INTEGER NOT NULL,
    backing_size    INTEGER NOT NULL,
    backing_mtime   INTEGER NOT NULL,
    content_version INTEGER NOT NULL DEFAULT 0,
    updated_at      INTEGER NOT NULL,
    CHECK (format IN ('flac','mp3','m4a','opus','vorbis','oggflac','wav')),
    CHECK (audio_offset >= 0),
    CHECK (audio_length >= 0),
    CHECK (backing_size >= 0),
    CHECK (backing_mtime >= 0),
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
    CHECK (value_blob IS NULL OR value = '')
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
    CHECK (height IS NULL OR height >= 0)
);

CREATE TABLE track_art (
    track_id     INTEGER NOT NULL REFERENCES tracks(id) ON DELETE CASCADE,
    art_id       INTEGER NOT NULL REFERENCES art(id),
    picture_type INTEGER NOT NULL DEFAULT 3,
    description  TEXT NOT NULL DEFAULT '',
    ordinal      INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (track_id, ordinal),
    CHECK (picture_type BETWEEN 0 AND 20),
    CHECK (ordinal >= 0)
);

INSERT INTO tracks SELECT * FROM _m4_tracks;
INSERT INTO art SELECT * FROM _m4_art;
INSERT INTO tags SELECT * FROM _m4_tags;
INSERT INTO track_art SELECT * FROM _m4_track_art;

DROP TABLE _m4_track_art;
DROP TABLE _m4_tags;
DROP TABLE _m4_art;
DROP TABLE _m4_tracks;

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
";

const MIGRATIONS: &[&str] = &[MIGRATION_V1, MIGRATION_V2, MIGRATION_V3, MIGRATION_V4];

pub fn migrate(conn: &mut Connection) -> Result<()> {
    let latest = i64::try_from(MIGRATIONS.len()).expect("MIGRATIONS count must fit i64");
    // Fast path: already at the latest version, no transaction needed.
    if conn.pragma_query_value::<i64, _>(None, "user_version", |r| r.get(0))? >= latest {
        return Ok(());
    }
    // Use an IMMEDIATE transaction so the write lock is acquired up front. The
    // user_version read below is then authoritative: a second process opening
    // the same database concurrently blocks here until the first commits, then
    // sees the updated version and skips re-applying the migration.
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let current: i64 = tx.pragma_query_value(None, "user_version", |r| r.get(0))?;
    for (target, sql) in (1i64..).zip(MIGRATIONS) {
        if current < target {
            tx.execute_batch(sql)?;
            tx.pragma_update(None, "user_version", target)?;
        }
    }
    tx.commit()?;
    Ok(())
}

#[cfg(test)]
mod migration_v2_tests {
    use rusqlite::Connection;

    #[test]
    fn v2_adds_value_blob_and_structural_blocks_and_is_idempotent() {
        let mut conn = Connection::open_in_memory().unwrap();
        super::migrate(&mut conn).unwrap();
        // user_version reflects the number of migrations applied.
        let uv: i64 = conn
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap();
        assert_eq!(uv, 4);

        // value_blob exists on tags and defaults to NULL.
        conn.execute(
            "INSERT INTO tracks (backing_path, format, audio_offset, audio_length, \
             backing_size, backing_mtime, updated_at) \
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
        assert_eq!(uv2, 4);
    }

    #[test]
    fn v1_rows_survive_v2_migration_with_null_value_blob() {
        let mut conn = Connection::open_in_memory().unwrap();
        // Apply ONLY V1, then stamp the version so migrate() resumes at V2.
        conn.execute_batch(super::MIGRATIONS[0]).unwrap();
        conn.pragma_update(None, "user_version", 1i64).unwrap();

        // Insert under the V1 schema (no value_blob column exists yet).
        conn.execute(
            "INSERT INTO tracks (backing_path, format, audio_offset, audio_length, \
             backing_size, backing_mtime, updated_at) \
             VALUES ('/legacy.flac','flac',10,20,30,0,0)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO tags (track_id, key, value, ordinal) VALUES (1,'artist','Legacy',0)",
            [],
        )
        .unwrap();

        // Upgrade V1 -> V2 -> V3.
        super::migrate(&mut conn).unwrap();
        let uv: i64 = conn
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap();
        assert_eq!(uv, 4);

        // The pre-existing row survived unchanged, with value_blob defaulted NULL.
        let (value, blob_is_null): (String, bool) = conn
            .query_row(
                "SELECT value, value_blob IS NULL FROM tags WHERE track_id=1 AND key='artist'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(value, "Legacy");
        assert!(
            blob_is_null,
            "existing tag rows must default value_blob to NULL"
        );

        // The track row survived too.
        let offset: i64 = conn
            .query_row("SELECT audio_offset FROM tracks WHERE id=1", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(offset, 10);
    }
}

#[cfg(test)]
mod migration_v3_tests {
    use rusqlite::Connection;

    fn count_changes(conn: &Connection) -> i64 {
        conn.query_row("SELECT COUNT(*) FROM track_changes", [], |r| r.get(0))
            .unwrap()
    }

    fn insert_track(conn: &Connection, path: &str) {
        conn.execute(
            "INSERT INTO tracks (backing_path, format, audio_offset, audio_length, \
             backing_size, backing_mtime, updated_at) \
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
        assert_eq!(uv, 4);

        insert_track(&conn, "/a.flac"); // tracks AI -> 1 row
        assert_eq!(count_changes(&conn), 1);

        conn.execute(
            "UPDATE tracks SET backing_mtime = 1 WHERE id = 1", // tracks AU -> 1 row
            [],
        )
        .unwrap();
        assert_eq!(count_changes(&conn), 2);

        conn.execute("DELETE FROM tracks WHERE id = 1", []).unwrap(); // tracks AD -> 1 row
        assert_eq!(count_changes(&conn), 3);

        let ids: Vec<i64> = conn
            .prepare("SELECT track_id FROM track_changes ORDER BY seq")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(ids, vec![1, 1, 1]);
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
            conn.execute("UPDATE tracks SET backing_mtime = ?1 WHERE id = 1", [i])
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
    fn v2_db_upgrades_to_v3_preserving_rows() {
        let mut conn = Connection::open_in_memory().unwrap();
        // Apply V1+V2 only, stamp version 2, insert under the V2 schema.
        conn.execute_batch(super::MIGRATIONS[0]).unwrap();
        conn.execute_batch(super::MIGRATIONS[1]).unwrap();
        conn.pragma_update(None, "user_version", 2i64).unwrap();
        insert_track(&conn, "/legacy.flac");

        super::migrate(&mut conn).unwrap();
        let uv: i64 = conn
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap();
        assert_eq!(uv, 4);
        // Pre-migration rows produce no retroactive changelog entries...
        assert_eq!(count_changes(&conn), 0);
        // ...but post-migration edits do.
        conn.execute("UPDATE tracks SET backing_mtime = 9 WHERE id = 1", [])
            .unwrap();
        assert_eq!(count_changes(&conn), 1);
    }

    /// The SQL literal and the exported constant must not drift.
    #[test]
    fn changelog_cap_constant_matches_migration_sql() {
        assert!(super::MIGRATIONS[2].contains(&format!("NEW.seq - {}", super::CHANGELOG_CAP)));
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
             USER_VERSION = {version}\n",
            sql = render_schema_sql(),
            version = MIGRATIONS.len()
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
        assert_eq!(
            user_version(&rendered),
            i64::try_from(MIGRATIONS.len()).unwrap()
        );
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
mod migration_v4_tests {
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
             backing_size, backing_mtime, updated_at) \
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
        assert_eq!(uv, 4);

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

    /// The `tracks` rebuild drops the table while foreign_keys is ON, which
    /// would cascade-delete `tags`/`track_art` if the rows were not stashed
    /// first. Prove the children survive a fresh upgrade carrying real rows.
    #[test]
    fn v4_rebuild_preserves_fk_children() {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.pragma_update(None, "foreign_keys", true).unwrap();
        // Apply V1+V2+V3 only, stamp version 3, insert under the V3 schema.
        conn.execute_batch(super::MIGRATIONS[0]).unwrap();
        conn.execute_batch(super::MIGRATIONS[1]).unwrap();
        conn.execute_batch(super::MIGRATIONS[2]).unwrap();
        conn.pragma_update(None, "user_version", 3i64).unwrap();

        insert_track(&conn, "/legacy.flac");
        conn.execute(
            "INSERT INTO art (sha256, mime, byte_len, data) VALUES (?1,'image/png',1,X'00')",
            [&"b".repeat(64)],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO tags (track_id, key, value, ordinal) VALUES (1,'artist','Legacy',0)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO track_art (track_id, art_id, picture_type, ordinal) \
             VALUES (1,1,3,0)",
            [],
        )
        .unwrap();

        // Upgrade V3 -> V4.
        super::migrate(&mut conn).unwrap();
        let uv: i64 = conn
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap();
        assert_eq!(uv, 4);

        // Children survived the parent rebuild (no cascade-delete).
        let tags: i64 = conn
            .query_row("SELECT COUNT(*) FROM tags", [], |r| r.get(0))
            .unwrap();
        let track_art: i64 = conn
            .query_row("SELECT COUNT(*) FROM track_art", [], |r| r.get(0))
            .unwrap();
        let art: i64 = conn
            .query_row("SELECT COUNT(*) FROM art", [], |r| r.get(0))
            .unwrap();
        assert_eq!((tags, track_art, art), (1, 1, 1));
        assert_eq!(
            Vec::<(i64, String, Option<String>)>::new(),
            conn.prepare("PRAGMA foreign_key_check")
                .unwrap()
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(3)?)))
                .unwrap()
                .collect::<rusqlite::Result<Vec<_>>>()
                .unwrap(),
            "no orphaned FK rows after rebuild"
        );

        // FK is still enforced after the rebuild: an orphan tag is rejected.
        let orphan = conn.execute(
            "INSERT INTO tags (track_id, key, value, ordinal) VALUES (999,'x','y',0)",
            [],
        );
        assert!(orphan.is_err(), "FK must still reject orphan child rows");
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
             backing_size, backing_mtime, updated_at) \
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
                 backing_size, backing_mtime, updated_at) \
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
             backing_size, backing_mtime, updated_at) \
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
             backing_size, backing_mtime, updated_at) \
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
             backing_size, backing_mtime, updated_at) \
             VALUES ('/x','flac',0,0,-1,0,0)",
        );
    }

    #[test]
    fn v4_tracks_rejects_negative_backing_mtime() {
        let mut conn = Connection::open_in_memory().unwrap();
        fresh(&mut conn);
        rejected(
            &conn,
            "INSERT INTO tracks (backing_path, format, audio_offset, audio_length, \
             backing_size, backing_mtime, updated_at) \
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
             backing_size, backing_mtime, content_version, updated_at) \
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
             backing_size, backing_mtime, updated_at) \
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
             backing_size, backing_mtime, updated_at) \
             VALUES ('/x','flac',5,10,14,0,0)",
        );
        conn.execute(
            "INSERT INTO tracks (backing_path, format, audio_offset, audio_length, \
             backing_size, backing_mtime, updated_at) \
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
}
