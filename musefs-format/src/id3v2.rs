//! Shared ID3v2 tag-boundary arithmetic.
//!
//! MP3 carries a leading ID3v2 tag by design. FLAC does not — but files that
//! prepend one (or several) before the `fLaC` marker exist in the wild (#602),
//! so both formats need the same question answered: where does the tag end?
//! The header decode lives here so the two callers cannot drift apart.
//!
//! Nothing here parses frames; see `mp3::read_tags` / `mp3::read_pictures` for
//! the contents, which are gated separately by `mp3::id3v2_alloc_safe`.

use crate::error::{FormatError, Result};
use crate::probe::Extent;

/// Decode a 4-byte synchsafe integer (28 bits, high bit clear in every byte) —
/// the encoding ID3v2 uses for its tag size and for v2.4 frame sizes.
pub(crate) fn synchsafe_decode(b: &[u8]) -> u32 {
    u32::from(b[0] & 0x7F) << 21
        | u32::from(b[1] & 0x7F) << 14
        | u32::from(b[2] & 0x7F) << 7
        | u32::from(b[3] & 0x7F)
}

/// Bytes in an ID3v2 header: `"ID3"`, 2 version bytes, flags, and a 4-byte size.
pub(crate) const HEADER_LEN: usize = 10;

/// Bytes in the optional ID3v2.4 footer, present when header flag bit 4 is set.
const FOOTER_LEN: usize = 10;

/// Does `data` begin with the `ID3` magic? A cheap pre-check for callers that
/// want to skip work (an extra read, a fallback parse) on the common case of no
/// tag at all; it does not validate the header.
#[must_use]
pub fn starts_with_tag(data: &[u8]) -> bool {
    data.len() >= 3 && &data[0..3] == b"ID3"
}

/// End of the ID3v2 tag *body* at the front of `data` — `HEADER_LEN` plus the
/// declared size, excluding any v2.4 footer. `None` when `data` does not start
/// with a tag header (or is too short to hold one).
///
/// This is the bound for walking the tag's frames. Use [`total_len`] to find
/// where the next thing in the file begins.
pub(crate) fn body_end(data: &[u8]) -> Result<Option<usize>> {
    if data.len() < HEADER_LEN || &data[0..3] != b"ID3" {
        return Ok(None);
    }
    if !matches!(data[3], 2..=4) {
        return Err(FormatError::Malformed);
    }
    // A well-formed synchsafe size has the high bit clear in every byte; reject
    // if any size byte has it set (the id3 crate may not mask those bits).
    if data[6..HEADER_LEN].iter().any(|&b| b & 0x80 != 0) {
        return Err(FormatError::Malformed);
    }
    Ok(Some(
        HEADER_LEN + synchsafe_decode(&data[6..HEADER_LEN]) as usize,
    ))
}

/// Total on-disk length of the ID3v2 tag at the front of `data`: header, body,
/// and the v2.4 footer when the header flags declare one. `None` when `data`
/// does not start with a tag header.
pub(crate) fn total_len(data: &[u8]) -> Result<Option<usize>> {
    let Some(end) = body_end(data)? else {
        return Ok(None);
    };
    let has_footer = data[5] & 0x10 != 0;
    Ok(Some(if has_footer { end + FOOTER_LEN } else { end }))
}

