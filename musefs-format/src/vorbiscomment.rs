//! VorbisComment body build/parse, shared by FLAC's VORBIS_COMMENT block and the
//! Ogg codecs' comment packets. This is the body only: it never includes the
//! Vorbis framing bit or any codec-specific magic.

use crate::error::{FormatError, Result};
use crate::input::TagInput;

pub(crate) const VENDOR: &str = "musefs";

/// Build a VorbisComment body: vendor string then count then `KEY=value` comments.
/// Lengths are 32-bit little-endian. Known canonical keys are mapped to their
/// Vorbis field name via the vocabulary; unknown keys are upper-cased.
pub(crate) fn build(tags: &[TagInput]) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    out.extend_from_slice(
        &u32::try_from(VENDOR.len())
            .map_err(|_| FormatError::TooLarge)?
            .to_le_bytes(),
    );
    out.extend_from_slice(VENDOR.as_bytes());
    out.extend_from_slice(
        &u32::try_from(tags.len())
            .map_err(|_| FormatError::TooLarge)?
            .to_le_bytes(),
    );
    for t in tags {
        let field = crate::tagmap::key_to_vorbis(&t.key)
            .map_or_else(|| t.key.to_ascii_uppercase(), str::to_string);
        let comment = format!("{field}={}", t.value);
        out.extend_from_slice(
            &u32::try_from(comment.len())
                .map_err(|_| FormatError::TooLarge)?
                .to_le_bytes(),
        );
        out.extend_from_slice(comment.as_bytes());
    }
    Ok(out)
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

/// Parse a VorbisComment body into `(key, value)` pairs in order. Comments
/// without a `=` are skipped. Known Vorbis field names are folded to their
/// canonical (lowercase) key via the vocabulary; unknown fields are kept verbatim.
/// Trailing bytes after the comment list (e.g. a Vorbis framing bit) are ignored.
pub fn parse(body: &[u8]) -> Result<Vec<(String, String)>> {
    let vendor_len = read_u32_le(body, 0)? as usize;
    let mut pos = 4 + vendor_len;
    let count = read_u32_le(body, pos)? as usize;
    pos += 4;
    let mut out = Vec::with_capacity(count.min(body.len() / 4));
    for _ in 0..count {
        let clen = read_u32_le(body, pos)? as usize;
        pos += 4;
        let end = pos + clen;
        if end > body.len() {
            return Err(FormatError::Malformed);
        }
        let comment = std::str::from_utf8(&body[pos..end]).map_err(|_| FormatError::Malformed)?;
        if let Some((field, value)) = comment.split_once('=') {
            let key = crate::tagmap::vorbis_to_key(field)
                .map_or_else(|| field.to_string(), str::to_string);
            out.push((key, value.to_string()));
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
        let body = build(&tags).unwrap();
        let parsed = parse(&body).unwrap();
        assert_eq!(
            parsed,
            vec![
                ("artist".to_string(), "Boards of Canada".to_string()),
                ("title".to_string(), "Roygbiv".to_string()),
            ]
        );
    }

    #[test]
    fn parse_rejects_bogus_huge_count_without_oom() {
        // vendor_len = 0, then count = u32::MAX, then no comment data.
        let mut body = Vec::new();
        body.extend_from_slice(&0u32.to_le_bytes()); // vendor_len = 0
        body.extend_from_slice(&u32::MAX.to_le_bytes()); // count ~= 4 billion; cap prevents OOM
        assert!(parse(&body).is_err());
    }

    #[test]
    fn parse_canonicalizes_known_fields_and_preserves_unknown() {
        let tags = vec![
            TagInput::new("albumartist", "VA"),
            TagInput::new("custom_thing", "x"),
        ];
        let body = build(&tags).unwrap();
        let parsed = parse(&body).unwrap();
        // build upper-cases unknown keys; parse folds known fields to canonical,
        // keeps unknown verbatim.
        assert_eq!(parsed[0], ("albumartist".to_string(), "VA".to_string()));
        assert_eq!(parsed[1], ("CUSTOM_THING".to_string(), "x".to_string()));
    }
}
