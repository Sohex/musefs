//! Lazy, cached per-file index for serving `Segment::OggAudio`: a single buffered
//! sequential pass over the backing file's audio region that renumbers each page's
//! sequence number and recomputes its CRC, recording only `{offset, header,
//! payload_len}` per page — payloads are never retained and are served from the
//! backing file.

use std::io::{BufReader, Read, Seek, SeekFrom};
use std::path::Path;

use musefs_format::ogg::parse_page;

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
}
