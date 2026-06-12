use crate::error::{FormatError, Result};
use crate::input::{BinaryTagInput, EmbeddedBinaryTag, EmbeddedPicture};
use crate::probe::Extent;
use crate::size;
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

/// Validate the RIFF/WAVE container header and return `(first_chunk_offset,
/// form_end)`, where `form_end = 8 + riff_size` is the byte just past the
/// declared RIFF form. Rejects RF64/BW64 and any non-`WAVE` RIFF file.
fn riff_wave_start(buf: &[u8]) -> Result<(usize, u64)> {
    if buf.len() < 12 || &buf[0..4] != b"RIFF" || &buf[8..12] != b"WAVE" {
        return Err(FormatError::NotWav);
    }
    let riff_size = u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]);
    Ok((12, 8 + u64::from(riff_size)))
}

/// Walk the top-level WAVE chunks, returning `(fourcc, payload_offset, payload_len)`
/// for each chunk whose 8-byte header is present. Advances header-to-header with
/// RIFF word-alignment padding, skipping payloads. Stops (after recording it) when
/// a chunk's declared payload runs past the buffer — e.g. the `data` chunk in a
/// front-only buffer.
fn walk_chunks(buf: &[u8]) -> Vec<([u8; 4], usize, u64)> {
    let mut out = Vec::new();
    let Ok((mut pos, _form_end)) = riff_wave_start(buf) else {
        return out;
    };
    while pos + 8 <= buf.len() {
        let mut id = [0u8; 4];
        id.copy_from_slice(&buf[pos..pos + 4]);
        let size = u64::from(u32::from_le_bytes([
            buf[pos + 4],
            buf[pos + 5],
            buf[pos + 6],
            buf[pos + 7],
        ]));
        let payload_offset = pos + 8;
        out.push((id, payload_offset, size));
        let advance = 8u64 + size + (size & 1); // word-align: pad odd payloads
        match (pos as u64).checked_add(advance) {
            Some(next) if next <= buf.len() as u64 => pos = crate::convert::usize_from(next),
            _ => break,
        }
    }
    out
}

/// Borrow a chunk's payload bytes if they fit fully in `buf`.
fn chunk_slice(buf: &[u8], offset: usize, len: u64) -> Option<&[u8]> {
    let end = offset.checked_add(crate::convert::usize_from(len))?;
    buf.get(offset..end)
}

/// Parse the file and return the `data` chunk payload bounds, or an error to skip
/// it. Requires both `fmt ` and `data`, and the `data` payload must fit in `buf`.
pub fn locate_audio(buf: &[u8]) -> Result<WavBounds> {
    let (_, form_end) = riff_wave_start(buf)?;
    if form_end > buf.len() as u64 {
        return Err(FormatError::Malformed);
    }
    let chunks = walk_chunks(buf);
    let fmt = chunks.iter().find(|(id, _, _)| id == b"fmt ");
    let data = chunks.iter().find(|(id, _, _)| id == b"data");
    let (Some(&(_, fmt_off, fmt_len)), Some(&(_, off, len))) = (fmt, data) else {
        return Err(FormatError::NotWav);
    };
    // `fmt ` and `data` must both fall within the declared RIFF form; a chunk
    // past `form_end` means the form does not describe the audio. (Trailing
    // metadata chunks beyond `form_end` are still walked for best-effort tags.)
    let fmt_end = (fmt_off as u64).saturating_add(fmt_len);
    let data_end = (off as u64).saturating_add(len);
    if data_end > buf.len() as u64 || data_end > form_end || fmt_end > form_end {
        return Err(FormatError::Malformed);
    }
    Ok(WavBounds {
        audio_offset: off as u64,
        audio_length: len,
    })
}

/// Bounded twin of [`locate_audio`]. WAV metadata chunks can trail the `data`
/// payload, which the slice walk cannot skip past, so completion requires the
/// whole file in `prefix`; otherwise request it. Equivalence is trivially
/// preserved (the completing parse is exactly `locate_audio` on the full file).
pub fn locate_audio_bounded(prefix: &[u8], file_len: u64) -> Result<Extent<WavBounds>> {
    if (prefix.len() as u64) < file_len {
        return Ok(Extent::NeedMore { up_to: file_len });
    }
    Ok(Extent::Complete(locate_audio(prefix)?))
}

