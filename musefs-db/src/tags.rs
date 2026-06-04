use crate::models::{BinaryTag, BinaryTagRow, Tag};
use crate::{Db, ReadWrite, Result};
use rusqlite::params;

impl<M> Db<M> {
    pub fn get_tags(&self, track_id: i64) -> Result<Vec<Tag>> {
        let mut stmt = self.conn.prepare(
            "SELECT key, value, ordinal FROM tags \
             WHERE track_id = ?1 AND value_blob IS NULL ORDER BY key, ordinal",
        )?;
        let rows = stmt.query_map(params![track_id], |r| {
            Ok(Tag {
                key: r.get(0)?,
                value: r.get(1)?,
                ordinal: r.get(2)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Tags for a specific set of track ids, grouped by track id, ordered within each
    /// track by `key, ordinal` (same as `tags_grouped`, so `tags_to_fields` sees the
    /// lowest-ordinal value of each key first). The `IN (…)` list is chunked to stay
    /// under SQLite's bound-variable limit. Used by incremental refresh to render only
    /// changed/added tracks. See SP2 Component 2.
    pub fn tags_for_tracks(
        &self,
        track_ids: &[i64],
    ) -> Result<std::collections::HashMap<i64, Vec<Tag>>> {
        const CHUNK: usize = 900;
        let mut out: std::collections::HashMap<i64, Vec<Tag>> = std::collections::HashMap::new();
        for chunk in track_ids.chunks(CHUNK) {
            let placeholders = vec!["?"; chunk.len()].join(",");
            let sql = format!(
                "SELECT track_id, key, value, ordinal FROM tags \
                 WHERE track_id IN ({placeholders}) AND value_blob IS NULL \
                 ORDER BY track_id, key, ordinal"
            );
            let mut stmt = self.conn.prepare(&sql)?;
            let params = rusqlite::params_from_iter(chunk.iter());
            let rows = stmt.query_map(params, |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    Tag {
                        key: r.get(1)?,
                        value: r.get(2)?,
                        ordinal: r.get(3)?,
                    },
                ))
            })?;
            for row in rows {
                let (track_id, tag) = row?;
                out.entry(track_id).or_default().push(tag);
            }
        }
        Ok(out)
    }

    /// All tags for all tracks in one query, grouped by track id. Matches
    /// `get_tags`'s per-track ordering (`key, ordinal`), so callers can use it as
    /// a drop-in batch replacement for N calls to `get_tags`.
    pub fn tags_grouped(&self) -> Result<std::collections::HashMap<i64, Vec<Tag>>> {
        let mut stmt = self.conn.prepare(
            "SELECT track_id, key, value, ordinal FROM tags \
             WHERE value_blob IS NULL ORDER BY track_id, key, ordinal",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                Tag {
                    key: r.get(1)?,
                    value: r.get(2)?,
                    ordinal: r.get(3)?,
                },
            ))
        })?;
        let mut out: std::collections::HashMap<i64, Vec<Tag>> = std::collections::HashMap::new();
        for row in rows {
            let (track_id, tag) = row?;
            out.entry(track_id).or_default().push(tag);
        }
        Ok(out)
    }

    /// Binary tag rows for a track: streaming handle (rowid), key, and payload
    /// length. Ordered by (key, ordinal) to match `get_binary_tags`/synthesis order.
    pub fn get_binary_tags(&self, track_id: i64) -> Result<Vec<BinaryTagRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT rowid, key, length(value_blob) FROM tags \
             WHERE track_id = ?1 AND value_blob IS NOT NULL ORDER BY key, ordinal",
        )?;
        let rows = stmt.query_map(params![track_id], |r| {
            Ok(BinaryTagRow {
                rowid: r.get(0)?,
                key: r.get(1)?,
                byte_len: r.get(2)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Stream binary-tag bytes at `offset` directly into `buf` via incremental blob
    /// I/O — no intermediate allocation (#70). A short read means the row changed
    /// underneath the resolved layout; `read_at_exact` surfaces it as an error rather
    /// than zero-filling. (`payload_id` is the `tags` rowid; see the spec's
    /// "payload_id validity invariant".)
    pub fn read_binary_tag_chunk_into(
        &self,
        payload_id: i64,
        offset: u64,
        buf: &mut [u8],
    ) -> Result<()> {
        let blob = self
            .conn
            .blob_open("main", "tags", "value_blob", payload_id, true)?;
        blob.read_at_exact(buf, offset as usize)?;
        Ok(())
    }

    /// Allocating convenience form of `read_binary_tag_chunk_into` (non-hot-path
    /// callers).
    pub fn read_binary_tag_chunk(
        &self,
        payload_id: i64,
        offset: u64,
        len: usize,
    ) -> Result<Vec<u8>> {
        let mut buf = vec![0u8; len];
        self.read_binary_tag_chunk_into(payload_id, offset, &mut buf)?;
        Ok(buf)
    }
}

impl Db<ReadWrite> {
    pub fn replace_tags(&self, track_id: i64, tags: &[Tag]) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        tx.execute(
            "DELETE FROM tags WHERE track_id = ?1 AND value_blob IS NULL",
            params![track_id],
        )?;
        {
            let mut stmt = tx.prepare(
                "INSERT INTO tags (track_id, key, value, ordinal) VALUES (?1, ?2, ?3, ?4)",
            )?;
            for t in tags {
                stmt.execute(params![track_id, t.key, t.value, t.ordinal])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Replace the track's binary tag rows (value_blob IS NOT NULL); text rows
    /// (managed by `replace_tags`) are untouched. Binary rows store '' in `value`.
    pub fn set_binary_tags(&self, track_id: i64, tags: &[BinaryTag]) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        tx.execute(
            "DELETE FROM tags WHERE track_id = ?1 AND value_blob IS NOT NULL",
            params![track_id],
        )?;
        {
            let mut stmt = tx.prepare(
                "INSERT INTO tags (track_id, key, value, value_blob, ordinal) \
                 VALUES (?1, ?2, '', ?3, ?4)",
            )?;
            for t in tags {
                stmt.execute(params![track_id, t.key, t.payload, t.ordinal])?;
            }
        }
        tx.commit()?;
        Ok(())
    }
}

#[cfg(test)]
mod tags_for_tracks_tests {
    use super::*;
    use crate::{Format, NewTrack, Tag};

    fn open_mem() -> Db {
        Db::open_in_memory().unwrap()
    }
    fn new_track(path: &str) -> NewTrack {
        NewTrack {
            backing_path: path.into(),
            format: Format::Flac,
            audio_offset: 0,
            audio_length: 1,
            backing_size: 1,
            backing_mtime: 0,
        }
    }

    #[test]
    fn tags_for_tracks_returns_only_requested_ordered_by_key_ordinal() {
        let db = open_mem();
        let a = db.upsert_track(&new_track("/a.flac")).unwrap();
        let b = db.upsert_track(&new_track("/b.flac")).unwrap();
        let c = db.upsert_track(&new_track("/c.flac")).unwrap();
        db.replace_tags(
            a,
            &[
                Tag::new("ARTIST", "second", 1),
                Tag::new("ARTIST", "first", 0),
            ],
        )
        .unwrap();
        db.replace_tags(b, &[Tag::new("ARTIST", "bee", 0)]).unwrap();
        db.replace_tags(c, &[Tag::new("ARTIST", "cee", 0)]).unwrap();

        let got = db.tags_for_tracks(&[a, b]).unwrap();
        assert_eq!(got.len(), 2, "c was not requested");
        assert!(!got.contains_key(&c));
        let a_tags = &got[&a];
        assert_eq!(a_tags[0].value, "first");
        assert_eq!(a_tags[1].value, "second");
    }

    #[test]
    fn tags_for_tracks_chunks_beyond_sqlite_variable_limit() {
        let db = open_mem();
        let mut ids = Vec::new();
        for i in 0..1500 {
            let id = db.upsert_track(&new_track(&format!("/t{i}.flac"))).unwrap();
            db.replace_tags(id, &[Tag::new("TITLE", &format!("t{i}"), 0)])
                .unwrap();
            ids.push(id);
        }
        let got = db.tags_for_tracks(&ids).unwrap();
        assert_eq!(got.len(), 1500, "all chunks fetched");
    }

    #[test]
    fn tags_for_tracks_empty_input_is_empty_map() {
        let db = open_mem();
        assert!(db.tags_for_tracks(&[]).unwrap().is_empty());
    }

    #[test]
    fn text_queries_exclude_binary_rows() {
        let db = open_mem();
        let a = db.upsert_track(&new_track("/a.flac")).unwrap();
        db.replace_tags(a, &[Tag::new("artist", "Alice", 0)])
            .unwrap();
        db.conn
            .execute(
                "INSERT INTO tags (track_id, key, value, value_blob, ordinal) \
                 VALUES (?1, 'PRIV', '', X'DEADBEEF', 0)",
                rusqlite::params![a],
            )
            .unwrap();

        let got = db.get_tags(a).unwrap();
        assert_eq!(got, vec![Tag::new("artist", "Alice", 0)]);
        let grouped = db.tags_grouped().unwrap();
        assert_eq!(grouped[&a], vec![Tag::new("artist", "Alice", 0)]);
        let for_tracks = db.tags_for_tracks(&[a]).unwrap();
        assert_eq!(for_tracks[&a], vec![Tag::new("artist", "Alice", 0)]);
    }

    #[test]
    fn binary_tags_round_trip_and_are_independent_of_text() {
        let db = open_mem();
        let a = db.upsert_track(&new_track("/a.flac")).unwrap();
        db.replace_tags(a, &[Tag::new("artist", "Alice", 0)])
            .unwrap();
        db.set_binary_tags(
            a,
            &[
                crate::BinaryTag {
                    key: "PRIV".into(),
                    payload: vec![1, 2, 3],
                    ordinal: 0,
                },
                crate::BinaryTag {
                    key: "PRIV".into(),
                    payload: vec![9, 9],
                    ordinal: 1,
                },
                crate::BinaryTag {
                    key: "GEOB".into(),
                    payload: vec![7],
                    ordinal: 0,
                },
            ],
        )
        .unwrap();

        assert_eq!(
            db.get_tags(a).unwrap(),
            vec![Tag::new("artist", "Alice", 0)]
        );

        let rows = db.get_binary_tags(a).unwrap();
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].key, "GEOB");
        assert_eq!(rows[0].byte_len, 1);
        assert_eq!(rows[1].key, "PRIV");
        assert_eq!(rows[1].byte_len, 3);
        assert_eq!(rows[2].byte_len, 2);

        let full = db.read_binary_tag_chunk(rows[1].rowid, 0, 3).unwrap();
        assert_eq!(full, vec![1, 2, 3]);
        let mid = db.read_binary_tag_chunk(rows[1].rowid, 1, 2).unwrap();
        assert_eq!(mid, vec![2, 3]);

        db.set_binary_tags(a, &[]).unwrap();
        assert!(db.get_binary_tags(a).unwrap().is_empty());
        assert_eq!(
            db.get_tags(a).unwrap(),
            vec![Tag::new("artist", "Alice", 0)]
        );
    }
}
