//! Lazy, cached per-file index for serving `Segment::OggAudio`: a single buffered
//! sequential pass over the backing file's audio region that renumbers each page's
//! sequence number and recomputes its CRC, recording only `{offset, header,
//! payload_len}` per page — payloads are never retained and are served from the
//! backing file.

use std::io::{BufReader, Read, Seek, SeekFrom};
use std::path::Path;

use musefs_format::ogg::parse_page;
use musefs_format::ogg::{patch_page_header_algebraic, verify_page_crc};

use crate::error::{CoreError, Result};

/// One renumbered audio page: its offset within the audio region, the patched
/// header bytes, and the payload length (served from the backing file).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexedPage {
    pub region_offset: u64,
    pub header: Vec<u8>,
    pub payload_len: u64,
}

/// The full renumbered-audio index for one file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OggPageIndex {
    pub pages: Vec<IndexedPage>,
}

/// Build the index by reading `[audio_offset, audio_offset + audio_length)` from
/// `path` sequentially. Each original page's sequence number is shifted by
/// `seq_delta` and its CRC recomputed (via `patch_page_header`).
pub fn build_index(
    path: &Path,
    audio_offset: u64,
    audio_length: u64,
    seq_delta: i64,
) -> Result<OggPageIndex> {
    let file = std::fs::File::open(path)?;
    let mut reader = BufReader::with_capacity(256 * 1024, file);
    reader.seek(SeekFrom::Start(audio_offset))?;

    let mut pages = Vec::new();
    let mut consumed = 0u64;
    let mut hdr = [0u8; 27];
    while consumed < audio_length {
        reader.read_exact(&mut hdr)?;
        let seg_count = hdr[26] as usize;
        let mut table = vec![0u8; seg_count];
        reader.read_exact(&mut table)?;
        let payload_len: usize = table.iter().map(|&b| b as usize).sum();

        // Reassemble the full page bytes to renumber + CRC.
        let mut full = Vec::with_capacity(27 + seg_count + payload_len);
        full.extend_from_slice(&hdr);
        full.extend_from_slice(&table);
        let mut payload = vec![0u8; payload_len];
        reader.read_exact(&mut payload)?;
        full.extend_from_slice(&payload);

        let old = parse_page(&full, 0).map_err(CoreError::from)?;
        let new_seq = (old.seq as i64 + seq_delta) as u32;
        let header =
            musefs_format::ogg::patch_page_header(&full, new_seq).map_err(CoreError::from)?;

        pages.push(IndexedPage {
            region_offset: consumed,
            header,
            payload_len: payload_len as u64,
        });
        consumed += old.total_len() as u64;
    }
    if consumed != audio_length {
        return Err(CoreError::from(musefs_format::FormatError::Malformed));
    }
    Ok(OggPageIndex { pages })
}

use std::os::unix::fs::FileExt;

/// Serve `[rstart, rend)` (relative to the start of the audio region) into `out`,
/// splicing patched page headers with verbatim payload bytes read from the backing
/// file at `audio_offset + region payload position`.
pub fn serve(
    index: &OggPageIndex,
    backing: &std::fs::File,
    audio_offset: u64,
    rstart: u64,
    rend: u64,
    out: &mut Vec<u8>,
) -> Result<()> {
    for p in &index.pages {
        let hlen = p.header.len() as u64;
        let page_start = p.region_offset;
        let header_end = page_start + hlen;
        let payload_end = header_end + p.payload_len;
        if payload_end <= rstart {
            continue;
        }
        if page_start >= rend {
            break;
        }
        // Header overlap.
        let hs = rstart.max(page_start);
        let he = rend.min(header_end);
        if hs < he {
            let a = (hs - page_start) as usize;
            let b = (he - page_start) as usize;
            out.extend_from_slice(&p.header[a..b]);
        }
        // Payload overlap (served from the backing file).
        let ps = rstart.max(header_end);
        let pe = rend.min(payload_end);
        if ps < pe {
            let within = ps - header_end;
            let n = (pe - ps) as usize;
            let mut buf = vec![0u8; n];
            backing.read_exact_at(&mut buf, audio_offset + p.region_offset + hlen + within)?;
            out.extend_from_slice(&buf);
        }
    }
    Ok(())
}

/// Maximum Ogg page size in bytes: 27 fixed header + 255 seg-table + 255×255 payload.
const MAX_OGG_PAGE_BYTES: u64 = 65_307;
/// Maximum Ogg page header size: 27 fixed + 255 seg-table.
const MAX_OGG_HEADER_BYTES: usize = 282;

