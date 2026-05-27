//! Canonical tag vocabulary: the single source of truth mapping a canonical
//! (lowercase) tag key to its native representation in each container format.
//! Format modules consult this for both scanning (native -> canonical) and
//! synthesis (canonical -> native). Tags absent from the vocabulary are
//! user-defined and round-trip verbatim through each format's extension slot.

/// How a canonical key is represented inside an ID3v2 tag.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Id3Slot {
    /// A standard text information frame, e.g. `b"TIT2"`.
    Text(&'static [u8; 4]),
    /// A `TXXX` user-defined text frame with this fixed, exact-case description.
    Txxx(&'static str),
    /// The `COMM` comment frame.
    Comment,
    /// The `USLT` unsynchronised-lyrics frame.
    Lyrics,
}

/// How a canonical key is represented inside an MP4 `ilst`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Mp4Slot {
    /// A text atom, e.g. `b"\xa9nam"`.
    Text(&'static [u8; 4]),
    /// A binary number atom (`trkn`/`disk`); the `usize` is the `data` body width.
    Number(&'static [u8; 4], usize),
    /// A `----` freeform atom: `(mean, name)`.
    Freeform(&'static str, &'static str),
}

pub(crate) struct Entry {
    pub key: &'static str, // canonical, lowercase
    pub id3: Id3Slot,
    pub mp4: Mp4Slot,
    pub vorbis: &'static str, // Vorbis field name (uppercase convention)
}

const VOCAB: &[Entry] = &[
    Entry {
        key: "title",
        id3: Id3Slot::Text(b"TIT2"),
        mp4: Mp4Slot::Text(b"\xa9nam"),
        vorbis: "TITLE",
    },
    Entry {
        key: "artist",
        id3: Id3Slot::Text(b"TPE1"),
        mp4: Mp4Slot::Text(b"\xa9ART"),
        vorbis: "ARTIST",
    },
    Entry {
        key: "album",
        id3: Id3Slot::Text(b"TALB"),
        mp4: Mp4Slot::Text(b"\xa9alb"),
        vorbis: "ALBUM",
    },
    Entry {
        key: "albumartist",
        id3: Id3Slot::Text(b"TPE2"),
        mp4: Mp4Slot::Text(b"aART"),
        vorbis: "ALBUMARTIST",
    },
    Entry {
        key: "genre",
        id3: Id3Slot::Text(b"TCON"),
        mp4: Mp4Slot::Text(b"\xa9gen"),
        vorbis: "GENRE",
    },
    Entry {
        key: "date",
        id3: Id3Slot::Text(b"TDRC"),
        mp4: Mp4Slot::Text(b"\xa9day"),
        vorbis: "DATE",
    },
    Entry {
        key: "composer",
        id3: Id3Slot::Text(b"TCOM"),
        mp4: Mp4Slot::Text(b"\xa9wrt"),
        vorbis: "COMPOSER",
    },
    Entry {
        key: "grouping",
        id3: Id3Slot::Text(b"TIT1"),
        mp4: Mp4Slot::Text(b"\xa9grp"),
        vorbis: "GROUPING",
    },
    Entry {
        key: "tracknumber",
        id3: Id3Slot::Text(b"TRCK"),
        mp4: Mp4Slot::Number(b"trkn", 8),
        vorbis: "TRACKNUMBER",
    },
    Entry {
        key: "discnumber",
        id3: Id3Slot::Text(b"TPOS"),
        mp4: Mp4Slot::Number(b"disk", 6),
        vorbis: "DISCNUMBER",
    },
    Entry {
        key: "comment",
        id3: Id3Slot::Comment,
        mp4: Mp4Slot::Text(b"\xa9cmt"),
        vorbis: "COMMENT",
    },
    Entry {
        key: "lyrics",
        id3: Id3Slot::Lyrics,
        mp4: Mp4Slot::Text(b"\xa9lyr"),
        vorbis: "LYRICS",
    },
    Entry {
        key: "copyright",
        id3: Id3Slot::Text(b"TCOP"),
        mp4: Mp4Slot::Freeform("com.apple.iTunes", "COPYRIGHT"),
        vorbis: "COPYRIGHT",
    },
    Entry {
        key: "isrc",
        id3: Id3Slot::Text(b"TSRC"),
        mp4: Mp4Slot::Freeform("com.apple.iTunes", "ISRC"),
        vorbis: "ISRC",
    },
    Entry {
        key: "lyricist",
        id3: Id3Slot::Text(b"TEXT"),
        mp4: Mp4Slot::Freeform("com.apple.iTunes", "LYRICIST"),
        vorbis: "LYRICIST",
    },
    Entry {
        key: "conductor",
        id3: Id3Slot::Text(b"TPE3"),
        mp4: Mp4Slot::Freeform("com.apple.iTunes", "CONDUCTOR"),
        vorbis: "CONDUCTOR",
    },
    Entry {
        key: "replaygain_track_gain",
        id3: Id3Slot::Txxx("REPLAYGAIN_TRACK_GAIN"),
        mp4: Mp4Slot::Freeform("com.apple.iTunes", "replaygain_track_gain"),
        vorbis: "REPLAYGAIN_TRACK_GAIN",
    },
    Entry {
        key: "replaygain_album_gain",
        id3: Id3Slot::Txxx("REPLAYGAIN_ALBUM_GAIN"),
        mp4: Mp4Slot::Freeform("com.apple.iTunes", "replaygain_album_gain"),
        vorbis: "REPLAYGAIN_ALBUM_GAIN",
    },
    Entry {
        key: "replaygain_track_peak",
        id3: Id3Slot::Txxx("REPLAYGAIN_TRACK_PEAK"),
        mp4: Mp4Slot::Freeform("com.apple.iTunes", "replaygain_track_peak"),
        vorbis: "REPLAYGAIN_TRACK_PEAK",
    },
    Entry {
        key: "replaygain_album_peak",
        id3: Id3Slot::Txxx("REPLAYGAIN_ALBUM_PEAK"),
        mp4: Mp4Slot::Freeform("com.apple.iTunes", "replaygain_album_peak"),
        vorbis: "REPLAYGAIN_ALBUM_PEAK",
    },
    Entry {
        key: "musicbrainz_albumid",
        id3: Id3Slot::Txxx("MusicBrainz Album Id"),
        mp4: Mp4Slot::Freeform("com.apple.iTunes", "MusicBrainz Album Id"),
        vorbis: "MUSICBRAINZ_ALBUMID",
    },
    Entry {
        key: "musicbrainz_artistid",
        id3: Id3Slot::Txxx("MusicBrainz Artist Id"),
        mp4: Mp4Slot::Freeform("com.apple.iTunes", "MusicBrainz Artist Id"),
        vorbis: "MUSICBRAINZ_ARTISTID",
    },
];

/// ID3 text frame id (e.g. "TIT2") -> canonical key, for `Text` slots only.
pub(crate) fn id3_text_to_key(frame_id: &str) -> Option<&'static str> {
    VOCAB.iter().find_map(|e| match e.id3 {
        Id3Slot::Text(id) if &id[..] == frame_id.as_bytes() => Some(e.key),
        _ => None,
    })
}

/// `TXXX` description -> canonical key (case-insensitive), for `Txxx` slots only.
pub(crate) fn id3_txxx_to_key(description: &str) -> Option<&'static str> {
    VOCAB.iter().find_map(|e| match e.id3 {
        Id3Slot::Txxx(d) if d.eq_ignore_ascii_case(description) => Some(e.key),
        _ => None,
    })
}

