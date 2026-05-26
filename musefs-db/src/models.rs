#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Flac,
    Mp3,
    M4a,
}

impl Format {
    pub fn as_str(self) -> &'static str {
        match self {
            Format::Flac => "flac",
            Format::Mp3 => "mp3",
            Format::M4a => "m4a",
        }
    }

    pub fn parse(s: &str) -> Option<Format> {
        match s {
            "flac" => Some(Format::Flac),
            "mp3" => Some(Format::Mp3),
            "m4a" => Some(Format::M4a),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Format;

    #[test]
    fn m4a_round_trips() {
        assert_eq!(Format::M4a.as_str(), "m4a");
        assert_eq!(Format::parse("m4a"), Some(Format::M4a));
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tag {
    pub key: String,
    pub value: String,
    pub ordinal: i64,
}

impl Tag {
    pub fn new(key: &str, value: &str, ordinal: i64) -> Tag {
        Tag {
            key: key.to_string(),
            value: value.to_string(),
            ordinal,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Art {
    pub id: i64,
    pub sha256: String,
    pub mime: String,
    pub width: Option<i64>,
    pub height: Option<i64>,
    pub byte_len: i64,
    pub data: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtMeta {
    pub mime: String,
    pub width: Option<i64>,
    pub height: Option<i64>,
    pub byte_len: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackArt {
    pub art_id: i64,
    pub picture_type: i64,
    pub description: String,
    pub ordinal: i64,
}
