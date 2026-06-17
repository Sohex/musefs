use super::art_source::ArtSource;
use super::crc::{crc_shift_zeros, crc32, crc32_update};
use crate::error::{FormatError, Result};

pub const CAPTURE: &[u8; 4] = b"OggS";

/// Header-type flag bits.
pub const FLAG_CONTINUED: u8 = 0x01;
pub const FLAG_BOS: u8 = 0x02;
#[allow(dead_code)]
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
    v.push(u8::try_from(payload_len % 255).expect("x % 255 < 256"));
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
        out.push(u8::try_from(chunk).expect("chunk is .min(255) so fits in u8"));
        out.extend_from_slice(table);
        out.extend_from_slice(&packet[payload_pos..payload_pos + page_payload]);

        let crc = crc32(&out[page_start..]);
        out[page_start + 22..page_start + 26].copy_from_slice(&crc.to_le_bytes());

        lace_pos += chunk;
        payload_pos += page_payload;
        seq = seq.wrapping_add(1);
        first = false;
    }
    (out, seq.wrapping_sub(seq_start))
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

/// Patch a page header algebraically — no payload read needed.
///
/// `header` must be exactly `27 + seg_count` bytes (the fixed Ogg page header
/// plus segment table; seg_count is read from byte 26). Returns the patched
/// header bytes with `new_seq` written and the CRC updated via:
///
///   new_crc = old_crc XOR crc32(DELTA)
///
/// where DELTA is the all-zero message of length page_len, except bytes 18–21
/// hold `old_seq XOR new_seq`. The payload cancels out of the XOR because the
/// Ogg CRC is linear (init=0, no xorout). `payload_len` is derived from the
/// segment table (no payload I/O required).
pub fn patch_page_header_algebraic(header: &[u8], new_seq: u32) -> Result<Vec<u8>> {
    if header.len() < 27 {
        return Err(FormatError::Malformed);
    }
    let seg_count = header[26] as usize;
    let header_len = 27 + seg_count;
    if header.len() < header_len {
        return Err(FormatError::Malformed);
    }
    let payload_len: usize = header[27..header_len].iter().map(|&b| b as usize).sum();
    let old_seq = u32::from_le_bytes(header[18..22].try_into().unwrap());
    let old_crc = u32::from_le_bytes(header[22..26].try_into().unwrap());
    // 18 leading zeros leave the CRC state at 0 (TABLE[0]=0), so we start
    // directly from the 4-byte seq delta, then shift by the trailing zero count.
    let delta_bytes = (old_seq ^ new_seq).to_le_bytes();
    let trailing = 5 + seg_count + payload_len; // bytes 22..page_end are zero in DELTA
    let delta_crc = crc_shift_zeros(crc32(&delta_bytes), trailing);
    let new_crc = old_crc ^ delta_crc;
    let mut out = header[..header_len].to_vec();
    out[18..22].copy_from_slice(&new_seq.to_le_bytes());
    out[22..26].copy_from_slice(&new_crc.to_le_bytes());
    Ok(out)
}

/// Verify that the page at the start of `page` carries a stored CRC matching a
/// fresh computation. `page` must hold at least the full page (`total_len()`
/// bytes). Used by the backward-scan entry-page guard to reject a coincidental
/// `OggS` match in audio payload (a false page start fails this check).
pub fn verify_page_crc(page: &[u8]) -> Result<bool> {
    let h = parse_page(page, 0)?;
    if page.len() < h.total_len() {
        return Err(FormatError::Malformed);
    }
    let mut buf = page[..h.total_len()].to_vec();
    buf[22..26].copy_from_slice(&0u32.to_le_bytes());
    Ok(crc32(&buf) == h.crc)
}

use crate::layout::Segment;

/// One span of a packet's payload during chunk-aware lacing.
pub(crate) enum PayloadChunk {
    /// Literal bytes copied verbatim into the layout as `Inline`.
    Bytes(Vec<u8>),
    /// An art run. Carries no bytes: its OUTPUT length is derived from `art_total`
    /// (base64-expanded when `base64`), and its bytes are streamed from an
    /// `ArtSource` to compute page CRCs, then never stored — the layout keeps an
    /// `OggArtSlice` referencing `art_id`. `art_total` is the raw image length.
    Art {
        art_id: i64,
        base64: bool,
        art_total: u64,
    },
}