/// Canonical key -> ID3 slot (key matched case-insensitively).
pub(crate) fn key_to_id3(key: &str) -> Option<Id3Slot> {
    let k = key.to_ascii_lowercase();
    VOCAB.iter().find(|e| e.key == k).map(|e| e.id3)
}

/// MP4 text atom -> canonical key, for `Text` slots only. `Number` atoms
/// (`trkn`/`disk`) are intentionally excluded: the scan path decodes their binary
/// track/disc value positionally rather than through this lookup.
pub(crate) fn mp4_atom_to_key(atom: &[u8; 4]) -> Option<&'static str> {
    VOCAB.iter().find_map(|e| match e.mp4 {
        Mp4Slot::Text(a) if a == atom => Some(e.key),
        _ => None,
    })
}

/// MP4 `----` (mean, name) -> canonical key (case-insensitive), `Freeform` only.
pub(crate) fn mp4_freeform_to_key(mean: &str, name: &str) -> Option<&'static str> {
    VOCAB.iter().find_map(|e| match e.mp4 {
        Mp4Slot::Freeform(m, n) if m.eq_ignore_ascii_case(mean) && n.eq_ignore_ascii_case(name) => {
            Some(e.key)
        }
        _ => None,
    })
}

/// Canonical key -> MP4 slot (key matched case-insensitively).
pub(crate) fn key_to_mp4(key: &str) -> Option<Mp4Slot> {
    let k = key.to_ascii_lowercase();
    VOCAB.iter().find(|e| e.key == k).map(|e| e.mp4)
}

