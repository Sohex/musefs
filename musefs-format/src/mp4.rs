//! Hand-rolled MP4/M4A box layer: parse the structure, read iTunes metadata, and
//! regenerate `moov` (with patched chunk offsets) to synthesize a re-tagged file
//! whose `mdat` audio payload is served verbatim. Strict: anything outside the
//! supported shape (single audio track, one `mdat`, non-fragmented) is rejected.

use crate::error::{FormatError, Result};
use crate::input::{ArtInput, EmbeddedPicture, TagInput};
use crate::layout::{RegionLayout, Segment};

fn be_u32(b: &[u8], pos: usize) -> Result<u32> {
    let s = b.get(pos..pos + 4).ok_or(FormatError::Malformed)?;
    Ok(u32::from_be_bytes(s.try_into().unwrap()))
}

fn be_u64(b: &[u8], pos: usize) -> Result<u64> {
    let s = b.get(pos..pos + 8).ok_or(FormatError::Malformed)?;
    Ok(u64::from_be_bytes(s.try_into().unwrap()))
}

/// A located box header within some buffer. `start` is relative to that buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct BoxRef {
    kind: [u8; 4],
    start: usize,
    header_len: usize, // 8, or 16 for 64-bit largesize
    total_len: usize,  // header + payload
}

impl BoxRef {
    fn payload_start(&self) -> usize {
        self.start + self.header_len
    }
    fn end(&self) -> usize {
        self.start + self.total_len
    }
    /// `buf` must be the same buffer `read_box` parsed this header from — offsets
    /// are relative to it. The debug assertion catches a wrong-buffer call in tests.
    fn payload<'a>(&self, buf: &'a [u8]) -> &'a [u8] {
        debug_assert!(
            self.end() <= buf.len(),
            "BoxRef::payload called with a buffer it was not parsed from"
        );
        &buf[self.payload_start()..self.end()]
    }
}

fn read_box(buf: &[u8], pos: usize) -> Result<BoxRef> {
    let size32 = be_u32(buf, pos)? as u64;
    let kind: [u8; 4] = buf
        .get(pos + 4..pos + 8)
        .ok_or(FormatError::Malformed)?
        .try_into()
        .unwrap();
    let (header_len, total) = match size32 {
        1 => (16usize, be_u64(buf, pos + 8)?),
        0 => (8usize, (buf.len() - pos) as u64),
        n => (8usize, n),
    };
    let total = total as usize;
    if total < header_len || pos + total > buf.len() {
        return Err(FormatError::Malformed);
    }
    Ok(BoxRef { kind, start: pos, header_len, total_len: total })
}

fn child_boxes(buf: &[u8]) -> Result<Vec<BoxRef>> {
    let mut out = Vec::new();
    let mut pos = 0;
    while pos + 8 <= buf.len() {
        let b = read_box(buf, pos)?;
        pos = b.end();
        out.push(b);
    }
    Ok(out)
}

fn find_box(buf: &[u8], kind: &[u8; 4]) -> Result<Option<BoxRef>> {
    Ok(child_boxes(buf)?.into_iter().find(|b| &b.kind == kind))
}

/// Descend a path of box types; return `(payload_start, payload_len)` relative to
/// `buf` for the box at the end of the path, or None if any step is missing.
fn find_path(buf: &[u8], path: &[&[u8; 4]]) -> Result<Option<(usize, usize)>> {
    let mut base = 0usize;
    let mut last = None;
    for kind in path {
        let region = &buf[base..];
        let b = match find_box(region, kind)? {
            Some(b) => b,
            None => return Ok(None),
        };
        let ps = base + b.payload_start();
        last = Some((ps, b.total_len - b.header_len));
        base = ps;
    }
    Ok(last)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a 32-bit-size box: [size][type][payload].
    fn bx(kind: &[u8; 4], payload: &[u8]) -> Vec<u8> {
        let mut v = ((8 + payload.len()) as u32).to_be_bytes().to_vec();
        v.extend_from_slice(kind);
        v.extend_from_slice(payload);
        v
    }

    #[test]
    fn walks_top_level_boxes() {
        let mut buf = bx(b"ftyp", b"M4A ");
        buf.extend(bx(b"free", b"\x00\x00"));
        let boxes = child_boxes(&buf).unwrap();
        assert_eq!(boxes.len(), 2);
        assert_eq!(&boxes[0].kind, b"ftyp");
        assert_eq!(boxes[0].payload(&buf), b"M4A ");
        assert_eq!(&boxes[1].kind, b"free");
    }

    #[test]
    fn find_box_and_nested_path() {
        let mut hdlr_payload = vec![0u8; 8];
        hdlr_payload.extend_from_slice(b"soun");
        hdlr_payload.extend_from_slice(&[0u8; 12]);
        let moov = bx(b"moov", &bx(b"trak", &bx(b"mdia", &bx(b"hdlr", &hdlr_payload))));

        let m = find_box(&moov, b"moov").unwrap().unwrap();
        let (start, len) = find_path(m.payload(&moov), &[b"trak", b"mdia", b"hdlr"])
            .unwrap()
            .unwrap();
        assert_eq!(&m.payload(&moov)[start..start + len][8..12], b"soun");
    }

    #[test]
    fn rejects_truncated_box() {
        let buf = [0u8, 0, 0, 99, b'm', b'o', b'o', b'v']; // claims 99, only 8 present
        assert!(child_boxes(&buf).is_err());
    }
}
