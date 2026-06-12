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

    let arts_meta = arb_arts(&mut u).unwrap_or_default();
    // Streaming couples image length to data_len (production enforces
    // `byte_len = length(data)`), so generate exactly data_len bytes per image.
    let images: Vec<Vec<u8>> = arts_meta
        .iter()
        .map(|m| {
            let mut img = vec![0u8; m.data_len.get() as usize];
            let _ = u.fill_buffer(&mut img);
            img
        })
        .collect();
    let src = ogg::MapArtSource::new(
        arts_meta.iter().zip(images.iter()).map(|(m, img)| (m.art_id, img.clone())),
    );
    let arts: Vec<OggArt> = arts_meta.iter().map(|m| OggArt { meta: m }).collect();

    if let Ok(layout) = ogg::synthesize_layout(
        &header, scan.audio_offset, scan.audio_length, &tags, &arts, &src,
    ) {
        assert_backing_covers_audio(scan.audio_offset, scan.audio_length, &layout);
    }
});