/// Best-effort WAV bounds when the file exceeds the probe budget: the `fmt `/`data`
/// chunk headers sit at the front and are present in `prefix`, but the `data`
/// payload — and hence any metadata chunks trailing it — lie beyond what we are
/// willing to read. Trusts the declared `data` length, validated against the real
/// `file_len` so a corrupt header claiming a payload larger than the file is still
/// rejected. Unlike [`locate_audio`] the payload need not be present in `prefix`;
/// any tags trailing it are necessarily lost.
pub fn locate_audio_at_ceiling(prefix: &[u8], file_len: u64) -> Result<WavBounds> {
    let (_, form_end) = riff_wave_start(prefix)?;
    if form_end > file_len {
        return Err(FormatError::Malformed);
    }
    let chunks = walk_chunks(prefix);
    let fmt = chunks.iter().find(|(id, _, _)| id == b"fmt ");
    let data = chunks.iter().find(|(id, _, _)| id == b"data");
    let (Some(&(_, fmt_off, fmt_len)), Some(&(_, off, len))) = (fmt, data) else {
        return Err(FormatError::NotWav);
    };
    // `fmt ` and `data` must both fall within the declared RIFF form (mirrors
    // [`locate_audio`]); the payload itself need not be present in `prefix`.
    let fmt_end = (fmt_off as u64).saturating_add(fmt_len);
    let data_end = (off as u64).saturating_add(len);
    if data_end > file_len || data_end > form_end || fmt_end > form_end {
        return Err(FormatError::Malformed);
    }
    Ok(WavBounds {
        audio_offset: off as u64,
        audio_length: len,
    })
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
fn build_info_payload(tags: &[TagInput]) -> Result<Option<Vec<u8>>> {
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
        return Ok(None);
    }
    let mut payload = Vec::new();
    payload.extend_from_slice(b"INFO");
    for (cc, value) in entries {
        let mut v = value.as_bytes().to_vec();
        v.push(0x00); // INFO values are NUL-terminated
        append_chunk(&mut payload, cc, &v)?;
    }
    Ok(Some(payload))
}

/// 8-byte RIFF chunk header: fourcc + LE u32 size.
fn chunk_header(id: &[u8; 4], len: u32) -> [u8; 8] {
    let mut h = [0u8; 8];
    h[..4].copy_from_slice(id);
    h[4..].copy_from_slice(&len.to_le_bytes());
    h
}

/// Append a chunk (`fourcc + LE size + payload + word-align pad`) to `out`.
fn append_chunk(out: &mut Vec<u8>, id: &[u8; 4], payload: &[u8]) -> Result<()> {
    let len = u32::try_from(payload.len()).map_err(|_| FormatError::TooLarge)?;
    out.extend_from_slice(&chunk_header(id, len));
    out.extend_from_slice(payload);
    if payload.len() % 2 == 1 {
        out.push(0x00);
    }
    Ok(())
}

