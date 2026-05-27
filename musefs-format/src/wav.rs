use crate::error::{FormatError, Result};

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
    let mut pos = match riff_wave_start(buf) {
        Ok(p) => p,
        Err(_) => return out,
    };
    while pos + 8 <= buf.len() {
        let mut id = [0u8; 4];
        id.copy_from_slice(&buf[pos..pos + 4]);
        let size = u32::from_le_bytes([buf[pos + 4], buf[pos + 5], buf[pos + 6], buf[pos + 7]]) as u64;
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
