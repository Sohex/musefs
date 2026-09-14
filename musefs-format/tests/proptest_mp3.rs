#![cfg(feature = "fuzzing")]
use musefs_format::fuzz_check::{assert_backing_covers_audio, fixtures};
use musefs_format::{ArtInput, BinaryTagInput, Extent, Segment, TagInput, mp3};
use proptest::prelude::*;

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    #[test]
    fn mp3_synthesis_preserves_audio(
        tags in proptest::collection::vec(("[A-Z]{1,12}", "[ -~]{0,40}"), 0..8),
    ) {
        let file = fixtures::mp3();
        let bounds = mp3::locate_audio(&file).unwrap();
        let taginputs: Vec<TagInput> = tags.iter().map(|(k, v)| TagInput::new(k, v)).collect();
        let arts: Vec<ArtInput> = Vec::new();
        if let Ok(layout) = mp3::synthesize_layout(
            bounds.audio_offset,
            bounds.audio_length,
            &taginputs,
            &[],
            &arts,
        ) {
            assert_backing_covers_audio(bounds.audio_offset, bounds.audio_length, &layout);
        }
    }

    #[test]
    fn binary_tags_round_trip_survives_byte_identically(
        priv_payload in proptest::collection::vec(any::<u8>(), 1..100),
        geob_payload in proptest::collection::vec(any::<u8>(), 1..100),
        sylt_payload in proptest::collection::vec(any::<u8>(), 1..100),
        popm_rating in proptest::option::of(0u8..=255),
        playcount in 0u64..10_000,
        has_mb_ufid in proptest::bool::ANY,
    ) {
        use id3::frame::{Content, Popularimeter, UniqueFileIdentifier, Unknown};
        use id3::{Encoder, Frame, Tag, TagLike, Version};
        use musefs_format::build_id3v2_segments;

        let mut tag = Tag::new();

        // PRIV opaque frame with arbitrary payload.
        tag.add_frame(Frame::with_content(
            "PRIV",
            Content::Unknown(Unknown { data: priv_payload.clone(), version: Version::Id3v24 }),
        ));

        // GEOB/SYLT opaque frames whose bodies open with a 0x00 (ISO-8859-1)
        // text-encoding byte followed by arbitrary, likely non-UTF-8 bytes — the
        // exact case the crate's `to_unknown()` re-encode would mangle. The raw
        // walker must preserve them byte-identical.
        let geob_body: Vec<u8> = std::iter::once(0x00).chain(geob_payload.iter().copied()).collect();
        tag.add_frame(Frame::with_content(
            "GEOB",
            Content::Unknown(Unknown { data: geob_body.clone(), version: Version::Id3v24 }),
        ));
        let sylt_body: Vec<u8> = std::iter::once(0x00).chain(sylt_payload.iter().copied()).collect();
        tag.add_frame(Frame::with_content(
            "SYLT",
            Content::Unknown(Unknown { data: sylt_body.clone(), version: Version::Id3v24 }),
        ));

        // POPM — promoted.
        if let Some(rating) = popm_rating {
            tag.add_frame(Popularimeter {
                user: "user@example".into(),
                rating,
                counter: playcount,
            });
        }

        // UFID — MusicBrainz (promoted).
        if has_mb_ufid {
            tag.add_frame(UniqueFileIdentifier {
                owner_identifier: "http://musicbrainz.org".into(),
                identifier: b"test-mbid-value".to_vec(),
            });
        }

        // UFID — non-MusicBrainz (opaque).
        tag.add_frame(UniqueFileIdentifier {
            owner_identifier: "http://other.example".into(),
            identifier: b"other-id-data".to_vec(),
        });

        // Encode.
        let mut tag_bytes = Vec::new();
        Encoder::new().version(Version::Id3v24).encode(&tag, &mut tag_bytes).unwrap();

        // Step 1: Parse binary tags.
        let (opaque, promoted) = mp3::read_binary_tags(&tag_bytes);
        prop_assert!(opaque.iter().any(|e| e.key == "PRIV"), "PRIV must be opaque");
        prop_assert!(opaque.iter().any(|e| e.key == "GEOB"), "GEOB must be opaque");
        prop_assert!(opaque.iter().any(|e| e.key == "SYLT"), "SYLT must be opaque");
        prop_assert!(opaque.iter().any(|e| e.key == "UFID"), "non-MB UFID must be opaque");

        // Step 2: DB round-trip.
        let db = musefs_db::Db::open_in_memory().unwrap();
        let tid = db.upsert_track(&musefs_db::NewTrack {
            backing_path: std::path::PathBuf::from("/a.mp3"),
            format: musefs_db::Format::Mp3,
            audio_offset: 0,
            audio_length: 0,
            backing_size: 0,
            backing_mtime_ns: 0,
            backing_ctime_ns: 0,
            backing_ino: None,
}).unwrap();
        let db_tags: Vec<musefs_db::BinaryTag> = opaque.iter().enumerate().map(|(i, e)| {
            musefs_db::BinaryTag { key: e.key.clone(), payload: e.payload.clone(), ordinal: u64::try_from(i).unwrap() }
        }).collect();
        db.set_binary_tags(tid, &db_tags).unwrap();
        let rows = db.get_binary_tags(tid).unwrap();
        let binary_tag_inputs: Vec<BinaryTagInput> = rows.iter().map(|r| {
            BinaryTagInput { key: r.key.clone(), payload_id: r.rowid, len: musefs_format::BlobLen::new(r.byte_len).unwrap() }
        }).collect();

        // Step 3: Build promoted text tags.
        let mut text_tags: Vec<TagInput> = Vec::new();
        for (k, v) in &promoted {
            text_tags.push(TagInput::new(k, v));
        }

        // Step 4: Synthesize ID3v2 segments.
        let (segments, _len) = build_id3v2_segments(&text_tags, &binary_tag_inputs, &[]).unwrap();

        // Step 5: Materialize — inline bytes + substituted BinaryTag payloads.
        let mut materialized = Vec::new();
        let payload_map: std::collections::HashMap<i64, Vec<u8>> = rows.iter().map(|r| {
            let blob = db.read_binary_tag_chunk(r.rowid, 0, usize::try_from(r.byte_len).unwrap()).unwrap();
            (r.rowid, blob)
        }).collect();
        for seg in &segments {
            match seg {
                Segment::Inline(b) => materialized.extend_from_slice(b),
                Segment::BinaryTag { payload_id, .. } => {
                    materialized.extend_from_slice(payload_map.get(payload_id).unwrap());
                }
                _ => prop_assert!(false, "unexpected segment type in tag-only build"),
            }
        }

        // Step 6: Re-parse materialized tag.
        let (opaque2, promoted2) = mp3::read_binary_tags(&materialized);

        // Step 7: Opaque frames must be byte-identical.
        prop_assert_eq!(opaque.len(), opaque2.len(), "opaque count mismatch");
        for orig in &opaque {
            let found = opaque2.iter().find(|o| o.key == orig.key && o.payload == orig.payload);
            prop_assert!(found.is_some(), "opaque frame {:?} not found in round-trip", orig.key);
        }

        // Step 8: Promoted values survive (semantic, not byte-identical).
        if let Some(rating) = popm_rating {
            prop_assert!(
                promoted.iter().any(|(k, v)| k == "rating" && v == &rating.to_string()),
                "rating not promoted on first parse"
            );
            prop_assert!(
                promoted2.iter().any(|(k, v)| k == "rating" && v == &rating.to_string()),
                "rating lost on round-trip"
            );
            if playcount > 0 {
                prop_assert!(
                    promoted.iter().any(|(k, _)| k == "playcount"),
                    "playcount not promoted on first parse"
                );
            }
        }
        if has_mb_ufid {
            prop_assert!(
                promoted.iter().any(|(k, _)| k == "musicbrainz_trackid"),
                "mbid not promoted on first parse"
            );
            prop_assert!(
                promoted2.iter().any(|(k, _)| k == "musicbrainz_trackid"),
                "mbid lost on round-trip"
            );
        }

        // Dual-UFID: the synthesized tag must contain two distinct UFID frames
        // only when the MB UFID was included (promoted MB + opaque non-MB).
        let inline_bytes: Vec<u8> = segments.iter().flat_map(|s| match s {
            Segment::Inline(b) => b.clone(),
            _ => Vec::new(),
        }).collect();
        let ufid_count = inline_bytes.windows(4).filter(|w| w == b"UFID").count();
        let expected_ufid_count = if has_mb_ufid { 2 } else { 1 };
        prop_assert_eq!(ufid_count, expected_ufid_count, "UFID frame count mismatch (MB promoted + non-MB opaque)");
    }
}

