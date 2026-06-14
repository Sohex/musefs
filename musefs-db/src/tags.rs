use crate::error::{check_field_len, check_tag_count};
use crate::limits::{MAX_TAG_KEY_LEN, MAX_TAG_VALUE_LEN};
use crate::models::{BinaryTag, BinaryTagRow, Tag};
use crate::{Db, ReadWrite, Result};
use rusqlite::params;

/// Reject an over-cap text-tag row from its `length(key)`/`length(value)`
/// columns *before* the strings are materialized. Routes through the shared
/// `check_field_len`, so the allocation-free guarantee is the same one its
/// unit test pins (spec N13).
fn check_tag_lengths(key_len: i64, value_len: i64) -> Result<()> {
    check_field_len("tags", "key", key_len, MAX_TAG_KEY_LEN)?;
    check_field_len("tags", "value", value_len, MAX_TAG_VALUE_LEN)?;
    Ok(())
}

/// Columns the grouped tag readers project: `track_id` followed by the five
/// columns `read_tag_row` consumes. Kept in lockstep with `read_tag_row`'s
/// offset arithmetic.
const GROUPED_TAG_COLS: &str = "track_id, length(key), length(value), key, value, ordinal";

/// Read one text-tag row laid out as `length(key), length(value), key, value,
/// ordinal` starting at column `base`; length-guards the row before its strings
/// are materialized (spec N13).
fn read_tag_row(r: &rusqlite::Row, base: usize) -> Result<Tag> {
    check_tag_lengths(r.get(base)?, r.get(base + 1)?)?;
    Ok(Tag {
        key: r.get(base + 2)?,
        value: r.get(base + 3)?,
        ordinal: r.get(base + 4)?,
    })
}

/// Drain grouped tag rows (`GROUPED_TAG_COLS`: `track_id` then `read_tag_row`'s
/// five columns at base 1) into `out`, enforcing the per-track count cap.
fn collect_grouped_tags(
    rows: &mut rusqlite::Rows,
    out: &mut std::collections::HashMap<i64, Vec<Tag>>,
) -> Result<()> {
    while let Some(r) = rows.next()? {
        let track_id: i64 = r.get(0)?;
        let entry = out.entry(track_id).or_default();
        entry.push(read_tag_row(r, 1)?);
        check_tag_count(track_id, entry.len())?;
    }
    Ok(())
}

impl<M> Db<M> {
    pub fn get_tags(&self, track_id: i64) -> Result<Vec<Tag>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT length(key), length(value), key, value, ordinal FROM tags \
             WHERE track_id = ?1 AND value_blob IS NULL ORDER BY key, ordinal",
        )?;
        let mut rows = stmt.query(params![track_id])?;
        let mut out = Vec::new();
        while let Some(r) = rows.next()? {
            out.push(read_tag_row(r, 0)?);
            check_tag_count(track_id, out.len())?;
        }
        Ok(out)
    }

    pub fn tags_for_tracks(
        &self,
        track_ids: &[i64],
    ) -> Result<std::collections::HashMap<i64, Vec<Tag>>> {
        let mut out = std::collections::HashMap::new();
        crate::query_in_chunks(
            &self.conn,
            track_ids,
            |ph| {
                format!(
                    "SELECT {GROUPED_TAG_COLS} FROM tags \
                     WHERE track_id IN ({ph}) AND value_blob IS NULL \
                     ORDER BY track_id, key, ordinal"
                )
            },
            |rows| collect_grouped_tags(rows, &mut out),
        )?;
        Ok(out)
    }

    pub fn tags_grouped(&self) -> Result<std::collections::HashMap<i64, Vec<Tag>>> {
        let sql = format!(
            "SELECT {GROUPED_TAG_COLS} FROM tags \
             WHERE value_blob IS NULL ORDER BY track_id, key, ordinal"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let mut rows = stmt.query([])?;
        let mut out = std::collections::HashMap::new();
        collect_grouped_tags(&mut rows, &mut out)?;
        Ok(out)
    }

    pub fn tags_grouped_for_keys(
        &self,
        keys: &[&str],
    ) -> Result<std::collections::HashMap<i64, Vec<Tag>>> {
        let lowered: Vec<String> = keys.iter().map(|k| k.to_ascii_lowercase()).collect();
        let mut out = std::collections::HashMap::new();
        crate::query_in_chunks(
            &self.conn,
            &lowered,
            |ph| {
                format!(
                    "SELECT {GROUPED_TAG_COLS} FROM tags \
                     WHERE value_blob IS NULL AND lower(key) IN ({ph}) \
                     ORDER BY track_id, key, ordinal"
                )
            },
            |rows| collect_grouped_tags(rows, &mut out),
        )?;
        Ok(out)
    }

    /// Binary tag rows for a track: streaming handle (rowid), key, and payload
    /// length. Ordered by (key, ordinal) to match the layout builder's emission
    /// order. The blob bytes stream at read time; only `key` (materialized here)
    /// is length-guarded, plus the per-track row count.
    pub fn get_binary_tags(&self, track_id: i64) -> Result<Vec<BinaryTagRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT length(key), rowid, key, length(value_blob) FROM tags \
             WHERE track_id = ?1 AND value_blob IS NOT NULL ORDER BY key, ordinal",
        )?;
        let mut rows = stmt.query(params![track_id])?;
        let mut out = Vec::new();
        while let Some(r) = rows.next()? {
            check_field_len("tags", "key", r.get(0)?, MAX_TAG_KEY_LEN)?;
            out.push(BinaryTagRow {
                rowid: r.get(1)?,
                key: r.get(2)?,
                byte_len: r.get(3)?,
            });
            check_tag_count(track_id, out.len())?;
        }
        Ok(out)
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
        blob.read_at_exact(buf, crate::convert::usize_from(offset))?;
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

