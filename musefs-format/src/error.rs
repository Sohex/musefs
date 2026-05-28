use thiserror::Error;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum FormatError {
    #[error("not a FLAC stream (missing fLaC marker)")]
    NotFlac,
    #[error("not an MP3 stream (no MPEG frame sync at the audio offset)")]
    NotMp3,
    #[error("truncated or malformed metadata")]
    Malformed,
    #[error("synthesized metadata exceeds the format's size limit")]
    TooLarge,
    #[error("not a supported MP4/M4A file")]
    NotMp4,
    #[error("not a supported WAV/RIFF file")]
    NotWav,
    #[error("synthesized region layout violates producer invariants")]
    InvalidLayout,
}

pub type Result<T> = std::result::Result<T, FormatError>;