/// Where a layout puts its ID3v1 trailer, relative to its appended tags.
#[derive(Debug, Clone, Copy)]
enum Id3v1At {
    Absent,
    AfterAppended,
    BeforeAppended,
}

/// Synchsafe size encoding, independent of the production encoder.
fn syncsafe(n: u32) -> [u8; 4] {
    [
        ((n >> 21) & 0x7F) as u8,
        ((n >> 14) & 0x7F) as u8,
        ((n >> 7) & 0x7F) as u8,
        (n & 0x7F) as u8,
    ]
}

/// A v2.4 text tag with `title` and maybe `artist`, marked as an update (an
/// extended header with flag b, structure §3.2) when `update`, and given a
/// `3DI` footer (§3.4) when `footer`.
fn test_tag(title: &str, artist: Option<&str>, update: bool, footer: bool) -> Vec<u8> {
    let mut pairs = vec![("title", title)];
    if let Some(a) = artist {
        pairs.push(("artist", a));
    }
    let mut tag = fixtures::id3v24_text_tag(&pairs);
    if update {
        let frames = tag[10..].to_vec();
        tag = vec![b'I', b'D', b'3', 4, 0, 0x40];
        tag.extend_from_slice(&syncsafe(u32::try_from(frames.len() + 6).unwrap()));
        tag.extend_from_slice(&[0, 0, 0, 6, 0x01, 0x40]);
        tag.extend_from_slice(&frames);
    }
    if footer {
        tag[5] |= 0x10;
        let copy = tag[3..10].to_vec();
        tag.extend_from_slice(b"3DI");
        tag.extend_from_slice(&copy);
    }
    tag
}

