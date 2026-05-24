#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Flac,
    Mp3,
}

impl Format {
    pub fn as_str(self) -> &'static str {
        match self {
            Format::Flac => "flac",
            Format::Mp3 => "mp3",
        }
    }

    pub fn parse(s: &str) -> Option<Format> {
        match s {
            "flac" => Some(Format::Flac),
            "mp3" => Some(Format::Mp3),
            _ => None,
        }
    }
}
