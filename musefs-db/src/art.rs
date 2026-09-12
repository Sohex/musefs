use crate::error::{check_art_count, check_text_field};
use crate::limits::{MAX_ART_DESCRIPTION_LEN, MAX_ART_MIME_LEN};
use crate::models::{Art, ArtMeta, NewArt, TrackArt};
use crate::{Db, ReadWrite, Result};
use rusqlite::params;
use sha2::{Digest, Sha256};

pub(crate) fn sha256_hex(data: &[u8]) -> String {
    format!("{:x}", base16ct::HexDisplay(&Sha256::digest(data)))
}

impl<M> Db<M> {
    pub fn get_art(&self, id: i64) -> Result<Option<Art>> {
        crate::query_optional(
            &self.conn,
            "SELECT id, sha256, mime, width, height, byte_len, data FROM art WHERE id = ?1",
            params![id],
            |r| {
                Ok(Art {
                    id: r.get(0)?,
                    sha256: r.get(1)?,
                    mime: r.get(2)?,
                    width: r.get(3)?,
                    height: r.get(4)?,
                    byte_len: r.get(5)?,
                    data: r.get(6)?,
                })
            },
        )
    }

    /// Art row metadata without loading the image blob — used to build synthesis
    /// inputs at resolve time without materializing art in memory.
    pub fn get_art_meta(&self, id: i64) -> Result<Option<ArtMeta>> {
        crate::query_optional(
            &self.conn,
            "SELECT length(mime), length(CAST(mime AS BLOB)), mime, width, height, byte_len \
             FROM art WHERE id = ?1",
            params![id],
            |r| {
                check_text_field("art", "mime", r.get(0)?, r.get(1)?, MAX_ART_MIME_LEN)?;
                Ok(ArtMeta {
                    mime: r.get(2)?,
                    width: r.get(3)?,
                    height: r.get(4)?,
                    byte_len: r.get(5)?,
                })
            },
        )
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
            "SELECT length(description), length(CAST(description AS BLOB)),
                    art_id, picture_type, description, ordinal
             FROM track_art WHERE track_id = ?1 ORDER BY ordinal",
        )?;
        let mut rows = stmt.query(params![track_id])?;
        let mut out = Vec::new();
        while let Some(r) = rows.next()? {
            check_text_field(
                "track_art",
                "description",
                r.get(0)?,
                r.get(1)?,
                MAX_ART_DESCRIPTION_LEN,
            )?;
            out.push(TrackArt {
                art_id: r.get(2)?,
                picture_type: r.get(3)?,
                description: r.get(4)?,
                ordinal: r.get(5)?,
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
            "SELECT length(ta.description), length(CAST(ta.description AS BLOB)), \
             ta.art_id, ta.picture_type, ta.description, ta.ordinal, \
             length(a.mime), length(CAST(a.mime AS BLOB)), \
             a.mime, a.width, a.height, a.byte_len \
             FROM track_art ta LEFT JOIN art a ON a.id = ta.art_id \
             WHERE ta.track_id = ?1 ORDER BY ta.ordinal",
        )?;
        let mut rows = stmt.query(params![track_id])?;
        let mut out = Vec::new();
        while let Some(r) = rows.next()? {
            check_text_field(
                "track_art",
                "description",
                r.get(0)?,
                r.get(1)?,
                MAX_ART_DESCRIPTION_LEN,
            )?;
            let track_art = TrackArt {
                art_id: r.get(2)?,
                picture_type: r.get(3)?,
                description: r.get(4)?,
                ordinal: r.get(5)?,
            };
            // A NULL `length(a.mime)` means the LEFT JOIN found no `art` row
            // (orphaned link); `mime` is NOT NULL in the schema, so the length
            // column is a reliable presence sentinel — and checking it lets us
            // reject an over-cap mime before the string is ever materialized
            // (the allocation-free guarantee, spec N13).
            let meta = match r.get::<_, Option<i64>>(6)? {
                Some(mime_chars) => {
                    check_text_field("art", "mime", mime_chars, r.get(7)?, MAX_ART_MIME_LEN)?;
                    Some(ArtMeta {
                        mime: r.get(8)?,
                        width: r.get(9)?,
                        height: r.get(10)?,
                        byte_len: r.get(11)?,
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

/// Insert `a` (deduplicated by content sha256) and return its `art` id. Runs on
/// `conn` so `Db<ReadWrite>` and `BulkWriter` share one body.
pub(crate) fn upsert_art_in(conn: &rusqlite::Connection, a: &NewArt) -> Result<i64> {
    let sha = sha256_hex(&a.data);
    conn.execute(
        "INSERT INTO art (sha256, mime, width, height, byte_len, data)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6) ON CONFLICT(sha256) DO NOTHING",
        params![sha, a.mime, a.width, a.height, a.data.len() as u64, a.data],
    )?;
    Ok(
        conn.query_row("SELECT id FROM art WHERE sha256 = ?1", params![sha], |r| {
            r.get(0)
        })?,
    )
}

/// Replace a track's `track_art` links. Runs on `conn` so `Db<ReadWrite>` (own
/// transaction) and `BulkWriter` (caller-held transaction) share one body.
pub(crate) fn set_track_art_in(
    conn: &rusqlite::Connection,
    track_id: i64,
    items: &[TrackArt],
) -> Result<()> {
    conn.execute(
        "DELETE FROM track_art WHERE track_id = ?1",
        params![track_id],
    )?;
    let mut stmt = conn.prepare_cached(
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
    Ok(())
}

impl Db<ReadWrite> {
    pub fn upsert_art(&self, a: &NewArt) -> Result<i64> {
        upsert_art_in(&self.conn, a)
    }

    pub fn set_track_art(&self, track_id: i64, items: &[TrackArt]) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        set_track_art_in(&tx, track_id, items)?;
        tx.commit()?;
        Ok(())
    }

    /// Delete `art` rows no longer referenced by any `track_art`. Returns the
    /// number of rows removed.
    ///
    /// Uses `NOT EXISTS` rather than `NOT IN (subquery)`: SQLite's `NOT IN`
    /// evaluates to UNKNOWN for the whole row if any subquery value is NULL,
    /// which would silently delete nothing should a NULL `art_id` ever reach
    /// `track_art` (#507). `NOT EXISTS` is NULL-safe.
    pub fn gc_orphan_art(&self) -> Result<usize> {
        let removed = self.conn.execute(
            "DELETE FROM art WHERE NOT EXISTS \
             (SELECT 1 FROM track_art WHERE track_art.art_id = art.id)",
            [],
        )?;
        Ok(removed)
    }
}

#[cfg(test)]
mod guard_tests {
    use crate::error::DbError;
    use crate::limits::{MAX_ART_DESCRIPTION_LEN, MAX_ART_MIME_LEN};
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

    /// A value whose SQLite character length is 1 and whose byte length is
    /// `bytes`: SQLite stops counting characters at an embedded NUL, which is
    /// how #693 slips an unbounded payload past a character cap.
    fn nul_truncated(bytes: usize) -> String {
        let mut v = String::from("x\0");
        v.push_str(&"y".repeat(bytes - v.len()));
        v
    }

    fn mime_byte_ceiling() -> usize {
        usize::try_from(MAX_ART_MIME_LEN).unwrap() * 4
    }

    fn description_byte_ceiling() -> usize {
        usize::try_from(MAX_ART_DESCRIPTION_LEN).unwrap() * 4
    }

    /// Plant an `art` row with an arbitrary mime, returning its id. `art` rows
    /// are immutable under the V5 `art_reject_content_update` trigger, so the
    /// row goes in by INSERT rather than by mutating an existing one.
    fn insert_art_with_mime(db: &Db, mime: &str) -> i64 {
        db.conn
            .execute(
                "INSERT INTO art (sha256, mime, width, height, byte_len, data) \
                 VALUES (?1, ?2, NULL, NULL, 1, X'00')",
                rusqlite::params!["c".repeat(64), mime],
            )
            .unwrap();
        db.conn.last_insert_rowid()
    }

    #[test]
    fn get_art_meta_rejects_a_nul_truncated_mime() {
        let (db, _t, _art) = db_track_art();
        // No `ignore_check_constraints`: the schema CHECK counts characters
        // too, so this row is accepted on the honest write path. That is the
        // bug — only the byte projection can see it.
        let bad = insert_art_with_mime(&db, &nul_truncated(mime_byte_ceiling() + 1));
        let err = db.get_art_meta(bad).unwrap_err();
        assert!(
            matches!(
                err,
                DbError::FieldTooLarge {
                    table: "art",
                    field: "mime",
                    unit: "bytes",
                    ..
                }
            ),
            "{err:?}"
        );
    }

    /// The ceiling must not narrow the field: a mime of four-byte characters at
    /// the character cap sits exactly on the ceiling and still reads.
    #[test]
    fn get_art_meta_accepts_four_byte_characters_at_cap() {
        let (db, _t, _art) = db_track_art();
        let mime = "\u{1D11E}".repeat(usize::try_from(MAX_ART_MIME_LEN).unwrap());
        assert_eq!(mime.len(), mime_byte_ceiling());
        let id = insert_art_with_mime(&db, &mime);
        assert_eq!(db.get_art_meta(id).unwrap().unwrap().mime, mime);
    }

    #[test]
    fn get_track_art_rejects_a_nul_truncated_description() {
        let (db, track, art) = db_track_art();
        db.set_track_art(
            track,
            &[TrackArt {
                art_id: art,
                picture_type: 3,
                description: nul_truncated(description_byte_ceiling() + 1),
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
                    unit: "bytes",
                    ..
                }
            ),
            "{err:?}"
        );
    }

    /// The join reads both NUL-truncatable fields, so it carries both guards.
    #[test]
    fn get_track_art_with_meta_rejects_a_nul_truncated_description() {
        let (db, track, art) = db_track_art();
        db.set_track_art(
            track,
            &[TrackArt {
                art_id: art,
                picture_type: 3,
                description: nul_truncated(description_byte_ceiling() + 1),
                ordinal: 0,
            }],
        )
        .unwrap();
        let err = db.get_track_art_with_meta(track).unwrap_err();
        assert!(
            matches!(
                err,
                DbError::FieldTooLarge {
                    table: "track_art",
                    field: "description",
                    unit: "bytes",
                    ..
                }
            ),
            "{err:?}"
        );
    }

    #[test]
    fn get_track_art_with_meta_rejects_a_nul_truncated_mime() {
        let (db, track, _art) = db_track_art();
        let bad = insert_art_with_mime(&db, &nul_truncated(mime_byte_ceiling() + 1));
        db.set_track_art(
            track,
            &[TrackArt {
                art_id: bad,
                picture_type: 3,
                description: "ok".into(),
                ordinal: 0,
            }],
        )
        .unwrap();
        let err = db.get_track_art_with_meta(track).unwrap_err();
        assert!(
            matches!(
                err,
                DbError::FieldTooLarge {
                    table: "art",
                    field: "mime",
                    unit: "bytes",
                    ..
                }
            ),
            "{err:?}"
        );
    }

    /// An orphaned link still reads as `None` rather than tripping the mime
    /// guard: the NULL byte-length column must not be mistaken for a violation.
    #[test]
    fn get_track_art_with_meta_still_reports_an_orphaned_link() {
        let (db, track, art) = db_track_art();
        db.set_track_art(
            track,
            &[TrackArt {
                art_id: art,
                picture_type: 3,
                description: "ok".into(),
                ordinal: 0,
            }],
        )
        .unwrap();
        // The production Db sets foreign_keys=true, so the delete would
        // RESTRICT-fail; FK enforcement is per-connection, which is exactly how
        // an external writer leaves a link dangling in the first place.
        db.conn.pragma_update(None, "foreign_keys", false).unwrap();
        db.conn
            .execute("DELETE FROM art WHERE id = ?1", rusqlite::params![art])
            .unwrap();
        let got = db.get_track_art_with_meta(track).unwrap();
        assert_eq!(got.len(), 1);
        assert!(got[0].1.is_none(), "the orphaned link surfaces as None");
    }

    #[test]
    fn get_track_art_rejects_oversize_description() {
        let (db, track, art) = db_track_art();
        db.conn
            .execute_batch("PRAGMA ignore_check_constraints=ON")
            .unwrap();
        let desc = "d".repeat(usize::try_from(MAX_ART_DESCRIPTION_LEN).unwrap() + 1);
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

    #[test]
    fn sha256_hex_matches_known_digest() {
        // NIST sample vector: sha256("abc").
        assert_eq!(
            super::sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn gc_orphan_art_not_exists_is_null_safe() {
        // Regression for #507: the orphan-art GC must use NOT EXISTS, not
        // NOT IN (subquery). With a NULL in the subquery, SQLite's NOT IN
        // evaluates to UNKNOWN for every row and deletes nothing; NOT EXISTS
        // is unaffected. The live schema's NOT NULL on `track_art.art_id`
        // makes this unreachable today, so the NULL is reproduced on a relaxed
        // scratch schema to pin the pattern choice against regression.
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE art (id INTEGER PRIMARY KEY);
             CREATE TABLE track_art (art_id INTEGER);
             INSERT INTO art (id) VALUES (1), (2);
             INSERT INTO track_art (art_id) VALUES (1), (NULL);",
        )
        .unwrap();

        // The buggy NOT IN form deletes nothing because of the NULL.
        let not_in = conn
            .execute(
                "DELETE FROM art WHERE id NOT IN (SELECT art_id FROM track_art)",
                [],
            )
            .unwrap();
        assert_eq!(not_in, 0, "NOT IN deletes nothing when a NULL is present");

        // The shipped NOT EXISTS form removes the genuine orphan (id 2).
        let not_exists = conn
            .execute(
                "DELETE FROM art WHERE NOT EXISTS \
                 (SELECT 1 FROM track_art WHERE track_art.art_id = art.id)",
                [],
            )
            .unwrap();
        assert_eq!(
            not_exists, 1,
            "NOT EXISTS removes the orphan despite the NULL"
        );
    }
}
