use crate::models::{Format, NewTrack, Track};
use crate::{Db, Result};
use rusqlite::{params, Row};

const TRACK_COLS: &str = "id, backing_path, format, audio_offset, audio_length, \
                          backing_size, backing_mtime, content_version, updated_at";

/// Parse a `format` column value, mapping an unknown name to the rusqlite
/// conversion error every row-mapper needs (single source — three readers).
fn parse_format_col(fmt: &str) -> rusqlite::Result<Format> {
    Format::parse(fmt).ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(
            usize::MAX,
            rusqlite::types::Type::Text,
            format!("unknown format {fmt}").into(),
        )
    })
}

fn row_to_track(r: &Row) -> rusqlite::Result<Track> {
    let fmt: String = r.get("format")?;
    let format = parse_format_col(&fmt)?;
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

/// One read of the changelog ring past `last_seq`: the distinct changed track
/// ids (ascending) plus the table's retained seq bounds (0/0 when empty). The
/// caller derives gap detection from `min_seq` (see musefs-core's refresh).
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ChangelogRead {
    pub changed_ids: Vec<i64>,
    pub min_seq: i64,
    pub max_seq: i64,
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

    /// Begin a deferred (read) transaction: subsequent reads on this connection see
    /// a single consistent snapshot until `end_read`. Used to make a binary-tag
    /// read's content_version check and its blob reads mutually consistent.
    pub fn begin_read(&self) -> Result<()> {
        self.conn.execute_batch("BEGIN DEFERRED")?;
        Ok(())
    }

    /// End the read transaction opened by `begin_read` (rollback — it is read-only).
    pub fn end_read(&self) -> Result<()> {
        self.conn.execute_batch("ROLLBACK")?;
        Ok(())
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

    /// Test-only: force a track's format column directly (no rescan), bumping
    /// data_version. The only way to exercise a format-only change — production
    /// never mutates format without a rescan. content_version is NOT bumped (no
    /// trigger fires on the tracks.format column), so this is a pure format-only edit.
    #[doc(hidden)]
    pub fn set_format_for_test(&self, id: i64, fmt: Format) -> Result<()> {
        self.conn.execute(
            "UPDATE tracks SET format = ?1, updated_at = CAST(strftime('%s','now') AS INTEGER) WHERE id = ?2",
            params![fmt.as_str(), id],
        )?;
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
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, i64>(1)?,
                parse_format_col(&fmt)?,
            ))
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// One read of the changelog ring past `last_seq`: the distinct changed track
    /// ids (ascending) plus the table's retained seq bounds (0/0 when empty). The
    /// caller derives gap detection from `min_seq` (see musefs-core's refresh).
    pub fn changelog_since(&self, last_seq: i64) -> Result<ChangelogRead> {
        // One deferred read transaction pins a single WAL snapshot for both
        // queries: under separate implicit snapshots a concurrent write burst
        // (with track_changes_prune trimming the old end) could pair fresh ids
        // with stale bounds — masking a prune gap while advancing the watermark.
        let tx = self.conn.unchecked_transaction()?;
        let (min_seq, max_seq): (i64, i64) = tx.query_row(
            "SELECT COALESCE(MIN(seq),0), COALESCE(MAX(seq),0) FROM track_changes",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        let changed_ids = {
            let mut stmt = tx.prepare(
                "SELECT DISTINCT track_id FROM track_changes WHERE seq > ?1 ORDER BY track_id",
            )?;
            let ids = stmt
                .query_map([last_seq], |r| r.get(0))?
                .collect::<rusqlite::Result<Vec<i64>>>()?;
            ids
        };
        tx.commit()?;
        Ok(ChangelogRead {
            changed_ids,
            min_seq,
            max_seq,
        })
    }

    /// Render keys for a specific id set (the changelog ids); ids no longer in
    /// `tracks` are simply absent from the result. Chunked like `tags_for_tracks`.
    pub fn render_keys_for(&self, ids: &[i64]) -> Result<Vec<(i64, i64, Format)>> {
        const CHUNK: usize = 900;
        let mut out = Vec::with_capacity(ids.len());
        for chunk in ids.chunks(CHUNK) {
            let placeholders = vec!["?"; chunk.len()].join(",");
            let sql = format!(
                "SELECT id, content_version, format FROM tracks \
                 WHERE id IN ({placeholders}) ORDER BY id"
            );
            let mut stmt = self.conn.prepare(&sql)?;
            let params = rusqlite::params_from_iter(chunk.iter());
            let rows = stmt.query_map(params, |r| {
                let fmt: String = r.get(2)?;
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, i64>(1)?,
                    parse_format_col(&fmt)?,
                ))
            })?;
            out.extend(rows.collect::<rusqlite::Result<Vec<_>>>()?);
        }
        Ok(out)
    }

    /// Test-only: delete changelog rows up to and including `seq`, simulating the
    /// ring having pruned past a sleeping mount (gap-path coverage). Follows the
    /// `set_format_for_test` precedent.
    #[doc(hidden)]
    pub fn delete_changelog_through_for_test(&self, seq: i64) -> Result<()> {
        self.conn
            .execute("DELETE FROM track_changes WHERE seq <= ?1", [seq])?;
        Ok(())
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

    #[test]
    fn set_format_for_test_persists_the_new_format() {
        let db = open_mem();
        let id = db
            .upsert_track(&new_track("/a.flac", Format::Flac))
            .unwrap();
        db.set_format_for_test(id, Format::Mp3).unwrap();
        let keys = db.list_render_keys().unwrap();
        assert_eq!(keys[0].0, id);
        assert_eq!(
            keys[0].2,
            Format::Mp3,
            "set_format_for_test must actually UPDATE the format column"
        );
    }

    /// `begin_read`/`end_read` bracket a single WAL read snapshot on a connection,
    /// so a write by another connection that bumps `content_version` (or reuses a
    /// freed binary-tag rowid) is invisible until the snapshot ends. The
    /// `read` fast path's BinaryTag guard depends on this consistency: it pins the
    /// version + the blob reads to one snapshot so a reused rowid can't be served.
    #[test]
    fn begin_read_pins_a_single_wal_snapshot_against_external_writes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("m.db");
        let writer = Db::open(&path).unwrap();
        let id = writer
            .upsert_track(&new_track("/a.mp3", Format::Mp3))
            .unwrap();
        assert_eq!(writer.track_content_version(id).unwrap(), 0);

        // The reader opens a second connection; the two share the WAL.
        let reader = Db::open(&path).unwrap();
        assert_eq!(reader.track_content_version(id).unwrap(), 0);

        reader.begin_read().unwrap();
        // Within the snapshot: the version is 0.
        assert_eq!(reader.track_content_version(id).unwrap(), 0);

        // An external write bumps the version. The reader's snapshot must NOT see it.
        writer
            .replace_tags(id, &[Tag::new("artist", "Alice", 0)])
            .unwrap();
        assert_eq!(
            reader.track_content_version(id).unwrap(),
            0,
            "snapshot must pin to the pre-write content_version"
        );
        // Latest version (visible without the snapshot) is bumped.
        assert_eq!(writer.track_content_version(id).unwrap(), 1);

        reader.end_read().unwrap();
        // After the snapshot ends, the reader sees the new version.
        assert_eq!(reader.track_content_version(id).unwrap(), 1);
    }
}
