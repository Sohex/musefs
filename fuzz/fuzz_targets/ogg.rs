#![no_main]
use arbitrary::Unstructured;
use libfuzzer_sys::fuzz_target;
use musefs_format::{fuzz_check::assert_backing_covers_audio, ogg};
use musefs_fuzz::{arb_tags, MAX_INPUT};

fuzz_target!(|data: &[u8]| {
    if data.len() > MAX_INPUT {
        return;
    }
    let _ = ogg::read_tags(data);
    let _ = ogg::read_pictures(data);
    let scan = match ogg::locate_audio(data) {
        Ok(s) => s,
        Err(_) => return,
    };
    let header = match ogg::read_metadata(data) {
        Ok(h) => h,
        Err(_) => return,
    };
    let mut u = Unstructured::new(data);
    let tags = arb_tags(&mut u).unwrap_or_default();
    if let Ok(layout) =
        ogg::synthesize_layout(&header, scan.audio_offset, scan.audio_length, &tags, &[])
    {
        assert_backing_covers_audio(scan.audio_offset, scan.audio_length, &layout);
    }
});
