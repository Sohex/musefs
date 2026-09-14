#![no_main]
use arbitrary::Unstructured;
use libfuzzer_sys::fuzz_target;
use musefs_format::{
    fuzz_check::{assert_backing_covers_audio, assert_mp4_single_metadata_system},
    mp4,
};
use musefs_fuzz::{MAX_INPUT, arb_arts, arb_binary_tags, arb_tags};

fuzz_target!(|data: &[u8]| {
    if data.len() > MAX_INPUT {
        return;
    }
    let _ = mp4::locate_audio(data);
    let _ = mp4::read_tags(data);
    let _ = mp4::read_pictures_reporting(data, 64);
    let _ = mp4::read_binary_tags_reporting(data, 64);
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

    // #771: served from tags alone (nothing streamed, so it materializes cheaply),
    // the file re-parses, keeps its audio byte for byte, carries no QuickTime keyed
    // metadata, and every chunk offset moved by exactly the relocation delta.
    if let Ok(layout) = mp4::synthesize_layout(&scan, &tags, &[], &[]) {
        assert_mp4_single_metadata_system(data, &scan, &layout);
    }
});
