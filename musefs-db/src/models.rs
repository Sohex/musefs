#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "mutants", derive(Default))]
pub enum Format {
    #[cfg_attr(feature = "mutants", default)]
    Flac,
    Mp3,
    M4a,
    Opus,
    Vorbis,
    OggFlac,
    Wav,
}

impl Format {
    pub fn as_str(self) -> &'static str {
        match self {
            Format::Flac => "flac",
            Format::Mp3 => "mp3",
            Format::M4a => "m4a",
            Format::Opus => "opus",
            Format::Vorbis => "vorbis",
            Format::OggFlac => "oggflac",
            Format::Wav => "wav",
        }
    }

    pub fn parse(s: &str) -> Option<Format> {
        match s {
            "flac" => Some(Format::Flac),
            "mp3" => Some(Format::Mp3),
            "m4a" => Some(Format::M4a),
            "opus" => Some(Format::Opus),
            "vorbis" => Some(Format::Vorbis),
            "oggflac" => Some(Format::OggFlac),
            "wav" => Some(Format::Wav),
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

    #[test]
    fn ogg_codecs_round_trip() {
        for (f, s) in [
            (Format::Opus, "opus"),
            (Format::Vorbis, "vorbis"),
            (Format::OggFlac, "oggflac"),
        ] {
            assert_eq!(f.as_str(), s);
            assert_eq!(Format::parse(s), Some(f));
        }
    }

    #[test]
    fn wav_round_trips() {
        assert_eq!(Format::Wav.as_str(), "wav");
        assert_eq!(Format::parse("wav"), Some(Format::Wav));
    }
}

#[cfg(test)]
mod binary_tag_models_tests {
    #[test]
    fn binary_tag_constructs() {
        let bt = super::BinaryTag {
            key: "PRIV".to_string(),
            payload: vec![1, 2, 3],
            ordinal: 0,
        };
        assert_eq!(bt.payload.len(), 3);
        let row = super::BinaryTagRow {
            rowid: 7,
            key: "PRIV".to_string(),
            byte_len: 3,
        };
        assert_eq!(row.rowid, 7);
        let sb = super::StructuralBlock {
            kind: "STREAMINFO".to_string(),
            ordinal: 0,
            body: vec![0u8; 34],
        };
        assert_eq!(sb.body.len(), 34);
    }
}

#[cfg_attr(feature = "mutants", derive(Default))]
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

#[cfg_attr(feature = "mutants", derive(Default))]
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

#[cfg_attr(feature = "mutants", derive(Default))]
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

#[cfg_attr(feature = "mutants", derive(Default))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtMeta {
    pub mime: String,
    pub width: Option<i64>,
    pub height: Option<i64>,
    pub byte_len: i64,
}

#[cfg_attr(feature = "mutants", derive(Default))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackArt {
    pub art_id: i64,
    pub picture_type: i64,
    pub description: String,
    pub ordinal: i64,
}

/// A binary tag payload to write (e.g. an opaque ID3 `PRIV` frame body). `key` is
/// the format-private identifier (ID3 frame id, `APPLICATION`/`CUESHEET`,
/// `----:<mean>:<name>`); `payload` is the post-header frame/block body.
#[cfg_attr(feature = "mutants", derive(Default))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BinaryTag {
    pub key: String,
    pub payload: Vec<u8>,
    pub ordinal: i64,
}

/// A binary tag row read back for synthesis: the streaming handle (`rowid`), the
/// key, and the payload length — the bytes themselves stream at read time.
#[cfg_attr(feature = "mutants", derive(Default))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BinaryTagRow {
    pub rowid: i64,
    pub key: String,
    pub byte_len: i64,
}

/// A read-only structural metadata block derived from the backing file
/// (FLAC `STREAMINFO`/`SEEKTABLE`). Stored outside the editable `tags` contract.
#[cfg_attr(feature = "mutants", derive(Default))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StructuralBlock {
    pub kind: String,
    pub ordinal: i64,
    pub body: Vec<u8>,
}
