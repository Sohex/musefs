#![no_main]
use arbitrary::Unstructured;
use libfuzzer_sys::fuzz_target;
use musefs_format::ogg::OggArt;
use musefs_format::{Extent, fuzz_check::assert_backing_covers_audio, ogg};
use musefs_fuzz::{MAX_INPUT, arb_arts, arb_tags};

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
    // #212: ogg::read_metadata == read_header; the bounded twin Completes only
    // when read_header succeeds, and cannot return NeedMore at a whole buffer.
    let len = data.len() as u64;
    match ogg::read_metadata_bounded(data, len) {
        Ok(Extent::Complete(h)) => assert_eq!(h, header, "ogg bounded != read_metadata"),
        Ok(Extent::NeedMore { up_to }) => {
            panic!("ogg bounded NeedMore at whole buffer: up_to={up_to}")
        }
        Err(_) => panic!("ogg bounded Err but read_metadata succeeded"),
    }
    let mut u = Unstructured::new(data);
    let tags = arb_tags(&mut u).unwrap_or_default();

    // Derive art metadata from the shared helper (so OGG exercises the same
    // randomized width/height/data_len as every other format target), then
    // generate each image's bytes independently. OGG is the one synthesis path
    // carrying both a `data_len` field and a separate `image` slice, so their
    // lengths must be free to disagree. Images stay non-empty: synthesize_layout
    // documents that the bridge drops zero-length art at construction.
    let arts_meta = arb_arts(&mut u).unwrap_or_default();
    let images: Vec<Vec<u8>> = arts_meta
        .iter()
        .map(|_| {
            let len = u.int_in_range(1..=8192usize).unwrap_or(1);
            u.bytes(len).map(<[u8]>::to_vec).unwrap_or_default()
        })
        .collect();
    let arts: Vec<OggArt> = arts_meta
        .iter()
        .zip(images.iter())
        .filter(|(_, image)| !image.is_empty())
        .map(|(meta, image)| OggArt {
            meta,
            image: image.as_slice(),
        })
        .collect();

    if let Ok(layout) =
        ogg::synthesize_layout(&header, scan.audio_offset, scan.audio_length, &tags, &arts)
    {
        assert_backing_covers_audio(scan.audio_offset, scan.audio_length, &layout);
    }
});
