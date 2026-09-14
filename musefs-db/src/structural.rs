use crate::error::DbError;
use crate::models::StructuralBlock;
use crate::{Db, ReadWrite, Result};
use rusqlite::params;

impl<M> Db<M> {
    /// Track ids that have at least one structural block row. Used by `revalidate`
    /// to detect legacy FLAC tracks (scanned under V1) that still need a backfill.
    pub fn track_ids_with_structural_blocks(&self) -> Result<std::collections::HashSet<i64>> {
        let mut stmt = self
            .conn
            .prepare("SELECT DISTINCT track_id FROM structural_blocks")?;
        let rows = stmt.query_map([], |r| r.get::<_, i64>(0))?;
        Ok(rows.collect::<rusqlite::Result<std::collections::HashSet<i64>>>()?)
    }

    /// Structural blocks for a track, ordered by (kind, ordinal). Empty when a
    /// FLAC track has not been (re)scanned under V2 — callers fall back to a
    /// front read in that case.
    pub fn get_structural_blocks(&self, track_id: i64) -> Result<Vec<StructuralBlock>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT length(kind), length(CAST(kind AS BLOB)), kind, ordinal, \
             length(body), body FROM structural_blocks \
             WHERE track_id = ?1 ORDER BY kind, ordinal",
        )?;
        let mut rows = stmt.query(params![track_id])?;
        let mut out = Vec::new();
        while let Some(r) = rows.next()? {
            // Bounded from its projections before it is allocated (#715): the
            // allowlist below is exact-match, so it rejects a hostile `kind`, but
            // only after reading it — and the only other bound is a write-time
            // CHECK a crafted store can have skipped.
            crate::error::check_text_field(
                "structural_blocks",
                "kind",
                r.get(0)?,
                r.get(1)?,
                crate::limits::MAX_STRUCTURAL_KIND_LEN,
            )?;
            let kind: String = r.get(2)?;
            let ordinal: i64 = r.get(3)?;
            let body_len: i64 = r.get(4)?;
            if !crate::limits::STRUCTURAL_KINDS.contains(&kind.as_str()) {
                return Err(DbError::InvalidStructuralBlock {
                    track_id,
                    detail: format!("unknown kind {kind:?}"),
                });
            }
            if ordinal < 0 {
                return Err(DbError::InvalidStructuralBlock {
                    track_id,
                    detail: format!("negative ordinal {ordinal}"),
                });
            }
            crate::error::check_field_bytes(
                "structural_blocks",
                "body",
                body_len,
                crate::limits::MAX_STRUCTURAL_BODY_LEN,
            )?;
            out.push(StructuralBlock {
                kind,
                ordinal: u64::try_from(ordinal).expect("ordinal guarded >= 0 above"),
                body: r.get(5)?,
            });
        }
        Ok(out)
    }
}

/// Replace a track's structural blocks. Runs on `conn` so `Db<ReadWrite>` (own
/// transaction) and `BulkWriter` (caller-held transaction) share one body.
///
/// A set identical to the stored one is left alone. Rewriting it fires the
/// delete and insert triggers, which bump `content_version`, and since #725 that
/// moves the synthesized file's served mtime: a revalidate re-probing unchanged
/// files made every FLAC look modified to anything comparing size and mtime.
pub(crate) fn set_structural_blocks_in(
    conn: &rusqlite::Connection,
    track_id: i64,
    blocks: &[StructuralBlock],
) -> Result<()> {
    if stored_set_is(conn, track_id, blocks)? {
        return Ok(());
    }
    conn.execute(
        "DELETE FROM structural_blocks WHERE track_id = ?1",
        params![track_id],
    )?;
    let mut stmt = conn.prepare_cached(
        "INSERT INTO structural_blocks (track_id, kind, ordinal, body) \
         VALUES (?1, ?2, ?3, ?4)",
    )?;
    for b in blocks {
        stmt.execute(params![track_id, b.kind, b.ordinal, b.body])?;
    }
    Ok(())
}

