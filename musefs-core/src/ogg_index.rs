//! Per-request Ogg audio serving via backwards-scan and algebraic CRC patching.
//! Replaces the eager whole-region `build_index` with a stateless strategy:
//! `find_page_start` locates the containing page via a ~65 KB backwards read;
//! `serve_ogg_window` patches headers algebraically and serves payload slices
//! via exact positioned reads — no in-memory index, no first-read scan cost.

use std::os::unix::fs::FileExt;

use musefs_format::ogg::{patch_page_header_algebraic, verify_page_crc};

use musefs_db::convert::usize_from;

use crate::error::Result;

/// A one-entry memo of the most-recently-served page: `(page offset within the
/// audio region, page total length, patched header bytes)`. Lives on the resolved
/// file so consecutive reads (a) reuse the patched header for the page straddling a
/// chunk boundary, and (b) short-circuit `find_page_start` — skipping its backward
/// scan AND the full-page entry CRC guard — when the next request lands inside this
/// already-located page. Trusting it does not weaken the determinism guarantee: the
/// page descended from a CRC-validated entry within the same resolved file, whose
/// backing bytes are immutable for its life (a content change rebuilds the file and
/// yields a fresh, empty memo). Bounded to a single ~282-byte header.
pub type LastPageMemo = std::sync::Mutex<Option<(u64, u64, Vec<u8>)>>;

/// Maximum Ogg page size in bytes: 27 fixed header + 255 seg-table + 255×255 payload.
const MAX_OGG_PAGE_BYTES: u64 = 65_307;
/// Maximum Ogg page header size: 27 fixed + 255 seg-table.
const MAX_OGG_HEADER_BYTES: usize = 282;

