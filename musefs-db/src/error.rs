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
}
