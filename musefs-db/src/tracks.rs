use crate::models::{Format, NewTrack, Track};
use crate::{Db, Result};
use rusqlite::{params, Row};

const TRACK_COLS: &str = "id, backing_path, format, audio_offset, audio_length, \
                          backing_size, backing_mtime, content_version, updated_at";

fn row_to_track(r: &Row) -> rusqlite::Result<Track> {
    let fmt: String = r.get("format")?;
    let format = Format::parse(&fmt).ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(
            0,
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

    fn query_optional_track(
        &self,
        sql: &str,
        p: impl rusqlite::Params,
    ) -> Result<Option<Track>> {
        let mut stmt = self.conn.prepare(sql)?;
        let mut rows = stmt.query(p)?;
        match rows.next()? {
            Some(r) => Ok(Some(row_to_track(r)?)),
            None => Ok(None),
        }
    }
}