/// Whether `blocks` is exactly the set stored for `track_id`: as many rows, each
/// `(kind, ordinal)` given once, and each stored with a byte-equal body.
///
/// Anything short of that proof answers no, because the two wrong answers are
/// not alike: an unneeded rewrite costs one bump, while a wrong "identical"
/// serves a stale header. So a repeated key answers no as well, leaving the
/// rewrite to refuse it rather than letting two copies of one stored block
/// stand in for a different second one. Bodies are compared in SQL one block at
/// a time, so no stored body is materialized to decide.
fn stored_set_is(
    conn: &rusqlite::Connection,
    track_id: i64,
    blocks: &[StructuralBlock],
) -> Result<bool> {
    let stored: i64 = conn.query_row(
        "SELECT count(*) FROM structural_blocks WHERE track_id = ?1",
        params![track_id],
        |r| r.get(0),
    )?;
    if i64::try_from(blocks.len()).ok() != Some(stored) {
        return Ok(false);
    }
    let mut keys = std::collections::HashSet::with_capacity(blocks.len());
    for b in blocks {
        if !keys.insert((b.kind.as_str(), b.ordinal)) {
            return Ok(false);
        }
        let same = crate::query_optional(
            conn,
            "SELECT body = ?4 FROM structural_blocks \
             WHERE track_id = ?1 AND kind = ?2 AND ordinal = ?3",
            params![track_id, b.kind, b.ordinal, b.body],
            |r| Ok(r.get::<_, bool>(0)?),
        )?;
        if same != Some(true) {
            return Ok(false);
        }
    }
    Ok(true)
}

impl Db<ReadWrite> {
    /// Replace the track's structural blocks (FLAC STREAMINFO/SEEKTABLE).
    pub fn set_structural_blocks(&self, track_id: i64, blocks: &[StructuralBlock]) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        set_structural_blocks_in(&tx, track_id, blocks)?;
        tx.commit()?;
        Ok(())
    }
}

#[cfg(test)]
mod guard_tests {
    use crate::error::DbError;
    use crate::{Db, Format, NewTrack};

    fn db_with_track() -> (Db, i64) {
        let db = Db::open_in_memory().unwrap();
        let id = db
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
        (db, id)
    }

    #[test]
    fn rejects_oversize_body() {
        let (db, id) = db_with_track();
        db.conn
            .execute_batch("PRAGMA ignore_check_constraints=ON")
            .unwrap();
        db.conn
            .execute(
                "INSERT INTO structural_blocks (track_id, kind, ordinal, body) \
                 VALUES (?1, 'STREAMINFO', 0, zeroblob(16777216))",
                rusqlite::params![id],
            )
            .unwrap();
        let err = db.get_structural_blocks(id).unwrap_err();
        assert!(
            matches!(err, DbError::FieldTooLarge { field: "body", .. }),
            "{err:?}"
        );
    }

