//! VorbisComment body build/parse, shared by FLAC's VORBIS_COMMENT block and the
//! Ogg codecs' comment packets. This is the body only: it never includes the
//! Vorbis framing bit or any codec-specific magic.

use crate::error::{FormatError, Result};
use crate::input::TagInput;

pub(crate) const VENDOR: &str = "musefs";

/// True if `key` is a legal VorbisComment field name: one or more characters in
/// ASCII 0x20..=0x7D, excluding 0x3D (`=`). This is the Vorbis spec grammar and
/// matches what mutagen/TagLib enforce when writing. Non-ASCII, control chars,
/// `=`, and the empty string are all rejected.
pub fn is_valid_key(key: &str) -> bool {
    !key.is_empty() && key.bytes().all(|b| (0x20..=0x7D).contains(&b) && b != b'=')
}

pub(crate) fn build(tags: &[TagInput]) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    out.extend_from_slice(
        &u32::try_from(VENDOR.len())
            .map_err(|_| FormatError::TooLarge)?
            .to_le_bytes(),
    );
    out.extend_from_slice(VENDOR.as_bytes());
    // Skip keys outside the Vorbis field-name grammar (e.g. an `=` key would shift
    // the key/value boundary on re-parse). build() is the single enforcement point,
    // so it stays total for any caller — including the fuzz harness.
    let valid: Vec<&TagInput> = tags.iter().filter(|t| is_valid_key(&t.key)).collect();
    out.extend_from_slice(
        &u32::try_from(valid.len())
            .map_err(|_| FormatError::TooLarge)?
            .to_le_bytes(),
    );
    for t in valid {
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
    fn build_skips_keys_outside_vorbis_grammar() {
        // The issue's case: an `a=b` key would otherwise synthesize `A=B=c` and
        // shift the boundary on re-parse. Empty keys are also dropped. Valid keys
        // keep their order, and the comment count reflects only survivors.
        let tags = vec![
            TagInput::new("artist", "Alice"),
            TagInput::new("a=b", "c"),
            TagInput::new("", "x"),
            TagInput::new("title", "Song"),
        ];
        let body = build(&tags).unwrap();
        let parsed = parse(&body).unwrap();
        assert_eq!(
            parsed,
            vec![
                ("artist".to_string(), "Alice".to_string()),
                ("title".to_string(), "Song".to_string()),
            ]
        );
    }

    #[test]
    fn build_is_total_over_arbitrary_keys() {
        // build() must remain total: it has no assert guarding key validity, so the
        // fuzz harness can feed it arbitrary keys. It must never panic and must emit
        // only valid comments.
        let tags = vec![
            TagInput::new("a=b=c", "v"),
            TagInput::new("\u{0}\u{1}", "v"),
            TagInput::new("ok", "v"),
        ];
        let body = build(&tags).unwrap();
        let parsed = parse(&body).unwrap();
        assert_eq!(parsed, vec![("OK".to_string(), "v".to_string())]);
    }

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
    fn is_valid_key_enforces_vorbis_grammar() {
        // Legal: one-or-more ASCII 0x20..=0x7D, excluding '=' (0x3D).
        assert!(is_valid_key("title"));
        assert!(is_valid_key("CUSTOM_THING"));
        assert!(is_valid_key("}")); // 0x7D, upper bound
        assert!(is_valid_key(" ")); // 0x20, lower bound
        // Illegal.
        assert!(!is_valid_key("")); // empty
        assert!(!is_valid_key("a=b")); // contains '='
        assert!(!is_valid_key("a\u{1f}b")); // control char 0x1F
        assert!(!is_valid_key("a\u{7f}b")); // DEL 0x7F
        assert!(!is_valid_key("a~b")); // 0x7E, just past upper bound
        assert!(!is_valid_key("género")); // non-ASCII high bytes
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
