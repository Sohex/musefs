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
        let mut stmt = self.conn.prepare(
            "SELECT kind, ordinal, body FROM structural_blocks \
             WHERE track_id = ?1 ORDER BY kind, ordinal",
        )?;
        let rows = stmt.query_map(params![track_id], |r| {
            Ok(StructuralBlock {
                kind: r.get(0)?,
                ordinal: r.get::<_, i64>(1)? as u64,
                body: r.get(2)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }
}

impl Db<ReadWrite> {
    /// Replace the track's structural blocks (FLAC STREAMINFO/SEEKTABLE).
    pub fn set_structural_blocks(&self, track_id: i64, blocks: &[StructuralBlock]) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        tx.execute(
            "DELETE FROM structural_blocks WHERE track_id = ?1",
            params![track_id],
        )?;
        {
            let mut stmt = tx.prepare(
                "INSERT INTO structural_blocks (track_id, kind, ordinal, body) \
                 VALUES (?1, ?2, ?3, ?4)",
            )?;
            for b in blocks {
                stmt.execute(params![track_id, b.kind, b.ordinal as i64, b.body])?;
            }
        }
        tx.commit()?;
        Ok(())
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
                backing_path: "/a.flac".into(),
                format: Format::Flac,
                audio_offset: 0,
                audio_length: 1,
                backing_size: 1,
                backing_mtime: 0,
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
}
