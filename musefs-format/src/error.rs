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
    #[error("synthesized region layout violates producer invariants: {0}")]
    InvalidLayout(#[from] crate::layout::LayoutError),
    #[error("producer invariant violated: {0}")]
    ProducerBug(&'static str),
}

pub type Result<T> = std::result::Result<T, FormatError>;

#[cfg(test)]
mod tests {
    use super::FormatError;
    use crate::layout::LayoutError;

    #[test]
    fn invalid_layout_carries_inner_layout_error() {
        let e: FormatError = LayoutError::EmptySegment.into();
        assert!(matches!(
            e,
            FormatError::InvalidLayout(LayoutError::EmptySegment)
        ));
        // Display includes the inner reason, not just a generic string.
        assert!(e.to_string().contains("zero length"));
    }

    #[test]
    fn producer_bug_carries_reason() {
        let e = FormatError::ProducerBug("no leading Inline");
        assert!(e.to_string().contains("no leading Inline"));
    }
}
