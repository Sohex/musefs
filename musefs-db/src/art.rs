use crate::error::{check_art_count, check_field_bytes, check_text_field};
use crate::limits::{ART_SHA256_LEN, MAX_ART_BYTES, MAX_ART_DESCRIPTION_LEN, MAX_ART_MIME_LEN};
use crate::models::{Art, ArtMeta, EmbeddedArt, NewArt, TrackArt};
use crate::{Db, ReadWrite, Result};
use rusqlite::params;
use sha2::{Digest, Sha256};
use std::collections::HashMap;

pub(crate) fn sha256_hex(data: &[u8]) -> String {
    format!("{:x}", base16ct::HexDisplay(&Sha256::digest(data)))
}

impl<M> Db<M> {
    /// A whole `art` row, image blob included. Unlike [`Self::get_art_meta`] and
    /// the streaming readers, this one materializes every column, so both of
    /// its unbounded columns are length-guarded first: `sha256` is TEXT and
    /// carries the NUL-truncation problem (#693), and `data` is bounded only by
    /// two `CHECK`s a crafted store can have been written without.
    ///
    /// The picture metadata that used to live here moved to `track_art` (#716):
    /// it describes one file's embedding, not the bytes every file shares.
    pub fn get_art(&self, id: i64) -> Result<Option<Art>> {
        crate::query_optional(
            &self.conn,
            "SELECT length(sha256), length(CAST(sha256 AS BLOB)), length(data), \
             id, sha256, byte_len, data \
             FROM art WHERE id = ?1",
            params![id],
            |r| {
                check_text_field("art", "sha256", r.get(0)?, r.get(1)?, ART_SHA256_LEN)?;
                check_field_bytes("art", "data", r.get(2)?, MAX_ART_BYTES)?;
                Ok(Art {
                    id: r.get(3)?,
                    sha256: r.get(4)?,
                    byte_len: r.get(5)?,
                    data: r.get(6)?,
                })
            },
        )
    }

