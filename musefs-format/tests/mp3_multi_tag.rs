//! MP3s whose ID3v2 metadata is more than one tag at the front (#767), or a tag
//! appended after the audio (#768). Each case pins where the audio region is:
//! every tag byte at either end is metadata, and none of it is `BackingAudio`.

use musefs_format::fuzz_check::fixtures::{
    MP3_FIXTURE_AUDIO, id3v1_trailer, id3v24_text_tag, mp3_with_front_and_back_tags,
    mp3_with_leading_tag_run, with_id3v24_footer,
};
use musefs_format::mp3::locate_audio;

/// Synchsafe size encoding, independent of the production encoder.
fn syncsafe(n: u32) -> [u8; 4] {
    [
        ((n >> 21) & 0x7F) as u8,
        ((n >> 14) & 0x7F) as u8,
        ((n >> 7) & 0x7F) as u8,
        (n & 0x7F) as u8,
    ]
}

fn appended(title: &str) -> Vec<u8> {
    with_id3v24_footer(&id3v24_text_tag(&[("title", title)]))
}

/// Assert `file` locates to exactly [`MP3_FIXTURE_AUDIO`], starting at `offset`.
fn assert_audio_at(file: &[u8], offset: usize) {
    let b = locate_audio(file).unwrap();
    assert_eq!(b.audio_offset, offset as u64, "audio offset");
    assert_eq!(
        b.audio_length,
        MP3_FIXTURE_AUDIO.len() as u64,
        "audio length"
    );
    assert_eq!(
        &file[offset..offset + MP3_FIXTURE_AUDIO.len()],
        MP3_FIXTURE_AUDIO
    );
}

#[test]
fn a_run_of_leading_tags_is_stepped_over_whole() {
    let file = mp3_with_leading_tag_run();
    assert_audio_at(&file, file.len() - MP3_FIXTURE_AUDIO.len());
}

#[test]
fn an_appended_tag_is_not_audio() {
    let mut file = MP3_FIXTURE_AUDIO.to_vec();
    file.extend(appended("Back"));
    assert_audio_at(&file, 0);
}

#[test]
fn front_and_back_tags_and_an_id3v1_trailer_are_all_excluded() {
    let file = mp3_with_front_and_back_tags();
    let front = id3v24_text_tag(&[("title", "Front Title"), ("artist", "Front Artist")]).len();
    assert_audio_at(&file, front);
}

#[test]
fn an_appended_tag_after_an_id3v1_trailer_is_excluded_too() {
    // Not the order ID3v2.4 §5 asks for, which puts the ID3v2 tag before other
    // tagging systems' tags, but the one a writer appending at EOF produces.
    let mut file = MP3_FIXTURE_AUDIO.to_vec();
    file.extend(id3v1_trailer());
    file.extend(appended("Back"));
    assert_audio_at(&file, 0);
}

#[test]
fn repeated_appended_tags_are_all_excluded() {
    let mut file = MP3_FIXTURE_AUDIO.to_vec();
    file.extend(appended("One"));
    file.extend(appended("Two"));
    file.extend(appended("Three"));
    file.extend(id3v1_trailer());
    assert_audio_at(&file, 0);
}

#[test]
fn an_appended_tag_directly_after_the_frame_sync_is_excluded() {
    let mut file = MP3_FIXTURE_AUDIO[..2].to_vec();
    file.extend(appended("Back"));
    let b = locate_audio(&file).unwrap();
    assert_eq!((b.audio_offset, b.audio_length), (0, 2));
}

/// A footer is only a tag's footer when every field says so, and the header it
/// points back to agrees with it. Each corruption below breaks one of those, so
/// the bytes stay audio: stripping a lookalike would cut real audio short.
#[test]
fn a_footer_that_does_not_validate_leaves_its_bytes_in_the_audio() {
    let tag = appended("Back");
    let n = tag.len();
    type Corrupt = Box<dyn Fn(&mut Vec<u8>)>;
    let cases: Vec<(&str, Corrupt)> = vec![
        ("footer magic", Box::new(move |t| t[n - 10] = b'X')),
        (
            "a version with no footer (v2.3)",
            Box::new(move |t| {
                t[3] = 3;
                t[n - 7] = 3;
            }),
        ),
        (
            "revision $FF",
            Box::new(move |t| {
                t[4] = 0xFF;
                t[n - 6] = 0xFF;
            }),
        ),
        (
            "footer flag clear",
            Box::new(move |t| {
                t[5] = 0;
                t[n - 5] = 0;
            }),
        ),
        (
            "an undefined flag bit",
            Box::new(move |t| {
                t[5] |= 0x01;
                t[n - 5] |= 0x01;
            }),
        ),
        (
            "a size byte with its high bit set",
            Box::new(move |t| {
                t[9] |= 0x80;
                t[n - 1] |= 0x80;
            }),
        ),
        (
            "a size reaching past the start of the file",
            Box::new(move |t| {
                t[6..10].copy_from_slice(&[0x7F; 4]);
                t[n - 4..].copy_from_slice(&[0x7F; 4]);
            }),
        ),
        ("header magic", Box::new(|t| t[0] = b'X')),
        ("header revision differs", Box::new(|t| t[4] = 1)),
        ("header flags differ", Box::new(|t| t[5] |= 0x80)),
        ("header size differs", Box::new(|t| t[9] ^= 0x01)),
        ("footer size one short", Box::new(move |t| t[n - 1] -= 1)),
    ];
    for (what, corrupt) in cases {
        let mut bad = tag.clone();
        corrupt(&mut bad);
        let mut file = MP3_FIXTURE_AUDIO.to_vec();
        file.extend(&bad);
        let b = locate_audio(&file).unwrap();
        assert_eq!(
            (b.audio_offset, b.audio_length),
            (0, file.len() as u64),
            "{what}: a footer that fails validation does not end a tag"
        );
    }
}

/// The tag a footer at EOF points back to must lie after the frame sync. Here
/// it points into the prepended tag's body, where a matching header was planted,
/// so it describes bytes that are already metadata and the audio behind them.
#[test]
fn a_footer_pointing_back_into_the_leading_tag_is_not_an_appended_tag() {
    // The planted header declares a body running from inside the front tag,
    // across the audio, to just before its footer at EOF.
    let planted_at = 20usize; // inside PRIV's body, below
    let front_len = 10 + 10 + 32; // header, PRIV frame header, 32-byte body
    let body_len = front_len - planted_at - 10 + MP3_FIXTURE_AUDIO.len();
    let size = syncsafe(u32::try_from(body_len).unwrap());
    let mut planted = vec![b'I', b'D', b'3', 4, 0, 0x10];
    planted.extend_from_slice(&size);

    let mut priv_body = vec![0xAA; 32];
    priv_body[planted_at - 20..planted_at - 20 + 10].copy_from_slice(&planted);
    let mut file = vec![b'I', b'D', b'3', 4, 0, 0];
    file.extend_from_slice(&syncsafe(10 + 32));
    file.extend_from_slice(b"PRIV");
    file.extend_from_slice(&syncsafe(32));
    file.extend_from_slice(&[0, 0]);
    file.extend_from_slice(&priv_body);
    assert_eq!(file.len(), front_len);
    file.extend_from_slice(MP3_FIXTURE_AUDIO);
    file.extend_from_slice(b"3DI");
    file.extend_from_slice(&planted[3..]);

    let b = locate_audio(&file).unwrap();
    assert_eq!(b.audio_offset, front_len as u64);
    assert_eq!(b.audio_length, file.len() as u64 - front_len as u64);
}
