use crate::Result;
use rusqlite::Connection;

const MIGRATION_V1: &str = r#"
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
"#;

const MIGRATIONS: &[&str] = &[MIGRATION_V1];

pub fn migrate(conn: &Connection) -> Result<()> {
    let latest = MIGRATIONS.len() as i64;
    // Fast path: already at the latest version, no transaction needed.
    if conn.pragma_query_value::<i64, _>(None, "user_version", |r| r.get(0))? >= latest {
        return Ok(());
    }
    // Re-read user_version inside the write transaction so the value is
    // authoritative under the write lock. Two processes opening the same
    // database concurrently could otherwise both read a stale version and the
    // second would fail applying DDL the first already committed.
    let tx = conn.unchecked_transaction()?;
    let current: i64 = tx.pragma_query_value(None, "user_version", |r| r.get(0))?;
    for (i, sql) in MIGRATIONS.iter().enumerate() {
        let target = (i + 1) as i64;
        if current < target {
            tx.execute_batch(sql)?;
            tx.pragma_update(None, "user_version", target)?;
        }
    }
    tx.commit()?;
    Ok(())
}
