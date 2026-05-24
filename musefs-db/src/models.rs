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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Track {
    pub id: i64,
    pub backing_path: String,
    pub format: Format,
    pub audio_offset: i64,
    pub audio_length: i64,
    pub backing_size: i64,
    pub backing_mtime: i64,
    pub content_version: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone)]
pub struct NewTrack {
    pub backing_path: String,
    pub format: Format,
    pub audio_offset: i64,
    pub audio_length: i64,
    pub backing_size: i64,
    pub backing_mtime: i64,
}

#[derive(Debug, Clone)]
pub struct NewArt {
    pub mime: String,
    pub width: Option<i64>,
    pub height: Option<i64>,
    pub data: Vec<u8>,
}
