use musefs_format::mp3::read_pictures;

#[test]
fn extracts_apic_pictures() {
    use id3::TagLike;

    let img = vec![0x77u8; 64];
    let mut tag = id3::Tag::new();
    tag.add_frame(id3::frame::Picture {
        mime_type: "image/jpeg".to_string(),
        picture_type: id3::frame::PictureType::CoverFront,
        description: "cover".to_string(),
        data: img.clone(),
    });
    let mut bytes = Vec::new();
    tag.write_to(&mut bytes, id3::Version::Id3v24).unwrap();
    bytes.extend_from_slice(&[0xFF, 0xFB, 0, 0]);

    let pics = read_pictures(&bytes);
    assert_eq!(pics.len(), 1);
    let p = &pics[0];
    assert_eq!(p.mime, "image/jpeg");
    assert_eq!(p.picture_type, 3); // front cover
    assert_eq!(p.description, "cover");
    assert_eq!(p.data, img);
}

#[test]
fn no_tag_yields_empty() {
    let data = [0xFF, 0xFB, 0, 0, 0, 0];
    assert!(read_pictures(&data).is_empty());
}