    #[test]
    fn accepts_body_at_cap() {
        let (db, id) = db_with_track();
        db.conn
            .execute(
                "INSERT INTO structural_blocks (track_id, kind, ordinal, body) \
                 VALUES (?1, 'STREAMINFO', 0, zeroblob(16777215))",
                rusqlite::params![id],
            )
            .unwrap();
        let rows = db.get_structural_blocks(id).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].body.len(), 16_777_215);
    }

    #[test]
    fn rejects_unknown_kind() {
        let (db, id) = db_with_track();
        db.conn
            .execute_batch("PRAGMA ignore_check_constraints=ON")
            .unwrap();
        db.conn
            .execute(
                "INSERT INTO structural_blocks (track_id, kind, ordinal, body) \
                 VALUES (?1, 'PADDING', 0, X'00')",
                rusqlite::params![id],
            )
            .unwrap();
        let err = db.get_structural_blocks(id).unwrap_err();
        assert!(
            matches!(err, DbError::InvalidStructuralBlock { .. }),
            "{err:?}"
        );
    }

    /// #715: an oversized `kind` is refused from its length alone, before the
    /// value is read — not by the allowlist after allocating it, and then again
    /// in the error's debug-escaped copy.
    #[test]
    fn rejects_oversize_kind_before_reading_it() {
        let (db, id) = db_with_track();
        db.conn
            .execute_batch("PRAGMA ignore_check_constraints=ON")
            .unwrap();
        db.conn
            .execute(
                "INSERT INTO structural_blocks (track_id, kind, ordinal, body) \
                 VALUES (?1, replace(hex(zeroblob(1048576)), '00', 'X'), 0, X'00')",
                rusqlite::params![id],
            )
            .unwrap();
        let err = db.get_structural_blocks(id).unwrap_err();
        assert!(
            matches!(err, DbError::FieldTooLarge { field: "kind", .. }),
            "{err:?}"
        );
    }

    /// SQLite stops counting a TEXT value's characters at an embedded NUL, so a
    /// valid kind followed by a NUL and a megabyte reads as ten characters. The
    /// byte projection is what catches it (#693's shape, on this column).
    #[test]
    fn rejects_nul_padded_kind_by_its_bytes() {
        let (db, id) = db_with_track();
        db.conn
            .execute_batch("PRAGMA ignore_check_constraints=ON")
            .unwrap();
        db.conn
            .execute(
                "INSERT INTO structural_blocks (track_id, kind, ordinal, body) \
                 VALUES (?1, 'STREAMINFO' || char(0) || \
                 replace(hex(zeroblob(1048576)), '00', 'X'), 0, X'00')",
                rusqlite::params![id],
            )
            .unwrap();
        let err = db.get_structural_blocks(id).unwrap_err();
        assert!(
            matches!(err, DbError::FieldTooLarge { field: "kind", .. }),
            "{err:?}"
        );
    }

    #[test]
    fn rejects_negative_ordinal() {
        let (db, id) = db_with_track();
        db.conn
            .execute_batch("PRAGMA ignore_check_constraints=ON")
            .unwrap();
        db.conn
            .execute(
                "INSERT INTO structural_blocks (track_id, kind, ordinal, body) \
                 VALUES (?1, 'STREAMINFO', -1, X'00')",
                rusqlite::params![id],
            )
            .unwrap();
        let err = db.get_structural_blocks(id).unwrap_err();
        assert!(
            matches!(err, DbError::InvalidStructuralBlock { .. }),
            "{err:?}"
        );
    }
}

#[cfg(test)]
mod tests {
    use crate::{Db, Format, NewTrack, StructuralBlock};

    #[test]
    fn structural_blocks_round_trip_and_replace() {
        let db = Db::open_in_memory().unwrap();
        let id = db
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
        db.set_structural_blocks(
            id,
            &[
                StructuralBlock {
                    kind: "STREAMINFO".into(),
                    ordinal: 0,
                    body: vec![1, 2],
                },
                StructuralBlock {
                    kind: "SEEKTABLE".into(),
                    ordinal: 0,
                    body: vec![3],
                },
            ],
        )
        .unwrap();
        let got = db.get_structural_blocks(id).unwrap();
        assert_eq!(got.len(), 2);
        // ordered by kind: SEEKTABLE before STREAMINFO
        assert_eq!(got[0].kind, "SEEKTABLE");
        assert_eq!(got[1].body, vec![1, 2]);

        db.set_structural_blocks(id, &[]).unwrap();
        assert!(db.get_structural_blocks(id).unwrap().is_empty());
    }

    fn block(kind: &str, ordinal: u64, body: &[u8]) -> StructuralBlock {
        StructuralBlock {
            kind: kind.into(),
            ordinal,
            body: body.to_vec(),
        }
    }