/// Replace a track's text-tag rows (`value_blob IS NULL`); binary rows are
/// untouched. Runs on `conn` so both `Db<ReadWrite>` (own transaction) and
/// `BulkWriter` (caller-held transaction) share one implementation.
pub(crate) fn replace_tags_in(
    conn: &rusqlite::Connection,
    track_id: i64,
    tags: &[Tag],
) -> Result<()> {
    conn.execute(
        "DELETE FROM tags WHERE track_id = ?1 AND value_blob IS NULL",
        params![track_id],
    )?;
    let mut stmt = conn.prepare_cached(
        "INSERT INTO tags (track_id, key, value, ordinal) VALUES (?1, ?2, ?3, ?4)",
    )?;
    for t in tags {
        stmt.execute(params![track_id, t.key, t.value, t.ordinal])?;
    }
    Ok(())
}

/// Replace a track's binary-tag rows (`value_blob IS NOT NULL`); text rows are
/// untouched. Binary rows store `''` in `value`. See `replace_tags_in` for the
/// shared-`conn` rationale.
pub(crate) fn set_binary_tags_in(
    conn: &rusqlite::Connection,
    track_id: i64,
    tags: &[BinaryTag],
) -> Result<()> {
    conn.execute(
        "DELETE FROM tags WHERE track_id = ?1 AND value_blob IS NOT NULL",
        params![track_id],
    )?;
    let mut stmt = conn.prepare_cached(
        "INSERT INTO tags (track_id, key, value, value_blob, ordinal) \
         VALUES (?1, ?2, '', ?3, ?4)",
    )?;
    for t in tags {
        stmt.execute(params![track_id, t.key, t.payload, t.ordinal])?;
    }
    Ok(())
}

