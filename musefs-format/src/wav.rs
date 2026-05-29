use crate::error::{FormatError, Result};
use crate::input::EmbeddedPicture;
use std::collections::HashSet;

/// The served audio bounds of a WAV: the `data` chunk's payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WavBounds {
    pub audio_offset: u64,
    pub audio_length: u64,
}

/// The structural chunks preserved for synthesis: the required `fmt ` payload and
/// the optional `fact` payload (present for non-PCM codecs).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WavScan {
    pub fmt: Vec<u8>,
    pub fact: Option<Vec<u8>>,
}

/// Validate the RIFF/WAVE container header and return the offset of the first
/// chunk (always 12). Rejects RF64/BW64 (their form id is not `RIFF`) and any
/// non-`WAVE` RIFF file.
fn riff_wave_start(buf: &[u8]) -> Result<usize> {
    if buf.len() < 12 || &buf[0..4] != b"RIFF" || &buf[8..12] != b"WAVE" {
        return Err(FormatError::NotWav);
    }
    Ok(12)
}

/// Walk the top-level WAVE chunks, returning `(fourcc, payload_offset, payload_len)`
/// for each chunk whose 8-byte header is present. Advances header-to-header with
/// RIFF word-alignment padding, skipping payloads. Stops (after recording it) when
/// a chunk's declared payload runs past the buffer — e.g. the `data` chunk in a
/// front-only buffer.
fn walk_chunks(buf: &[u8]) -> Vec<([u8; 4], usize, u64)> {
    let mut out = Vec::new();
    let Ok(mut pos) = riff_wave_start(buf) else {
        return out;
    };
    while pos + 8 <= buf.len() {
        let mut id = [0u8; 4];
        id.copy_from_slice(&buf[pos..pos + 4]);
        let size =
            u32::from_le_bytes([buf[pos + 4], buf[pos + 5], buf[pos + 6], buf[pos + 7]]) as u64;
        let payload_offset = pos + 8;
        out.push((id, payload_offset, size));
        let advance = 8u64 + size + (size & 1); // word-align: pad odd payloads
        match (pos as u64).checked_add(advance) {
            Some(next) if next <= buf.len() as u64 => pos = next as usize,
            _ => break,
        }
    }
    out
}

/// Borrow a chunk's payload bytes if they fit fully in `buf`.
fn chunk_slice(buf: &[u8], offset: usize, len: u64) -> Option<&[u8]> {
    let end = offset.checked_add(len as usize)?;
    buf.get(offset..end)
}

/// Parse the file and return the `data` chunk payload bounds, or an error to skip
/// it. Requires both `fmt ` and `data`, and the `data` payload must fit in `buf`.
pub fn locate_audio(buf: &[u8]) -> Result<WavBounds> {
    riff_wave_start(buf)?;
    let chunks = walk_chunks(buf);
    let has_fmt = chunks.iter().any(|(id, _, _)| id == b"fmt ");
    let data = chunks.iter().find(|(id, _, _)| id == b"data");
    match (has_fmt, data) {
        (true, Some(&(_, off, len))) => {
            if (off as u64).saturating_add(len) > buf.len() as u64 {
                return Err(FormatError::Malformed);
            }
            Ok(WavBounds {
                audio_offset: off as u64,
                audio_length: len,
            })
        }
        _ => Err(FormatError::NotWav),
    }
}

/// Read the preserved structural chunks (`fmt `, optional `fact`) from the front
/// of the file (everything before the `data` payload). Errors if `fmt ` is absent
/// or a preserved chunk's payload is truncated.
pub fn read_structure(front: &[u8]) -> Result<WavScan> {
    riff_wave_start(front)?;
    let chunks = walk_chunks(front);

    let &(_, fmt_off, fmt_len) = chunks
        .iter()
        .find(|(id, _, _)| id == b"fmt ")
        .ok_or(FormatError::NotWav)?;
    let fmt = chunk_slice(front, fmt_off, fmt_len)
        .ok_or(FormatError::Malformed)?
        .to_vec();

    let fact = match chunks.iter().find(|(id, _, _)| id == b"fact") {
        Some(&(_, off, len)) => Some(
            chunk_slice(front, off, len)
                .ok_or(FormatError::Malformed)?
                .to_vec(),
        ),
        None => None,
    };

    Ok(WavScan { fmt, fact })
}

