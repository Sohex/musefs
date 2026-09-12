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
    /// The store is older than this build and the next step up is gated: it
    /// rewrites data, needs disk headroom, or ends compatibility with older
    /// binaries, so it is not something to do as a side effect of `mount`
    /// (#706). The opposite direction from [`DbError::StoreTooNew`], and the
    /// opposite remedy — upgrade the store, not the binary — which is why the
    /// two are separate variants rather than one message.
    #[error(
        "store schema version {found} needs an explicit upgrade to version {target} \
         before this musefs build can open it; run `musefs migrate --db <store>`. \
         The upgrade rewrites the store in place and older musefs builds will no \
         longer open it, which is why it is not applied automatically"
    )]
    StoreNeedsMigration { found: i64, target: i64 },
    #[error("the store is in use — unmount the filesystem or stop any scan before vacuuming")]
    StoreInUse(#[source] rusqlite::Error),
    #[error("{table}.{field} is {len} {unit}, over the {max}-{unit} cap (crafted or corrupt DB)")]
    FieldTooLarge {
        table: &'static str,
        field: &'static str,
        len: i64,
        max: i64,
        /// `bytes` or `characters`. SQLite's `length()` counts characters on a
        /// TEXT column and bytes on a BLOB or a `CAST(... AS BLOB)`, and both
        /// projections guard the same fields (#693), so the message has to say
        /// which one it measured or it sends the reader counting the wrong
        /// thing. Mirrors `musefs_core::CoreError::TrackFieldTooLarge`.
        unit: &'static str,
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

/// Max bytes UTF-8 spends on one Unicode scalar value.
const MAX_UTF8_CHAR_BYTES: i64 = 4;

/// The byte ceiling implied by a cap of `max_chars` Unicode characters. No
/// honest value can exceed it, so it is a sound allocation bound to place
/// alongside the character cap without narrowing what the field may hold
/// (#693).
pub(crate) const fn max_utf8_bytes(max_chars: i64) -> i64 {
    max_chars.saturating_mul(MAX_UTF8_CHAR_BYTES)
}

/// Reject a field whose SQL-computed `length()` exceeds `max`, before the value
/// is ever materialized. Takes only the length, so by construction it cannot
/// touch the (potentially huge) payload — the allocation-free guarantee the
/// reader guards rely on (spec N13). The single `>` for every reader guard in
/// the crate lives here, so it is one mutation target rather than a dozen.
fn check_field_len(
    table: &'static str,
    field: &'static str,
    len: i64,
    max: i64,
    unit: &'static str,
) -> Result<()> {
    if len > max {
        return Err(DbError::FieldTooLarge {
            table,
            field,
            len,
            max,
            unit,
        });
    }
    Ok(())
}

/// Bound a `length(col)` projection over a TEXT column, which SQLite counts in
/// characters.
pub(crate) fn check_field_chars(
    table: &'static str,
    field: &'static str,
    len: i64,
    max: i64,
) -> Result<()> {
    check_field_len(table, field, len, max, "characters")
}

/// Bound a `length(col)` projection over a BLOB, or a `length(CAST(col AS
/// BLOB))` over TEXT — both of which SQLite counts in bytes.
pub(crate) fn check_field_bytes(
    table: &'static str,
    field: &'static str,
    len: i64,
    max: i64,
) -> Result<()> {
    check_field_len(table, field, len, max, "bytes")
}

/// Bound a character-capped TEXT field from *both* its projections before the
/// value is materialized (#693).
///
/// SQLite permits an embedded U+0000 in a TEXT value and stops counting at it,
/// so `length(col)` reports 1 for `"X\0"` followed by a hundred megabytes. The
/// character cap alone therefore guarantees neither the documented field
/// grammar nor, more importantly here, any bound at all on what
/// `Row::get::<String>` is about to allocate — rusqlite takes the column's byte
/// length, and an embedded NUL is valid UTF-8. Pairing the character cap with
/// [`max_utf8_bytes`] closes that without narrowing the field: the byte ceiling
/// is what the character cap already implies for honest UTF-8.
///
/// Checked character-first, so a plainly over-long field keeps reporting the
/// cap the schema states rather than its byte ceiling.
pub(crate) fn check_text_field(
    table: &'static str,
    field: &'static str,
    char_len: i64,
    byte_len: i64,
    max_chars: i64,
) -> Result<()> {
    check_field_chars(table, field, char_len, max_chars)?;
    check_field_bytes(table, field, byte_len, max_utf8_bytes(max_chars))
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
    use super::{check_field_bytes, check_field_chars, check_text_field, max_utf8_bytes};

    #[test]
    fn rejects_on_length_only_inclusive_boundary() {
        // The decision is a pure function of length — the value is never passed
        // in, so an over-cap row provably cannot be materialized to reject it.
        assert!(check_field_bytes("tags", "value", 262_145, 262_144).is_err());
        assert!(check_field_bytes("tags", "value", 262_144, 262_144).is_ok());
    }

    /// The unit reaches the message, so an operator reading it counts the thing
    /// that was actually measured (#693).
    #[test]
    fn the_message_names_the_unit_it_measured() {
        let chars = check_field_chars("tags", "key", 257, 256).unwrap_err();
        assert_eq!(
            chars.to_string(),
            "tags.key is 257 characters, over the 256-characters cap (crafted or corrupt DB)"
        );
        let bytes = check_field_bytes("tags", "value", 9, 8).unwrap_err();
        assert_eq!(
            bytes.to_string(),
            "tags.value is 9 bytes, over the 8-bytes cap (crafted or corrupt DB)"
        );
    }

    /// UTF-8 spends at most four bytes on a scalar value, so this is the widest
    /// an honestly-encoded character-capped field can be.
    #[test]
    fn the_byte_ceiling_is_four_times_the_character_cap() {
        assert_eq!(max_utf8_bytes(256), 1024);
        assert_eq!(max_utf8_bytes(0), 0);
        // Saturating, so a nonsense cap cannot wrap the ceiling negative and
        // turn the guard into a pass-through.
        assert_eq!(max_utf8_bytes(i64::MAX), i64::MAX);
    }

    #[test]
    fn text_field_bounds_both_projections_at_an_inclusive_boundary() {
        // 256 four-byte characters: at both caps, and accepted.
        assert!(check_text_field("tags", "key", 256, 1024, 256).is_ok());
        // One character over, whatever the byte count.
        assert!(check_text_field("tags", "key", 257, 257, 256).is_err());
        // Under the character cap but past the byte ceiling — the NUL-truncated
        // shape, which the character count alone cannot see.
        assert!(check_text_field("tags", "key", 1, 1025, 256).is_err());
        assert!(check_text_field("tags", "key", 1, 1024, 256).is_ok());
    }

    /// A field over both caps reports the character cap the schema states, not
    /// the byte ceiling derived from it.
    #[test]
    fn character_overflow_is_reported_before_byte_overflow() {
        let err = check_text_field("tags", "key", 300, 100_000, 256).unwrap_err();
        assert!(
            matches!(
                err,
                super::DbError::FieldTooLarge {
                    len: 300,
                    max: 256,
                    unit: "characters",
                    ..
                }
            ),
            "{err:?}"
        );
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
