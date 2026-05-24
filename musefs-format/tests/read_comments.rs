mod common;
use common::{make_flac, streaminfo_body, vorbis_comment_body};
use musefs_format::flac::read_vorbis_comments;

#[test]
fn reads_existing_comments_including_multivalue() {
    let si = streaminfo_body();
    let vc = vorbis_comment_body("somevendor", &["TITLE=Song", "ARTIST=Alice", "ARTIST=Bob"]);
    let file = make_flac(&[(0, si), (4, vc)], &[0u8; 8]);

    let comments = read_vorbis_comments(&file).unwrap();
    assert_eq!(
        comments,
        vec![
            ("TITLE".to_string(), "Song".to_string()),
            ("ARTIST".to_string(), "Alice".to_string()),
            ("ARTIST".to_string(), "Bob".to_string()),
        ]
    );
}

#[test]
fn returns_empty_when_no_comment_block() {
    let si = streaminfo_body();
    let file = make_flac(&[(0, si)], &[0u8; 8]);
    assert_eq!(read_vorbis_comments(&file).unwrap(), Vec::new());
}

#[test]
fn skips_comment_without_equals_sign() {
    let si = streaminfo_body();
    let vc = vorbis_comment_body("v", &["NOEQUALS", "TITLE=Ok"]);
    let file = make_flac(&[(0, si), (4, vc)], &[0u8; 4]);
    assert_eq!(
        read_vorbis_comments(&file).unwrap(),
        vec![("TITLE".to_string(), "Ok".to_string())]
    );
}
