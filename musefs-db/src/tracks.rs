use crate::models::{Format, NewTrack, Track, TrackBounds};
use crate::{Db, ReadWrite, Result};
use rusqlite::{Row, params};

/// Build a `SELECT <track columns> FROM tracks <tail>` as a compile-time string
/// literal, so every track read shares one column list (kept in lockstep with
/// `row_to_track`) and can be served via `prepare_cached` — no per-call `format!`
/// allocation and no SQL recompilation on the `getattr`/`read` hot path.
macro_rules! track_select {
    ($tail:literal) => {
        concat!(
            "SELECT id, backing_path, format, audio_offset, audio_length, \
             backing_size, backing_mtime_ns, backing_ctime_ns, content_version, updated_at, \
             fingerprint, content_hash \
             FROM tracks ",
            $tail
        )
    };
}

/// Parse a `format` column value, mapping an unknown name to the rusqlite
/// conversion error every row-mapper needs (single source — three readers).
fn parse_format_col(fmt: &str) -> rusqlite::Result<Format> {
    fmt.parse::<Format>().ok().ok_or_else(|| {
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
    let audio_offset: u64 = r.get("audio_offset")?;
    let audio_length: u64 = r.get("audio_length")?;
    let backing_size: u64 = r.get("backing_size")?;
    let bounds = TrackBounds::new(audio_offset, audio_length, backing_size).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(
            usize::MAX,
            rusqlite::types::Type::Integer,
            e.to_string().into(),
        )
    })?;
    Ok(Track {
        id: r.get("id")?,
        backing_path: r.get("backing_path")?,
        format,
        bounds,
        backing_size,
        backing_mtime_ns: r.get("backing_mtime_ns")?,
        backing_ctime_ns: r.get("backing_ctime_ns")?,
        content_version: r.get("content_version")?,
        updated_at: r.get("updated_at")?,
        fingerprint: r.get("fingerprint")?,
        content_hash: r.get("content_hash")?,
    })
}

/// Upsert a track by `backing_path`, returning its id (via `RETURNING`, so the
/// insert and id-read are one statement). Runs on `conn` so `Db<ReadWrite>` and
/// `BulkWriter` share one body.
pub(crate) fn upsert_track_in(conn: &rusqlite::Connection, t: &NewTrack) -> Result<i64> {
    Ok(conn.query_row(
        "INSERT INTO tracks
            (backing_path, format, audio_offset, audio_length, backing_size, backing_mtime_ns, backing_ctime_ns, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, CAST(strftime('%s','now') AS INTEGER))
         ON CONFLICT(backing_path) DO UPDATE SET
            format=excluded.format, audio_offset=excluded.audio_offset,
            audio_length=excluded.audio_length, backing_size=excluded.backing_size,
            backing_mtime_ns=excluded.backing_mtime_ns,
            backing_ctime_ns=excluded.backing_ctime_ns,
            updated_at=CAST(strftime('%s','now') AS INTEGER)
         RETURNING id",
        params![
            t.backing_path,
            t.format.as_str(),
            t.audio_offset,
            t.audio_length,
            t.backing_size,
            t.backing_mtime_ns,
            t.backing_ctime_ns,
        ],
        |r| r.get(0),
    )?)
}

pub(crate) fn get_track_by_path_in(
    conn: &rusqlite::Connection,
    path: &str,
) -> Result<Option<Track>> {
    crate::query_optional(
        conn,
        track_select!("WHERE backing_path = ?1"),
        params![path],
        |r| Ok(row_to_track(r)?),
    )
}

