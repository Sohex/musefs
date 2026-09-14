use thiserror::Error;

#[derive(Debug, Error)]
#[non_exhaustive]
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
    #[error(transparent)]
    InvalidTemplate(#[from] crate::template::TemplateError),
    #[error("MP4 {box_kind} box is {size} bytes, exceeds the {cap}-byte metadata cap")]
    Mp4MetadataTooLarge {
        box_kind: &'static str,
        size: u64,
        cap: u64,
    },
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("backing file I/O at {path}: {source}")]
    BackingIo {
        path: std::path::PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("backing file changed since scan: {0}")]
    BackingChanged(std::path::PathBuf),
    /// Derived state this build had cached no longer describes the store, so
    /// the caller should rebuild or re-resolve rather than serve what it holds.
    ///
    /// Split from [`CoreError::BackingChanged`] when that variant took a real
    /// path (#680). The two travel together everywhere it matters — the same
    /// errno, the same retry, the same attr-cache drop — but they are not the
    /// same claim: this one is about musefs's own derived state, names no file,
    /// and had been borrowing `BackingChanged`'s `String` payload to carry a
    /// sentence. Once that payload became a path there was nothing honest to
    /// put in it.
    #[error("derived state is stale: {0}")]
    DerivedStateStale(String),
    #[error("{path}: {item} is {len} {unit}, over musefs's limit of {cap} {unit}")]
    TrackFieldTooLarge {
        path: std::path::PathBuf,
        /// What was too big, in the user's terms — `tag "LYRICS"`,
        /// `binary tag "GEOB"`, `embedded image/jpeg art`, `art description`.
        item: String,
        len: u64,
        cap: u64,
        /// `bytes` or `characters` — the schema `CHECK`s count TEXT columns in
        /// characters and blobs/`CAST(... AS BLOB)` in bytes, and a message that
        /// quotes the wrong unit sends the user measuring the wrong thing.
        unit: &'static str,
    },
    #[error(
        "{path}: its {format} tag block would be {len} bytes, over the {cap}-byte \
         {format} metadata limit — musefs could not serve this file"
    )]
    TrackMetadataTooLarge {
        path: std::path::PathBuf,
        format: &'static str,
        len: u64,
        cap: u64,
    },
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
    #[error("track {track_id} art {art_id} is {byte_len} bytes, exceeds the {cap}-byte art cap")]
    ArtTooLarge {
        track_id: i64,
        art_id: i64,
        byte_len: u64,
        cap: u64,
    },
    #[error("front/header read of {requested} bytes exceeds the {cap}-byte serve cap")]
    HeaderTooLarge { requested: u64, cap: u64 },
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

impl CoreError {
    /// Attach the backing-file path to an I/O error. A moved, permission-denied,
    /// or failing backing file is the most common passthrough failure; carrying
    /// the path makes the warn log and any surfaced error name the file rather
    /// than collapsing to a bare `EIO` (#521).
    pub(crate) fn backing_io(path: impl AsRef<std::path::Path>, source: std::io::Error) -> Self {
        CoreError::BackingIo {
            path: path.as_ref().to_path_buf(),
            source,
        }
    }
}

pub type Result<T> = std::result::Result<T, CoreError>;

impl CoreError {
    /// One value of every variant, for a test that has to cover them all
    /// (#708).
    ///
    /// `CoreError` is `#[non_exhaustive]`, so no other crate can match it
    /// without a wildcard, and a variant added here would reach that wildcard
    /// unnoticed. Handing such a test one of each is the stand-in for the
    /// exhaustive match it lost; `every_variant_is_sampled_exactly_once` keeps
    /// the list whole.
    #[cfg(any(test, feature = "test-support"))]
    #[must_use]
    pub fn every_variant_for_test() -> Vec<CoreError> {
        let too_new = || musefs_db::DbError::StoreTooNew {
            found: 9,
            supported: 4,
        };
        vec![
            CoreError::Db(too_new()),
            CoreError::DbOpen {
                path: "store.db".into(),
                source: too_new(),
            },
            CoreError::Format(musefs_format::FormatError::NotFlac),
            CoreError::InvalidTemplate(crate::template::TemplateError::UnterminatedField),
            CoreError::Mp4MetadataTooLarge {
                box_kind: "moov",
                size: 2,
                cap: 1,
            },
            // Real OS errnos (EACCES, ENOENT), so a test can tell a
            // passed-through errno from a collapse to EIO.
            CoreError::Io(std::io::Error::from_raw_os_error(13)),
            CoreError::BackingIo {
                path: "a.flac".into(),
                source: std::io::Error::from_raw_os_error(2),
            },
            CoreError::BackingChanged("a.flac".into()),
            CoreError::DerivedStateStale("stale".into()),
            CoreError::TrackFieldTooLarge {
                path: "a.flac".into(),
                item: "tag \"LYRICS\"".into(),
                len: 2,
                cap: 1,
                unit: "bytes",
            },
            CoreError::TrackMetadataTooLarge {
                path: "a.flac".into(),
                format: "FLAC",
                len: 2,
                cap: 1,
            },
            CoreError::OrphanedArt {
                track_id: 1,
                art_id: 2,
            },
            CoreError::InvalidPictureType {
                track_id: 1,
                art_id: 2,
                value: 99,
            },
            CoreError::ArtTooLarge {
                track_id: 1,
                art_id: 2,
                byte_len: 2,
                cap: 1,
            },
            CoreError::HeaderTooLarge {
                requested: 2,
                cap: 1,
            },
            CoreError::TrackNotFound(1),
            CoreError::NoEntry(1),
            CoreError::IsDir(1),
            CoreError::NotADir(1),
            CoreError::HandleTableFull,
        ]
    }
}

#[cfg(test)]
mod variant_sample_tests {
    use super::CoreError;

    /// How many variants `CoreError` has.
    const VARIANTS: usize = 20;

    /// A variant's place in declaration order. Exhaustive, with no wildcard,
    /// so a new variant does not compile until it is given a place here; giving
    /// it one past [`VARIANTS`] panics below until the count grows, and growing
    /// the count fails the test until the sample list carries the new variant.
    fn variant_index(err: &CoreError) -> usize {
        match err {
            CoreError::Db(_) => 0,
            CoreError::DbOpen { .. } => 1,
            CoreError::Format(_) => 2,
            CoreError::InvalidTemplate(_) => 3,
            CoreError::Mp4MetadataTooLarge { .. } => 4,
            CoreError::Io(_) => 5,
            CoreError::BackingIo { .. } => 6,
            CoreError::BackingChanged(_) => 7,
            CoreError::DerivedStateStale(_) => 8,
            CoreError::TrackFieldTooLarge { .. } => 9,
            CoreError::TrackMetadataTooLarge { .. } => 10,
            CoreError::OrphanedArt { .. } => 11,
            CoreError::InvalidPictureType { .. } => 12,
            CoreError::ArtTooLarge { .. } => 13,
            CoreError::HeaderTooLarge { .. } => 14,
            CoreError::TrackNotFound(_) => 15,
            CoreError::NoEntry(_) => 16,
            CoreError::IsDir(_) => 17,
            CoreError::NotADir(_) => 18,
            CoreError::HandleTableFull => 19,
        }
    }

    #[test]
    fn every_variant_is_sampled_exactly_once() {
        let mut seen = [0usize; VARIANTS];
        for err in CoreError::every_variant_for_test() {
            seen[variant_index(&err)] += 1;
        }
        assert!(
            seen.iter().all(|&n| n == 1),
            "every CoreError variant must appear exactly once in \
             every_variant_for_test, by declaration index: {seen:?}"
        );
    }
}
