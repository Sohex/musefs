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
            Some(r) => {
                let byte_len: i64 = r.get(5)?;
                if byte_len < 0 {
                    return Err(rusqlite::Error::InvalidParameterName(format!(
                        "negative byte_len {byte_len} for art id {id}"
                    ))
                    .into());
                }
                Ok(Some(Art {
                    id: r.get(0)?,
                    sha256: r.get(1)?,
                    mime: r.get(2)?,
                    width: r.get::<_, Option<i64>>(3)?.map(|v| v as u32),
                    height: r.get::<_, Option<i64>>(4)?.map(|v| v as u32),
                    byte_len: byte_len as u64,
                    data: r.get(6)?,
                }))
            }
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
            Some(r) => {
                let byte_len: i64 = r.get(3)?;
                if byte_len < 0 {
                    return Err(rusqlite::Error::InvalidParameterName(format!(
                        "negative byte_len {byte_len} for art id {id}"
                    ))
                    .into());
                }
                Ok(Some(ArtMeta {
                    mime: r.get(0)?,
                    width: r.get::<_, Option<i64>>(1)?.map(|v| v as u32),
                    height: r.get::<_, Option<i64>>(2)?.map(|v| v as u32),
                    byte_len: byte_len as u64,
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
        let mut stmt = self.conn.prepare(
            "SELECT art_id, picture_type, description, ordinal
             FROM track_art WHERE track_id = ?1 ORDER BY ordinal",
        )?;
        let rows = stmt.query_map(params![track_id], |r| {
            Ok(TrackArt {
                art_id: r.get(0)?,
                picture_type: r.get::<_, i32>(1)? as u32,
                description: r.get(2)?,
                ordinal: r.get::<_, i64>(3)? as u64,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }
}

impl Db<ReadWrite> {
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
                    it.picture_type as i32,
                    it.description,
                    it.ordinal as i64
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
