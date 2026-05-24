use crate::error::{FormatError, Result};

/// Where the MP3 audio frames begin and end (excluding any ID3v2 prefix and
/// ID3v1 trailer). Unlike FLAC there is no preserved structural metadata: the
/// ID3v2 tag is regenerated from the DB, and the Xing/LAME info frame lives
/// inside the first audio frame, carried by the backing-audio segment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mp3Bounds {
    pub audio_offset: u64,
    pub audio_length: u64,
}

fn synchsafe_decode(b: &[u8]) -> u32 {
    ((b[0] & 0x7F) as u32) << 21
        | ((b[1] & 0x7F) as u32) << 14
        | ((b[2] & 0x7F) as u32) << 7
        | (b[3] & 0x7F) as u32
}

/// Locate the audio region: skip a leading ID3v2 tag (if present) and a trailing
/// 128-byte ID3v1 tag (if present), then require an MPEG frame sync at the audio
/// offset. The synthesized file re-prepends a fresh ID3v2 tag, so the original
/// one is intentionally *not* preserved.
pub fn locate_audio(data: &[u8]) -> Result<Mp3Bounds> {
    let len = data.len();

    let mut audio_offset = 0usize;
    if len >= 10 && &data[0..3] == b"ID3" {
        let flags = data[5];
        let body = synchsafe_decode(&data[6..10]) as usize;
        let mut tag_len = 10 + body;
        if flags & 0x10 != 0 {
            tag_len += 10; // ID3v2.4 footer
        }
        if tag_len > len {
            return Err(FormatError::Malformed);
        }
        audio_offset = tag_len;
    }

    let mut audio_end = len;
    if audio_end >= audio_offset + 128 && &data[audio_end - 128..audio_end - 125] == b"TAG" {
        audio_end -= 128; // strip ID3v1 trailer
    }

    // Require an MPEG audio frame sync (11 set bits) at the audio offset.
    if audio_offset + 1 >= len
        || data[audio_offset] != 0xFF
        || (data[audio_offset + 1] & 0xE0) != 0xE0
    {
        return Err(FormatError::NotMp3);
    }

    Ok(Mp3Bounds {
        audio_offset: audio_offset as u64,
        audio_length: (audio_end - audio_offset) as u64,
    })
}
