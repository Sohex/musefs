#![no_main]
use arbitrary::Unstructured;
use libfuzzer_sys::fuzz_target;
use musefs_format::{fuzz_check::assert_backing_covers_audio, mp3, Extent};
use musefs_fuzz::{arb_arts, arb_binary_tags, arb_tags, MAX_INPUT};

fuzz_target!(|data: &[u8]| {
    if data.len() > MAX_INPUT {
        return;
    }
    let _ = mp3::read_tags(data);
    let _ = mp3::read_pictures(data);
    let _ = mp3::read_binary_tags(data);
    let bounds = match mp3::locate_audio(data) {
        Ok(b) => b,
        Err(_) => return,
    };
    // #212: the bounded twin must agree with the full parse on a whole buffer.
    let len = data.len() as u64;
    let tail: Option<&[u8; 128]> = if data.len() >= 128 {
        data[data.len() - 128..].try_into().ok()
    } else {
        None
    };
    match mp3::locate_audio_bounded(data, len, tail) {
        Ok(Extent::Complete(bb)) => assert_eq!(bb, bounds, "mp3 bounded != full"),
        other => panic!("mp3 bounded diverged from full Ok: {other:?}"),
    }
    let mut u = Unstructured::new(data);
    let tags = arb_tags(&mut u).unwrap_or_default();
    let binary = arb_binary_tags(&mut u).unwrap_or_default();
    let arts = arb_arts(&mut u).unwrap_or_default();
    if let Ok(layout) = mp3::synthesize_layout(
        bounds.audio_offset,
        bounds.audio_length,
        &tags,
        &binary,
        &arts,
    ) {
        assert_backing_covers_audio(bounds.audio_offset, bounds.audio_length, &layout);
    }
});
