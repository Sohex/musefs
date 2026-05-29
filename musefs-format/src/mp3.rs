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

fn push_frame_header(out: &mut Vec<u8>, id: &[u8; 4], data_len: usize) -> Result<()> {
    // ID3v2.4 frame sizes are a 28-bit syncsafe field; guard so an oversized frame
    // is a hard error rather than a silently-truncated (corrupt) tag.
    if data_len > 0x0FFF_FFFF {
        return Err(FormatError::TooLarge);
    }
    out.extend_from_slice(id);
    out.extend_from_slice(&syncsafe(data_len as u32));
    out.extend_from_slice(&[0x00, 0x00]); // frame flags
    Ok(())
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

/// COMM/USLT share a body layout: `[enc][lang(3)][descriptor NUL][text]`. We
/// write UTF-8 with an unknown language (`XXX`) and empty descriptor; the
/// original language code and descriptor are not preserved on round-trip.
fn comm_like_frame_data(value: &str) -> Vec<u8> {
    let mut d = vec![ENC_UTF8];
    d.extend_from_slice(b"XXX"); // language: unknown
    d.push(0x00); // empty content descriptor, NUL-terminated
    d.extend_from_slice(value.as_bytes());
    d
}

/// True if `key` is shaped like an ID3v2 text frame id (`T` + 3 upper/digit),
/// excluding `TXXX` itself. Used to round-trip unmapped standard text frames.
fn is_id3_text_frame_id(key: &str) -> bool {
    key.len() == 4
        && key != "TXXX"
        && key.starts_with('T')
        && key
            .bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit())
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

/// Build the ID3v2.4 tag region for `tags`/`arts`: an inline 10-byte header
/// followed by text/`TXXX` frames and `APIC` frames whose image bytes are
/// streamed as `ArtImage` segments. Returns the segments (no backing audio) and
/// the total tag length (`10 + frames_len`). Shared by MP3 synthesis and the WAV
/// `id3 ` chunk.
pub(crate) fn build_id3v2_segments(
    tags: &[TagInput],
    arts: &[ArtInput],
) -> Result<(Vec<Segment>, u64)> {
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
        match crate::tagmap::key_to_id3(key) {
            Some(crate::tagmap::Id3Slot::Text(id)) => {
                let data = text_frame_data(values);
                push_frame_header(&mut buf, id, data.len())?;
                buf.extend_from_slice(&data);
                frames_len += 10 + data.len() as u64;
            }
            Some(crate::tagmap::Id3Slot::Txxx(desc)) => {
                for value in values {
                    let data = txxx_frame_data(desc, value);
                    push_frame_header(&mut buf, b"TXXX", data.len())?;
                    buf.extend_from_slice(&data);
                    frames_len += 10 + data.len() as u64;
                }
            }
            Some(crate::tagmap::Id3Slot::Comment) => {
                for value in values {
                    let data = comm_like_frame_data(value);
                    push_frame_header(&mut buf, b"COMM", data.len())?;
                    buf.extend_from_slice(&data);
                    frames_len += 10 + data.len() as u64;
                }
            }
            Some(crate::tagmap::Id3Slot::Lyrics) => {
                for value in values {
                    let data = comm_like_frame_data(value);
                    push_frame_header(&mut buf, b"USLT", data.len())?;
                    buf.extend_from_slice(&data);
                    frames_len += 10 + data.len() as u64;
                }
            }
            None if is_id3_text_frame_id(key) => {
                // safe: is_id3_text_frame_id guarantees key is exactly 4 bytes
                let id: [u8; 4] = key.as_bytes().try_into().unwrap();
                let data = text_frame_data(values);
                push_frame_header(&mut buf, &id, data.len())?;
                buf.extend_from_slice(&data);
                frames_len += 10 + data.len() as u64;
            }
            None => {
                for value in values {
                    let data = txxx_frame_data(key, value);
                    push_frame_header(&mut buf, b"TXXX", data.len())?;
                    buf.extend_from_slice(&data);
                    frames_len += 10 + data.len() as u64;
                }
            }
        }
    }

    for art in arts {
        let framing = apic_framing(art);
        let data_len = framing.len() as u64 + art.data_len;
        push_frame_header(&mut buf, b"APIC", data_len as usize)?;
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

    // The total tag size is a 28-bit syncsafe field. Ingestion caps each art well
    // under this, but guard at the format boundary so an oversized tag (e.g. many
    // large pictures summing past the limit) is a hard error, not a truncated file.
    if frames_len > 0x0FFF_FFFF {
        return Err(FormatError::TooLarge);
    }
    header.extend_from_slice(&syncsafe(frames_len as u32));
    segments.insert(0, Segment::Inline(header));

    Ok((segments, 10 + frames_len))
}

/// Build the synthesized region for an MP3: a fresh ID3v2.4 tag (text frames +
/// APIC frames, with image bytes streamed as `ArtImage` segments) followed by the
/// backing audio.
pub fn synthesize_layout(
    audio_offset: u64,
    audio_length: u64,
    tags: &[TagInput],
    arts: &[ArtInput],
) -> Result<RegionLayout> {
    let (mut segments, _tag_len) = build_id3v2_segments(tags, arts)?;
    segments.push(Segment::BackingAudio {
        offset: audio_offset,
        len: audio_length,
    });
    RegionLayout::validated(segments).map_err(|_| FormatError::InvalidLayout)
}

/// Returns false when `data` begins with an ID3v2 tag whose declared frame sizes
/// could drive an unbounded allocation in the `id3` crate (which eagerly
/// `with_capacity`s a frame's declared size — and ID3v2.3 frame sizes are plain
/// 32-bit, up to 4 GiB). When false, callers skip ID3 parsing (yielding no tags
/// for that file) rather than risk an OOM. Conservative: tags using an extended
/// header or unsynchronisation, a malformed synchsafe body/frame-size field
/// (any byte with high bit set), or an unrecognised major version are skipped
/// (those files lose scan-time tag extraction, but cannot OOM the scanner).
/// Files without an ID3v2 tag return true (the id3 crate handles them cheaply).
fn id3v2_alloc_safe(data: &[u8]) -> bool {
    // id3::Tag::read_from2 scans forward to locate a tag, so handing it any
    // buffer that is not a validated ID3v2 tag at offset 0 risks the unbounded
    // allocation we are guarding against. Only parse when an ID3v2 header is at
    // offset 0 (and its frames validate, below). Trade-off: scan-time tag
    // extraction for ID3v1-only files (no leading ID3v2 header) is skipped;
    // ID3v1 is legacy/fixed-size and tags can be populated via the DB
    // (beets/picard) regardless.
    if data.len() < 10 || &data[0..3] != b"ID3" {
        return false;
    }
    let major = data[3];
    if !matches!(major, 2..=4) {
        return false;
    }
    let flags = data[5];
    // Extended header (0x40) and unsynchronisation (0x80) complicate frame
    // bounds; skip rather than risk mis-validating.
    if flags & 0xC0 != 0 {
        return false;
    }
    // A well-formed synchsafe integer has the high bit clear in every byte.  If
    // any byte has the high bit set the field is malformed; the id3 crate may
    // not mask those bits, producing a body size much larger than our
    // spec-correct synchsafe decode and walking frames we have not validated.
    // Reject such tags conservatively rather than risk mis-validating bounds.
    if data[6] | data[7] | data[8] | data[9] >= 0x80 {
        return false;
    }
    let body = synchsafe_decode(&data[6..10]) as usize;
    let Some(tag_end) = 10usize.checked_add(body) else {
        return false;
    };
    if tag_end > data.len() {
        return false;
    }
    let header_len = if major == 2 { 6 } else { 10 };
    // Walk frames over the entire remaining buffer (not just [10, tag_end)):
    // the id3 crate does not consistently stop at the declared tag body and
    // can walk and allocate from bytes beyond tag_end.  Any incomplete frame
    // header visible in data (i.e. pos + header_len <= data.len()) is also
    // validated.  We still reject if a frame's declared size exceeds tag_end.
    let scan_end = data.len();
    let mut pos = 10usize;
    while pos + header_len <= scan_end {
        // A zero first id byte marks the start of the padding region.
        if data[pos] == 0 {
            break;
        }
        // CHAP and CTOC frames contain embedded sub-frames; the id3 crate
        // allocates based on those sub-frame sizes, creating a recursive OOM
        // vector.  Reject tags containing either frame type (v2.3/v2.4 only;
        // v2.2 uses 3-byte frame ids and never defines chapter frames).
        if major != 2 && (&data[pos..pos + 4] == b"CHAP" || &data[pos..pos + 4] == b"CTOC") {
            return false;
        }
        let size = if major == 2 {
            ((data[pos + 3] as usize) << 16)
                | ((data[pos + 4] as usize) << 8)
                | (data[pos + 5] as usize)
        } else if major == 3 {
            // ID3v2.3: plain 32-bit big-endian frame size.
            // Frame flags at pos+8..pos+10: reject any non-zero flags.  The id3
            // crate handles COMPRESSION (0x0080) by subtracting 4 from the size
            // (panicking if size < 4), and ENCRYPTION/GROUPING_IDENTITY by
            // returning errors; rejecting all non-zero frame flags avoids those
            // paths entirely.
            if data[pos + 8] != 0 || data[pos + 9] != 0 {
                return false;
            }
            u32::from_be_bytes([data[pos + 4], data[pos + 5], data[pos + 6], data[pos + 7]])
                as usize
        } else {
            // ID3v2.4: synchsafe frame size.  Reject if any byte has its high
            // bit set (malformed synchsafe), for the same reason as the body.
            // Also reject non-zero frame flags for the same reasons as v2.3.
            if data[pos + 4] | data[pos + 5] | data[pos + 6] | data[pos + 7] >= 0x80 {
                return false;
            }
            if data[pos + 8] != 0 || data[pos + 9] != 0 {
                return false;
            }
            synchsafe_decode(&data[pos + 4..pos + 8]) as usize
        };
        let data_start = pos + header_len;
        // Reject if the frame header itself extends past the declared tag body,
        // or if the frame payload claims more bytes than the remaining body.
        // The id3 crate would otherwise attempt to subtract or allocate with
        // an invalid size, causing a panic or OOM.
        if data_start > tag_end || size > tag_end - data_start {
            return false;
        }
        pos = data_start + size;
        // Stop once we have walked past the declared tag body: any subsequent
        // bytes are audio or trailing tags, not ID3v2 frames.
        if pos >= tag_end {
            break;
        }
    }
    true
}

/// Extract all APIC pictures from an MP3's ID3v2 tag as embedded pictures, for
/// scan-time art ingestion. Returns empty if there is no tag or no pictures.
pub fn read_pictures(data: &[u8]) -> Vec<EmbeddedPicture> {
    if !id3v2_alloc_safe(data) {
        return Vec::new();
    }
    let Ok(tag) = id3::Tag::read_from2(std::io::Cursor::new(data)) else {
        return Vec::new();
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

/// Read an existing ID3v2 tag and fold it into canonical `(key, value)` pairs.
/// Text frames map via the vocabulary (NUL-separated multi-value yields one pair
/// per value); unmapped text frames pass through keyed by their frame id; `TXXX`
/// frames key on their description (folded to canonical when known); `COMM`/`USLT`
/// yield `comment`/`lyrics` (text only). Other/binary frames are skipped.
/// Multiple `COMM` or `USLT` frames (e.g. one per language) each emit a separate
/// pair; their language and description fields are not preserved.
pub fn read_tags(data: &[u8]) -> Vec<(String, String)> {
    if !id3v2_alloc_safe(data) {
        return Vec::new();
    }
    let Ok(tag) = id3::Tag::read_from2(std::io::Cursor::new(data)) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for frame in tag.frames() {
        let content = frame.content();
        if let Some(et) = content.extended_text() {
            let key = crate::tagmap::id3_txxx_to_key(&et.description)
                .map_or_else(|| et.description.clone(), str::to_string);
            out.push((key, et.value.clone()));
        } else if let Some(c) = content.comment() {
            out.push(("comment".to_string(), c.text.clone()));
        } else if let Some(l) = content.lyrics() {
            out.push(("lyrics".to_string(), l.text.clone()));
        } else if let Some(text) = content.text() {
            let id = frame.id();
            let key =
                crate::tagmap::id3_text_to_key(id).map_or_else(|| id.to_string(), str::to_string);
            for value in text.split('\0').filter(|v| !v.is_empty()) {
                out.push((key.clone(), value.to_string()));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a minimal ID3v2.3 tag with a single frame whose declared size
    /// overflows the tag bounds, and assert the guard rejects it.
    #[test]
    fn id3v2_guard_rejects_oversized_v23_frame() {
        // Tag header: b"ID3" major=3 rev=0 flags=0
        // Synchsafe body size encoding 10 (= one 10-byte frame header, no payload):
        //   syncsafe(10) = [0, 0, 0, 0x0A]
        // Frame: id=TIT2 (4 bytes), size=0xFFFF_FFFF (4 bytes, plain BE), flags=0x00 0x00
        let mut bytes: Vec<u8> = Vec::new();
        bytes.extend_from_slice(b"ID3");
        bytes.push(0x03); // major version 2.3
        bytes.push(0x00); // revision
        bytes.push(0x00); // flags: no extended header, no unsync
                          // synchsafe body = 10 (covers exactly one 10-byte frame header)
        bytes.extend_from_slice(&[0x00, 0x00, 0x00, 0x0A]);
        // Frame header: id "TIT2", size 0xFFFF_FFFF (big-endian, plain 32-bit)
        bytes.extend_from_slice(b"TIT2");
        bytes.extend_from_slice(&[0xFF, 0xFF, 0xFF, 0xFF]);
        bytes.extend_from_slice(&[0x00, 0x00]); // frame flags

        assert!(
            !id3v2_alloc_safe(&bytes),
            "guard should reject frame claiming more bytes than the tag holds"
        );
        // Must return quickly without OOM and produce no tags.
        assert!(
            read_tags(&bytes).is_empty(),
            "read_tags must return empty for unsafe tag"
        );
    }

    /// A buffer that does not start with "ID3" must be rejected by the guard.
    /// id3::Tag::read_from2 scans forward to locate a tag, so any non-ID3-prefixed
    /// buffer is unsafe regardless of what bytes appear later.
    #[test]
    fn id3v2_guard_rejects_non_id3_prefixed() {
        // Plain non-ID3 bytes.
        assert!(
            !id3v2_alloc_safe(b"RIFF....just not an id3 tag...."),
            "guard must reject buffer not starting with ID3"
        );
        assert!(
            read_tags(b"RIFF....just not an id3 tag....").is_empty(),
            "read_tags must return empty for non-ID3-prefixed buffer"
        );

        // The WAV crash vector: "RIFF..." body whose bytes do not start with "ID3"
        // but contain a nested ID3v2.3 tag with a TDA frame declaring ~4 GiB.
        // Extracted from fuzz/artifacts/wav/oom-4a21767820d5f05328f01d975fb6d3314f3fb902:
        // the ID3 chunk body starts at offset 0x18 and begins with "RIFF".
        const RIFF_BODY: &[u8] = &[
            0x52, 0x49, 0x46, 0x46, 0x32, 0x00, 0x00, 0x00, // "RIFF2..."
            0x57, 0x41, 0x56, 0x45, // "WAVE"
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x4c, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x49, 0x44, 0x33, 0x20, // nested "ID3 " fourcc
            0x15, 0x00, 0x00, 0x00, // chunk size = 21
            0x49, 0x44, 0x33, // "ID3" — nested tag starts here
            0x03, 0x00, 0x00, 0x00, 0xf7, 0x00, 0x00, 0x54, 0x44, 0x41, 0x03, 0xf6, 0x00, 0x00,
            0x00, // TDA frame size = 0xF600_0000 (~4 GiB)
        ];
        assert!(
            !id3v2_alloc_safe(RIFF_BODY),
            "guard must reject RIFF-prefixed buffer (WAV crash vector)"
        );
        assert!(
            read_tags(RIFF_BODY).is_empty(),
            "read_tags must return empty for RIFF-prefixed buffer"
        );
    }

    /// Write a real ID3v2.4 tag via the id3 crate and confirm the guard allows it
    /// and that read_tags extracts the expected values.
    #[test]
    fn id3v2_guard_allows_valid_tag() {
        use id3::{Tag, TagLike, Version};

        let mut tag = Tag::new();
        tag.set_text("TIT2", "Hello");
        tag.set_text("TPE1", "Artist");
        let mut buf = Vec::new();
        tag.write_to(&mut buf, Version::Id3v24).unwrap();

        assert!(
            id3v2_alloc_safe(&buf),
            "guard should allow a well-formed tag written by the id3 crate"
        );
        let tags = read_tags(&buf);
        assert!(
            tags.contains(&("title".to_string(), "Hello".to_string())),
            "missing title in {tags:?}"
        );
        assert!(
            tags.contains(&("artist".to_string(), "Artist".to_string())),
            "missing artist in {tags:?}"
        );
    }

    /// Replay fuzz-discovered crash artifacts: tags that would OOM the id3 crate.
    /// The guard must reject all of them and return empty without allocating.
    #[test]
    fn read_tags_handles_oom_crash_input_safely() {
        // Artifact 1 (oom-a9b766b...): 30-byte ID3v2.3 tag with flags=0xf0
        // (extended header + unsync bits set).  Guard rejects via flags & 0xC0.
        // xxd fuzz/artifacts/mp3/oom-a9b766b841c2a964e72b01f31c174f25bf11b2d2
        const CRASH1: &[u8] = &[
            0x49, 0x44, 0x33, // "ID3"
            0x03, 0xf0, // major=3, flags=0xf0 (extended header + unsync)
            0x00, 0x00, 0xf9, 0x2d, // synchsafe body size
            0x49, 0x50, 0x4c, 0x53, // frame id "IPLS"
            0x00, 0xf9, 0x3d, 0x02, // frame size (big-endian)
            0x00, 0x2d, 0x01, 0x00, // frame flags + data
            0x00, 0x03, 0x00, 0x49, 0x07, 0x10, 0xff, 0x07, 0xfe,
        ];
        // Artifact 2 (oom-54f1f5e1...): 26-byte ID3v2.3 tag with a malformed
        // synchsafe body field (data[9]=0x80, high bit set).  The id3 crate
        // treated the raw value as 128, walked the oversized IPLS frame, and
        // OOMed.  Guard rejects via the high-bit check on body bytes.
        // xxd fuzz/artifacts/mp3/oom-54f1f5e197c4aa191f4aac77bc263939a4e4ee83
        const CRASH2: &[u8] = &[
            0x49, 0x44, 0x33, // "ID3"
            0x03, 0x00, // major=3, flags=0 (no extended header / unsync)
            0x00, 0x00, 0x00, 0x80, // body bytes: data[9]=0x80 — malformed synchsafe
            0x0a, 0x27, 0x2f, 0x00, // frame id (partial)
            0xff, 0xee, 0x01, 0x00, // frame size declares ~4 GB
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x0a, 0x2f,
        ];
        for (i, crash) in [CRASH1, CRASH2].iter().enumerate() {
            assert!(
                read_tags(crash).is_empty(),
                "read_tags must be safe on crash artifact {i}"
            );
        }
    }

    #[test]
    fn read_tags_captures_txxx_comm_uslt_and_unmapped_text() {
        use id3::frame::{Comment, ExtendedText, Lyrics};
        use id3::{Tag, TagLike, Version}; // TagLike brings set_text/add_frame into scope

        let mut tag = Tag::new();
        tag.set_text("TIT2", "Song");
        tag.set_text("TBPM", "120"); // standard frame, not in vocabulary
        tag.add_frame(ExtendedText {
            description: "MOOD".into(),
            value: "happy".into(),
        });
        tag.add_frame(ExtendedText {
            description: "REPLAYGAIN_TRACK_GAIN".into(),
            value: "-6.5 dB".into(),
        });
        tag.add_frame(Comment {
            lang: "eng".into(),
            description: String::new(),
            text: "nice".into(),
        });
        tag.add_frame(Lyrics {
            lang: "eng".into(),
            description: String::new(),
            text: "la la".into(),
        });

        let mut buf = Vec::new();
        tag.write_to(&mut buf, Version::Id3v24).unwrap();

        let tags = read_tags(&buf);
        assert!(tags.contains(&("title".to_string(), "Song".to_string())));
        assert!(tags.contains(&("TBPM".to_string(), "120".to_string())));
        assert!(tags.contains(&("MOOD".to_string(), "happy".to_string())));
        assert!(tags.contains(&("replaygain_track_gain".to_string(), "-6.5 dB".to_string())));
        assert!(tags.contains(&("comment".to_string(), "nice".to_string())));
        assert!(tags.contains(&("lyrics".to_string(), "la la".to_string())));
    }

    #[test]
    fn synthesize_round_trips_arbitrary_id3_tags() {
        let tags = vec![
            TagInput::new("title", "Song"),
            TagInput::new("TBPM", "120"),     // unmapped standard frame
            TagInput::new("MyRating", "5"),   // user-defined -> TXXX
            TagInput::new("comment", "nice"), // -> COMM
            TagInput::new("lyrics", "la la"), // -> USLT
            TagInput::new("replaygain_track_gain", "-3.21 dB"), // -> TXXX (fixed desc)
        ];
        let (segments, _len) = build_id3v2_segments(&tags, &[]).unwrap();
        let mut buf = Vec::new();
        for seg in &segments {
            if let Segment::Inline(bytes) = seg {
                buf.extend_from_slice(bytes);
            }
        }
        let read = read_tags(&buf);
        for expected in [
            ("title", "Song"),
            ("TBPM", "120"),
            ("MyRating", "5"),
            ("comment", "nice"),
            ("lyrics", "la la"),
            ("replaygain_track_gain", "-3.21 dB"),
        ] {
            assert!(
                read.contains(&(expected.0.to_string(), expected.1.to_string())),
                "missing {expected:?} in {read:?}"
            );
        }
    }

    #[test]
    fn synchsafe_decode_assembles_7bit_groups() {
        // (1<<21)|(2<<14)|(3<<7)|4
        assert_eq!(synchsafe_decode(&[0x01, 0x02, 0x03, 0x04]), 0x0020_8184);
        // high bit of each byte masked (& 0x7F): 0xFF -> 0x7F per group.
        assert_eq!(synchsafe_decode(&[0xFF, 0xFF, 0xFF, 0xFF]), 0x0FFF_FFFF);
        // only the top group set -> pins the `<<21` (kills `<<21 -> >>21`).
        assert_eq!(synchsafe_decode(&[0x7F, 0x00, 0x00, 0x00]), 0x0FE0_0000);
        // only the second group set -> pins the `<<14` (kills `<<14 -> >>14`).
        assert_eq!(synchsafe_decode(&[0x00, 0x7F, 0x00, 0x00]), 0x001F_C000);
    }

    #[test]
    fn syncsafe_encodes_and_round_trips() {
        // pins the `>>21` and `>>14` group extraction.
        assert_eq!(syncsafe(0x0FE0_0000), [0x7F, 0x00, 0x00, 0x00]);
        assert_eq!(syncsafe(0x001F_C000), [0x00, 0x7F, 0x00, 0x00]);
        // round-trip over the full 28-bit range pins every group boundary.
        for n in [0u32, 1, 127, 128, 0x0123_4567, 0x0FFF_FFFF] {
            assert_eq!(synchsafe_decode(&syncsafe(n)), n);
        }
    }

    #[test]
    fn locate_audio_no_id3_starts_at_zero() {
        // >=10 bytes, not "ID3": original skips the ID3 block (audio at 0). The
        // `&& -> ||` mutant enters the block, decodes garbage, and returns Err — so
        // this unwrap kills it. Frame sync 0xFF 0xFB at offset 0.
        let data = [0xFF, 0xFB, 0x90, 0x00, 0, 0, 0, 0, 0, 0];
        let b = locate_audio(&data).unwrap();
        assert_eq!(b.audio_offset, 0);
        assert_eq!(b.audio_length, 10);
    }

    #[test]
    fn locate_audio_skips_id3v2_then_finds_sync() {
        // "ID3" v2.4, flags=0, synchsafe body=4 -> tag_len=14. Sync at offset 14.
        let mut data = Vec::new();
        data.extend_from_slice(b"ID3");
        data.extend_from_slice(&[0x04, 0x00, 0x00]); // major, rev, flags
        data.extend_from_slice(&[0x00, 0x00, 0x00, 0x04]); // synchsafe body=4
        data.extend_from_slice(&[0xAA, 0xBB, 0xCC, 0xDD]); // 4 body bytes
        data.extend_from_slice(&[0xFF, 0xFB, 0x90, 0x00]); // audio sync at 14
        let b = locate_audio(&data).unwrap();
        assert_eq!(b.audio_offset, 14);
        assert_eq!(b.audio_length, 4);
    }

    #[test]
    fn locate_audio_honors_footer_flag() {
        // footer flag (0x10) adds 10 to tag_len. body=0 -> tag_len = 10+0+10 = 20.
        // Sync at offset 20. The `+= -> -=`/`*=` mutant computes the wrong tag_len
        // and the sync check lands on the wrong byte -> Err (kills the `+=`).
        let mut data = Vec::new();
        data.extend_from_slice(b"ID3");
        data.extend_from_slice(&[0x04, 0x00, 0x10]); // flags: footer present
        data.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]); // synchsafe body=0
        data.extend_from_slice(&[0u8; 10]); // 10-byte footer region
        data.extend_from_slice(&[0xFF, 0xFB, 0x90, 0x00]); // sync at offset 20
        let b = locate_audio(&data).unwrap();
        assert_eq!(b.audio_offset, 20);
    }

    #[test]
    fn locate_audio_requires_frame_sync() {
        // data[0]=0xFF but data[1] lacks the 0xE0 sync bits: original rejects
        // (NotMp3). The `|| -> &&` mutant accepts (only rejects if ALL conditions
        // hold). The `+ -> *` on data[audio_offset+1] would read data[0] instead of
        // data[1]; with distinct bytes the sync decision flips.
        let data = [0xFF, 0x00, 0x00, 0x00, 0, 0, 0, 0, 0, 0];
        assert_eq!(locate_audio(&data), Err(FormatError::NotMp3));
        // 1-byte buffer: original NotMp3 (audio_offset+1 >= len). The `+ -> *`
        // mutant computes 0*1=0 >= 1 = false, falls through, and panics on data[1].
        assert_eq!(locate_audio(&[0xFF]), Err(FormatError::NotMp3));
    }

    #[test]
    fn push_frame_header_size_boundary_is_inclusive() {
        // ID3v2.4 frame size is a 28-bit syncsafe field; the guard rejects
        // data_len > 0x0FFF_FFFF. 0x0FFF_FFFF is the inclusive max (Ok); +1 errors.
        let mut out = Vec::new();
        assert!(push_frame_header(&mut out, b"TIT2", 0x0FFF_FFFF).is_ok());
        let mut over = Vec::new();
        assert_eq!(
            push_frame_header(&mut over, b"TIT2", 0x1000_0000),
            Err(FormatError::TooLarge)
        );
    }

    #[test]
    fn is_id3_text_frame_id_classifies_text_frames() {
        assert!(is_id3_text_frame_id("TPE1")); // T + 3 upper/digit, not TXXX
        assert!(is_id3_text_frame_id("TIT2"));
        assert!(!is_id3_text_frame_id("TXXX")); // excluded (kills `!= -> ==`)
        assert!(!is_id3_text_frame_id("COMM")); // not T-prefixed
        assert!(!is_id3_text_frame_id("TPE")); // wrong length
        assert!(!is_id3_text_frame_id("Txx1")); // lowercase -> false
    }

    #[test]
    fn build_id3v2_segments_emits_standard_text_frame_as_itself() {
        // A 4-char T-frame key (TPE1) must round-trip as a TPE1 frame, not TXXX.
        // The `is_id3_text_frame_id` match-guard `-> false` mutant would route it to
        // the TXXX branch, so read_tags would surface it under a different key.
        let tags = vec![TagInput::new("TPE1", "Band")];
        let (segments, _len) = build_id3v2_segments(&tags, &[]).unwrap();
        let mut buf = Vec::new();
        for seg in &segments {
            if let Segment::Inline(b) = seg {
                buf.extend_from_slice(b);
            }
        }
        // The literal frame id "TPE1" must appear in the emitted tag bytes.
        assert!(
            buf.windows(4).any(|w| w == b"TPE1"),
            "TPE1 frame not emitted: routed elsewhere"
        );
        // And it round-trips to the mapped key (artist), not a TXXX user field.
        let read = read_tags(&buf);
        assert!(
            read.contains(&("artist".to_string(), "Band".to_string())),
            "got {read:?}"
        );
    }

    #[test]
    fn build_id3v2_segments_rejects_oversized_total_tag() {
        // The total-tag guard rejects frames_len > 0x0FFF_FFFF. An APIC art whose
        // data_len (a count, not allocated) pushes the total just over the limit
        // must error; one byte under must succeed.
        let mk = |data_len: u64| ArtInput {
            art_id: 1,
            mime: "image/png".to_string(),
            description: String::new(),
            picture_type: 3,
            width: 0,
            height: 0,
            data_len,
        };
        assert_eq!(
            build_id3v2_segments(&[], &[mk(0x1000_0000)]).err(),
            Some(FormatError::TooLarge)
        );
        assert!(build_id3v2_segments(&[], &[mk(16)]).is_ok());
        // Exact boundary: compute the APIC framing overhead, then place
        // frames_len exactly on 0x0FFF_FFFF (one byte under must succeed) and
        // 0x1_0000_0000 (must error). This pins the `> -> >=` mutation.
        let (_, total_at_zero) = build_id3v2_segments(&[], &[mk(0)]).unwrap();
        let overhead = total_at_zero - 10; // frames_len when data_len=0
        let boundary_data_len = 0x0FFF_FFFF - overhead;
        assert!(
            build_id3v2_segments(&[], &[mk(boundary_data_len)]).is_ok(),
            "exact boundary (frames_len == 0x0FFF_FFFF) should be accepted"
        );
        assert_eq!(
            build_id3v2_segments(&[], &[mk(boundary_data_len + 1)]).err(),
            Some(FormatError::TooLarge),
            "one byte past boundary must be rejected"
        );
    }

    /// Independent synchsafe encoder for fixtures (does NOT call `syncsafe`, so a
    /// mutation there cannot mask a fixture).
    fn ss(n: u32) -> [u8; 4] {
        [
            ((n >> 21) & 0x7F) as u8,
            ((n >> 14) & 0x7F) as u8,
            ((n >> 7) & 0x7F) as u8,
            (n & 0x7F) as u8,
        ]
    }

    /// Build an ID3v2 tag: "ID3", `major`, rev=0, `flags`, synchsafe `body` size,
    /// then the raw `frames` bytes.
    fn id3v2(major: u8, flags: u8, body: u32, frames: &[u8]) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(b"ID3");
        v.push(major);
        v.push(0x00);
        v.push(flags);
        v.extend_from_slice(&ss(body));
        v.extend_from_slice(frames);
        v
    }

    #[test]
    fn alloc_safe_accepts_minimal_valid_header() {
        // 10-byte v2.4 header, body=0, no frames -> safe. This is exactly the
        // len==10 boundary, so the `< -> <=` mutant (10<=10 -> reject) flips it.
        let tag = id3v2(0x04, 0x00, 0, &[]);
        assert_eq!(tag.len(), 10);
        assert!(id3v2_alloc_safe(&tag));
    }

    #[test]
    fn alloc_safe_rejects_short_and_non_id3() {
        // "ID3" + 2 bytes (len 5, marker correct): original returns false (len<10).
        // `< -> ==` (5==10 false) and `|| -> &&` (true && false) both fall through
        // and panic reading data[5]. Asserting `!safe` kills them.
        assert!(!id3v2_alloc_safe(b"ID3xx"));
        // Right length, wrong marker -> false.
        assert!(!id3v2_alloc_safe(b"XXX\x04\x00\x00\x00\x00\x00\x00"));
    }

    #[test]
    fn alloc_safe_rejects_bad_version_and_header_flags() {
        // major outside 2..=4 -> false (kills the `matches!(major, 2..=4)` mutations).
        assert!(!id3v2_alloc_safe(&id3v2(0x05, 0x00, 0, &[])));
        assert!(!id3v2_alloc_safe(&id3v2(0x01, 0x00, 0, &[])));
        // extended-header (0x40) or unsync (0x80) -> false (kills `& 0xC0` mutations).
        assert!(!id3v2_alloc_safe(&id3v2(0x04, 0x40, 0, &[])));
        assert!(!id3v2_alloc_safe(&id3v2(0x04, 0x80, 0, &[])));
    }

    #[test]
    fn alloc_safe_rejects_high_bit_in_body_size() {
        // Two body-size bytes with the high bit set: OR = 0x80 (reject). The
        // `| -> ^` mutant gives 0x80^0x80 = 0 (accept); `| -> &` gives 0x80&0x80&0&0
        // = 0 (accept). Built by hand because `ss()` would clear the high bits.
        let tag = vec![b'I', b'D', b'3', 0x04, 0x00, 0x00, 0x80, 0x80, 0x00, 0x00];
        assert!(!id3v2_alloc_safe(&tag));
        // Single high-bit byte still rejected (pins the `>= 0x80` comparison).
        let tag1 = vec![b'I', b'D', b'3', 0x04, 0x00, 0x00, 0x00, 0x00, 0x00, 0x80];
        assert!(!id3v2_alloc_safe(&tag1));
    }

    #[test]
    fn alloc_safe_rejects_high_bit_in_v24_frame_size() {
        // v2.4 frame size is synchsafe; two size bytes with the high bit set must be
        // rejected (whole-byte OR check on data[pos+4..pos+8]). The frame is 10 bytes
        // (4 id + 4 size + 2 flags), so body=10 makes tag_end == len (20): the walk
        // is entered (NOT short-circuited by `tag_end > data.len()`) and the high-bit
        // check fires.
        let mut frame = b"TIT2".to_vec();
        frame.extend_from_slice(&[0x80, 0x80, 0x00, 0x00]); // size bytes, two high bits
        frame.extend_from_slice(&[0x00, 0x00]); // frame flags
        let tag = id3v2(0x04, 0x00, 10, &frame);
        assert!(!id3v2_alloc_safe(&tag));
    }

    /// A valid ID3v2.3 frame: 4-byte id, 4-byte plain big-endian size, 2 flag bytes,
    /// then `payload`.
    fn v23_frame(id: &[u8; 4], size: u32, payload: &[u8]) -> Vec<u8> {
        let mut v = id.to_vec();
        v.extend_from_slice(&size.to_be_bytes());
        v.extend_from_slice(&[0x00, 0x00]);
        v.extend_from_slice(payload);
        v
    }

    #[test]
    fn alloc_safe_v22_24bit_size_decode() {
        // v2.2 frame header is 6 bytes: 3-byte id + 3-byte 24-bit big-endian size.
        // Declare a size that the *correct* decode puts out of bounds (reject), so a
        // wrong shift/OR that shrinks the size would wrongly accept.
        // size bytes [0x00,0x01,0x00] = 256, body = 6 (header only, no room) -> reject.
        let mut f_mid = b"TT2".to_vec();
        f_mid.extend_from_slice(&[0x00, 0x01, 0x00]); // 24-bit size = 256
        assert!(!id3v2_alloc_safe(&id3v2(0x02, 0x00, 6, &f_mid))); // kills <<8 and |->&
                                                                   // size bytes [0x01,0x00,0x00] = 65536 -> reject; `<<16 -> >>16` shrinks to 0.
        let mut f_hi = b"TT2".to_vec();
        f_hi.extend_from_slice(&[0x01, 0x00, 0x00]);
        assert!(!id3v2_alloc_safe(&id3v2(0x02, 0x00, 6, &f_hi)));
        // A valid in-bounds v2.2 frame is accepted: size 4, body = 6+4 = 10.
        let mut f_ok = b"TT2".to_vec();
        f_ok.extend_from_slice(&[0x00, 0x00, 0x04]);
        f_ok.extend_from_slice(&[1, 2, 3, 4]);
        assert!(id3v2_alloc_safe(&id3v2(0x02, 0x00, 10, &f_ok)));
    }

    #[test]
    fn alloc_safe_rejects_nonzero_frame_flags() {
        // v2.3: non-zero frame flags -> reject (the v2.3 flag check).
        let mut f3 = b"TIT2".to_vec();
        f3.extend_from_slice(&4u32.to_be_bytes()); // plain size 4
        f3.extend_from_slice(&[0x00, 0x01]); // non-zero frame flags
        f3.extend_from_slice(&[1, 2, 3, 4]);
        assert!(!id3v2_alloc_safe(&id3v2(0x03, 0x00, 14, &f3)));

        // v2.4: non-zero frame flags -> reject. This is a SEPARATE code path (the
        // v2.4 `else` branch) from the v2.3 check, so it needs its own fixture.
        let mut f4 = b"TIT2".to_vec();
        f4.extend_from_slice(&ss(4)); // valid synchsafe size 4
        f4.extend_from_slice(&[0x00, 0x01]); // non-zero frame flags
        f4.extend_from_slice(&[1, 2, 3, 4]);
        assert!(!id3v2_alloc_safe(&id3v2(0x04, 0x00, 14, &f4)));
    }

    #[test]
    fn alloc_safe_rejects_chap_and_ctoc() {
        // CHAP/CTOC carry sub-frames -> recursive OOM vector -> reject (v2.3/2.4).
        let chap = v23_frame(b"CHAP", 4, &[1, 2, 3, 4]);
        assert!(!id3v2_alloc_safe(&id3v2(0x03, 0x00, 14, &chap)));
        let ctoc = v23_frame(b"CTOC", 4, &[1, 2, 3, 4]);
        assert!(!id3v2_alloc_safe(&id3v2(0x03, 0x00, 14, &ctoc)));
    }

    #[test]
    fn alloc_safe_frame_size_bounds() {
        // Frame exactly filling the body -> accept (size 4, body = 10+4 = 14).
        // data_start = 10+10 = 20, tag_end = 24, rem = 4, size 4 -> 4 > 4 is false.
        // Kills A `+ -> *` (data_start=100 -> 100>24 -> reject) and C `> -> >=`
        // (4 >= 4 -> reject).
        let ok = v23_frame(b"TIT2", 4, &[1, 2, 3, 4]);
        assert!(id3v2_alloc_safe(&id3v2(0x03, 0x00, 14, &ok)));
        // size one byte past the remainder -> reject (size 5: 5 > 24-20=4). Kills C
        // `> -> ==` (5==4 false -> accept), C `- -> +` (rem=44 -> 5>44 false ->
        // accept), D `|| -> &&` (false && true -> accept), and A `+ -> -`
        // (data_start=0 -> 5 > 24-0=24 false -> accept).
        let over = v23_frame(b"TIT2", 5, &[1, 2, 3, 4]);
        assert!(!id3v2_alloc_safe(&id3v2(0x03, 0x00, 14, &over)));
    }

    #[test]
    fn alloc_safe_data_start_equal_to_tag_end_is_ok() {
        // A size-0 frame: data_start (20) == tag_end (20). Original: `20 > 20` is
        // false -> accept. Kills B `> -> ==` (20==20 -> reject) and `> -> >=`.
        let zero = v23_frame(b"TIT2", 0, &[]);
        assert!(id3v2_alloc_safe(&id3v2(0x03, 0x00, 10, &zero)));
    }

    #[test]
    fn alloc_safe_rejects_bad_second_frame_in_body() {
        // Valid frame1 (size 2) then an out-of-bounds frame2 (size 100), both inside
        // the declared body (body=26, tag_end=36). Original walks to frame2 and
        // rejects. Kills E `+ -> *` (pos = 20*2 = 40 >= 36 -> break -> accept,
        // skipping frame2) and E `+ -> -` (pos = 20-2 = 18 -> data[18]==0 padding
        // break -> accept).
        let mut frames = v23_frame(b"TIT2", 2, &[0xAA, 0xBB]); // 12 bytes, 10..22
        frames.extend_from_slice(&v23_frame(b"TPE1", 100, &[1, 2, 3, 4])); // 14, 22..36
        assert!(!id3v2_alloc_safe(&id3v2(0x03, 0x00, 26, &frames)));
    }

    #[test]
    fn alloc_safe_stops_at_tag_body_end() {
        // A size-0 frame fills the body (tag_end=20), then a bad trailing frame
        // beyond tag_end but within the buffer. Original breaks at `pos >= tag_end`
        // (20 >= 20) and accepts without walking the trailing garbage. Kills F
        // `>= -> <` (20 < 20 false -> no break -> walks the bad frame -> reject).
        let mut frames = v23_frame(b"TIT2", 0, &[]); // 10 bytes, 10..20
        frames.extend_from_slice(&v23_frame(b"TPE1", 100, &[1, 2, 3, 4])); // 14, 20..34
        assert!(id3v2_alloc_safe(&id3v2(0x03, 0x00, 10, &frames)));
    }

    #[test]
    fn alloc_safe_walks_two_frames_and_stops_at_padding() {
        // Two valid frames (24 bytes, 10..34) then 10 padding zero bytes (34..44).
        // body=25 -> tag_end=35, so after frame2 (pos=34) `34 >= 35` is false (no
        // tag-end break); the next iteration enters (`34+10=44 <= 44`) and
        // `data[34] == 0` triggers the PADDING break. Kills I `== -> !=` (no break ->
        // walks zero bytes -> data_start past tag_end -> reject) and exercises the
        // multi-frame walk (E) and the while guard (G).
        let mut frames = v23_frame(b"TIT2", 2, &[0xAA, 0xBB]);
        frames.extend_from_slice(&v23_frame(b"TPE1", 2, &[0xCC, 0xDD]));
        frames.extend_from_slice(&[0u8; 10]); // >= header_len of padding so the walk re-enters
        assert!(id3v2_alloc_safe(&id3v2(0x03, 0x00, 25, &frames)));
    }

    #[test]
    fn alloc_safe_rejects_frame_size_exceeding_tag_end() {
        // Single frame claiming size 100 in a 14-byte body -> reject before any
        // allocation. Reinforces C.
        let huge = v23_frame(b"TIT2", 100, &[1, 2, 3, 4]);
        assert!(!id3v2_alloc_safe(&id3v2(0x03, 0x00, 14, &huge)));
    }
}
