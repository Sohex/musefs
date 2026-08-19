//! FLAC files that put one or more ID3v2 tags in front of the `fLaC` marker
//! (#602). Non-standard, but common enough in the wild — often a blank header
//! left behind by a converter — that skipping the file is the wrong answer.

mod common;
use common::{make_flac, streaminfo_body, vorbis_comment_body};
use musefs_format::flac::{
    has_leading_id3, locate_audio, locate_audio_bounded, read_metadata_bounded,
    read_vorbis_comments,
};
use musefs_format::{Extent, FormatError};

/// Encode a 28-bit synchsafe size the way an ID3v2 header does.
fn syncsafe(n: u32) -> [u8; 4] {
    [
        ((n >> 21) & 0x7F) as u8,
        ((n >> 14) & 0x7F) as u8,
        ((n >> 7) & 0x7F) as u8,
        (n & 0x7F) as u8,
    ]
}

/// An ID3v2.4 tag whose body is `body_len` zero bytes — the "blank header"
/// shape the issue reports.
fn blank_id3(body_len: u32) -> Vec<u8> {
    let mut tag = vec![b'I', b'D', b'3', 4, 0, 0];
    tag.extend_from_slice(&syncsafe(body_len));
    tag.extend(std::iter::repeat_n(0u8, usize::try_from(body_len).unwrap()));
    tag
}

/// An ID3v2.4 tag carrying UTF-8 text frames, e.g. `("TIT2", "Song")`.
fn id3_with_frames(frames: &[(&str, &str)]) -> Vec<u8> {
    let mut body = Vec::new();
    for (id, value) in frames {
        let mut content = vec![0x03]; // UTF-8 encoding byte
        content.extend_from_slice(value.as_bytes());
        body.extend_from_slice(id.as_bytes());
        body.extend_from_slice(&syncsafe(u32::try_from(content.len()).unwrap()));
        body.extend_from_slice(&[0, 0]); // frame flags
        body.extend_from_slice(&content);
    }
    let mut tag = vec![b'I', b'D', b'3', 4, 0, 0];
    tag.extend_from_slice(&syncsafe(u32::try_from(body.len()).unwrap()));
    tag.extend(body);
    tag
}

/// `prefix ++ <a minimal FLAC>`; returns the bytes and the FLAC's audio payload.
fn id3_then_flac(prefix: &[u8], comments: &[&str], audio: &[u8]) -> Vec<u8> {
    let flac = make_flac(
        &[
            (0, streaminfo_body()),
            (4, vorbis_comment_body("v", comments)),
        ],
        audio,
    );
    let mut out = prefix.to_vec();
    out.extend_from_slice(&flac);
    out
}

#[test]
fn locates_audio_past_a_leading_id3_tag() {
    let tag = blank_id3(64);
    let audio = vec![0xAA; 50];
    let file = id3_then_flac(&tag, &["TITLE=Behind The Tag"], &audio);

    let scan = locate_audio(&file).unwrap();

    // The offset is absolute — it counts the ID3 tag, so the audio segment reads
    // the right range out of the backing file.
    assert_eq!(scan.audio_offset, (file.len() - audio.len()) as u64);
    assert!(scan.audio_offset > tag.len() as u64);
    assert_eq!(scan.audio_length, audio.len() as u64);
    // STREAMINFO is still recovered from behind the tag.
    assert_eq!(scan.preserved[0].block_type, 0);
    assert_eq!(scan.preserved[0].body, streaminfo_body());
}

#[test]
fn reads_vorbis_comments_behind_a_leading_id3_tag() {
    let file = id3_then_flac(&blank_id3(32), &["TITLE=Real", "ARTIST=Band"], &[0x11; 8]);

    let comments = read_vorbis_comments(&file).unwrap();

    assert!(comments.contains(&("title".to_string(), "Real".to_string())));
    assert!(comments.contains(&("artist".to_string(), "Band".to_string())));
}

#[test]
fn accumulates_several_leading_tags() {
    let mut prefix = blank_id3(16);
    prefix.extend(id3_with_frames(&[("TIT2", "Ignored here")]));
    prefix.extend(blank_id3(8));
    let audio = vec![0x22; 12];
    let file = id3_then_flac(&prefix, &["TITLE=Third"], &audio);

    let scan = locate_audio(&file).unwrap();

    assert_eq!(scan.audio_offset, (file.len() - audio.len()) as u64);
    assert_eq!(scan.audio_length, audio.len() as u64);
}

#[test]
fn has_leading_id3_reports_the_prefix() {
    assert!(has_leading_id3(&id3_then_flac(
        &blank_id3(4),
        &[],
        &[0u8; 4]
    )));
    assert!(!has_leading_id3(&id3_then_flac(&[], &[], &[0u8; 4])));
}