/// Push a fully-inline chunk (`fourcc + LE size + payload + word-align pad`).
fn push_inline_chunk(segments: &mut Vec<Segment>, id: &[u8; 4], payload: &[u8]) -> Result<()> {
    let mut chunk = Vec::with_capacity(8 + payload.len() + 1);
    append_chunk(&mut chunk, id, payload)?;
    segments.push(Segment::Inline(chunk));
    Ok(())
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
    binary_tags: &[BinaryTagInput],
    arts: &[ArtInput],
) -> Result<RegionLayout> {
    let audio_length_u32 = u32::try_from(audio_length).map_err(|_| FormatError::TooLarge)?; // RF64 territory; out of scope

    let mut segments: Vec<Segment> = Vec::new();

    push_inline_chunk(&mut segments, b"fmt ", &scan.fmt)?;
    if let Some(fact) = &scan.fact {
        push_inline_chunk(&mut segments, b"fact", fact)?;
    }
    if let Some(info) = build_info_payload(tags)? {
        push_inline_chunk(&mut segments, b"LIST", &info)?;
    }

    // Embedded `id3 ` chunk: 8-byte chunk header + the ID3v2 tag segments, padded.
    // Zero-length art inputs are impossible (BlobLen is non-zero by construction),
    // so WAV inherits that invariant by delegating to `build_id3v2_segments`.
    let (tag_segments, tag_len) = crate::mp3::build_id3v2_segments(tags, binary_tags, arts)?;
    let tag_len_u32 = u32::try_from(tag_len).map_err(|_| FormatError::TooLarge)?;
    segments.push(Segment::Inline(chunk_header(b"id3 ", tag_len_u32).to_vec()));
    segments.extend(tag_segments);
    if tag_len % 2 == 1 {
        segments.push(Segment::Inline(vec![0x00]));
    }

    // `data` chunk: header + the original payload (BackingAudio) + word-align pad.
    segments.push(Segment::Inline(
        chunk_header(b"data", audio_length_u32).to_vec(),
    ));
    segments.push(Segment::BackingAudio {
        offset: audio_offset,
        len: audio_length,
    });
    if audio_length % 2 == 1 {
        segments.push(Segment::Inline(vec![0x00]));
    }

    // RIFF size = (everything after the 8-byte "RIFF"+size prefix) = body + "WAVE".
    let body_len: u64 = size::checked_sum(segments.iter().map(Segment::len))?;
    let riff_size =
        u32::try_from(size::checked_add(body_len, 4)?).map_err(|_| FormatError::TooLarge)?;
    let mut header = Vec::with_capacity(12);
    header.extend_from_slice(b"RIFF");
    header.extend_from_slice(&riff_size.to_le_bytes());
    header.extend_from_slice(b"WAVE");
    segments.insert(0, Segment::Inline(header));

    Ok(RegionLayout::validated(segments)?)
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

/// Extract binary ID3 frames from a WAV's embedded `id3 ` chunk. Classification is
/// identical to MP3 (`mp3::read_binary_tags`); only the chunk extraction differs.
/// Returns `(opaque, promoted)`; empty when there is no `id3 ` chunk.
pub fn read_binary_tags(data: &[u8]) -> (Vec<EmbeddedBinaryTag>, Vec<(String, String)>) {
    let chunks = walk_chunks(data);
    match find_id3_chunk(data, &chunks) {
        Some(id3_bytes) => crate::mp3::read_binary_tags(id3_bytes),
        None => (Vec::new(), Vec::new()),
    }
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
        assert_eq!(riff_wave_start(&buf), Ok((12, 8)));
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
            body.extend_from_slice(&u32::try_from(payload.len()).unwrap().to_le_bytes());
            body.extend_from_slice(payload);
            if payload.len() % 2 == 1 {
                body.push(0x00);
            }
        }
        let mut out = b"RIFF".to_vec();
        out.extend_from_slice(&u32::try_from(body.len() + 4).unwrap().to_le_bytes());
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

    #[test]
    fn info_fourcc_emits_each_mapped_key() {
        // :119-124 arm deletions: each key must map to its INFO FourCC. A deleted
        // arm makes the key unmapped → no payload (single-tag input → None).
        let cases: [(&str, &[u8; 4]); 6] = [
            ("artist", b"IART"),
            ("album", b"IPRD"),
            ("date", b"ICRD"),
            ("genre", b"IGNR"),
            ("comment", b"ICMT"),
            ("tracknumber", b"ITRK"),
        ];
        for (key, cc) in cases {
            let payload = build_info_payload(&[TagInput::new(key, "X")])
                .unwrap()
                .unwrap_or_else(|| panic!("INFO payload for {key}"));
            assert!(
                payload.windows(4).any(|w| w == &cc[..]),
                "key {key} must emit FourCC {:?}",
                std::str::from_utf8(cc).unwrap()
            );
        }
    }

    #[test]
    fn build_info_payload_word_aligns_values() {
        // :155 `v.len() % 2 == 1`. v = value bytes + NUL.
        // Value "a"  -> v.len()=2 (even, NO pad). Kills `% → /` (2/2==1 pads) and
        //               `== → !=` (2%2=0 != 1 pads).
        // Value "ab" -> v.len()=3 (odd, padded). Kills `% → +` (3+2 != 1, no pad).
        let even = build_info_payload(&[TagInput::new("title", "a")])
            .unwrap()
            .unwrap();
        // "INFO"(4) + "INAM"(4) + len(4) + "a\0"(2) = 14, no pad.
        assert_eq!(even.len(), 14);

        let odd = build_info_payload(&[TagInput::new("title", "ab")])
            .unwrap()
            .unwrap();
        // "INFO"(4) + "INAM"(4) + len(4) + "ab\0"(3) + pad(1) = 16.
        assert_eq!(odd.len(), 16);
    }

    #[test]
    fn push_inline_chunk_word_aligns_payload() {
        // :168 `payload.len() % 2 == 1`.
        // Even payload (len 2): NO pad. Kills `% → /` (2/2==1 pads).
        let mut segs = Vec::new();
        push_inline_chunk(&mut segs, b"test", &[0xAA, 0xBB]).unwrap();
        assert_eq!(segs.len(), 1);
        assert_eq!(segs[0].len(), 10); // "test"(4) + len(4) + payload(2)

        // Odd payload (len 3): padded. Kills `% → +` (3+2 != 1, no pad).
        let mut segs2 = Vec::new();
        push_inline_chunk(&mut segs2, b"test", &[0xAA, 0xBB, 0xCC]).unwrap();
        assert_eq!(segs2[0].len(), 12); // 4 + 4 + 3 + pad(1)
    }

    /// An `INFO` payload: `"INFO"` FourCC + NUL-terminated, word-aligned subchunks.
    fn info_payload(pairs: &[(&[u8; 4], &str)]) -> Vec<u8> {
        let mut p = b"INFO".to_vec();
        for (cc, val) in pairs {
            let mut v = val.as_bytes().to_vec();
            v.push(0x00);
            p.extend_from_slice(*cc);
            p.extend_from_slice(&u32::try_from(v.len()).unwrap().to_le_bytes());
            p.extend_from_slice(&v);
            if v.len() % 2 == 1 {
                p.push(0x00);
            }
        }
        p
    }

    #[test]
    fn info_to_key_decodes_each_mapped_fourcc() {
        // :245-249 arm deletions: each INFO FourCC must decode to its tag key.
        let cases: [(&[u8; 4], &str, &str); 4] = [
            (b"IPRD", "album", "Anthology"),
            (b"ICRD", "date", "1999"),
            (b"ICMT", "comment", "Nice"),
            (b"ITRK", "tracknumber", "3"),
        ];
        for (cc, key, val) in cases {
            let buf = wav(&[
                (b"fmt ", fmt_pcm()),
                (b"LIST", info_payload(&[(cc, val)])),
                (b"data", vec![0x00; 4]),
            ]);
            let tags = read_tags(&buf);
            assert!(
                tags.contains(&(key.to_string(), val.to_string())),
                "FourCC {:?} must decode to {key}",
                std::str::from_utf8(cc).unwrap()
            );
        }
    }

    #[test]
    fn read_tags_rejects_short_list_without_panic() {
        // :300 `&& → ||`: a LIST chunk with a <4-byte payload. Original
        // short-circuits (`len >= 4` false → no INFO, empty). The `||` mutant
        // evaluates `&slice[0..4]` on the 2-byte slice → panic. Asserting the clean
        // empty result kills it (panic ≠ empty).
        let buf = wav(&[
            (b"fmt ", fmt_pcm()),
            (b"LIST", vec![0x49, 0x4E]), // "IN" — 2 bytes, < 4
            (b"data", vec![0x00; 4]),
        ]);
        assert!(read_tags(&buf).is_empty());
    }

    /// Byte offset, in the assembled stream, of the first `Inline` segment whose
    /// first four bytes are `fourcc`. Used to assert RIFF word-alignment.
    fn inline_offset_of(layout: &RegionLayout, fourcc: &[u8; 4]) -> u64 {
        let mut off = 0u64;
        for s in layout.segments() {
            if let Segment::Inline(b) = s
                && b.len() >= 4
                && &b[0..4] == fourcc
            {
                return off;
            }
            off += s.len();
        }
        panic!("no inline chunk starting with {fourcc:?}");
    }

    #[test]
    fn synthesize_word_aligns_embedded_id3_chunk() {
        // :207 `tag_len % 2 == 1` — the pad after the `id3 ` chunk. When tag_len is
        // odd, the original pads so the following `data` chunk starts on an even
        // byte (RIFF word-alignment). Both mutants (`/`, `+`) drop that pad for odd
        // tag_len, landing `data` on an odd offset.
        //
        // Find tags whose ID3v2 tag_len is odd (parity depends on id3 framing, so
        // discover it rather than hard-code). "albumartist" maps to id3 only (no
        // INFO/LIST chunk), keeping the layout simple.
        let mut tags = Vec::new();
        let mut tag_len = 0u64;
        for n in 1..64 {
            let cand = vec![TagInput::new("albumartist", &"x".repeat(n))];
            let (_, tl) = crate::mp3::build_id3v2_segments(&cand, &[], &[]).unwrap();
            if tl % 2 == 1 {
                tags = cand;
                tag_len = tl;
                break;
            }
        }
        assert_eq!(tag_len % 2, 1, "expected to find an odd-length id3 tag");

        let scan = WavScan {
            fmt: fmt_pcm(),
            fact: None,
        };
        let layout = synthesize_layout(&scan, 0, 8, &tags, &[], &[]).unwrap();
        assert_eq!(
            inline_offset_of(&layout, b"data") % 2,
            0,
            "the data chunk must be word-aligned"
        );
    }

    #[test]
    fn synthesize_rejects_riff_size_overflow() {
        // `BackingAudio` is virtual (no real allocation), so we can pass
        // `audio_length == u32::MAX`: it passes the audio_length u32 check but makes
        // `riff_size > u32::MAX` once the other chunks are added.
        //
        // NOTE — this must use exactly u32::MAX, not `u32::MAX + 1`: the larger
        // value is caught by the audio_length conversion first and never reaches
        // the riff_size check.
        let scan = WavScan {
            fmt: fmt_pcm(),
            fact: None,
        };
        let res = synthesize_layout(&scan, 0, u64::from(u32::MAX), &[], &[], &[]);
        assert_eq!(res, Err(FormatError::TooLarge));
    }

    /// RIFF/WAVE with a `fmt ` (16-byte) chunk and a `data` chunk of `audio`.
    fn wav_file(audio: &[u8]) -> Vec<u8> {
        let mut body = Vec::new();
        body.extend_from_slice(b"fmt ");
        body.extend_from_slice(&16u32.to_le_bytes());
        body.extend(std::iter::repeat_n(0u8, 16));
        body.extend_from_slice(b"data");
        body.extend_from_slice(&u32::try_from(audio.len()).unwrap().to_le_bytes());
        body.extend_from_slice(audio);
        let mut v = b"RIFF".to_vec();
        v.extend_from_slice(&u32::try_from(4 + body.len()).unwrap().to_le_bytes());
        v.extend_from_slice(b"WAVE");
        v.extend_from_slice(&body);
        v
    }

    #[test]
    fn locate_audio_bounded_complete_when_prefix_is_whole_file() {
        let full = wav_file(b"AUDIOAUDIO");
        let file_len = full.len() as u64;
        match locate_audio_bounded(&full, file_len).unwrap() {
            Extent::Complete(b) => assert_eq!(b.audio_length, 10),
            other @ Extent::NeedMore { .. } => panic!("expected Complete, got {other:?}"),
        }
    }

    #[test]
    fn locate_audio_bounded_needmore_when_prefix_short() {
        let full = wav_file(b"AUDIOAUDIO");
        let file_len = full.len() as u64;
        let prefix = &full[..full.len() - 4];
        match locate_audio_bounded(prefix, file_len).unwrap() {
            Extent::NeedMore { up_to } => assert_eq!(up_to, file_len),
            other @ Extent::Complete(_) => panic!("expected NeedMore, got {other:?}"),
        }
    }

    /// A front-only RIFF/WAVE buffer: `fmt ` (16 bytes) plus a `data` chunk header
    /// declaring `data_len` bytes of payload, with the payload itself absent — the
    /// shape the ceiling probe sees when the audio runs past the read budget. The
    /// `data` payload begins at offset 44.
    fn wav_front(data_len: u64) -> Vec<u8> {
        let mut body = Vec::new();
        body.extend_from_slice(b"fmt ");
        body.extend_from_slice(&16u32.to_le_bytes());
        body.extend(std::iter::repeat_n(0u8, 16));
        body.extend_from_slice(b"data");
        body.extend_from_slice(&u32::try_from(data_len).unwrap().to_le_bytes());
        let mut v = b"RIFF".to_vec();
        let riff_size = 36u32 + u32::try_from(data_len).unwrap(); // form: WAVE + fmt + data hdr + payload
        v.extend_from_slice(&riff_size.to_le_bytes());
        v.extend_from_slice(b"WAVE");
        v.extend_from_slice(&body);
        v
    }

    #[test]
    fn locate_audio_at_ceiling_trusts_data_header_without_payload() {
        let data_len = 200u64;
        let front = wav_front(data_len);
        let audio_offset = front.len() as u64; // 44
        let file_len = audio_offset + data_len;
        let b = locate_audio_at_ceiling(&front, file_len).unwrap();
        assert_eq!(b.audio_offset, audio_offset);
        assert_eq!(b.audio_length, data_len);
    }

    #[test]
    fn locate_audio_at_ceiling_accepts_data_shorter_than_file() {
        // Chunks trailing the payload make the file larger than `off + len`.
        let data_len = 200u64;
        let front = wav_front(data_len);
        let audio_offset = front.len() as u64;
        let file_len = audio_offset + data_len + 64;
        let b = locate_audio_at_ceiling(&front, file_len).unwrap();
        assert_eq!(b.audio_offset, audio_offset);
        assert_eq!(b.audio_length, data_len);
    }

    #[test]
    fn locate_audio_at_ceiling_rejects_data_running_past_file() {
        // A header claiming more payload than the file holds is corrupt, not trusted.
        let front = wav_front(1_000);
        let audio_offset = front.len() as u64;
        let file_len = audio_offset + 10;
        assert_eq!(
            locate_audio_at_ceiling(&front, file_len),
            Err(FormatError::Malformed)
        );
    }

    #[test]
    fn locate_audio_at_ceiling_requires_fmt_chunk() {
        // `data` present but no `fmt `: not a usable WAV, must be rejected.
        let mut body = Vec::new();
        body.extend_from_slice(b"data");
        body.extend_from_slice(&200u32.to_le_bytes());
        let mut front = b"RIFF".to_vec();
        front.extend_from_slice(&0u32.to_le_bytes());
        front.extend_from_slice(b"WAVE");
        front.extend_from_slice(&body);
        let file_len = front.len() as u64 + 200;
        assert_eq!(
            locate_audio_at_ceiling(&front, file_len),
            Err(FormatError::NotWav)
        );
    }

    #[test]
    fn locate_audio_rejects_form_end_before_data() {
        // Correctly framed chunks, but the RIFF size declares a form that ends before
        // the data payload. Build with `wav` (valid size) then overwrite bytes 4..8.
        let mut buf = wav(&[(b"fmt ", fmt_pcm()), (b"data", vec![0x11; 8])]);
        buf[4..8].copy_from_slice(&8u32.to_le_bytes()); // form_end = 16, before data
        assert_eq!(locate_audio(&buf), Err(FormatError::Malformed));
    }

    #[test]
    fn locate_audio_rejects_form_end_past_file() {
        let mut buf = wav(&[(b"fmt ", fmt_pcm()), (b"data", vec![0x11; 8])]);
        let huge = u32::try_from(buf.len()).unwrap() + 100;
        buf[4..8].copy_from_slice(&huge.to_le_bytes()); // form_end > physical file
        assert_eq!(locate_audio(&buf), Err(FormatError::Malformed));
    }

    #[test]
    fn locate_audio_accepts_valid_form_with_odd_chunk_and_trailing_metadata() {
        // Odd-size chunk before data (word-padded) + a LIST chunk trailing data, all
        // inside a correctly-sized RIFF form. Must still parse.
        let buf = wav(&[
            (b"fmt ", fmt_pcm()),
            (b"data", vec![0x22; 7]), // odd payload -> 1 pad byte
            (b"LIST", vec![0x33; 4]),
        ]);
        let b = locate_audio(&buf).unwrap();
        assert_eq!(b.audio_length, 7);
    }

    #[test]
    fn locate_audio_at_ceiling_rejects_form_end_before_data() {
        // Ceiling path (over-budget file): the RIFF size declares a form ending
        // before the data payload, while the data payload still fits inside the
        // physical file. Must reject on the `data_end > form_end` clause.
        let mut buf = wav(&[(b"fmt ", fmt_pcm()), (b"data", vec![0x11; 8])]);
        let file_len = buf.len() as u64;
        buf[4..8].copy_from_slice(&8u32.to_le_bytes()); // form_end = 16, before data
        assert_eq!(
            locate_audio_at_ceiling(&buf, file_len),
            Err(FormatError::Malformed)
        );
    }

    #[test]
    fn locate_audio_rejects_fmt_outside_declared_form() {
        // `data` first (in-form), then a `fmt ` chunk located past `form_end`. The
        // declared form does not contain `fmt `, so reject even though the `data`
        // payload fits the form. (data ends at 28; fmt spans 28..52.)
        let mut buf = wav(&[(b"data", vec![0x11; 8]), (b"fmt ", fmt_pcm())]);
        buf[4..8].copy_from_slice(&20u32.to_le_bytes()); // form_end = 28: covers data, not fmt
        assert_eq!(locate_audio(&buf), Err(FormatError::Malformed));
    }

    #[test]
    fn locate_audio_at_ceiling_rejects_fmt_outside_declared_form() {
        let mut buf = wav(&[(b"data", vec![0x11; 8]), (b"fmt ", fmt_pcm())]);
        let file_len = buf.len() as u64;
        buf[4..8].copy_from_slice(&20u32.to_le_bytes()); // form_end = 28: covers data, not fmt
        assert_eq!(
            locate_audio_at_ceiling(&buf, file_len),
            Err(FormatError::Malformed)
        );
    }

    #[test]
    fn wav_read_binary_tags_extracts_id3_chunk_frames() {
        use id3::frame::{Content, Unknown};
        use id3::{Frame, Tag, TagLike, Version};
        let mut tag = Tag::new();
        tag.add_frame(Frame::with_content(
            "PRIV",
            Content::Unknown(Unknown {
                data: vec![5, 6, 7],
                version: Version::Id3v24,
            }),
        ));
        let mut id3 = Vec::new();
        id3::Encoder::new()
            .version(Version::Id3v24)
            .encode(&tag, &mut id3)
            .unwrap();
        let wav = wav(&[(b"id3 ", id3)]);

        let (opaque, _promoted) = super::read_binary_tags(&wav);
        let priv_tag = opaque
            .iter()
            .find(|e| e.key == "PRIV")
            .expect("PRIV preserved");
        assert_eq!(priv_tag.payload, vec![5, 6, 7]);
    }

    // Documented EQUIVALENT mutants in this file (no test targets them; each was
    // confirmed by hand-apply — the relevant test stays green under the mutation):
    //  * walk_chunks:49  guard `next <= buf.len()` → `true`. When `next > buf.len()`
    //    the mutant sets `pos = next`, but the `while pos + 8 <= buf.len()` test is
    //    then immediately false, so the output Vec is identical to the original's
    //    `break` (the header was pushed before the advance).
    //  * synthesize_layout:186  `audio_length > u32::MAX` (both `==` and `>=`).
    //    `body_len >= audio_length`, so whenever this would fire, `riff_size`
    //    overflows and the :227 guard returns the identical TooLarge.
    //  * synthesize_layout:227  `> → >=` only. Every synthesized chunk is
    //    word-aligned, so `riff_size` is always even; `riff_size == u32::MAX` (odd)
    //    is unreachable, the only point where `>` and `>=` differ.
}