use crate::input::{ArtInput, TagInput};
use crate::layout::{RegionLayout, Segment};

/// Canonical (lowercase) tag key -> RIFF `INFO` subchunk FourCC. INFO is the
/// broad-compatibility surface with a small vocabulary; richer fields
/// (albumartist, disc, MusicBrainz ids) ride only in the `id3 ` chunk.
fn info_fourcc(key: &str) -> Option<&'static [u8; 4]> {
    Some(match key {
        "title" => b"INAM",
        "artist" => b"IART",
        "album" => b"IPRD",
        "date" => b"ICRD",
        "genre" => b"IGNR",
        "comment" => b"ICMT",
        "tracknumber" => b"ITRK",
        _ => return None,
    })
}

/// Build the `LIST`/`INFO` chunk payload (`"INFO"` + subchunks) from the first
/// value of each mappable tag key, in first-seen order. Returns `None` when no
/// tag maps to an INFO field (so the chunk is omitted entirely).
fn build_info_payload(tags: &[TagInput]) -> Option<Vec<u8>> {
    let mut entries: Vec<(&'static [u8; 4], &str)> = Vec::new();
    let mut used: Vec<&str> = Vec::new();
    for t in tags {
        if used.contains(&t.key.as_str()) {
            continue;
        }
        if let Some(cc) = info_fourcc(&t.key) {
            used.push(t.key.as_str());
            entries.push((cc, t.value.as_str()));
        }
    }
    if entries.is_empty() {
        return None;
    }
    let mut payload = Vec::new();
    payload.extend_from_slice(b"INFO");
    for (cc, value) in entries {
        let mut v = value.as_bytes().to_vec();
        v.push(0x00); // INFO values are NUL-terminated
        payload.extend_from_slice(cc);
        payload.extend_from_slice(&(v.len() as u32).to_le_bytes());
        payload.extend_from_slice(&v);
        if v.len() % 2 == 1 {
            payload.push(0x00); // word-align
        }
    }
    Some(payload)
}

/// Push a fully-inline chunk (`fourcc + LE size + payload + word-align pad`).
fn push_inline_chunk(segments: &mut Vec<Segment>, id: &[u8; 4], payload: &[u8]) {
    let mut chunk = Vec::with_capacity(8 + payload.len() + 1);
    chunk.extend_from_slice(id);
    chunk.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    chunk.extend_from_slice(payload);
    if payload.len() % 2 == 1 {
        chunk.push(0x00);
    }
    segments.push(Segment::Inline(chunk));
}

/// Build the synthesized WAV region: a fresh `RIFF`/`WAVE` front carrying the
/// preserved `fmt `/`fact`, a native `LIST`/`INFO` chunk, and an embedded `id3 `
/// chunk (full ID3v2 + APIC art), followed by the untouched `data` payload as a
/// `BackingAudio` segment. Every length is known up front, so the `RIFF` and
/// chunk size fields are byte-exact.
pub fn synthesize_layout(
    scan: &WavScan,
    audio_offset: u64,
    audio_length: u64,
    tags: &[TagInput],
    arts: &[ArtInput],
) -> Result<RegionLayout> {
    if audio_length > u32::MAX as u64 {
        return Err(FormatError::TooLarge); // RF64 territory; out of scope
    }

    let mut segments: Vec<Segment> = Vec::new();

    push_inline_chunk(&mut segments, b"fmt ", &scan.fmt);
    if let Some(fact) = &scan.fact {
        push_inline_chunk(&mut segments, b"fact", fact);
    }
    if let Some(info) = build_info_payload(tags) {
        push_inline_chunk(&mut segments, b"LIST", &info);
    }

    // Embedded `id3 ` chunk: 8-byte chunk header + the ID3v2 tag segments, padded.
    let (tag_segments, tag_len) = crate::mp3::build_id3v2_segments(tags, arts)?;
    let mut id3_head = Vec::with_capacity(8);
    id3_head.extend_from_slice(b"id3 ");
    id3_head.extend_from_slice(&(tag_len as u32).to_le_bytes());
    segments.push(Segment::Inline(id3_head));
    segments.extend(tag_segments);
    if tag_len % 2 == 1 {
        segments.push(Segment::Inline(vec![0x00]));
    }

    // `data` chunk: header + the original payload (BackingAudio) + word-align pad.
    let mut data_head = Vec::with_capacity(8);
    data_head.extend_from_slice(b"data");
    data_head.extend_from_slice(&(audio_length as u32).to_le_bytes());
    segments.push(Segment::Inline(data_head));
    segments.push(Segment::BackingAudio {
        offset: audio_offset,
        len: audio_length,
    });
    if audio_length % 2 == 1 {
        segments.push(Segment::Inline(vec![0x00]));
    }

    // RIFF size = (everything after the 8-byte "RIFF"+size prefix) = body + "WAVE".
    let body_len: u64 = segments.iter().map(Segment::len).sum();
    let riff_size = body_len + 4;
    if riff_size > u32::MAX as u64 {
        return Err(FormatError::TooLarge);
    }
    let mut header = Vec::with_capacity(12);
    header.extend_from_slice(b"RIFF");
    header.extend_from_slice(&(riff_size as u32).to_le_bytes());
    header.extend_from_slice(b"WAVE");
    segments.insert(0, Segment::Inline(header));

    RegionLayout::validated(segments).map_err(|_| FormatError::InvalidLayout)
}

/// RIFF `INFO` subchunk FourCC -> canonical (lowercase) tag key. Inverse of
/// `info_fourcc`.
fn info_to_key(id: &[u8; 4]) -> Option<&'static str> {
    Some(match id {
        b"INAM" => "title",
        b"IART" => "artist",
        b"IPRD" => "album",
        b"ICRD" => "date",
        b"IGNR" => "genre",
        b"ICMT" => "comment",
        b"ITRK" => "tracknumber",
        _ => return None,
    })
}

