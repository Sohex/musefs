use super::crc::crc32;
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

/// Encode `payload_len` as Ogg lacing values: ⌊L/255⌋ values of 255 followed by
/// one value of L mod 255. When L is a multiple of 255 this appends a terminating
/// 0, which is required to signal the packet's end.
fn lacing_values(payload_len: usize) -> Vec<u8> {
    let mut v = vec![255u8; payload_len / 255];
    v.push((payload_len % 255) as u8);
    v
}

/// Lace one packet into one or more pages starting at sequence number `seq_start`.
/// Each page carries up to 255 lacing values (≤ 65 025 payload bytes). `bos` sets
/// the BOS flag on the packet's first page; continuation pages get FLAG_CONTINUED.
/// All pages use the given `granule`. Returns `(bytes, pages_used)`.
pub fn lace_packet(
    serial: u32,
    seq_start: u32,
    bos: bool,
    granule: u64,
    packet: &[u8],
) -> (Vec<u8>, u32) {
    let laces = lacing_values(packet.len());
    let mut out = Vec::new();
    let mut seq = seq_start;
    let mut lace_pos = 0usize;
    let mut payload_pos = 0usize;
    let mut first = true;
    // Always emit at least one page (handles a zero-length packet: laces == [0]).
    while first || lace_pos < laces.len() {
        let chunk = (laces.len() - lace_pos).min(255);
        let table = &laces[lace_pos..lace_pos + chunk];
        let page_payload: usize = table.iter().map(|&b| b as usize).sum();

        let mut header_type = 0u8;
        if bos && first {
            header_type |= FLAG_BOS;
        }
        if !first {
            header_type |= FLAG_CONTINUED;
        }

        let page_start = out.len();
        out.extend_from_slice(CAPTURE);
        out.push(0);
        out.push(header_type);
        out.extend_from_slice(&granule.to_le_bytes());
        out.extend_from_slice(&serial.to_le_bytes());
        out.extend_from_slice(&seq.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes()); // CRC placeholder
        out.push(chunk as u8);
        out.extend_from_slice(table);
        out.extend_from_slice(&packet[payload_pos..payload_pos + page_payload]);

        let crc = crc32(&out[page_start..]);
        out[page_start + 22..page_start + 26].copy_from_slice(&crc.to_le_bytes());

        lace_pos += chunk;
        payload_pos += page_payload;
        seq += 1;
        first = false;
    }
    (out, seq - seq_start)
}

/// Lace a sequence of header packets onto fresh pages starting at sequence 0, with
/// BOS on the very first page and granule 0 throughout (header pages carry no
/// audio). Each packet begins a new page. Returns `(bytes, page_count)`.
pub fn build_header(serial: u32, packets: &[&[u8]]) -> (Vec<u8>, u32) {
    let mut out = Vec::new();
    let mut seq = 0u32;
    for (i, pkt) in packets.iter().enumerate() {
        let (bytes, used) = lace_packet(serial, seq, i == 0, 0, pkt);
        out.extend_from_slice(&bytes);
        seq += used;
    }
    (out, seq)
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

    #[test]
    fn single_page_packet_round_trips_and_crc_valid() {
        let packet: Vec<u8> = (0..200u8).collect();
        let (bytes, pages) = lace_packet(0xABCD, 0, true, 0, &packet);
        assert_eq!(pages, 1);
        let h = parse_page(&bytes, 0).unwrap();
        assert_eq!(h.header_type, FLAG_BOS);
        assert_eq!(h.payload_len, 200);
        // CRC self-check: zero the field, recompute, compare to stored.
        let mut z = bytes.clone();
        z[22..26].copy_from_slice(&0u32.to_le_bytes());
        assert_eq!(crc32(&z), h.crc);
    }

    #[test]
    fn exact_multiple_of_255_appends_terminating_zero() {
        let packet = vec![0u8; 255];
        let (bytes, pages) = lace_packet(1, 0, false, 0, &packet);
        assert_eq!(pages, 1);
        let h = parse_page(&bytes, 0).unwrap();
        // 255 + terminating 0 => two lacing values, both summing to 255 payload.
        assert_eq!(h.seg_count, 2);
        assert_eq!(h.payload_len, 255);
    }

    #[test]
    fn large_packet_spans_multiple_pages_with_continuation() {
        let packet = vec![0x5Au8; 70_000]; // > 65 025 => 2 pages
        let (bytes, pages) = lace_packet(2, 5, false, 0, &packet);
        assert_eq!(pages, 2);
        let p0 = parse_page(&bytes, 0).unwrap();
        assert_eq!(p0.header_type & FLAG_CONTINUED, 0);
        assert_eq!(p0.payload_len, 65_025);
        let p1 = parse_page(&bytes, p0.total_len()).unwrap();
        assert_eq!(p1.header_type & FLAG_CONTINUED, FLAG_CONTINUED);
        assert_eq!(p1.seq, 6);
        assert_eq!(p0.payload_len + p1.payload_len, 70_000);
    }

    #[test]
    fn build_header_numbers_pages_and_sets_bos_once() {
        let a = vec![1u8; 10];
        let b = vec![2u8; 10];
        let (bytes, count) = build_header(9, &[&a, &b]);
        assert_eq!(count, 2);
        let p0 = parse_page(&bytes, 0).unwrap();
        let p1 = parse_page(&bytes, p0.total_len()).unwrap();
        assert_eq!(p0.header_type & FLAG_BOS, FLAG_BOS);
        assert_eq!(p1.header_type & FLAG_BOS, 0);
        assert_eq!(p0.seq, 0);
        assert_eq!(p1.seq, 1);
    }
}