impl PayloadChunk {
    fn out_len(&self) -> usize {
        match self {
            PayloadChunk::Bytes(b) => b.len(),
            PayloadChunk::Art {
                base64, art_total, ..
            } => {
                let n = if *base64 {
                    crate::ogg::b64::b64_len(*art_total)
                } else {
                    *art_total
                };
                crate::convert::usize_from(n)
            }
        }
    }
}

/// Lace one packet (a chunk list) into pages from sequence `seq_start`, emitting
/// layout segments: page headers + literal payload as `Inline` (CRCs baked in),
/// art runs as `OggArtSlice` (no bytes stored). Art bytes are streamed from `src`
/// only to compute page CRCs. Returns `(segments, pages_used)`.
pub(crate) fn lace_chunks_to_segments(
    serial: u32,
    seq_start: u32,
    bos: bool,
    chunks: &[PayloadChunk],
    src: &dyn ArtSource,
) -> crate::error::Result<(Vec<Segment>, u32)> {
    let total: usize = chunks.iter().map(PayloadChunk::out_len).sum();
    let laces = lacing_values(total);

    let mut segments: Vec<Segment> = Vec::new();
    let mut seq = seq_start;
    let mut lace_pos = 0usize;
    let mut payload_pos = 0usize;
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

        // Build the page header + lacing table only (CRC field zeroed), then stream
        // the page CRC over header+payload without materializing the payload.
        let header_len = 27 + seg_count;
        let mut header = Vec::with_capacity(header_len);
        header.extend_from_slice(CAPTURE);
        header.push(0);
        header.push(header_type);
        header.extend_from_slice(&0u64.to_le_bytes()); // granule 0 (header page)
        header.extend_from_slice(&serial.to_le_bytes());
        header.extend_from_slice(&seq.to_le_bytes());
        header.extend_from_slice(&0u32.to_le_bytes()); // CRC placeholder
        header.push(u8::try_from(seg_count).expect("seg_count is .min(255) so fits in u8"));
        header.extend_from_slice(table);

        let mut crc = crc32_update(0, &header);
        crc_feed_payload(&mut crc, chunks, src, payload_pos, page_payload)?;
        header[22..26].copy_from_slice(&crc.to_le_bytes());

        emit_segments(&mut segments, &header, chunks, payload_pos, page_payload);

        payload_pos += page_payload;
        lace_pos += seg_count;
        seq += 1;
        first = false;
    }
    Ok((segments, seq - seq_start))
}

/// Fold payload bytes `[p0, p0+plen)` (packet-payload coordinates) into `crc`,
/// reading art runs from `src` instead of materializing them.
fn crc_feed_payload(
    crc: &mut u32,
    chunks: &[PayloadChunk],
    src: &dyn ArtSource,
    p0: usize,
    plen: usize,
) -> crate::error::Result<()> {
    let end = p0 + plen;
    let mut cs = 0usize;
    for c in chunks {
        let ce = cs + c.out_len();
        let os = p0.max(cs);
        let oe = end.min(ce);
        if os < oe {
            match c {
                PayloadChunk::Bytes(b) => {
                    *crc = crc32_update(*crc, &b[os - cs..oe - cs]);
                }
                PayloadChunk::Art {
                    art_id,
                    base64,
                    art_total,
                } => {
                    crc_feed_art(
                        crc,
                        src,
                        *art_id,
                        *base64,
                        *art_total,
                        (os - cs) as u64,
                        oe - os,
                    )?;
                }
            }
        }
        cs = ce;
    }
    Ok(())
}