/// Find the embedded ID3v2 tag chunk payload, accepting `id3 ` or `ID3 ` casing.
fn find_id3_chunk<'a>(buf: &'a [u8], chunks: &[([u8; 4], usize, u64)]) -> Option<&'a [u8]> {
    let &(_, off, len) = chunks
        .iter()
        .find(|(id, _, _)| id == b"id3 " || id == b"ID3 ")?;
    chunk_slice(buf, off, len)
}

/// Parse `LIST`/`INFO` subchunks into canonical `(key, value)` pairs. `body` is the
/// INFO payload after the leading `"INFO"` FourCC.
fn read_info_tags(body: &[u8]) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut pos = 0usize;
    while pos + 8 <= body.len() {
        let mut id = [0u8; 4];
        id.copy_from_slice(&body[pos..pos + 4]);
        let size = u32::from_le_bytes([body[pos + 4], body[pos + 5], body[pos + 6], body[pos + 7]])
            as usize;
        let val_start = pos + 8;
        let val_end = val_start.saturating_add(size).min(body.len());
        if let Some(key) = info_to_key(&id) {
            let raw = String::from_utf8_lossy(&body[val_start..val_end]);
            let value = raw.trim_end_matches('\0').to_string();
            if !value.is_empty() {
                out.push((key.to_string(), value));
            }
        }
        pos = val_start + size + (size & 1);
    }
    out
}

/// Read WAV tags for scan-time seeding: an embedded `id3 ` chunk (full ID3v2) and a
/// `LIST`/`INFO` chunk, merged per field with id3 taking precedence and INFO filling
/// gaps. Walks chunk headers without reading the `data` payload.
pub fn read_tags(buf: &[u8]) -> Vec<(String, String)> {
    let chunks = walk_chunks(buf);

    let from_id3 = find_id3_chunk(buf, &chunks)
        .map(crate::mp3::read_tags)
        .unwrap_or_default();

    let from_info = chunks
        .iter()
        .find(|(id, _, _)| id == b"LIST")
        .and_then(|&(_, off, len)| chunk_slice(buf, off, len))
        .filter(|slice| slice.len() >= 4 && &slice[0..4] == b"INFO")
        .map(|slice| read_info_tags(&slice[4..]))
        .unwrap_or_default();

    let id3_keys: HashSet<&str> = from_id3.iter().map(|(k, _)| k.as_str()).collect();
    let mut out = from_id3.clone();
    for (k, v) in from_info {
        if !id3_keys.contains(k.as_str()) {
            out.push((k, v));
        }
    }
    out
}

