use id3::TagLike;
use musefs_format::mp3::read_tags;

#[test]
fn folds_recognized_text_frames_to_canonical_keys() {
    // Build a real ID3v2.4 tag with the id3 crate, then append a fake audio frame.
    let mut tag = id3::Tag::new();
    tag.set_title("Song");
    tag.set_artist("Alice");
    tag.set_album("Record");
    let mut buf = Vec::new();
    tag.write_to(&mut buf, id3::Version::Id3v24).unwrap();
    buf.extend_from_slice(&[0xFF, 0xFB, 0x90, 0x00]);

    let tags = read_tags(&buf);
    assert!(tags.contains(&("title".to_string(), "Song".to_string())));
    assert!(tags.contains(&("artist".to_string(), "Alice".to_string())));
    assert!(tags.contains(&("album".to_string(), "Record".to_string())));
}

#[test]
fn returns_empty_when_there_is_no_id3v2_tag() {
    let data = [0xFF, 0xFB, 0, 0, 0, 0];
    assert!(read_tags(&data).is_empty());
}