impl Db<ReadWrite> {
    pub fn replace_tags(&self, track_id: i64, tags: &[Tag]) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        replace_tags_in(&tx, track_id, tags)?;
        tx.commit()?;
        Ok(())
    }

    /// Replace the track's binary tag rows (value_blob IS NOT NULL); text rows
    /// (managed by `replace_tags`) are untouched. Binary rows store '' in `value`.
    pub fn set_binary_tags(&self, track_id: i64, tags: &[BinaryTag]) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        set_binary_tags_in(&tx, track_id, tags)?;
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
            backing_mtime_ns: 0,
            backing_ctime_ns: 0,
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

    #[test]
    fn tags_grouped_for_keys_filters_case_insensitively() {
        let db = open_mem();
        let a = db.upsert_track(&new_track("/a.flac")).unwrap();
        db.replace_tags(
            a,
            &[
                Tag::new("ARTIST", "Pix", 0),
                Tag::new("Title", "Song", 0),
                Tag::new("LYRICS", "la la", 0),
            ],
        )
        .unwrap();
        let got = db.tags_grouped_for_keys(&["artist", "title"]).unwrap();
        let tags = &got[&a];
        assert!(tags.iter().any(|t| t.value == "Pix"), "ARTIST matched");
        assert!(tags.iter().any(|t| t.value == "Song"), "Title matched");
        assert!(!tags.iter().any(|t| t.value == "la la"), "LYRICS excluded");
    }

    #[test]
    fn get_tags_rejects_oversize_value() {
        let db = open_mem();
        let a = db.upsert_track(&new_track("/a.flac")).unwrap();
        db.conn
            .execute_batch("PRAGMA ignore_check_constraints=ON")
            .unwrap();
        let big = "v".repeat(262_145);
        db.conn
            .execute(
                "INSERT INTO tags (track_id, key, value, ordinal) VALUES (?1, 'k', ?2, 0)",
                rusqlite::params![a, big],
            )
            .unwrap();
        let err = db.get_tags(a).unwrap_err();
        assert!(
            matches!(err, crate::DbError::FieldTooLarge { field: "value", .. }),
            "{err:?}"
        );
    }

    #[test]
    fn get_tags_accepts_value_at_cap() {
        let db = open_mem();
        let a = db.upsert_track(&new_track("/a.flac")).unwrap();
        let at = "v".repeat(262_144);
        db.conn
            .execute(
                "INSERT INTO tags (track_id, key, value, ordinal) VALUES (?1, 'k', ?2, 0)",
                rusqlite::params![a, at],
            )
            .unwrap();
        assert_eq!(db.get_tags(a).unwrap()[0].value.len(), 262_144);
    }

    #[test]
    fn get_binary_tags_rejects_oversize_key() {
        let db = open_mem();
        let a = db.upsert_track(&new_track("/a.flac")).unwrap();
        db.conn
            .execute_batch("PRAGMA ignore_check_constraints=ON")
            .unwrap();
        let key = "k".repeat(257);
        db.conn
            .execute(
                "INSERT INTO tags (track_id, key, value, value_blob, ordinal) VALUES (?1, ?2, '', X'00', 0)",
                rusqlite::params![a, key],
            )
            .unwrap();
        let err = db.get_binary_tags(a).unwrap_err();
        assert!(
            matches!(
                err,
                crate::DbError::FieldTooLarge {
                    table: "tags",
                    field: "key",
                    ..
                }
            ),
            "{err:?}"
        );
    }

    #[test]
    fn per_track_count_cap_text_and_binary() {
        let db = open_mem();
        let a = db.upsert_track(&new_track("/a.flac")).unwrap();
        // 4097 text rows -> TooManyValues on get_tags.
        {
            let tx = db.conn.unchecked_transaction().unwrap();
            let mut stmt = tx
                .prepare(
                    "INSERT INTO tags (track_id, key, value, ordinal) VALUES (?1, 'k', 'v', ?2)",
                )
                .unwrap();
            for i in 0..4097 {
                stmt.execute(rusqlite::params![a, i]).unwrap();
            }
            drop(stmt);
            tx.commit().unwrap();
        }
        let err = db.get_tags(a).unwrap_err();
        assert!(
            matches!(err, crate::DbError::TooManyValues { .. }),
            "{err:?}"
        );
    }

    #[test]
    fn bulk_reader_rejects_one_oversized_track_in_batch() {
        let db = open_mem();
        let a = db.upsert_track(&new_track("/a.flac")).unwrap();
        let b = db.upsert_track(&new_track("/b.flac")).unwrap();
        db.replace_tags(b, &[Tag::new("ok", "fine", 0)]).unwrap();
        db.conn
            .execute_batch("PRAGMA ignore_check_constraints=ON")
            .unwrap();
        let big = "v".repeat(262_145);
        db.conn
            .execute(
                "INSERT INTO tags (track_id, key, value, ordinal) VALUES (?1, 'k', ?2, 0)",
                rusqlite::params![a, big],
            )
            .unwrap();
        let err = db.tags_for_tracks(&[a, b]).unwrap_err();
        assert!(
            matches!(err, crate::DbError::FieldTooLarge { field: "value", .. }),
            "{err:?}"
        );
    }

    #[test]
    fn tags_grouped_for_keys_empty_keys_is_empty_map() {
        let db = open_mem();
        let a = db.upsert_track(&new_track("/a.flac")).unwrap();
        db.replace_tags(a, &[Tag::new("ARTIST", "Pix", 0)]).unwrap();
        let got = db.tags_grouped_for_keys(&[]).unwrap();
        assert!(got.is_empty());
    }

    #[test]
    fn replace_tags_rejects_floor_violating_keys() {
        let db = open_mem();
        let t = db.upsert_track(&new_track("/a.flac")).unwrap();
        // A row violating the floor aborts the whole row-by-row transactional insert.
        assert!(db.replace_tags(t, &[Tag::new("", "v", 0)]).is_err());
        assert!(db.replace_tags(t, &[Tag::new("\u{7}", "v", 0)]).is_err());
        // '=' passes the DB floor (only the Vorbis path bars it).
        db.replace_tags(t, &[Tag::new("a=b", "c", 0)]).unwrap();
        let got = db.get_tags(t).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].key, "a=b");
    }

    #[test]
    fn replace_tags_rolls_back_a_mixed_valid_invalid_batch() {
        let db = open_mem();
        let t = db.upsert_track(&new_track("/a.flac")).unwrap();
        db.replace_tags(t, &[Tag::new("artist", "Alice", 0)])
            .unwrap();
        // replace_tags DELETEs the existing text rows before re-inserting; a CHECK
        // violation later in the batch must roll the whole transaction back —
        // including the DELETE — so the original rows survive rather than the batch
        // half-applying.
        assert!(
            db.replace_tags(t, &[Tag::new("title", "ok", 0), Tag::new("", "bad", 0)])
                .is_err()
        );
        let got = db.get_tags(t).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].key, "artist");
        assert_eq!(got[0].value, "Alice");
    }
}