pub(crate) fn tracks_by_fingerprint_in(
    conn: &rusqlite::Connection,
    fp: &str,
) -> Result<Vec<Track>> {
    let mut stmt = conn.prepare_cached(track_select!("WHERE fingerprint = ?1 ORDER BY id"))?;
    let rows = stmt.query_map(params![fp], row_to_track)?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

pub(crate) fn set_track_checksums_in(
    conn: &rusqlite::Connection,
    id: i64,
    fingerprint: Option<&str>,
    content_hash: Option<&str>,
) -> Result<()> {
    conn.execute(
        "UPDATE tracks SET
            fingerprint  = COALESCE(?2, fingerprint),
            content_hash = COALESCE(?3, content_hash)
         WHERE id = ?1",
        params![id, fingerprint, content_hash],
    )?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn retarget_track_in(
    conn: &rusqlite::Connection,
    id: i64,
    new_backing_path: &str,
    backing_size: u64,
    backing_mtime_ns: i64,
    backing_ctime_ns: i64,
    audio_offset: u64,
    audio_length: u64,
    fingerprint: Option<&str>,
    content_hash: Option<&str>,
) -> Result<()> {
    conn.execute(
        "UPDATE tracks SET
            backing_path     = ?2,
            backing_size     = ?3,
            backing_mtime_ns = ?4,
            backing_ctime_ns = ?5,
            audio_offset     = ?6,
            audio_length     = ?7,
            fingerprint      = COALESCE(?8, fingerprint),
            content_hash     = COALESCE(?9, content_hash),
            updated_at       = CAST(strftime('%s','now') AS INTEGER)
         WHERE id = ?1",
        params![
            id,
            new_backing_path,
            backing_size,
            backing_mtime_ns,
            backing_ctime_ns,
            audio_offset,
            audio_length,
            fingerprint,
            content_hash,
        ],
    )?;
    Ok(())
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

impl<M> Db<M> {
    pub fn get_track(&self, id: i64) -> Result<Option<Track>> {
        self.query_optional_track(track_select!("WHERE id = ?1"), params![id])
    }

    pub fn get_track_by_path(&self, path: &str) -> Result<Option<Track>> {
        get_track_by_path_in(&self.conn, path)
    }

    pub fn list_tracks(&self) -> Result<Vec<Track>> {
        let mut stmt = self.conn.prepare_cached(track_select!("ORDER BY id"))?;
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

    /// The two columns `getattr` needs to validate cached attrs — the freshness
    /// stamp (`content_version`) and the path to re-stat (`backing_path`) —
    /// without materializing a full `Track` (no `format` parse, no
    /// `TrackBounds`) on the hottest metadata op. `None` if the id is unknown.
    pub fn track_version_and_path(&self, id: i64) -> Result<Option<(i64, String)>> {
        crate::query_optional(
            &self.conn,
            "SELECT content_version, backing_path FROM tracks WHERE id = ?1",
            params![id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
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
        crate::query_optional(&self.conn, sql, p, |r| Ok(row_to_track(r)?))
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
            stmt.query_map([last_seq], |r| r.get(0))?
                .collect::<rusqlite::Result<Vec<i64>>>()?
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
        let mut out = Vec::with_capacity(ids.len());
        crate::query_in_chunks(
            &self.conn,
            ids,
            |ph| {
                format!(
                    "SELECT id, content_version, format FROM tracks \
                     WHERE id IN ({ph}) ORDER BY id"
                )
            },
            |rows| {
                while let Some(r) = rows.next()? {
                    let fmt: String = r.get(2)?;
                    out.push((
                        r.get::<_, i64>(0)?,
                        r.get::<_, i64>(1)?,
                        parse_format_col(&fmt)?,
                    ));
                }
                Ok(())
            },
        )?;
        Ok(out)
    }
}

impl Db<ReadWrite> {
    pub fn upsert_track(&self, t: &NewTrack) -> Result<i64> {
        upsert_track_in(&self.conn, t)
    }

    /// Delete a track row. Foreign keys cascade to its `tags` and `track_art`
    /// rows; the referenced `art` rows are left for `gc_orphan_art`.
    pub fn delete_track(&self, id: i64) -> Result<()> {
        self.conn
            .execute("DELETE FROM tracks WHERE id = ?1", params![id])?;
        Ok(())
    }

    /// All tracks whose stored fingerprint equals `fp` (rows with NULL
    /// fingerprint never match). Used by the scan refind to find move candidates.
    pub fn tracks_by_fingerprint(&self, fp: &str) -> Result<Vec<Track>> {
        tracks_by_fingerprint_in(&self.conn, fp)
    }

    /// Set the scanner-owned checksums for a track. A `None` argument leaves the
    /// existing column value intact (COALESCE), so a lower-tier pass never clears
    /// a higher tier's value.
    pub fn set_track_checksums(
        &self,
        id: i64,
        fingerprint: Option<&str>,
        content_hash: Option<&str>,
    ) -> Result<()> {
        set_track_checksums_in(&self.conn, id, fingerprint, content_hash)
    }

    /// Point an existing track at a relocated backing file: update its path,
    /// validation stamp, and audio bounds in place, preserving its `id` (and
    /// thus its tags/art/structural blocks). Checksum args COALESCE like
    /// `set_track_checksums`. `updated_at` is refreshed; `content_version` is
    /// left to the geometry trigger (it bumps only if `backing_mtime_ns`
    /// actually changed — a pure move preserves mtime, so no bump).
    #[allow(clippy::too_many_arguments)]
    pub fn retarget_track(
        &self,
        id: i64,
        new_backing_path: &str,
        backing_size: u64,
        backing_mtime_ns: i64,
        backing_ctime_ns: i64,
        audio_offset: u64,
        audio_length: u64,
        fingerprint: Option<&str>,
        content_hash: Option<&str>,
    ) -> Result<()> {
        retarget_track_in(
            &self.conn,
            id,
            new_backing_path,
            backing_size,
            backing_mtime_ns,
            backing_ctime_ns,
            audio_offset,
            audio_length,
            fingerprint,
            content_hash,
        )
    }

    /// Test-only: force a track's format column directly (no rescan), bumping
    /// data_version. The only way to exercise a format-only change — production
    /// never mutates format without a rescan. As of V5 this also bumps
    /// content_version (the `tracks_geometry_au` format guard); it is no longer a
    /// content_version-neutral edit.
    #[doc(hidden)]
    pub fn set_format_for_test(&self, id: i64, fmt: Format) -> Result<()> {
        self.conn.execute(
            "UPDATE tracks SET format = ?1, updated_at = CAST(strftime('%s','now') AS INTEGER) WHERE id = ?2",
            params![fmt.as_str(), id],
        )?;
        Ok(())
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
mod negative_audio_bounds_tests {
    use crate::{Db, Format, NewTrack};

    #[test]
    fn negative_audio_bounds_error_at_row_read() {
        let db = Db::open_in_memory().unwrap();
        let id = db
            .upsert_track(&NewTrack {
                backing_path: "/x.flac".into(),
                format: Format::Flac,
                audio_offset: 0,
                audio_length: 1,
                backing_size: 1,
                backing_mtime_ns: 0,
                backing_ctime_ns: 0,
            })
            .unwrap();
        // Simulate a malformed external write to a contract column. The V4
        // `audio_offset >= 0` CHECK would reject this on a normal connection, so
        // bypass CHECK enforcement to plant the bad row — the row-reader defensive
        // path (not the CHECK) is what this test pins.
        db.conn
            .pragma_update(None, "ignore_check_constraints", true)
            .unwrap();
        db.conn
            .execute("UPDATE tracks SET audio_offset = -1 WHERE id = ?1", [id])
            .unwrap();
        db.conn
            .pragma_update(None, "ignore_check_constraints", false)
            .unwrap();
        assert!(
            db.get_track(id).is_err(),
            "negative audio_offset must fail row-read, not wrap"
        );
    }

    #[test]
    fn out_of_range_bounds_error_at_row_read() {
        let db = Db::open_in_memory().unwrap();
        let id = db
            .upsert_track(&NewTrack {
                backing_path: "/x.flac".into(),
                format: Format::Flac,
                audio_offset: 0,
                audio_length: 1,
                backing_size: 1,
                backing_mtime_ns: 0,
                backing_ctime_ns: 0,
            })
            .unwrap();
        // Plant offset+length > backing_size past the V4 CHECK (layer 1) so we can
        // prove TrackBounds (layer 2) rejects it at row read.
        db.conn
            .pragma_update(None, "ignore_check_constraints", true)
            .unwrap();
        db.conn
            .execute("UPDATE tracks SET audio_length = 5 WHERE id = ?1", [id])
            .unwrap();
        db.conn
            .pragma_update(None, "ignore_check_constraints", false)
            .unwrap();
        assert!(
            db.get_track(id).is_err(),
            "audio_offset + audio_length > backing_size must fail row-read"
        );
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
            backing_mtime_ns: 0,
            backing_ctime_ns: 0,
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

#[cfg(test)]
mod checksum_tests {
    use crate::{Db, NewTrack, models::Format};

    fn new_track(path: &str) -> NewTrack {
        NewTrack {
            backing_path: path.to_string(),
            format: Format::Flac,
            audio_offset: 0,
            audio_length: 10,
            backing_size: 10,
            backing_mtime_ns: 0,
            backing_ctime_ns: 0,
        }
    }

    #[test]
    fn set_and_read_back_checksums() {
        let db = Db::open_in_memory().unwrap();
        let id = db.upsert_track(&new_track("/a.flac")).unwrap();
        db.set_track_checksums(id, Some(&"a".repeat(64)), Some(&"d".repeat(64)))
            .unwrap();
        let t = db.get_track(id).unwrap().unwrap();
        assert_eq!(t.fingerprint.as_deref(), Some(&"a".repeat(64)[..]));
        assert_eq!(t.content_hash.as_deref(), Some(&"d".repeat(64)[..]));
    }

    #[test]
    fn set_checksums_none_does_not_clobber_existing() {
        let db = Db::open_in_memory().unwrap();
        let id = db.upsert_track(&new_track("/a.flac")).unwrap();
        db.set_track_checksums(id, Some(&"a".repeat(64)), Some(&"d".repeat(64)))
            .unwrap();
        // A later lower-tier pass passes None and must preserve both.
        db.set_track_checksums(id, None, None).unwrap();
        let t = db.get_track(id).unwrap().unwrap();
        assert_eq!(t.fingerprint.as_deref(), Some(&"a".repeat(64)[..]));
        assert_eq!(t.content_hash.as_deref(), Some(&"d".repeat(64)[..]));
    }

    #[test]
    fn tracks_by_fingerprint_returns_matches() {
        let db = Db::open_in_memory().unwrap();
        let a = db.upsert_track(&new_track("/a.flac")).unwrap();
        let b = db.upsert_track(&new_track("/b.flac")).unwrap();
        db.set_track_checksums(a, Some(&"b".repeat(64)), None)
            .unwrap();
        db.set_track_checksums(b, Some(&"b".repeat(64)), None)
            .unwrap();
        db.upsert_track(&new_track("/c.flac")).unwrap(); // fingerprint NULL
        let mut ids: Vec<i64> = db
            .tracks_by_fingerprint(&"b".repeat(64))
            .unwrap()
            .into_iter()
            .map(|t| t.id)
            .collect();
        ids.sort_unstable();
        assert_eq!(ids, vec![a, b]);
        assert!(
            db.tracks_by_fingerprint(&"c".repeat(64))
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn retarget_updates_path_stamp_and_bounds_keeping_id() {
        let db = Db::open_in_memory().unwrap();
        let id = db.upsert_track(&new_track("/old.flac")).unwrap();
        db.set_track_checksums(id, Some(&"a".repeat(64)), None)
            .unwrap();
        db.retarget_track(
            id,
            "/new.flac",
            99,
            1234,
            5678,
            42,
            50,
            None,
            Some(&"e".repeat(64)),
        )
        .unwrap();
        let t = db.get_track(id).unwrap().unwrap();
        assert_eq!(t.id, id);
        assert_eq!(t.backing_path, "/new.flac");
        assert_eq!(t.backing_size, 99);
        assert_eq!(t.backing_mtime_ns, 1234);
        assert_eq!(t.backing_ctime_ns, 5678);
        assert_eq!(t.bounds.audio_offset(), 42);
        assert_eq!(t.bounds.audio_length(), 50);
        assert_eq!(t.fingerprint.as_deref(), Some(&"a".repeat(64)[..])); // None arg preserves
        assert_eq!(t.content_hash.as_deref(), Some(&"e".repeat(64)[..]));
        assert!(db.get_track_by_path("/old.flac").unwrap().is_none());
    }

    #[test]
    fn bulk_writer_retarget_and_checksums_match_db() {
        let db = Db::open_in_memory().unwrap();
        let id = {
            let mut bw = db.bulk_writer().unwrap();
            let id = bw.upsert_track(&new_track("/old.flac")).unwrap();
            bw.set_track_checksums(id, Some(&"a".repeat(64)), None)
                .unwrap();
            bw.retarget_track(id, "/new.flac", 10, 1, 2, 0, 10, None, None)
                .unwrap();
            bw.commit().unwrap();
            id
        };
        let t = db.get_track(id).unwrap().unwrap();
        assert_eq!(t.backing_path, "/new.flac");
        assert_eq!(t.fingerprint.as_deref(), Some(&"a".repeat(64)[..]));
    }
}
