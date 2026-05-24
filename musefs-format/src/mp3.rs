use crate::error::{FormatError, Result};
use crate::input::{ArtInput, EmbeddedPicture, TagInput};
use crate::layout::{RegionLayout, Segment};

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

const ENC_UTF8: u8 = 0x03;

fn syncsafe(n: u32) -> [u8; 4] {
    [
        ((n >> 21) & 0x7F) as u8,
        ((n >> 14) & 0x7F) as u8,
        ((n >> 7) & 0x7F) as u8,
        (n & 0x7F) as u8,
    ]
}

fn push_frame_header(out: &mut Vec<u8>, id: &[u8; 4], data_len: usize) {
    // ID3v2.4 frame sizes are syncsafe (28-bit). Real metadata/art is far smaller;
    // a hard limit at art ingestion is deferred to a later milestone.
    debug_assert!(
        data_len <= 0x0FFF_FFFF,
        "ID3v2.4 frame data ({data_len} bytes) exceeds the 28-bit syncsafe limit"
    );
    out.extend_from_slice(id);
    out.extend_from_slice(&syncsafe(data_len as u32));
    out.extend_from_slice(&[0x00, 0x00]); // frame flags
}

/// Canonical (lowercase) tag key -> ID3v2.4 text frame id. Unknown keys are
/// written as `TXXX` user-defined frames.
fn key_to_frame(key: &str) -> Option<&'static [u8; 4]> {
    Some(match key {
        "title" => b"TIT2",
        "artist" => b"TPE1",
        "album" => b"TALB",
        "albumartist" => b"TPE2",
        "tracknumber" => b"TRCK",
        "discnumber" => b"TPOS",
        "date" => b"TDRC",
        "genre" => b"TCON",
        "composer" => b"TCOM",
        _ => return None,
    })
}

fn text_frame_data(values: &[String]) -> Vec<u8> {
    let mut d = vec![ENC_UTF8];
    d.extend_from_slice(values.join("\0").as_bytes());
    d
}

fn txxx_frame_data(desc: &str, value: &str) -> Vec<u8> {
    let mut d = vec![ENC_UTF8];
    d.extend_from_slice(desc.as_bytes());
    d.push(0x00);
    d.extend_from_slice(value.as_bytes());
    d
}

/// APIC frame data up to (but excluding) the image bytes:
/// `[encoding][mime\0][picture type][description\0]`.
fn apic_framing(art: &ArtInput) -> Vec<u8> {
    let mut d = vec![ENC_UTF8];
    d.extend_from_slice(art.mime.as_bytes());
    d.push(0x00);
    d.push(art.picture_type as u8);
    d.extend_from_slice(art.description.as_bytes());
    d.push(0x00);
    d
}

/// Build the synthesized region for an MP3: a fresh ID3v2.4 tag (text frames +
/// APIC frames, with image bytes streamed as `ArtImage` segments) followed by the
/// backing audio. The header's syncsafe size is measured from the generated
/// frames, so the reported size matches the bytes produced exactly.
pub fn synthesize_layout(
    audio_offset: u64,
    audio_length: u64,
    tags: &[TagInput],
    arts: &[ArtInput],
) -> RegionLayout {
    // Group consecutive same-key values (the DB returns tags ordered by key).
    let mut groups: Vec<(String, Vec<String>)> = Vec::new();
    for t in tags {
        match groups.last_mut() {
            Some(g) if g.0 == t.key => g.1.push(t.value.clone()),
            _ => groups.push((t.key.clone(), vec![t.value.clone()])),
        }
    }

    let mut segments: Vec<Segment> = Vec::new();
    let mut buf: Vec<u8> = Vec::new();
    let mut frames_len: u64 = 0;

    for (key, values) in &groups {
        match key_to_frame(key) {
            Some(id) => {
                let data = text_frame_data(values);
                push_frame_header(&mut buf, id, data.len());
                buf.extend_from_slice(&data);
                frames_len += 10 + data.len() as u64;
            }
            None => {
                for value in values {
                    let data = txxx_frame_data(key, value);
                    push_frame_header(&mut buf, b"TXXX", data.len());
                    buf.extend_from_slice(&data);
                    frames_len += 10 + data.len() as u64;
                }
            }
        }
    }

    for art in arts {
        let framing = apic_framing(art);
        let data_len = framing.len() as u64 + art.data_len;
        push_frame_header(&mut buf, b"APIC", data_len as usize);
        buf.extend_from_slice(&framing);
        segments.push(Segment::Inline(std::mem::take(&mut buf)));
        segments.push(Segment::ArtImage {
            art_id: art.art_id,
            len: art.data_len,
        });
        frames_len += 10 + data_len;
    }

    if !buf.is_empty() {
        segments.push(Segment::Inline(std::mem::take(&mut buf)));
    }

    // Prepend the 10-byte ID3v2.4 header now that the total frame length is known.
    let mut header = Vec::with_capacity(10);
    header.extend_from_slice(b"ID3");
    header.extend_from_slice(&[0x04, 0x00]); // version 2.4.0
    header.push(0x00); // flags: no unsync / extended header / footer
    debug_assert!(
        frames_len <= 0x0FFF_FFFF,
        "ID3v2.4 tag ({frames_len} bytes) exceeds the 28-bit syncsafe limit"
    );
    header.extend_from_slice(&syncsafe(frames_len as u32));
    segments.insert(0, Segment::Inline(header));

    segments.push(Segment::BackingAudio {
        offset: audio_offset,
        len: audio_length,
    });

    RegionLayout::new(segments)
}

/// ID3v2 text frame id -> canonical (lowercase) tag key. Several legacy date
/// frames fold to `date`.
fn frame_to_key(id: &str) -> Option<&'static str> {
    Some(match id {
        "TIT2" => "title",
        "TPE1" => "artist",
        "TALB" => "album",
        "TPE2" => "albumartist",
        "TRCK" => "tracknumber",
        "TPOS" => "discnumber",
        "TDRC" | "TYER" => "date",
        "TCON" => "genre",
        "TCOM" => "composer",
        _ => return None,
    })
}

/// Extract all APIC pictures from an MP3's ID3v2 tag as embedded pictures, for
/// scan-time art ingestion. Returns empty if there is no tag or no pictures.
pub fn read_pictures(data: &[u8]) -> Vec<EmbeddedPicture> {
    let tag = match id3::Tag::read_from2(std::io::Cursor::new(data)) {
        Ok(t) => t,
        Err(_) => return Vec::new(),
    };
    tag.pictures()
        .map(|p| EmbeddedPicture {
            mime: p.mime_type.clone(),
            picture_type: u8::from(p.picture_type) as u32,
            description: p.description.clone(),
            width: 0,
            height: 0,
            data: p.data.clone(),
        })
        .collect()
}

/// Read an existing ID3v2 tag from `data` and fold its recognized text frames into
/// canonical `(key, value)` pairs (keys lowercase). NUL-separated multi-value
/// frames yield one pair per value. Returns empty if there is no ID3v2 tag.
pub fn read_tags(data: &[u8]) -> Vec<(String, String)> {
    let tag = match id3::Tag::read_from2(std::io::Cursor::new(data)) {
        Ok(t) => t,
        Err(_) => return Vec::new(),
    };
    let mut out = Vec::new();
    for frame in tag.frames() {
        let Some(key) = frame_to_key(frame.id()) else {
            continue;
        };
        if let Some(text) = frame.content().text() {
            for value in text.split('\0').filter(|v| !v.is_empty()) {
                out.push((key.to_string(), value.to_string()));
            }
        }
    }
    out
}
