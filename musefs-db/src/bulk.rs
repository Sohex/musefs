use crate::art::{set_track_art_in, upsert_art_in};
use crate::models::{
    BinaryTag, ChecksumWrite, NewArt, NewTrack, StructuralBlock, Tag, Track, TrackArt,
};
use crate::structural::set_structural_blocks_in;
use crate::tags::{replace_tags_in, set_binary_tags_in};
use crate::tracks::{
    get_track_by_path_in, retarget_track_in, set_track_checksums_in, tracks_by_fingerprint_in,
    upsert_track_in,
};
use crate::{Db, DbError, ReadWrite, Result};
use rusqlite::Transaction;

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

/// A batch of track writes held in one transaction. Each method delegates to the
/// shared per-row writer helper (`upsert_track_in` / `replace_tags_in` /
/// `set_binary_tags_in` / `set_structural_blocks_in` / `upsert_art_in` /
/// `set_track_art_in`) that also backs the `Db<ReadWrite>` writers, but runs it
/// on a single caller-held transaction so a whole batch commits with one fsync.
pub struct BulkWriter<'c> {
    tx: Transaction<'c>,
}

impl BulkWriter<'_> {
    pub fn upsert_track(&mut self, t: &NewTrack) -> Result<i64> {
        upsert_track_in(&self.tx, t)
    }

    pub fn tracks_by_fingerprint(&self, fp: &str) -> Result<Vec<Track>> {
        tracks_by_fingerprint_in(&self.tx, fp)
    }

    pub fn set_track_checksums(
        &self,
        id: i64,
        fingerprint: ChecksumWrite<'_>,
        content_hash: ChecksumWrite<'_>,
    ) -> Result<()> {
        set_track_checksums_in(&self.tx, id, fingerprint, content_hash)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn retarget_track(
        &self,
        id: i64,
        new_backing_path: &std::path::Path,
        backing_size: u64,
        backing_mtime_ns: i64,
        backing_ctime_ns: i64,
        backing_ino: Option<u64>,
        audio_offset: u64,
        audio_length: u64,
        fingerprint: ChecksumWrite<'_>,
        content_hash: ChecksumWrite<'_>,
    ) -> Result<()> {
        retarget_track_in(
            &self.tx,
            id,
            new_backing_path,
            backing_size,
            backing_mtime_ns,
            backing_ctime_ns,
            backing_ino,
            audio_offset,
            audio_length,
            fingerprint,
            content_hash,
        )
    }

    pub fn get_track_by_path(&self, path: &std::path::Path) -> Result<Option<Track>> {
        get_track_by_path_in(&self.tx, path)
    }

    pub fn replace_tags(&mut self, track_id: i64, tags: &[Tag]) -> Result<()> {
        replace_tags_in(&self.tx, track_id, tags)
    }

    pub fn set_binary_tags(&mut self, track_id: i64, tags: &[BinaryTag]) -> Result<()> {
        set_binary_tags_in(&self.tx, track_id, tags)
    }

    pub fn set_structural_blocks(
        &mut self,
        track_id: i64,
        blocks: &[StructuralBlock],
    ) -> Result<()> {
        set_structural_blocks_in(&self.tx, track_id, blocks)
    }

    pub fn upsert_art(&mut self, a: &NewArt) -> Result<i64> {
        upsert_art_in(&self.tx, a)
    }

    pub fn set_track_art(&mut self, track_id: i64, items: &[TrackArt]) -> Result<()> {
        set_track_art_in(&self.tx, track_id, items)
    }

    /// Run one item's writes inside a `SAVEPOINT`, so an error discards only
    /// the rows that item wrote and leaves the rest of the batch live and
    /// committable. The caller decides which errors are worth failing a single
    /// item over — [`DbError::is_constraint_violation`] draws that line (#662).
    ///
    /// Generic over the closure's error so callers above this crate can keep
    /// their own error type, as long as it carries a [`DbError`]. A rollback
    /// that itself fails is returned in place of the error that provoked it:
    /// the transaction's state is then unknown, which is the more serious fact.
    ///
    /// One fixed savepoint name, so this does not nest — a nested call would
    /// silently release the outer scope. The scan's one-file-at-a-time ingest
    /// needs no nesting.
    pub fn item<T, E>(
        &mut self,
        f: impl FnOnce(&mut Self) -> std::result::Result<T, E>,
    ) -> std::result::Result<T, E>
    where
        E: From<DbError>,
    {
        self.batch("SAVEPOINT musefs_item")?;
        match f(self) {
            Ok(v) => {
                self.batch("RELEASE musefs_item")?;
                Ok(v)
            }
            Err(e) => {
                self.batch("ROLLBACK TO musefs_item; RELEASE musefs_item")?;
                Err(e)
            }
        }
    }

    /// One savepoint statement, with its error mapped into the caller's type.
    fn batch<E: From<DbError>>(&self, sql: &str) -> std::result::Result<(), E> {
        self.tx
            .execute_batch(sql)
            .map_err(|e| E::from(DbError::from(e)))
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
                        backing_path: std::path::PathBuf::from(format!("/m/{i}.flac")),
                        format: Format::Flac,
                        audio_offset: 100,
                        audio_length: 200,
                        backing_size: 300,
                        backing_mtime_ns: 1,
                        backing_ctime_ns: 0,
                        backing_ino: None,
                    })
                    .unwrap();
                bw.replace_tags(id, &[Tag::new("title", &format!("t{i}"), 0)])
                    .unwrap();
                let art_id = bw
                    .upsert_art(&NewArt {
                        data: vec![1, 2, 3, 4],
                    })
                    .unwrap();
                bw.set_track_art(
                    id,
                    &[TrackArt {
                        art_id,
                        picture_type: 3,
                        description: String::new(),
                        mime: "image/png".into(),
                        width: None,
                        height: None,
                        depth: 0,
                        colors: 0,
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

    fn new_track(path: &str) -> NewTrack {
        NewTrack {
            backing_path: path.into(),
            format: Format::Flac,
            audio_offset: 0,
            audio_length: 0,
            backing_size: 0,
            backing_mtime_ns: 0,
            backing_ctime_ns: 0,
            backing_ino: None,
        }
    }

    /// The point of the savepoint: a failed item takes its own rows with it and
    /// nothing else, and the batch is still committable afterwards (#662).
    #[test]
    fn item_rolls_back_only_the_failing_item_and_leaves_the_batch_committable() {
        let db = Db::open_in_memory().unwrap();
        {
            let mut bw = db.bulk_writer().unwrap();
            bw.upsert_track(&new_track("/m/before.flac")).unwrap();

            let err = bw
                .item(|bw| -> crate::Result<()> {
                    // A real write, then a failure after it: the first has to be
                    // undone, which a plain early return could not do.
                    bw.upsert_track(&new_track("/m/doomed.flac"))?;
                    Err(crate::DbError::Sqlite(rusqlite::Error::QueryReturnedNoRows))
                })
                .expect_err("the closure's error must propagate");
            assert!(matches!(err, crate::DbError::Sqlite(_)), "{err}");

            bw.upsert_track(&new_track("/m/after.flac")).unwrap();
            bw.commit().unwrap();
        }
        let paths: Vec<std::path::PathBuf> = db
            .list_tracks()
            .unwrap()
            .into_iter()
            .map(|t| t.backing_path)
            .collect();
        assert_eq!(
            paths.len(),
            2,
            "the doomed item must leave nothing: {paths:?}"
        );
        assert!(
            !paths.iter().any(|p| p.to_string_lossy().contains("doomed")),
            "{paths:?}"
        );
    }

    /// A successful item releases its savepoint rather than rolling it back, and
    /// the release must not undo the writes.
    #[test]
    fn item_keeps_the_writes_of_a_successful_item() {
        let db = Db::open_in_memory().unwrap();
        {
            let mut bw = db.bulk_writer().unwrap();
            let id = bw
                .item(|bw| bw.upsert_track(&new_track("/m/kept.flac")))
                .unwrap();
            bw.replace_tags(id, &[Tag::new("title", "kept", 0)])
                .unwrap();
            bw.commit().unwrap();
        }
        assert_eq!(db.list_tracks().unwrap().len(), 1);
    }

    /// Back-to-back calls: the fixed savepoint name is reused, so a released or
    /// rolled-back scope must leave nothing behind that breaks the next one.
    #[test]
    fn item_is_reusable_across_a_run_of_items() {
        let db = Db::open_in_memory().unwrap();
        {
            let mut bw = db.bulk_writer().unwrap();
            for i in 0..4 {
                let path = format!("/m/{i}.flac");
                let outcome = bw.item(|bw| -> crate::Result<()> {
                    bw.upsert_track(&new_track(&path))?;
                    // Every other item fails, so releases and rollbacks alternate.
                    if i % 2 == 0 {
                        return Err(crate::DbError::Sqlite(rusqlite::Error::QueryReturnedNoRows));
                    }
                    Ok(())
                });
                assert_eq!(outcome.is_err(), i % 2 == 0, "item {i}");
            }
            bw.commit().unwrap();
        }
        let paths: Vec<std::path::PathBuf> = db
            .list_tracks()
            .unwrap()
            .into_iter()
            .map(|t| t.backing_path)
            .collect();
        assert_eq!(paths.len(), 2, "{paths:?}");
        assert!(
            paths
                .iter()
                .all(|p| p.ends_with("1.flac") || p.ends_with("3.flac")),
            "{paths:?}"
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
                backing_path: std::path::PathBuf::from("/a.mp3"),
                format: crate::Format::Mp3,
                audio_offset: 0,
                audio_length: 0,
                backing_size: 0,
                backing_mtime_ns: 0,
                backing_ctime_ns: 0,
                backing_ino: None,
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
                backing_path: std::path::PathBuf::from("/a.mp3"),
                format: crate::Format::Mp3,
                audio_offset: 0,
                audio_length: 0,
                backing_size: 0,
                backing_mtime_ns: 0,
                backing_ctime_ns: 0,
                backing_ino: None,
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
                backing_path: std::path::PathBuf::from("/m/ghost.flac"),
                format: Format::Flac,
                audio_offset: 0,
                audio_length: 0,
                backing_size: 0,
                backing_mtime_ns: 0,
                backing_ctime_ns: 0,
                backing_ino: None,
            })
            .unwrap();
            // Dropped here without `commit()` → Transaction rolls back.
        }
        assert_eq!(db.list_tracks().unwrap().len(), 0);
    }
}
