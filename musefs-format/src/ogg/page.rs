use super::crc::crc32;
use crate::error::{FormatError, Result};

pub const CAPTURE: &[u8; 4] = b"OggS";

/// Header-type flag bits.
pub const FLAG_CONTINUED: u8 = 0x01;
pub const FLAG_BOS: u8 = 0x02;

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
    if buf[pos + 4] != 0 {
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
pub(crate) fn lacing_values(payload_len: usize) -> Vec<u8> {
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

/// A packet reassembled from one or more pages, plus the byte offset just past
/// the page on which it completed (used to locate where audio begins).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadPacket {
    pub data: Vec<u8>,
    pub end_offset: usize,
    pub pages_through_end: u32,
}

/// Reassemble up to `want` packets from the pages starting at `data[0]`. Stops as
/// soon as `want` packets have completed (audio for Opus/Vorbis/OggFLAC begins on
/// a fresh page after the header packets). A packet ends at the first lacing value
/// < 255.
pub fn read_packets(data: &[u8], want: usize) -> Result<Vec<ReadPacket>> {
    let mut out: Vec<ReadPacket> = Vec::new();
    let mut pos = 0usize;
    let mut pages = 0u32;
    let mut cur: Vec<u8> = Vec::new();
    while out.len() < want {
        let h = parse_page(data, pos)?;
        pages += 1;
        let table_start = pos + 27;
        let mut payload_pos = h.header_len;
        for i in 0..h.seg_count as usize {
            let lace = data[table_start + i] as usize;
            let seg_start = pos + payload_pos;
            let seg_end = seg_start + lace;
            if seg_end > data.len() {
                return Err(FormatError::Malformed);
            }
            cur.extend_from_slice(&data[seg_start..seg_end]);
            payload_pos += lace;
            if lace < 255 {
                out.push(ReadPacket {
                    data: std::mem::take(&mut cur),
                    end_offset: pos + h.total_len(),
                    pages_through_end: pages,
                });
                if out.len() == want {
                    break;
                }
            }
        }
        pos += h.total_len();
    }
    Ok(out)
}

/// Given the full bytes of one page, return just its header bytes (length
/// `header_len`) with the sequence number set to `new_seq` and the CRC recomputed
/// over the patched page. The payload is read (to recompute the CRC) but not
/// returned — callers splice it verbatim from the backing file.
pub fn patch_page_header(page: &[u8], new_seq: u32) -> Result<Vec<u8>> {
    let h = parse_page(page, 0)?;
    if page.len() < h.total_len() {
        return Err(FormatError::Malformed);
    }
    let mut full = page[..h.total_len()].to_vec();
    full[18..22].copy_from_slice(&new_seq.to_le_bytes());
    full[22..26].copy_from_slice(&0u32.to_le_bytes());
    let crc = crc32(&full);
    full[22..26].copy_from_slice(&crc.to_le_bytes());
    full.truncate(h.header_len);
    Ok(full)
}

use crate::layout::Segment;

/// One span of a packet's payload during chunk-aware lacing.
pub(crate) enum PayloadChunk {
    /// Literal bytes copied verbatim into the layout as `Inline`.
    Bytes(Vec<u8>),
    /// An art run. `out` holds the run's full OUTPUT bytes (base64(image) when
    /// `base64`, else raw image) — used here only to compute page CRCs and lengths,
    /// then dropped; the layout stores an `OggArtSlice` referencing `art_id` so the
    /// bytes are re-derived at read time. `art_total` is the raw image length.
    Art {
        art_id: i64,
        out: Vec<u8>,
        base64: bool,
        art_total: u64,
    },
}

impl PayloadChunk {
    fn out_len(&self) -> usize {
        match self {
            PayloadChunk::Bytes(b) => b.len(),
            PayloadChunk::Art { out, .. } => out.len(),
        }
    }
}

/// Lace one packet (described as a chunk list) into pages starting at sequence
/// `seq_start`, emitting layout segments: page headers + literal payload as
/// `Inline` (CRCs baked in), and art runs as `OggArtSlice` (no bytes stored). The
/// art `out` bytes are materialized only to compute page CRCs, then dropped.
/// Returns `(segments, pages_used)`.
pub(crate) fn lace_chunks_to_segments(
    serial: u32,
    seq_start: u32,
    bos: bool,
    chunks: &[PayloadChunk],
) -> (Vec<Segment>, u32) {
    let total: usize = chunks.iter().map(|c| c.out_len()).sum();
    let laces = lacing_values(total);

    let mut segments: Vec<Segment> = Vec::new();
    let mut seq = seq_start;
    let mut lace_pos = 0usize;
    let mut payload_pos = 0usize; // absolute position within the packet payload
    let mut first = true;

    while first || lace_pos < laces.len() {
        let seg_count = (laces.len() - lace_pos).min(255);
        let table = &laces[lace_pos..lace_pos + seg_count];
        let page_payload: usize = table.iter().map(|&b| b as usize).sum();

        let mut header_type = 0u8;
        if bos && first {
            header_type |= FLAG_BOS;
        }
        if !first {
            header_type |= FLAG_CONTINUED;
        }

        // Assemble full page bytes (with art materialized) to compute the CRC.
        let mut page = Vec::with_capacity(27 + seg_count + page_payload);
        page.extend_from_slice(CAPTURE);
        page.push(0);
        page.push(header_type);
        page.extend_from_slice(&0u64.to_le_bytes()); // granule 0 (header page)
        page.extend_from_slice(&serial.to_le_bytes());
        page.extend_from_slice(&seq.to_le_bytes());
        page.extend_from_slice(&0u32.to_le_bytes()); // CRC placeholder
        page.push(seg_count as u8);
        page.extend_from_slice(table);
        copy_payload(&mut page, chunks, payload_pos, page_payload);
        let crc = crc32(&page);
        page[22..26].copy_from_slice(&crc.to_le_bytes());

        let header_len = 27 + seg_count;
        emit_segments(
            &mut segments,
            &page[..header_len],
            chunks,
            payload_pos,
            page_payload,
        );

        payload_pos += page_payload;
        lace_pos += seg_count;
        seq += 1;
        first = false;
    }
    (segments, seq - seq_start)
}

/// Append payload bytes `[p0, p0+plen)` (in packet-payload coordinates) into `dst`
/// by copying from the chunk list (materializing art `out`).
fn copy_payload(dst: &mut Vec<u8>, chunks: &[PayloadChunk], p0: usize, plen: usize) {
    let end = p0 + plen;
    let mut cs = 0usize;
    for c in chunks {
        let ce = cs + c.out_len();
        let os = p0.max(cs);
        let oe = end.min(ce);
        if os < oe {
            let bytes: &[u8] = match c {
                PayloadChunk::Bytes(b) => b,
                PayloadChunk::Art { out, .. } => out,
            };
            dst.extend_from_slice(&bytes[os - cs..oe - cs]);
        }
        cs = ce;
    }
}

/// Emit the page header + payload `[p0, p0+plen)` as layout segments: `Inline` for
/// the header and literal byte spans, `OggArtSlice` for art spans.
fn emit_segments(
    segments: &mut Vec<Segment>,
    header: &[u8],
    chunks: &[PayloadChunk],
    p0: usize,
    plen: usize,
) {
    let end = p0 + plen;
    let mut buf: Vec<u8> = header.to_vec();
    let mut cs = 0usize;
    for c in chunks {
        let ce = cs + c.out_len();
        let os = p0.max(cs);
        let oe = end.min(ce);
        if os < oe {
            match c {
                PayloadChunk::Bytes(b) => buf.extend_from_slice(&b[os - cs..oe - cs]),
                PayloadChunk::Art {
                    art_id,
                    base64,
                    art_total,
                    ..
                } => {
                    if !buf.is_empty() {
                        segments.push(Segment::Inline(std::mem::take(&mut buf)));
                    }
                    segments.push(Segment::OggArtSlice {
                        art_id: *art_id,
                        offset: (os - cs) as u64,
                        len: (oe - os) as u64,
                        base64: *base64,
                        art_total: *art_total,
                    });
                }
            }
        }
        cs = ce;
    }
    if !buf.is_empty() {
        segments.push(Segment::Inline(buf));
    }
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
        p.extend(std::iter::repeat_n(0xAB, 0x30));
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
    fn rejects_nonzero_version() {
        let mut p = hand_page();
        p[4] = 1; // bad version
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

    #[test]
    fn read_packets_reassembles_multipage_packet() {
        // One small packet, then one packet that spans two pages.
        let small = vec![7u8; 5];
        let big = vec![9u8; 70_000];
        let (mut bytes, _) = lace_packet(3, 0, true, 0, &small);
        let (b2, _) = lace_packet(3, 1, false, 0, &big);
        bytes.extend_from_slice(&b2);

        let pkts = read_packets(&bytes, 2).unwrap();
        assert_eq!(pkts.len(), 2);
        assert_eq!(pkts[0].data, small);
        assert_eq!(pkts[1].data, big);
        assert_eq!(pkts[1].pages_through_end, 3);
        assert_eq!(pkts[1].end_offset, bytes.len());
    }

    #[test]
    fn patch_page_header_updates_seq_and_crc() {
        let packet = vec![0x42u8; 300];
        let (page, _) = lace_packet(0xCAFE, 10, false, 7, &packet);
        let patched = patch_page_header(&page, 12).unwrap();
        let h0 = parse_page(&page, 0).unwrap();
        assert_eq!(patched.len(), h0.header_len);
        // Reassemble a full page from the patched header + original payload and
        // verify the parsed seq and a self-consistent CRC.
        let mut full = patched.clone();
        full.extend_from_slice(&page[h0.header_len..h0.total_len()]);
        let h1 = parse_page(&full, 0).unwrap();
        assert_eq!(h1.seq, 12);
        let mut z = full.clone();
        z[22..26].copy_from_slice(&0u32.to_le_bytes());
        assert_eq!(crc32(&z), h1.crc);
    }

    use crate::layout::Segment;

    // Reconstruct the laced byte stream from segments, expanding OggArtSlice from a
    // provided art output map, so we can validate framing/CRCs end to end.
    fn flatten(segments: &[Segment], art_out: &[u8]) -> Vec<u8> {
        let mut v = Vec::new();
        for s in segments {
            match s {
                Segment::Inline(b) => v.extend_from_slice(b),
                Segment::OggArtSlice {
                    offset,
                    len,
                    base64,
                    ..
                } => {
                    assert!(*base64);
                    v.extend_from_slice(&art_out[*offset as usize..(*offset + *len) as usize]);
                }
                other => panic!("unexpected segment {other:?}"),
            }
        }
        v
    }

    #[test]
    fn chunk_lacer_splits_art_across_pages_and_crcs_validate() {
        // A packet: 50 literal bytes, then a 70_000-byte art run (spans pages), then
        // 10 trailing literal bytes.
        let head = vec![0xA0u8; 50];
        let art_out: Vec<u8> = (0..70_000u32).map(|i| (i % 251) as u8).collect();
        let tail = vec![0xB0u8; 10];
        let chunks = vec![
            PayloadChunk::Bytes(head.clone()),
            PayloadChunk::Art {
                art_id: 42,
                out: art_out.clone(),
                base64: true,
                art_total: 12345,
            },
            PayloadChunk::Bytes(tail.clone()),
        ];
        let (segments, pages) = lace_chunks_to_segments(0x1234, 0, true, &chunks);
        assert!(pages >= 2, "art run should span multiple pages");

        // Reassemble the packet payload and confirm it equals head ++ art ++ tail.
        let flat = flatten(&segments, &art_out);
        // Walk pages: validate CRC + collect payloads.
        let mut pos = 0usize;
        let mut payload = Vec::new();
        let mut seq_expected = 0u32;
        while pos < flat.len() {
            let h = parse_page(&flat, pos).unwrap();
            assert_eq!(h.seq, seq_expected);
            seq_expected += 1;
            // CRC self-check.
            let mut z = flat[pos..pos + h.total_len()].to_vec();
            z[22..26].copy_from_slice(&0u32.to_le_bytes());
            assert_eq!(crc32(&z), h.crc);
            payload.extend_from_slice(&flat[pos + h.header_len..pos + h.total_len()]);
            pos += h.total_len();
        }
        let mut expected = head.clone();
        expected.extend_from_slice(&art_out);
        expected.extend_from_slice(&tail);
        assert_eq!(payload, expected);

        // The art bytes must be carried by OggArtSlice segments (not Inline).
        let art_served: u64 = segments
            .iter()
            .filter_map(|s| match s {
                Segment::OggArtSlice { len, .. } => Some(*len),
                _ => None,
            })
            .sum();
        assert_eq!(art_served, art_out.len() as u64);
    }
}