/// Find the absolute file offset of the Ogg page whose region contains or
/// immediately precedes `abs_target` within `[audio_offset, audio_offset + ?)`.
///
/// Special case: `abs_target == audio_offset` returns `audio_offset` immediately
/// (the first audio page always starts there, validated at scan time).
///
/// General case: reads the window `[max(audio_offset, abs_target−65307), abs_target)`
/// in one `pread` and scans backwards for the rightmost OggS page start. Each
/// candidate that passes the cheap header checks (version byte 0,
/// `header_type & 0xF8 == 0`, segment table fits) is then CRC-validated by
/// `page_crc_ok`; a coincidental `OggS` in audio payload fails the CRC check, so
/// the scan rejects it and continues. This makes page location deterministic —
/// the byte-identical guarantee holds unconditionally, not just probabilistically.
fn find_page_start(
    backing: &std::fs::File,
    audio_offset: u64,
    abs_target: u64,
) -> Result<u64> {
    if abs_target == audio_offset {
        return Ok(audio_offset);
    }
    let scan_start = abs_target
        .saturating_sub(MAX_OGG_PAGE_BYTES)
        .max(audio_offset);
    let window_len = (abs_target - scan_start) as usize;
    let mut window = vec![0u8; window_len];
    backing.read_exact_at(&mut window, scan_start)?;

    // Scan backwards for the rightmost CRC-valid OggS capture.
    let mut i = window_len.saturating_sub(4);
    loop {
        if window[i..].starts_with(b"OggS") {
            // Cheap pre-filter on whatever header bytes fall inside the window.
            // The window ends at `abs_target`, so a true page start near the target
            // may have its version/header_type/segment-table bytes BEYOND the
            // window — those must never reject the candidate (a missing byte passes).
            // `page_crc_ok` reads the full page from the file and is authoritative.
            let cheap_ok = window.get(i + 4).is_none_or(|&v| v == 0)       // version == 0
                && window.get(i + 5).is_none_or(|&ht| ht & 0xF8 == 0);     // header_type
            if cheap_ok {
                let candidate = scan_start + i as u64;
                if page_crc_ok(backing, candidate)? {
                    return Ok(candidate);
                }
            }
        }
        if i == 0 {
            break;
        }
        i -= 1;
    }
    Err(musefs_format::FormatError::Malformed.into())
}

/// Read the full page at `page_start` and verify its stored CRC. Returns
/// `Ok(false)` (not an error) on a CRC mismatch, a too-short header, or EOF, so
/// the backward scan treats the candidate as a false positive and continues.
fn page_crc_ok(backing: &std::fs::File, page_start: u64) -> Result<bool> {
    // Read up to the max header (tolerating a short read at EOF) to learn the
    // page length without a second round trip.
    let mut head = vec![0u8; MAX_OGG_HEADER_BYTES];
    let n = backing.read_at(&mut head, page_start)?;
    head.truncate(n);
    if head.len() < 27 {
        return Ok(false);
    }
    let seg_count = head[26] as usize;
    let header_len = 27 + seg_count;
    if head.len() < header_len {
        return Ok(false);
    }
    let payload_len: usize = head[27..header_len].iter().map(|&b| b as usize).sum();
    let total = header_len + payload_len;
    let mut page = vec![0u8; total];
    if backing.read_exact_at(&mut page, page_start).is_err() {
        return Ok(false); // page runs past EOF → not a real page here
    }
    Ok(verify_page_crc(&page).unwrap_or(false))
}

