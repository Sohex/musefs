use crate::error::{FormatError, Result};
use crate::input::{ArtInput, BinaryTagInput, EmbeddedBinaryTag, EmbeddedPicture, TagInput};
use crate::layout::{RegionLayout, Segment};
use crate::probe::Extent;

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

/// Bounded twin of [`locate_audio`]. `prefix` is a front window; `file_len` is the
/// true size; `tail` is the file's last 128 bytes (or `None` if the file is
/// shorter than 128 bytes). The audio start is the end of any leading ID3v2 tag
/// (declared in its 10-byte header); if that end is past the prefix, returns
/// `NeedMore`. The audio end is `file_len` minus a 128-byte ID3v1 trailer when the
/// `tail` begins with `TAG`.
pub fn locate_audio_bounded(
    prefix: &[u8],
    file_len: u64,
    tail: Option<&[u8; 128]>,
) -> Result<Extent<Mp3Bounds>> {
    let mut audio_offset = 0usize;
    if prefix.len() >= 10 && &prefix[0..3] == b"ID3" {
        let flags = prefix[5];
        let body = synchsafe_decode(&prefix[6..10]) as usize;
        let mut tag_len = 10 + body;
        if flags & 0x10 != 0 {
            tag_len += 10; // ID3v2.4 footer
        }
        if tag_len as u64 > file_len {
            return Err(FormatError::Malformed);
        }
        audio_offset = tag_len;
    } else if prefix.len() < 10 && file_len >= 10 {
        // Not enough bytes even to read the ID3v2 header.
        return Ok(Extent::NeedMore { up_to: 10 });
    }

    // The audio start (plus its 2-byte frame sync) must fit in the file. Mirrors
    // the unbounded `locate_audio`'s `audio_offset + 1 >= len` reject: without
    // this, a tag that claims audio begins at/after EOF would return `NeedMore`
    // with `up_to > file_len`, and the caller would widen to the full file and
    // get the same answer every retry instead of failing fast.
    if audio_offset as u64 + 2 > file_len {
        return Err(FormatError::NotMp3);
    }

    // Need the frame-sync pair at the audio offset to be inside the prefix.
    if audio_offset + 2 > prefix.len() {
        return Ok(Extent::NeedMore {
            up_to: (audio_offset + 2) as u64,
        });
    }

    if prefix[audio_offset] != 0xFF || (prefix[audio_offset + 1] & 0xE0) != 0xE0 {
        return Err(FormatError::NotMp3);
    }

    let mut audio_end = file_len;
    if let Some(tail) = tail {
        if file_len >= audio_offset as u64 + 128 && &tail[0..3] == b"TAG" {
            audio_end -= 128;
        }
    }

    Ok(Extent::Complete(Mp3Bounds {
        audio_offset: audio_offset as u64,
        audio_length: audio_end - audio_offset as u64,
    }))
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

/// POPM body: `<owner>\0<rating:u8>[<counter: 4-byte big-endian>]`. Owner is empty
/// by design (spec §5 — the original tagger identity is dropped). The counter is
/// emitted as 4 bytes when `playcount > 0` and omitted when 0; values above
/// `u32::MAX` are clamped (the typed read path caps at u64, the common case fits
/// u32).
fn popm_frame_data(rating: u8, playcount: u64) -> Vec<u8> {
    let mut d = Vec::new();
    d.push(0x00); // empty owner, NUL-terminated
    d.push(rating);
    if playcount > 0 {
        let c = u32::try_from(playcount).unwrap_or(u32::MAX);
        d.extend_from_slice(&c.to_be_bytes());
    }
    d
}

/// UFID body: `<owner>\0<identifier bytes>`.
fn ufid_frame_data(owner: &str, identifier: &[u8]) -> Vec<u8> {
    let mut d = Vec::new();
    d.extend_from_slice(owner.as_bytes());
    d.push(0x00);
    d.extend_from_slice(identifier);
    d
}

/// True for the canonical text keys that are rebuilt as POPM/UFID frames and must
/// therefore be excluded from the generic text/TXXX emission (no double-store).
fn is_promoted_key(key: &str) -> bool {
    matches!(key, "rating" | "playcount" | "musicbrainz_trackid")
}

/// Build the ID3v2.4 tag region for `tags`/`arts`: an inline 10-byte header
/// followed by text/`TXXX` frames and `APIC` frames whose image bytes are
/// streamed as `ArtImage` segments. Returns the segments (no backing audio) and
/// the total tag length (`10 + frames_len`). Shared by MP3 synthesis and the WAV
/// `id3 ` chunk.
pub fn build_id3v2_segments(
    tags: &[TagInput],
    binary_tags: &[BinaryTagInput],
    arts: &[ArtInput],
) -> Result<(Vec<Segment>, u64)> {
    // Pull the promoted scalar values out of `tags`: first `rating` /
    // `musicbrainz_trackid` wins, `playcount` takes the last parseable value. A
    // single POPM/UFID is the norm, so this only diverges from "first wins" for
    // the rare multi-frame tag.
    let mut popm_rating: Option<u8> = None;
    let mut popm_playcount: u64 = 0;
    let mut mbid: Option<String> = None;
    for t in tags {
        match t.key.as_str() {
            "rating" if popm_rating.is_none() => popm_rating = t.value.parse().ok(),
            "playcount" => popm_playcount = t.value.parse().unwrap_or(popm_playcount),
            "musicbrainz_trackid" if mbid.is_none() => mbid = Some(t.value.clone()),
            _ => {}
        }
    }

    // Group consecutive same-key values (the DB returns tags ordered by key),
    // skipping promoted keys so they never enter the generic text/TXXX path
    // (no double-store).
    let mut groups: Vec<(String, Vec<String>)> = Vec::new();
    for t in tags {
        if is_promoted_key(&t.key) {
            continue;
        }
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

    // Rebuilt promoted frames (POPM from rating/playcount, UFID from MBID).
    if let Some(rating) = popm_rating {
        let data = popm_frame_data(rating, popm_playcount);
        push_frame_header(&mut buf, b"POPM", data.len())?;
        buf.extend_from_slice(&data);
        frames_len += 10 + data.len() as u64;
    }
    if let Some(id) = &mbid {
        let data = ufid_frame_data(MUSICBRAINZ_UFID_OWNER, id.as_bytes());
        push_frame_header(&mut buf, b"UFID", data.len())?;
        buf.extend_from_slice(&data);
        frames_len += 10 + data.len() as u64;
    }

    // Opaque binary frames: header (inline) + streamed body (BinaryTag segment).
    for bt in binary_tags {
        if bt.len == 0 {
            // An empty BinaryTag fails `RegionLayout::validate` (`EmptySegment`).
            continue;
        }
        // Defensive: ID3 opaque keys are 4-byte frame ids.
        let Ok(id): std::result::Result<[u8; 4], _> = bt.key.as_bytes().try_into() else {
            continue;
        };
        push_frame_header(&mut buf, &id, bt.len as usize)?;
        segments.push(Segment::Inline(std::mem::take(&mut buf)));
        segments.push(Segment::BinaryTag {
            payload_id: bt.payload_id,
            len: bt.len,
        });
        frames_len += 10 + bt.len;
    }

    for art in arts {
        if art.data_len == 0 {
            // Skip degenerate empty art: an `ArtImage { len: 0 }` segment fails
            // `RegionLayout::validate` (`EmptySegment`) and would make the whole
            // track unreadable at serve time (finding #16).
            continue;
        }
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
    binary_tags: &[BinaryTagInput],
    arts: &[ArtInput],
) -> Result<RegionLayout> {
    let (mut segments, _tag_len) = build_id3v2_segments(tags, binary_tags, arts)?;
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

pub(crate) const MUSICBRAINZ_UFID_OWNER: &str = "http://musicbrainz.org";

/// Extract an ID3v2.3/2.4 tag's binary frames. Returns `(opaque, promoted)`:
/// - `opaque`: frames preserved **byte-exact** — `(frame-id, raw post-header body)`.
///   `PRIV`/`GEOB`/`SYLT`/`MCDI`/unknown frames and any non-MusicBrainz `UFID`.
/// - `promoted`: `(key, value)` text pairs — `POPM` → `rating` (raw 0–255) + `playcount`
///   (counter, omitted when 0); MusicBrainz `UFID` → `musicbrainz_trackid`. Promoted
///   frames are NOT in `opaque`.
///
/// Text (`T***`), `COMM`, `USLT`, `APIC` are handled by `read_tags`/`read_pictures`
/// and skipped. Gated by `id3v2_alloc_safe`, so the tag is well-formed, has no
/// unsynchronisation/extended header/frame flags, and bodies are sliced verbatim.
/// v2.2 (3-char ids) is not processed (rare; text/art still parse via the crate).
pub fn read_binary_tags(data: &[u8]) -> (Vec<EmbeddedBinaryTag>, Vec<(String, String)>) {
    let mut opaque = Vec::new();
    let mut promoted = Vec::new();
    if !id3v2_alloc_safe(data) || data[3] < 3 {
        return (opaque, promoted);
    }
    let tag_end = 10 + synchsafe_decode(&data[6..10]) as usize;
    let mut pos = 10usize;
    while pos + 10 <= tag_end {
        if data[pos] == 0 {
            break;
        }
        let id = &data[pos..pos + 4];
        let size = if data[3] == 3 {
            u32::from_be_bytes([data[pos + 4], data[pos + 5], data[pos + 6], data[pos + 7]])
                as usize
        } else {
            synchsafe_decode(&data[pos + 4..pos + 8]) as usize
        };
        let body_start = pos + 10;
        if body_start + size > tag_end {
            break;
        }
        classify_binary_frame(
            id,
            &data[body_start..body_start + size],
            &mut opaque,
            &mut promoted,
        );
        pos = body_start + size;
    }
    (opaque, promoted)
}

/// Classify one ID3v2 frame body into opaque-passthrough or promoted-text.
fn classify_binary_frame(
    id: &[u8],
    body: &[u8],
    opaque: &mut Vec<EmbeddedBinaryTag>,
    promoted: &mut Vec<(String, String)>,
) {
    // Handled by read_tags/read_pictures: text frames (T***), COMM, USLT, APIC.
    if id[0] == b'T' || id == b"COMM" || id == b"USLT" || id == b"APIC" {
        return;
    }
    match id {
        b"POPM" => {
            // <owner>\0<rating:u8>[<counter: big-endian>]
            if let Some(nul) = body.iter().position(|&b| b == 0) {
                if let Some((&rating, counter)) = body[nul + 1..].split_first() {
                    promoted.push(("rating".to_string(), rating.to_string()));
                    let c = counter
                        .iter()
                        .take(8)
                        .fold(0u64, |a, &b| (a << 8) | b as u64);
                    if c > 0 {
                        promoted.push(("playcount".to_string(), c.to_string()));
                    }
                }
            }
        }
        b"UFID" => {
            // <owner>\0<identifier>. MusicBrainz owner promotes; others opaque.
            match body.iter().position(|&b| b == 0) {
                Some(nul) if &body[..nul] == MUSICBRAINZ_UFID_OWNER.as_bytes() => {
                    promoted.push((
                        "musicbrainz_trackid".to_string(),
                        String::from_utf8_lossy(&body[nul + 1..]).into_owned(),
                    ));
                }
                _ => opaque.push(EmbeddedBinaryTag {
                    key: "UFID".to_string(),
                    payload: body.to_vec(),
                }),
            }
        }
        _ => {
            // Opaque verbatim: PRIV, GEOB, SYLT, MCDI, W***, unknown, … (4-byte ids).
            if id.iter().all(u8::is_ascii_graphic) {
                opaque.push(EmbeddedBinaryTag {
                    key: String::from_utf8_lossy(id).into_owned(),
                    payload: body.to_vec(),
                });
            }
        }
    }
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
        let (segments, _len) = build_id3v2_segments(&tags, &[], &[]).unwrap();
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
        let (segments, _len) = build_id3v2_segments(&tags, &[], &[]).unwrap();
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
            build_id3v2_segments(&[], &[], &[mk(0x1000_0000)]).err(),
            Some(FormatError::TooLarge)
        );
        assert!(build_id3v2_segments(&[], &[], &[mk(16)]).is_ok());
        // Exact boundary: compute the APIC framing overhead, then place
        // frames_len exactly on 0x0FFF_FFFF (one byte under must succeed) and
        // 0x1_0000_0000 (must error). This pins the `> -> >=` mutation. The
        // baseline art uses data_len=1 (not 0) because zero-byte art is skipped.
        let (_, total_at_one) = build_id3v2_segments(&[], &[], &[mk(1)]).unwrap();
        let overhead = total_at_one - 10 - 1; // frames_len = overhead + data_len
        let boundary_data_len = 0x0FFF_FFFF - overhead;
        assert!(
            build_id3v2_segments(&[], &[], &[mk(boundary_data_len)]).is_ok(),
            "exact boundary (frames_len == 0x0FFF_FFFF) should be accepted"
        );
        assert_eq!(
            build_id3v2_segments(&[], &[], &[mk(boundary_data_len + 1)]).err(),
            Some(FormatError::TooLarge),
            "one byte past boundary must be rejected"
        );
    }

    #[test]
    fn build_id3v2_segments_skips_zero_byte_art() {
        // A zero-byte APIC would emit `Segment::ArtImage { len: 0 }`, which
        // `RegionLayout::validate` rejects as `EmptySegment` -> the whole track
        // becomes unreadable at serve time. Degenerate empty art must be skipped
        // at synthesis (mirrors the FLAC fix for finding #16).
        let empty = ArtInput {
            art_id: 1,
            mime: "image/png".to_string(),
            description: String::new(),
            picture_type: 3,
            width: 0,
            height: 0,
            data_len: 0,
        };
        let (segments, _len) = build_id3v2_segments(&[], &[], &[empty]).unwrap();
        assert!(
            !segments
                .iter()
                .any(|s| matches!(s, Segment::ArtImage { .. })),
            "zero-byte art must not emit an ArtImage segment"
        );
        let mut buf = Vec::new();
        for seg in &segments {
            if let Segment::Inline(b) = seg {
                buf.extend_from_slice(b);
            }
        }
        assert!(
            !buf.windows(4).any(|w| w == b"APIC"),
            "zero-byte art must not emit an APIC frame"
        );
    }

    #[test]
    fn build_id3v2_segments_keeps_real_art_when_mixed_with_empty() {
        // An empty art alongside a real one: only the non-empty art is emitted.
        let mk = |art_id: i64, data_len: u64| ArtInput {
            art_id,
            mime: "image/png".to_string(),
            description: String::new(),
            picture_type: 3,
            width: 0,
            height: 0,
            data_len,
        };
        let (segments, _len) = build_id3v2_segments(&[], &[], &[mk(1, 0), mk(2, 16)]).unwrap();
        let art_segs: Vec<_> = segments
            .iter()
            .filter_map(|s| match s {
                Segment::ArtImage { art_id, len } => Some((*art_id, *len)),
                _ => None,
            })
            .collect();
        assert_eq!(
            art_segs,
            vec![(2_i64, 16_u64)],
            "only the non-empty art should be emitted"
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

    /// ID3v2 header declaring `body` bytes of tag, then a frame-sync byte pair,
    /// then `audio`. Returns (full, audio_offset).
    fn mp3_with_id3v2(body_len: usize, audio: &[u8]) -> (Vec<u8>, u64) {
        let mut v = b"ID3\x04\x00\x00".to_vec(); // version 2.4, no flags
        v.extend_from_slice(&syncsafe(body_len as u32));
        v.extend(std::iter::repeat_n(0u8, body_len)); // tag body
        let audio_offset = v.len() as u64;
        v.extend_from_slice(&[0xFF, 0xFB]); // MPEG frame sync
        v.extend_from_slice(audio);
        (v, audio_offset)
    }

    #[test]
    fn locate_audio_bounded_complete_with_no_id3v1() {
        let (full, audio_offset) = mp3_with_id3v2(8, b"frames");
        let prefix = &full[..audio_offset as usize + 2]; // covers tag + sync
        let file_len = full.len() as u64;
        match locate_audio_bounded(prefix, file_len, None).unwrap() {
            Extent::Complete(b) => {
                assert_eq!(b.audio_offset, audio_offset);
                assert_eq!(b.audio_length, file_len - audio_offset);
            }
            other @ Extent::NeedMore { .. } => panic!("expected Complete, got {other:?}"),
        }
    }

    #[test]
    fn locate_audio_bounded_needmore_when_tag_exceeds_prefix() {
        let (full, _audio_offset) = mp3_with_id3v2(4096, b"frames");
        let prefix = &full[..32]; // only the 10-byte header is present
        let file_len = full.len() as u64;
        match locate_audio_bounded(prefix, file_len, None).unwrap() {
            Extent::NeedMore { up_to } => assert_eq!(up_to, 10 + 4096 + 2),
            other @ Extent::Complete(_) => panic!("expected NeedMore, got {other:?}"),
        }
    }

    #[test]
    fn locate_audio_bounded_strips_id3v1_tail() {
        let (mut full, audio_offset) = mp3_with_id3v2(8, b"frames");
        let body_end = full.len();
        full.extend_from_slice(b"TAG"); // ID3v1 marker
        full.extend(std::iter::repeat_n(0u8, 125)); // 128-byte tag total
        let file_len = full.len() as u64;
        let tail: [u8; 128] = full[full.len() - 128..].try_into().unwrap();
        let prefix = &full[..audio_offset as usize + 2];
        match locate_audio_bounded(prefix, file_len, Some(&tail)).unwrap() {
            Extent::Complete(b) => {
                assert_eq!(b.audio_offset, audio_offset);
                assert_eq!(b.audio_length, body_end as u64 - audio_offset);
            }
            other @ Extent::NeedMore { .. } => panic!("expected Complete, got {other:?}"),
        }
    }

    #[test]
    fn locate_audio_bounded_rejects_audio_start_past_eof() {
        // An ID3v2 tag whose declared length leaves no room for the frame sync
        // (audio_offset == file_len). The bounded prober must fail fast with
        // `NotMp3` rather than loop on `NeedMore { up_to > file_len }`.
        let mut full = b"ID3\x04\x00\x00".to_vec();
        full.extend_from_slice(&syncsafe(8));
        full.extend(std::iter::repeat_n(0u8, 8)); // tag body; file ends here
        let file_len = full.len() as u64; // == tag end == audio_offset
        match locate_audio_bounded(&full, file_len, None) {
            Err(FormatError::NotMp3) => {}
            other => panic!("expected Err(NotMp3), got {other:?}"),
        }
    }

    // kills mp3 L75 (`prefix.len() >= 10 && &prefix[0..3] == b"ID3"`: `&&`->`||`).
    // A long (>=10) prefix that is NOT "ID3" and starts with a valid frame sync.
    // Correct (`&&`): the ID3 branch is skipped -> audio_offset stays 0 -> Complete
    // at offset 0. Under `||`: `len>=10 || "ID3"==..` is true, so it parses an ID3
    // header out of the non-ID3 bytes, computing a bogus tag_len and a wrong
    // audio_offset (or Malformed). Asserting audio_offset==0 kills it.
    #[test]
    fn locate_audio_bounded_plain_mp3_no_id3_starts_at_zero() {
        // 0xFF 0xFB frame sync at offset 0, then payload. len 12 (>= 10).
        let data = [0xFF, 0xFB, 0x90, 0x00, 1, 2, 3, 4, 5, 6, 7, 8];
        let file_len = data.len() as u64;
        match locate_audio_bounded(&data, file_len, None).unwrap() {
            Extent::Complete(b) => {
                assert_eq!(b.audio_offset, 0);
                assert_eq!(b.audio_length, file_len);
            }
            other @ Extent::NeedMore { .. } => {
                panic!("expected Complete at offset 0, got {other:?}")
            }
        }
    }

    // Reinforces L75 with a short non-ID3 prefix below the ID3-header length.
    // A 5-byte prefix that is not "ID3", file_len < 10. Correct (`&&`): the ID3
    // branch is false (len 5 < 10) AND the else-if at L86 is `len<10 && file_len>=10`
    // = `true && false` = false, so it proceeds; the frame sync at offset 0 is in
    // the prefix -> Complete. Under L75 `||`: `5>=10 || "ID3"==prefix[0..3]` ->
    // false || false is still false here, BUT the point is the `&&`->`||` mutant on
    // a len>=10 non-ID3 prefix (covered above). This case pins that a short non-ID3
    // prefix with a valid sync resolves to Complete (no panic indexing prefix[5..]).
    #[test]
    fn locate_audio_bounded_short_non_id3_with_small_file() {
        // 0xFF 0xFB sync at offset 0; file_len 5 (< 10).
        let data = [0xFF, 0xFB, 0x90, 0x00, 0x00];
        let file_len = data.len() as u64; // 5
        match locate_audio_bounded(&data, file_len, None).unwrap() {
            Extent::Complete(b) => {
                assert_eq!(b.audio_offset, 0);
                assert_eq!(b.audio_length, 5);
            }
            other @ Extent::NeedMore { .. } => panic!("expected Complete, got {other:?}"),
        }
    }

    // kills mp3 L80 (footer `tag_len += 10`: `+=`->`-=`,`*=`).
    // ID3v2.4 tag WITH the footer flag (0x10) and a known body. tag_len must be
    // 10 (header) + body + 10 (footer). With body=6, audio_offset must be 26.
    // `-=` gives 10+6-10 = 6; `*=` gives (10+6)*10 = 160 (> file_len -> Malformed).
    // Frame sync is placed at offset 26 so the correct path returns Complete.
    #[test]
    fn locate_audio_bounded_footer_flag_adds_ten() {
        let body = 6usize;
        let mut full = b"ID3\x04\x00".to_vec();
        full.push(0x10); // flags: footer present
        full.extend_from_slice(&syncsafe(body as u32));
        full.extend(std::iter::repeat_n(0u8, body)); // tag body
        full.extend(std::iter::repeat_n(0u8, 10)); // footer region
        let expected_offset = full.len() as u64; // 10 + 6 + 10 = 26
        full.extend_from_slice(&[0xFF, 0xFB]); // frame sync at offset 26
        full.extend_from_slice(b"audio");
        let file_len = full.len() as u64;
        match locate_audio_bounded(&full, file_len, None).unwrap() {
            Extent::Complete(b) => {
                assert_eq!(b.audio_offset, 26);
                assert_eq!(b.audio_offset, expected_offset);
                assert_eq!(b.audio_length, file_len - 26);
            }
            other @ Extent::NeedMore { .. } => {
                panic!("expected Complete at offset 26, got {other:?}")
            }
        }
    }

    // kills mp3 L82 (`tag_len as u64 > file_len`: `>`->`==`,`>=`).
    // Construct a tag where tag_len == file_len EXACTLY (no room for audio).
    // Correct (`>`): `tag_len > file_len` is false -> proceeds; then the L96
    // `audio_offset + 2 > file_len` check fires (audio_offset == file_len) ->
    // Err(NotMp3). Under `==`/`>=`: `tag_len == file_len` true -> early
    // Err(Malformed). Asserting NotMp3 (not Malformed) kills both.
    #[test]
    fn locate_audio_bounded_tag_len_equals_file_len_is_notmp3_not_malformed() {
        let body = 8usize;
        let mut full = b"ID3\x04\x00\x00".to_vec();
        full.extend_from_slice(&syncsafe(body as u32));
        full.extend(std::iter::repeat_n(0u8, body)); // file ends exactly at tag end
        let file_len = full.len() as u64; // == tag_len == audio_offset (18)
        match locate_audio_bounded(&full, file_len, None) {
            Err(FormatError::NotMp3) => {}
            other => panic!("expected Err(NotMp3) for tag_len==file_len, got {other:?}"),
        }
    }

    // kills mp3 L82 true branch (`>`): a tag declaring more than the file holds
    // must be Malformed. Pins that the `>` branch is reachable and returns
    // Malformed (so the `>`->`==`/`>=` mutants, which change WHICH side is taken,
    // are distinguished from the equals case above).
    #[test]
    fn locate_audio_bounded_tag_len_exceeds_file_len_is_malformed() {
        // Declare body=100 but provide a tiny file. tag_len = 110 > file_len.
        let mut full = b"ID3\x04\x00\x00".to_vec();
        full.extend_from_slice(&syncsafe(100));
        full.extend_from_slice(&[0xFF, 0xFB]); // some bytes, but file is short
        let file_len = full.len() as u64; // 12, << 110
        match locate_audio_bounded(&full, file_len, None) {
            Err(FormatError::Malformed) => {}
            other => panic!("expected Err(Malformed), got {other:?}"),
        }
    }

    // kills mp3 L86 (`prefix.len() < 10 && file_len >= 10`: the NeedMore{up_to:10}
    // else-if). Short non-ID3 prefix (len 5) with file_len >= 10. Correct: `5 < 10
    // && 10 >= 10` = true -> NeedMore{up_to:10} (we cannot even read the ID3 header).
    // `&&`->`||` keeps it true here; the distinguishing variants are below.
    #[test]
    fn locate_audio_bounded_short_prefix_large_file_needs_header() {
        let prefix = [0x00, 0x00, 0x00, 0x00, 0x00]; // 5 bytes, not "ID3"
        let file_len = 64u64; // >= 10
        match locate_audio_bounded(&prefix, file_len, None).unwrap() {
            Extent::NeedMore { up_to } => assert_eq!(up_to, 10),
            other @ Extent::Complete(_) => panic!("expected NeedMore{{up_to:10}}, got {other:?}"),
        }
    }

    // kills mp3 L86 `<`->`<=` (and `<`->`==`): boundary prefix.len()==10.
    // A 10-byte non-ID3 prefix with file_len >= 10. Correct (`<`): `10 < 10` is
    // false -> does NOT take the NeedMore-header branch -> proceeds. The first two
    // prefix bytes are a valid frame sync, so audio at offset 0 resolves Complete.
    // Under `<=`: `10 <= 10` true -> wrongly NeedMore{up_to:10}. Under `==`:
    // `10 == 10` true -> wrongly NeedMore. Asserting Complete kills both.
    #[test]
    fn locate_audio_bounded_prefix_len_exactly_ten_proceeds() {
        // 10 bytes, not "ID3", frame sync at offset 0.
        let prefix = [0xFF, 0xFB, 0x90, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00];
        let file_len = 64u64; // >= 10, audio extends to file_len
        match locate_audio_bounded(&prefix, file_len, None).unwrap() {
            Extent::Complete(b) => {
                assert_eq!(b.audio_offset, 0);
                assert_eq!(b.audio_length, file_len);
            }
            other @ Extent::NeedMore { .. } => {
                panic!("expected Complete (10<10 false), got {other:?}")
            }
        }
    }

    // kills mp3 L86 `>=`->`<` on file_len (and helps `&&`->`||`). Short non-ID3
    // prefix (len 5) with file_len < 10 (file_len=8). Correct (`>=`): `5 < 10 &&
    // 8 >= 10` = `true && false` = false -> does NOT NeedMore -> proceeds; sync at
    // offset 0 is in the prefix -> Complete with audio_length 8. Under `>=`->`<`:
    // `8 < 10` true -> `true && true` -> wrongly NeedMore{up_to:10}. Under `&&`->
    // `||`: `true || false` -> true -> wrongly NeedMore. Asserting Complete kills
    // both the `>=`->`<` and the `&&`->`||` mutants.
    #[test]
    fn locate_audio_bounded_short_prefix_small_file_proceeds() {
        let data = [0xFF, 0xFB, 0x90, 0x00, 0x00]; // len 5, file_len 8 -> but prefix==file here
                                                   // Make file_len 8 with the same 5-byte prefix window; the sync pair (2 bytes)
                                                   // is inside the prefix, so it resolves without needing more.
        let file_len = 8u64;
        match locate_audio_bounded(&data, file_len, None).unwrap() {
            Extent::Complete(b) => {
                assert_eq!(b.audio_offset, 0);
                assert_eq!(b.audio_length, 8);
            }
            other @ Extent::NeedMore { .. } => {
                panic!("expected Complete (file_len<10), got {other:?}")
            }
        }
    }

    // kills mp3 L96 (`audio_offset as u64 + 2 > file_len`: `+`->`-`).
    // Build a real ID3v2 tag so audio_offset > 0, with the audio start placed
    // JUST past EOF: audio_offset + 2 == file_len + 1 (i.e. audio_offset ==
    // file_len - 1). Correct (`+`): `audio_offset + 2 > file_len` -> true ->
    // Err(NotMp3). Under `-`: `audio_offset - 2 > file_len` -> false (since
    // audio_offset < file_len) -> proceeds -> would read past EOF / wrong answer.
    #[test]
    fn locate_audio_bounded_sync_one_byte_past_eof_is_notmp3() {
        let body = 4usize;
        let mut full = b"ID3\x04\x00\x00".to_vec();
        full.extend_from_slice(&syncsafe(body as u32));
        full.extend(std::iter::repeat_n(0u8, body)); // tag end at offset 14
        let audio_offset = full.len() as u64; // 14
        full.push(0xFF); // a single sync byte present (so prefix has audio_offset+1)
                         // file_len = audio_offset + 1, so audio_offset + 2 == file_len + 1 (just past).
        let file_len = audio_offset + 1; // 15
        match locate_audio_bounded(&full, file_len, None) {
            Err(FormatError::NotMp3) => {}
            other => panic!("expected Err(NotMp3) (sync past EOF), got {other:?}"),
        }
    }

    // Complement to L96: audio_offset + 2 <= file_len must proceed (not reject).
    // Pins that the `>` comparison's false branch is reachable; with `+`->`-` the
    // earlier case flips, so this guards the true semantics of "+2 fits".
    #[test]
    fn locate_audio_bounded_sync_fits_in_file_proceeds() {
        let (full, audio_offset) = mp3_with_id3v2(4, b"frames");
        let file_len = full.len() as u64; // audio_offset + 2 + 6
        match locate_audio_bounded(&full, file_len, None).unwrap() {
            Extent::Complete(b) => assert_eq!(b.audio_offset, audio_offset),
            other @ Extent::NeedMore { .. } => panic!("expected Complete, got {other:?}"),
        }
    }

    #[test]
    fn locate_audio_bounded_sync_exactly_at_eof_proceeds() {
        // Boundary: audio_offset + 2 == file_len exactly (audio is just the 2-byte
        // frame sync). `audio_offset + 2 > file_len` is false -> Complete. The
        // `>`->`>=` mutant makes `16 >= 16` true -> wrongly Err(NotMp3). Mirrors the
        // unbounded reject `audio_offset + 1 >= len` (accepts when +2 <= len).
        let body = 4usize;
        let mut full = b"ID3\x04\x00\x00".to_vec();
        full.extend_from_slice(&syncsafe(body as u32));
        full.extend(std::iter::repeat_n(0u8, body)); // tag end at offset 14
        let audio_offset = full.len() as u64; // 14
        full.push(0xFF); // frame sync pair, and nothing after
        full.push(0xFB);
        let file_len = full.len() as u64; // 16 == audio_offset + 2
                                          // kills mp3 L96 `>`->`>=`: equal-fit audio must be accepted, not rejected.
        match locate_audio_bounded(&full, file_len, None).unwrap() {
            Extent::Complete(b) => {
                assert_eq!(b.audio_offset, audio_offset);
                assert_eq!(b.audio_length, 2);
            }
            other @ Extent::NeedMore { .. } => {
                panic!("expected Complete (exact fit), got {other:?}")
            }
        }
    }

    // kills mp3 L107 (`prefix[audio_offset] != 0xFF || (prefix[audio_offset+1] &
    // 0xE0) != 0xE0`): `||`->`&&` and `+`->`*`.
    // Frame-sync byte 0 is 0xFF but byte 1 lacks the 0xE0 sync bits. Correct
    // (`||`): first operand false, second true -> reject NotMp3. Under `&&`:
    // `false && true` -> accept (wrong) -> would return Complete. The `+`->`*` on
    // `audio_offset + 1`: with audio_offset==0, `0*1 == 0` reads byte 0 (0xFF)
    // instead of byte 1, so `(0xFF & 0xE0) != 0xE0` is false -> with `||` short of
    // first-operand-false the decision changes; pairing distinct bytes makes the
    // sync verdict observable.
    #[test]
    fn locate_audio_bounded_rejects_bad_second_sync_byte() {
        // byte0 = 0xFF (passes first half), byte1 = 0x00 (fails the 0xE0 check).
        let data = [
            0xFF, 0x00, 0x90, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        ];
        let file_len = data.len() as u64;
        match locate_audio_bounded(&data, file_len, None) {
            Err(FormatError::NotMp3) => {}
            other => panic!("expected Err(NotMp3) (bad sync byte 1), got {other:?}"),
        }
    }

    // Reinforces L107 `+`->`*` at a NON-zero audio_offset so `audio_offset + 1`
    // and `audio_offset * 1` differ. With an ID3 tag pushing audio_offset to 14:
    // byte[14] = 0xFF (good), byte[15] = 0x00 (bad second byte). Correct reads
    // byte[15] -> reject NotMp3. Under `+`->`*`: `14 * 1 == 14` reads byte[14]
    // (0xFF) again -> `(0xFF & 0xE0)==0xE0` so the second test passes -> accept
    // (wrong). Asserting NotMp3 kills `+`->`*`.
    #[test]
    fn locate_audio_bounded_rejects_bad_second_sync_byte_after_id3() {
        let body = 4usize;
        let mut full = b"ID3\x04\x00\x00".to_vec();
        full.extend_from_slice(&syncsafe(body as u32));
        full.extend(std::iter::repeat_n(0u8, body)); // audio_offset = 14
        full.extend_from_slice(&[0xFF, 0x00]); // byte14=0xFF good, byte15=0x00 bad
        full.extend_from_slice(b"tail");
        let file_len = full.len() as u64;
        match locate_audio_bounded(&full, file_len, None) {
            Err(FormatError::NotMp3) => {}
            other => panic!("expected Err(NotMp3) (bad sync at 15), got {other:?}"),
        }
    }

    // kills mp3 L101 frame-sync NeedMore (`audio_offset + 2 > prefix.len()`).
    // A tag whose audio_offset is inside file_len, but the prefix is shorter than
    // audio_offset + 2 (the sync pair is past the prefix window). Correct: returns
    // NeedMore{up_to: audio_offset + 2}. A `+`->`*` (audio_offset*2) or a flipped
    // comparison changes up_to. Here audio_offset=14, so up_to must be 16; prefix
    // is only 15 bytes (one short of the sync pair).
    #[test]
    fn locate_audio_bounded_needmore_for_sync_past_prefix() {
        let body = 4usize;
        let mut full = b"ID3\x04\x00\x00".to_vec();
        full.extend_from_slice(&syncsafe(body as u32));
        full.extend(std::iter::repeat_n(0u8, body)); // audio_offset = 14
        full.extend_from_slice(&[0xFF, 0xFB]); // sync at 14..16
        full.extend_from_slice(b"more audio bytes here");
        let file_len = full.len() as u64; // plenty of room
        let prefix = &full[..15]; // 14-byte tag + only 1 of the 2 sync bytes
        match locate_audio_bounded(prefix, file_len, None).unwrap() {
            Extent::NeedMore { up_to } => assert_eq!(up_to, 16), // audio_offset(14) + 2
            other @ Extent::Complete(_) => panic!("expected NeedMore{{up_to:16}}, got {other:?}"),
        }
    }

    // kills mp3 L113 (`file_len >= audio_offset + 128 && &tail[0..3] == b"TAG"`:
    // `&&`->`||`) — the TRIM case. A valid MP3 with a "TAG"-prefixed tail and
    // file_len >= audio_offset + 128. Correct: trim -> audio_length = file_len -
    // audio_offset - 128. (The complement no-trim case is below; together they pin
    // the `&&`.)
    #[test]
    fn locate_audio_bounded_trims_id3v1_when_tag_and_room() {
        let (mut full, audio_offset) = mp3_with_id3v2(8, b"frames");
        let body_end = full.len();
        full.extend_from_slice(b"TAG");
        full.extend(std::iter::repeat_n(0u8, 125)); // 128-byte ID3v1 trailer
        let file_len = full.len() as u64;
        assert!(file_len >= audio_offset + 128); // both conditions true
        let tail: [u8; 128] = full[full.len() - 128..].try_into().unwrap();
        let prefix = &full[..audio_offset as usize + 2];
        match locate_audio_bounded(prefix, file_len, Some(&tail)).unwrap() {
            Extent::Complete(b) => {
                assert_eq!(b.audio_offset, audio_offset);
                // kills mp3 L113: trimmed length excludes the 128-byte ID3v1 tail.
                assert_eq!(b.audio_length, file_len - audio_offset - 128);
                assert_eq!(b.audio_length, body_end as u64 - audio_offset);
            }
            other @ Extent::NeedMore { .. } => panic!("expected Complete (trimmed), got {other:?}"),
        }
    }

    // kills mp3 L113 (`&&`->`||`) — the NO-TRIM case. file_len >= audio_offset+128
    // is TRUE, but the tail does NOT start with "TAG". Correct (`&&`): second
    // operand false -> no trim -> audio_length == file_len - audio_offset. Under
    // `||`: first operand true -> trims 128 wrongly -> shorter length. Asserting
    // the un-trimmed length kills the `||` mutant.
    #[test]
    fn locate_audio_bounded_no_trim_when_tail_not_tag() {
        let (mut full, audio_offset) = mp3_with_id3v2(8, b"frames");
        // Pad with enough non-"TAG" trailing bytes so file_len >= audio_offset+128.
        full.extend(std::iter::repeat_n(0u8, 200));
        let file_len = full.len() as u64;
        assert!(file_len >= audio_offset + 128); // first operand TRUE
        let tail: [u8; 128] = full[full.len() - 128..].try_into().unwrap();
        assert_ne!(&tail[0..3], b"TAG"); // second operand FALSE
        let prefix = &full[..audio_offset as usize + 2];
        match locate_audio_bounded(prefix, file_len, Some(&tail)).unwrap() {
            Extent::Complete(b) => {
                assert_eq!(b.audio_offset, audio_offset);
                // No trim: full audio length from offset to EOF.
                assert_eq!(b.audio_length, file_len - audio_offset);
            }
            other @ Extent::NeedMore { .. } => panic!("expected Complete (no trim), got {other:?}"),
        }
    }

    // Complement to L113 first-operand: tail starts with "TAG" but file_len <
    // audio_offset + 128 (no room for a real ID3v1). Correct (`&&`): first operand
    // false -> no trim. Under `||`: second operand true -> trims 128 even though
    // file_len < audio_offset + 128, which would underflow / shorten wrongly.
    // Asserting the un-trimmed length pins the first operand of the `&&`.
    #[test]
    fn locate_audio_bounded_no_trim_when_no_room_even_with_tag_tail() {
        let (mut full, audio_offset) = mp3_with_id3v2(8, b"frames");
        // Short file: append a "TAG"-prefixed tail but keep file_len < offset+128.
        full.extend_from_slice(b"TAGxx"); // tail-ish marker, but file stays short
        let file_len = full.len() as u64;
        assert!(file_len < audio_offset + 128); // first operand FALSE
                                                // Build a 128-byte tail buffer that starts with "TAG" (the function only
                                                // looks at tail[0..3]); file_len is the real gate here.
        let mut tail = [0u8; 128];
        tail[0..3].copy_from_slice(b"TAG");
        let prefix = &full[..audio_offset as usize + 2];
        match locate_audio_bounded(prefix, file_len, Some(&tail)).unwrap() {
            Extent::Complete(b) => {
                assert_eq!(b.audio_offset, audio_offset);
                assert_eq!(b.audio_length, file_len - audio_offset); // no trim
            }
            other @ Extent::NeedMore { .. } => {
                panic!("expected Complete (no room, no trim), got {other:?}")
            }
        }
    }

    /// Build a minimal ID3v2.4 tag containing the given frames, with header
    /// flags=0 (no unsync, no extended header) and per-frame flags=0 so
    /// `id3v2_alloc_safe` accepts it. Used by `read_binary_tags` tests that
    /// need a tag without going through the `id3` crate's encoder (which would
    /// re-encode `Unknown` bodies and defeat the byte-exact property).
    fn build_v24_tag(frames: &[(&[u8; 4], &[u8])]) -> Vec<u8> {
        let total_body: usize = frames.iter().map(|(_, b)| 10 + b.len()).sum();
        let mut out = Vec::new();
        out.extend_from_slice(b"ID3");
        out.extend_from_slice(&[0x04, 0x00, 0x00]); // v2.4.0, no flags
        out.extend_from_slice(&ss(total_body as u32));
        for (id, body) in frames {
            out.extend_from_slice(*id);
            out.extend_from_slice(&ss(body.len() as u32));
            out.extend_from_slice(&[0x00, 0x00]); // frame flags
            out.extend_from_slice(body);
        }
        out
    }

    #[test]
    fn read_binary_tags_promotes_popm_and_mbid_and_passes_through_priv() {
        use id3::frame::{Content, Popularimeter, UniqueFileIdentifier, Unknown};
        use id3::{Encoder, Frame, Tag, TagLike, Version};

        let mut tag = Tag::new();
        tag.add_frame(Popularimeter {
            user: "a@b.c".into(),
            rating: 200,
            counter: 7,
        });
        tag.add_frame(UniqueFileIdentifier {
            owner_identifier: "http://musicbrainz.org".into(),
            identifier: b"mbid-123".to_vec(),
        });
        tag.add_frame(UniqueFileIdentifier {
            owner_identifier: "http://other.example".into(),
            identifier: b"other".to_vec(),
        });
        tag.add_frame(Frame::with_content(
            "PRIV",
            Content::Unknown(Unknown {
                data: vec![9, 8, 7],
                version: Version::Id3v24,
            }),
        ));
        let mut buf = Vec::new();
        Encoder::new()
            .version(Version::Id3v24)
            .encode(&tag, &mut buf)
            .unwrap();

        let (opaque, promoted) = super::read_binary_tags(&buf);
        assert!(promoted.contains(&("rating".to_string(), "200".to_string())));
        assert!(promoted.contains(&("playcount".to_string(), "7".to_string())));
        assert!(promoted.contains(&("musicbrainz_trackid".to_string(), "mbid-123".to_string())));
        let keys: Vec<&str> = opaque.iter().map(|e| e.key.as_str()).collect();
        assert!(keys.contains(&"PRIV"));
        // Non-MusicBrainz UFID is opaque (raw body, owner + identifier); exactly one UFID.
        assert_eq!(keys.iter().filter(|k| **k == "UFID").count(), 1);
        assert_eq!(
            opaque.iter().find(|e| e.key == "PRIV").unwrap().payload,
            vec![9, 8, 7]
        );
    }

    #[test]
    fn read_binary_tags_preserves_geob_body_byte_exact() {
        // A GEOB body with a Latin1 (encoding 0x00) description — the exact case
        // the crate's to_unknown() would re-encode to UTF-8. Build a minimal v2.4
        // tag by hand so the bytes on the wire are guaranteed to match the
        // asserted body.
        let geob_body: Vec<u8> = {
            let mut b = vec![0x00]; // text encoding: ISO-8859-1
            b.extend_from_slice(b"application/octet-stream\0"); // mime
            b.extend_from_slice(b"Serato Overview\0"); // filename (latin1)
            b.extend_from_slice(b"\0"); // description (empty, terminator only)
            b.extend_from_slice(&[0xDE, 0xAD, 0xBE, 0xEF]); // object data
            b
        };
        let tag = build_v24_tag(&[(b"GEOB", &geob_body)]);

        let (opaque, _promoted) = super::read_binary_tags(&tag);
        let geob = opaque
            .iter()
            .find(|e| e.key == "GEOB")
            .expect("GEOB preserved");
        assert_eq!(
            geob.payload, geob_body,
            "GEOB body must survive byte-identical"
        );
    }

    #[test]
    fn build_id3v2_segments_rebuilds_popm_ufid_and_streams_opaque() {
        use crate::BinaryTagInput;
        let tags = vec![
            TagInput::new("artist", "A"),
            TagInput::new("rating", "200"),
            TagInput::new("playcount", "7"),
            TagInput::new("musicbrainz_trackid", "mbid-123"),
        ];
        let bin = vec![BinaryTagInput {
            key: "PRIV".into(),
            payload_id: 42,
            len: 3,
        }];
        let (segments, _len) = super::build_id3v2_segments(&tags, &bin, &[]).unwrap();

        assert!(
            segments.iter().any(|s| matches!(
                s,
                Segment::BinaryTag {
                    payload_id: 42,
                    len: 3
                }
            )),
            "opaque PRIV must stream as Segment::BinaryTag"
        );

        let inline: Vec<u8> = segments
            .iter()
            .flat_map(|s| match s {
                Segment::Inline(b) => b.clone(),
                _ => Vec::new(),
            })
            .collect();
        assert!(find_sub(&inline, b"POPM"), "POPM not rebuilt");
        assert!(find_sub(&inline, b"UFID"), "UFID not rebuilt");
        assert!(
            find_sub(&inline, b"http://musicbrainz.org"),
            "UFID owner missing"
        );
        assert!(!find_sub(&inline, b"rating"), "promoted key leaked as TXXX");
        assert!(
            !find_sub(&inline, b"musicbrainz_trackid"),
            "promoted key leaked as TXXX"
        );
    }

    fn find_sub(hay: &[u8], needle: &[u8]) -> bool {
        hay.windows(needle.len()).any(|w| w == needle)
    }
}
