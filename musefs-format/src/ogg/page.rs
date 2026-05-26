use crate::error::{FormatError, Result};

pub const CAPTURE: &[u8; 4] = b"OggS";

/// Header-type flag bits.
pub const FLAG_CONTINUED: u8 = 0x01;
pub const FLAG_BOS: u8 = 0x02;
pub const FLAG_EOS: u8 = 0x04;

/// A parsed Ogg page header (the 27 fixed bytes + the segment table) plus the
/// derived payload length. Multi-byte fields are little-endian on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PageHeader {
    pub header_type: u8,
    pub granule: u64,
    pub serial: u32,
    pub seq: u32,
    pub crc: u32,
    pub seg_count: u8,
    pub header_len: usize,
    pub payload_len: usize,
}

impl PageHeader {
    pub fn total_len(&self) -> usize {
        self.header_len + self.payload_len
    }
}

/// Parse the page starting at `pos`. Errors if the capture pattern is missing or
/// the buffer is too short for the header + segment table.
pub fn parse_page(buf: &[u8], pos: usize) -> Result<PageHeader> {
    if pos + 27 > buf.len() || &buf[pos..pos + 4] != CAPTURE {
        return Err(FormatError::Malformed);
    }
    let header_type = buf[pos + 5];
    let granule = u64::from_le_bytes(buf[pos + 6..pos + 14].try_into().unwrap());
    let serial = u32::from_le_bytes(buf[pos + 14..pos + 18].try_into().unwrap());
    let seq = u32::from_le_bytes(buf[pos + 18..pos + 22].try_into().unwrap());
    let crc = u32::from_le_bytes(buf[pos + 22..pos + 26].try_into().unwrap());
    let seg_count = buf[pos + 26];
    let table_start = pos + 27;
    let table_end = table_start + seg_count as usize;
    if table_end > buf.len() {
        return Err(FormatError::Malformed);
    }
    let payload_len: usize = buf[table_start..table_end]
        .iter()
        .map(|&b| b as usize)
        .sum();
    Ok(PageHeader {
        header_type,
        granule,
        serial,
        seq,
        crc,
        seg_count,
        header_len: 27 + seg_count as usize,
        payload_len,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hand_page() -> Vec<u8> {
        let mut p = Vec::new();
        p.extend_from_slice(CAPTURE);
        p.push(0); // version
        p.push(FLAG_BOS); // header_type
        p.extend_from_slice(&0u64.to_le_bytes()); // granule
        p.extend_from_slice(&0xDEAD_BEEFu32.to_le_bytes()); // serial
        p.extend_from_slice(&7u32.to_le_bytes()); // seq
        p.extend_from_slice(&0x1122_3344u32.to_le_bytes()); // crc field (as stored)
        p.push(2); // seg_count
        p.push(0x10);
        p.push(0x20); // segment table => payload 0x30
        p.extend(std::iter::repeat(0xAB).take(0x30));
        p
    }

    #[test]
    fn parses_fields_and_lengths() {
        let p = hand_page();
        let h = parse_page(&p, 0).unwrap();
        assert_eq!(h.header_type, FLAG_BOS);
        assert_eq!(h.serial, 0xDEAD_BEEF);
        assert_eq!(h.seq, 7);
        assert_eq!(h.crc, 0x1122_3344);
        assert_eq!(h.seg_count, 2);
        assert_eq!(h.payload_len, 0x30);
        assert_eq!(h.header_len, 29);
        assert_eq!(h.total_len(), 0x30 + 29);
    }

    #[test]
    fn rejects_bad_capture() {
        let mut p = hand_page();
        p[0] = b'X';
        assert_eq!(parse_page(&p, 0), Err(FormatError::Malformed));
    }
}
