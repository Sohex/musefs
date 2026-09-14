#![no_main]
use arbitrary::Unstructured;
use libfuzzer_sys::fuzz_target;
use musefs_format::{Extent, fuzz_check::assert_backing_covers_audio, mp3};
use musefs_fuzz::{MAX_INPUT, arb_arts, arb_binary_tags, arb_tags};

/// The scan's first tail window: an ID3v1 trailer and the footer in front of it.
const TAIL_WINDOW: usize = 138;

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
    let len = data.len() as u64;
    let metadata = mp3::read_metadata(data, data, len, &bounds);

    // #212: the bounded twin must agree with the full parse on a whole buffer.
    let whole_trailer = match mp3::locate_trailer(data, len) {
        Ok(Extent::Complete(t)) => t,
        other => panic!("mp3 trailer over a whole buffer did not complete: {other:?}"),
    };
    match mp3::locate_audio_bounded(data, len, &whole_trailer) {
        Ok(Extent::Complete(bb)) => assert_eq!(bb, bounds, "mp3 bounded != full"),
        other => panic!("mp3 bounded diverged from full Ok: {other:?}"),
    }

    // #767/#768: probed the way the scan probes it, through a tail and a prefix
    // each widened on `NeedMore`, the file lands on the same bounds and the same
    // merged metadata. Every `NeedMore` must make progress inside the file.
    let mut tail_len = data.len().min(TAIL_WINDOW);
    let trailer = loop {
        match mp3::locate_trailer(&data[data.len() - tail_len..], len) {
            Ok(Extent::Complete(t)) => break t,
            Ok(Extent::NeedMore { up_to }) => {
                assert!(up_to > tail_len as u64 && up_to <= len, "tail NeedMore {up_to}");
                tail_len = up_to as usize;
            }
            Err(e) => panic!("windowed trailer failed where the whole buffer did not: {e:?}"),
        }
    };
    let mut want = data.len().min(usize::from(data[0] & 0x3F) + 1);
    let windowed = loop {
        match mp3::locate_audio_bounded(&data[..want], len, &trailer) {
            Ok(Extent::Complete(b)) => break b,
            Ok(Extent::NeedMore { up_to }) => {
                assert!(up_to > want as u64 && up_to <= len, "prefix NeedMore {up_to}");
                want = up_to as usize;
            }
            Err(e) => panic!("windowed locate failed where the whole buffer did not: {e:?}"),
        }
    };
    assert_eq!(windowed, bounds, "windowed mp3 probe != full");
    assert_eq!(
        mp3::read_metadata(&data[..want], &data[data.len() - tail_len..], len, &windowed),
        metadata,
        "windowed mp3 metadata != full"
    );

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
