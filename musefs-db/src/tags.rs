use crate::models::Tag;
use crate::{Db, Result};
use rusqlite::params;

impl Db {
    pub fn replace_tags(&self, track_id: i64, tags: &[Tag]) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        tx.execute("DELETE FROM tags WHERE track_id = ?1", params![track_id])?;
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

    pub fn get_tags(&self, track_id: i64) -> Result<Vec<Tag>> {
        let mut stmt = self.conn.prepare(
            "SELECT key, value, ordinal FROM tags WHERE track_id = ?1 ORDER BY key, ordinal",
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

    /// All tags for all tracks in one query, grouped by track id. Matches
    /// `get_tags`'s per-track ordering (`key, ordinal`), so callers can use it as
    /// a drop-in batch replacement for N calls to `get_tags`.
    pub fn tags_grouped(&self) -> Result<std::collections::HashMap<i64, Vec<Tag>>> {
        let mut stmt = self.conn.prepare(
            "SELECT track_id, key, value, ordinal FROM tags ORDER BY track_id, key, ordinal",
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
}
