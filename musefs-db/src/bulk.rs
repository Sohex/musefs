use crate::models::{NewArt, NewTrack, Tag, TrackArt};
use crate::{Db, Result};
use rusqlite::{params, Transaction};
use sha2::{Digest, Sha256};

fn sha256_hex(data: &[u8]) -> String {
    let digest = Sha256::digest(data);
    let mut s = String::with_capacity(64);
    for b in digest {
        use std::fmt::Write;
        let _ = write!(s, "{b:02x}");
    }
    s
}

impl Db {
    /// Apply the bulk-write pragmas to an open connection. WAL is left untouched
    /// (retained from `open`), so concurrent mount readers keep working. Safe on
    /// in-memory DBs. Intended for a scan-scoped `Db` the caller drops at scan end.
    pub fn apply_bulk_pragmas(conn: &rusqlite::Connection) -> Result<()> {
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
                (backing_path, format, audio_offset, audio_length, backing_size, backing_mtime, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, CAST(strftime('%s','now') AS INTEGER))
             ON CONFLICT(backing_path) DO UPDATE SET
                format=excluded.format, audio_offset=excluded.audio_offset,
                audio_length=excluded.audio_length, backing_size=excluded.backing_size,
                backing_mtime=excluded.backing_mtime,
                updated_at=CAST(strftime('%s','now') AS INTEGER)",
            params![t.backing_path, t.format.as_str(), t.audio_offset, t.audio_length, t.backing_size, t.backing_mtime],
        )?;
        Ok(self.tx.query_row(
            "SELECT id FROM tracks WHERE backing_path = ?1",
            params![t.backing_path],
            |r| r.get(0),
        )?)
    }

    pub fn replace_tags(&mut self, track_id: i64, tags: &[Tag]) -> Result<()> {
        self.tx
            .execute("DELETE FROM tags WHERE track_id = ?1", params![track_id])?;
        let mut stmt = self.tx.prepare_cached(
            "INSERT INTO tags (track_id, key, value, ordinal) VALUES (?1, ?2, ?3, ?4)",
        )?;
        for t in tags {
            stmt.execute(params![track_id, t.key, t.value, t.ordinal])?;
        }
        Ok(())
    }

    pub fn upsert_art(&mut self, a: &NewArt) -> Result<i64> {
        let sha = sha256_hex(&a.data);
        self.tx.execute(
            "INSERT INTO art (sha256, mime, width, height, byte_len, data)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6) ON CONFLICT(sha256) DO NOTHING",
            params![sha, a.mime, a.width, a.height, a.data.len() as i64, a.data],
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
    use crate::models::{Format, NewArt, NewTrack, Tag, TrackArt};
    use crate::Db;

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
                        backing_mtime: 1,
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
    }
}
