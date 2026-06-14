#![cfg(feature = "fuzzing")]

mod common;

use common::resolve_layout;
use musefs_format::fuzz_check::{assert_backing_covers_audio, fixtures};
use musefs_format::{ArtInput, BinaryTagInput, BlobLen, PictureType, TagInput, mp4};
use proptest::prelude::*;

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    #[test]
    fn mp4_synthesis_preserves_audio(
        payload in proptest::collection::vec(any::<u8>(), 1..256),
        tags in proptest::collection::vec(("[A-Z]{1,12}", "[ -~]{0,40}"), 0..8),
        arts in proptest::collection::vec((1..3u8, 1..500u64), 0..3),
    ) {
        let file = fixtures::m4a(&payload);
        let scan = mp4::read_structure(&file).unwrap();
        let taginputs: Vec<TagInput> = tags.iter().map(|(k, v)| TagInput::new(k, v)).collect();
        // (kind, len) pairs: kind 1 = jpeg, 2 = png; len is always non-zero
        // (BlobLen invariant) so zero-byte art is never tested here.
        let arts: Vec<ArtInput> = arts
            .iter()
            .enumerate()
            .map(|(i, (kind, len))| ArtInput {
                art_id: i64::try_from(i).unwrap() + 1,
                mime: if *kind == 1 { "image/jpeg".into() } else { "image/png".into() },
                description: String::new(),
                picture_type: PictureType::new(3).unwrap(),
                width: 0,
                height: 0,
                data_len: BlobLen::new(*len).unwrap(),
            })
            .collect();
        if let Ok(layout) = mp4::synthesize_layout(&scan, &taginputs, &[], &arts) {
            assert_backing_covers_audio(scan.mdat_payload_offset, scan.mdat_payload_len, &layout);
        }
    }

    #[test]
    fn mp4_binary_freeform_round_trips_byte_identically(
        payload_audio in proptest::collection::vec(any::<u8>(), 1..256),
        bins in proptest::collection::vec(
            ("[a-zA-Z][a-zA-Z0-9._]{0,11}", proptest::collection::vec(any::<u8>(), 1..80)),
            1..5,
        ),
    ) {
        use std::collections::HashMap;

        let file = fixtures::m4a(&payload_audio);
        let scan = mp4::read_structure(&file).unwrap();

        // Synthetic payload handles standing in for `tags` rowids; a map stands in
        // for the DB blob store the reader streams from.
        let mut inputs: Vec<BinaryTagInput> = Vec::new();
        let mut map: HashMap<i64, Vec<u8>> = HashMap::new();
        for (i, (name, bytes)) in bins.iter().enumerate() {
            let id = i64::try_from(i).unwrap() + 1;
            inputs.push(BinaryTagInput {
                key: format!("----:com.apple.iTunes:{name}"),
                payload_id: id,
                len: BlobLen::new(bytes.len() as u64).unwrap(),
            });
            map.insert(id, bytes.clone());
        }

        let layout = mp4::synthesize_layout(&scan, &[], &inputs, &[]).unwrap();

        // Byte-identical-audio invariant WITH binary frames present (spec §Testing):
        // the original mdat payload is still served verbatim as a BackingAudio run.
        assert_backing_covers_audio(scan.mdat_payload_offset, scan.mdat_payload_len, &layout);

        // Materialize the served file: inline verbatim, BinaryTag from the map,
        // BackingAudio from the original fixture.
        let served = resolve_layout(&layout, &file, &HashMap::new(), &map);

        // Re-parse the served file: every input payload survives byte-identically.
        let reparsed = mp4::read_binary_tags(&served, usize::MAX);
        prop_assert_eq!(reparsed.len(), inputs.len(), "binary tag count mismatch");
        for input in &inputs {
            let want = map.get(&input.payload_id).unwrap();
            let found = reparsed
                .iter()
                .find(|t| t.key == input.key && &t.payload == want);
            prop_assert!(found.is_some(), "round-trip lost {:?}", input.key);
        }
    }
}