/// What a prepended tag in the generated run is. An appended tag is always v2.4,
/// since only v2.4 defines the footer that places one after the audio.
#[derive(Debug, Clone, Copy)]
enum Leading {
    V22,
    V23,
    V24 { update: bool, footer: bool },
}

impl Leading {
    /// Does this tag update the tags before it, rather than replace them? v2.2 and
    /// v2.3 tags always do (ID3v2.3.0 §4.19); a v2.4 tag only with the update
    /// flag (ID3v2.4.0 structure §5).
    fn updates(self) -> bool {
        match self {
            Self::V22 | Self::V23 => true,
            Self::V24 { update, .. } => update,
        }
    }
}

/// A v2.2 or v2.3 text tag with `title` and maybe `artist`, in ISO-8859-1: v2.2
/// frames have three-character ids and 24-bit sizes, v2.3 frames plain 32-bit
/// sizes and two flag bytes.
fn legacy_tag(version: u8, title: &str, artist: Option<&str>) -> Vec<u8> {
    let (title_id, artist_id): (&[u8], &[u8]) = if version == 2 {
        (b"TT2", b"TP1")
    } else {
        (b"TIT2", b"TPE1")
    };
    let mut body = Vec::new();
    for (id, value) in std::iter::once((title_id, title)).chain(artist.map(|a| (artist_id, a))) {
        let size = u32::try_from(value.len() + 1).unwrap().to_be_bytes();
        body.extend_from_slice(id);
        if version == 2 {
            body.extend_from_slice(&size[1..]);
        } else {
            body.extend_from_slice(&size);
            body.extend_from_slice(&[0, 0]);
        }
        body.push(0);
        body.extend_from_slice(value.as_bytes());
    }
    let mut tag = vec![b'I', b'D', b'3', version, 0, 0];
    tag.extend_from_slice(&syncsafe(u32::try_from(body.len()).unwrap()));
    tag.extend(body);
    tag
}

