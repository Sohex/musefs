use crate::error::{check_art_count, check_field_len};
use crate::limits::{MAX_ART_DESCRIPTION_LEN, MAX_ART_MIME_LEN};
use crate::models::{Art, ArtMeta, NewArt, TrackArt};
use crate::{Db, ReadWrite, Result};
use rusqlite::params;
use sha2::{Digest, Sha256};

// Hand-encoded: sha2 0.11's digest output (hybrid_array::Array) has no
// LowerHex impl, so `format!("{:x}", ..)` does not compile. Revisit on the
// next sha2 bump (RustCrypto/hybrid-array#201).
pub(crate) fn sha256_hex(data: &[u8]) -> String {
    let mut s = String::with_capacity(64);
    for b in Sha256::digest(data) {
        use std::fmt::Write;
        let _ = write!(s, "{b:02x}");
    }
    s
}

impl<M> Db<M> {
    pub fn get_art(&self, id: i64) -> Result<Option<Art>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, sha256, mime, width, height, byte_len, data FROM art WHERE id = ?1",
        )?;
        let mut rows = stmt.query(params![id])?;
        match rows.next()? {
            Some(r) => Ok(Some(Art {
                id: r.get(0)?,
                sha256: r.get(1)?,
                mime: r.get(2)?,
                width: r.get(3)?,
                height: r.get(4)?,
                byte_len: r.get(5)?,
                data: r.get(6)?,
            })),
            None => Ok(None),
        }
    }

    /// Art row metadata without loading the image blob — used to build synthesis
    /// inputs at resolve time without materializing art in memory.
    pub fn get_art_meta(&self, id: i64) -> Result<Option<ArtMeta>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT length(mime), mime, width, height, byte_len FROM art WHERE id = ?1",
        )?;
        let mut rows = stmt.query(params![id])?;
        match rows.next()? {
            Some(r) => {
                check_field_len("art", "mime", r.get(0)?, MAX_ART_MIME_LEN)?;
                Ok(Some(ArtMeta {
                    mime: r.get(1)?,
                    width: r.get(2)?,
                    height: r.get(3)?,
                    byte_len: r.get(4)?,
                }))
            }
            None => Ok(None),
        }
    }

    /// Stream art-blob bytes at `offset` directly into `buf` via SQLite incremental
    /// blob I/O — no intermediate allocation (#70). A short read means the row no
    /// longer matches the layout; `read_at_exact` surfaces that as an error rather
    /// than silently zero-filling.
    pub fn read_art_chunk_into(&self, art_id: i64, offset: u64, buf: &mut [u8]) -> Result<()> {
        let blob = self.conn.blob_open("main", "art", "data", art_id, true)?;
        blob.read_at_exact(buf, crate::convert::usize_from(offset))?;
        Ok(())
    }

    /// Allocating convenience form of `read_art_chunk_into` (non-hot-path callers).
    pub fn read_art_chunk(&self, art_id: i64, offset: u64, len: usize) -> Result<Vec<u8>> {
        let mut buf = vec![0u8; len];
        self.read_art_chunk_into(art_id, offset, &mut buf)?;
        Ok(buf)
    }

    pub fn get_track_art(&self, track_id: i64) -> Result<Vec<TrackArt>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT length(description), art_id, picture_type, description, ordinal
             FROM track_art WHERE track_id = ?1 ORDER BY ordinal",
        )?;
        let mut rows = stmt.query(params![track_id])?;
        let mut out = Vec::new();
        while let Some(r) = rows.next()? {
            check_field_len(
                "track_art",
                "description",
                r.get(0)?,
                MAX_ART_DESCRIPTION_LEN,
            )?;
            out.push(TrackArt {
                art_id: r.get(1)?,
                picture_type: r.get(2)?,
                description: r.get(3)?,
                ordinal: r.get(4)?,
            });
            check_art_count(track_id, out.len())?;
        }
        Ok(out)
    }

    /// A track's `track_art` links joined with their `art` row metadata (no
    /// image blob), in one query — collapses the former N+1 (`get_track_art`
    /// plus one `get_art_meta` per row) on the resolve hot path. The `art` side
    /// is `None` for an orphaned link: SQLite FK enforcement is per-connection,
    /// so an external writer can leave a `track_art` row dangling, and the
    /// caller surfaces that rather than silently dropping the art.
    pub fn get_track_art_with_meta(
        &self,
        track_id: i64,
    ) -> Result<Vec<(TrackArt, Option<ArtMeta>)>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT length(ta.description), ta.art_id, ta.picture_type, ta.description, ta.ordinal, \
             length(a.mime), a.mime, a.width, a.height, a.byte_len \
             FROM track_art ta LEFT JOIN art a ON a.id = ta.art_id \
             WHERE ta.track_id = ?1 ORDER BY ta.ordinal",
        )?;
        let mut rows = stmt.query(params![track_id])?;
        let mut out = Vec::new();
        while let Some(r) = rows.next()? {
            check_field_len(
                "track_art",
                "description",
                r.get(0)?,
                MAX_ART_DESCRIPTION_LEN,
            )?;
            let track_art = TrackArt {
                art_id: r.get(1)?,
                picture_type: r.get(2)?,
                description: r.get(3)?,
                ordinal: r.get(4)?,
            };
            // A NULL `length(a.mime)` means the LEFT JOIN found no `art` row
            // (orphaned link); `mime` is NOT NULL in the schema, so the length
            // column is a reliable presence sentinel — and checking it lets us
            // reject an over-cap mime before the string is ever materialized
            // (the allocation-free guarantee, spec N13).
            let meta = match r.get::<_, Option<i64>>(5)? {
                Some(mime_len) => {
                    check_field_len("art", "mime", mime_len, MAX_ART_MIME_LEN)?;
                    Some(ArtMeta {
                        mime: r.get(6)?,
                        width: r.get(7)?,
                        height: r.get(8)?,
                        byte_len: r.get(9)?,
                    })
                }
                None => None,
            };
            out.push((track_art, meta));
            check_art_count(track_id, out.len())?;
        }
        Ok(out)
    }
}

