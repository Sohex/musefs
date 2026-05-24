use thiserror::Error;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum FormatError {
    #[error("not a FLAC stream (missing fLaC marker)")]
    NotFlac,
    #[error("not an MP3 stream (no MPEG frame sync at the audio offset)")]
    NotMp3,
    #[error("truncated or malformed metadata")]
    Malformed,
}

pub type Result<T> = std::result::Result<T, FormatError>;
