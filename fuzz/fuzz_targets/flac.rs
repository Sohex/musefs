#![no_main]
use arbitrary::Unstructured;
use libfuzzer_sys::fuzz_target;
use musefs_format::{flac, fuzz_check::assert_backing_covers_audio};
use musefs_fuzz::{arb_arts, arb_tags, MAX_INPUT};

fuzz_target!(|data: &[u8]| {
    if data.len() > MAX_INPUT {
        return;
    }
    let _ = flac::read_pictures(data);
    let scan = match flac::locate_audio(data) {
        Ok(s) => s,
        Err(_) => return,
    };
    let mut u = Unstructured::new(data);
    let tags = arb_tags(&mut u).unwrap_or_default();
    let arts = arb_arts(&mut u).unwrap_or_default();
    if let Ok(layout) = flac::synthesize_layout(
        &scan.preserved,
        scan.audio_offset,
        scan.audio_length,
        &tags,
        &[],
        &arts,
    ) {
        assert_backing_covers_audio(scan.audio_offset, scan.audio_length, &layout);
    }
});
