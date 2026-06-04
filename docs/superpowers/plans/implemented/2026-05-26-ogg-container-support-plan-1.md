# Ogg Container Support — Plan 1 (container + text-tag synthesis) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add read-only synthesis for Ogg-contained **Opus**, **Vorbis**, and **FLAC-in-Ogg**, presenting a re-tagged view (text VorbisComments regenerated from the DB) while serving the original audio pages byte-identically via sequence-number renumbering + per-page CRC recompute. Embedded cover art is deferred to Plan 2.

**Architecture:** A new pure-bytes `ogg` module in `musefs-format` parses/builds Ogg pages (CRC-32, packet lacing) and synthesizes a header region plus a single compact `Segment::OggAudio { offset, len, seq_delta }`. `musefs-core` serves `OggAudio` by building a lazy, cached per-file page index (one buffered sequential pass that recomputes each renumbered page's CRC) guarded for concurrent first reads, and gains a byte-bounded LRU header cache. `Format` grows `Opus`/`Vorbis`/`OggFlac` variants; `scan`/`reader` wire the three codecs in.

**Tech Stack:** Rust (workspace crates `musefs-format`, `musefs-core`, `musefs-db`, `musefs-fuse`), `base64` (decode existing art at scan), `once_cell` (concurrency-guarded lazy index), `crc` + `ogg` (dev-only, independent validation).

**Spec:** `docs/superpowers/specs/2026-05-26-ogg-container-support-design.md`

---

## File Structure

**Create:**
- `musefs-format/src/ogg/mod.rs` — public API: `Codec`, `OggScan`, `OggMeta`, `read_header`, `locate_audio`, `read_metadata`, `read_tags`, `read_pictures`, `synthesize_layout`.
- `musefs-format/src/ogg/crc.rs` — Ogg CRC-32.
- `musefs-format/src/ogg/page.rs` — `PageHeader`, `parse_page`, `lace_packet`, `build_header`, `patch_page_header`.
- `musefs-format/src/vorbiscomment.rs` — shared VorbisComment body build/parse (used by `flac` and `ogg`).
- `musefs-core/src/ogg_index.rs` — buffered sequential page-index builder + range server for `OggAudio`.

**Modify:**
- `musefs-format/src/lib.rs` — declare modules, export `ogg` types.
- `musefs-format/src/flac.rs` — use shared `vorbiscomment`; expose block helpers `pub(crate)`.
- `musefs-format/src/layout.rs` — add `Segment::OggAudio`.
- `musefs-format/Cargo.toml` — add `base64`; dev `crc`.
- `musefs-db/src/models.rs` — `Format::{Opus,Vorbis,OggFlac}`.
- `musefs-core/src/reader.rs` — resolve arms + `ResolvedFile.ogg_index` + `OggAudio` serving + byte-bounded cache.
- `musefs-core/src/scan.rs` — probe + `collect_audio` for `.ogg`/`.oga`/`.opus`.
- `musefs-core/Cargo.toml` — add `once_cell`.
- `musefs-fuse/Cargo.toml` — dev `ogg`.
- `musefs-fuse/tests/` — e2e read-through for the three codecs.

---

## Task 1: Ogg CRC-32 module

**Files:**
- Create: `musefs-format/src/ogg/crc.rs`
- Modify: `musefs-format/src/lib.rs`
- Modify: `musefs-format/Cargo.toml`

- [ ] **Step 1: Wire the module and dev-dep**

In `musefs-format/src/lib.rs`, add after the existing `mod`/`pub mod` lines:

```rust
pub mod ogg;
```

Create `musefs-format/src/ogg/mod.rs` with just:

```rust
mod crc;
```

In `musefs-format/Cargo.toml` add to `[dev-dependencies]`:

```toml
crc = "3"
```

- [ ] **Step 2: Write the failing test**

Create `musefs-format/src/ogg/crc.rs`:

```rust
//! Ogg page CRC-32: polynomial 0x04c11db7, init 0, no input/output reflection,
//! no final XOR. The caller passes the full page with the 4 CRC bytes (offset
//! 22..26) zeroed.

const POLY: u32 = 0x04c1_1db7;

const fn build_table() -> [u32; 256] {
    let mut t = [0u32; 256];
    let mut i = 0usize;
    while i < 256 {
        let mut crc = (i as u32) << 24;
        let mut j = 0;
        while j < 8 {
            crc = if crc & 0x8000_0000 != 0 {
                (crc << 1) ^ POLY
            } else {
                crc << 1
            };
            j += 1;
        }
        t[i] = crc;
        i += 1;
    }
    t
}

const TABLE: [u32; 256] = build_table();

pub fn crc32(buf: &[u8]) -> u32 {
    let mut crc: u32 = 0;
    for &b in buf {
        crc = (crc << 8) ^ TABLE[(((crc >> 24) as u8) ^ b) as usize];
    }
    crc
}

#[cfg(test)]
mod tests {
    use super::crc32;

    fn reference(data: &[u8]) -> u32 {
        // Independent implementation via the `crc` crate, configured with Ogg's
        // exact parameters (init 0, no reflection, no xorout).
        const ALG: crc::Algorithm<u32> = crc::Algorithm {
            width: 32,
            poly: 0x04c1_1db7,
            init: 0,
            refin: false,
            refout: false,
            xorout: 0,
            check: 0,
            residue: 0,
        };
        let c = crc::Crc::<u32>::new(&ALG);
        c.checksum(data)
    }

    #[test]
    fn matches_independent_reference() {
        assert_eq!(crc32(b""), reference(b""));
        assert_eq!(crc32(b"123456789"), reference(b"123456789"));
        let blob: Vec<u8> = (0..=255u8).cycle().take(5000).collect();
        assert_eq!(crc32(&blob), reference(&blob));
    }
}
```

- [ ] **Step 3: Run test to verify it fails**

Run: `cargo test -p musefs-format ogg::crc -- --nocapture`
Expected: FAIL — `crc` dev-dependency not yet downloaded / module not found until Cargo.toml + lib.rs edits are saved; once they are, it compiles and passes. If it fails to compile because `crc` isn't fetched, run `cargo build -p musefs-format` first.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p musefs-format ogg::crc`
Expected: PASS (`matches_independent_reference`).

- [ ] **Step 5: Commit**

```bash
git add musefs-format/src/lib.rs musefs-format/src/ogg/mod.rs musefs-format/src/ogg/crc.rs musefs-format/Cargo.toml
git commit -m "feat(ogg): Ogg CRC-32 with independent reference test"
```

---

## Task 2: Ogg page header parsing

**Files:**
- Create: `musefs-format/src/ogg/page.rs`
- Modify: `musefs-format/src/ogg/mod.rs`

- [ ] **Step 1: Write the failing test**

Create `musefs-format/src/ogg/page.rs`:

```rust
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
    let payload_len: usize = buf[table_start..table_end].iter().map(|&b| b as usize).sum();
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
```

In `musefs-format/src/ogg/mod.rs` add:

```rust
mod page;
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p musefs-format ogg::page`
Expected: FAIL before the file exists / PASS once saved. (If a prior step left it compiling, confirm both tests are discovered.)

- [ ] **Step 3: Run test to verify it passes**

Run: `cargo test -p musefs-format ogg::page`
Expected: PASS (`parses_fields_and_lengths`, `rejects_bad_capture`).

- [ ] **Step 4: Commit**

```bash
git add musefs-format/src/ogg/page.rs musefs-format/src/ogg/mod.rs
git commit -m "feat(ogg): parse Ogg page headers"
```

---

## Task 3: Packet lacing and page building

**Files:**
- Modify: `musefs-format/src/ogg/page.rs`

- [ ] **Step 1: Write the failing test**

Append to `musefs-format/src/ogg/page.rs` (above the `#[cfg(test)]` module), the lacing/builder functions:

```rust
use crate::ogg::crc::crc32;

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
```

Add these tests inside the existing `#[cfg(test)] mod tests` block:

```rust
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
```

- [ ] **Step 2: Run test to verify it fails, then passes**

Run: `cargo test -p musefs-format ogg::page`
Expected: compiles after the functions are added; all four new tests PASS.

- [ ] **Step 3: Commit**

```bash
git add musefs-format/src/ogg/page.rs
git commit -m "feat(ogg): packet lacing and page building with CRC"
```

---

## Task 4: Packet reassembly and header scan

**Files:**
- Modify: `musefs-format/src/ogg/page.rs` (add `read_packets`)
- Modify: `musefs-format/src/ogg/mod.rs` (add `Codec`, `read_header`)

- [ ] **Step 1: Write the failing test (page.rs `read_packets`)**

Append to `musefs-format/src/ogg/page.rs` (before the test module):

```rust
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
```

Add to the test module:

```rust
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
```

- [ ] **Step 2: Run, verify pass**

Run: `cargo test -p musefs-format ogg::page::tests::read_packets_reassembles_multipage_packet`
Expected: PASS.

- [ ] **Step 3: Write the failing test (mod.rs `Codec` + `read_header`)**

Replace the contents of `musefs-format/src/ogg/mod.rs` with:

```rust
mod crc;
mod page;

use crate::error::{FormatError, Result};

/// The codec carried inside an Ogg logical bitstream that we synthesize.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Codec {
    Opus,
    Vorbis,
    OggFlac,
}

fn detect_codec(first_packet: &[u8]) -> Result<Codec> {
    if first_packet.len() >= 8 && &first_packet[0..8] == b"OpusHead" {
        Ok(Codec::Opus)
    } else if first_packet.len() >= 7 && &first_packet[0..7] == b"\x01vorbis" {
        Ok(Codec::Vorbis)
    } else if first_packet.len() >= 5 && &first_packet[0..5] == b"\x7FFLAC" {
        Ok(Codec::OggFlac)
    } else {
        Err(FormatError::Malformed)
    }
}

/// For OggFLAC, packet 0 is `0x7F "FLAC" major minor count(2, BE) "fLaC" STREAMINFO`.
/// The 16-bit big-endian count is the number of metadata-block packets that follow
/// packet 0.
fn oggflac_following_packets(first_packet: &[u8]) -> Result<usize> {
    if first_packet.len() < 9 {
        return Err(FormatError::Malformed);
    }
    Ok(u16::from_be_bytes([first_packet[7], first_packet[8]]) as usize)
}

/// The parsed Ogg header region: codec, serial, the reassembled header packets,
/// the number of header pages, and where audio begins.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OggHeader {
    pub codec: Codec,
    pub serial: u32,
    pub packets: Vec<Vec<u8>>,
    pub header_pages: u32,
    pub audio_offset: u64,
}

/// Parse the header region from the front of a logical bitstream. `data` may be the
/// whole file or just `[0, audio_offset)`; either way parsing stops once all header
/// packets are reassembled.
pub fn read_header(data: &[u8]) -> Result<OggHeader> {
    let first_page = page::parse_page(data, 0)?;
    let serial = first_page.serial;

    // Reassemble the first packet to detect the codec and (for OggFLAC) the count.
    let first = page::read_packets(data, 1)?;
    let first_pkt = first.first().ok_or(FormatError::Malformed)?;
    let codec = detect_codec(&first_pkt.data)?;

    let want = match codec {
        Codec::Opus => 2,
        Codec::Vorbis => 3,
        Codec::OggFlac => 1 + oggflac_following_packets(&first_pkt.data)?,
    };

    let pkts = page::read_packets(data, want)?;
    if pkts.len() != want {
        return Err(FormatError::Malformed);
    }
    let last = pkts.last().unwrap();
    Ok(OggHeader {
        codec,
        serial,
        packets: pkts.iter().map(|p| p.data.clone()).collect(),
        header_pages: last.pages_through_end,
        audio_offset: last.end_offset as u64,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ogg::page::{build_header, lace_packet};

    fn opus_headers() -> Vec<u8> {
        let head = b"OpusHead\x01\x02\x38\x01\x80\xbb\x00\x00\x00\x00\x00".to_vec();
        let tags = b"OpusTags\x06\x00\x00\x00musefs\x00\x00\x00\x00".to_vec();
        let (bytes, _) = build_header(0x1234, &[&head, &tags]);
        bytes
    }

    #[test]
    fn reads_opus_header() {
        let mut data = opus_headers();
        // Append one audio page so audio_offset lands before EOF.
        let (audio, _) = lace_packet(0x1234, 2, false, 960, &vec![0u8; 100]);
        let header_len = data.len();
        data.extend_from_slice(&audio);

        let h = read_header(&data).unwrap();
        assert_eq!(h.codec, Codec::Opus);
        assert_eq!(h.serial, 0x1234);
        assert_eq!(h.packets.len(), 2);
        assert_eq!(h.audio_offset, header_len as u64);
        assert_eq!(h.header_pages, 2);
    }
}
```

- [ ] **Step 4: Run, verify pass**

Run: `cargo test -p musefs-format ogg::`
Expected: PASS (all `ogg::crc`, `ogg::page`, `ogg::tests`).

- [ ] **Step 5: Commit**

```bash
git add musefs-format/src/ogg/page.rs musefs-format/src/ogg/mod.rs
git commit -m "feat(ogg): packet reassembly and codec/header scan"
```

---

## Task 5: Extract shared VorbisComment helpers

**Files:**
- Create: `musefs-format/src/vorbiscomment.rs`
- Modify: `musefs-format/src/lib.rs`
- Modify: `musefs-format/src/flac.rs`

- [ ] **Step 1: Create the shared module**

Create `musefs-format/src/vorbiscomment.rs`:

```rust
//! VorbisComment body build/parse, shared by FLAC's VORBIS_COMMENT block and the
//! Ogg codecs' comment packets. This is the body only: it never includes the
//! Vorbis framing bit or any codec-specific magic.

use crate::error::{FormatError, Result};
use crate::input::TagInput;

pub(crate) const VENDOR: &str = "musefs";

/// Build a VorbisComment body: vendor string then count then `KEY=value` comments.
/// Lengths are 32-bit little-endian; keys are upper-cased.
pub(crate) fn build(tags: &[TagInput]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&(VENDOR.len() as u32).to_le_bytes());
    out.extend_from_slice(VENDOR.as_bytes());
    out.extend_from_slice(&(tags.len() as u32).to_le_bytes());
    for t in tags {
        let comment = format!("{}={}", t.key.to_ascii_uppercase(), t.value);
        out.extend_from_slice(&(comment.len() as u32).to_le_bytes());
        out.extend_from_slice(comment.as_bytes());
    }
    out
}

fn read_u32_le(data: &[u8], pos: usize) -> Result<u32> {
    if pos + 4 > data.len() {
        return Err(FormatError::Malformed);
    }
    Ok(u32::from_le_bytes([
        data[pos],
        data[pos + 1],
        data[pos + 2],
        data[pos + 3],
    ]))
}

/// Parse a VorbisComment body into `(FIELD, value)` pairs in order. Comments
/// without a `=` are skipped. Trailing bytes after the comment list (e.g. a Vorbis
/// framing bit) are ignored.
pub(crate) fn parse(body: &[u8]) -> Result<Vec<(String, String)>> {
    let vendor_len = read_u32_le(body, 0)? as usize;
    let mut pos = 4 + vendor_len;
    let count = read_u32_le(body, pos)? as usize;
    pos += 4;
    let mut out = Vec::with_capacity(count);
    for _ in 0..count {
        let clen = read_u32_le(body, pos)? as usize;
        pos += 4;
        let end = pos + clen;
        if end > body.len() {
            return Err(FormatError::Malformed);
        }
        let comment = std::str::from_utf8(&body[pos..end]).map_err(|_| FormatError::Malformed)?;
        if let Some((field, value)) = comment.split_once('=') {
            out.push((field.to_string(), value.to_string()));
        }
        pos = end;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::TagInput;

    #[test]
    fn build_then_parse_round_trips() {
        let tags = vec![
            TagInput::new("artist", "Boards of Canada"),
            TagInput::new("title", "Roygbiv"),
        ];
        let body = build(&tags);
        let parsed = parse(&body).unwrap();
        assert_eq!(
            parsed,
            vec![
                ("ARTIST".to_string(), "Boards of Canada".to_string()),
                ("TITLE".to_string(), "Roygbiv".to_string()),
            ]
        );
    }
}
```

In `musefs-format/src/lib.rs` add (with the other `mod` lines):

```rust
mod vorbiscomment;
```

Confirm `TagInput::new(key, value)` exists (it does — `input.rs` `impl TagInput`). If its signature differs, adjust the test's constructor calls accordingly.

- [ ] **Step 2: Repoint `flac.rs` and expose block helpers**

In `musefs-format/src/flac.rs`:

1. Delete the private `vorbis_comment_body` function (lines defining it) and the private `parse_vorbis_comment_body` function and the now-unused private `read_u32_le`.
2. Replace the call `let vc = vorbis_comment_body(tags);` in `synthesize_layout` with `let vc = crate::vorbiscomment::build(tags);`.
3. In `read_vorbis_comments`, replace `return parse_vorbis_comment_body(&data[body_start..body_end]);` with `return crate::vorbiscomment::parse(&data[body_start..body_end]);`.
4. Change `fn push_block_header(...)` to `pub(crate) fn push_block_header(...)` and the block-type constants (`BLOCK_STREAMINFO`, `BLOCK_APPLICATION`, `BLOCK_SEEKTABLE`, `BLOCK_VORBIS_COMMENT`, `BLOCK_CUESHEET`, `BLOCK_PICTURE`) and `FLAC_MARKER` to `pub(crate)` (most already are — verify each).
5. Keep `read_u32_be`, `parse_picture_block` as-is (still used by `read_pictures`); change `parse_picture_block` and `read_u32_be` to `pub(crate)` so the Ogg module can reuse them in Task 7.

- [ ] **Step 3: Run the FLAC suite to verify nothing regressed**

Run: `cargo test -p musefs-format flac && cargo test -p musefs-format vorbiscomment`
Expected: PASS — all existing FLAC tests still pass, plus `build_then_parse_round_trips`.

- [ ] **Step 4: Commit**

```bash
git add musefs-format/src/vorbiscomment.rs musefs-format/src/lib.rs musefs-format/src/flac.rs
git commit -m "refactor(format): extract shared VorbisComment helpers; expose FLAC block helpers"
```

---

## Task 6: `read_tags` for the three codecs

**Files:**
- Modify: `musefs-format/src/ogg/mod.rs`

- [ ] **Step 1: Write the failing test**

Add to `musefs-format/src/ogg/mod.rs` (above the test module):

```rust
/// Strip a codec's comment-packet prefix, returning the VorbisComment body slice.
fn comment_body<'a>(codec: Codec, packet: &'a [u8]) -> Result<&'a [u8]> {
    let prefix = match codec {
        Codec::Opus => 8,      // "OpusTags"
        Codec::Vorbis => 7,    // 0x03 "vorbis"
        Codec::OggFlac => 4,   // FLAC metadata block header (type + 24-bit length)
    };
    if packet.len() < prefix {
        return Err(FormatError::Malformed);
    }
    Ok(&packet[prefix..])
}

/// The index of the comment packet within the reassembled header packets.
fn comment_packet_index(header: &OggHeader) -> usize {
    match header.codec {
        Codec::Opus => 1,
        Codec::Vorbis => 1,
        // OggFLAC: packet 0 is the mapping header; the VORBIS_COMMENT block is
        // whichever following packet has block type 4.
        Codec::OggFlac => header
            .packets
            .iter()
            .enumerate()
            .skip(1)
            .find(|(_, p)| !p.is_empty() && (p[0] & 0x7F) == 4)
            .map(|(i, _)| i)
            .unwrap_or(0),
    }
}

/// Read existing `(FIELD, value)` tags from a complete file. Empty if none.
pub fn read_tags(data: &[u8]) -> Result<Vec<(String, String)>> {
    let header = read_header(data)?;
    let idx = comment_packet_index(&header);
    if idx == 0 {
        return Ok(Vec::new()); // no comment packet present
    }
    let body = comment_body(header.codec, &header.packets[idx])?;
    crate::vorbiscomment::parse(body)
}
```

Add to the test module (reusing the `opus_headers` helper from Task 4):

```rust
    #[test]
    fn read_tags_opus() {
        // Build an OpusTags packet with one real comment via the shared builder.
        let body = crate::vorbiscomment::build(&[crate::input::TagInput::new("title", "Sun")]);
        let mut tags_pkt = b"OpusTags".to_vec();
        tags_pkt.extend_from_slice(&body);
        let head = b"OpusHead\x01\x02\x38\x01\x80\xbb\x00\x00\x00\x00\x00".to_vec();
        let (mut data, _) = crate::ogg::page::build_header(7, &[&head, &tags_pkt]);
        let (audio, _) = crate::ogg::page::lace_packet(7, 2, false, 960, &vec![0u8; 50]);
        data.extend_from_slice(&audio);

        let tags = read_tags(&data).unwrap();
        assert_eq!(tags, vec![("TITLE".to_string(), "Sun".to_string())]);
    }
```

- [ ] **Step 2: Run, verify pass**

Run: `cargo test -p musefs-format ogg::tests::read_tags_opus`
Expected: PASS.

- [ ] **Step 3: Commit**

```bash
git add musefs-format/src/ogg/mod.rs
git commit -m "feat(ogg): read existing tags from Opus/Vorbis/OggFLAC comment packets"
```

---

## Task 7: `read_pictures` (decode existing embedded art)

**Files:**
- Modify: `musefs-format/src/ogg/mod.rs`
- Modify: `musefs-format/Cargo.toml`

- [ ] **Step 1: Add the base64 dependency**

In `musefs-format/Cargo.toml` `[dependencies]` add:

```toml
base64 = "0.22"
```

- [ ] **Step 2: Write the failing test**

Add to `musefs-format/src/ogg/mod.rs`:

```rust
use crate::input::EmbeddedPicture;

/// Extract embedded pictures from a complete file for scan-time ingestion.
///
/// Opus/Vorbis carry art as a base64 `METADATA_BLOCK_PICTURE` comment whose decoded
/// bytes are a FLAC PICTURE block body; OggFLAC carries native PICTURE block
/// packets (block type 6). Plan 1 only *reads* art (to seed the DB); synthesis does
/// not yet re-embed it.
pub fn read_pictures(data: &[u8]) -> Result<Vec<EmbeddedPicture>> {
    use base64::Engine;
    let header = read_header(data)?;
    let mut out = Vec::new();
    match header.codec {
        Codec::Opus | Codec::Vorbis => {
            let idx = comment_packet_index(&header);
            if idx == 0 {
                return Ok(out);
            }
            let body = comment_body(header.codec, &header.packets[idx])?;
            for (field, value) in crate::vorbiscomment::parse(body)? {
                if field.eq_ignore_ascii_case("METADATA_BLOCK_PICTURE") {
                    let raw = base64::engine::general_purpose::STANDARD
                        .decode(value.as_bytes())
                        .map_err(|_| FormatError::Malformed)?;
                    out.push(crate::flac::parse_picture_block(&raw)?);
                }
            }
        }
        Codec::OggFlac => {
            for pkt in header.packets.iter().skip(1) {
                if !pkt.is_empty() && (pkt[0] & 0x7F) == 6 {
                    // Strip the 4-byte FLAC metadata block header.
                    out.push(crate::flac::parse_picture_block(&pkt[4..])?);
                }
            }
        }
    }
    Ok(out)
}
```

Add to the test module:

```rust
    #[test]
    fn read_pictures_opus_decodes_metadata_block_picture() {
        use base64::Engine;
        // A minimal FLAC PICTURE block body: type=3, mime="image/png", empty desc,
        // 1x1, depth 0, colors 0, data="PNG".
        let mut pic = Vec::new();
        pic.extend_from_slice(&3u32.to_be_bytes());
        let mime = b"image/png";
        pic.extend_from_slice(&(mime.len() as u32).to_be_bytes());
        pic.extend_from_slice(mime);
        pic.extend_from_slice(&0u32.to_be_bytes()); // desc len
        pic.extend_from_slice(&1u32.to_be_bytes()); // width
        pic.extend_from_slice(&1u32.to_be_bytes()); // height
        pic.extend_from_slice(&0u32.to_be_bytes()); // depth
        pic.extend_from_slice(&0u32.to_be_bytes()); // colors
        let img = b"PNG";
        pic.extend_from_slice(&(img.len() as u32).to_be_bytes());
        pic.extend_from_slice(img);
        let b64 = base64::engine::general_purpose::STANDARD.encode(&pic);

        let mut body = Vec::new();
        body.extend_from_slice(&(crate::vorbiscomment::VENDOR.len() as u32).to_le_bytes());
        body.extend_from_slice(crate::vorbiscomment::VENDOR.as_bytes());
        body.extend_from_slice(&1u32.to_le_bytes()); // one comment
        let comment = format!("METADATA_BLOCK_PICTURE={}", b64);
        body.extend_from_slice(&(comment.len() as u32).to_le_bytes());
        body.extend_from_slice(comment.as_bytes());

        let mut tags_pkt = b"OpusTags".to_vec();
        tags_pkt.extend_from_slice(&body);
        let head = b"OpusHead\x01\x02\x38\x01\x80\xbb\x00\x00\x00\x00\x00".to_vec();
        let (mut data, _) = crate::ogg::page::build_header(7, &[&head, &tags_pkt]);
        let (audio, _) = crate::ogg::page::lace_packet(7, 2, false, 960, &vec![0u8; 50]);
        data.extend_from_slice(&audio);

        let pics = read_pictures(&data).unwrap();
        assert_eq!(pics.len(), 1);
        assert_eq!(pics[0].mime, "image/png");
        assert_eq!(pics[0].data, b"PNG");
    }
```

Make `crate::vorbiscomment::VENDOR` reachable from the test by leaving it `pub(crate)` (already done in Task 5).

- [ ] **Step 3: Run, verify pass**

Run: `cargo test -p musefs-format ogg::tests::read_pictures_opus_decodes_metadata_block_picture`
Expected: PASS.

- [ ] **Step 4: Commit**

```bash
git add musefs-format/src/ogg/mod.rs musefs-format/Cargo.toml
git commit -m "feat(ogg): decode existing embedded art for scan ingestion"
```

---

## Task 8: `locate_audio` and `read_metadata`

**Files:**
- Modify: `musefs-format/src/ogg/mod.rs`
- Modify: `musefs-format/src/lib.rs` (export public types)

- [ ] **Step 1: Write the failing test**

Add to `musefs-format/src/ogg/mod.rs`:

```rust
/// Audio bounds + codec from a complete file, for the scanner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OggScan {
    pub codec: Codec,
    pub audio_offset: u64,
    pub audio_length: u64,
}

pub fn locate_audio(data: &[u8]) -> Result<OggScan> {
    let header = read_header(data)?;
    if header.audio_offset > data.len() as u64 {
        return Err(FormatError::Malformed);
    }
    Ok(OggScan {
        codec: header.codec,
        audio_offset: header.audio_offset,
        audio_length: data.len() as u64 - header.audio_offset,
    })
}

/// The header region parsed from the front of the file (`[0, audio_offset)`), for
/// synthesis. Identical to `read_header` but named to mirror `flac::read_metadata`.
pub fn read_metadata(front: &[u8]) -> Result<OggHeader> {
    read_header(front)
}
```

In `musefs-format/src/lib.rs`, export the public Ogg types:

```rust
pub use ogg::{Codec, OggHeader, OggScan};
```

Add to the test module:

```rust
    #[test]
    fn locate_audio_reports_bounds() {
        let mut data = opus_headers();
        let header_len = data.len();
        let (audio, _) = crate::ogg::page::lace_packet(0x1234, 2, false, 960, &vec![0u8; 120]);
        data.extend_from_slice(&audio);

        let scan = locate_audio(&data).unwrap();
        assert_eq!(scan.codec, Codec::Opus);
        assert_eq!(scan.audio_offset, header_len as u64);
        assert_eq!(scan.audio_length, (data.len() - header_len) as u64);
    }
```

- [ ] **Step 2: Run, verify pass**

Run: `cargo test -p musefs-format ogg::tests::locate_audio_reports_bounds`
Expected: PASS.

- [ ] **Step 3: Commit**

```bash
git add musefs-format/src/ogg/mod.rs musefs-format/src/lib.rs
git commit -m "feat(ogg): locate_audio and read_metadata public API"
```

---

## Task 9: Opus `synthesize_layout` (text tags)

**Files:**
- Modify: `musefs-format/src/layout.rs` (add `OggAudio` — needed by synthesis output)
- Modify: `musefs-format/src/ogg/mod.rs`

- [ ] **Step 1: Add the `OggAudio` segment variant**

In `musefs-format/src/layout.rs`, add a variant to `Segment` and a `len` arm:

```rust
    /// A run of original audio pages served with each page's sequence number
    /// shifted by `seq_delta` and its CRC recomputed. The byte length is unchanged
    /// (renumbering patches in place), so `len` equals the backing audio length.
    OggAudio { offset: u64, len: u64, seq_delta: i64 },
```

In `impl Segment::len`, add:

```rust
            Segment::OggAudio { len, .. } => *len,
```

(`header_len` in `RegionLayout` already excludes only `BackingAudio`; `OggAudio` is audio and should likewise be excluded from the header. Update the filter in `header_len` to also exclude `OggAudio`:)

```rust
    pub fn header_len(&self) -> u64 {
        self.segments
            .iter()
            .filter(|s| !matches!(s, Segment::BackingAudio { .. } | Segment::OggAudio { .. }))
            .map(|s| s.len())
            .sum()
    }
```

- [ ] **Step 2: Write the failing test (Opus synthesis)**

Add to `musefs-format/src/ogg/mod.rs`:

```rust
use crate::input::TagInput;
use crate::layout::{RegionLayout, Segment};

/// Build the regenerated header packets for a codec from the original header
/// packets and the new text tags. Plan 1 emits text VorbisComments only (no art).
fn rebuild_header_packets(header: &OggHeader, tags: &[TagInput]) -> Result<Vec<Vec<u8>>> {
    let vc = crate::vorbiscomment::build(tags);
    match header.codec {
        Codec::Opus => {
            let mut tags_pkt = b"OpusTags".to_vec();
            tags_pkt.extend_from_slice(&vc);
            Ok(vec![header.packets[0].clone(), tags_pkt])
        }
        Codec::Vorbis => {
            let mut comment = b"\x03vorbis".to_vec();
            comment.extend_from_slice(&vc);
            comment.push(0x01); // framing bit
            Ok(vec![
                header.packets[0].clone(),
                comment,
                header.packets[2].clone(),
            ])
        }
        Codec::OggFlac => rebuild_oggflac_packets(header, &vc),
    }
}

/// Assemble a synthesized layout: regenerated header pages (Inline) + one compact
/// `OggAudio` segment whose `seq_delta` renumbers the preserved audio pages.
pub fn synthesize_layout(
    header: &OggHeader,
    audio_offset: u64,
    audio_length: u64,
    tags: &[TagInput],
) -> Result<RegionLayout> {
    let new_packets = rebuild_header_packets(header, tags)?;
    let refs: Vec<&[u8]> = new_packets.iter().map(|p| p.as_slice()).collect();
    let (header_bytes, new_pages) = crate::ogg::page::build_header(header.serial, &refs);
    let seq_delta = new_pages as i64 - header.header_pages as i64;
    Ok(RegionLayout::new(vec![
        Segment::Inline(header_bytes),
        Segment::OggAudio {
            offset: audio_offset,
            len: audio_length,
            seq_delta,
        },
    ]))
}
```

Add a stub for `rebuild_oggflac_packets` so Opus/Vorbis compile now (filled in Task 11):

```rust
fn rebuild_oggflac_packets(_header: &OggHeader, _vc: &[u8]) -> Result<Vec<Vec<u8>>> {
    Err(FormatError::Malformed)
}
```

Add the test (reuses `opus_headers`):

```rust
    #[test]
    fn synthesize_opus_emits_valid_header_and_audio_segment() {
        let mut data = opus_headers();
        let scan = locate_audio({
            let (audio, _) = crate::ogg::page::lace_packet(0x1234, 2, false, 960, &vec![0u8; 80]);
            data.extend_from_slice(&audio);
            &data
        })
        .unwrap();
        let header = read_metadata(&data[..scan.audio_offset as usize]).unwrap();

        let layout = synthesize_layout(
            &header,
            scan.audio_offset,
            scan.audio_length,
            &[TagInput::new("album", "Geogaddi")],
        )
        .unwrap();

        // Header segment is valid Ogg with regenerated tags; audio segment carries
        // the original bounds.
        match &layout.segments()[0] {
            Segment::Inline(bytes) => {
                let h = read_header(bytes).unwrap();
                assert_eq!(h.codec, Codec::Opus);
                let body = comment_body(Codec::Opus, &h.packets[1]).unwrap();
                let tags = crate::vorbiscomment::parse(body).unwrap();
                assert_eq!(tags, vec![("ALBUM".to_string(), "Geogaddi".to_string())]);
            }
            other => panic!("expected Inline header, got {other:?}"),
        }
        match &layout.segments()[1] {
            Segment::OggAudio { offset, len, .. } => {
                assert_eq!(*offset, scan.audio_offset);
                assert_eq!(*len, scan.audio_length);
            }
            other => panic!("expected OggAudio, got {other:?}"),
        }
    }
```

- [ ] **Step 3: Run, verify pass**

Run: `cargo test -p musefs-format ogg::tests::synthesize_opus_emits_valid_header_and_audio_segment && cargo test -p musefs-format layout`
Expected: PASS.

- [ ] **Step 4: Commit**

```bash
git add musefs-format/src/layout.rs musefs-format/src/ogg/mod.rs
git commit -m "feat(ogg): OggAudio segment + Opus text-tag synthesis"
```

---

## Task 10: Vorbis `synthesize_layout` (text tags)

**Files:**
- Modify: `musefs-format/src/ogg/mod.rs`

- [ ] **Step 1: Write the failing test**

Add to the test module a Vorbis fixture builder and test:

```rust
    fn vorbis_headers_with(setup: &[u8]) -> Vec<u8> {
        // Minimal-but-shaped Vorbis ID header (30 bytes from 0x01"vorbis").
        let mut id = b"\x01vorbis".to_vec();
        id.extend_from_slice(&0u32.to_le_bytes()); // version
        id.push(2); // channels
        id.extend_from_slice(&44100u32.to_le_bytes()); // sample rate
        id.extend_from_slice(&0u32.to_le_bytes()); // bitrate max
        id.extend_from_slice(&128000u32.to_le_bytes()); // nominal
        id.extend_from_slice(&0u32.to_le_bytes()); // min
        id.push(0xB8); // blocksizes
        id.push(0x01); // framing bit
        let mut comment = b"\x03vorbis".to_vec();
        comment.extend_from_slice(&crate::vorbiscomment::build(&[]));
        comment.push(0x01);
        let (bytes, _) = crate::ogg::page::build_header(55, &[&id, &comment, setup]);
        bytes
    }

    #[test]
    fn synthesize_vorbis_preserves_setup_and_rewrites_comment() {
        let setup = b"\x05vorbis-SETUP-CODEBOOKS-PLACEHOLDER".to_vec();
        let mut data = vorbis_headers_with(&setup);
        let (audio, _) = crate::ogg::page::lace_packet(55, 99, false, 1024, &vec![0u8; 64]);
        data.extend_from_slice(&audio);

        let scan = locate_audio(&data).unwrap();
        assert_eq!(scan.codec, Codec::Vorbis);
        let header = read_metadata(&data[..scan.audio_offset as usize]).unwrap();
        // The original setup packet (3rd header packet) must be carried through.
        assert_eq!(header.packets[2], setup);

        let layout = synthesize_layout(
            &header,
            scan.audio_offset,
            scan.audio_length,
            &[TagInput::new("artist", "Autechre")],
        )
        .unwrap();

        if let Segment::Inline(bytes) = &layout.segments()[0] {
            let h = read_header(bytes).unwrap();
            assert_eq!(h.codec, Codec::Vorbis);
            assert_eq!(h.packets[2], setup); // setup preserved byte-for-byte
            let body = comment_body(Codec::Vorbis, &h.packets[1]).unwrap();
            let tags = crate::vorbiscomment::parse(body).unwrap();
            assert_eq!(tags, vec![("ARTIST".to_string(), "Autechre".to_string())]);
        } else {
            panic!("expected Inline header");
        }
    }
```

Vorbis synthesis is already implemented by the `Codec::Vorbis` arm of `rebuild_header_packets` (Task 9). This task verifies it and locks the setup-preservation invariant.

- [ ] **Step 2: Run, verify pass**

Run: `cargo test -p musefs-format ogg::tests::synthesize_vorbis_preserves_setup_and_rewrites_comment`
Expected: PASS.

- [ ] **Step 3: Commit**

```bash
git add musefs-format/src/ogg/mod.rs
git commit -m "test(ogg): Vorbis synthesis preserves setup, rewrites comment"
```

---

## Task 11: OggFLAC `synthesize_layout` (text tags)

**Files:**
- Modify: `musefs-format/src/ogg/mod.rs`

- [ ] **Step 1: Implement `rebuild_oggflac_packets`**

Replace the stub from Task 9 with:

```rust
/// Rebuild OggFLAC header packets: keep packet 0 (mapping header `0x7F FLAC` +
/// version + count + `fLaC` + STREAMINFO) but recompute its 16-bit following-packet
/// count; carry over structural metadata-block packets (APPLICATION=2, SEEKTABLE=3,
/// CUESHEET=5); drop existing VORBIS_COMMENT/PICTURE/PADDING; append one fresh
/// VORBIS_COMMENT block. Set the last-metadata-block flag on the final block.
fn rebuild_oggflac_packets(header: &OggHeader, vc: &[u8]) -> Result<Vec<Vec<u8>>> {
    if header.packets.is_empty() {
        return Err(FormatError::Malformed);
    }
    // Structural blocks to keep (each block packet starts with the 4-byte FLAC
    // metadata block header; type is the low 7 bits of byte 0).
    let mut blocks: Vec<Vec<u8>> = Vec::new();
    for pkt in header.packets.iter().skip(1) {
        if pkt.is_empty() {
            continue;
        }
        match pkt[0] & 0x7F {
            2 | 3 | 5 => blocks.push(pkt.clone()), // APPLICATION, SEEKTABLE, CUESHEET
            _ => {}
        }
    }

    // Fresh VORBIS_COMMENT block (type 4): 4-byte header + body.
    let mut comment = Vec::new();
    crate::flac::push_block_header(&mut comment, 4, vc.len(), false);
    comment.extend_from_slice(vc);
    blocks.push(comment);

    // Normalize the last-metadata-block flag: clear on all but the last, set on the
    // last. Byte 0 high bit (0x80) is the flag.
    let n = blocks.len();
    for (i, b) in blocks.iter_mut().enumerate() {
        if i + 1 == n {
            b[0] |= 0x80;
        } else {
            b[0] &= 0x7F;
        }
    }

    // Rebuild the mapping header (packet 0) with the new following-packet count.
    let mut mapping = header.packets[0].clone();
    if mapping.len() < 9 {
        return Err(FormatError::Malformed);
    }
    let count = u16::try_from(blocks.len()).map_err(|_| FormatError::TooLarge)?;
    mapping[7..9].copy_from_slice(&count.to_be_bytes());

    let mut out = vec![mapping];
    out.extend(blocks);
    Ok(out)
}
```

- [ ] **Step 2: Write the failing test**

Add to the test module:

```rust
    fn oggflac_headers() -> Vec<u8> {
        // STREAMINFO block (type 0): 4-byte header + 34-byte body (zeros are fine
        // for our framing test).
        let mut streaminfo = Vec::new();
        crate::flac::push_block_header(&mut streaminfo, 0, 34, false);
        streaminfo.extend(std::iter::repeat(0u8).take(34));

        // Mapping header packet: 0x7F "FLAC" v1.0 count "fLaC" STREAMINFO.
        let mut mapping = vec![0x7F];
        mapping.extend_from_slice(b"FLAC");
        mapping.push(1);
        mapping.push(0);
        mapping.extend_from_slice(&2u16.to_be_bytes()); // count: SEEKTABLE + VORBIS_COMMENT
        mapping.extend_from_slice(b"fLaC");
        mapping.extend_from_slice(&streaminfo);

        // A SEEKTABLE block (type 3, structural — must be preserved).
        let mut seektable = Vec::new();
        crate::flac::push_block_header(&mut seektable, 3, 18, false);
        seektable.extend(std::iter::repeat(0xEEu8).take(18));

        // An existing VORBIS_COMMENT (type 4, last) to be replaced.
        let mut old_vc = Vec::new();
        let body = crate::vorbiscomment::build(&[crate::input::TagInput::new("x", "old")]);
        crate::flac::push_block_header(&mut old_vc, 4, body.len(), true);
        old_vc.extend_from_slice(&body);

        let (bytes, _) = crate::ogg::page::build_header(77, &[&mapping, &seektable, &old_vc]);
        bytes
    }

    #[test]
    fn synthesize_oggflac_keeps_seektable_replaces_comment_and_count() {
        let mut data = oggflac_headers();
        let (audio, _) = crate::ogg::page::lace_packet(77, 3, false, 4096, &vec![0u8; 64]);
        data.extend_from_slice(&audio);

        let scan = locate_audio(&data).unwrap();
        assert_eq!(scan.codec, Codec::OggFlac);
        let header = read_metadata(&data[..scan.audio_offset as usize]).unwrap();

        let layout = synthesize_layout(
            &header,
            scan.audio_offset,
            scan.audio_length,
            &[TagInput::new("title", "Kaini Industries")],
        )
        .unwrap();

        if let Segment::Inline(bytes) = &layout.segments()[0] {
            let h = read_header(bytes).unwrap();
            assert_eq!(h.codec, Codec::OggFlac);
            // packet 0 mapping count == number of following blocks (SEEKTABLE + VC == 2)
            assert_eq!(u16::from_be_bytes([h.packets[0][7], h.packets[0][8]]), 2);
            // SEEKTABLE preserved
            assert!(h.packets.iter().skip(1).any(|p| (p[0] & 0x7F) == 3));
            // exactly one VORBIS_COMMENT, with the new tag, flagged last
            let vc = h.packets.iter().skip(1).find(|p| (p[0] & 0x7F) == 4).unwrap();
            assert_eq!(vc[0] & 0x80, 0x80);
            let tags = crate::vorbiscomment::parse(&vc[4..]).unwrap();
            assert_eq!(tags, vec![("TITLE".to_string(), "Kaini Industries".to_string())]);
        } else {
            panic!("expected Inline header");
        }
    }
```

- [ ] **Step 3: Run, verify pass**

Run: `cargo test -p musefs-format ogg::tests::synthesize_oggflac_keeps_seektable_replaces_comment_and_count`
Expected: PASS.

- [ ] **Step 4: Commit**

```bash
git add musefs-format/src/ogg/mod.rs
git commit -m "feat(ogg): OggFLAC text-tag synthesis (mapping count + structural blocks)"
```

---

## Task 12: Page-header patching helper

**Files:**
- Modify: `musefs-format/src/ogg/page.rs`

- [ ] **Step 1: Write the failing test**

Append to `musefs-format/src/ogg/page.rs` (before the test module):

```rust
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
```

Add to the test module:

```rust
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
```

- [ ] **Step 2: Run, verify pass**

Run: `cargo test -p musefs-format ogg::page::tests::patch_page_header_updates_seq_and_crc`
Expected: PASS.

- [ ] **Step 3: Commit**

```bash
git add musefs-format/src/ogg/page.rs
git commit -m "feat(ogg): patch_page_header (renumber + CRC recompute)"
```

---

## Task 13: Core page-index builder

**Files:**
- Create: `musefs-core/src/ogg_index.rs`
- Modify: `musefs-core/src/lib.rs`
- Modify: `musefs-core/Cargo.toml`
- Modify: `musefs-format/src/ogg/page.rs` (re-export needed items)

- [ ] **Step 1: Make page primitives reachable from core**

In `musefs-format/src/ogg/mod.rs`, expose the page submodule's needed items publicly:

```rust
pub use page::{parse_page, patch_page_header, PageHeader};
```

Confirm `mod page;` becomes `pub use` of those names (the `mod page;` line stays private; only the re-exports are public).

- [ ] **Step 2: Add `once_cell` and wire the module**

In `musefs-core/Cargo.toml` `[dependencies]` add:

```toml
once_cell = "1"
```

In `musefs-core/src/lib.rs` add:

```rust
mod ogg_index;
```

- [ ] **Step 3: Write the failing test**

Create `musefs-core/src/ogg_index.rs`:

```rust
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
        let header = musefs_format::ogg::patch_page_header(&full, new_seq).map_err(CoreError::from)?;

        pages.push(IndexedPage {
            region_offset: consumed,
            header,
            payload_len: payload_len as u64,
        });
        consumed += old.total_len() as u64;
    }
    Ok(OggPageIndex { pages })
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
```

To give the core test a stable way to build Ogg pages, add a tiny public test-support shim in `musefs-format/src/ogg/mod.rs`:

```rust
#[doc(hidden)]
pub mod page_test_support {
    pub use crate::ogg::page::lace_packet as lace_packet_pub;
}
```

- [ ] **Step 4: Run, verify pass**

Run: `cargo test -p musefs-core ogg_index`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add musefs-core/src/ogg_index.rs musefs-core/src/lib.rs musefs-core/Cargo.toml musefs-format/src/ogg/mod.rs
git commit -m "feat(core): buffered sequential Ogg page-index builder"
```

---

## Task 14: Serve `OggAudio` in `read_at` with a concurrency-guarded lazy index

**Files:**
- Modify: `musefs-core/src/ogg_index.rs` (range server)
- Modify: `musefs-core/src/reader.rs`

- [ ] **Step 1: Add the range server to `ogg_index.rs`**

Append to `musefs-core/src/ogg_index.rs` (before the test module):

```rust
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
```

- [ ] **Step 2: Add the cached index to `ResolvedFile` and serve it**

In `musefs-core/src/reader.rs`:

1. Add imports near the top:

```rust
use once_cell::sync::OnceCell;
use crate::ogg_index::{build_index, serve, OggPageIndex};
```

2. Add a field to `ResolvedFile`:

```rust
    /// Lazily built on the first read that touches an `OggAudio` segment; guarded
    /// so concurrent first reads build it once. Empty for non-Ogg files.
    pub ogg_index: OnceCell<Arc<OggPageIndex>>,
```

3. At the two `ResolvedFile { ... }` construction sites in `resolve` (StructureOnly and Synthesis both flow into the single `Arc::new(ResolvedFile { ... })`), add `ogg_index: OnceCell::new(),` to the struct literal.

4. In `read_at`, add an arm to the `match seg` for `Segment::OggAudio`:

```rust
                Segment::OggAudio {
                    offset: ao,
                    seq_delta,
                    len,
                } => {
                    let index = resolved
                        .ogg_index
                        .get_or_try_init(|| {
                            build_index(&resolved.backing_path, *ao, *len, *seq_delta).map(Arc::new)
                        })?
                        .clone();
                    if backing.is_none() {
                        backing = Some(std::fs::File::open(&resolved.backing_path)?);
                    }
                    let f = backing.as_ref().unwrap();
                    serve(&index, f, *ao, within, within + n as u64, &mut out)?;
                }
```

(`within` is the offset into this segment, `n` the byte count — both already computed in the loop. `get_or_try_init` returns `Result`, so a transient I/O error is propagated and the slot stays empty for the next read to retry.)

- [ ] **Step 3: Write the failing test**

Add to `musefs-core/src/reader.rs` a test module (or extend the existing one):

```rust
#[cfg(test)]
mod ogg_serve_tests {
    use super::*;
    use musefs_format::ogg::page_test_support::lace_packet_pub;
    use musefs_format::Segment;
    use std::io::Write;

    #[test]
    fn read_at_renumbers_audio_and_preserves_payload() {
        // Build a file: 8 header bytes + two audio pages (seq 3,4).
        let (mut audio, _) = lace_packet_pub(0x99, 3, false, 10, &vec![0xA1u8; 200]);
        let (a2, _) = lace_packet_pub(0x99, 4, false, 20, &vec![0xB2u8; 250]);
        audio.extend_from_slice(&a2);
        let audio_offset = 8u64;
        let mut file_bytes = vec![0xFFu8; audio_offset as usize];
        file_bytes.extend_from_slice(&audio);

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.opus");
        std::fs::File::create(&path).unwrap().write_all(&file_bytes).unwrap();

        let layout = RegionLayout::new(vec![
            Segment::Inline(b"HDRBYTES".to_vec()), // 8 inline header bytes
            Segment::OggAudio {
                offset: audio_offset,
                len: audio.len() as u64,
                seq_delta: 1, // 3->4, 4->5
            },
        ]);
        let total = layout.total_len();
        let resolved = ResolvedFile {
            layout,
            total_len: total,
            content_version: 0,
            backing_path: path.clone(),
            mtime_secs: 0,
            ogg_index: OnceCell::new(),
        };

        // Read the whole virtual file; needs a Db only for ArtImage (unused here).
        let db = musefs_db::Db::open_in_memory().unwrap();
        let got = read_at(&resolved, &db, 0, total).unwrap();
        assert_eq!(got.len(), total as usize);
        assert_eq!(&got[0..8], b"HDRBYTES");

        // The served audio region must have renumbered seqs (4 and 5) and identical
        // payloads to the source.
        let served_audio = &got[8..];
        let h0 = musefs_format::ogg::parse_page(served_audio, 0).unwrap();
        assert_eq!(h0.seq, 4);
        let p1_off = h0.total_len();
        let h1 = musefs_format::ogg::parse_page(served_audio, p1_off).unwrap();
        assert_eq!(h1.seq, 5);
        // Payload bytes unchanged.
        assert!(served_audio[h0.header_len..h0.total_len()].iter().all(|&b| b == 0xA1));
        assert!(served_audio[p1_off + h1.header_len..p1_off + h1.total_len()].iter().all(|&b| b == 0xB2));
    }
}
```

If `Db::open_in_memory()` is not the exact constructor name, use the crate's in-memory/temp DB constructor (check `musefs-db/src/lib.rs` exports). The DB is only needed to satisfy `read_at`'s signature; no art is read in this test.

- [ ] **Step 4: Run, verify pass**

Run: `cargo test -p musefs-core ogg_serve_tests`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add musefs-core/src/ogg_index.rs musefs-core/src/reader.rs
git commit -m "feat(core): serve OggAudio via concurrency-guarded lazy page index"
```

---

## Task 15: Byte-bounded LRU header cache

**Files:**
- Modify: `musefs-core/src/reader.rs`

- [ ] **Step 1: Add a cache-byte cost to `ResolvedFile` and bound the cache**

In `musefs-core/src/reader.rs`:

1. Add a field to `ResolvedFile`:

```rust
    /// Approximate resident bytes this entry costs the cache (sum of `Inline`
    /// segment bytes; backing/art/ogg-audio bytes are not resident).
    pub cache_bytes: u64,
```

Set it where `ResolvedFile` is built, computing from the layout:

```rust
        let cache_bytes = layout
            .segments()
            .iter()
            .map(|s| match s {
                Segment::Inline(b) => b.len() as u64,
                _ => 0,
            })
            .sum();
```

Add `cache_bytes,` to the `resolve` struct literal. Then `grep -rn "ResolvedFile {" musefs-core` and add a `cache_bytes` value to **every** other literal too — in particular the `ogg_serve_tests` test added in Task 14 (use `cache_bytes: 8,`, matching its 8 inline header bytes). The `cache_bound_tests` literal below already sets it. (Compilation fails until all literals have the new field.)

2. Change `HeaderCache` to track a byte budget and LRU order:

```rust
pub struct HeaderCache {
    map: HashMap<i64, Arc<ResolvedFile>>,
    order: Vec<i64>, // LRU order, least-recent first
    bytes: u64,
    budget: u64,
    mode: Mode,
}

/// Default resident-bytes budget for the header cache (64 MiB).
pub const DEFAULT_CACHE_BUDGET: u64 = 64 * 1024 * 1024;
```

3. Update `HeaderCache::new` to take the budget (keep a convenience default):

```rust
    pub fn new(mode: Mode) -> HeaderCache {
        HeaderCache::with_budget(mode, DEFAULT_CACHE_BUDGET)
    }

    pub fn with_budget(mode: Mode, budget: u64) -> HeaderCache {
        HeaderCache {
            map: HashMap::new(),
            order: Vec::new(),
            bytes: 0,
            budget,
            mode,
        }
    }
```

4. Update `clear` to also reset accounting:

```rust
    pub fn clear(&mut self) {
        self.map.clear();
        self.order.clear();
        self.bytes = 0;
    }
```

5. Add helpers and update `resolve` to use them. Replace the cache-hit block and the final insert with LRU-aware logic:

```rust
    fn touch(&mut self, track_id: i64) {
        if let Some(pos) = self.order.iter().position(|&k| k == track_id) {
            self.order.remove(pos);
        }
        self.order.push(track_id);
    }

    fn evict_to_budget(&mut self) {
        while self.bytes > self.budget && self.order.len() > 1 {
            let victim = self.order.remove(0);
            if let Some(old) = self.map.remove(&victim) {
                self.bytes = self.bytes.saturating_sub(old.cache_bytes);
            }
        }
    }

    fn store(&mut self, track_id: i64, resolved: Arc<ResolvedFile>) {
        if let Some(old) = self.map.remove(&track_id) {
            self.bytes = self.bytes.saturating_sub(old.cache_bytes);
        }
        self.bytes += resolved.cache_bytes;
        self.map.insert(track_id, resolved);
        self.touch(track_id);
        self.evict_to_budget();
    }
```

In `resolve`, replace the cache-hit early-return:

```rust
        if let Some(cached) = self.map.get(&track_id) {
            if cached.content_version == track.content_version {
                let hit = cached.clone();
                self.touch(track_id);
                return Ok(hit);
            }
        }
```

And replace the final `self.map.insert(track_id, resolved.clone());` with:

```rust
        self.store(track_id, resolved.clone());
```

- [ ] **Step 2: Write the failing test**

Add a test module to `musefs-core/src/reader.rs`:

```rust
#[cfg(test)]
mod cache_bound_tests {
    use super::*;

    fn entry(bytes: u64) -> Arc<ResolvedFile> {
        Arc::new(ResolvedFile {
            layout: RegionLayout::new(vec![Segment::Inline(vec![0u8; bytes as usize])]),
            total_len: bytes,
            content_version: 0,
            backing_path: std::path::PathBuf::from("/dev/null"),
            mtime_secs: 0,
            ogg_index: once_cell::sync::OnceCell::new(),
            cache_bytes: bytes,
        })
    }

    #[test]
    fn evicts_least_recently_used_over_byte_budget() {
        let mut c = HeaderCache::with_budget(Mode::Synthesis, 250);
        c.store(1, entry(100));
        c.store(2, entry(100));
        c.touch(1); // 1 is now most-recent
        c.store(3, entry(100)); // total would be 300 > 250 => evict LRU (2)
        assert!(c.map.contains_key(&1));
        assert!(!c.map.contains_key(&2));
        assert!(c.map.contains_key(&3));
        assert!(c.bytes <= 250);
    }
}
```

- [ ] **Step 3: Run, verify pass**

Run: `cargo test -p musefs-core cache_bound_tests && cargo test -p musefs-core`
Expected: PASS. Fix any callers of `HeaderCache::new` that broke (none expected — signature preserved).

- [ ] **Step 4: Commit**

```bash
git add musefs-core/src/reader.rs
git commit -m "feat(core): byte-bounded LRU header cache"
```

---

## Task 16: `Format` variants for the Ogg codecs

**Files:**
- Modify: `musefs-db/src/models.rs`

- [ ] **Step 1: Write the failing test**

In `musefs-db/src/models.rs`, extend the `tests` module:

```rust
    #[test]
    fn ogg_codecs_round_trip() {
        for (f, s) in [
            (Format::Opus, "opus"),
            (Format::Vorbis, "vorbis"),
            (Format::OggFlac, "oggflac"),
        ] {
            assert_eq!(f.as_str(), s);
            assert_eq!(Format::parse(s), Some(f));
        }
    }
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p musefs-db ogg_codecs_round_trip`
Expected: FAIL — `Format::Opus` etc. do not exist.

- [ ] **Step 3: Add the variants**

In `musefs-db/src/models.rs`:

```rust
pub enum Format {
    Flac,
    Mp3,
    M4a,
    Opus,
    Vorbis,
    OggFlac,
}
```

Add arms to `as_str`:

```rust
            Format::Opus => "opus",
            Format::Vorbis => "vorbis",
            Format::OggFlac => "oggflac",
```

Add arms to `parse`:

```rust
            "opus" => Some(Format::Opus),
            "vorbis" => Some(Format::Vorbis),
            "oggflac" => Some(Format::OggFlac),
```

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test -p musefs-db`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add musefs-db/src/models.rs
git commit -m "feat(db): add Opus/Vorbis/OggFlac format variants"
```

---

## Task 17: Wire synthesis into `reader::resolve`

**Files:**
- Modify: `musefs-core/src/reader.rs`

- [ ] **Step 1: Add the resolve arms**

In `musefs-core/src/reader.rs`, inside the `Mode::Synthesis` `match track.format` block, add after the `Format::M4a` arm:

```rust
                    Format::Opus | Format::Vorbis | Format::OggFlac => {
                        let front =
                            read_front(Path::new(&track.backing_path), track.audio_offset as u64)?;
                        let header = musefs_format::ogg::read_metadata(&front)?;
                        musefs_format::ogg::synthesize_layout(
                            &header,
                            track.audio_offset as u64,
                            track.audio_length as u64,
                            &inputs,
                        )?
                    }
```

(`musefs_format::ogg::read_metadata` and `synthesize_layout` are already public from earlier tasks. `read_front` already exists in this file.)

- [ ] **Step 2: Write the failing test (end-to-end resolve over a real Opus file)**

Add a test module to `musefs-core/src/reader.rs`:

```rust
#[cfg(test)]
mod resolve_ogg_tests {
    use super::*;
    use musefs_db::{Db, Format, NewTrack, Tag};
    use musefs_format::ogg::page_test_support::lace_packet_pub;
    use std::io::Write;

    fn build_opus_file(path: &std::path::Path) -> (u64, u64) {
        let head = b"OpusHead\x01\x02\x38\x01\x80\xbb\x00\x00\x00\x00\x00".to_vec();
        let mut tags = b"OpusTags".to_vec();
        tags.extend_from_slice(&musefs_format::ogg::page_test_support::vorbis_body_empty());
        let (mut bytes, pages) =
            musefs_format::ogg::page_test_support::build_header_pub(0x1234, &[&head, &tags]);
        let audio_offset = bytes.len() as u64;
        let _ = pages;
        let (audio, _) = lace_packet_pub(0x1234, 2, false, 960, &vec![0x7Eu8; 400]);
        bytes.extend_from_slice(&audio);
        std::fs::File::create(path).unwrap().write_all(&bytes).unwrap();
        (audio_offset, bytes.len() as u64 - audio_offset)
    }

    #[test]
    fn resolves_and_reads_opus_with_identical_audio() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("track.opus");
        let (audio_offset, audio_length) = build_opus_file(&path);
        let original = std::fs::read(&path).unwrap();

        let db = Db::open_in_memory().unwrap();
        let meta = std::fs::metadata(&path).unwrap();
        let track_id = db
            .upsert_track(&NewTrack {
                backing_path: path.to_string_lossy().to_string(),
                format: Format::Opus,
                audio_offset: audio_offset as i64,
                audio_length: audio_length as i64,
                backing_size: meta.len() as i64,
                backing_mtime: mtime_secs(&meta),
            })
            .unwrap();
        db.replace_tags(track_id, &[Tag::new("title", "Telephasic Workshop", 0)])
            .unwrap();

        let mut cache = HeaderCache::new(Mode::Synthesis);
        let resolved = cache.resolve(&db, track_id).unwrap();
        let out = read_at(&resolved, &db, 0, resolved.total_len).unwrap();

        // The synthesized audio region (after the regenerated header) must be the
        // original audio pages with seqs preserved (delta 0 here) and byte-identical
        // payloads. The whole tail equals the original audio bytes when seq_delta==0.
        let header = musefs_format::ogg::read_header(&out).unwrap();
        let synth_audio = &out[header.audio_offset as usize..];
        assert_eq!(synth_audio, &original[audio_offset as usize..]);

        // Tags were rewritten.
        let tags = musefs_format::ogg::read_tags(&out).unwrap();
        assert!(tags.iter().any(|(k, v)| k == "TITLE" && v == "Telephasic Workshop"));
    }
}
```

To support the test, add two more shims to `page_test_support` in `musefs-format/src/ogg/mod.rs`:

```rust
#[doc(hidden)]
pub mod page_test_support {
    pub use crate::ogg::page::{build_header as build_header_pub, lace_packet as lace_packet_pub};

    /// An empty VorbisComment body (vendor + zero comments), for fixtures.
    pub fn vorbis_body_empty() -> Vec<u8> {
        crate::vorbiscomment::build(&[])
    }
}
```

(Replace the earlier `page_test_support` block from Task 13 with this expanded one. `lace_packet_pub` is preserved.)

Note: in this fixture the regenerated header may differ in page count from the original (musefs vendor string vs the fixture's), so `seq_delta` may be non-zero in general. This test deliberately uses an empty-comment original so the header page count matches and `seq_delta == 0`, making the audio tail byte-identical. Renumbering with non-zero delta is covered by Task 14.

- [ ] **Step 3: Run, verify pass**

Run: `cargo test -p musefs-core resolve_ogg_tests`
Expected: PASS.

- [ ] **Step 4: Commit**

```bash
git add musefs-core/src/reader.rs musefs-format/src/ogg/mod.rs
git commit -m "feat(core): resolve Opus/Vorbis/OggFLAC via ogg synthesis"
```

---

## Task 18: Wire the scanner

**Files:**
- Modify: `musefs-core/src/scan.rs`

- [ ] **Step 1: Add the extensions and probe arm**

In `musefs-core/src/scan.rs`:

1. Add `use musefs_format::ogg;` to the `musefs_format` import line (or a new `use`).

2. In `collect_audio`, extend the extension filter:

```rust
            && (has_ext(&path, "flac")
                || has_ext(&path, "mp3")
                || has_ext(&path, "m4a")
                || has_ext(&path, "m4b")
                || has_ext(&path, "ogg")
                || has_ext(&path, "oga")
                || has_ext(&path, "opus"))
```

3. In `probe`, add a branch before the final `else { None }`:

```rust
    } else if has_ext(path, "ogg") || has_ext(path, "oga") || has_ext(path, "opus") {
        let scan = ogg::locate_audio(bytes).ok()?;
        let format = match scan.codec {
            ogg::Codec::Opus => Format::Opus,
            ogg::Codec::Vorbis => Format::Vorbis,
            ogg::Codec::OggFlac => Format::OggFlac,
        };
        Some(Probed {
            format,
            audio_offset: scan.audio_offset,
            audio_length: scan.audio_length,
            tags: ogg::read_tags(bytes).unwrap_or_default(),
            pictures: ogg::read_pictures(bytes).unwrap_or_default(),
        })
```

- [ ] **Step 2: Write the failing test**

Add to `musefs-core/src/scan.rs` a test module:

```rust
#[cfg(test)]
mod ogg_probe_tests {
    use super::*;
    use musefs_format::ogg::page_test_support::{build_header_pub, lace_packet_pub, vorbis_body_empty};
    use std::io::Write;

    #[test]
    fn probe_detects_opus_and_seeds_tags() {
        let head = b"OpusHead\x01\x02\x38\x01\x80\xbb\x00\x00\x00\x00\x00".to_vec();
        let mut tags = b"OpusTags".to_vec();
        tags.extend_from_slice(&vorbis_body_empty());
        let (mut bytes, _) = build_header_pub(0x1234, &[&head, &tags]);
        let (audio, _) = lace_packet_pub(0x1234, 2, false, 960, &vec![0u8; 100]);
        bytes.extend_from_slice(&audio);

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("song.opus");
        std::fs::File::create(&path).unwrap().write_all(&bytes).unwrap();

        let probed = probe(&path, &bytes).expect("opus should probe");
        assert_eq!(probed.format, Format::Opus);
        assert_eq!(probed.audio_offset, (bytes.len() - audio.len()) as u64);
    }
}
```

If `probe`/`Probed` are private (they are), this in-file test module can access them directly.

- [ ] **Step 3: Run, verify pass**

Run: `cargo test -p musefs-core ogg_probe_tests`
Expected: PASS.

- [ ] **Step 4: Commit**

```bash
git add musefs-core/src/scan.rs
git commit -m "feat(core): scan/probe .ogg/.oga/.opus and seed tags + art"
```

---

## Task 19: End-to-end mount read-through for the three codecs

**Files:**
- Modify: `musefs-fuse/Cargo.toml`
- Create/Modify: `musefs-fuse/tests/ogg_read_through.rs`

- [ ] **Step 1: Add the validation dev-dependency**

In `musefs-fuse/Cargo.toml` `[dev-dependencies]` add:

```toml
ogg = "0.9"
```

- [ ] **Step 2: Write the e2e test (complete, no placeholder)**

Create `musefs-fuse/tests/ogg_read_through.rs`, mirroring the harness in `musefs-fuse/tests/mount.rs` (`Db::open_in_memory`, `scan_directory`, `Musefs::open`, `musefs_fuse::spawn`):

```rust
use std::collections::BTreeMap;

use musefs_core::{scan_directory, MountConfig, Musefs};

/// Generate a tiny tagged .opus via ffmpeg. Returns false (skip) if ffmpeg or the
/// libopus encoder is unavailable.
fn make_opus_fixture(path: &std::path::Path) -> bool {
    std::process::Command::new("ffmpeg")
        .args([
            "-f", "lavfi", "-i", "anullsrc=r=48000:cl=stereo",
            "-t", "0.2", "-c:a", "libopus",
            "-metadata", "title=Roygbiv", "-metadata", "artist=Boards",
            "-y",
        ])
        .arg(path)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
        && path.exists()
}

/// Read every Ogg packet's data. The `ogg` crate validates page CRCs while
/// reading, so a corrupt page makes `read_packet` error (panicking the test).
fn read_packets(bytes: &[u8]) -> Vec<Vec<u8>> {
    let mut rdr = ogg::PacketReader::new(std::io::Cursor::new(bytes.to_vec()));
    let mut out = Vec::new();
    while let Some(p) = rdr.read_packet().expect("valid Ogg pages (CRC ok)") {
        out.push(p.data);
    }
    out
}

fn find_one_file(root: &std::path::Path) -> std::path::PathBuf {
    for e in std::fs::read_dir(root).unwrap() {
        let e = e.unwrap();
        let p = e.path();
        if e.file_type().unwrap().is_dir() {
            return find_one_file(&p);
        }
        return p;
    }
    panic!("no file found under {root:?}");
}

fn config() -> MountConfig {
    MountConfig {
        template: "$artist/$title".to_string(),
        fallbacks: BTreeMap::new(),
        default_fallback: "Unknown".to_string(),
        mode: musefs_core::Mode::Synthesis,
    }
}

#[test]
#[ignore = "requires /dev/fuse + libfuse + ffmpeg; run with --ignored"]
fn opus_read_through_validates_pages_and_audio() {
    let backing = tempfile::tempdir().unwrap();
    let src = backing.path().join("in.opus");
    if !make_opus_fixture(&src) {
        eprintln!("ffmpeg/libopus unavailable; skipping");
        return;
    }
    let source_bytes = std::fs::read(&src).unwrap();

    let db = musefs_db::Db::open_in_memory().unwrap();
    scan_directory(&db, backing.path()).unwrap();
    let fs = Musefs::open(db, config()).unwrap();

    let mountpoint = tempfile::tempdir().unwrap();
    let session = musefs_fuse::spawn(fs, mountpoint.path(), "musefs-ogg-test").unwrap();

    let mounted_path = find_one_file(mountpoint.path());
    let mounted = std::fs::read(&mounted_path).unwrap();

    // 1. Pages well-formed: read_packets panics on any bad CRC.
    let mp = read_packets(&mounted);
    let sp = read_packets(&source_bytes);

    // 2. Header packets present and re-tagged.
    assert!(mp[0].starts_with(b"OpusHead"));
    assert!(mp[1].starts_with(b"OpusTags"));
    assert!(
        mp[1].windows(b"TITLE=Roygbiv".len()).any(|w| w == b"TITLE=Roygbiv"),
        "synthesized OpusTags should carry the rewritten title"
    );

    // 3. Audio packets (codec frames) byte-identical to the source — repagination
    //    changes page framing/sequence numbers only, never the audio packets.
    assert_eq!(mp.len(), sp.len());
    assert_eq!(&mp[2..], &sp[2..]);

    drop(session); // unmounts
    drop(backing);
}
```

- [ ] **Step 3: Run the e2e test**

Run: `cargo test -p musefs-fuse --test ogg_read_through -- --ignored`
Expected: PASS where `/dev/fuse`, libfuse, and ffmpeg(+libopus) are available; prints the skip notice and returns when ffmpeg is absent.

- [ ] **Step 4: Commit**

```bash
git add musefs-fuse/Cargo.toml musefs-fuse/tests/ogg_read_through.rs
git commit -m "test(ogg): e2e mount read-through with independent page validation"
```

---

## Task 20: Final verification

- [ ] **Step 1: Full build, lint, format, test**

Run:

```bash
cargo fmt --all
cargo clippy --all-targets
cargo test
```

Expected: clean format, no clippy warnings, all (non-ignored) tests pass across the workspace.

- [ ] **Step 2: Manual smoke (optional, requires a real Ogg file and FUSE)**

```bash
cargo run -p musefs-cli -- scan /path/to/ogg/dir --db /tmp/musefs.db
cargo run -p musefs-cli -- mount /tmp/mnt --db /tmp/musefs.db &
# Compare audio payloads: ffprobe / opusinfo on a mounted file; confirm tags reflect the DB
opusinfo /tmp/mnt/<rendered/path>.opus
```

- [ ] **Step 3: Commit any fmt/clippy fixups**

```bash
git add -A
git commit -m "chore(ogg): fmt + clippy cleanup"
```

---

## Plan 2 (next, separate plan)

Embedded cover art for the Ogg codecs is intentionally **out of this plan**. It will be its own spec-driven plan covering: the streaming `Segment::OggArt { art_id, encoding }` variant (base64 for Opus/Vorbis, raw for OggFLAC); base64-quantum (4-char / 3-byte) page-payload alignment so each art page streams independently; transient art materialization at resolve only to compute the art-page CRCs (dropped after), with `Arc`-interning by `art_id`; and re-embedding art into the regenerated header packets. Plan 1's byte-bounded cache and `OggArt`-free synthesis are designed to accept this addition without rework.

---

## Self-Review notes

- **Spec coverage:** detection (T16, T18), no schema migration (reuses `audio_offset`/`audio_length`), shared VorbisComment helpers (T5), page parse/lace/CRC (T1–T4, T12), per-codec text synthesis (T9–T11), `OggAudio` + analytic size (T9, layout `total_len`), lazy concurrency-guarded index + recompute-in-pass (T13–T14), byte-bounded cache (T15), wiring (T17–T18), e2e + independent validation (T19). Embedded art and the `OggArt` segment are explicitly deferred to Plan 2 per the agreed split.
- **Placeholders:** none. T19 carries the complete e2e test (real mount harness from `musefs-fuse/tests/mount.rs` + `ogg`-crate validation), skipping cleanly when ffmpeg/FUSE are unavailable.
- **Type consistency:** `read_header`/`read_metadata` return `OggHeader`; `locate_audio` returns `OggScan`; `synthesize_layout(&OggHeader, u64, u64, &[TagInput])`; `Segment::OggAudio { offset, len, seq_delta }`; `build_index(path, audio_offset, audio_length, seq_delta) -> OggPageIndex`; `serve(&OggPageIndex, &File, audio_offset, rstart, rend, &mut Vec<u8>)`. `ResolvedFile` gains `ogg_index: OnceCell<Arc<OggPageIndex>>` and `cache_bytes: u64` — both set at every construction site.