    fn track(db: &Db) -> i64 {
        db.upsert_track(&NewTrack {
            backing_path: std::path::PathBuf::from("/a.flac"),
            format: Format::Flac,
            audio_offset: 0,
            audio_length: 1,
            backing_size: 1,
            backing_mtime_ns: 0,
            backing_ctime_ns: 0,
            backing_ino: None,
        })
        .unwrap()
    }

    /// The order `get_structural_blocks` reads back in.
    fn as_read(mut blocks: Vec<StructuralBlock>) -> Vec<StructuralBlock> {
        blocks.sort_by(|a, b| (&a.kind, a.ordinal).cmp(&(&b.kind, b.ordinal)));
        blocks
    }

    fn base() -> Vec<StructuralBlock> {
        vec![
            block("STREAMINFO", 0, &[1, 2]),
            block("SEEKTABLE", 1, &[3]),
            block("SEEKTABLE", 2, &[4]),
        ]
    }

    /// #757: a re-probe finding the blocks it already stored must not bump
    /// `content_version`, which since #725 moves the served mtime. The slice
    /// order is the caller's, not part of the set.
    #[test]
    fn an_identical_set_is_left_alone_in_any_order() {
        let db = Db::open_in_memory().unwrap();
        let id = track(&db);
        db.set_structural_blocks(id, &base()).unwrap();
        let before = db.track_content_version(id).unwrap();

        db.set_structural_blocks(id, &base()).unwrap();
        let mut reversed = base();
        reversed.reverse();
        db.set_structural_blocks(id, &reversed).unwrap();

        assert_eq!(db.track_content_version(id).unwrap(), before);
        assert_eq!(db.get_structural_blocks(id).unwrap(), as_read(base()));
    }

    /// Every way two sets can differ still rewrites and bumps: skipping one of
    /// these would serve a header synthesized from blocks the file no longer has.
    #[test]
    fn every_difference_still_rewrites_and_bumps() {
        let with = |change: fn(&mut Vec<StructuralBlock>)| {
            let mut set = base();
            change(&mut set);
            set
        };
        for (what, next) in [
            (
                "a block added",
                with(|s| s.push(block("SEEKTABLE", 3, &[5]))),
            ),
            (
                "a block removed",
                with(|s| {
                    s.pop();
                }),
            ),
            ("a kind", with(|s| s[0].kind = "SEEKTABLE".into())),
            (
                "the order",
                with(|s| {
                    s[1].body = vec![4];
                    s[2].body = vec![3];
                }),
            ),
            ("one body byte", with(|s| s[0].body[1] = 9)),
        ] {
            let db = Db::open_in_memory().unwrap();
            let id = track(&db);
            db.set_structural_blocks(id, &base()).unwrap();
            let before = db.track_content_version(id).unwrap();

            db.set_structural_blocks(id, &next).unwrap();

            assert!(
                db.track_content_version(id).unwrap() > before,
                "{what} changed, so the set must be rewritten"
            );
            assert_eq!(
                db.get_structural_blocks(id).unwrap(),
                as_read(next),
                "{what}"
            );
        }
    }

    /// Two copies of one stored block, against a stored set of the same size,
    /// agree on the count and on every block given. Were that enough, the write
    /// would be skipped and succeed; it must still be refused.
    #[test]
    fn a_repeated_key_is_refused_rather_than_matched() {
        let db = Db::open_in_memory().unwrap();
        let id = track(&db);
        let stored = vec![block("STREAMINFO", 0, &[1, 2]), block("SEEKTABLE", 1, &[3])];
        db.set_structural_blocks(id, &stored).unwrap();

        let twice = [
            block("STREAMINFO", 0, &[1, 2]),
            block("STREAMINFO", 0, &[1, 2]),
        ];
        assert!(db.set_structural_blocks(id, &twice).is_err());
        assert_eq!(db.get_structural_blocks(id).unwrap(), as_read(stored));
    }
}
