#![no_main]
use arbitrary::Unstructured;
use libfuzzer_sys::fuzz_target;
use musefs_format::{fuzz_check::assert_backing_covers_audio, mp4};
use musefs_fuzz::{arb_arts, arb_tags, MAX_INPUT};

fuzz_target!(|data: &[u8]| {
    if data.len() > MAX_INPUT {
        return;
    }
    let _ = mp4::locate_audio(data);
    let _ = mp4::read_tags(data);
    let _ = mp4::read_pictures(data);
    let scan = match mp4::read_structure(data) {
        Ok(s) => s,
        Err(_) => return,
    };
    let mut u = Unstructured::new(data);
    let tags = arb_tags(&mut u).unwrap_or_default();
    let arts = arb_arts(&mut u).unwrap_or_default();
    if let Ok(layout) = mp4::synthesize_layout(&scan, &tags, &[], &arts) {
        assert_backing_covers_audio(scan.mdat_payload_offset, scan.mdat_payload_len, &layout);
    }
});
