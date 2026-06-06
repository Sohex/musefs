use strum::{EnumIter, EnumString, IntoStaticStr};

/// The DB text representation (the `tracks.format` column) is derived:
/// `serialize_all = "lowercase"` lowercases the whole variant ident
/// (`OggFlac` → `"oggflac"`). The strings are an external contract —
/// beets/Picard write them — pinned by `tests::db_strings_are_pinned`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, EnumString, IntoStaticStr, EnumIter)]
#[strum(serialize_all = "lowercase")]
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
        self.into()
    }
}

#[cfg(test)]
mod tests {
    use super::Format;
    use strum::IntoEnumIterator;

    #[test]
    fn every_format_round_trips() {
        for f in Format::iter() {
            assert_eq!(f.as_str().parse::<Format>(), Ok(f));
        }
    }

    /// The strings are a DB contract — external writers (beets/Picard) store
    /// them. A variant rename must not silently change the stored string.
    #[test]
    fn db_strings_are_pinned() {
        let expected = [
            (Format::Flac, "flac"),
            (Format::Mp3, "mp3"),
            (Format::M4a, "m4a"),
            (Format::Opus, "opus"),
            (Format::Vorbis, "vorbis"),
            (Format::OggFlac, "oggflac"),
            (Format::Wav, "wav"),
        ];
        assert_eq!(expected.len(), Format::iter().count());
        for (f, s) in expected {
            assert_eq!(f.as_str(), s);
        }
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
    pub audio_offset: u64,
    pub audio_length: u64,
    pub backing_size: u64,
    pub backing_mtime: i64,
    pub content_version: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone)]
pub struct NewTrack {
    pub backing_path: String,
    pub format: Format,
    pub audio_offset: u64,
    pub audio_length: u64,
    pub backing_size: u64,
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