/// Vorbis field name -> canonical key (case-insensitive).
pub(crate) fn vorbis_to_key(field: &str) -> Option<&'static str> {
    VOCAB
        .iter()
        .find_map(|e| e.vorbis.eq_ignore_ascii_case(field).then_some(e.key))
}

/// Canonical key -> Vorbis field name (key matched case-insensitively).
pub(crate) fn key_to_vorbis(key: &str) -> Option<&'static str> {
    let k = key.to_ascii_lowercase();
    VOCAB.iter().find(|e| e.key == k).map(|e| e.vorbis)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_duplicate_canonical_keys() {
        let mut keys: Vec<&str> = VOCAB.iter().map(|e| e.key).collect();
        let n = keys.len();
        keys.sort_unstable();
        keys.dedup();
        assert_eq!(keys.len(), n, "duplicate canonical key in VOCAB");
    }

    #[test]
    fn id3_text_round_trips() {
        for e in VOCAB {
            if let Id3Slot::Text(id) = e.id3 {
                let frame = std::str::from_utf8(id).unwrap();
                assert_eq!(id3_text_to_key(frame), Some(e.key));
                assert!(matches!(key_to_id3(e.key), Some(Id3Slot::Text(_))));
            }
        }
    }

    #[test]
    fn mp4_text_round_trips() {
        for e in VOCAB {
            if let Mp4Slot::Text(a) = e.mp4 {
                assert_eq!(mp4_atom_to_key(a), Some(e.key));
            }
        }
    }

    #[test]
    fn vorbis_round_trips() {
        for e in VOCAB {
            assert_eq!(vorbis_to_key(e.vorbis), Some(e.key));
            assert_eq!(key_to_vorbis(e.key), Some(e.vorbis));
        }
    }

    #[test]
    fn key_to_slot_round_trips() {
        for e in VOCAB {
            assert_eq!(
                key_to_id3(e.key),
                Some(e.id3),
                "key_to_id3 failed for {}",
                e.key
            );
            assert_eq!(
                key_to_mp4(e.key),
                Some(e.mp4),
                "key_to_mp4 failed for {}",
                e.key
            );
        }
    }

    #[test]
    fn txxx_and_freeform_lookups_are_case_insensitive() {
        assert_eq!(
            id3_txxx_to_key("musicbrainz album id"),
            Some("musicbrainz_albumid")
        );
        assert_eq!(
            mp4_freeform_to_key("com.apple.itunes", "MusicBrainz Album Id"),
            Some("musicbrainz_albumid")
        );
    }
}