impl Db<ReadWrite> {
    pub fn upsert_art(&self, a: &NewArt) -> Result<i64> {
        let sha = sha256_hex(&a.data);
        self.conn.execute(
            "INSERT INTO art (sha256, mime, width, height, byte_len, data)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(sha256) DO NOTHING",
            params![sha, a.mime, a.width, a.height, a.data.len() as u64, a.data],
        )?;
        let id =
            self.conn
                .query_row("SELECT id FROM art WHERE sha256 = ?1", params![sha], |r| {
                    r.get(0)
                })?;
        Ok(id)
    }

    pub fn set_track_art(&self, track_id: i64, items: &[TrackArt]) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        tx.execute(
            "DELETE FROM track_art WHERE track_id = ?1",
            params![track_id],
        )?;
        {
            let mut stmt = tx.prepare(
                "INSERT INTO track_art (track_id, art_id, picture_type, description, ordinal)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
            )?;
            for it in items {
                stmt.execute(params![
                    track_id,
                    it.art_id,
                    it.picture_type,
                    it.description,
                    it.ordinal
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Delete `art` rows no longer referenced by any `track_art`. Returns the
    /// number of rows removed.
    pub fn gc_orphan_art(&self) -> Result<usize> {
        let removed = self.conn.execute(
            "DELETE FROM art WHERE id NOT IN (SELECT art_id FROM track_art)",
            [],
        )?;
        Ok(removed)
    }
}

#[cfg(test)]
mod guard_tests {
    use crate::error::DbError;
    use crate::models::{NewArt, TrackArt};
    use crate::{Db, Format, NewTrack};

    fn db_track_art() -> (Db, i64, i64) {
        let db = Db::open_in_memory().unwrap();
        let track = db
            .upsert_track(&NewTrack {
                backing_path: "/a.flac".into(),
                format: Format::Flac,
                audio_offset: 0,
                audio_length: 1,
                backing_size: 1,
                backing_mtime_ns: 0,
                backing_ctime_ns: 0,
            })
            .unwrap();
        let art = db
            .upsert_art(&NewArt {
                mime: "image/png".into(),
                width: None,
                height: None,
                data: vec![0u8],
            })
            .unwrap();
        (db, track, art)
    }

    #[test]
    fn get_art_meta_rejects_oversize_mime() {
        let (db, _t, _art) = db_track_art();
        db.conn
            .execute_batch("PRAGMA ignore_check_constraints=ON")
            .unwrap();
        // art rows are immutable under the V5 `art_reject_content_update`
        // trigger (which `ignore_check_constraints` does not disable), so plant
        // the oversize-mime row with a fresh INSERT — the trigger guards only
        // UPDATE — rather than mutating an existing row in place.
        let mime = "x".repeat(256);
        db.conn
            .execute(
                "INSERT INTO art (sha256, mime, width, height, byte_len, data) \
                 VALUES (?1, ?2, NULL, NULL, 1, X'00')",
                rusqlite::params!["b".repeat(64), mime],
            )
            .unwrap();
        let bad = db.conn.last_insert_rowid();
        let err = db.get_art_meta(bad).unwrap_err();
        assert!(
            matches!(
                err,
                DbError::FieldTooLarge {
                    table: "art",
                    field: "mime",
                    ..
                }
            ),
            "{err:?}"
        );
    }

    #[test]
    fn get_track_art_rejects_oversize_description() {
        let (db, track, art) = db_track_art();
        db.conn
            .execute_batch("PRAGMA ignore_check_constraints=ON")
            .unwrap();
        let desc = "d".repeat(1025);
        db.set_track_art(
            track,
            &[TrackArt {
                art_id: art,
                picture_type: 3,
                description: desc,
                ordinal: 0,
            }],
        )
        .unwrap();
        let err = db.get_track_art(track).unwrap_err();
        assert!(
            matches!(
                err,
                DbError::FieldTooLarge {
                    table: "track_art",
                    field: "description",
                    ..
                }
            ),
            "{err:?}"
        );
    }

    #[test]
    fn get_track_art_accepts_description_at_cap() {
        let (db, track, art) = db_track_art();
        let desc = "d".repeat(1024);
        db.set_track_art(
            track,
            &[TrackArt {
                art_id: art,
                picture_type: 3,
                description: desc,
                ordinal: 0,
            }],
        )
        .unwrap();
        assert_eq!(db.get_track_art(track).unwrap()[0].description.len(), 1024);
    }

    #[test]
    fn get_track_art_rejects_excess_rows() {
        let (db, track, art) = db_track_art();
        // 4097 track_art rows sharing one art_id -> TooManyArtRows. Raw INSERT
        // (not set_track_art) keeps the fixture to a single planted blob; the
        // PRIMARY KEY (track_id, ordinal) is satisfied by the distinct ordinals.
        let tx = db.conn.unchecked_transaction().unwrap();
        let mut stmt = tx
            .prepare(
                "INSERT INTO track_art (track_id, art_id, picture_type, description, ordinal) \
                 VALUES (?1, ?2, 3, '', ?3)",
            )
            .unwrap();
        for i in 0..4097 {
            stmt.execute(rusqlite::params![track, art, i]).unwrap();
        }
        drop(stmt);
        tx.commit().unwrap();
        let err = db.get_track_art(track).unwrap_err();
        assert!(matches!(err, DbError::TooManyArtRows { .. }), "{err:?}");
    }

    #[test]
    fn get_track_art_accepts_rows_at_cap() {
        let (db, track, art) = db_track_art();
        let tx = db.conn.unchecked_transaction().unwrap();
        let mut stmt = tx
            .prepare(
                "INSERT INTO track_art (track_id, art_id, picture_type, description, ordinal) \
                 VALUES (?1, ?2, 3, '', ?3)",
            )
            .unwrap();
        for i in 0..4096 {
            stmt.execute(rusqlite::params![track, art, i]).unwrap();
        }
        drop(stmt);
        tx.commit().unwrap();
        assert_eq!(db.get_track_art(track).unwrap().len(), 4096);
    }
}
