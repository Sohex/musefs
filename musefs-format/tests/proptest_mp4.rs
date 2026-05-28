#![cfg(feature = "fuzzing")]
use musefs_format::fuzz_check::{assert_backing_covers_audio, fixtures};
use musefs_format::{mp4, ArtInput, TagInput};
use proptest::prelude::*;

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    #[test]
    fn mp4_synthesis_preserves_audio(
        payload in proptest::collection::vec(any::<u8>(), 1..256),
        tags in proptest::collection::vec(("[A-Z]{1,12}", "[ -~]{0,40}"), 0..8),
    ) {
        let file = fixtures::m4a(&payload);
        let scan = mp4::read_structure(&file).unwrap();
        let taginputs: Vec<TagInput> = tags.iter().map(|(k, v)| TagInput::new(k, v)).collect();
        let arts: Vec<ArtInput> = Vec::new();
        if let Ok(layout) = mp4::synthesize_layout(&scan, &taginputs, &arts) {
            assert_backing_covers_audio(scan.mdat_payload_offset, scan.mdat_payload_len, &layout);
        }
    }
}