/// Fold one art window — output bytes `[out_off, out_off+out_len)` of the run —
/// into `crc`, base64-encoding on the fly when `base64`. `out_len` is page-bounded.
fn crc_feed_art(
    crc: &mut u32,
    src: &dyn ArtSource,
    art_id: i64,
    base64: bool,
    art_total: u64,
    out_off: u64,
    out_len: usize,
) -> crate::error::Result<()> {
    if base64 {
        let w = crate::ogg::b64::b64_window(out_off, out_len as u64, art_total);
        let mut raw = vec![0u8; crate::convert::usize_from(w.in_len)];
        src.read_window(art_id, w.in_start, &mut raw)?;
        let enc = crate::ogg::b64::encode_b64_slice(&raw, w.skip, out_len)
            .ok_or(crate::error::FormatError::ArtRead { art_id })?;
        *crc = crc32_update(*crc, &enc);
    } else {
        let mut raw = vec![0u8; out_len];
        src.read_window(art_id, out_off, &mut raw)?;
        *crc = crc32_update(*crc, &raw);
    }
    Ok(())
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
                } => {
                    if !buf.is_empty() {
                        segments.push(Segment::Inline(std::mem::take(&mut buf)));
                    }
                    segments.push(Segment::OggArtSlice {
                        art_id: *art_id,
                        offset: (os - cs) as u64,
                        len: crate::BlobLen::new((oe - os) as u64)
                            .expect("ogg art slice span is non-empty"),
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
    fn multipage_payload_cursor_advances_across_pages() {
        // A >65 025-byte packet with DISTINCT bytes at every position: the second
        // page must carry the *tail* of the packet, not a re-read from offset 0.
        // Pins `payload_pos += page_payload` against a cursor stuck at 0 (a uniform
        // payload, as in the other multi-page tests, can't observe this).
        let packet: Vec<u8> = (0..70_000u32).map(|i| (i % 251) as u8).collect();
        let (bytes, pages) = lace_packet(2, 0, false, 0, &packet);
        assert_eq!(pages, 2);
        let pkts = read_packets(&bytes, 1).unwrap();
        assert_eq!(pkts[0].data, packet);
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
                    v.extend_from_slice(
                        &art_out[usize::try_from(*offset).unwrap()
                            ..usize::try_from(*offset + len.get()).unwrap()],
                    );
                }
                other => panic!("unexpected segment {other:?}"),
            }
        }
        v
    }

    #[test]
    fn chunk_lacer_splits_art_across_pages_and_crcs_validate() {
        use super::super::art_source::MapArtSource;
        // A packet: 50 literal bytes, then a 60_000-byte art run (spans pages), then
        // 10 trailing literal bytes.
        let head = vec![0xA0u8; 50];
        // 60_000 raw bytes -> b64 output ~80_000 > one page (65025), so it spans pages.
        let image: Vec<u8> = (0..60_000u32).map(|i| (i % 251) as u8).collect();
        let art_out = crate::ogg::b64::encode_b64_slice(
            &image,
            0,
            crate::convert::usize_from(crate::ogg::b64::b64_len(image.len() as u64)),
        )
        .expect("full-length window lies within the encoded output");
        let tail = vec![0xB0u8; 10];
        let chunks = vec![
            PayloadChunk::Bytes(head.clone()),
            PayloadChunk::Art {
                art_id: 42,
                base64: true,
                art_total: image.len() as u64,
            },
            PayloadChunk::Bytes(tail.clone()),
        ];
        let src = MapArtSource::new([(42i64, image.clone())]);
        let (segments, pages) = lace_chunks_to_segments(0x1234, 0, true, &chunks, &src).unwrap();
        assert!(pages >= 2, "art run should span multiple pages");

        // Reassemble the packet payload and confirm it equals head ++ art_out ++ tail.
        let flat = flatten(&segments, &art_out);
        // Walk pages: validate CRC + collect payloads.
        let mut pos = 0usize;
        let mut payload = Vec::new();
        let mut seq_expected = 0u32;
        while pos < flat.len() {
            let h = parse_page(&flat, pos).unwrap();
            // Flag-bit kills: BOS only on the first page, FLAG_CONTINUED on every
            // later page (kills :263 |=->&= on BOS, :266 on CONTINUED, :265 delete !).
            let is_first = seq_expected == 0;
            if is_first {
                assert_eq!(h.header_type & FLAG_BOS, FLAG_BOS);
                assert_eq!(h.header_type & FLAG_CONTINUED, 0);
            } else {
                assert_eq!(h.header_type & FLAG_BOS, 0);
                assert_eq!(h.header_type & FLAG_CONTINUED, FLAG_CONTINUED);
            }
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
                Segment::OggArtSlice { len, .. } => Some(len.get()),
                _ => None,
            })
            .sum();
        assert_eq!(art_served, art_out.len() as u64);
    }

    #[test]
    fn eos_bit_is_preserved_through_renumber() {
        // Build a one-page packet, set its EOS bit, repatch the CRC, then renumber
        // via patch_page_header and confirm header_type (incl. EOS) is unchanged.
        let (mut page, _) = lace_packet(0xEE, 3, false, 9, &[0x11u8; 120]);
        page[5] |= FLAG_EOS; // header_type byte
        // Recompute the CRC over the EOS-modified page (CRC field zeroed first).
        let mut z = page.clone();
        z[22..26].copy_from_slice(&0u32.to_le_bytes());
        let crc = crc32(&z);
        page[22..26].copy_from_slice(&crc.to_le_bytes());

        let patched = patch_page_header(&page, 99).unwrap();
        let h0 = parse_page(&page, 0).unwrap();
        let mut full = patched.clone();
        full.extend_from_slice(&page[h0.header_len..h0.total_len()]);
        let h1 = parse_page(&full, 0).unwrap();
        assert_eq!(h1.seq, 99);
        assert_eq!(h1.header_type & FLAG_EOS, FLAG_EOS, "EOS bit dropped");
        assert_eq!(h1.header_type, h0.header_type, "header_type changed");
    }

    #[test]
    fn parse_page_rejects_truncated_header_and_table() {
        // Truncated 27-byte header (kills :33 `> -> ==`/`>=`).
        let p = hand_page();
        assert_eq!(parse_page(&p[..26], 0), Err(FormatError::Malformed));
        assert!(parse_page(&p[..27], 0).is_err()); // header present but table missing
        // Header present, segment table truncated (kills :47).
        assert_eq!(parse_page(&p[..28], 0), Err(FormatError::Malformed));
        // Exactly full header+table+payload parses.
        assert!(parse_page(&p, 0).is_ok());
    }

    #[test]
    fn parse_page_accepts_27_byte_zero_segment_page() {
        // A zero-segment page is exactly the 27-byte fixed header (no lacing table,
        // no payload). `pos + 27 == buf.len()` is the valid boundary; `>=` would
        // wrongly reject a page that fills the buffer exactly.
        let mut page = vec![0u8; 27];
        page[0..4].copy_from_slice(CAPTURE); // "OggS"
        page[26] = 0; // seg_count = 0
        let h = parse_page(&page, 0).unwrap();
        assert_eq!(h.seg_count, 0);
        assert_eq!(h.total_len(), 27);
    }

    #[test]
    fn patch_page_header_rejects_truncated_page() {
        let (page, _) = lace_packet(0xCAFE, 1, false, 0, &vec![0x42u8; 300]);
        let h = parse_page(&page, 0).unwrap();
        // Hand a buffer shorter than total_len: original returns Err; the `>` mutant
        // would proceed and panic slicing page[..total_len].
        assert_eq!(
            patch_page_header(&page[..h.total_len() - 10], 2),
            Err(FormatError::Malformed)
        );
    }

    #[test]
    fn read_packets_stops_exactly_at_want_within_a_page() {
        // One page carrying two complete packets (two lacing values < 255).
        // want=1 must return after the first, ignoring the second on the same page.
        let mut page = Vec::new();
        page.extend_from_slice(CAPTURE);
        page.push(0);
        page.push(FLAG_BOS);
        page.extend_from_slice(&0u64.to_le_bytes());
        page.extend_from_slice(&7u32.to_le_bytes());
        page.extend_from_slice(&0u32.to_le_bytes());
        page.extend_from_slice(&0u32.to_le_bytes()); // crc placeholder
        page.push(2); // 2 segments
        page.push(3); // packet A: 3 bytes
        page.push(4); // packet B: 4 bytes
        page.extend_from_slice(&[1, 2, 3, 9, 9, 9, 9]);
        let mut z = page.clone();
        z[22..26].copy_from_slice(&0u32.to_le_bytes());
        let crc = crc32(&z);
        page[22..26].copy_from_slice(&crc.to_le_bytes());

        let pkts = read_packets(&page, 1).unwrap();
        assert_eq!(pkts.len(), 1);
        assert_eq!(pkts[0].data, vec![1, 2, 3]);
    }

    #[test]
    fn patch_algebraic_matches_full_page() {
        // For each combination of payload size and seq values, the algebraic
        // patch must produce the same header bytes as the full-page oracle.
        for &payload_len in &[0usize, 1, 255, 3000, 65025] {
            for &old_seq in &[0u32, 1, 42, u32::MAX - 5] {
                for &new_seq in &[old_seq, old_seq.wrapping_add(1), old_seq.wrapping_add(10)] {
                    let payload = vec![0xA5u8; payload_len];
                    let (page_bytes, _) = lace_packet(0x1234, old_seq, false, 0, &payload);
                    // Full-page oracle (existing function).
                    let want = patch_page_header(&page_bytes, new_seq).unwrap();
                    // Header-only algebraic version.
                    let h = parse_page(&page_bytes, 0).unwrap();
                    let got =
                        patch_page_header_algebraic(&page_bytes[..h.header_len], new_seq).unwrap();
                    assert_eq!(
                        got, want,
                        "payload_len={payload_len} old_seq={old_seq} new_seq={new_seq}"
                    );
                }
            }
        }
    }

    #[test]
    fn verify_page_crc_accepts_valid_rejects_tampered() {
        // A freshly laced page has a correct CRC.
        let (page, _) = lace_packet(0x55, 9, false, 42, &vec![0x7Eu8; 500]);
        assert!(
            verify_page_crc(&page).unwrap(),
            "valid page must verify true"
        );
        // Flip one payload byte → CRC no longer matches.
        let mut tampered = page.clone();
        let h = parse_page(&page, 0).unwrap();
        tampered[h.header_len] ^= 0xFF; // first payload byte
        assert!(
            !verify_page_crc(&tampered).unwrap(),
            "tampered payload must verify false"
        );
        // Corrupt the stored CRC field directly → also false.
        let mut bad_crc = page.clone();
        bad_crc[22] ^= 0x01;
        assert!(
            !verify_page_crc(&bad_crc).unwrap(),
            "corrupt stored CRC must verify false"
        );
    }

    #[test]
    fn patch_algebraic_accepts_zero_segment_header() {
        // A valid 0-segment page header is exactly 27 bytes (header_len == 27), so it
        // exercises the `header.len() < 27` guard at the boundary: it must be
        // accepted. `<`->`==`/`<=` would reject this valid header.
        let mut hdr = vec![0u8; 27];
        hdr[..4].copy_from_slice(b"OggS");
        hdr[18..22].copy_from_slice(&7u32.to_le_bytes()); // old_seq
        // byte 26 (seg_count) == 0 → header_len 27, payload_len 0.
        let out = patch_page_header_algebraic(&hdr, 9).unwrap();
        assert_eq!(out.len(), 27);
        assert_eq!(u32::from_le_bytes(out[18..22].try_into().unwrap()), 9);
    }

    #[test]
    fn patch_algebraic_rejects_truncated_segment_table() {
        // A 27-byte header whose seg_count byte claims 5 segments → header_len 32 >
        // 27 bytes provided. The `header.len() < header_len` guard must reject it;
        // `<`->`>` would proceed and read past the slice.
        let mut hdr = vec![0u8; 27];
        hdr[..4].copy_from_slice(b"OggS");
        hdr[26] = 5; // seg_count 5 → header_len 32
        assert!(patch_page_header_algebraic(&hdr, 1).is_err());
    }

    #[test]
    fn verify_page_crc_rejects_truncated_page() {
        // A page buffer shorter than its declared total_len. The `page.len() <
        // total_len` guard must reject it; `<`->`>` would slice past the buffer.
        let (page, _) = lace_packet(0x55, 1, false, 0, &vec![0u8; 300]);
        let h = parse_page(&page, 0).unwrap();
        let truncated = &page[..h.total_len() - 10];
        assert!(verify_page_crc(truncated).is_err());
    }
}
