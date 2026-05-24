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
}
