use musefs_format::FormatError;
use musefs_format::mp3::locate_audio;

/// A 10-byte ID3v2.4 header declaring `body_len` bytes of tag body (no footer).
fn id3v2_header(body_len: u32) -> Vec<u8> {
    let mut h = vec![b'I', b'D', b'3', 0x04, 0x00, 0x00];
    h.extend_from_slice(&[
        ((body_len >> 21) & 0x7F) as u8,
        ((body_len >> 14) & 0x7F) as u8,
        ((body_len >> 7) & 0x7F) as u8,
        (body_len & 0x7F) as u8,
    ]);
    h
}

#[test]
fn skips_id3v2_and_excludes_trailing_id3v1() {
    let mut data = id3v2_header(4);
    data.extend_from_slice(&[0u8; 4]); // tag body
    data.extend_from_slice(&[0xFF, 0xFB, 1, 2, 3, 4]); // 6 bytes of "audio"
    let mut v1 = vec![b'T', b'A', b'G'];
    v1.extend_from_slice(&[0u8; 125]); // 128-byte ID3v1 trailer
    data.extend_from_slice(&v1);

    let b = locate_audio(&data).unwrap();
    assert_eq!(b.audio_offset, 14); // 10-byte header + 4-byte body
    assert_eq!(b.audio_length, 6); // excludes the 128-byte ID3v1 trailer
}

#[test]
fn no_id3v2_starts_audio_at_zero() {
    let data = [0xFF, 0xFB, 0, 0, 0, 0];
    let b = locate_audio(&data).unwrap();
    assert_eq!(b.audio_offset, 0);
    assert_eq!(b.audio_length, 6);
}

#[test]
fn rejects_input_without_a_frame_sync() {
    // 10-byte ID3v2 header, no audio frame after it.
    let data = id3v2_header(0);
    assert_eq!(locate_audio(&data), Err(FormatError::NotMp3));
}

#[test]
fn rejects_id3v2_size_larger_than_file() {
    let data = id3v2_header(9999);
    assert_eq!(locate_audio(&data), Err(FormatError::Malformed));
}
