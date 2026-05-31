use crate::models::{Format, NewTrack, Track};
use crate::{Db, Result};
use rusqlite::{params, Row};

const TRACK_COLS: &str = "id, backing_path, format, audio_offset, audio_length, \
                          backing_size, backing_mtime, content_version, updated_at";

fn row_to_track(r: &Row) -> rusqlite::Result<Track> {
    let fmt: String = r.get("format")?;
    let format = Format::parse(&fmt).ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(
            usize::MAX,
            rusqlite::types::Type::Text,
            format!("unknown format {fmt}").into(),
        )
    })?;
    Ok(Track {
        id: r.get("id")?,
        backing_path: r.get("backing_path")?,
        format,
        audio_offset: r.get("audio_offset")?,
        audio_length: r.get("audio_length")?,
        backing_size: r.get("backing_size")?,
        backing_mtime: r.get("backing_mtime")?,
        content_version: r.get("content_version")?,
        updated_at: r.get("updated_at")?,
    })
}

impl Db {
    pub fn upsert_track(&self, t: &NewTrack) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO tracks
                (backing_path, format, audio_offset, audio_length, backing_size, backing_mtime, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, CAST(strftime('%s','now') AS INTEGER))
             ON CONFLICT(backing_path) DO UPDATE SET
                format        = excluded.format,
                audio_offset  = excluded.audio_offset,
                audio_length  = excluded.audio_length,
                backing_size  = excluded.backing_size,
                backing_mtime = excluded.backing_mtime,
                updated_at    = CAST(strftime('%s','now') AS INTEGER)",
            params![
                t.backing_path,
                t.format.as_str(),
                t.audio_offset,
                t.audio_length,
                t.backing_size,
                t.backing_mtime,
            ],
        )?;
        let id = self.conn.query_row(
            "SELECT id FROM tracks WHERE backing_path = ?1",
            params![t.backing_path],
            |r| r.get(0),
        )?;
        Ok(id)
    }

    pub fn get_track(&self, id: i64) -> Result<Option<Track>> {
        let sql = format!("SELECT {TRACK_COLS} FROM tracks WHERE id = ?1");
        self.query_optional_track(&sql, params![id])
    }

    pub fn get_track_by_path(&self, path: &str) -> Result<Option<Track>> {
        let sql = format!("SELECT {TRACK_COLS} FROM tracks WHERE backing_path = ?1");
        self.query_optional_track(&sql, params![path])
    }

    pub fn list_tracks(&self) -> Result<Vec<Track>> {
        let sql = format!("SELECT {TRACK_COLS} FROM tracks ORDER BY id");
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map([], row_to_track)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn track_content_version(&self, id: i64) -> Result<i64> {
        Ok(self.conn.query_row(
            "SELECT content_version FROM tracks WHERE id = ?1",
            params![id],
            |r| r.get(0),
        )?)
    }

    fn query_optional_track(&self, sql: &str, p: impl rusqlite::Params) -> Result<Option<Track>> {
        let mut stmt = self.conn.prepare(sql)?;
        let mut rows = stmt.query(p)?;
        match rows.next()? {
            Some(r) => Ok(Some(row_to_track(r)?)),
            None => Ok(None),
        }
    }

    /// Delete a track row. Foreign keys cascade to its `tags` and `track_art`
    /// rows; the referenced `art` rows are left for `gc_orphan_art`.
    pub fn delete_track(&self, id: i64) -> Result<()> {
        self.conn
            .execute("DELETE FROM tracks WHERE id = ?1", params![id])?;
        Ok(())
    }

    /// Cheap render-key identity scan for incremental refresh: `(id, content_version,
    /// format)` for every track, ordered by id. No tags, no path columns — just the
    /// two track-level inputs that determine a rendered path. See SP2 Component 1.
    pub fn list_render_keys(&self) -> Result<Vec<(i64, i64, Format)>> {
        let mut stmt = self
            .conn
            .prepare("SELECT id, content_version, format FROM tracks ORDER BY id")?;
        let rows = stmt.query_map([], |r| {
            let fmt: String = r.get(2)?;
            let format = Format::parse(&fmt).ok_or_else(|| {
                rusqlite::Error::FromSqlConversionFailure(
                    usize::MAX,
                    rusqlite::types::Type::Text,
                    format!("unknown format {fmt}").into(),
                )
            })?;
            Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?, format))
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }
}

#[cfg(test)]
mod render_key_tests {
    use super::*;
    use crate::{Format, NewTrack, Tag};

    fn open_mem() -> Db {
        Db::open_in_memory().unwrap()
    }

    fn new_track(path: &str, fmt: Format) -> NewTrack {
        NewTrack {
            backing_path: path.to_string(),
            format: fmt,
            audio_offset: 0,
            audio_length: 1,
            backing_size: 1,
            backing_mtime: 0,
        }
    }

    #[test]
    fn list_render_keys_returns_id_version_format_sorted_by_id() {
        let db = open_mem();
        let a = db
            .upsert_track(&new_track("/a.flac", Format::Flac))
            .unwrap();
        let b = db.upsert_track(&new_track("/b.mp3", Format::Mp3)).unwrap();
        // Bump a's content_version via a tag write (trigger).
        db.replace_tags(a, &[Tag::new("TITLE", "x", 0)]).unwrap();

        let keys = db.list_render_keys().unwrap();
        assert_eq!(keys.len(), 2);
        assert_eq!(keys[0].0, a);
        assert_eq!(keys[1].0, b);
        assert!(keys[0].1 >= 1, "a content_version should have risen");
        assert_eq!(keys[1].1, 0, "b content_version untouched");
        assert_eq!(keys[0].2, Format::Flac);
        assert_eq!(keys[1].2, Format::Mp3);
    }
}
