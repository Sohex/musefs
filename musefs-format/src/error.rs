use thiserror::Error;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum FormatError {
    #[error("not a FLAC stream (missing fLaC marker)")]
    NotFlac,
    #[error("truncated or malformed FLAC metadata")]
    Malformed,
}

pub type Result<T> = std::result::Result<T, FormatError>;
