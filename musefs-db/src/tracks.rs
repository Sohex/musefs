use crate::error::check_field_bytes;
use crate::limits::MAX_BACKING_PATH_BYTES;
use crate::models::{ChecksumWrite, Format, NewTrack, Track, TrackBounds};
use crate::{Db, ReadWrite, Result};
use rusqlite::{Row, params};

/// Build a `SELECT <track columns> FROM tracks <tail>` as a compile-time string
/// literal, so every track read shares one column list (kept in lockstep with
/// `row_to_track`) and can be served via `prepare_cached` — no per-call `format!`
/// allocation and no SQL recompilation on the `getattr`/`read` hot path.
/// The `tracks` projection every reader shares.
///
/// `backing_path` is a `BLOB` (#680) and the model is a `PathBuf`, so it is read
/// as the bytes it is — no cast either way. The cast this used to carry was
/// sound only while every stored path had come from a Rust `String`; a path is
/// an arbitrary byte string on Unix, and the whole point of the byte-typed
/// model is that such a path round-trips instead of being mangled.
///
/// `backing_path_len` rides beside it so `row_to_track` can refuse an over-cap
/// path before loading it (#758).
macro_rules! track_select {
    ($tail:literal) => {
        concat!(
            "SELECT id, length(backing_path) AS backing_path_len, backing_path, format, \
             audio_offset, audio_length, \
             backing_size, backing_mtime_ns, backing_ctime_ns, backing_ino, \
             content_version, updated_at, \
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

/// Refuse a `backing_path` over the cap from its projected `length()`, before the
/// value is read (#758). Every reader of the column materializes it — into a
/// `PathBuf`, on `getattr` among others — so without this a crafted store chose
/// the size of that allocation; the V4 `CHECK` cannot protect a store written
/// with its constraints off. The column is a BLOB, so `length()` counts bytes.
fn check_backing_path_len(len: i64) -> Result<()> {
    check_field_bytes("tracks", "backing_path", len, MAX_BACKING_PATH_BYTES)
}

/// Drain a `track_select!` result, guarding each row as `row_to_track` does.
fn collect_tracks(mut rows: rusqlite::Rows) -> Result<Vec<Track>> {
    let mut out = Vec::new();
    while let Some(r) = rows.next()? {
        out.push(row_to_track(r)?);
    }
    Ok(out)
}

fn row_to_track(r: &Row) -> Result<Track> {
    check_backing_path_len(r.get("backing_path_len")?)?;
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
        backing_path: crate::models::path_from_col(r.get("backing_path")?),
        format,
        bounds,
        backing_size,
        backing_mtime_ns: r.get("backing_mtime_ns")?,
        backing_ctime_ns: r.get("backing_ctime_ns")?,
        backing_ino: crate::models::ino_from_col(r.get("backing_ino")?),
        content_version: r.get("content_version")?,
        updated_at: r.get("updated_at")?,
        fingerprint: r.get("fingerprint")?,
        content_hash: r.get("content_hash")?,
    })
}

/// Upsert a track by `backing_path`, returning its id (via `RETURNING`, so the
/// insert and id-read are one statement). Runs on `conn` so `Db<ReadWrite>` and
/// `BulkWriter` share one body.
///
/// `updated_at` moves only when a column this writes differs from the stored
/// one (#757). A synthesized file's served second follows it, so stamping it on
/// every re-probe made a revalidate over unchanged files look like a change to
/// every size-plus-mtime consumer.
pub(crate) fn upsert_track_in(conn: &rusqlite::Connection, t: &NewTrack) -> Result<i64> {
    Ok(conn.query_row(
        "INSERT INTO tracks
            (backing_path, format, audio_offset, audio_length, backing_size, backing_mtime_ns, backing_ctime_ns, backing_ino, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, CAST(strftime('%s','now') AS INTEGER))
         ON CONFLICT(backing_path) DO UPDATE SET
            format=excluded.format, audio_offset=excluded.audio_offset,
            audio_length=excluded.audio_length, backing_size=excluded.backing_size,
            backing_mtime_ns=excluded.backing_mtime_ns,
            backing_ctime_ns=excluded.backing_ctime_ns,
            backing_ino=excluded.backing_ino,
            updated_at=CASE
                WHEN format <> excluded.format
                  OR audio_offset <> excluded.audio_offset
                  OR audio_length <> excluded.audio_length
                  OR backing_size <> excluded.backing_size
                  OR backing_mtime_ns <> excluded.backing_mtime_ns
                  OR backing_ctime_ns <> excluded.backing_ctime_ns
                  OR backing_ino <> excluded.backing_ino
                THEN CAST(strftime('%s','now') AS INTEGER)
                ELSE updated_at
            END
         RETURNING id",
        params![
            crate::models::path_to_col(&t.backing_path),
            t.format.as_str(),
            t.audio_offset,
            t.audio_length,
            t.backing_size,
            t.backing_mtime_ns,
            t.backing_ctime_ns,
            crate::models::ino_to_col(t.backing_ino),
        ],
        |r| r.get(0),
    )?)
}

pub(crate) fn get_track_by_path_in(
    conn: &rusqlite::Connection,
    path: &std::path::Path,
) -> Result<Option<Track>> {
    crate::query_optional(
        conn,
        track_select!("WHERE backing_path = ?1"),
        params![crate::models::path_to_col(path)],
        row_to_track,
    )
}

pub(crate) fn tracks_by_fingerprint_in(
    conn: &rusqlite::Connection,
    fp: &str,
) -> Result<Vec<Track>> {
    let mut stmt = conn.prepare_cached(track_select!("WHERE fingerprint = ?1 ORDER BY id"))?;
    collect_tracks(stmt.query(params![fp])?)
}

/// Both checksum writers below take each column as a `(overwrite?, value)`
/// pair, which is what makes all three [`ChecksumWrite`] intents expressible:
/// `Keep` leaves the column alone, `Set` and `Clear` write the value, and that
/// value is NULL for `Clear`. The `COALESCE(?, col)` these replaced could only
/// express two of them, and read `Clear` as `Keep` (#689).
pub(crate) fn set_track_checksums_in(
    conn: &rusqlite::Connection,
    id: i64,
    fingerprint: ChecksumWrite<'_>,
    content_hash: ChecksumWrite<'_>,
) -> Result<()> {
    let (fp_set, fp_val) = fingerprint.params();
    let (ch_set, ch_val) = content_hash.params();
    conn.execute(
        "UPDATE tracks SET
            fingerprint  = CASE WHEN ?2 THEN ?3 ELSE fingerprint  END,
            content_hash = CASE WHEN ?4 THEN ?5 ELSE content_hash END
         WHERE id = ?1",
        params![id, fp_set, fp_val, ch_set, ch_val],
    )?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn retarget_track_in(
    conn: &rusqlite::Connection,
    id: i64,
    new_backing_path: &std::path::Path,
    backing_size: u64,
    backing_mtime_ns: i64,
    backing_ctime_ns: i64,
    backing_ino: Option<u64>,
    audio_offset: u64,
    audio_length: u64,
    fingerprint: ChecksumWrite<'_>,
    content_hash: ChecksumWrite<'_>,
) -> Result<()> {
    let (fp_set, fp_val) = fingerprint.params();
    let (ch_set, ch_val) = content_hash.params();
    conn.execute(
        "UPDATE tracks SET
            backing_path     = ?2,
            backing_size     = ?3,
            backing_mtime_ns = ?4,
            backing_ctime_ns = ?5,
            backing_ino      = ?6,
            audio_offset     = ?7,
            audio_length     = ?8,
            fingerprint      = CASE WHEN ?9  THEN ?10 ELSE fingerprint  END,
            content_hash     = CASE WHEN ?11 THEN ?12 ELSE content_hash END,
            updated_at       = CAST(strftime('%s','now') AS INTEGER)
         WHERE id = ?1",
        params![
            id,
            crate::models::path_to_col(new_backing_path),
            backing_size,
            backing_mtime_ns,
            backing_ctime_ns,
            crate::models::ino_to_col(backing_ino),
            audio_offset,
            audio_length,
            fp_set,
            fp_val,
            ch_set,
            ch_val,
        ],
    )?;
    Ok(())
}

/// One read of the changelog ring past `last_seq`: the distinct changed track
/// ids (ascending) plus the table's retained seq bounds (0/0 when empty). The
/// caller derives gap detection from `min_seq` (see musefs-core's refresh).
#[derive(Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct ChangelogRead {
    pub changed_ids: Vec<i64>,
    pub min_seq: i64,
    pub max_seq: i64,
    /// A row past `last_seq` whose `track_id` is not an integer (#760). V4's
    /// `CHECK` refuses one, so only a store written with its constraints off
    /// holds it. It names no track the caller can act on, so it is left out of
    /// `changed_ids` rather than failing the read, and the caller treats the
    /// window as a gap: an error would advance no watermark, and every later
    /// read would meet the same row.
    pub malformed: bool,
}

impl<M> Db<M> {
    pub fn get_track(&self, id: i64) -> Result<Option<Track>> {
        self.query_optional_track(track_select!("WHERE id = ?1"), params![id])
    }

    pub fn get_track_by_path(&self, path: &std::path::Path) -> Result<Option<Track>> {
        get_track_by_path_in(&self.conn, path)
    }

    pub fn list_tracks(&self) -> Result<Vec<Track>> {
        let mut stmt = self.conn.prepare_cached(track_select!("ORDER BY id"))?;
        collect_tracks(stmt.query([])?)
    }

    /// Just the `backing_path` column for every track: the projection a scan's
    /// "already present" set needs, without materializing a `Track` (and its
    /// `fingerprint`/`content_hash` strings) per row — ~40 MB of transient
    /// allocation on a 200k-track store, on a path already holding a connection.
    /// Unordered by design; the caller collects into a set.
    pub fn list_backing_paths(&self) -> Result<Vec<std::path::PathBuf>> {
        let mut stmt = self
            .conn
            .prepare_cached("SELECT length(backing_path), backing_path FROM tracks")?;
        let mut rows = stmt.query([])?;
        let mut out = Vec::new();
        while let Some(r) = rows.next()? {
            check_backing_path_len(r.get(0)?)?;
            out.push(crate::models::path_from_col(r.get(1)?));
        }
        Ok(out)
    }

    /// How many tracks carry no `fingerprint`.
    ///
    /// This is the deficiency a migration that retires the column leaves
    /// behind, and the number `musefs migrate` reports so the user knows a
    /// rescan is owed. `revalidate` already re-probes a row missing the
    /// checksum its tier asks for, so it is also the number that goes back to
    /// zero when they run one.
    pub fn count_tracks_without_fingerprint(&self) -> Result<u64> {
        Ok(self.conn.query_row(
            "SELECT count(*) FROM tracks WHERE fingerprint IS NULL",
            [],
            |r| r.get(0),
        )?)
    }

    /// How many tracks no probe has visited since the store was upgraded: no
    /// `fingerprint` and no recorded inode. That is every row the 2.0.0
    /// migration leaves, and none a default-tier scan or revalidate writes.
    ///
    /// The lasting half of the count `musefs migrate` reports once: `mount`,
    /// `scan` and `revalidate` warn while it is non-zero (#705). A
    /// `--checksum none` scan on a filesystem whose inode numbers are not
    /// recorded — FAT and exFAT, SMB shares, FUSE mounts, overlayfs, anything
    /// not known to keep them — writes neither and lands a row here too. A
    /// default-tier `revalidate` clears it the same way: it re-probes a row with
    /// no fingerprint, and the re-probe writes one.
    pub fn count_tracks_awaiting_revalidate(&self) -> Result<u64> {
        Ok(self.conn.query_row(
            "SELECT count(*) FROM tracks WHERE fingerprint IS NULL AND backing_ino = 0",
            [],
            |r| r.get(0),
        )?)
    }

    pub fn track_content_version(&self, id: i64) -> Result<i64> {
        Ok(self.conn.query_row(
            "SELECT content_version FROM tracks WHERE id = ?1",
            params![id],
            |r| r.get(0),
        )?)
    }

    /// The identity columns `getattr` needs to validate cached attrs — the
    /// content stamp (`content_version`) plus the backing-source identity (the
    /// path to re-stat and the stamp recorded for it) — without materializing a
    /// full `Track` on the hottest metadata op. `None` if the id is unknown.
    pub fn track_identity(&self, id: i64) -> Result<Option<crate::TrackIdentity>> {
        crate::query_optional(
            &self.conn,
            "SELECT length(backing_path), content_version, backing_path, backing_size, \
             backing_mtime_ns, backing_ctime_ns, backing_ino FROM tracks WHERE id = ?1",
            params![id],
            |r| {
                check_backing_path_len(r.get(0)?)?;
                Ok(crate::TrackIdentity {
                    content_version: r.get(1)?,
                    backing_path: crate::models::path_from_col(r.get(2)?),
                    backing_size: r.get(3)?,
                    backing_mtime_ns: r.get(4)?,
                    backing_ctime_ns: r.get(5)?,
                    backing_ino: crate::models::ino_from_col(r.get(6)?),
                })
            },
        )
    }

    /// Begin a deferred (read) transaction: subsequent reads on this connection see
    /// a single consistent snapshot until `end_read`. Used to make a binary-tag
    /// read's content_version check and its blob reads mutually consistent.
    pub fn begin_read(&self) -> Result<()> {
        // Defense-in-depth: if a prior snapshot leaked — its `end_read` ROLLBACK
        // failed and the error was swallowed (the core callers do `let _ =
        // db.end_read()`), or a future caller forgot the pairing — the connection
        // is still mid-transaction and a raw BEGIN would fail with rusqlite's
        // opaque "cannot start a transaction within a transaction", pointing at
        // the symptom rather than the leak. Self-heal by rolling the stale
        // snapshot back first, with a diagnostic naming the actual cause (#549).
        if !self.conn.is_autocommit() {
            log::warn!(
                "begin_read found a leaked read transaction on this connection; \
                 rolling it back (a prior end_read likely failed to release the snapshot)"
            );
            self.conn.execute_batch("ROLLBACK")?;
        }
        self.conn.execute_batch("BEGIN DEFERRED")?;
        Ok(())
    }

    /// End the read transaction opened by `begin_read` (rollback — it is read-only).
    pub fn end_read(&self) -> Result<()> {
        self.conn.execute_batch("ROLLBACK")?;
        Ok(())
    }

    fn query_optional_track(&self, sql: &str, p: impl rusqlite::Params) -> Result<Option<Track>> {
        crate::query_optional(&self.conn, sql, p, row_to_track)
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
                "SELECT DISTINCT track_id FROM track_changes \
                 WHERE seq > ?1 AND typeof(track_id) = 'integer' ORDER BY track_id",
            )?;
            stmt.query_map([last_seq], |r| r.get(0))?
                .collect::<rusqlite::Result<Vec<i64>>>()?
        };
        let malformed: bool = tx.query_row(
            "SELECT EXISTS (SELECT 1 FROM track_changes \
             WHERE seq > ?1 AND typeof(track_id) <> 'integer')",
            [last_seq],
            |r| r.get(0),
        )?;
        tx.commit()?;
        Ok(ChangelogRead {
            changed_ids,
            min_seq,
            max_seq,
            malformed,
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

    /// Set the scanner-owned checksums for a track. Each column is written
    /// under its own [`ChecksumWrite`] intent: `Keep` leaves the stored value
    /// intact, so a lower-tier pass never clears a higher tier's value, while
    /// `Clear` nulls a value the pass knows no longer describes the file.
    pub fn set_track_checksums(
        &self,
        id: i64,
        fingerprint: ChecksumWrite<'_>,
        content_hash: ChecksumWrite<'_>,
    ) -> Result<()> {
        set_track_checksums_in(&self.conn, id, fingerprint, content_hash)
    }

    /// Point an existing track at a relocated backing file: update its path,
    /// validation stamp, and audio bounds in place, preserving its `id` (and
    /// thus its tags/art/structural blocks). Checksum args carry the same
    /// [`ChecksumWrite`] intent as `set_track_checksums`: a retarget that could
    /// not confirm the new file's full hash passes `Clear`, never `Keep`, so
    /// the row cannot keep the departed file's hash. `updated_at` is refreshed;
    /// `content_version` is left to the geometry trigger, which bumps only if
    /// something the served bytes depend on changed. A rename within a
    /// filesystem preserves both mtime and inode, so it does not bump; a move
    /// that was really a copy gets a fresh inode and does, which is the case
    /// #674 added the column for.
    #[allow(clippy::too_many_arguments)]
    pub fn retarget_track(
        &self,
        id: i64,
        new_backing_path: &std::path::Path,
        backing_size: u64,
        backing_mtime_ns: i64,
        backing_ctime_ns: i64,
        backing_ino: Option<u64>,
        audio_offset: u64,
        audio_length: u64,
        fingerprint: ChecksumWrite<'_>,
        content_hash: ChecksumWrite<'_>,
    ) -> Result<()> {
        retarget_track_in(
            &self.conn,
            id,
            new_backing_path,
            backing_size,
            backing_mtime_ns,
            backing_ctime_ns,
            backing_ino,
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
    #[cfg(any(test, feature = "test-support"))]
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
    #[cfg(any(test, feature = "test-support"))]
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
                backing_path: std::path::PathBuf::from("/x.flac"),
                format: Format::Flac,
                audio_offset: 0,
                audio_length: 1,
                backing_size: 1,
                backing_mtime_ns: 0,
                backing_ctime_ns: 0,
                backing_ino: None,
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
                backing_path: std::path::PathBuf::from("/x.flac"),
                format: Format::Flac,
                audio_offset: 0,
                audio_length: 1,
                backing_size: 1,
                backing_mtime_ns: 0,
                backing_ctime_ns: 0,
                backing_ino: None,
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
            backing_path: std::path::PathBuf::from(path),
            format: fmt,
            audio_offset: 0,
            audio_length: 1,
            backing_size: 1,
            backing_mtime_ns: 0,
            backing_ctime_ns: 0,
            backing_ino: None,
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

    /// The number `musefs migrate` reports after retiring the column, so it has
    /// to count the rows that are missing one and no others.
    #[test]
    fn count_tracks_without_fingerprint_counts_only_the_missing() {
        use crate::models::ChecksumWrite;
        let db = open_mem();
        let a = db
            .upsert_track(&new_track("/a.flac", Format::Flac))
            .unwrap();
        db.upsert_track(&new_track("/b.mp3", Format::Mp3)).unwrap();
        assert_eq!(db.count_tracks_without_fingerprint().unwrap(), 2);

        db.set_track_checksums(a, ChecksumWrite::Set(&"a".repeat(64)), ChecksumWrite::Keep)
            .unwrap();
        assert_eq!(db.count_tracks_without_fingerprint().unwrap(), 1);

        db.set_track_checksums(a, ChecksumWrite::Clear, ChecksumWrite::Keep)
            .unwrap();
        assert_eq!(db.count_tracks_without_fingerprint().unwrap(), 2);
    }

    /// A row is owed a revalidate only while it lacks both values a probe
    /// writes. Either one is proof a 2.0.0 probe has visited it.
    #[test]
    fn count_tracks_awaiting_revalidate_needs_both_values_missing() {
        use crate::models::ChecksumWrite;
        let db = open_mem();
        let a = db
            .upsert_track(&new_track("/a.flac", Format::Flac))
            .unwrap();
        db.upsert_track(&new_track("/b.mp3", Format::Mp3)).unwrap();
        assert_eq!(db.count_tracks_awaiting_revalidate().unwrap(), 2);

        db.set_track_checksums(a, ChecksumWrite::Set(&"a".repeat(64)), ChecksumWrite::Keep)
            .unwrap();
        assert_eq!(db.count_tracks_awaiting_revalidate().unwrap(), 1);

        let mut b = new_track("/b.mp3", Format::Mp3);
        b.backing_ino = Some(7);
        db.upsert_track(&b).unwrap();
        assert_eq!(db.count_tracks_awaiting_revalidate().unwrap(), 0);
    }

    #[test]
    fn list_backing_paths_returns_every_stored_path() {
        let db = open_mem();
        db.upsert_track(&new_track("/a.flac", Format::Flac))
            .unwrap();
        db.upsert_track(&new_track("/b.mp3", Format::Mp3)).unwrap();

        let mut paths = db.list_backing_paths().unwrap();
        paths.sort();
        assert_eq!(paths, vec!["/a.flac".to_string(), "/b.mp3".to_string()]);
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

    /// A leaked read snapshot — a `begin_read` whose `end_read` ROLLBACK never
    /// ran (the core callers swallow `end_read`'s error) — leaves the connection
    /// mid-transaction. The next `begin_read` must self-heal rather than fail
    /// with rusqlite's opaque "cannot start a transaction within a transaction"
    /// (#549).
    #[test]
    fn begin_read_self_heals_a_leaked_prior_snapshot() {
        let db = open_mem();
        db.begin_read().unwrap();
        // Leak it: no end_read.
        assert!(
            db.begin_read().is_ok(),
            "a leaked read snapshot must self-heal, not surface an opaque error"
        );
        db.end_read().unwrap();
    }
}

#[cfg(test)]
mod checksum_tests {
    use crate::{ChecksumWrite, Db, NewTrack, models::Format};
    use std::path::Path;

    fn new_track(path: &str) -> NewTrack {
        NewTrack {
            backing_path: std::path::PathBuf::from(path),
            format: Format::Flac,
            audio_offset: 0,
            audio_length: 10,
            backing_size: 10,
            backing_mtime_ns: 0,
            backing_ctime_ns: 0,
            backing_ino: None,
        }
    }

    #[test]
    fn set_and_read_back_checksums() {
        let db = Db::open_in_memory().unwrap();
        let id = db.upsert_track(&new_track("/a.flac")).unwrap();
        db.set_track_checksums(
            id,
            ChecksumWrite::Set(&"a".repeat(64)),
            ChecksumWrite::Set(&"d".repeat(64)),
        )
        .unwrap();
        let t = db.get_track(id).unwrap().unwrap();
        assert_eq!(t.fingerprint.as_deref(), Some(&"a".repeat(64)[..]));
        assert_eq!(t.content_hash.as_deref(), Some(&"d".repeat(64)[..]));
    }

    #[test]
    fn set_checksums_keep_does_not_clobber_existing() {
        let db = Db::open_in_memory().unwrap();
        let id = db.upsert_track(&new_track("/a.flac")).unwrap();
        db.set_track_checksums(
            id,
            ChecksumWrite::Set(&"a".repeat(64)),
            ChecksumWrite::Set(&"d".repeat(64)),
        )
        .unwrap();
        // A later pass with nothing new to say must preserve both.
        db.set_track_checksums(id, ChecksumWrite::Keep, ChecksumWrite::Keep)
            .unwrap();
        let t = db.get_track(id).unwrap().unwrap();
        assert_eq!(t.fingerprint.as_deref(), Some(&"a".repeat(64)[..]));
        assert_eq!(t.content_hash.as_deref(), Some(&"d".repeat(64)[..]));
    }

    /// The #689 distinction: `Clear` nulls a column `Keep` would have left
    /// standing, and does so per column.
    #[test]
    fn set_checksums_clear_nulls_only_the_cleared_column() {
        let db = Db::open_in_memory().unwrap();
        let id = db.upsert_track(&new_track("/a.flac")).unwrap();
        db.set_track_checksums(
            id,
            ChecksumWrite::Set(&"a".repeat(64)),
            ChecksumWrite::Set(&"d".repeat(64)),
        )
        .unwrap();
        db.set_track_checksums(id, ChecksumWrite::Keep, ChecksumWrite::Clear)
            .unwrap();
        let t = db.get_track(id).unwrap().unwrap();
        assert_eq!(t.fingerprint.as_deref(), Some(&"a".repeat(64)[..]));
        assert_eq!(t.content_hash, None, "Clear must null, not preserve");

        db.set_track_checksums(id, ChecksumWrite::Clear, ChecksumWrite::Keep)
            .unwrap();
        assert_eq!(db.get_track(id).unwrap().unwrap().fingerprint, None);
    }

    #[test]
    fn tracks_by_fingerprint_returns_matches() {
        let db = Db::open_in_memory().unwrap();
        let a = db.upsert_track(&new_track("/a.flac")).unwrap();
        let b = db.upsert_track(&new_track("/b.flac")).unwrap();
        db.set_track_checksums(a, ChecksumWrite::Set(&"b".repeat(64)), ChecksumWrite::Keep)
            .unwrap();
        db.set_track_checksums(b, ChecksumWrite::Set(&"b".repeat(64)), ChecksumWrite::Keep)
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
        db.set_track_checksums(id, ChecksumWrite::Set(&"a".repeat(64)), ChecksumWrite::Keep)
            .unwrap();
        db.retarget_track(
            id,
            Path::new("/new.flac"),
            99,
            1234,
            5678,
            Some(7),
            42,
            50,
            ChecksumWrite::Keep,
            ChecksumWrite::Set(&"e".repeat(64)),
        )
        .unwrap();
        let t = db.get_track(id).unwrap().unwrap();
        assert_eq!(t.id, id);
        assert_eq!(t.backing_path, Path::new("/new.flac"));
        assert_eq!(t.backing_size, 99);
        assert_eq!(t.backing_mtime_ns, 1234);
        assert_eq!(t.backing_ctime_ns, 5678);
        assert_eq!(t.backing_ino, Some(7));
        assert_eq!(t.bounds.audio_offset(), 42);
        assert_eq!(t.bounds.audio_length(), 50);
        assert_eq!(t.fingerprint.as_deref(), Some(&"a".repeat(64)[..])); // Keep preserves
        assert_eq!(t.content_hash.as_deref(), Some(&"e".repeat(64)[..]));
        assert!(
            db.get_track_by_path(Path::new("/old.flac"))
                .unwrap()
                .is_none()
        );
    }

    /// `tracks.backing_ino` is NOT NULL with a 0 sentinel and the model is
    /// `Option<u64>` (#674), so the translation has to survive a round trip in
    /// both directions — and the sentinel has to read back as "not recorded"
    /// rather than as inode zero.
    #[test]
    fn backing_ino_round_trips_through_the_sentinel() {
        let db = Db::open_in_memory().unwrap();

        let known = db
            .upsert_track(&NewTrack {
                backing_ino: Some(4242),
                ..new_track("/known.flac")
            })
            .unwrap();
        assert_eq!(
            db.get_track(known).unwrap().unwrap().backing_ino,
            Some(4242)
        );
        assert_eq!(
            db.track_identity(known).unwrap().unwrap().backing_ino,
            Some(4242),
            "the identity read `getattr` uses must carry it too"
        );

        let unknown = db.upsert_track(&new_track("/unknown.flac")).unwrap();
        assert_eq!(db.get_track(unknown).unwrap().unwrap().backing_ino, None);
        // Stored as the sentinel, not as NULL: the column is NOT NULL and the
        // invalidation trigger compares it with `<>`.
        let raw: i64 = db
            .conn
            .query_row(
                "SELECT backing_ino FROM tracks WHERE id = ?1",
                [unknown],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(raw, 0);
    }

    /// An inode above `i64::MAX` must reach the store. `st_ino` is a full
    /// `u64` and FUSE and network filesystems synthesize inode numbers freely,
    /// so this is a range real backing filesystems reach — and rusqlite's `u64`
    /// binding refuses it outright (`ToSqlConversionFailure(PosOverflow)`).
    /// Worse than losing the guard: a bind failure is not a constraint
    /// violation, so the scanner treats it as fatal and the whole run aborts.
    #[test]
    fn an_inode_past_i64_max_is_stored_and_read_back() {
        let db = Db::open_in_memory().unwrap();
        let huge = u64::try_from(i64::MAX).unwrap() + 1;
        let id = db
            .upsert_track(&NewTrack {
                backing_ino: Some(huge),
                ..new_track("/huge.flac")
            })
            .expect("an inode past i64::MAX must not fail the write");
        assert_eq!(db.get_track(id).unwrap().unwrap().backing_ino, Some(huge));
        assert_eq!(
            db.track_identity(id).unwrap().unwrap().backing_ino,
            Some(huge)
        );
        // Stored negative, which is what the dropped `>= 0` CHECK was in the
        // way of: the column holds the bit pattern, not the magnitude.
        let raw: i64 = db
            .conn
            .query_row("SELECT backing_ino FROM tracks WHERE id = ?1", [id], |r| {
                r.get(0)
            })
            .unwrap();
        assert!(raw < 0, "expected a negative bit pattern, got {raw}");

        // And `u64::MAX` — the value whose bit pattern is -1 — is still not the
        // sentinel, so the busiest edge case does not read back as unrecorded.
        let max = db
            .upsert_track(&NewTrack {
                backing_ino: Some(u64::MAX),
                ..new_track("/max.flac")
            })
            .unwrap();
        assert_eq!(
            db.get_track(max).unwrap().unwrap().backing_ino,
            Some(u64::MAX)
        );
    }

    /// The upsert half of the same round trip: a re-scan that now knows the
    /// inode must overwrite the sentinel rather than leave the row unrecorded.
    #[test]
    fn upsert_fills_in_an_inode_the_row_did_not_have() {
        let db = Db::open_in_memory().unwrap();
        let id = db.upsert_track(&new_track("/a.flac")).unwrap();
        assert_eq!(db.get_track(id).unwrap().unwrap().backing_ino, None);

        let again = db
            .upsert_track(&NewTrack {
                backing_ino: Some(77),
                ..new_track("/a.flac")
            })
            .unwrap();
        assert_eq!(again, id, "same path, same row");
        assert_eq!(db.get_track(id).unwrap().unwrap().backing_ino, Some(77));
    }

    /// A retarget that could not confirm the new file must not carry the
    /// departed file's `content_hash` forward (#689).
    #[test]
    fn retarget_clear_drops_the_previous_content_hash() {
        let db = Db::open_in_memory().unwrap();
        let id = db.upsert_track(&new_track("/old.flac")).unwrap();
        db.set_track_checksums(
            id,
            ChecksumWrite::Set(&"a".repeat(64)),
            ChecksumWrite::Set(&"d".repeat(64)),
        )
        .unwrap();
        db.retarget_track(
            id,
            Path::new("/new.flac"),
            10,
            1,
            2,
            None,
            0,
            10,
            ChecksumWrite::Set(&"b".repeat(64)),
            ChecksumWrite::Clear,
        )
        .unwrap();
        let t = db.get_track(id).unwrap().unwrap();
        assert_eq!(t.fingerprint.as_deref(), Some(&"b".repeat(64)[..]));
        assert_eq!(t.content_hash, None);
    }

    // Direct coverage of the BulkWriter read accessors used by ingest_unit's
    // retarget path: a mutated empty/None return makes these asserts fail. Kills
    // bulk.rs `tracks_by_fingerprint -> Ok(vec![])` and `get_track_by_path -> Ok(None)`.
    #[test]
    fn bulk_writer_reads_back_fingerprint_and_path() {
        let db = Db::open_in_memory().unwrap();
        let fp = "f".repeat(64);
        let mut bw = db.bulk_writer().unwrap();
        let id = bw.upsert_track(&new_track("/x.flac")).unwrap();
        bw.set_track_checksums(id, ChecksumWrite::Set(&fp), ChecksumWrite::Keep)
            .unwrap();

        let by_fp = bw.tracks_by_fingerprint(&fp).unwrap();
        assert_eq!(by_fp.len(), 1, "fingerprint match must be returned");
        assert_eq!(by_fp[0].id, id);

        let by_path = bw.get_track_by_path(Path::new("/x.flac")).unwrap();
        assert_eq!(by_path.map(|t| t.id), Some(id), "path lookup must hit");

        bw.commit().unwrap();
    }

    #[test]
    fn bulk_writer_retarget_and_checksums_match_db() {
        let db = Db::open_in_memory().unwrap();
        let id = {
            let mut bw = db.bulk_writer().unwrap();
            let id = bw.upsert_track(&new_track("/old.flac")).unwrap();
            bw.set_track_checksums(id, ChecksumWrite::Set(&"a".repeat(64)), ChecksumWrite::Keep)
                .unwrap();
            bw.retarget_track(
                id,
                Path::new("/new.flac"),
                10,
                1,
                2,
                Some(9),
                0,
                10,
                ChecksumWrite::Keep,
                ChecksumWrite::Keep,
            )
            .unwrap();
            bw.commit().unwrap();
            id
        };
        let t = db.get_track(id).unwrap().unwrap();
        assert_eq!(t.backing_path, Path::new("/new.flac"));
        assert_eq!(t.fingerprint.as_deref(), Some(&"a".repeat(64)[..]));
    }
}
