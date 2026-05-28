#![cfg(feature = "fuzzing")]
use musefs_format::fuzz_check::{assert_backing_covers_audio, fixtures};
use musefs_format::{wav, ArtInput, TagInput};
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
            &scan, bounds.audio_offset, bounds.audio_length, &taginputs, &arts,
        ) {
            assert_backing_covers_audio(bounds.audio_offset, bounds.audio_length, &layout);
        }
    }
}