/// Combined length of the run of consecutive ID3v2 tags at the front of `data`,
/// i.e. the offset at which the real stream begins. `0` when there is no tag.
///
/// `NeedMore { up_to }` when a tag's declared length runs past `data`: the
/// bytes needed to step over it are not present, so a bounded caller must widen
/// its window to `up_to` and retry. Each tag is at least `HEADER_LEN` bytes, so
/// the walk always makes progress.
pub(crate) fn leading_tags_len(data: &[u8]) -> Result<Extent<usize>> {
    let mut off = 0usize;
    loop {
        let rest = &data[off..];
        if !starts_with_tag(rest) {
            return Ok(Extent::Complete(off));
        }
        if rest.len() < HEADER_LEN {
            // The magic is there but the header is cut short by the window.
            return Ok(Extent::NeedMore {
                up_to: (off + HEADER_LEN) as u64,
            });
        }
        let Some(len) = total_len(rest)? else {
            return Ok(Extent::Complete(off));
        };
        // A 28-bit synchsafe size cannot overflow usize on any supported target.
        off += len;
        if off > data.len() {
            return Ok(Extent::NeedMore { up_to: off as u64 });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal ID3v2.4 tag header declaring `body` bytes of (absent) frames.
    fn header(body_len: u32, flags: u8) -> Vec<u8> {
        let ss = [
            ((body_len >> 21) & 0x7F) as u8,
            ((body_len >> 14) & 0x7F) as u8,
            ((body_len >> 7) & 0x7F) as u8,
            (body_len & 0x7F) as u8,
        ];
        let mut v = vec![b'I', b'D', b'3', 4, 0, flags];
        v.extend_from_slice(&ss);
        v
    }

    fn tag(body_len: u32, flags: u8) -> Vec<u8> {
        let mut v = header(body_len, flags);
        v.extend(std::iter::repeat_n(0u8, body_len as usize));
        if flags & 0x10 != 0 {
            v.extend_from_slice(&[b'3', b'D', b'I', 4, 0, flags, 0, 0, 0, 0]);
        }
        v
    }

    #[test]
    fn no_tag_is_zero_length() {
        assert_eq!(
            leading_tags_len(b"fLaC\x80\x00\x00\x22").unwrap(),
            Extent::Complete(0)
        );
        assert!(!starts_with_tag(b"fLaC"));
    }

    #[test]
    fn single_tag_reports_header_plus_body() {
        let mut f = tag(20, 0);
        let tag_len = f.len();
        f.extend_from_slice(b"fLaC");
        assert_eq!(leading_tags_len(&f).unwrap(), Extent::Complete(tag_len));
        assert_eq!(tag_len, HEADER_LEN + 20);
    }

    #[test]
    fn footer_flag_adds_ten_bytes() {
        let f = tag(20, 0x10);
        assert_eq!(total_len(&f).unwrap(), Some(HEADER_LEN + 20 + FOOTER_LEN));
        // body_end stays footer-free: it bounds the frame walk, not the file.
        assert_eq!(body_end(&f).unwrap(), Some(HEADER_LEN + 20));
    }

    #[test]
    fn consecutive_tags_accumulate() {
        let mut f = tag(8, 0);
        f.extend(tag(12, 0));
        f.extend_from_slice(b"fLaC");
        assert_eq!(
            leading_tags_len(&f).unwrap(),
            Extent::Complete(2 * HEADER_LEN + 8 + 12)
        );
    }

    #[test]
    fn body_running_past_the_window_needs_more() {
        let mut f = header(4096, 0);
        f.extend_from_slice(b"only a few body bytes");
        assert_eq!(
            leading_tags_len(&f).unwrap(),
            Extent::NeedMore {
                up_to: (HEADER_LEN + 4096) as u64
            }
        );
    }

    #[test]
    fn truncated_header_needs_more() {
        assert_eq!(
            leading_tags_len(b"ID3\x04").unwrap(),
            Extent::NeedMore {
                up_to: HEADER_LEN as u64
            }
        );
    }

    #[test]
    fn bad_version_and_size_are_malformed() {
        let mut bad_version = header(4, 0);
        bad_version[3] = 9;
        assert!(leading_tags_len(&bad_version).is_err());

        let mut bad_size = header(4, 0);
        bad_size[7] = 0x80; // high bit set in a synchsafe byte
        assert!(leading_tags_len(&bad_size).is_err());
    }

    #[test]
    fn zero_length_tags_still_terminate() {
        let mut f = tag(0, 0);
        f.extend(tag(0, 0));
        f.extend_from_slice(b"fLaC");
        assert_eq!(
            leading_tags_len(&f).unwrap(),
            Extent::Complete(2 * HEADER_LEN)
        );
    }
}
