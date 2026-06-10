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
}

pub type Result<T> = std::result::Result<T, DbError>;
