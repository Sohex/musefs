#![cfg(feature = "fuzzing")]

mod common;

use common::{fmt_pcm_16bit_mono, resolve_layout};
use musefs_format::fuzz_check::{assert_backing_covers_audio, fixtures};
use musefs_format::{ArtInput, BinaryTagInput, BlobLen, TagInput, wav};
use proptest::prelude::*;

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    #[test]
    fn wav_synthesis_preserves_audio(
        samples in proptest::collection::vec(any::<i16>(), 1..128),
        tags in proptest::collection::vec(("[A-Z]{1,12}", "[ -~]{0,40}"), 0..8),
    ) {
        let file = fixtures::wav(&samples);
        let scan = wav::read_structure(&file).unwrap();
        let bounds = wav::locate_audio(&file).unwrap();
        let taginputs: Vec<TagInput> = tags.iter().map(|(k, v)| TagInput::new(k, v)).collect();
        let arts: Vec<ArtInput> = Vec::new();
        if let Ok(layout) = wav::synthesize_layout(
            &scan,
            bounds.audio_offset,
            bounds.audio_length,
            &taginputs,
            &[],
            &arts,
        ) {
            assert_backing_covers_audio(bounds.audio_offset, bounds.audio_length, &layout);
        }
    }
}

/// Wrap raw ID3v2 tag bytes + PCM audio into a minimal RIFF/WAVE file carrying an
/// `id3 ` chunk, so `wav::read_binary_tags` has a real chunk to extract.
fn wav_with_id3(id3: &[u8], audio: &[u8]) -> Vec<u8> {
    fn chunk(id: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut c = id.to_vec();
        c.extend_from_slice(&u32::try_from(body.len()).unwrap().to_le_bytes());
        c.extend_from_slice(body);
        if body.len() % 2 == 1 {
            c.push(0); // RIFF word-alignment pad
        }
        c
    }
    let mut body = Vec::new();
    body.extend_from_slice(b"WAVE");
    body.extend(chunk(b"fmt ", &fmt_pcm_16bit_mono()));
    body.extend(chunk(b"id3 ", id3));
    body.extend(chunk(b"data", audio));
    let mut out = Vec::new();
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&u32::try_from(body.len()).unwrap().to_le_bytes());
    out.extend_from_slice(&body);
    out
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    #[test]
    fn wav_binary_tags_round_trip_survives_byte_identically(
        priv_payload in proptest::collection::vec(any::<u8>(), 1..100),
        geob_payload in proptest::collection::vec(any::<u8>(), 1..100),
        popm_rating in proptest::option::of(0u8..=255),
        playcount in 0u64..10_000,
        has_mb_ufid in proptest::bool::ANY,
        samples in proptest::collection::vec(any::<i16>(), 1..64),
    ) {
        use id3::frame::{Content, Popularimeter, UniqueFileIdentifier, Unknown};
        use id3::{Encoder, Frame, Tag, TagLike, Version};
        use std::collections::HashMap;

        let audio: Vec<u8> = samples.iter().flat_map(|s| s.to_le_bytes()).collect();

        // Build an ID3v2.4 tag with opaque + promotable frames.
        let mut tag = Tag::new();
        tag.add_frame(Frame::with_content(
            "PRIV",
            Content::Unknown(Unknown { data: priv_payload.clone(), version: Version::Id3v24 }),
        ));
        let geob_body: Vec<u8> = std::iter::once(0x00).chain(geob_payload.iter().copied()).collect();
        tag.add_frame(Frame::with_content(
            "GEOB",
            Content::Unknown(Unknown { data: geob_body, version: Version::Id3v24 }),
        ));
        if let Some(rating) = popm_rating {
            tag.add_frame(Popularimeter { user: "user@example".into(), rating, counter: playcount });
        }
        if has_mb_ufid {
            tag.add_frame(UniqueFileIdentifier {
                owner_identifier: "http://musicbrainz.org".into(),
                identifier: b"test-mbid-value".to_vec(),
            });
        }
        tag.add_frame(UniqueFileIdentifier {
            owner_identifier: "http://other.example".into(),
            identifier: b"other-id-data".to_vec(),
        });
        let mut id3_bytes = Vec::new();
        Encoder::new().version(Version::Id3v24).encode(&tag, &mut id3_bytes).unwrap();

        // Source WAV → first parse.
        let source = wav_with_id3(&id3_bytes, &audio);
        let (opaque, promoted) = wav::read_binary_tags(&source);
        prop_assert!(opaque.iter().any(|e| e.key == "PRIV"), "PRIV must be opaque");
        prop_assert!(opaque.iter().any(|e| e.key == "GEOB"), "GEOB must be opaque");
        prop_assert!(opaque.iter().any(|e| e.key == "UFID"), "non-MB UFID must be opaque");
        if let Some(rating) = popm_rating {
            prop_assert!(promoted.iter().any(|(k, v)| k == "rating" && v == &rating.to_string()), "rating not promoted on first parse");
            if playcount > 0 {
                prop_assert!(promoted.iter().any(|(k, v)| k == "playcount" && v == &playcount.to_string()), "playcount not promoted on first parse");
            }
        }
        if has_mb_ufid {
            prop_assert!(promoted.iter().any(|(k, _)| k == "musicbrainz_trackid"), "mbid not promoted on first parse");
        }

        // DB round-trip (synthetic rowids via the in-memory DB).
        let db = musefs_db::Db::open_in_memory().unwrap();
        let tid = db.upsert_track(&musefs_db::NewTrack {
            backing_path: "/a.wav".into(),
            format: musefs_db::Format::Wav,
            audio_offset: 0, audio_length: 0, backing_size: 0, backing_mtime_ns: 0, backing_ctime_ns: 0,
        }).unwrap();
        let rows: Vec<musefs_db::BinaryTag> = opaque.iter().enumerate().map(|(i, e)| {
            musefs_db::BinaryTag { key: e.key.clone(), payload: e.payload.clone(), ordinal: u64::try_from(i).unwrap() }
        }).collect();
        db.set_binary_tags(tid, &rows).unwrap();
        let stored = db.get_binary_tags(tid).unwrap();
        let inputs: Vec<BinaryTagInput> = stored.iter().map(|r| {
            BinaryTagInput { key: r.key.clone(), payload_id: r.rowid, len: BlobLen::new(r.byte_len).unwrap() }
        }).collect();
        let mut map: HashMap<i64, Vec<u8>> = HashMap::new();
        for r in &stored {
            map.insert(r.rowid, db.read_binary_tag_chunk(r.rowid, 0, usize::try_from(r.byte_len).unwrap()).unwrap());
        }

        // Promoted text tags drive POPM/UFID regeneration.
        let text: Vec<TagInput> = promoted.iter().map(|(k, v)| TagInput::new(k, v)).collect();

        // Synthesize a fresh WAV and re-parse.
        let scan = wav::WavScan { fmt: fmt_pcm_16bit_mono(), fact: None };
        let layout = wav::synthesize_layout(&scan, 0, audio.len() as u64, &text, &inputs, &[]).unwrap();
        let served = resolve_layout(&layout, &audio, &HashMap::new(), &map);
        let (opaque2, promoted2) = wav::read_binary_tags(&served);

        // Opaque payloads survive byte-identically.
        prop_assert_eq!(opaque.len(), opaque2.len(), "opaque count mismatch");
        for orig in &opaque {
            prop_assert!(
                opaque2.iter().any(|o| o.key == orig.key && o.payload == orig.payload),
                "opaque frame {:?} lost in round-trip", orig.key
            );
        }
        // Promoted values survive (semantic, not byte-identical).
        if let Some(rating) = popm_rating {
            prop_assert!(promoted2.iter().any(|(k, v)| k == "rating" && v == &rating.to_string()), "rating lost");
        }
        if has_mb_ufid {
            prop_assert!(promoted2.iter().any(|(k, _)| k == "musicbrainz_trackid"), "mbid lost");
        }
    }
}