/// Probe `file` the way the scan does: a 138-byte tail and a `window`-byte
/// prefix, each widened on `NeedMore`. Returns the bounds and the final prefix
/// and tail lengths.
fn locate_windowed(file: &[u8], window: usize) -> (mp3::Mp3Bounds, usize, usize) {
    let len = file.len() as u64;
    let mut tail_len = file.len().min(138);
    let trailer = loop {
        match mp3::locate_trailer(&file[file.len() - tail_len..], len).unwrap() {
            Extent::Complete(t) => break t,
            Extent::NeedMore { up_to } => {
                assert!(up_to > tail_len as u64 && up_to <= len);
                tail_len = usize::try_from(up_to).unwrap();
            }
        }
    };
    let mut want = file.len().min(window);
    loop {
        match mp3::locate_audio_bounded(&file[..want], len, &trailer).unwrap() {
            Extent::Complete(b) => return (b, want, tail_len),
            Extent::NeedMore { up_to } => {
                assert!(up_to > want as u64 && up_to <= len);
                want = usize::try_from(up_to).unwrap();
            }
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// #767/#768: any run of prepended v2.2, v2.3 and v2.4 tags, any appended
    /// tags, and an ID3v1 trailer on either side of them. The audio region is
    /// exactly the audio; a windowed probe agrees with the whole-buffer one; and
    /// the merged tags follow the specs in file order: a v2.2 or v2.3 tag, or a
    /// v2.4 tag flagged as an update, overrides only the keys it carries
    /// (ID3v2.3.0 §4.19, ID3v2.4.0 structure §3.2); any other v2.4 tag replaces
    /// what came before (§5).
    #[test]
    fn tags_at_both_ends_are_located_and_merged_in_file_order(
        leading in proptest::collection::vec(
            (
                "[a-z]{1,8}",
                proptest::option::of("[a-z]{1,8}"),
                prop_oneof![
                    Just(Leading::V22),
                    Just(Leading::V23),
                    (any::<bool>(), any::<bool>())
                        .prop_map(|(update, footer)| Leading::V24 { update, footer }),
                ],
            ),
            0..4,
        ),
        appended in proptest::collection::vec(
            ("[a-z]{1,8}", proptest::option::of("[a-z]{1,8}"), any::<bool>()),
            0..4,
        ),
        // Below 0x40, so no filler byte can spell `TAG` or `3DI`.
        audio_fill in proptest::collection::vec(0u8..0x40, 0..64),
        id3v1 in prop_oneof![
            Just(Id3v1At::Absent),
            Just(Id3v1At::AfterAppended),
            Just(Id3v1At::BeforeAppended),
        ],
        window in 1usize..200,
    ) {
        let mut file = Vec::new();
        for (title, artist, kind) in &leading {
            file.extend(match *kind {
                Leading::V22 => legacy_tag(2, title, artist.as_deref()),
                Leading::V23 => legacy_tag(3, title, artist.as_deref()),
                Leading::V24 { update, footer } => test_tag(title, artist.as_deref(), update, footer),
            });
        }
        let audio_offset = file.len() as u64;
        let mut audio = vec![0xFF, 0xFB];
        audio.extend_from_slice(&audio_fill);
        file.extend_from_slice(&audio);
        if matches!(id3v1, Id3v1At::BeforeAppended) {
            file.extend(fixtures::id3v1_trailer());
        }
        for (title, artist, update) in &appended {
            file.extend(test_tag(title, artist.as_deref(), *update, true));
        }
        if matches!(id3v1, Id3v1At::AfterAppended) {
            file.extend(fixtures::id3v1_trailer());
        }

        let bounds = mp3::locate_audio(&file).unwrap();
        prop_assert_eq!((bounds.audio_offset, bounds.audio_length), (audio_offset, audio.len() as u64));

        let (windowed, prefix_len, tail_len) = locate_windowed(&file, window);
        prop_assert_eq!(&windowed, &bounds);
        let len = file.len() as u64;
        let whole = mp3::read_metadata(&file, &file, len, &bounds);
        let partial = mp3::read_metadata(&file[..prefix_len], &file[file.len() - tail_len..], len, &windowed);
        prop_assert_eq!(&partial, &whole);

        let in_order = leading
            .iter()
            .map(|(title, artist, kind)| (title, artist, kind.updates()))
            .chain(appended.iter().map(|(title, artist, update)| (title, artist, *update)));
        let mut model = std::collections::BTreeMap::new();
        for (title, artist, updates) in in_order {
            if !updates {
                model.clear();
            }
            model.insert("title".to_string(), title.clone());
            if let Some(a) = artist {
                model.insert("artist".to_string(), a.clone());
            }
        }
        let mut merged = whole.tags.clone();
        merged.sort();
        let expected: Vec<(String, String)> = model.into_iter().collect();
        prop_assert_eq!(merged, expected);
    }
}
