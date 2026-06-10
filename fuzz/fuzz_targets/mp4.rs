#![no_main]
use arbitrary::Unstructured;
use libfuzzer_sys::fuzz_target;
use musefs_format::{fuzz_check::assert_backing_covers_audio, mp4};
use musefs_fuzz::{arb_arts, arb_binary_tags, arb_tags, MAX_INPUT};

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

    // #212: the seeking variant reads headers and skips the mdat payload; on a
    // whole buffer it must produce the same Mp4Scan as the full-buffer parse.
    let mut cursor = std::io::Cursor::new(data);
    match mp4::read_structure_from(&mut cursor, data.len() as u64) {
        Ok(s) => assert_eq!(s, scan, "mp4 read_structure_from != read_structure"),
        Err(e) => panic!("mp4 read_structure_from Err but read_structure Ok: {e:?}"),
    }

    let mut u = Unstructured::new(data);
    let tags = arb_tags(&mut u).unwrap_or_default();
    let binary = arb_binary_tags(&mut u).unwrap_or_default();
    let arts = arb_arts(&mut u).unwrap_or_default();
    if let Ok(layout) = mp4::synthesize_layout(&scan, &tags, &binary, &arts) {
        assert_backing_covers_audio(scan.mdat_payload_offset, scan.mdat_payload_len, &layout);
    }
});