/// Positioned read that records serve-path pread metrics (count + bytes).
/// Counts on the attempt, like `on_open` — a failed read is still a round-trip.
fn read_counted(f: &std::fs::File, buf: &mut [u8], offset: u64) -> std::io::Result<()> {
    crate::metrics::on_pread(buf.len() as u64);
    f.read_exact_at(buf, offset)
}

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
///
/// Fast path: if `memo` holds a page whose `[start, start+total_len)` region
/// contains `abs_target`, return that start directly — skipping both the backward
/// scan and the full-page CRC guard. That page was already located within this
/// resolved file (whose backing bytes are immutable for its life), so reusing it is
/// as sound as the forward page-length walk `serve_ogg_window` already trusts.
fn find_page_start(
    backing: &std::fs::File,
    audio_offset: u64,
    abs_target: u64,
    memo: Option<&LastPageMemo>,
) -> Result<u64> {
    if abs_target == audio_offset {
        return Ok(audio_offset);
    }
    if let Some(m) = memo {
        let guard = crate::lock::lock_or_clear(m, "ogg last-page memo");
        if let Some((rel, total_len, _)) = guard.as_ref() {
            let start = audio_offset + *rel;
            if start <= abs_target && abs_target < start + *total_len {
                return Ok(start);
            }
        }
    }
    let scan_start = abs_target
        .saturating_sub(MAX_OGG_PAGE_BYTES)
        .max(audio_offset);
    let window_len = usize_from(abs_target - scan_start);
    let mut window = vec![0u8; window_len];
    read_counted(backing, &mut window, scan_start)?;

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
                && window.get(i + 5).is_none_or(|&ht| ht & 0xF8 == 0); // header_type
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
    crate::metrics::on_pread(head.len() as u64);
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
    if read_counted(backing, &mut page, page_start).is_err() {
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
#[allow(clippy::too_many_arguments)] // serve geometry + memo; bundling adds no clarity
pub fn serve_ogg_window(
    backing: &crate::readahead::BackingReader,
    audio_offset: u64,
    audio_length: u64,
    seq_delta: i64,
    rstart: u64,
    rend: u64,
    out: &mut Vec<u8>,
    memo: Option<&LastPageMemo>,
) -> Result<()> {
    if rstart >= rend {
        return Ok(());
    }
    let audio_end = audio_offset + audio_length;
    let abs_rstart = audio_offset + rstart;
    let mut pos = find_page_start(backing.file(), audio_offset, abs_rstart, memo)?;

    while pos < audio_end {
        let page_rel = pos - audio_offset;
        if page_rel >= rend {
            break;
        }
        // One pread for the full header (27 + up to 255 seg-table bytes).
        // Clamped to the declared audio region end.
        let read_len = MAX_OGG_HEADER_BYTES.min(usize_from(audio_end - pos));
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
        let payload_len: usize = hdr_buf[27..header_len].iter().map(|&b| b as usize).sum();

        // Reuse the patched header if this is the page the last serve ended on
        // (sequential reads re-touch the page straddling each chunk boundary). The
        // header for a given page_rel is deterministic for the life of the resolved
        // file (immutable backing + fixed seq_delta), so a one-entry memo is always
        // correct. The lock is released before patching so concurrent readers never
        // serialize on the CRC work.
        let cached = memo.and_then(|m| {
            let g = crate::lock::lock_or_clear(m, "ogg last-page memo");
            g.as_ref()
                .filter(|(mp, _, _)| *mp == page_rel)
                .map(|(_, _, h)| h.clone())
        });
        let patched_hdr = if let Some(h) = cached {
            h
        } else {
            let old_seq = u32::from_le_bytes(hdr_buf[18..22].try_into().unwrap());
            #[expect(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let new_seq = old_seq.wrapping_add(seq_delta as u32);
            patch_page_header_algebraic(&hdr_buf[..header_len], new_seq)?
        };
        if let Some(m) = memo {
            let total_len = (header_len + payload_len) as u64;
            *crate::lock::lock_or_clear(m, "ogg last-page memo") =
                Some((page_rel, total_len, patched_hdr.clone()));
        }

        let hdr_end = page_rel + header_len as u64;
        let page_end = hdr_end + payload_len as u64;

        // Header overlap.
        let hs = rstart.max(page_rel);
        let he = rend.min(hdr_end);
        if hs < he {
            let a = usize_from(hs - page_rel);
            let b = usize_from(he - page_rel);
            out.extend_from_slice(&patched_hdr[a..b]);
        }

        // Payload overlap — exactly the bytes requested, no full-page read.
        let ps = rstart.max(hdr_end);
        let pe = rend.min(page_end);
        if ps < pe {
            let within = ps - hdr_end;
            let n = usize_from(pe - ps);
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
    use std::sync::{Arc, Mutex};

    struct TestBr {
        pool: crate::readahead::ReadAheadPool,
        buf: Arc<Mutex<crate::readahead::ReadAhead>>,
    }
    impl TestBr {
        fn new() -> Self {
            TestBr {
                pool: crate::readahead::ReadAheadPool::new(0),
                buf: Arc::new(Mutex::new(crate::readahead::ReadAhead::new(0))),
            }
        }
        fn reader<'a>(
            &'a self,
            f: &'a std::fs::File,
            len: u64,
        ) -> crate::readahead::BackingReader<'a> {
            crate::readahead::BackingReader::new(f, &self.buf, &self.pool, 0, len)
        }
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
        for seg in layout.segments() {
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
        let layout = synthesize_layout(
            &header,
            scan.audio_offset,
            scan.audio_length,
            &[],
            &[],
            &musefs_format::ogg::MapArtSource::default(),
        )
        .unwrap();
        let (hdr_bytes, ao, alen, delta) = materialize_header_and_audio_params(&layout);

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.ogg");
        std::fs::File::create(&path)
            .unwrap()
            .write_all(file)
            .unwrap();

        let backing = std::fs::File::open(&path).unwrap();
        let test_br = TestBr::new();
        let br = test_br.reader(&backing, u64::MAX);
        let mut audio = Vec::new();
        serve_ogg_window(&br, ao, alen, delta, 0, alen, &mut audio, None).unwrap();

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
        comment.extend_from_slice(&[0, 0, u8::try_from(vc.len()).unwrap()]);
        comment.extend_from_slice(vc);
        let audio0 = vec![0xC1u8; 6000];
        let file = build_codec_file(0x3333, &[&p0, &comment], &[&audio0]);
        oracle_roundtrip_new(&file);
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
        std::fs::File::create(&path)
            .unwrap()
            .write_all(&file)
            .unwrap();
        let audio_length = audio.len() as u64;
        (dir, path, 16, audio_length)
    }

    /// Build the reference served bytes for seq_delta=2 by applying the full-page
    /// oracle (patch_page_header) to every page and concatenating header+payload.
    fn new_reference_region(path: &std::path::Path, ao: u64, alen: u64) -> Vec<u8> {
        use musefs_format::ogg::{parse_page, patch_page_header};
        let backing = std::fs::File::open(path).unwrap();
        let mut full = vec![0u8; usize_from(alen)];
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
        let test_br = TestBr::new();
        let br = test_br.reader(&backing, u64::MAX);
        let mut out = Vec::new();
        serve_ogg_window(&br, ao, alen, 2, a, b, &mut out, None).unwrap();
        out
    }

    // ── find_page_start tests ─────────────────────────────────────────────────

    #[test]
    fn find_page_start_at_audio_offset_returns_immediately() {
        let (_d, path, ao, _alen) = new_serve_fixture();
        let backing = std::fs::File::open(&path).unwrap();
        // abs_target == audio_offset → special-case, no backward read.
        assert_eq!(find_page_start(&backing, ao, ao, None).unwrap(), ao);
    }

    #[test]
    fn find_page_start_mid_page_returns_page_start() {
        let (_d, path, ao, _alen) = new_serve_fixture();
        let backing = std::fs::File::open(&path).unwrap();
        // Parse the first page header to know its length.
        let mut hdr = vec![0u8; MAX_OGG_HEADER_BYTES];
        backing.read_exact_at(&mut hdr, ao).unwrap();
        let h = musefs_format::ogg::parse_page(&hdr, 0).unwrap();
        // Target 10 bytes into the payload of page 0.
        let target = ao + h.header_len as u64 + 10;
        let found = find_page_start(&backing, ao, target, None).unwrap();
        assert_eq!(
            found, ao,
            "mid-payload target should resolve to page 0's start"
        );
    }

    #[test]
    fn find_page_start_at_page_boundary_returns_preceding_page() {
        let (_d, path, ao, _alen) = new_serve_fixture();
        let backing = std::fs::File::open(&path).unwrap();
        let mut hdr = vec![0u8; MAX_OGG_HEADER_BYTES];
        backing.read_exact_at(&mut hdr, ao).unwrap();
        let h = musefs_format::ogg::parse_page(&hdr, 0).unwrap();
        // Target exactly at the boundary between page 0 and page 1.
        // The half-open scan window [start, abs_target) does not include abs_target,
        // so the scan returns page 0's start. The forward pass in serve_ogg_window
        // will skip page 0 (no overlap) and serve from page 1 correctly.
        let page1_abs = ao + h.total_len() as u64;
        let found = find_page_start(&backing, ao, page1_abs, None).unwrap();
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
        std::fs::File::create(&path)
            .unwrap()
            .write_all(&file)
            .unwrap();
        let ao = 16u64;
        let backing = std::fs::File::open(&path).unwrap();
        // Target near the end of the real page; the fake OggS at payload+100 sits
        // between the real start and the target, so it is the rightmost candidate.
        let abs_target = ao + audio.len() as u64 - 1;
        let found = find_page_start(&backing, ao, abs_target, None).unwrap();
        assert_eq!(
            found, ao,
            "must reject the CRC-invalid fake OggS and return the real start"
        );
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
        let mut hdr = vec![0u8; MAX_OGG_HEADER_BYTES];
        backing.read_exact_at(&mut hdr, ao).unwrap();
        let h = musefs_format::ogg::parse_page(&hdr, 0).unwrap();
        let hlen = h.header_len as u64;
        // First 10 bytes of header.
        assert_eq!(new_serve_range(&path, ao, alen, 0, 10), want[..10]);
        // Exactly the whole header of page 0.
        assert_eq!(
            new_serve_range(&path, ao, alen, 0, hlen),
            want[..usize_from(hlen)]
        );
    }

    #[test]
    fn serve_ogg_window_payload_mid_start() {
        // Serve starting 10 bytes into page 0's payload.
        let (_d, path, ao, alen) = new_serve_fixture();
        let want = new_reference_region(&path, ao, alen);
        let backing = std::fs::File::open(&path).unwrap();
        let mut hdr = vec![0u8; MAX_OGG_HEADER_BYTES];
        backing.read_exact_at(&mut hdr, ao).unwrap();
        let h = musefs_format::ogg::parse_page(&hdr, 0).unwrap();
        let hlen = h.header_len as u64;
        let start = hlen + 10;
        let end = hlen + 60;
        assert_eq!(
            new_serve_range(&path, ao, alen, start, end),
            want[usize_from(start)..usize_from(end)]
        );
    }

    #[test]
    fn serve_ogg_window_spanning_header_and_payload() {
        let (_d, path, ao, alen) = new_serve_fixture();
        let want = new_reference_region(&path, ao, alen);
        let backing = std::fs::File::open(&path).unwrap();
        let mut hdr = vec![0u8; MAX_OGG_HEADER_BYTES];
        backing.read_exact_at(&mut hdr, ao).unwrap();
        let h = musefs_format::ogg::parse_page(&hdr, 0).unwrap();
        let hlen = h.header_len as u64;
        let r = (hlen - 5)..(hlen + 20);
        assert_eq!(
            new_serve_range(&path, ao, alen, r.start, r.end),
            want[usize_from(r.start)..usize_from(r.end)]
        );
    }

    #[test]
    fn serve_ogg_window_crossing_page_boundary() {
        let (_d, path, ao, alen) = new_serve_fixture();
        let want = new_reference_region(&path, ao, alen);
        let backing = std::fs::File::open(&path).unwrap();
        let mut hdr = vec![0u8; MAX_OGG_HEADER_BYTES];
        backing.read_exact_at(&mut hdr, ao).unwrap();
        let h = musefs_format::ogg::parse_page(&hdr, 0).unwrap();
        let p0_end = h.total_len() as u64;
        let r = (p0_end - 30)..(p0_end + 40);
        assert_eq!(
            new_serve_range(&path, ao, alen, r.start, r.end),
            want[usize_from(r.start)..usize_from(r.end)]
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
            want[usize_from(alen - 25)..]
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
        std::fs::File::create(&path)
            .unwrap()
            .write_all(&bytes)
            .unwrap();
        let audio_length = bytes.len() as u64 - 5;
        let backing = std::fs::File::open(&path).unwrap();
        let test_br = TestBr::new();
        let br = test_br.reader(&backing, u64::MAX);
        let mut out = Vec::new();
        let r = serve_ogg_window(&br, 0, audio_length, 0, 0, audio_length, &mut out, None);
        assert!(r.is_err(), "misaligned audio_length must error");
    }

    #[test]
    fn serve_ogg_window_memo_reuse_is_byte_identical() {
        // Serve the whole region in small adjacent chunks (< page size, so chunks
        // straddle page boundaries) reusing ONE memo, the way sequential reads do.
        // The page straddling each boundary is re-touched, hitting the memo. Output
        // must be byte-identical to the no-memo reference, and the memo must fill.
        let (_d, path, ao, alen) = new_serve_fixture();
        let want = new_reference_region(&path, ao, alen);
        let backing = std::fs::File::open(&path).unwrap();
        let test_br = TestBr::new();
        let br = test_br.reader(&backing, u64::MAX);
        let memo: LastPageMemo = std::sync::Mutex::new(None);
        let mut out = Vec::new();
        let mut off = 0u64;
        let chunk = 20_000u64;
        while off < alen {
            let end = (off + chunk).min(alen);
            serve_ogg_window(&br, ao, alen, 2, off, end, &mut out, Some(&memo)).unwrap();
            off = end;
        }
        assert_eq!(out, want, "memo-served bytes must match the reference");
        assert!(
            memo.lock().unwrap().is_some(),
            "memo should hold the last patched page"
        );
    }

    #[test]
    fn find_page_start_short_circuits_within_memoized_page() {
        let (_d, path, ao, _alen) = new_serve_fixture();
        let backing = std::fs::File::open(&path).unwrap();
        let mut hdr = vec![0u8; MAX_OGG_HEADER_BYTES];
        backing.read_exact_at(&mut hdr, ao).unwrap();
        let h = musefs_format::ogg::parse_page(&hdr, 0).unwrap();
        // Prime the memo with page 0's geometry (header bytes unused by the lookup).
        let memo: LastPageMemo =
            std::sync::Mutex::new(Some((0u64, h.total_len() as u64, Vec::new())));
        // A target inside page 0 short-circuits to page 0's start (no scan/CRC).
        let inside = ao + h.header_len as u64 + 50;
        assert_eq!(
            find_page_start(&backing, ao, inside, Some(&memo)).unwrap(),
            ao
        );
        // A target past page 0 misses the memo and falls through to the scan,
        // returning page 1's start — proving the short-circuit is range-gated.
        let beyond = ao + h.total_len() as u64 + 10;
        assert_eq!(
            find_page_start(&backing, ao, beyond, Some(&memo)).unwrap(),
            ao + h.total_len() as u64
        );
    }

    #[test]
    fn find_page_start_memo_range_boundaries_are_exact() {
        // Backing with NO valid Ogg page → the scan path always errors, so
        // find_page_start returns Ok ONLY when the memo short-circuit fires. That
        // makes the range gate `start <= abs_target < start + total_len` observable
        // at both boundaries (kills the <=/< boundary mutations on that line).
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("zeros.bin");
        std::fs::File::create(&path)
            .unwrap()
            .write_all(&vec![0u8; 4096])
            .unwrap();
        let backing = std::fs::File::open(&path).unwrap();
        let ao = 100u64;
        // start = ao + 0 = 100; validated range [100, 1100).
        let memo: LastPageMemo = std::sync::Mutex::new(Some((0u64, 1000u64, Vec::new())));
        // Strictly inside → short-circuit (kills `<=`->`>` on `start <= abs_target`
        // and `<`->`==` on `abs_target < start+total_len`, both of which would drop
        // to the erroring scan here).
        assert_eq!(
            find_page_start(&backing, ao, 500, Some(&memo)).unwrap(),
            100
        );
        // One byte below the end → still inside.
        assert_eq!(
            find_page_start(&backing, ao, 1099, Some(&memo)).unwrap(),
            100
        );
        // Exactly at the end (half-open) → NOT inside → scan → Err. Kills `<`->`<=`,
        // which would wrongly short-circuit at the boundary.
        assert!(find_page_start(&backing, ao, 1100, Some(&memo)).is_err());
        // Without the memo, even an inside target errors (no valid page to scan).
        assert!(find_page_start(&backing, ao, 500, None).is_err());
    }

    #[test]
    fn find_page_start_rejects_valid_crc_page_with_bad_header_type() {
        // `parse_page` (hence `page_crc_ok`) does NOT check header_type, so a page
        // with a high header_type bit set but a correctly recomputed CRC would pass
        // the CRC guard. Only the cheap-filter's `header_type & 0xF8 == 0` test
        // rejects it — so the `&&` joining the version and header_type checks is
        // load-bearing. The backward scan must skip this bad page and return the
        // real first page; an `&&`->`||` would instead accept the bad page.
        let (mut real, _) = lace_packet_pub(0xABCD, 5, false, 100, &[3u8; 200]);
        let (mut bad, _) = lace_packet_pub(0xABCD, 6, false, 200, &[4u8; 200]);
        bad[5] = 0x80; // header_type high bit -> fails (ht & 0xF8 != 0), version stays 0
        bad[22..26].copy_from_slice(&0u32.to_le_bytes());
        let crc = crc::Crc::<u32>::new(&CRC_32_OGG).checksum(&bad);
        bad[22..26].copy_from_slice(&crc.to_le_bytes());
        let real_len = real.len() as u64;
        real.extend_from_slice(&bad);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("badht.ogg");
        std::fs::File::create(&path)
            .unwrap()
            .write_all(&real)
            .unwrap();
        let backing = std::fs::File::open(&path).unwrap();
        // Target inside the bad page → it is the rightmost cheap candidate. The scan
        // must reject it (bad header_type) and fall back to the real page at 0.
        let abs_target = real_len + 10;
        assert_eq!(find_page_start(&backing, 0, abs_target, None).unwrap(), 0);
    }

    /// A valid 0-segment Ogg page: 27 bytes, no payload, correct CRC.
    fn empty_page(serial: u32, seq: u32) -> Vec<u8> {
        let mut p = vec![0u8; 27];
        p[..4].copy_from_slice(b"OggS"); // capture; version/header_type/granule = 0
        p[14..18].copy_from_slice(&serial.to_le_bytes());
        p[18..22].copy_from_slice(&seq.to_le_bytes()); // p[26] = seg_count 0
        let crc = crc::Crc::<u32>::new(&CRC_32_OGG).checksum(&p);
        p[22..26].copy_from_slice(&crc.to_le_bytes());
        p
    }

    #[test]
    fn serve_ogg_window_serves_valid_27_byte_trailing_page() {
        // A real page followed by a valid 0-segment (exactly 27-byte) page. Serving
        // the whole region makes serve_ogg_window read that trailing page with
        // read_len == 27, exercising its `hdr_buf.len() < 27` guard at the boundary:
        // a valid 27-byte page must NOT be rejected. `<`->`==`/`<=` would Err here.
        let (mut data, _) = lace_packet_pub(0xABCD, 5, false, 100, &[9u8; 200]);
        data.extend_from_slice(&empty_page(0xABCD, 6));
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("emptytail.ogg");
        std::fs::File::create(&path)
            .unwrap()
            .write_all(&data)
            .unwrap();
        let backing = std::fs::File::open(&path).unwrap();
        let test_br = TestBr::new();
        let br = test_br.reader(&backing, u64::MAX);
        let alen = data.len() as u64;
        let mut out = Vec::new();
        // seq_delta 0 → patched headers equal the originals, so output == input.
        serve_ogg_window(&br, 0, alen, 0, 0, alen, &mut out, None).unwrap();
        assert_eq!(out, data);
    }

    #[test]
    fn find_page_start_accepts_valid_27_byte_page_at_eof() {
        // A valid 0-segment page whose 27 bytes are exactly all that remain at EOF.
        // page_crc_ok reads exactly 27 bytes; `head.len() < 27` -> `<=` would reject
        // the valid page and return the earlier one instead.
        let (mut data, _) = lace_packet_pub(0xABCD, 5, false, 100, &[8u8; 200]);
        let empty_start = data.len() as u64;
        data.extend_from_slice(&empty_page(0xABCD, 6));
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("emptyeof.ogg");
        std::fs::File::create(&path)
            .unwrap()
            .write_all(&data)
            .unwrap();
        let backing = std::fs::File::open(&path).unwrap();
        // A few bytes past the empty page's start so its "OggS" is fully in the scan
        // window; page_crc_ok then reads the full 27-byte page from the file.
        assert_eq!(
            find_page_start(&backing, 0, empty_start + 4, None).unwrap(),
            empty_start
        );
    }

    #[test]
    fn find_page_start_skips_sub_27_byte_oggs_tail() {
        // A bare "OggS" + a few bytes (< 27 total) at EOF. The backward scan hits it
        // first; page_crc_ok must treat the short read as "not a page" (Ok(false))
        // and continue to the real page. `head.len() < 27` -> `==` would index past
        // the short buffer (panic) instead of bailing out.
        let (mut data, _) = lace_packet_pub(0xABCD, 5, false, 100, &[7u8; 200]);
        data.extend_from_slice(b"OggS\x00\x00\x00\x00\x00\x00"); // 10 bytes < 27
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("shorttail.ogg");
        std::fs::File::create(&path)
            .unwrap()
            .write_all(&data)
            .unwrap();
        let backing = std::fs::File::open(&path).unwrap();
        assert_eq!(
            find_page_start(&backing, 0, data.len() as u64, None).unwrap(),
            0
        );
    }

    #[test]
    fn serve_ogg_window_wraps_seq_past_u32_max() {
        use musefs_format::ogg::{parse_page, patch_page_header};
        // A single audio page whose sequence number is u32::MAX. With seq_delta = +1
        // the patched sequence must wrap to 0, not fail the read.
        let payload = vec![0x5Au8; 300];
        let (page, _) = lace_packet_pub(0x1234, u32::MAX, false, 0, &payload);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wrap.ogg");
        let mut file = vec![0u8; 16];
        file.extend_from_slice(&page);
        std::fs::File::create(&path)
            .unwrap()
            .write_all(&file)
            .unwrap();

        let ao = 16u64;
        let alen = page.len() as u64;
        let backing = std::fs::File::open(&path).unwrap();
        let test_br = TestBr::new();
        let br = test_br.reader(&backing, u64::MAX);
        let mut out = Vec::new();
        serve_ogg_window(&br, ao, alen, 1, 0, alen, &mut out, None).unwrap();

        // The served region must be the page with its sequence wrapped (u32::MAX + 1 == 0):
        // patched header followed by the original payload bytes.
        let h = parse_page(&page, 0).unwrap();
        let mut want = patch_page_header(&page[..h.total_len()], 0).unwrap();
        want.extend_from_slice(&page[h.header_len..h.total_len()]);
        assert_eq!(out, want);
    }
}
