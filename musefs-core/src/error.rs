use thiserror::Error;

#[derive(Debug, Error)]
pub enum CoreError {
    #[error(transparent)]
    Db(#[from] musefs_db::DbError),
    #[error("failed to open database at {path}")]
    DbOpen {
        path: std::path::PathBuf,
        #[source]
        source: musefs_db::DbError,
    },
    #[error(transparent)]
    Format(#[from] musefs_format::FormatError),
    #[error("MP4 {box_kind} box is {size} bytes, exceeds the {cap}-byte metadata cap")]
    Mp4MetadataTooLarge {
        box_kind: &'static str,
        size: u64,
        cap: u64,
    },
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("backing file changed since scan: {0}")]
    BackingChanged(String),
    #[error(
        "track {track_id} references art {art_id}, which has no metadata row (orphaned track_art — DB contract violation)"
    )]
    OrphanedArt { track_id: i64, art_id: i64 },
    #[error(
        "track {track_id} art {art_id} has out-of-range picture_type {value} (expected 0..=20)"
    )]
    InvalidPictureType {
        track_id: i64,
        art_id: i64,
        value: u32,
    },
    #[error("track {0} not found")]
    TrackNotFound(i64),
    #[error("no such inode: {0}")]
    NoEntry(u64),
    #[error("inode {0} is a directory")]
    IsDir(u64),
    #[error("inode {0} is not a directory")]
    NotADir(u64),
    #[error("handle table full")]
    HandleTableFull,
}

pub type Result<T> = std::result::Result<T, CoreError>;