#[test]
fn bounded_probe_widens_past_a_large_tag_then_completes() {
    let tag = blank_id3(4096);
    let audio = vec![0x33; 20];
    let file = id3_then_flac(&tag, &["TITLE=Wide"], &audio);

    // A window that stops inside the tag cannot see the marker yet: ask for the
    // exact byte the tag ends at rather than declaring the file unparseable.
    match read_metadata_bounded(&file[..100]).unwrap() {
        Extent::NeedMore { up_to } => assert_eq!(up_to, tag.len() as u64),
        Extent::Complete(_) => panic!("a window inside the tag cannot be complete"),
    }

    // Even a window landing exactly on the tag's end needs the marker's 4 bytes.
    match read_metadata_bounded(&file[..tag.len()]).unwrap() {
        Extent::NeedMore { up_to } => assert_eq!(up_to, (tag.len() + 4) as u64),
        Extent::Complete(_) => panic!("the marker is not in this window"),
    }

    let Extent::Complete(meta) = read_metadata_bounded(&file).unwrap() else {
        panic!("the whole file must parse");
    };
    assert_eq!(meta.audio_offset, (file.len() - audio.len()) as u64);
}

#[test]
fn bounded_matches_the_full_buffer_parse() {
    let audio = vec![0x44; 64];
    let file = id3_then_flac(&blank_id3(128), &["TITLE=Same"], &audio);

    let full = locate_audio(&file).unwrap();
    let Extent::Complete(bounded) = locate_audio_bounded(&file, file.len() as u64, None).unwrap()
    else {
        panic!("complete file must parse bounded");
    };

    assert_eq!(full, bounded);
}

#[test]
fn strips_an_id3v1_trailer_behind_a_leading_tag() {
    let audio = vec![0x55; 200];
    let mut file = id3_then_flac(&blank_id3(16), &["TITLE=Trailed"], &audio);
    let audio_offset = (file.len() - audio.len()) as u64;
    let mut trailer = b"TAG".to_vec();
    trailer.extend(std::iter::repeat_n(0u8, 125));
    file.extend_from_slice(&trailer);

    let scan = locate_audio(&file).unwrap();

    assert_eq!(scan.audio_offset, audio_offset);
    assert_eq!(
        scan.audio_length,
        audio.len() as u64,
        "the 128-byte ID3v1 trailer is not audio"
    );

    // The bounded path agrees when handed the same tail.
    let tail: &[u8; 128] = file.last_chunk::<128>().unwrap();
    let Extent::Complete(bounded) =
        locate_audio_bounded(&file, file.len() as u64, Some(tail)).unwrap()
    else {
        panic!("complete file must parse bounded");
    };
    assert_eq!(bounded.audio_length, audio.len() as u64);
}

#[test]
fn keeps_audio_that_merely_looks_like_a_trailer() {
    // No leading ID3v2 tag, so a stock FLAC is never tail-checked and audio
    // bytes that happen to spell `TAG` are served, not truncated.
    let mut audio = vec![0x66; 200];
    let n = audio.len();
    audio[n - 128..n - 125].copy_from_slice(b"TAG");
    let file = id3_then_flac(&[], &["TITLE=Not Trailed"], &audio);

    let scan = locate_audio(&file).unwrap();

    assert_eq!(scan.audio_length, audio.len() as u64);
}

#[test]
fn still_rejects_a_tag_followed_by_junk() {
    let mut file = blank_id3(8);
    file.extend_from_slice(b"NOPEnot-a-flac-stream");
    assert_eq!(locate_audio(&file).unwrap_err(), FormatError::NotFlac);
}

#[test]
fn rejects_a_malformed_tag_header() {
    let mut file = blank_id3(8);
    file[3] = 9; // no such ID3v2 major version
    file.extend_from_slice(b"fLaC");
    assert_eq!(locate_audio(&file).unwrap_err(), FormatError::Malformed);
}

#[test]
fn the_committed_fuzz_seed_fixture_is_a_valid_id3_prefixed_flac() {
    // `generate_seeds` writes this fixture to fuzz/corpus/flac/seed_id3_prefix. If
    // it ever stopped being a valid ID3-then-FLAC file the seed would still be
    // committed, silently costing the fuzzer its only entry into the skip path.
    let audio = [1u8, 2, 3, 4, 5, 6, 7, 8];
    let file = musefs_format::fuzz_check::fixtures::flac_with_leading_id3(&audio);

    assert!(has_leading_id3(&file));
    let scan = locate_audio(&file).expect("fixture parses as a FLAC behind its ID3 tag");
    assert_eq!(scan.audio_length, audio.len() as u64);
    assert_eq!(
        &file[usize::try_from(scan.audio_offset).unwrap()..],
        &audio[..]
    );
}

#[test]
fn bounded_rejects_a_file_len_shorter_than_the_metadata() {
    let file = id3_then_flac(&blank_id3(16), &["TITLE=Short"], &[0x77; 32]);
    // No real file can have its audio start past its own end, so a `file_len`
    // disagreeing with the prefix is a caller bug, not a short window.
    assert_eq!(
        locate_audio_bounded(&file, 8, None).unwrap_err(),
        FormatError::Malformed
    );
}

#[test]
fn bounded_accepts_metadata_that_fills_the_whole_file() {
    // audio_offset == file_len: a FLAC with no audio frames at all. Degenerate but
    // well-formed, so it parses rather than being rejected at the bounds check.
    let file = id3_then_flac(&blank_id3(8), &["TITLE=Empty"], &[]);
    let Extent::Complete(scan) = locate_audio_bounded(&file, file.len() as u64, None).unwrap()
    else {
        panic!("a metadata-only FLAC is still a FLAC");
    };
    assert_eq!(scan.audio_offset, file.len() as u64);
    assert_eq!(scan.audio_length, 0);
}
