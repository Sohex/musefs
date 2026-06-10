#![no_main]
use arbitrary::Unstructured;
use libfuzzer_sys::fuzz_target;
use musefs_format::{flac, fuzz_check::assert_backing_covers_audio, Extent};
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
    // #212: flac's bounded twin takes no file_len, so NeedMore can fire at the
    // whole buffer when a block declares a body past EOF. Since locate_audio
    // succeeded here, the file fully parses, so the bounded twin must Complete
    // and equal read_metadata.
    match flac::read_metadata_bounded(data) {
        Ok(Extent::Complete(m)) => {
            let full = flac::read_metadata(data).expect("locate_audio Ok but read_metadata Err");
            assert_eq!(m, full, "flac bounded != full read_metadata");
        }
        other => panic!("flac bounded diverged (locate_audio succeeded): {other:?}"),
    }
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
