use musefs_format::flac::read_pictures;
use musefs_format::fuzz_check::fixtures::{flac_block, streaminfo_body};

fn picture_body(pic_type: u32, mime: &str, desc: &str, w: u32, h: u32, data: &[u8]) -> Vec<u8> {
    let mut b = Vec::new();
    b.extend_from_slice(&pic_type.to_be_bytes());
    b.extend_from_slice(&(mime.len() as u32).to_be_bytes());
    b.extend_from_slice(mime.as_bytes());
    b.extend_from_slice(&(desc.len() as u32).to_be_bytes());
    b.extend_from_slice(desc.as_bytes());
    b.extend_from_slice(&w.to_be_bytes());
    b.extend_from_slice(&h.to_be_bytes());
    b.extend_from_slice(&0u32.to_be_bytes()); // color depth
    b.extend_from_slice(&0u32.to_be_bytes()); // colors used
    b.extend_from_slice(&(data.len() as u32).to_be_bytes());
    b.extend_from_slice(data);
    b
}

#[test]
fn extracts_picture_blocks() {
    let img = vec![0xABu8; 50];
    let mut flac = Vec::new();
    flac.extend_from_slice(b"fLaC");
    flac.extend_from_slice(&flac_block(0, &streaminfo_body(), false));
    flac.extend_from_slice(&flac_block(
        6,
        &picture_body(3, "image/png", "front", 10, 20, &img),
        true,
    ));
    flac.extend_from_slice(&[0xFFu8; 8]); // audio

    let pics = read_pictures(&flac).unwrap();
    assert_eq!(pics.len(), 1);
    let p = &pics[0];
    assert_eq!(p.picture_type, 3);
    assert_eq!(p.mime, "image/png");
    assert_eq!(p.description, "front");
    assert_eq!(p.width, 10);
    assert_eq!(p.height, 20);
    assert_eq!(p.data, img);
}

#[test]
fn no_pictures_yields_empty() {
    let mut flac = Vec::new();
    flac.extend_from_slice(b"fLaC");
    flac.extend_from_slice(&flac_block(0, &streaminfo_body(), true));
    flac.extend_from_slice(&[0xFFu8; 4]);
    assert!(read_pictures(&flac).unwrap().is_empty());
}
