#![cfg(feature = "fuzzing")]
use musefs_format::fuzz_check::{assert_backing_covers_audio, fixtures};
use musefs_format::{TagInput, ogg};
use proptest::prelude::*;

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn ogg_synthesis_preserves_audio(
        tags in proptest::collection::vec(("[A-Z]{1,12}", "[ -~]{0,40}"), 0..8),
    ) {
        let file = fixtures::ogg_opus();
        let scan = ogg::locate_audio(&file).unwrap();
        let header = ogg::read_metadata(&file).unwrap();
        let taginputs: Vec<TagInput> = tags.iter().map(|(k, v)| TagInput::new(k, v)).collect();
        if let Ok(layout) =
            ogg::synthesize_layout(&header, scan.audio_offset, scan.audio_length, &taginputs, &[])
        {
            assert_backing_covers_audio(scan.audio_offset, scan.audio_length, &layout);
        }
    }
}