/// Serve bytes `[rstart, rend)` (relative to the audio region start) into `out`.
///
/// Locates the containing page via a backwards scan, then walks pages forward,
/// patching each header algebraically (`patch_page_header_algebraic`) and serving
/// payload slices via exact positioned reads — no full-page I/O and no in-memory
/// page index.
///
/// Integrity guard: errors (`FormatError::Malformed`) if the page walk overruns
/// `audio_offset + audio_length`, which indicates corrupt or misaligned data.
/// This preserves the `consumed == audio_length` check the removed `build_index`
/// enforced, as a hard error in both debug and release builds.
pub fn serve_ogg_window(
    backing: &std::fs::File,
    audio_offset: u64,
    audio_length: u64,
    seq_delta: i64,
    rstart: u64,
    rend: u64,
    out: &mut Vec<u8>,
) -> Result<()> {
    if rstart >= rend {
        return Ok(());
    }
    let audio_end = audio_offset + audio_length;
    let abs_rstart = audio_offset + rstart;
    let mut pos = find_page_start(backing, audio_offset, abs_rstart)?;

    while pos < audio_end {
        let page_rel = pos - audio_offset;
        if page_rel >= rend {
            break;
        }
        // One pread for the full header (27 + up to 255 seg-table bytes).
        // Clamped to the declared audio region end.
        let read_len = MAX_OGG_HEADER_BYTES.min((audio_end - pos) as usize);
        let mut hdr_buf = vec![0u8; read_len];
        backing.read_exact_at(&mut hdr_buf, pos)?;
        if hdr_buf.len() < 27 {
            return Err(musefs_format::FormatError::Malformed.into());
        }
        let seg_count = hdr_buf[26] as usize;
        let header_len = 27 + seg_count;
        if hdr_buf.len() < header_len {
            return Err(musefs_format::FormatError::Malformed.into());
        }
        let payload_len: usize =
            hdr_buf[27..header_len].iter().map(|&b| b as usize).sum();

        let old_seq = u32::from_le_bytes(hdr_buf[18..22].try_into().unwrap());
        let new_seq = (old_seq as i64 + seq_delta) as u32;
        let patched_hdr =
            patch_page_header_algebraic(&hdr_buf[..header_len], new_seq)
                .map_err(CoreError::from)?;

        let hdr_end = page_rel + header_len as u64;
        let page_end = hdr_end + payload_len as u64;

        // Header overlap.
        let hs = rstart.max(page_rel);
        let he = rend.min(hdr_end);
        if hs < he {
            let a = (hs - page_rel) as usize;
            let b = (he - page_rel) as usize;
            out.extend_from_slice(&patched_hdr[a..b]);
        }

        // Payload overlap — exactly the bytes requested, no full-page read.
        let ps = rstart.max(hdr_end);
        let pe = rend.min(page_end);
        if ps < pe {
            let within = ps - hdr_end;
            let n = (pe - ps) as usize;
            let start = out.len();
            out.resize(start + n, 0);
            backing.read_exact_at(&mut out[start..], pos + header_len as u64 + within)?;
        }

        pos += (header_len + payload_len) as u64;

        // Integrity guard: a page whose declared length pushes past the declared
        // audio region means the file is truncated/misaligned or the DB bounds are
        // stale. Hard error (matches the removed build_index consumed-check).
        if pos > audio_end {
            return Err(musefs_format::FormatError::Malformed.into());
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use musefs_format::ogg::page_test_support::lace_packet_pub;
    use std::io::Write;

    #[test]
    fn build_index_errors_when_audio_length_is_not_on_a_page_boundary() {
        // One 300-byte packet -> one page of total_len T. Passing audio_length = T-5
        // makes the loop read the whole page (consumed = T) then exit with
        // consumed != audio_length.
        let (bytes, _) = lace_packet_pub(0xABCD, 0, false, 0, &vec![7u8; 300]);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.ogg");
        std::fs::File::create(&path)
            .unwrap()
            .write_all(&bytes)
            .unwrap();
        let short = bytes.len() as u64 - 5;
        let err = build_index(&path, 0, short, 0);
        assert!(
            err.is_err(),
            "expected Err on non-page-boundary audio_length"
        );
    }

    #[test]
    fn build_index_renumbers_and_preserves_payload_length() {
        use musefs_format::ogg::{parse_page, PageHeader};
        const FLAG_CONTINUED: u8 = 0x01; // not re-exported from musefs_format::ogg
        let (mut bytes, _) = lace_packet_pub(0xABCD, 5, false, 100, &vec![1u8; 300]);
        let (b2, _) = lace_packet_pub(0xABCD, 6, false, 200, &vec![2u8; 70_000]);
        bytes.extend_from_slice(&b2);

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("audio.ogg");
        let mut file_bytes = vec![0u8; 16];
        file_bytes.extend_from_slice(&bytes);
        std::fs::File::create(&path)
            .unwrap()
            .write_all(&file_bytes)
            .unwrap();

        let idx = build_index(&path, 16, bytes.len() as u64, 2).unwrap();
        assert_eq!(idx.pages.len(), 3); // 1 small page + 2 from the big packet

        // Contiguous region offsets summing to audio_length.
        let mut expected_off = 0u64;
        for p in &idx.pages {
            assert_eq!(p.region_offset, expected_off);
            expected_off += p.header.len() as u64 + p.payload_len;
        }
        assert_eq!(expected_off, bytes.len() as u64);

        // Parse each patched header (append its payload so parse sees a full page),
        // assert seq renumbering, payload_len match, and a self-consistent CRC.
        let mut prev_seq: Option<u32> = None;
        for (i, p) in idx.pages.iter().enumerate() {
            let mut full = p.header.clone();
            full.extend(std::iter::repeat_n(0u8, p.payload_len as usize));
            let h: PageHeader = parse_page(&full, 0).unwrap();
            assert_eq!(h.payload_len as u64, p.payload_len);
            // seqs are old+2 and strictly increasing: page 0 -> 7, pages 1&2 -> 8,9.
            if let Some(prev) = prev_seq {
                assert_eq!(h.seq, prev + 1);
            } else {
                assert_eq!(h.seq, 7);
            }
            prev_seq = Some(h.seq);
            // The continuation page of the big packet carries FLAG_CONTINUED.
            if i == 2 {
                assert_eq!(h.header_type & FLAG_CONTINUED, FLAG_CONTINUED);
            }
        }
    }

    use std::os::unix::fs::FileExt;

    /// A backing file: 16-byte prefix, then a 300-byte packet (seq 5) and a
    /// 70_000-byte packet (seq 6, spans 2 pages). Returns the index built with
    /// seq_delta=+2, an open backing handle, the audio_offset, and the total
    /// served length of the whole audio region.
    fn serve_fixture() -> (tempfile::TempDir, OggPageIndex, std::fs::File, u64, u64) {
        let (mut bytes, _) = lace_packet_pub(0xABCD, 5, false, 100, &vec![1u8; 300]);
        let (b2, _) = lace_packet_pub(0xABCD, 6, false, 200, &vec![2u8; 70_000]);
        bytes.extend_from_slice(&b2);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("audio.ogg");
        let mut file_bytes = vec![0u8; 16];
        file_bytes.extend_from_slice(&bytes);
        std::fs::File::create(&path)
            .unwrap()
            .write_all(&file_bytes)
            .unwrap();
        let audio_offset = 16u64;
        let idx = build_index(&path, audio_offset, bytes.len() as u64, 2).unwrap();
        let backing = std::fs::File::open(&path).unwrap();
        let total: u64 = idx
            .pages
            .iter()
            .map(|p| p.header.len() as u64 + p.payload_len)
            .sum();
        (dir, idx, backing, audio_offset, total)
    }

    /// Independent reference: the full served region is, for every page, its
    /// patched header followed by its payload read verbatim from the backing file.
    fn reference_region(idx: &OggPageIndex, backing: &std::fs::File, audio_offset: u64) -> Vec<u8> {
        let mut out = Vec::new();
        for p in &idx.pages {
            out.extend_from_slice(&p.header);
            let mut buf = vec![0u8; p.payload_len as usize];
            backing
                .read_exact_at(
                    &mut buf,
                    audio_offset + p.region_offset + p.header.len() as u64,
                )
                .unwrap();
            out.extend_from_slice(&buf);
        }
        out
    }

    fn serve_range(
        idx: &OggPageIndex,
        backing: &std::fs::File,
        audio_offset: u64,
        a: u64,
        b: u64,
    ) -> Vec<u8> {
        let mut out = Vec::new();
        serve(idx, backing, audio_offset, a, b, &mut out).unwrap();
        out
    }

    #[test]
    fn serve_whole_region_matches_reference() {
        let (_d, idx, backing, ao, total) = serve_fixture();
        let want = reference_region(&idx, &backing, ao);
        assert_eq!(want.len() as u64, total);
        assert_eq!(serve_range(&idx, &backing, ao, 0, total), want);
    }

    #[test]
    fn serve_header_only_read() {
        let (_d, idx, backing, ao, _t) = serve_fixture();
        let want = reference_region(&idx, &backing, ao);
        let hlen = idx.pages[0].header.len() as u64;
        // First 10 bytes of page 0's header.
        assert_eq!(serve_range(&idx, &backing, ao, 0, 10), want[0..10]);
        // The whole of page 0's header, exactly.
        assert_eq!(
            serve_range(&idx, &backing, ao, 0, hlen),
            want[0..hlen as usize]
        );
    }

    #[test]
    fn serve_payload_only_read_starting_mid_payload() {
        // Kills ogg_index.rs:117 (the + -> - on the backing read offset): the read
        // starts 10 bytes INTO page 0's payload, so `within` = 10 != 0 and the sign
        // of the offset term is observable.
        let (_d, idx, backing, ao, _t) = serve_fixture();
        let want = reference_region(&idx, &backing, ao);
        let hlen = idx.pages[0].header.len() as u64;
        let start = hlen + 10;
        let end = hlen + 60;
        assert_eq!(
            serve_range(&idx, &backing, ao, start, end),
            want[start as usize..end as usize]
        );
    }

    #[test]
    fn serve_spanning_header_and_payload() {
        let (_d, idx, backing, ao, _t) = serve_fixture();
        let want = reference_region(&idx, &backing, ao);
        let hlen = idx.pages[0].header.len() as u64;
        let r = (hlen - 5)..(hlen + 20);
        assert_eq!(
            serve_range(&idx, &backing, ao, r.start, r.end),
            want[r.start as usize..r.end as usize]
        );
    }

    #[test]
    fn serve_crossing_page_boundary() {
        let (_d, idx, backing, ao, _t) = serve_fixture();
        let want = reference_region(&idx, &backing, ao);
        // End of page 0 region into the start of page 1.
        let p0_end = idx.pages[0].header.len() as u64 + idx.pages[0].payload_len;
        let r = (p0_end - 30)..(p0_end + 40);
        assert_eq!(
            serve_range(&idx, &backing, ao, r.start, r.end),
            want[r.start as usize..r.end as usize]
        );
    }

    #[test]
    fn serve_empty_and_past_end_reads() {
        let (_d, idx, backing, ao, total) = serve_fixture();
        // Empty range.
        assert!(serve_range(&idx, &backing, ao, 100, 100).is_empty());
        // Entirely past the last page.
        assert!(serve_range(&idx, &backing, ao, total, total + 50).is_empty());
        // rend past the region end clamps to what exists.
        let want = reference_region(&idx, &backing, ao);
        assert_eq!(
            serve_range(&idx, &backing, ao, total - 25, total + 1000),
            want[(total - 25) as usize..]
        );
    }

    /// CRC-32/Ogg: poly 0x04C11DB7, init 0, no reflection, no xorout. Independent
    /// of musefs-format::ogg::crc (different table, from the `crc` crate).
    const CRC_32_OGG: crc::Algorithm<u32> = crc::Algorithm {
        width: 32,
        poly: 0x04c1_1db7,
        init: 0x0000_0000,
        refin: false,
        refout: false,
        xorout: 0x0000_0000,
        check: 0x0000_0000,
        residue: 0x0000_0000,
    };

    /// Assert `stream` is a clean single Ogg bitstream: the `ogg` crate reassembles
    /// every packet without error (it validates page CRCs), and an independent CRC
    /// (the `crc` crate) matches every page's stored CRC while seq numbers run
    /// 0,1,2,... contiguously.
    fn assert_clean_bitstream(stream: &[u8]) {
        use musefs_format::ogg::parse_page;
        // (a) third-party structural decode (validates CRC during reassembly).
        let mut rdr = ogg::PacketReader::new(std::io::Cursor::new(stream.to_vec()));
        let mut packets = 0usize;
        while rdr.read_packet().expect("ogg decode error").is_some() {
            packets += 1;
        }
        assert!(packets > 0, "no packets decoded");
        // (b) independent per-page CRC + contiguous seq.
        let alg = crc::Crc::<u32>::new(&CRC_32_OGG);
        let mut pos = 0usize;
        let mut expect_seq = 0u32;
        while pos < stream.len() {
            let h = parse_page(stream, pos).unwrap();
            let mut page = stream[pos..pos + h.total_len()].to_vec();
            page[22..26].copy_from_slice(&0u32.to_le_bytes());
            assert_eq!(alg.checksum(&page), h.crc, "page CRC mismatch at {pos}");
            assert_eq!(h.seq, expect_seq, "seq not contiguous at {pos}");
            expect_seq += 1;
            pos += h.total_len();
        }
    }

    /// Materialize the synthesized header region (Inline segments only; these
    /// fixtures embed no art) up to the OggAudio segment, returning
    /// (header_bytes, audio_offset, audio_length, seq_delta).
    fn materialize_header_and_audio_params(
        layout: &musefs_format::RegionLayout,
    ) -> (Vec<u8>, u64, u64, i64) {
        use musefs_format::Segment;
        let mut header = Vec::new();
        let mut params = None;
        for seg in &layout.segments {
            match seg {
                Segment::Inline(b) => header.extend_from_slice(b),
                Segment::OggAudio {
                    offset,
                    len,
                    seq_delta,
                } => {
                    params = Some((*offset, *len, *seq_delta));
                }
                other => panic!("unexpected segment in no-art header: {other:?}"),
            }
        }
        let (offset, len, delta) = params.expect("OggAudio segment present");
        (header, offset, len, delta)
    }

    /// Build a complete synthetic Ogg file: `header_packets` laced as header pages
    /// (BOS on the first), then `audio_packets` laced as audio pages continuing the
    /// sequence numbers, all sharing `serial`.
    fn build_codec_file(serial: u32, header_packets: &[&[u8]], audio_packets: &[&[u8]]) -> Vec<u8> {
        use musefs_format::ogg::page_test_support::{build_header_pub, lace_packet_pub};
        let (mut bytes, header_pages) = build_header_pub(serial, header_packets);
        let mut seq = header_pages;
        for pkt in audio_packets {
            let (b, used) = lace_packet_pub(serial, seq, false, 1000, pkt);
            bytes.extend_from_slice(&b);
            seq += used;
        }
        bytes
    }


    fn oracle_roundtrip_new(file: &[u8]) {
        use musefs_format::ogg::{locate_audio, read_header, synthesize_layout};
        let scan = locate_audio(file).unwrap();
        let header = read_header(file).unwrap();
        let layout =
            synthesize_layout(&header, scan.audio_offset, scan.audio_length, &[], &[]).unwrap();
        let (hdr_bytes, ao, alen, delta) = materialize_header_and_audio_params(&layout);

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.ogg");
        std::fs::File::create(&path).unwrap().write_all(file).unwrap();

        let backing = std::fs::File::open(&path).unwrap();
        let mut audio = Vec::new();
        serve_ogg_window(&backing, ao, alen, delta, 0, alen, &mut audio).unwrap();

        let mut full = hdr_bytes;
        full.extend_from_slice(&audio);
        assert_clean_bitstream(&full);
    }

    #[test]
    fn oracle_new_opus_stream_is_clean() {
        let head = b"OpusHead\x01\x02\x38\x01\x80\xbb\x00\x00\x00\x00\x00".as_slice();
        let tags = b"OpusTags\x06\x00\x00\x00musefs\x00\x00\x00\x00".as_slice();
        let audio0 = vec![0xA1u8; 4000];
        let audio1 = vec![0xA2u8; 80_000];
        let file = build_codec_file(0x1234, &[head, tags], &[&audio0, &audio1]);
        oracle_roundtrip_new(&file);
    }

    #[test]
    fn oracle_new_vorbis_stream_is_clean() {
        let id = b"\x01vorbis\x00\x00\x00\x00\x02\x44\xac\x00\x00\x00\x00\x00\x00\x00\xee\x02\x00\x00\x00\x00\x00\x01".as_slice();
        let comment = b"\x03vorbis\x06\x00\x00\x00musefs\x00\x00\x00\x00\x01".as_slice();
        let setup = b"\x05vorbis-setup-stub".as_slice();
        let audio0 = vec![0xB1u8; 5000];
        let file = build_codec_file(0x2222, &[id, comment, setup], &[&audio0]);
        oracle_roundtrip_new(&file);
    }

    #[test]
    fn oracle_new_oggflac_stream_is_clean() {
        let mut p0 = Vec::new();
        p0.extend_from_slice(b"\x7FFLAC");
        p0.extend_from_slice(&[1, 0]);
        p0.extend_from_slice(&1u16.to_be_bytes());
        p0.extend_from_slice(b"fLaC");
        p0.push(0);
        p0.extend_from_slice(&[0, 0, 34]);
        p0.extend_from_slice(&[0u8; 34]);
        let mut comment = Vec::new();
        comment.push(0x84);
        let vc = b"\x06\x00\x00\x00musefs\x00\x00\x00\x00";
        comment.extend_from_slice(&[0, 0, vc.len() as u8]);
        comment.extend_from_slice(vc);
        let audio0 = vec![0xC1u8; 6000];
        let file = build_codec_file(0x3333, &[&p0, &comment], &[&audio0]);
        oracle_roundtrip_new(&file);
    }

    /// Run the full synth+serve pipeline for one file and assert the spliced stream
    /// is a clean bitstream.
    fn oracle_roundtrip(file: &[u8]) {
        use musefs_format::ogg::{locate_audio, read_header, synthesize_layout};
        let scan = locate_audio(file).unwrap();
        let header = read_header(file).unwrap();
        let layout =
            synthesize_layout(&header, scan.audio_offset, scan.audio_length, &[], &[]).unwrap();
        let (hdr, ao, alen, delta) = materialize_header_and_audio_params(&layout);

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.ogg");
        std::fs::File::create(&path)
            .unwrap()
            .write_all(file)
            .unwrap();
        let idx = build_index(&path, ao, alen, delta).unwrap();
        let backing = std::fs::File::open(&path).unwrap();
        let total: u64 = idx
            .pages
            .iter()
            .map(|p| p.header.len() as u64 + p.payload_len)
            .sum();
        let mut audio = Vec::new();
        serve(&idx, &backing, ao, 0, total, &mut audio).unwrap();

        let mut full = hdr;
        full.extend_from_slice(&audio);
        assert_clean_bitstream(&full);
    }

    #[test]
    fn oracle_opus_stream_is_clean_after_synth_and_serve() {
        let head = b"OpusHead\x01\x02\x38\x01\x80\xbb\x00\x00\x00\x00\x00".as_slice();
        let tags = b"OpusTags\x06\x00\x00\x00musefs\x00\x00\x00\x00".as_slice();
        let audio0 = vec![0xA1u8; 4000];
        let audio1 = vec![0xA2u8; 80_000]; // spans pages -> exercises renumber across pages
        let file = build_codec_file(0x1234, &[head, tags], &[&audio0, &audio1]);
        oracle_roundtrip(&file);
    }

    #[test]
    fn oracle_vorbis_stream_is_clean_after_synth_and_serve() {
        // Vorbis: 3 header packets (id, comment, setup).
        let id = b"\x01vorbis\x00\x00\x00\x00\x02\x44\xac\x00\x00\x00\x00\x00\x00\x00\xee\x02\x00\x00\x00\x00\x00\x01".as_slice();
        let comment = b"\x03vorbis\x06\x00\x00\x00musefs\x00\x00\x00\x00\x01".as_slice();
        let setup = b"\x05vorbis-setup-stub".as_slice();
        let audio0 = vec![0xB1u8; 5000];
        let file = build_codec_file(0x2222, &[id, comment, setup], &[&audio0]);
        oracle_roundtrip(&file);
    }

    #[test]
    fn oracle_oggflac_stream_is_clean_after_synth_and_serve() {
        // OggFLAC packet 0: 0x7F"FLAC" major minor count(BE=1) "fLaC" + STREAMINFO
        // header (type 0, len 34) + 34 bytes. One following packet: VORBIS_COMMENT.
        let mut p0 = Vec::new();
        p0.extend_from_slice(b"\x7FFLAC");
        p0.extend_from_slice(&[1, 0]); // major, minor
        p0.extend_from_slice(&1u16.to_be_bytes()); // 1 following packet
        p0.extend_from_slice(b"fLaC");
        p0.push(0); // STREAMINFO block type, not last
        p0.extend_from_slice(&[0, 0, 34]); // 24-bit length = 34
        p0.extend_from_slice(&[0u8; 34]);
        let mut comment = Vec::new();
        comment.push(0x84); // block type 4 (VORBIS_COMMENT), last-block bit set
        let vc = b"\x06\x00\x00\x00musefs\x00\x00\x00\x00";
        comment.extend_from_slice(&[0, 0, vc.len() as u8]);
        comment.extend_from_slice(vc);
        let audio0 = vec![0xC1u8; 6000];
        let file = build_codec_file(0x3333, &[&p0, &comment], &[&audio0]);
        oracle_roundtrip(&file);
    }


    // ── helpers for the new serve_ogg_window API ──────────────────────────────

    /// Synthetic fixture: 16-byte prefix, then two packets (300 B at seq 5,
    /// 70 000 B at seq 6 spanning 2 pages). Returns (TempDir, path,
    /// audio_offset=16, audio_length).
    fn new_serve_fixture() -> (tempfile::TempDir, std::path::PathBuf, u64, u64) {
        let (mut audio, _) = lace_packet_pub(0xABCD, 5, false, 100, &vec![1u8; 300]);
        let (b2, _) = lace_packet_pub(0xABCD, 6, false, 200, &vec![2u8; 70_000]);
        audio.extend_from_slice(&b2);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("audio.ogg");
        let mut file = vec![0u8; 16];
        file.extend_from_slice(&audio);
        std::fs::File::create(&path).unwrap().write_all(&file).unwrap();
        let audio_length = audio.len() as u64;
        (dir, path, 16, audio_length)
    }

    /// Build the reference served bytes for seq_delta=2 by applying the full-page
    /// oracle (patch_page_header) to every page and concatenating header+payload.
    fn new_reference_region(path: &std::path::Path, ao: u64, alen: u64) -> Vec<u8> {
        use musefs_format::ogg::{parse_page, patch_page_header};
        let backing = std::fs::File::open(path).unwrap();
        let mut full = vec![0u8; alen as usize];
        backing.read_exact_at(&mut full, ao).unwrap();
        let mut out = Vec::new();
        let mut pos = 0usize;
        while pos < full.len() {
            let h = parse_page(&full, pos).unwrap();
            let new_seq = h.seq.wrapping_add(2);
            let patched = patch_page_header(&full[pos..pos + h.total_len()], new_seq).unwrap();
            out.extend_from_slice(&patched);
            out.extend_from_slice(&full[pos + h.header_len..pos + h.total_len()]);
            pos += h.total_len();
        }
        out
    }

    fn new_serve_range(path: &std::path::Path, ao: u64, alen: u64, a: u64, b: u64) -> Vec<u8> {
        let backing = std::fs::File::open(path).unwrap();
        let mut out = Vec::new();
        serve_ogg_window(&backing, ao, alen, 2, a, b, &mut out).unwrap();
        out
    }

    // ── find_page_start tests ─────────────────────────────────────────────────

    #[test]
    fn find_page_start_at_audio_offset_returns_immediately() {
        let (_d, path, ao, _alen) = new_serve_fixture();
        let backing = std::fs::File::open(&path).unwrap();
        // abs_target == audio_offset → special-case, no backward read.
        assert_eq!(find_page_start(&backing, ao, ao).unwrap(), ao);
    }

    #[test]
    fn find_page_start_mid_page_returns_page_start() {
        let (_d, path, ao, _alen) = new_serve_fixture();
        let backing = std::fs::File::open(&path).unwrap();
        // Parse the first page header to know its length.
        let mut hdr = vec![0u8; 282];
        backing.read_exact_at(&mut hdr, ao).unwrap();
        let h = musefs_format::ogg::parse_page(&hdr, 0).unwrap();
        // Target 10 bytes into the payload of page 0.
        let target = ao + h.header_len as u64 + 10;
        let found = find_page_start(&backing, ao, target).unwrap();
        assert_eq!(found, ao, "mid-payload target should resolve to page 0's start");
    }

    #[test]
    fn find_page_start_at_page_boundary_returns_preceding_page() {
        let (_d, path, ao, _alen) = new_serve_fixture();
        let backing = std::fs::File::open(&path).unwrap();
        let mut hdr = vec![0u8; 282];
        backing.read_exact_at(&mut hdr, ao).unwrap();
        let h = musefs_format::ogg::parse_page(&hdr, 0).unwrap();
        // Target exactly at the boundary between page 0 and page 1.
        // The half-open scan window [start, abs_target) does not include abs_target,
        // so the scan returns page 0's start. The forward pass in serve_ogg_window
        // will skip page 0 (no overlap) and serve from page 1 correctly.
        let page1_abs = ao + h.total_len() as u64;
        let found = find_page_start(&backing, ao, page1_abs).unwrap();
        assert_eq!(found, ao);
    }

    #[test]
    fn find_page_start_skips_false_oggs_in_payload() {
        // A single real page whose payload embeds a coincidental "OggS" + a plausible
        // (version 0, header_type 0, seg_count 0) header but a garbage CRC field. The
        // backward scan finds this fake first (it is to the right of the real start),
        // must reject it via the CRC guard, and return the real page start.
        let mut payload = vec![0u8; 600];
        // A complete 27-byte fake header: OggS | ver(1)=0 | htype(1)=0 | granule(8)=0
        // | serial(4) | seq(4) | crc(4)=garbage | seg_count(1)=0. Passes the cheap
        // checks (version 0, header_type 0, seg_count 0) but its CRC field is garbage.
        let fake: &[u8] =
            b"OggS\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x9a\x9a\x9a\x9a\x11\x22\x33\x44\xde\xad\xbe\xef\x00";
        assert_eq!(fake.len(), 27);
        payload[100..100 + fake.len()].copy_from_slice(fake);
        let (audio, _) = lace_packet_pub(0xABCD, 5, false, 100, &payload);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fake.ogg");
        let mut file = vec![0u8; 16];
        file.extend_from_slice(&audio);
        std::fs::File::create(&path).unwrap().write_all(&file).unwrap();
        let ao = 16u64;
        let backing = std::fs::File::open(&path).unwrap();
        // Target near the end of the real page; the fake OggS at payload+100 sits
        // between the real start and the target, so it is the rightmost candidate.
        let abs_target = ao + audio.len() as u64 - 1;
        let found = find_page_start(&backing, ao, abs_target).unwrap();
        assert_eq!(found, ao, "must reject the CRC-invalid fake OggS and return the real start");
    }

    // ── serve_ogg_window tests ────────────────────────────────────────────────

    #[test]
    fn serve_ogg_window_whole_region_matches_reference() {
        let (_d, path, ao, alen) = new_serve_fixture();
        let want = new_reference_region(&path, ao, alen);
        assert_eq!(new_serve_range(&path, ao, alen, 0, alen), want);
    }


    #[test]
    fn serve_ogg_window_header_only_read() {
        let (_d, path, ao, alen) = new_serve_fixture();
        let want = new_reference_region(&path, ao, alen);
        // Parse the first page to get header_len.
        let backing = std::fs::File::open(&path).unwrap();
        let mut hdr = vec![0u8; 282];
        backing.read_exact_at(&mut hdr, ao).unwrap();
        let h = musefs_format::ogg::parse_page(&hdr, 0).unwrap();
        let hlen = h.header_len as u64;
        // First 10 bytes of header.
        assert_eq!(new_serve_range(&path, ao, alen, 0, 10), want[..10]);
        // Exactly the whole header of page 0.
        assert_eq!(new_serve_range(&path, ao, alen, 0, hlen), want[..hlen as usize]);
    }

    #[test]
    fn serve_ogg_window_payload_mid_start() {
        // Serve starting 10 bytes into page 0's payload.
        let (_d, path, ao, alen) = new_serve_fixture();
        let want = new_reference_region(&path, ao, alen);
        let backing = std::fs::File::open(&path).unwrap();
        let mut hdr = vec![0u8; 282];
        backing.read_exact_at(&mut hdr, ao).unwrap();
        let h = musefs_format::ogg::parse_page(&hdr, 0).unwrap();
        let hlen = h.header_len as u64;
        let start = hlen + 10;
        let end = hlen + 60;
        assert_eq!(
            new_serve_range(&path, ao, alen, start, end),
            want[start as usize..end as usize]
        );
    }

    #[test]
    fn serve_ogg_window_spanning_header_and_payload() {
        let (_d, path, ao, alen) = new_serve_fixture();
        let want = new_reference_region(&path, ao, alen);
        let backing = std::fs::File::open(&path).unwrap();
        let mut hdr = vec![0u8; 282];
        backing.read_exact_at(&mut hdr, ao).unwrap();
        let h = musefs_format::ogg::parse_page(&hdr, 0).unwrap();
        let hlen = h.header_len as u64;
        let r = (hlen - 5)..(hlen + 20);
        assert_eq!(
            new_serve_range(&path, ao, alen, r.start, r.end),
            want[r.start as usize..r.end as usize]
        );
    }

    #[test]
    fn serve_ogg_window_crossing_page_boundary() {
        let (_d, path, ao, alen) = new_serve_fixture();
        let want = new_reference_region(&path, ao, alen);
        let backing = std::fs::File::open(&path).unwrap();
        let mut hdr = vec![0u8; 282];
        backing.read_exact_at(&mut hdr, ao).unwrap();
        let h = musefs_format::ogg::parse_page(&hdr, 0).unwrap();
        let p0_end = h.total_len() as u64;
        let r = (p0_end - 30)..(p0_end + 40);
        assert_eq!(
            new_serve_range(&path, ao, alen, r.start, r.end),
            want[r.start as usize..r.end as usize]
        );
    }

    #[test]
    fn serve_ogg_window_empty_and_past_end() {
        let (_d, path, ao, alen) = new_serve_fixture();
        let want = new_reference_region(&path, ao, alen);
        // Empty range.
        assert!(new_serve_range(&path, ao, alen, 100, 100).is_empty());
        // Entirely past end.
        assert!(new_serve_range(&path, ao, alen, alen, alen + 50).is_empty());
        // rend clamped to region end.
        assert_eq!(
            new_serve_range(&path, ao, alen, alen - 25, alen + 1000),
            want[(alen - 25) as usize..]
        );
    }

    #[test]
    fn serve_ogg_window_errors_on_misaligned_audio_length() {
        // audio_length not on a page boundary triggers the integrity guard: the
        // single page's declared total_len pushes `pos` past audio_end → Malformed.
        // Mirrors the removed build_index consumed != audio_length check.
        let (bytes, _) = lace_packet_pub(0xABCD, 0, false, 0, &vec![7u8; 300]);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.ogg");
        std::fs::File::create(&path).unwrap().write_all(&bytes).unwrap();
        let audio_length = bytes.len() as u64 - 5;
        let backing = std::fs::File::open(&path).unwrap();
        let mut out = Vec::new();
        let r = serve_ogg_window(&backing, 0, audio_length, 0, 0, audio_length, &mut out);
        assert!(r.is_err(), "misaligned audio_length must error");
    }
}
