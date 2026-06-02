#![no_main]
use arbitrary::Unstructured;
use libfuzzer_sys::fuzz_target;
use musefs_format::{fuzz_check::assert_backing_covers_audio, wav};
use musefs_fuzz::{arb_arts, arb_tags, MAX_INPUT};

fuzz_target!(|data: &[u8]| {
    if data.len() > MAX_INPUT {
        return;
    }
    let _ = wav::read_tags(data);
    let _ = wav::read_pictures(data);
    let scan = match wav::read_structure(data) {
        Ok(s) => s,
        Err(_) => return,
    };
    let bounds = match wav::locate_audio(data) {
        Ok(b) => b,
        Err(_) => return,
    };
    let mut u = Unstructured::new(data);
    let tags = arb_tags(&mut u).unwrap_or_default();
    let arts = arb_arts(&mut u).unwrap_or_default();
    if let Ok(layout) = wav::synthesize_layout(
        &scan,
        bounds.audio_offset,
        bounds.audio_length,
        &tags,
        &[],
        &arts,
    ) {
        assert_backing_covers_audio(bounds.audio_offset, bounds.audio_length, &layout);
    }
});
