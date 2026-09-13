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

/// What one scanner pass knows about one checksum column (`tracks.fingerprint`
/// or `tracks.content_hash`).
///
/// `Option<&str>` could not express this: "I computed nothing, leave the stored
/// value alone" and "the recorded bytes changed and I have no replacement" are
/// different operations, and collapsing them onto `None` left a row pairing new
/// bytes with a checksum of the old ones (#689). The tier-preservation property
/// the old `COALESCE` protected survives, because `Clear` is driven by an
/// observed content change rather than by the pass's checksum tier.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChecksumWrite<'a> {
    /// No new information: leave whatever is stored. Only correct when the
    /// recorded bytes are known not to have changed.
    Keep,
    /// A checksum computed over the bytes being recorded now.
    Set(&'a str),
    /// The recorded content changed and this pass computed no replacement, so
    /// the stored value no longer describes the file. Writes NULL.
    Clear,
}

impl<'a> ChecksumWrite<'a> {
    /// `(overwrite?, value)` for the `CASE` arms in the checksum UPDATEs.
    pub(crate) fn params(self) -> (bool, Option<&'a str>) {
        match self {
            ChecksumWrite::Keep => (false, None),
            ChecksumWrite::Set(v) => (true, Some(v)),
            ChecksumWrite::Clear => (true, None),
        }
    }

    /// `Set` when this pass computed a value, otherwise `Keep`/`Clear` on
    /// whether the recorded bytes are known to be the same ones.
    pub fn from_computed(computed: Option<&'a str>, bytes_unchanged: bool) -> ChecksumWrite<'a> {
        match computed {
            Some(v) => ChecksumWrite::Set(v),
            None if bytes_unchanged => ChecksumWrite::Keep,
            None => ChecksumWrite::Clear,
        }
    }
}

#[cfg(test)]
mod checksum_write_tests {
    use super::ChecksumWrite;

    /// `from_computed` is the whole Keep-vs-Clear decision (#689), and it lives
    /// here rather than in the scanner, so it needs its own coverage here: the
    /// mutation gate tests a `musefs-db` mutant against `musefs-db`'s tests
    /// alone, and every caller is in `musefs-core`.
    #[test]
    fn from_computed_maps_all_three_intents() {
        assert_eq!(
            ChecksumWrite::from_computed(Some("h"), true),
            ChecksumWrite::Set("h"),
            "a computed value is always written, changed bytes or not"
        );
        assert_eq!(
            ChecksumWrite::from_computed(Some("h"), false),
            ChecksumWrite::Set("h")
        );
        assert_eq!(
            ChecksumWrite::from_computed(None, true),
            ChecksumWrite::Keep,
            "nothing computed over bytes that did not change: keep the value"
        );
        assert_eq!(
            ChecksumWrite::from_computed(None, false),
            ChecksumWrite::Clear,
            "nothing computed over bytes that did change: the value is stale"
        );
    }

    /// The `(overwrite?, value)` pair each intent hands the `CASE` arms. `Keep`
    /// and `Clear` differ only here, which is the distinction `COALESCE` could
    /// not express.
    #[test]
    fn params_distinguish_keep_from_clear() {
        assert_eq!(ChecksumWrite::Keep.params(), (false, None));
        assert_eq!(ChecksumWrite::Clear.params(), (true, None));
        assert_eq!(ChecksumWrite::Set("h").params(), (true, Some("h")));
    }
}

/// The identity columns `getattr` validates a cached entry against, without
/// materializing a full `Track` (no `format` parse, no `TrackBounds`) on the
/// hottest metadata op. Two independent axes: `content_version` is the served-
/// content identity, and the backing columns are the *source* identity — a
/// scan that retargets a moved file rewrites the latter and deliberately leaves
/// the former alone, so a cache holding a locator must compare both (#679).
#[cfg_attr(feature = "mutants", derive(Default))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackIdentity {
    pub content_version: i64,
    pub backing_path: String,
    pub backing_size: u64,
    pub backing_mtime_ns: i64,
    pub backing_ctime_ns: i64,
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
    pub byte_len: u64,
    pub data: Vec<u8>,
}

#[cfg_attr(feature = "mutants", derive(Default))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtMeta {
    pub byte_len: u64,
}

#[cfg_attr(feature = "mutants", derive(Default))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackArt {
    pub art_id: i64,
    pub picture_type: u32,
    pub description: String,
    /// The MIME type *this file's* picture block declares. It lives here rather
    /// than on `art` because it describes the embedding, not the bytes: two
    /// files can hold byte-identical art and describe it differently, and when
    /// `art` owned this the first one ingested chose it for every track sharing
    /// the blob (#716).
    pub mime: String,
    /// Dimensions as this file declares them, `None` when it declares none —
    /// which is every ID3 `APIC` and every MP4 `covr`, since neither format has
    /// a field for them.
    pub width: Option<u32>,
    pub height: Option<u32>,
    /// Bits per pixel and indexed-colour count, 0 for "not stated". Only FLAC's
    /// `PICTURE` block carries them; the parser used to read and discard both.
    pub depth: u32,
    pub colors: u32,
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
