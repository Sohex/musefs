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

/// Validated audio-region bounds for a track: `audio_offset + audio_length`
/// is guaranteed to fit within `backing_size`, so the reader can splice the
/// audio region without re-checking. Built at the `tracks` row reader.
#[cfg_attr(feature = "mutants", derive(Default))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TrackBounds {
    audio_offset: u64,
    audio_length: u64,
}

impl TrackBounds {
    /// Err if `audio_offset + audio_length` overflows or exceeds `backing_size`.
    pub fn new(
        audio_offset: u64,
        audio_length: u64,
        backing_size: u64,
    ) -> Result<TrackBounds, crate::DbError> {
        let end = audio_offset
            .checked_add(audio_length)
            .filter(|&end| end <= backing_size)
            .ok_or(crate::DbError::AudioBoundsOutOfRange {
                audio_offset,
                audio_length,
                backing_size,
            })?;
        let _ = end;
        Ok(TrackBounds {
            audio_offset,
            audio_length,
        })
    }

    pub fn audio_offset(&self) -> u64 {
        self.audio_offset
    }

    pub fn audio_length(&self) -> u64 {
        self.audio_length
    }
}

#[cfg_attr(feature = "mutants", derive(Default))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Track {
    pub id: i64,
    pub backing_path: String,
    pub format: Format,
    pub bounds: TrackBounds,
    pub backing_size: u64,
    pub backing_mtime_ns: i64,
    pub backing_ctime_ns: i64,
    pub content_version: i64,
    pub updated_at: i64,
    pub fingerprint: Option<String>,
    pub content_hash: Option<String>,
}

#[derive(Debug, Clone)]
pub struct NewTrack {
    pub backing_path: String,
    pub format: Format,
    pub audio_offset: u64,
    pub audio_length: u64,
    pub backing_size: u64,
    pub backing_mtime_ns: i64,
    pub backing_ctime_ns: i64,
}

#[derive(Debug, Clone)]
pub struct NewArt {
    pub mime: String,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub data: Vec<u8>,
}

#[cfg_attr(feature = "mutants", derive(Default))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tag {
    pub key: String,
    pub value: String,
    pub ordinal: u64,
}

impl Tag {
    pub fn new(key: &str, value: &str, ordinal: u64) -> Tag {
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
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub byte_len: u64,
    pub data: Vec<u8>,
}

#[cfg_attr(feature = "mutants", derive(Default))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtMeta {
    pub mime: String,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub byte_len: u64,
}

#[cfg_attr(feature = "mutants", derive(Default))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackArt {
    pub art_id: i64,
    pub picture_type: u32,
    pub description: String,
    pub ordinal: u64,
}

/// A binary tag payload to write (e.g. an opaque ID3 `PRIV` frame body). `key` is
/// the format-private identifier (ID3 frame id, `APPLICATION`/`CUESHEET`,
/// `----:<mean>:<name>`); `payload` is the post-header frame/block body.
#[cfg_attr(feature = "mutants", derive(Default))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BinaryTag {
    pub key: String,
    pub payload: Vec<u8>,
    pub ordinal: u64,
}

/// A binary tag row read back for synthesis: the streaming handle (`rowid`), the
/// key, and the payload length — the bytes themselves stream at read time.
#[cfg_attr(feature = "mutants", derive(Default))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BinaryTagRow {
    pub rowid: i64,
    pub key: String,
    pub byte_len: u64,
}

/// A read-only structural metadata block derived from the backing file
/// (FLAC `STREAMINFO`/`SEEKTABLE`). Stored outside the editable `tags` contract.
#[cfg_attr(feature = "mutants", derive(Default))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StructuralBlock {
    pub kind: String,
    pub ordinal: u64,
    pub body: Vec<u8>,
}

#[cfg(test)]
mod track_bounds_tests {
    use super::TrackBounds;

    #[test]
    fn accepts_in_range() {
        let b = TrackBounds::new(10, 20, 100).unwrap();
        assert_eq!(b.audio_offset(), 10);
        assert_eq!(b.audio_length(), 20);
    }

    #[test]
    fn accepts_exact_fit() {
        let b = TrackBounds::new(30, 70, 100).unwrap();
        assert_eq!(b.audio_offset(), 30);
        assert_eq!(b.audio_length(), 70);
    }

    #[test]
    fn accepts_zero_length() {
        // A zero-length audio run is valid (e.g. structure-only edge).
        let b = TrackBounds::new(0, 0, 0).unwrap();
        assert_eq!(b.audio_length(), 0);
    }

    #[test]
    fn rejects_exceeding_backing_size() {
        assert!(TrackBounds::new(50, 60, 100).is_err());
    }

    #[test]
    fn rejects_offset_plus_length_overflow() {
        assert!(TrackBounds::new(u64::MAX, 1, u64::MAX).is_err());
    }
}
