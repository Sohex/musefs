use crate::art::sha256_hex;
use crate::models::{BinaryTag, NewArt, NewTrack, StructuralBlock, Tag, TrackArt};
use crate::{Db, ReadWrite, Result};
use rusqlite::{Transaction, params};

impl Db<ReadWrite> {
    /// Apply the bulk-write pragmas to an open connection. WAL is left untouched
    /// (retained from `open`), so concurrent mount readers keep working. Safe on
    /// in-memory DBs. Intended for a scan-scoped `Db` the caller drops at scan end.
    pub(crate) fn apply_bulk_pragmas(conn: &rusqlite::Connection) -> Result<()> {
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "cache_size", -65536)?; // 64 MiB
        conn.pragma_update(None, "temp_store", "MEMORY")?;
        Ok(())
    }

    /// Apply bulk pragmas to this DB's own connection.
    pub fn apply_bulk_pragmas_self(&self) -> Result<()> {
        Self::apply_bulk_pragmas(&self.conn)
    }

    /// Begin a batch transaction. All writes go through the returned handle and
    /// land atomically on `commit()`.
    pub fn bulk_writer(&self) -> Result<BulkWriter<'_>> {
        Ok(BulkWriter {
            tx: self.conn.unchecked_transaction()?,
        })
    }
}

/// A batch of track writes held in one transaction. Mirrors `Db::upsert_track` /
/// `replace_tags` / `upsert_art` / `set_track_art`, but executes on a single
/// caller-held transaction so a whole batch commits with one fsync.
pub struct BulkWriter<'c> {
    tx: Transaction<'c>,
}

