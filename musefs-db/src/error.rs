use thiserror::Error;

#[derive(Debug, Error)]
pub enum DbError {
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
    #[error(
        "audio bounds out of range: offset {audio_offset} + length {audio_length} exceeds backing_size {backing_size}"
    )]
    AudioBoundsOutOfRange {
        audio_offset: u64,
        audio_length: u64,
        backing_size: u64,
    },
    #[error(
        "database schema does not match the version musefs expects (mismatch at {object}); \
         regenerate the store by running `musefs scan` against the library"
    )]
    SchemaMismatch { object: String },
    #[error(
        "store schema version {found} is newer than this musefs build supports \
         (max {supported}); upgrade musefs to read this store"
    )]
    StoreTooNew { found: i64, supported: i64 },
    #[error("the store is in use — unmount the filesystem or stop any scan before vacuuming")]
    StoreInUse(#[source] rusqlite::Error),
    #[error("{table}.{field} length {len} exceeds the {max} cap (crafted or corrupt DB)")]
    FieldTooLarge {
        table: &'static str,
        field: &'static str,
        len: i64,
        max: i64,
    },
    #[error("structural block for track {track_id} is invalid: {detail} (crafted or corrupt DB)")]
    InvalidStructuralBlock { track_id: i64, detail: String },
    #[error(
        "track {track_id} has {count} tag rows, exceeds the {max}-row cap (crafted or corrupt DB)"
    )]
    TooManyValues {
        track_id: i64,
        count: usize,
        max: usize,
    },
    #[error(
        "track {track_id} has {count} track_art rows, exceeds the {max}-row cap (crafted or corrupt DB)"
    )]
    TooManyArtRows {
        track_id: i64,
        count: usize,
        max: usize,
    },
}

impl DbError {
    /// Is this error a property of the rows the caller just tried to write,
    /// rather than of the store as a whole?
    ///
    /// True only for a SQLite constraint violation (`SQLITE_CONSTRAINT`, every
    /// extended code): a `CHECK`, `UNIQUE`, primary-key, `NOT NULL`,
    /// foreign-key or `RAISE(ABORT)` failure is decided by the values in the
    /// statement, so a caller writing one item at a time can fail that item and
    /// keep going (#662).
    ///
    /// An allowlist rather than a list of fatal codes, deliberately: a store
    /// that is corrupt, full, read-only, not a database, or failing I/O must
    /// keep aborting the run, and so must any code this build has never seen.
    /// `SQLITE_BUSY` is likewise excluded — locking is the writer's retry
    /// policy to own, not a row-level rejection.
    pub fn is_constraint_violation(&self) -> bool {
        matches!(
            self,
            DbError::Sqlite(rusqlite::Error::SqliteFailure(e, _))
                if e.code == rusqlite::ErrorCode::ConstraintViolation
        )
    }
}

pub type Result<T> = std::result::Result<T, DbError>;

/// Reject a field whose SQL-computed `length()` exceeds `max`, before the value
/// is ever materialized. Takes only the length, so by construction it cannot
/// touch the (potentially huge) payload — the allocation-free guarantee the
/// reader guards rely on (spec N13).
pub(crate) fn check_field_len(
    table: &'static str,
    field: &'static str,
    len: i64,
    max: i64,
) -> Result<()> {
    if len > max {
        return Err(DbError::FieldTooLarge {
            table,
            field,
            len,
            max,
        });
    }
    Ok(())
}

/// Reject a track whose materialized tag-row count exceeds the per-track cap.
/// Centralizing the comparison keeps a single boundary site (one mutation
/// target) shared by every tag reader, instead of one per reader.
pub(crate) fn check_tag_count(track_id: i64, count: usize) -> Result<()> {
    if count > crate::limits::MAX_TAGS_PER_TRACK {
        return Err(DbError::TooManyValues {
            track_id,
            count,
            max: crate::limits::MAX_TAGS_PER_TRACK,
        });
    }
    Ok(())
}

/// Reject a track whose materialized `track_art` row count exceeds the per-track
/// cap. There is a single art reader (`get_track_art`), so this helper is not
/// about sharing across callers the way `check_tag_count` is; it exists for
/// fidelity with that pattern and to keep the single `>` comparison as one
/// mutation-gate target.
pub(crate) fn check_art_count(track_id: i64, count: usize) -> Result<()> {
    if count > crate::limits::MAX_ART_ROWS_PER_TRACK {
        return Err(DbError::TooManyArtRows {
            track_id,
            count,
            max: crate::limits::MAX_ART_ROWS_PER_TRACK,
        });
    }
    Ok(())
}

#[cfg(test)]
mod guard_helper_tests {
    use super::check_field_len;

