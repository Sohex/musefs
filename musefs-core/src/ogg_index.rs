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
    fn build_index_renumbers_and_preserves_payload_length() {
        // Two audio pages at seq 5 and 6; shift by +2 => 7 and 8.
        let (mut bytes, _) = lace_packet_pub(0xABCD, 5, false, 100, &vec![1u8; 300]);
        let (b2, _) = lace_packet_pub(0xABCD, 6, false, 200, &vec![2u8; 70_000]);
        bytes.extend_from_slice(&b2);

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("audio.ogg");
        // Prefix 16 bytes of "header" so audio_offset is non-zero.
        let mut file_bytes = vec![0u8; 16];
        file_bytes.extend_from_slice(&bytes);
        std::fs::File::create(&path)
            .unwrap()
            .write_all(&file_bytes)
            .unwrap();

        let idx = build_index(&path, 16, bytes.len() as u64, 2).unwrap();
        assert_eq!(idx.pages.len(), 3); // 1 small page + 2 from the big packet
        assert_eq!(idx.pages[0].region_offset, 0);
        // Reconstruct page 0 and confirm its seq shifted to 7.
        let mut full = idx.pages[0].header.clone();
        full.extend(std::iter::repeat(1u8).take(idx.pages[0].payload_len as usize));
        let h = parse_page(&full, 0).unwrap();
        assert_eq!(h.seq, 7);
    }
}
