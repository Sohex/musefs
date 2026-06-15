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
-- from DB uniqueness. A length CHECK on fingerprint is added here once the hash
-- function is locked by the benchmark (Task E2) — different hash, different hex
-- width — and the whole feature is one unreleased branch, so we amend this same
-- migration rather than adding a follow-up.
ALTER TABLE tracks ADD COLUMN fingerprint  TEXT;
ALTER TABLE tracks ADD COLUMN content_hash TEXT
    CHECK (content_hash IS NULL OR length(content_hash) = 64);
CREATE INDEX tracks_fingerprint_idx ON tracks(fingerprint);
";

/// Ring capacity of the `track_changes` changelog. Must match the literal in
/// MIGRATION_V1 (guarded by `changelog_cap_constant_matches_migration_sql`).
#[allow(dead_code)]
pub const CHANGELOG_CAP: i64 = 8192;

const MIGRATIONS: &[&str] = &[MIGRATION_V1, MIGRATION_V2];

pub fn migrate(conn: &mut Connection) -> Result<()> {
    let latest = i64::try_from(MIGRATIONS.len()).expect("MIGRATIONS count must fit i64");
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
    for (target, sql) in (1i64..).zip(MIGRATIONS) {
        if current < target {
            tx.execute_batch(sql)?;
            tx.pragma_update(None, "user_version", target)?;
        }
    }
    tx.commit()?;
    Ok(())
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
        assert_eq!(uv, 2);

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
        assert_eq!(uv2, 2);
    }

    #[test]
    fn migration_v2_adds_fingerprint_and_content_hash_columns() {
        let mut conn = Connection::open_in_memory().unwrap();
        super::migrate(&mut conn).unwrap();
        assert_eq!(
            conn.pragma_query_value::<i64, _>(None, "user_version", |r| r.get(0))
                .unwrap(),
            2,
            "V2 migration must bump user_version to 2"
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

    #[test]
    fn check_literals_match_limits_constants() {
        use crate::limits::*;
        let sql = super::MIGRATION_V1;
        assert!(sql.contains(&format!("length(key) <= {MAX_TAG_KEY_LEN}")));
        assert!(sql.contains(&format!("length(value) <= {MAX_TAG_VALUE_LEN}")));
        assert!(sql.contains(&format!("length(value_blob) <= {MAX_BINARY_TAG_BYTES}")));
        assert!(sql.contains(&format!("length(mime) <= {MAX_ART_MIME_LEN}")));
        assert!(sql.contains(&format!("byte_len <= {MAX_ART_BYTES}")));
        assert!(sql.contains(&format!("length(description) <= {MAX_ART_DESCRIPTION_LEN}")));
        assert!(sql.contains(&format!("length(body) <= {MAX_STRUCTURAL_BODY_LEN}")));
        let kinds = STRUCTURAL_KINDS
            .iter()
            .map(|k| format!("'{k}'"))
            .collect::<Vec<_>>()
            .join(",");
        assert!(sql.contains(&format!("kind IN ({kinds})")));
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
        assert_eq!(uv, 2);

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
        assert_eq!(uv, 2);

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
        let mut conn = Connection::open_in_memory().unwrap();
        fresh(&mut conn);
        insert_track(&conn, "/a.flac");
        let big = "v".repeat(262_145);
        rejected(
            &conn,
            &format!(
                "INSERT INTO tags (track_id, key, value, ordinal) VALUES (1, 'k', '{big}', 0)"
            ),
        );
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
        let mut conn = Connection::open_in_memory().unwrap();
        fresh(&mut conn);
        seed_track_and_art(&conn);
        let desc = "d".repeat(1025);
        rejected(
            &conn,
            &format!(
                "INSERT INTO track_art (track_id, art_id, picture_type, description, ordinal) VALUES (1, 1, 3, '{desc}', 0)"
            ),
        );
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
    fn migration_reaches_user_version_1() {
        let conn = migrated();
        let uv: i64 = conn
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap();
        assert_eq!(uv, 2);
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
