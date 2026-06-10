#![no_main]
use arbitrary::Unstructured;
use libfuzzer_sys::fuzz_target;
use musefs_format::ogg::OggArt;
use musefs_format::{fuzz_check::assert_backing_covers_audio, ogg, ArtInput, Extent};
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

    let n = u.int_in_range(0..=2u8).unwrap_or(0);
    let mut images: Vec<Vec<u8>> = Vec::new();
    let mut inputs: Vec<ArtInput> = Vec::new();
    for i in 0..n {
        let len = u.int_in_range(0..=8192usize).unwrap_or(0);
        let bytes = u.bytes(len).map(<[u8]>::to_vec).unwrap_or_default();
        inputs.push(ArtInput {
            art_id: i as i64,
            mime: "image/png".to_string(),
            description: String::new(),
            picture_type: u.int_in_range(0..=20u32).unwrap_or(3),
            width: 0,
            height: 0,
            data_len: bytes.len() as u64,
        });
        images.push(bytes);
    }
    let arts: Vec<OggArt> = inputs
        .iter()
        .zip(images.iter())
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
