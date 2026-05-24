use crate::models::{Art, ArtMeta, NewArt, TrackArt};
use crate::{Db, Result};
use rusqlite::params;
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
    pub fn upsert_art(&self, a: &NewArt) -> Result<i64> {
        let sha = sha256_hex(&a.data);
        self.conn.execute(
            "INSERT INTO art (sha256, mime, width, height, byte_len, data)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(sha256) DO NOTHING",
            params![sha, a.mime, a.width, a.height, a.data.len() as i64, a.data],
        )?;
        let id =
            self.conn
                .query_row("SELECT id FROM art WHERE sha256 = ?1", params![sha], |r| {
                    r.get(0)
                })?;
        Ok(id)
    }

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
        let mut stmt = self
            .conn
            .prepare("SELECT mime, width, height, byte_len FROM art WHERE id = ?1")?;
        let mut rows = stmt.query(params![id])?;
        match rows.next()? {
            Some(r) => Ok(Some(ArtMeta {
                mime: r.get(0)?,
                width: r.get(1)?,
                height: r.get(2)?,
                byte_len: r.get(3)?,
            })),
            None => Ok(None),
        }
    }

    /// Stream `len` bytes of an art blob starting at `offset` via SQLite
    /// incremental blob I/O, so image bytes are never fully materialized. The
    /// caller guarantees `offset + len` is within the blob (the segment layout is
    /// built from `byte_len`).
    pub fn read_art_chunk(&self, art_id: i64, offset: u64, len: usize) -> Result<Vec<u8>> {
        let blob =
            self.conn
                .blob_open(rusqlite::DatabaseName::Main, "art", "data", art_id, true)?;
        let mut buf = vec![0u8; len];
        blob.read_at(&mut buf, offset as usize)?;
        Ok(buf)
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

    pub fn get_track_art(&self, track_id: i64) -> Result<Vec<TrackArt>> {
        let mut stmt = self.conn.prepare(
            "SELECT art_id, picture_type, description, ordinal
             FROM track_art WHERE track_id = ?1 ORDER BY ordinal",
        )?;
        let rows = stmt.query_map(params![track_id], |r| {
            Ok(TrackArt {
                art_id: r.get(0)?,
                picture_type: r.get(1)?,
                description: r.get(2)?,
                ordinal: r.get(3)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }
}