impl BulkWriter<'_> {
    pub fn upsert_track(&mut self, t: &NewTrack) -> Result<i64> {
        self.tx.execute(
            "INSERT INTO tracks
                (backing_path, format, audio_offset, audio_length, backing_size, backing_mtime_ns, backing_ctime_ns, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, CAST(strftime('%s','now') AS INTEGER))
             ON CONFLICT(backing_path) DO UPDATE SET
                format=excluded.format, audio_offset=excluded.audio_offset,
                audio_length=excluded.audio_length, backing_size=excluded.backing_size,
                backing_mtime_ns=excluded.backing_mtime_ns,
                backing_ctime_ns=excluded.backing_ctime_ns,
                updated_at=CAST(strftime('%s','now') AS INTEGER)",
            params![t.backing_path, t.format.as_str(), t.audio_offset, t.audio_length, t.backing_size, t.backing_mtime_ns, t.backing_ctime_ns],
        )?;
        Ok(self.tx.query_row(
            "SELECT id FROM tracks WHERE backing_path = ?1",
            params![t.backing_path],
            |r| r.get(0),
        )?)
    }

    pub fn replace_tags(&mut self, track_id: i64, tags: &[Tag]) -> Result<()> {
        self.tx.execute(
            "DELETE FROM tags WHERE track_id = ?1 AND value_blob IS NULL",
            params![track_id],
        )?;
        let mut stmt = self.tx.prepare_cached(
            "INSERT INTO tags (track_id, key, value, ordinal) VALUES (?1, ?2, ?3, ?4)",
        )?;
        for t in tags {
            stmt.execute(params![track_id, t.key, t.value, t.ordinal])?;
        }
        Ok(())
    }

    pub fn set_binary_tags(&mut self, track_id: i64, tags: &[BinaryTag]) -> Result<()> {
        self.tx.execute(
            "DELETE FROM tags WHERE track_id = ?1 AND value_blob IS NOT NULL",
            params![track_id],
        )?;
        let mut stmt = self.tx.prepare(
            "INSERT INTO tags (track_id, key, value, value_blob, ordinal) \
             VALUES (?1, ?2, '', ?3, ?4)",
        )?;
        for t in tags {
            stmt.execute(params![track_id, t.key, t.payload, t.ordinal])?;
        }
        Ok(())
    }

    pub fn set_structural_blocks(
        &mut self,
        track_id: i64,
        blocks: &[StructuralBlock],
    ) -> Result<()> {
        self.tx.execute(
            "DELETE FROM structural_blocks WHERE track_id = ?1",
            params![track_id],
        )?;
        let mut stmt = self.tx.prepare(
            "INSERT INTO structural_blocks (track_id, kind, ordinal, body) \
             VALUES (?1, ?2, ?3, ?4)",
        )?;
        for b in blocks {
            stmt.execute(params![track_id, b.kind, b.ordinal, b.body])?;
        }
        Ok(())
    }

    pub fn upsert_art(&mut self, a: &NewArt) -> Result<i64> {
        let sha = sha256_hex(&a.data);
        self.tx.execute(
            "INSERT INTO art (sha256, mime, width, height, byte_len, data)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6) ON CONFLICT(sha256) DO NOTHING",
            params![sha, a.mime, a.width, a.height, a.data.len() as u64, a.data],
        )?;
        Ok(self
            .tx
            .query_row("SELECT id FROM art WHERE sha256 = ?1", params![sha], |r| {
                r.get(0)
            })?)
    }

    pub fn set_track_art(&mut self, track_id: i64, items: &[TrackArt]) -> Result<()> {
        self.tx.execute(
            "DELETE FROM track_art WHERE track_id = ?1",
            params![track_id],
        )?;
        let mut stmt = self.tx.prepare_cached(
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

    pub fn commit(self) -> Result<()> {
        self.tx.commit()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use crate::Db;
    use crate::models::{Format, NewArt, NewTrack, Tag, TrackArt};

    #[test]
    fn bulk_writer_persists_a_batch_in_one_commit() {
        let db = Db::open_in_memory().unwrap();
        {
            let mut bw = db.bulk_writer().unwrap();
            for i in 0..3 {
                let id = bw
                    .upsert_track(&NewTrack {
                        backing_path: format!("/m/{i}.flac"),
                        format: Format::Flac,
                        audio_offset: 100,
                        audio_length: 200,
                        backing_size: 300,
                        backing_mtime_ns: 1,
                        backing_ctime_ns: 0,
                    })
                    .unwrap();
                bw.replace_tags(id, &[Tag::new("title", &format!("t{i}"), 0)])
                    .unwrap();
                let art_id = bw
                    .upsert_art(&NewArt {
                        mime: "image/png".into(),
                        width: None,
                        height: None,
                        data: vec![1, 2, 3, 4],
                    })
                    .unwrap();
                bw.set_track_art(
                    id,
                    &[TrackArt {
                        art_id,
                        picture_type: 3,
                        description: String::new(),
                        ordinal: 0,
                    }],
                )
                .unwrap();
            }
            bw.commit().unwrap();
        }
        assert_eq!(db.list_tracks().unwrap().len(), 3);
        // Dedup: identical art blob stored once.
        let count: i64 = db
            .conn
            .query_row("SELECT COUNT(*) FROM art", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1);
        // replace_tags actually persisted one tag per track (kills no-op replace_tags).
        let tag_count: i64 = db
            .conn
            .query_row("SELECT COUNT(*) FROM tags", [], |r| r.get(0))
            .unwrap();
        assert_eq!(tag_count, 3);
        let title0: String = db
            .conn
            .query_row(
                "SELECT value FROM tags WHERE key = 'title' ORDER BY value LIMIT 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(title0, "t0");
        // set_track_art actually persisted one link per track (kills no-op set_track_art).
        let track_art_count: i64 = db
            .conn
            .query_row("SELECT COUNT(*) FROM track_art", [], |r| r.get(0))
            .unwrap();
        assert_eq!(track_art_count, 3);
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
    fn apply_bulk_pragmas_self_sets_non_default_pragmas() {
        let db = Db::open_in_memory().unwrap();
        db.apply_bulk_pragmas_self().unwrap();
        // synchronous NORMAL == 1 (default for in-memory is FULL == 2).
        let synchronous: i64 = db
            .conn
            .pragma_query_value(None, "synchronous", |r| r.get(0))
            .unwrap();
        assert_eq!(synchronous, 1);
        // cache_size == -65536 (negative => KiB; sign matters, default is -2000).
        let cache_size: i64 = db
            .conn
            .pragma_query_value(None, "cache_size", |r| r.get(0))
            .unwrap();
        assert_eq!(cache_size, -65536);
        // temp_store MEMORY == 2 (default is 0).
        let temp_store: i64 = db
            .conn
            .pragma_query_value(None, "temp_store", |r| r.get(0))
            .unwrap();
        assert_eq!(temp_store, 2);
    }

    #[test]
    fn bulk_replace_tags_preserves_binary_rows() {
        let db = Db::open_in_memory().unwrap();
        let tid = db
            .upsert_track(&crate::NewTrack {
                backing_path: "/a.mp3".into(),
                format: crate::Format::Mp3,
                audio_offset: 0,
                audio_length: 0,
                backing_size: 0,
                backing_mtime_ns: 0,
                backing_ctime_ns: 0,
            })
            .unwrap();
        db.set_binary_tags(
            tid,
            &[crate::BinaryTag {
                key: "PRIV".into(),
                payload: vec![1, 2, 3],
                ordinal: 0,
            }],
        )
        .unwrap();

        {
            let mut bw = db.bulk_writer().unwrap();
            bw.replace_tags(tid, &[crate::Tag::new("artist", "A", 0)])
                .unwrap();
            bw.commit().unwrap();
        }

        assert_eq!(
            db.get_binary_tags(tid).unwrap().len(),
            1,
            "bulk replace_tags wiped binary rows"
        );
        assert_eq!(
            db.get_tags(tid).unwrap(),
            vec![crate::Tag::new("artist", "A", 0)]
        );
    }

    #[test]
    fn bulk_set_binary_tags_round_trips_and_scopes_to_binary_rows() {
        let db = Db::open_in_memory().unwrap();
        let tid = db
            .upsert_track(&crate::NewTrack {
                backing_path: "/a.mp3".into(),
                format: crate::Format::Mp3,
                audio_offset: 0,
                audio_length: 0,
                backing_size: 0,
                backing_mtime_ns: 0,
                backing_ctime_ns: 0,
            })
            .unwrap();
        {
            let mut bw = db.bulk_writer().unwrap();
            bw.replace_tags(tid, &[crate::Tag::new("artist", "A", 0)])
                .unwrap();
            bw.set_binary_tags(
                tid,
                &[crate::BinaryTag {
                    key: "PRIV".into(),
                    payload: vec![7, 7, 7],
                    ordinal: 0,
                }],
            )
            .unwrap();
            bw.commit().unwrap();
        }
        let rows = db.get_binary_tags(tid).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].key, "PRIV");
        assert_eq!(rows[0].byte_len, 3);
        assert_eq!(
            db.get_tags(tid).unwrap(),
            vec![crate::Tag::new("artist", "A", 0)]
        );
    }

    #[test]
    fn bulk_set_structural_blocks_round_trips() {
        use crate::StructuralBlock;
        let db = Db::open_in_memory().unwrap();
        let id = {
            let mut bw = db.bulk_writer().unwrap();
            let id = bw
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
            bw.set_structural_blocks(
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
            bw.commit().unwrap();
            id
        };
        let got = db.get_structural_blocks(id).unwrap();
        assert_eq!(got.len(), 2);
        // get_structural_blocks orders by kind: SEEKTABLE before STREAMINFO.
        assert_eq!(got[0].kind, "SEEKTABLE");
        assert_eq!(got[1].body, vec![1, 2]);
    }

    #[test]
    fn bulk_writer_dropped_without_commit_rolls_back() {
        let db = Db::open_in_memory().unwrap();
        {
            let mut bw = db.bulk_writer().unwrap();
            bw.upsert_track(&NewTrack {
                backing_path: "/m/ghost.flac".into(),
                format: Format::Flac,
                audio_offset: 0,
                audio_length: 0,
                backing_size: 0,
                backing_mtime_ns: 0,
                backing_ctime_ns: 0,
            })
            .unwrap();
            // Dropped here without `commit()` → Transaction rolls back.
        }
        assert_eq!(db.list_tracks().unwrap().len(), 0);
    }
}