    /// The blob's length without loading it — all that is left of the `art` row
    /// once the picture metadata moved to the link that describes it (#716).
    /// Synthesis needs the length to size the segment; everything else it needs
    /// now comes from `track_art`.
    pub fn get_art_meta(&self, id: i64) -> Result<Option<ArtMeta>> {
        crate::query_optional(
            &self.conn,
            "SELECT byte_len FROM art WHERE id = ?1",
            params![id],
            |r| {
                Ok(ArtMeta {
                    byte_len: r.get(0)?,
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
                    length(mime), length(CAST(mime AS BLOB)),
                    art_id, picture_type, description, mime, width, height,
                    depth, colors, ordinal
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
            // The mime guard follows the column (#693/#716): the allocation-free
            // check has to sit wherever the string is about to be materialized.
            check_text_field("track_art", "mime", r.get(2)?, r.get(3)?, MAX_ART_MIME_LEN)?;
            out.push(TrackArt {
                art_id: r.get(4)?,
                picture_type: r.get(5)?,
                description: r.get(6)?,
                mime: r.get(7)?,
                width: r.get(8)?,
                height: r.get(9)?,
                depth: r.get(10)?,
                colors: r.get(11)?,
                ordinal: r.get(12)?,
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
             length(ta.mime), length(CAST(ta.mime AS BLOB)), \
             ta.art_id, ta.picture_type, ta.description, ta.mime, \
             ta.width, ta.height, ta.depth, ta.colors, ta.ordinal, \
             a.byte_len \
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
            check_text_field("track_art", "mime", r.get(2)?, r.get(3)?, MAX_ART_MIME_LEN)?;
            let track_art = TrackArt {
                art_id: r.get(4)?,
                picture_type: r.get(5)?,
                description: r.get(6)?,
                mime: r.get(7)?,
                width: r.get(8)?,
                height: r.get(9)?,
                depth: r.get(10)?,
                colors: r.get(11)?,
                ordinal: r.get(12)?,
            };
            // A NULL `a.byte_len` means the LEFT JOIN found no `art` row
            // (orphaned link); the column is NOT NULL in the schema, so its
            // absence is a reliable presence sentinel. It used to be the mime
            // that played that part, which stopped working when the mime moved
            // to the link — the link is always there, so it can never be NULL.
            let meta = r
                .get::<_, Option<u64>>(13)?
                .map(|byte_len| ArtMeta { byte_len });
            out.push((track_art, meta));
            check_art_count(track_id, out.len())?;
        }
        Ok(out)
    }
}

/// Insert `a` (deduplicated by content sha256) and return its `art` id. Runs on
/// `conn` so `Db<ReadWrite>` and `BulkWriter` share one body.
///
/// A conflict hands back the row already filed under the digest, and a store is
/// only content-addressed if that row really holds these bytes. Nothing in the
/// schema ties `sha256` to `data`, so a crafted row can claim a digest it does
/// not match (#724). A conflicting row is therefore compared with the incoming
/// bytes — in SQL, so nothing is re-hashed — and a mismatch is an error rather
/// than a link. A fresh insert needs no comparison: it just stored these bytes.
///
/// `verified` records rows already compared, so a writer that meets one cover
/// on every track of an album pays one blob comparison for it, not one per
/// track. Only compared rows are recorded; a rolled-back insert cannot leave an
/// id in it that some other row later reuses.
pub(crate) fn upsert_art_in(
    conn: &rusqlite::Connection,
    a: &NewArt,
    verified: &mut std::collections::HashSet<i64>,
) -> Result<i64> {
    let sha = sha256_hex(&a.data);
    let inserted = conn.execute(
        "INSERT INTO art (sha256, byte_len, data)
         VALUES (?1, ?2, ?3) ON CONFLICT(sha256) DO NOTHING",
        params![sha, a.data.len() as u64, a.data],
    )?;
    let id: i64 = conn.query_row("SELECT id FROM art WHERE sha256 = ?1", params![sha], |r| {
        r.get(0)
    })?;
    if inserted == 0 && !verified.contains(&id) {
        let holds_these_bytes: bool = conn.query_row(
            "SELECT data = ?2 FROM art WHERE id = ?1",
            params![id, a.data],
            |r| r.get(0),
        )?;
        if !holds_these_bytes {
            return Err(crate::error::DbError::ArtDigestMismatch {
                art_id: id,
                sha256: sha,
            });
        }
        verified.insert(id);
    }
    Ok(id)
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
        "INSERT INTO track_art (track_id, art_id, picture_type, description,
                                mime, width, height, depth, colors, ordinal)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
    )?;
    for it in items {
        stmt.execute(params![
            track_id,
            it.art_id,
            it.picture_type,
            it.description,
            it.mime,
            it.width,
            it.height,
            it.depth,
            it.colors,
            it.ordinal
        ])?;
    }
    Ok(())
}

/// Restore the per-embedding metadata of the links `track_id`'s backing file
/// supplied itself, from `pictures` as the file embeds them now (#746). Returns
/// how many links changed.
///
/// A link is the file's when its art row is filed under the picture's digest
/// and it carries the picture's type and description. A link an external writer
/// made to other bytes, or re-described, matches nothing and is left as it is —
/// which is what lets a pass that must not touch curated art call this at all.
/// Only the columns describing the embedding are written: mime, dimensions,
/// depth and colours.
///
/// A file can embed the same bytes twice under one type and description with
/// different metadata, so the key alone does not pick a link. Matches pair up
/// in order instead: the file's n-th picture under a key with the n-th link
/// under it by ordinal, the order ingest wrote them in.
///
/// A link already holding these values is not rewritten, so an unchanged file
/// does not bump `content_version` and invalidate what readers hold.
pub(crate) fn refresh_embedded_art_in(
    conn: &rusqlite::Connection,
    track_id: i64,
    pictures: &[EmbeddedArt],
) -> Result<usize> {
    let mut links = conn.prepare_cached(
        "SELECT ta.ordinal FROM track_art ta JOIN art a ON a.id = ta.art_id
         WHERE ta.track_id = ?1 AND a.sha256 = ?2
           AND ta.picture_type = ?3 AND ta.description = ?4
         ORDER BY ta.ordinal",
    )?;
    let mut restore = conn.prepare_cached(
        "UPDATE track_art SET mime = ?3, width = ?4, height = ?5, depth = ?6, colors = ?7
         WHERE track_id = ?1 AND ordinal = ?2
           AND (mime IS NOT ?3 OR width IS NOT ?4 OR height IS NOT ?5
                OR depth IS NOT ?6 OR colors IS NOT ?7)",
    )?;
    let mut paired: HashMap<(String, u32, &str), usize> = HashMap::new();
    let mut changed = 0;
    for pic in pictures {
        let sha = sha256_hex(&pic.data);
        let ordinals = links
            .query_map(
                params![track_id, sha, pic.picture_type, pic.description],
                |r| r.get::<_, i64>(0),
            )?
            .collect::<rusqlite::Result<Vec<i64>>>()?;
        let nth = paired
            .entry((sha, pic.picture_type, pic.description.as_str()))
            .or_insert(0);
        if let Some(&ordinal) = ordinals.get(*nth) {
            changed += restore.execute(params![
                track_id, ordinal, pic.mime, pic.width, pic.height, pic.depth, pic.colors
            ])?;
        }
        *nth += 1;
    }
    Ok(changed)
}

impl Db<ReadWrite> {
    /// Insert `a`, deduplicated by content, and return its `art` id. A row filed
    /// under the same digest is verified to hold these bytes before it is
    /// returned (#724); see [`upsert_art_in`]. Each call verifies afresh — a
    /// bulk scan remembers what it verified through [`crate::BulkWriter`].
    pub fn upsert_art(&self, a: &NewArt) -> Result<i64> {
        upsert_art_in(&self.conn, a, &mut std::collections::HashSet::new())
    }

    pub fn set_track_art(&self, track_id: i64, items: &[TrackArt]) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        set_track_art_in(&tx, track_id, items)?;
        tx.commit()?;
        Ok(())
    }

    /// Restore what `track_id`'s backing file declares about its own embedded
    /// pictures onto the links it supplied, leaving every other link alone, and
    /// return how many changed (#746).
    pub fn refresh_embedded_art(&self, track_id: i64, pictures: &[EmbeddedArt]) -> Result<usize> {
        let tx = self.conn.unchecked_transaction()?;
        let changed = refresh_embedded_art_in(&tx, track_id, pictures)?;
        tx.commit()?;
        Ok(changed)
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
    use crate::limits::{ART_SHA256_LEN, MAX_ART_BYTES, MAX_ART_DESCRIPTION_LEN, MAX_ART_MIME_LEN};
    use crate::models::{EmbeddedArt, NewArt, TrackArt};
    use crate::{Db, Format, NewTrack};

    fn db_track_art() -> (Db, i64, i64) {
        let db = Db::open_in_memory().unwrap();
        let track = db
            .upsert_track(&NewTrack {
                backing_path: std::path::PathBuf::from("/a.flac"),
                format: Format::Flac,
                audio_offset: 0,
                audio_length: 1,
                backing_size: 1,
                backing_mtime_ns: 0,
                backing_ctime_ns: 0,
                backing_ino: None,
            })
            .unwrap();
        let art = db.upsert_art(&NewArt { data: vec![0u8] }).unwrap();
        (db, track, art)
    }

    fn file_art(data: &[u8], mime: &str, side: u32, depth: u32, colors: u32) -> EmbeddedArt {
        EmbeddedArt {
            data: data.to_vec(),
            picture_type: 3,
            description: String::new(),
            mime: mime.to_string(),
            width: Some(side),
            height: Some(side),
            depth,
            colors,
        }
    }

    /// #746: a link takes the file's metadata back only where it is the file's
    /// own — its bytes, type and description; identical keys pair up by
    /// ordinal; and a link already right is not rewritten, so the track's
    /// version holds.
    #[test]
    fn refreshing_embedded_art_restores_only_the_files_own_links() {
        let (db, track, _) = db_track_art();
        let cover = db
            .upsert_art(&NewArt {
                data: b"COVER".to_vec(),
            })
            .unwrap();
        let other = db
            .upsert_art(&NewArt {
                data: b"OTHER".to_vec(),
            })
            .unwrap();
        let link = |art_id, description: &str, ordinal| TrackArt {
            art_id,
            picture_type: 3,
            description: description.to_string(),
            mime: "image/gif".to_string(),
            width: Some(1),
            height: Some(1),
            depth: 0,
            colors: 0,
            ordinal,
        };
        db.set_track_art(
            track,
            &[
                link(cover, "", 0),
                link(cover, "", 1),
                link(other, "", 2),
                link(cover, "re-described", 3),
            ],
        )
        .unwrap();
        // The same bytes twice, described two ways.
        let file = [
            file_art(b"COVER", "image/jpeg", 1200, 24, 0),
            file_art(b"COVER", "image/png", 64, 8, 256),
        ];

        assert_eq!(db.refresh_embedded_art(track, &file).unwrap(), 2);
        let described = |ordinal: usize| {
            let l = &db.get_track_art(track).unwrap()[ordinal];
            (l.mime.clone(), l.width, l.depth, l.colors)
        };
        assert_eq!(described(0), ("image/jpeg".to_string(), Some(1200), 24, 0));
        assert_eq!(described(1), ("image/png".to_string(), Some(64), 8, 256));
        let untouched = ("image/gif".to_string(), Some(1), 0, 0);
        assert_eq!(described(2), untouched, "another image's link");
        assert_eq!(described(3), untouched, "a re-described link");

        let version = db.track_content_version(track).unwrap();
        assert_eq!(db.refresh_embedded_art(track, &file).unwrap(), 0);
        assert_eq!(
            db.track_content_version(track).unwrap(),
            version,
            "nothing rewritten, nothing invalidated"
        );

        let mut bulk = db.bulk_writer().unwrap();
        let changed = bulk
            .refresh_embedded_art(track, &[file_art(b"COVER", "image/webp", 1200, 24, 0)])
            .unwrap();
        assert_eq!(changed, 1, "the bulk writer runs the same body");
    }

    /// #724: a row filed under the digest of bytes it does not hold is refused
    /// rather than linked — through `Db` and through a bulk writer, on every
    /// attempt, since a refused row is never recorded as verified — while an
    /// honest duplicate still dedups to the row it matches.
    #[test]
    fn a_row_whose_digest_names_other_bytes_is_refused_not_linked() {
        let (db, _track, honest) = db_track_art();
        let real = b"REAL-IMAGE-X".to_vec();
        let planted_sha = crate::art::sha256_hex(&real);
        db.conn
            .execute(
                "INSERT INTO art (sha256, byte_len, data) VALUES (?1, 3, X'595959')",
                rusqlite::params![planted_sha],
            )
            .unwrap();
        let planted = db.conn.last_insert_rowid();

        let err = db.upsert_art(&NewArt { data: real.clone() }).unwrap_err();
        assert!(
            matches!(err, DbError::ArtDigestMismatch { art_id, .. } if art_id == planted),
            "{err:?}"
        );

        let mut bulk = db.bulk_writer().unwrap();
        for _ in 0..2 {
            let err = bulk.upsert_art(&NewArt { data: real.clone() }).unwrap_err();
            assert!(matches!(err, DbError::ArtDigestMismatch { .. }), "{err:?}");
        }
        let again = bulk.upsert_art(&NewArt { data: vec![0u8] }).unwrap();
        assert_eq!(again, honest, "an honest duplicate still dedups");
        let again = bulk.upsert_art(&NewArt { data: vec![0u8] }).unwrap();
        assert_eq!(again, honest, "and a verified one dedups again");
    }

    /// #724: a bulk writer compares a row once and trusts it for the rest of the
    /// batch — the point of `verified`, so a cover shared by every track of an
    /// album costs one blob comparison rather than one per track. Pinned by
    /// changing the row's bytes behind the writer after it compared them: the
    /// next dedup returns the row without looking again.
    #[test]
    fn a_bulk_writer_compares_a_shared_row_once() {
        let (db, _track, _) = db_track_art();
        let cover = b"ALBUM-COVER".to_vec();
        let id = db
            .upsert_art(&NewArt {
                data: cover.clone(),
            })
            .unwrap();

        let mut bulk = db.bulk_writer().unwrap();
        let first = bulk
            .upsert_art(&NewArt {
                data: cover.clone(),
            })
            .unwrap();
        assert_eq!(first, id, "compared, and it holds these bytes");
        // `art` rows are immutable, so the substitution is a delete and a
        // re-insert under the same id and digest — what a writer ignoring the
        // contract could leave. Same length, so only a comparison could notice.
        db.conn
            .execute("DELETE FROM art WHERE id = ?1", rusqlite::params![id])
            .unwrap();
        db.conn
            .execute(
                "INSERT INTO art (id, sha256, byte_len, data) VALUES (?1, ?2, ?3, ?4)",
                rusqlite::params![
                    id,
                    crate::art::sha256_hex(&cover),
                    cover.len() as u64,
                    vec![0u8; cover.len()]
                ],
            )
            .unwrap();
        let again = bulk.upsert_art(&NewArt { data: cover }).unwrap();
        assert_eq!(
            again, id,
            "already verified by this writer, so not read again"
        );
    }

    /// #724: only a row this writer compared is remembered as verified. A fresh
    /// insert needs no comparison, and must not be recorded as though it had
    /// one: inside a bulk write its item can roll back, SQLite hands the freed
    /// id to the next row inserted, and that row would then be linked unchecked.
    #[test]
    fn a_rolled_back_insert_leaves_nothing_verified_for_its_id_to_reuse() {
        struct RolledBack;
        impl From<DbError> for RolledBack {
            fn from(_: DbError) -> RolledBack {
                RolledBack
            }
        }

        let (db, _track, _) = db_track_art();
        let real = b"REAL-IMAGE-Z".to_vec();
        let mut bulk = db.bulk_writer().unwrap();
        let mut freed = 0;
        let rolled_back = bulk.item(|w| -> Result<(), RolledBack> {
            freed = w.upsert_art(&NewArt { data: real.clone() })?;
            Err(RolledBack)
        });
        assert!(rolled_back.is_err());

        // Another image's bytes, filed under this one's digest, at the freed id.
        db.conn
            .execute(
                "INSERT INTO art (sha256, byte_len, data) VALUES (?1, 3, X'595959')",
                rusqlite::params![crate::art::sha256_hex(&real)],
            )
            .unwrap();
        assert_eq!(
            db.conn.last_insert_rowid(),
            freed,
            "precondition: the planted row reuses the rolled-back id"
        );

        let err = bulk.upsert_art(&NewArt { data: real }).unwrap_err();
        assert!(
            matches!(err, DbError::ArtDigestMismatch { art_id, .. } if art_id == freed),
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
    /// The digest column is the semantic variant #693 names: `length(sha256) =
    /// 64` is satisfied by 64 valid hex characters, a NUL, and any amount of
    /// suffix — so the constraint does not in fact pin a 64-character stored
    /// identity, and the reader must not allocate the suffix.
    #[test]
    fn get_art_rejects_a_nul_truncated_sha256() {
        let (db, _t, _art) = db_track_art();
        let mut sha = "a".repeat(usize::try_from(ART_SHA256_LEN).unwrap());
        sha.push('\0');
        sha.push_str(&"b".repeat(usize::try_from(ART_SHA256_LEN).unwrap() * 4));
        // Planted with the constraints off, like the mime case above: the
        // schema bans the NUL now, and the read guard is what protects a store
        // that already holds such a row.
        db.conn
            .pragma_update(None, "ignore_check_constraints", true)
            .unwrap();
        db.conn
            .execute(
                "INSERT INTO art (sha256, byte_len, data) \
                 VALUES (?1, 1, X'00')",
                rusqlite::params![sha],
            )
            .unwrap();
        db.conn
            .pragma_update(None, "ignore_check_constraints", false)
            .unwrap();
        let bad = db.conn.last_insert_rowid();
        let err = db.get_art(bad).unwrap_err();
        assert!(
            matches!(
                err,
                DbError::FieldTooLarge {
                    table: "art",
                    field: "sha256",
                    unit: "bytes",
                    ..
                }
            ),
            "{err:?}"
        );
    }

    /// The blob is the largest allocation in the row and is bounded only by two
    /// `CHECK`s — `byte_len <= MAX_ART_BYTES` and `byte_len = length(data)` —
    /// which a crafted store can have been written without. Here both are
    /// bypassed, so `byte_len` claims 1 while the blob is over the cap.
    #[test]
    fn get_art_rejects_an_oversize_blob() {
        let (db, _t, _art) = db_track_art();
        db.conn
            .execute_batch("PRAGMA ignore_check_constraints=ON")
            .unwrap();
        db.conn
            .execute(
                "INSERT INTO art (sha256, byte_len, data) \
                 VALUES (?1, 1, zeroblob(?2))",
                rusqlite::params!["d".repeat(64), MAX_ART_BYTES + 1],
            )
            .unwrap();
        let bad = db.conn.last_insert_rowid();
        let err = db.get_art(bad).unwrap_err();
        assert!(
            matches!(
                err,
                DbError::FieldTooLarge {
                    table: "art",
                    field: "data",
                    unit: "bytes",
                    ..
                }
            ),
            "{err:?}"
        );
    }

    /// The guards must not narrow an honest row: a real digest and a blob at
    /// the cap both read back intact.
    #[test]
    fn get_art_accepts_an_honest_row_at_the_blob_cap() {
        let db = Db::open_in_memory().unwrap();
        let data = vec![0u8; usize::try_from(MAX_ART_BYTES).unwrap()];
        let id = db.upsert_art(&NewArt { data: data.clone() }).unwrap();
        let got = db.get_art(id).unwrap().expect("art row");
        assert_eq!(got.sha256.len(), usize::try_from(ART_SHA256_LEN).unwrap());
        assert_eq!(got.byte_len, u64::try_from(MAX_ART_BYTES).unwrap());
        assert_eq!(got.data.len(), data.len());
    }

    /// Link one art row to one track with the given mime, bypassing the schema.
    ///
    /// The guard follows the column: the mime lives on `track_art` now (#716),
    /// so this is where an over-cap value has to be caught before the string is
    /// materialized. Planted with the constraints off for the same reason its
    /// siblings are — V4 bans the NUL outright, and the read guard is what
    /// protects a store that already holds such a row.
    fn link_with_mime(db: &Db, track: i64, art: i64, mime: &str) {
        db.conn
            .pragma_update(None, "ignore_check_constraints", true)
            .unwrap();
        db.conn
            .execute(
                "INSERT INTO track_art (track_id, art_id, picture_type, description, \
                 mime, ordinal) VALUES (?1, ?2, 3, '', ?3, 0)",
                rusqlite::params![track, art, mime],
            )
            .unwrap();
        db.conn
            .pragma_update(None, "ignore_check_constraints", false)
            .unwrap();
    }

    #[test]
    fn get_track_art_rejects_a_nul_truncated_mime() {
        let (db, track, art) = db_track_art();
        link_with_mime(&db, track, art, &nul_truncated(mime_byte_ceiling() + 1));
        for err in [
            db.get_track_art(track).unwrap_err(),
            db.get_track_art_with_meta(track).unwrap_err(),
        ] {
            assert!(
                matches!(
                    err,
                    DbError::FieldTooLarge {
                        table: "track_art",
                        field: "mime",
                        unit: "bytes",
                        ..
                    }
                ),
                "{err:?}"
            );
        }
    }

    /// The ceiling must not narrow the field: a mime of four-byte characters at
    /// the character cap sits exactly on the ceiling and still reads.
    #[test]
    fn get_track_art_accepts_four_byte_characters_at_cap() {
        let (db, track, art) = db_track_art();
        let mime = "\u{1D11E}".repeat(usize::try_from(MAX_ART_MIME_LEN).unwrap());
        assert_eq!(mime.len(), mime_byte_ceiling());
        link_with_mime(&db, track, art, &mime);
        assert_eq!(db.get_track_art(track).unwrap()[0].mime, mime);
    }

    #[test]
    fn get_track_art_rejects_a_nul_truncated_description() {
        let (db, track, art) = db_track_art();
        // Written with the constraints off. The schema now bans an embedded NUL
        // outright (#693), so this row can only arrive the way the threat model
        // says it does -- written before the ban, or by a writer that turned the
        // constraints off. Guarding it at read time is the whole point: no
        // constraint added later can clean a store that already has one.
        db.conn
            .pragma_update(None, "ignore_check_constraints", true)
            .unwrap();
        db.set_track_art(
            track,
            &[TrackArt {
                art_id: art,
                picture_type: 3,
                description: nul_truncated(description_byte_ceiling() + 1),
                mime: "image/png".into(),
                width: None,
                height: None,
                depth: 0,
                colors: 0,
                ordinal: 0,
            }],
        )
        .unwrap();
        db.conn
            .pragma_update(None, "ignore_check_constraints", false)
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
        // Written with the constraints off. The schema now bans an embedded NUL
        // outright (#693), so this row can only arrive the way the threat model
        // says it does -- written before the ban, or by a writer that turned the
        // constraints off. Guarding it at read time is the whole point: no
        // constraint added later can clean a store that already has one.
        db.conn
            .pragma_update(None, "ignore_check_constraints", true)
            .unwrap();
        db.set_track_art(
            track,
            &[TrackArt {
                art_id: art,
                picture_type: 3,
                description: nul_truncated(description_byte_ceiling() + 1),
                mime: "image/png".into(),
                width: None,
                height: None,
                depth: 0,
                colors: 0,
                ordinal: 0,
            }],
        )
        .unwrap();
        db.conn
            .pragma_update(None, "ignore_check_constraints", false)
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
                mime: "image/png".into(),
                width: None,
                height: None,
                depth: 0,
                colors: 0,
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
                mime: "image/png".into(),
                width: None,
                height: None,
                depth: 0,
                colors: 0,
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
                mime: "image/png".into(),
                width: None,
                height: None,
                depth: 0,
                colors: 0,
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
