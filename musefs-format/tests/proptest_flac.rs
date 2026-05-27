#![cfg(feature = "fuzzing")]
use musefs_format::fuzz_check::{assert_backing_covers_audio, fixtures};
use musefs_format::{flac, ArtInput, Segment, TagInput};
use proptest::prelude::*;

fn tags_strategy() -> impl Strategy<Value = Vec<(String, String)>> {
    proptest::collection::vec(("[A-Z]{1,12}", "[ -~]{0,40}"), 0..8)
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    #[test]
    fn flac_synthesis_preserves_audio(
        audio in proptest::collection::vec(any::<u8>(), 1..256),
        tags in tags_strategy(),
    ) {
        let file = fixtures::flac(&audio);
        let scan = flac::locate_audio(&file).unwrap();
        let taginputs: Vec<TagInput> = tags.iter().map(|(k, v)| TagInput::new(k, v)).collect();
        let arts: Vec<ArtInput> = Vec::new();
        if let Ok(layout) = flac::synthesize_layout(&scan, &taginputs, &arts) {
            assert_backing_covers_audio(scan.audio_offset, scan.audio_length, &layout);
        }
    }

    #[test]
    fn flac_tag_roundtrip_is_stable(
        audio in proptest::collection::vec(any::<u8>(), 1..64),
        tags in tags_strategy(),
    ) {
        let file = fixtures::flac(&audio);
        let scan = flac::locate_audio(&file).unwrap();
        let taginputs: Vec<TagInput> = tags.iter().map(|(k, v)| TagInput::new(k, v)).collect();
        let arts: Vec<ArtInput> = Vec::new();
        let layout = match flac::synthesize_layout(&scan, &taginputs, &arts) {
            Ok(l) => l,
            Err(_) => return Ok(()),
        };
        let mut front = Vec::new();
        for seg in layout.segments() {
            if let Segment::Inline(b) = seg {
                front.extend_from_slice(b);
            }
        }
        let meta = flac::read_metadata(&front).unwrap();
        prop_assert_eq!(meta.audio_offset, layout.header_len());
    }
}
