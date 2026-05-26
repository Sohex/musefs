//! VorbisComment body build/parse, shared by FLAC's VORBIS_COMMENT block and the
//! Ogg codecs' comment packets. This is the body only: it never includes the
//! Vorbis framing bit or any codec-specific magic.

use crate::error::{FormatError, Result};
use crate::input::TagInput;

pub(crate) const VENDOR: &str = "musefs";

/// Build a VorbisComment body: vendor string then count then `KEY=value` comments.
/// Lengths are 32-bit little-endian; keys are upper-cased.
pub(crate) fn build(tags: &[TagInput]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&(VENDOR.len() as u32).to_le_bytes());
    out.extend_from_slice(VENDOR.as_bytes());
    out.extend_from_slice(&(tags.len() as u32).to_le_bytes());
    for t in tags {
        let comment = format!("{}={}", t.key.to_ascii_uppercase(), t.value);
        out.extend_from_slice(&(comment.len() as u32).to_le_bytes());
        out.extend_from_slice(comment.as_bytes());
    }
    out
}

fn read_u32_le(data: &[u8], pos: usize) -> Result<u32> {
    if pos + 4 > data.len() {
        return Err(FormatError::Malformed);
    }
    Ok(u32::from_le_bytes([
        data[pos],
        data[pos + 1],
        data[pos + 2],
        data[pos + 3],
    ]))
}

/// Parse a VorbisComment body into `(FIELD, value)` pairs in order. Comments
/// without a `=` are skipped. Trailing bytes after the comment list (e.g. a Vorbis
/// framing bit) are ignored.
pub(crate) fn parse(body: &[u8]) -> Result<Vec<(String, String)>> {
    let vendor_len = read_u32_le(body, 0)? as usize;
    let mut pos = 4 + vendor_len;
    let count = read_u32_le(body, pos)? as usize;
    pos += 4;
    let mut out = Vec::with_capacity(count);
    for _ in 0..count {
        let clen = read_u32_le(body, pos)? as usize;
        pos += 4;
        let end = pos + clen;
        if end > body.len() {
            return Err(FormatError::Malformed);
        }
        let comment = std::str::from_utf8(&body[pos..end]).map_err(|_| FormatError::Malformed)?;
        if let Some((field, value)) = comment.split_once('=') {
            out.push((field.to_string(), value.to_string()));
        }
        pos = end;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::TagInput;

    #[test]
    fn build_then_parse_round_trips() {
        let tags = vec![
            TagInput::new("artist", "Boards of Canada"),
            TagInput::new("title", "Roygbiv"),
        ];
        let body = build(&tags);
        let parsed = parse(&body).unwrap();
        assert_eq!(
            parsed,
            vec![
                ("ARTIST".to_string(), "Boards of Canada".to_string()),
                ("TITLE".to_string(), "Roygbiv".to_string()),
            ]
        );
    }
}
