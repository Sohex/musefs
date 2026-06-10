#![no_main]
use arbitrary::Unstructured;
use libfuzzer_sys::fuzz_target;
use musefs_format::{Extent, fuzz_check::assert_backing_covers_audio, wav};
use musefs_fuzz::{MAX_INPUT, arb_arts, arb_tags};

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
    // #212: bounded twin on a whole buffer is literally Complete(locate_audio).
    let len = data.len() as u64;
    match wav::locate_audio_bounded(data, len) {
        Ok(Extent::Complete(b)) => assert_eq!(b, bounds, "wav bounded != full"),
        other => panic!("wav bounded diverged (whole buffer): {other:?}"),
    }
    // The ceiling prober trusts a declared `data` length validated against
    // file_len, so it may accept where locate_audio rejects. Assert only that it
    // stays in bounds (no equality oracle).
    if let Ok(c) = wav::locate_audio_at_ceiling(data, len) {
        assert!(
            c.audio_offset.saturating_add(c.audio_length) <= len,
            "wav ceiling region exceeds file_len: off={} len={} file_len={}",
            c.audio_offset,
            c.audio_length,
            len,
        );
    }
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