/// Read embedded pictures for scan-time art ingestion. Pictures live only in the
/// embedded `id3 ` chunk (INFO has no picture mechanism).
pub fn read_pictures(buf: &[u8]) -> Vec<EmbeddedPicture> {
    let chunks = walk_chunks(buf);
    find_id3_chunk(buf, &chunks)
        .map(crate::mp3::read_pictures)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Regression for the fuzz-discovered WAV OOM vector
    /// (fuzz/artifacts/wav/oom-4a21767820d5f05328f01d975fb6d3314f3fb902):
    /// a crafted WAV whose "ID3 " chunk body starts with "RIFF" (not "ID3"),
    /// but contains a nested ID3v2.3 tag deeper inside with a "TDA " frame
    /// declaring size 0xF6000000 (~4.1 GiB).  id3::Tag::read_from2 scans
    /// forward and would OOM on the huge frame.  The alloc guard in
    /// mp3::id3v2_alloc_safe must reject the chunk body (which does not start
    /// with "ID3") before id3 is called.
    #[test]
    fn wav_oom_crash_artifact_is_safe() {
        // Full 181-byte crash artifact.
        const CRASH: &[u8] = &[
            0x52, 0x49, 0x46, 0x46, 0x32, 0x00, 0x00, 0x00, 0x57, 0x41, 0x56, 0x45, 0x49, 0x44,
            0x33, 0x20, 0x38, 0x00, 0x00, 0x00, 0x52, 0x49, 0x46, 0x46, 0x32, 0x00, 0x00, 0x00,
            0x57, 0x41, 0x56, 0x45, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x4c, 0x00, 0x00, 0x00, 0x00, 0x00, 0x49, 0x44, 0x33, 0x20, 0x15, 0x00, 0x00, 0x00,
            0x49, 0x44, 0x33, 0x03, 0x00, 0x00, 0x00, 0xf7, 0x00, 0x00, 0x54, 0x44, 0x41, 0x03,
            0xf6, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x01, 0x00, 0xa8, 0xa8, 0xa8, 0xa8, 0xa8, 0xa8, 0xa8, 0xa8, 0xa8,
            0xa8, 0xa8, 0xa8, 0xa8, 0xa8, 0xa8, 0xa8, 0xa8, 0xa8, 0xa8, 0xa8, 0xa8, 0xa8, 0xa8,
            0xa8, 0xa8, 0xa8, 0xa8, 0xa8, 0xa8, 0xa8, 0xa8, 0xa8, 0xa8, 0xa8, 0xa8, 0xa8, 0xa8,
            0xa8, 0xa8, 0xa8, 0xa8, 0xa8, 0xa8, 0xa8, 0xa8, 0xa8, 0xa8, 0xa8, 0xa8, 0xa8, 0xa8,
            0xa8, 0xa8, 0xa8, 0xa8, 0xa8, 0xa8, 0xa8, 0xa8, 0xa8, 0xa8, 0xa8, 0xa8, 0xa8, 0xa8,
            0xa8, 0xa8, 0xa8, 0xa8, 0xa8, 0xa8, 0xa8, 0xa8, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        ];
        assert!(
            read_tags(CRASH).is_empty(),
            "read_tags must not OOM on WAV crash artifact"
        );
        assert!(
            read_pictures(CRASH).is_empty(),
            "read_pictures must not OOM on WAV crash artifact"
        );
    }

    #[test]
    fn riff_wave_start_accepts_exactly_twelve_bytes() {
        // :24 `< → <=`: a valid 12-byte RIFF/WAVE buffer must be accepted.
        // The `<=` mutant computes `12 <= 12` (true) and wrongly rejects it.
        let buf = b"RIFF\0\0\0\0WAVE".to_vec();
        assert_eq!(buf.len(), 12);
        assert_eq!(riff_wave_start(&buf), Ok(12));
    }

    #[test]
    fn riff_wave_start_rejects_eleven_byte_riff_without_panic() {
        // :24 `< → ==`: an 11-byte buffer that starts with "RIFF". The original
        // short-circuits on `len < 12` → NotWav. The `==` mutant computes
        // `11 == 12` (false), falls through, and indexes `buf[8..12]` on an 11-byte
        // slice → panic. Asserting the clean Err kills it (panic ≠ Err).
        let buf = b"RIFF\0\0\0\0WAV".to_vec();
        assert_eq!(buf.len(), 11);
        assert_eq!(riff_wave_start(&buf), Err(FormatError::NotWav));
    }

    /// Build a minimal `RIFF/WAVE` buffer from `(fourcc, payload)` chunks in order,
    /// padding odd payloads to a word boundary (the on-disk RIFF layout).
    fn wav(chunks: &[(&[u8; 4], Vec<u8>)]) -> Vec<u8> {
        let mut body = Vec::new();
        for (id, payload) in chunks {
            body.extend_from_slice(*id);
            body.extend_from_slice(&(payload.len() as u32).to_le_bytes());
            body.extend_from_slice(payload);
            if payload.len() % 2 == 1 {
                body.push(0x00);
            }
        }
        let mut out = b"RIFF".to_vec();
        out.extend_from_slice(&((body.len() + 4) as u32).to_le_bytes());
        out.extend_from_slice(b"WAVE");
        out.extend_from_slice(&body);
        out
    }

    #[test]
    fn walk_chunks_advances_past_each_payload() {
        // :47 the `8 + size (+ size&1)` advance. An odd first payload forces the
        // word-align term to matter; a wrong advance (either `+ → -`) lands off the
        // next header, so the second chunk is lost or misread.
        let buf = wav(&[(b"AAAA", vec![0x11; 3]), (b"data", vec![0xBB; 8])]);
        let ids: Vec<[u8; 4]> = walk_chunks(&buf).iter().map(|(id, _, _)| *id).collect();
        assert_eq!(ids, vec![*b"AAAA", *b"data"]);
    }

    /// A 16-byte PCM `fmt ` payload: mono, 44.1 kHz, 16-bit.
    fn fmt_pcm() -> Vec<u8> {
        let mut f = Vec::new();
        f.extend_from_slice(&1u16.to_le_bytes());
        f.extend_from_slice(&1u16.to_le_bytes());
        f.extend_from_slice(&44_100u32.to_le_bytes());
        f.extend_from_slice(&88_200u32.to_le_bytes());
        f.extend_from_slice(&2u16.to_le_bytes());
        f.extend_from_slice(&16u16.to_le_bytes());
        f
    }

    #[test]
    fn locate_requires_fmt_chunk() {
        // :67 `== → !=`: a data-only WAV (no `fmt `). Original: `any(id == "fmt ")`
        // is false → NotWav. The `!=` mutant is true (the data chunk is != "fmt ")
        // → has_fmt, so it returns Ok/Malformed instead of NotWav.
        let buf = wav(&[(b"data", vec![0x11; 8])]);
        assert_eq!(locate_audio(&buf), Err(FormatError::NotWav));
    }

    #[test]
    fn locate_accepts_data_with_trailing_chunk() {
        // :71 `> → <`: a valid WAV with a chunk AFTER `data`, so off+len < buf.len.
        // Original `off+len > buf.len` is false → Ok. The `<` mutant is true →
        // Malformed.
        let buf = wav(&[
            (b"fmt ", fmt_pcm()),
            (b"data", vec![0x11; 8]),
            (b"junk", vec![0x00; 4]),
        ]);
        let bounds = locate_audio(&buf).unwrap();
        assert_eq!(bounds.audio_length, 8);
    }
}
