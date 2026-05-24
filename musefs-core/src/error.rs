use thiserror::Error;

#[derive(Debug, Error)]
pub enum CoreError {
    #[error(transparent)]
    Db(#[from] musefs_db::DbError),
    #[error(transparent)]
    Format(#[from] musefs_format::FormatError),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("backing file changed since scan: {0}")]
    BackingChanged(String),
    #[error("track {0} not found")]
    TrackNotFound(i64),
    #[error("no such inode: {0}")]
    NoEntry(u64),
    #[error("inode {0} is a directory")]
    IsDir(u64),
    #[error("embedded art is not supported in this milestone")]
    ArtNotSupported,
}

pub type Result<T> = std::result::Result<T, CoreError>;