    #[test]
    fn rejects_on_length_only_inclusive_boundary() {
        // The decision is a pure function of length — the value is never passed
        // in, so an over-cap row provably cannot be materialized to reject it.
        assert!(check_field_len("tags", "value", 262_145, 262_144).is_err());
        assert!(check_field_len("tags", "value", 262_144, 262_144).is_ok());
    }

    #[test]
    fn tag_count_accepts_at_cap_rejects_above() {
        use crate::limits::MAX_TAGS_PER_TRACK;
        // Boundary is inclusive: exactly the cap is accepted, one over rejected.
        // Pins the single `>` site so a `>`→`>=`/`==` mutant cannot survive.
        assert!(super::check_tag_count(1, MAX_TAGS_PER_TRACK).is_ok());
        assert!(super::check_tag_count(1, MAX_TAGS_PER_TRACK + 1).is_err());
    }

    #[test]
    fn art_count_accepts_at_cap_rejects_above() {
        use crate::limits::MAX_ART_ROWS_PER_TRACK;
        // Boundary is inclusive: exactly the cap is accepted, one over rejected.
        // Pins the single `>` site so a `>`→`>=`/`==` mutant cannot survive.
        assert!(super::check_art_count(1, MAX_ART_ROWS_PER_TRACK).is_ok());
        assert!(super::check_art_count(1, MAX_ART_ROWS_PER_TRACK + 1).is_err());
    }
}

#[cfg(test)]
mod classification_tests {
    use crate::DbError;

    /// Every code the scan is required to keep aborting on (#662), by their
    /// primary SQLite result codes.
    const FATAL_CODES: [(&str, i32); 6] = [
        ("SQLITE_CORRUPT", rusqlite::ffi::SQLITE_CORRUPT),
        ("SQLITE_FULL", rusqlite::ffi::SQLITE_FULL),
        ("SQLITE_IOERR", rusqlite::ffi::SQLITE_IOERR),
        ("SQLITE_READONLY", rusqlite::ffi::SQLITE_READONLY),
        ("SQLITE_NOTADB", rusqlite::ffi::SQLITE_NOTADB),
        // Locking is the writer's retry policy to own, not a row rejection.
        ("SQLITE_BUSY", rusqlite::ffi::SQLITE_BUSY),
    ];

    fn sqlite_failure(code: i32) -> DbError {
        DbError::Sqlite(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(code),
            None,
        ))
    }

    #[test]
    fn a_real_unique_violation_is_attributed_to_the_rows() {
        // Driven through the schema rather than a constructed error: what the
        // classifier has to recognise is whatever SQLite actually raises for a
        // primary-key collision on `tags` — the shape #659 hit.
        let db = crate::Db::open_in_memory().unwrap();
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
        let insert = "INSERT INTO tags (track_id, key, ordinal, value) \
                      VALUES (?1, 'title', 0, 'x')";
        db.conn.execute(insert, [tid]).unwrap();
        let err: DbError = db.conn.execute(insert, [tid]).unwrap_err().into();
        assert!(
            err.is_constraint_violation(),
            "a primary-key collision must be attributable to the rows: {err}"
        );
    }

    #[test]
    fn extended_constraint_codes_are_all_recognised() {
        // The classifier keys on the primary code, so every extended
        // `SQLITE_CONSTRAINT_*` must classify the same way — that is the point
        // of not enumerating causes one at a time.
        for code in [
            rusqlite::ffi::SQLITE_CONSTRAINT,
            rusqlite::ffi::SQLITE_CONSTRAINT_CHECK,
            rusqlite::ffi::SQLITE_CONSTRAINT_PRIMARYKEY,
            rusqlite::ffi::SQLITE_CONSTRAINT_UNIQUE,
            rusqlite::ffi::SQLITE_CONSTRAINT_NOTNULL,
            rusqlite::ffi::SQLITE_CONSTRAINT_FOREIGNKEY,
            rusqlite::ffi::SQLITE_CONSTRAINT_TRIGGER,
        ] {
            assert!(
                sqlite_failure(code).is_constraint_violation(),
                "extended code {code} must classify as a constraint violation"
            );
        }
    }

    #[test]
    fn store_wide_failures_are_not_attributed_to_the_rows() {
        for (name, code) in FATAL_CODES {
            let err = sqlite_failure(code);
            assert!(
                !err.is_constraint_violation(),
                "{name} condemns the run, not one item"
            );
        }
    }

    #[test]
    fn non_sqlite_errors_are_not_constraint_violations() {
        // The allowlist is on the SQLite code alone: musefs's own guard errors
        // are raised by readers against a crafted store, so they say nothing
        // about one write's rows.
        assert!(
            !DbError::SchemaMismatch {
                object: "tags".into()
            }
            .is_constraint_violation()
        );
        assert!(
            !DbError::Sqlite(rusqlite::Error::QueryReturnedNoRows).is_constraint_violation(),
            "a rusqlite error carrying no SQLite code cannot be a constraint violation"
        );
    }
}
