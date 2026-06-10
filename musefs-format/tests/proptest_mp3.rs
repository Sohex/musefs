#![cfg(feature = "fuzzing")]
use musefs_format::fuzz_check::{assert_backing_covers_audio, fixtures};
use musefs_format::{ArtInput, BinaryTagInput, Segment, TagInput, mp3};
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
            backing_path: "/a.mp3".into(),
            format: musefs_db::Format::Mp3,
            audio_offset: 0,
            audio_length: 0,
            backing_size: 0,
            backing_mtime: 0,
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
