# Ogg Container Support — Plan 2 (embedded cover art) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Re-embed cover art from the DB into synthesized Opus/Vorbis/FLAC-in-Ogg files, byte-faithful and decodable, **without holding art bytes in the cached layout** — served incrementally at read time so a full-library scan stays cheap on any backing filesystem.

**Architecture:** Synthesis builds the comment/PICTURE packet with the art included (Opus/Vorbis: a `METADATA_BLOCK_PICTURE=<base64>` comment; OggFLAC: a native PICTURE block), but laces it through a new **chunk-aware lacer** that materializes art only transiently to compute page CRCs, then emits `Segment::OggArtSlice { art_id, offset, len, base64, art_total }` for the art runs (no art bytes stored) and `Inline` for everything else. `read_at` serves an `OggArtSlice` by mapping the requested base64 window to a bounded raw-image input window (`db.read_art_chunk`), encoding, and trimming — O(requested bytes), constant memory. Picture-block prefixes are padded to a multiple of 3 so the image's base64 is an independent substring.

**Tech Stack:** Rust workspace (`musefs-format`, `musefs-core`, `musefs-db`, `musefs-fuse`), `base64` (already a `musefs-format` dep), `ogg`/`crc` (dev-only validation).

**Spec:** `docs/superpowers/specs/2026-05-26-ogg-container-support-design.md` (see "Embedded art (slice-based incremental streaming) — Plan 2").

**Builds on Plan 1 (merged to `main`).** Current relevant code:
- `musefs-format/src/ogg/mod.rs`: `synthesize_layout(header, audio_offset, audio_length, tags)`, `rebuild_header_packets(header, tags) -> Result<Vec<Vec<u8>>>`, `rebuild_oggflac_packets`, `Codec`, `OggHeader { codec, serial, packets, header_pages, audio_offset }`. `comment_body`, `read_tags`, `read_pictures`. `#[doc(hidden)] pub mod page_test_support { build_header_pub, lace_packet_pub, vorbis_body_empty }`.
- `musefs-format/src/ogg/page.rs`: `lacing_values` (private), `lace_packet`, `build_header`, `parse_page`, `crc32` (via `use super::crc::crc32`), `CAPTURE`, `FLAG_BOS`, `FLAG_CONTINUED`, `PageHeader`.
- `musefs-format/src/flac.rs`: `pub(crate) fn picture_body_framing(art: &ArtInput) -> Vec<u8>`? — currently **private** (`fn picture_body_framing`); this plan needs a padded variant in `ogg`, so it does not depend on flac's.
- `musefs-format/src/layout.rs`: `Segment { Inline, ArtImage, BackingAudio, OggAudio }`.
- `musefs-format/src/input.rs`: `ArtInput { art_id, mime, description, picture_type, width, height, data_len }`.
- `musefs-core/src/reader.rs`: `read_at` (segment loop with `within`/`n` locals), resolve arm `Format::Opus|Vorbis|OggFlac => { read_front; ogg::read_metadata; ogg::synthesize_layout(&header, audio_offset, audio_length, &inputs) }`. `inputs` = `tags_to_inputs(&tags)`. `art_inputs` via `track_art_to_inputs` is **not** currently used on the Ogg path.
- `musefs-core/src/mapping.rs`: `track_art_to_inputs(db, track_id) -> Result<Vec<ArtInput>>` (metadata only).
- `musefs-db`: `db.read_art_chunk(art_id, offset: u64, len: usize) -> Result<Vec<u8>>`, `db.get_art_meta(art_id)`.

---

## File Structure

**Modify:**
- `musefs-format/src/layout.rs` — add `Segment::OggArtSlice`.
- `musefs-format/src/ogg/page.rs` — make `lacing_values` `pub(crate)`; add the chunk-aware lacer (`PayloadChunk`, `lace_chunks_to_segments`).
- `musefs-format/src/ogg/b64.rs` — **new**: base64 window arithmetic + slice-encode helpers.
- `musefs-format/src/ogg/mod.rs` — `picture_prefix` (padded), `build_packets_with_art`, rewritten `synthesize_layout` signature (+ art), wire `mod b64;`, export helpers for core.
- `musefs-core/src/reader.rs` — `read_at` `OggArtSlice` arm; resolve passes art (id + bytes).
- `musefs-core/src/mapping.rs` — helper to read art image bytes for synthesis.
- `musefs-format/tests/*` and `musefs-core/tests/read_at.rs` — `Segment` match exhaustiveness for the new variant.
- `musefs-fuse/tests/ogg_read_through.rs` — e2e with embedded art.

---

## Task 1: base64 window arithmetic helpers

**Files:**
- Create: `musefs-format/src/ogg/b64.rs`
- Modify: `musefs-format/src/ogg/mod.rs` (add `mod b64;` and re-export)

- [ ] **Step 1: Create `musefs-format/src/ogg/b64.rs`**

```rust
//! Incremental base64 serving for embedded art: given a requested window of the
//! *output* base64 of an image, compute the bounded raw-input range to read and
//! how to trim the re-encoded result. base64 encodes each 3 input bytes into 4
//! output chars independently, so any output window `[o, o+len)` depends only on
//! input bytes `[⌊o/4⌋·3 .. ⌈(o+len)/4⌉·3)` (clipped to the image length, whose
//! final partial group yields the canonical `=` padding).

use base64::Engine;

/// The raw-input read plan for an output base64 window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct B64Window {
    /// First raw input byte to read.
    pub in_start: u64,
    /// Number of raw input bytes to read (clipped to the image length).
    pub in_len: u64,
    /// Leading base64 chars to drop after encoding the read bytes.
    pub skip: usize,
}

/// Compute the input read plan to serve output base64 chars `[out_offset,
/// out_offset+take)` of `base64(image)`, where the image is `img_total` bytes.
pub fn b64_window(out_offset: u64, take: u64, img_total: u64) -> B64Window {
    debug_assert!(take > 0);
    let g0 = out_offset / 4;
    let g1 = (out_offset + take - 1) / 4;
    let in_start = g0 * 3;
    let in_end = ((g1 + 1) * 3).min(img_total);
    B64Window {
        in_start,
        in_len: in_end.saturating_sub(in_start),
        skip: (out_offset - g0 * 4) as usize,
    }
}

/// Encode `raw` (the bytes named by a `B64Window`) and return exactly `take`
/// output chars starting at `skip`.
pub fn encode_b64_slice(raw: &[u8], skip: usize, take: usize) -> Vec<u8> {
    let enc = base64::engine::general_purpose::STANDARD.encode(raw);
    enc.as_bytes()[skip..skip + take].to_vec()
}

/// Total base64 output length for an image of `img_total` bytes.
pub fn b64_len(img_total: u64) -> u64 {
    img_total.div_ceil(3) * 4
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;

    fn full_b64(img: &[u8]) -> Vec<u8> {
        base64::engine::general_purpose::STANDARD
            .encode(img)
            .into_bytes()
    }

    #[test]
    fn any_window_matches_substring_of_full_encode() {
        // Cover image lengths that hit every length-mod-3 case and various windows.
        for &img_total in &[0u64, 1, 2, 3, 4, 5, 6, 7, 100, 257, 1024] {
            let img: Vec<u8> = (0..img_total).map(|i| (i * 7 + 3) as u8).collect();
            let full = full_b64(&img);
            assert_eq!(b64_len(img_total) as usize, full.len());
            if full.is_empty() {
                continue;
            }
            for o in 0..full.len() as u64 {
                for take in 1..=(full.len() as u64 - o) {
                    let w = b64_window(o, take, img_total);
                    let raw = &img[w.in_start as usize..(w.in_start + w.in_len) as usize];
                    let got = encode_b64_slice(raw, w.skip, take as usize);
                    assert_eq!(
                        got,
                        &full[o as usize..(o + take) as usize],
                        "img_total={img_total} o={o} take={take}"
                    );
                }
            }
        }
    }
}
```

- [ ] **Step 2: Wire the module**

In `musefs-format/src/ogg/mod.rs`, add near the top (with `mod crc; mod page;`):
```rust
mod b64;
pub use b64::{b64_len, b64_window, encode_b64_slice, B64Window};
```

- [ ] **Step 3: Run**

Run: `cargo test -p musefs-format ogg::b64`
Expected: PASS (`any_window_matches_substring_of_full_encode`). (The nested loop is bounded — max img length 1024.)

- [ ] **Step 4: Commit**

```bash
git add musefs-format/src/ogg/b64.rs musefs-format/src/ogg/mod.rs
git commit -m "feat(ogg): base64 window arithmetic for incremental art serving"
```

---

## Task 2: `Segment::OggArtSlice` variant

**Files:**
- Modify: `musefs-format/src/layout.rs`
- Modify: `musefs-format/tests/common/mod.rs`, `musefs-format/tests/mp3_synthesize.rs`, `musefs-format/tests/mp4_oracle.rs`, `musefs-core/tests/read_at.rs` (exhaustiveness — add arms only if those `match Segment` sites exist there; Plan 1 added `OggAudio` arms to the same files).

- [ ] **Step 1: Add the variant + `len` arm**

In `musefs-format/src/layout.rs`, add to `enum Segment` (after `OggAudio`):
```rust
    /// A run of an embedded picture's serialized bytes, served lazily from the art
    /// store (never stored in the layout). When `base64`, the run is `len` chars of
    /// `base64(image)` starting at output offset `offset`; otherwise it is `len`
    /// raw image bytes starting at raw offset `offset`. `art_total` is the raw image
    /// byte length (needed to clip the final base64 group).
    OggArtSlice {
        art_id: i64,
        offset: u64,
        len: u64,
        base64: bool,
        art_total: u64,
    },
```
In `impl Segment::len`, add:
```rust
            Segment::OggArtSlice { len, .. } => *len,
```
Do NOT change `RegionLayout::header_len` — `OggArtSlice` is part of the synthesized header region (it precedes the audio), so it should count toward `header_len` like `Inline` (only `BackingAudio`/`OggAudio` are excluded).

- [ ] **Step 2: Restore exhaustiveness in `Segment` matches outside the crate path**

Run `grep -rn "Segment::OggAudio" musefs-format/tests musefs-core` to find every `match`/test helper that enumerates `Segment` (Plan 1 added `OggAudio { .. } => unreachable!(...)` arms in `musefs-format/tests/{common/mod.rs,mp3_synthesize.rs,mp4_oracle.rs}`). In each such match, add an identical arm for the new variant:
```rust
            Segment::OggArtSlice { .. } => unreachable!("OggArtSlice only in ogg synthesis"),
```
(These test helpers synthesize FLAC/MP3/MP4 and never produce `OggArtSlice`, so `unreachable!` is correct.)

- [ ] **Step 3: Verify it compiles**

Run: `cargo build -p musefs-format && cargo test -p musefs-format layout`
Expected: compiles; `layout` tests pass. `cargo build` (workspace) will fail in `musefs-core`'s `read_at` (non-exhaustive) — that's fixed in Task 8. To keep this task green, add a TEMPORARY arm to `read_at`'s `match seg` in `musefs-core/src/reader.rs`:
```rust
                Segment::OggArtSlice { .. } => unreachable!("OggArtSlice serving lands in Task 8"),
```
Then `cargo build` (workspace) must succeed.

- [ ] **Step 4: Commit**

```bash
git add musefs-format/src/layout.rs musefs-format/tests musefs-core/src/reader.rs
git commit -m "feat(ogg): add OggArtSlice segment variant"
```

---

## Task 3: padded picture-block prefix builder

**Files:**
- Modify: `musefs-format/src/ogg/mod.rs`

- [ ] **Step 1: Write the failing test**

Add to `musefs-format/src/ogg/mod.rs` (above `#[cfg(test)]`):
```rust
/// Build the FLAC PICTURE block *body prefix* (everything before the image data:
/// type, mime, description, dimensions, depth, colors, data-length) for `art`,
/// padding the description with spaces so the prefix length is a multiple of 3.
/// This makes `base64(prefix ++ image) == base64(prefix) ++ base64(image)`, so the
/// image's base64 is an independent substring that can be served incrementally.
/// The declared data-length field is the true image length (`art.data_len`).
fn picture_prefix(art: &crate::input::ArtInput) -> Vec<u8> {
    // Unpadded prefix length = 4(type)+4(mimelen)+mime +4(desclen)+desc
    //   +4(w)+4(h)+4(depth)+4(colors)+4(datalen) = 32 + mime + desc.
    let base = 32 + art.mime.len() + art.description.len();
    let pad = (3 - base % 3) % 3;
    let description = format!("{}{}", art.description, " ".repeat(pad));

    let mut out = Vec::new();
    out.extend_from_slice(&art.picture_type.to_be_bytes());
    out.extend_from_slice(&(art.mime.len() as u32).to_be_bytes());
    out.extend_from_slice(art.mime.as_bytes());
    out.extend_from_slice(&(description.len() as u32).to_be_bytes());
    out.extend_from_slice(description.as_bytes());
    out.extend_from_slice(&art.width.to_be_bytes());
    out.extend_from_slice(&art.height.to_be_bytes());
    out.extend_from_slice(&0u32.to_be_bytes()); // depth
    out.extend_from_slice(&0u32.to_be_bytes()); // colors
    out.extend_from_slice(&(art.data_len as u32).to_be_bytes()); // image data length
    out
}
```

Add to the test module:
```rust
    #[test]
    fn picture_prefix_is_3_aligned_and_declares_image_len() {
        let art = crate::input::ArtInput {
            art_id: 1,
            mime: "image/png".to_string(), // 9 -> base = 32+9+0 = 41 -> pad 1
            description: String::new(),
            picture_type: 3,
            width: 1,
            height: 1,
            data_len: 12345,
        };
        let p = picture_prefix(&art);
        assert_eq!(p.len() % 3, 0);
        // datalen is the last 4 bytes (big-endian) and equals the true image length.
        let dl = u32::from_be_bytes(p[p.len() - 4..].try_into().unwrap());
        assert_eq!(dl, 12345);
        // Reusing the existing FLAC picture parser proves the framing is valid:
        // parse_picture_block expects the body (prefix + image); append dummy image.
        let mut body = p.clone();
        body.extend(std::iter::repeat_n(0u8, 12345));
        let pic = crate::flac::parse_picture_block(&body).unwrap();
        assert_eq!(pic.mime, "image/png");
        assert_eq!(pic.picture_type, 3);
    }
```

- [ ] **Step 2: Run**

Run: `cargo test -p musefs-format ogg::tests::picture_prefix_is_3_aligned_and_declares_image_len`
Expected: PASS. (`crate::flac::parse_picture_block` is `pub(crate)` from Plan 1.)

- [ ] **Step 3: Commit**

```bash
git add musefs-format/src/ogg/mod.rs
git commit -m "feat(ogg): padded picture-block prefix (3-aligned for streamable base64)"
```

---

## Task 4: chunk-aware lacer

**Files:**
- Modify: `musefs-format/src/ogg/page.rs`

- [ ] **Step 1: Expose `lacing_values` and add the lacer**

In `musefs-format/src/ogg/page.rs`, change `fn lacing_values` to `pub(crate) fn lacing_values`. Then add (above the `#[cfg(test)]` block):
```rust
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
        emit_segments(&mut segments, &page[..header_len], chunks, payload_pos, page_payload);

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
```

- [ ] **Step 2: Write the failing test**

Add to the `#[cfg(test)] mod tests` block in `page.rs`:
```rust
    use crate::layout::Segment;

    // Reconstruct the laced byte stream from segments, expanding OggArtSlice from a
    // provided art output map, so we can validate framing/CRCs end to end.
    fn flatten(segments: &[Segment], art_out: &[u8]) -> Vec<u8> {
        let mut v = Vec::new();
        for s in segments {
            match s {
                Segment::Inline(b) => v.extend_from_slice(b),
                Segment::OggArtSlice {
                    offset, len, base64, ..
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
```

- [ ] **Step 3: Run**

Run: `cargo test -p musefs-format ogg::page::tests::chunk_lacer_splits_art_across_pages_and_crcs_validate`
Expected: PASS.

- [ ] **Step 4: Commit**

```bash
git add musefs-format/src/ogg/page.rs
git commit -m "feat(ogg): chunk-aware lacer emitting OggArtSlice for art runs"
```

---

## Task 5: build packets with art (`build_packets_with_art`)

**Files:**
- Modify: `musefs-format/src/ogg/mod.rs`

- [ ] **Step 1: Add `build_packets_with_art`**

Add to `musefs-format/src/ogg/mod.rs` (above `#[cfg(test)]`). It returns one chunk list per header packet; packets without art are a single `Bytes` chunk (identical bytes to Plan 1's `rebuild_header_packets`), so text-only behavior is unchanged.
```rust
use crate::ogg::page::PayloadChunk;
use base64::Engine;

/// One image to embed: its metadata and raw bytes (read transiently at resolve).
pub struct OggArt<'a> {
    pub meta: &'a crate::input::ArtInput,
    pub image: &'a [u8],
}

fn b64_encode(bytes: &[u8]) -> Vec<u8> {
    base64::engine::general_purpose::STANDARD
        .encode(bytes)
        .into_bytes()
}

/// Build the regenerated header packets as chunk lists, embedding `arts`.
/// Opus/Vorbis: art goes into the comment packet as `METADATA_BLOCK_PICTURE`
/// comments (last). OggFLAC: each art is a native PICTURE block packet.
fn build_packets_with_art(
    header: &OggHeader,
    tags: &[TagInput],
    arts: &[OggArt],
) -> Result<Vec<Vec<PayloadChunk>>> {
    match header.codec {
        Codec::Opus => Ok(vec![
            vec![PayloadChunk::Bytes(header.packets[0].clone())],
            comment_packet_chunks(b"OpusTags", tags, arts, false),
        ]),
        Codec::Vorbis => Ok(vec![
            vec![PayloadChunk::Bytes(header.packets[0].clone())],
            comment_packet_chunks(b"\x03vorbis", tags, arts, true),
            vec![PayloadChunk::Bytes(header.packets[2].clone())],
        ]),
        Codec::OggFlac => oggflac_packets_with_art(header, tags, arts),
    }
}

/// Build a VorbisComment-style comment packet (Opus `OpusTags` / Vorbis `0x03vorbis`)
/// as chunks: a leading `Bytes` chunk (magic + vendor + count + text comments + each
/// art comment's framing and base64(prefix)), an `Art` chunk per image (base64 of
/// the image), and — for Vorbis — a trailing framing-bit `Bytes` chunk.
fn comment_packet_chunks(
    magic: &[u8],
    tags: &[TagInput],
    arts: &[OggArt],
    framing_bit: bool,
) -> Vec<PayloadChunk> {
    // Reuse the shared text-comment body, then we append art comments manually so
    // we control where the base64(image) run begins.
    let text_body = crate::vorbiscomment::build(tags); // vendor + count(text) + text comments
    // Patch the comment count to include the art comments. The count is the u32 LE
    // right after the vendor string: at offset 4 + vendor_len.
    let vendor_len =
        u32::from_le_bytes(text_body[0..4].try_into().unwrap()) as usize;
    let count_pos = 4 + vendor_len;
    let text_count = u32::from_le_bytes(
        text_body[count_pos..count_pos + 4].try_into().unwrap(),
    );
    let mut leading = text_body.clone();
    let new_count = text_count + arts.len() as u32;
    leading[count_pos..count_pos + 4].copy_from_slice(&new_count.to_le_bytes());

    let mut chunks: Vec<PayloadChunk> = Vec::new();
    let mut head = magic.to_vec();
    head.extend_from_slice(&leading);

    const KEY: &[u8] = b"METADATA_BLOCK_PICTURE=";
    for (i, art) in arts.iter().enumerate() {
        let prefix = picture_prefix(art.meta);
        let b64_prefix = b64_encode(&prefix);
        let value_len = KEY.len() + b64_prefix.len() + b64_len(art.meta.data_len) as usize;
        // comment = u32 LE length, then KEY, then base64(prefix), then base64(image).
        head.extend_from_slice(&(value_len as u32).to_le_bytes());
        head.extend_from_slice(KEY);
        head.extend_from_slice(&b64_prefix);
        chunks.push(PayloadChunk::Bytes(std::mem::take(&mut head)));
        chunks.push(PayloadChunk::Art {
            art_id: art.meta.art_id,
            out: b64_encode(art.image),
            base64: true,
            art_total: art.meta.data_len,
        });
        // `head` is empty now; the next art (or framing bit) appends to it.
        let _ = i;
    }
    if framing_bit {
        head.push(0x01);
    }
    if !head.is_empty() {
        chunks.push(PayloadChunk::Bytes(head));
    }
    chunks
}
```

Note: `picture_prefix` (Task 3) declares `data_len` as the true image length, and the `Art` chunk's `out = base64(image)` has length `b64_len(data_len)`. Because the prefix length is a multiple of 3, `base64(prefix) ++ base64(image) == base64(prefix ++ image)`, so the on-wire comment value is a valid `METADATA_BLOCK_PICTURE`.

- [ ] **Step 2: Add the OggFLAC art-packet builder**

Add to `musefs-format/src/ogg/mod.rs`:
```rust
/// OggFLAC header packets with art: the text comment packet (no art) plus one
/// native PICTURE block packet per image. Each PICTURE packet = 4-byte FLAC block
/// header + picture-block body prefix (raw) + an `Art` chunk for the raw image.
/// The last metadata-block packet carries the last-block flag, and packet 0's
/// 16-bit following-packet count is recomputed.
fn oggflac_packets_with_art(
    header: &OggHeader,
    tags: &[TagInput],
    arts: &[OggArt],
) -> Result<Vec<Vec<PayloadChunk>>> {
    if header.packets.is_empty() {
        return Err(FormatError::Malformed);
    }
    // Structural blocks to keep (APPLICATION=2, SEEKTABLE=3, CUESHEET=5).
    let mut structural: Vec<Vec<u8>> = Vec::new();
    for pkt in header.packets.iter().skip(1) {
        if !pkt.is_empty() && matches!(pkt[0] & 0x7F, 2 | 3 | 5) {
            structural.push(pkt.clone());
        }
    }

    // Fresh VORBIS_COMMENT block (type 4).
    let vc = crate::vorbiscomment::build(tags);
    let mut comment = Vec::new();
    crate::flac::push_block_header(&mut comment, 4, vc.len(), false);
    comment.extend_from_slice(&vc);

    // The metadata-block packets in order: structural..., VORBIS_COMMENT, PICTURE×N.
    // Each PICTURE block body = prefix (raw, for OggFLAC no 3-padding needed but the
    // padded prefix is still valid) + image. We split into [Bytes(header+prefix)],
    // [Art(raw image)].
    let following_count = structural.len() + 1 + arts.len();
    let count = u16::try_from(following_count).map_err(|_| FormatError::TooLarge)?;

    // Build the list of block packets as chunk lists, tracking which is last so we
    // can set the last-metadata-block flag on its FIRST byte.
    let mut block_packets: Vec<Vec<PayloadChunk>> = Vec::new();
    for s in &structural {
        block_packets.push(vec![PayloadChunk::Bytes(s.clone())]);
    }
    block_packets.push(vec![PayloadChunk::Bytes(comment)]);
    for art in arts {
        let prefix = picture_prefix(art.meta); // body prefix; data_len declared
        let body_len = prefix.len() as u64 + art.meta.data_len;
        if body_len > 0x00FF_FFFF {
            return Err(FormatError::TooLarge);
        }
        let mut blk = Vec::new();
        crate::flac::push_block_header(&mut blk, 6, body_len as usize, false);
        blk.extend_from_slice(&prefix);
        block_packets.push(vec![
            PayloadChunk::Bytes(blk),
            PayloadChunk::Art {
                art_id: art.meta.art_id,
                out: art.image.to_vec(),
                base64: false,
                art_total: art.meta.data_len,
            },
        ]);
    }

    // Set the last-metadata-block flag (0x80) on the first byte of the last block
    // packet's leading Bytes chunk; clear it on all others.
    let n = block_packets.len();
    for (i, bp) in block_packets.iter_mut().enumerate() {
        if let Some(PayloadChunk::Bytes(b)) = bp.first_mut() {
            if i + 1 == n {
                b[0] |= 0x80;
            } else {
                b[0] &= 0x7F;
            }
        }
    }

    // Mapping header (packet 0) with recomputed following count.
    let mut mapping = header.packets[0].clone();
    if mapping.len() < 9 {
        return Err(FormatError::Malformed);
    }
    mapping[7..9].copy_from_slice(&count.to_be_bytes());

    let mut out = vec![vec![PayloadChunk::Bytes(mapping)]];
    out.extend(block_packets);
    Ok(out)
}
```

- [ ] **Step 3: Build check (no test yet — exercised in Task 6)**

Run: `cargo build -p musefs-format`
Expected: compiles. (`build_packets_with_art`/`OggArt` may be unused until Task 6 — `dead_code` warnings are acceptable here; do not add `#[allow]`.)

- [ ] **Step 4: Commit**

```bash
git add musefs-format/src/ogg/mod.rs
git commit -m "feat(ogg): build header packets with embedded art as payload chunks"
```

---

## Task 6: rewire `synthesize_layout` to embed art

**Files:**
- Modify: `musefs-format/src/ogg/mod.rs`

- [ ] **Step 1: Replace `synthesize_layout` and retire `rebuild_header_packets`/`rebuild_oggflac_packets`**

Replace the body of `synthesize_layout` and DELETE `rebuild_header_packets` and `rebuild_oggflac_packets` (their logic now lives in `build_packets_with_art`/`oggflac_packets_with_art`):
```rust
/// Assemble a synthesized layout: regenerated header pages (with embedded art as
/// `OggArtSlice` runs) + one compact `OggAudio` segment renumbering the preserved
/// audio pages. `arts` carries each embedded image's metadata + raw bytes (used
/// transiently to compute page CRCs; not retained in the layout).
pub fn synthesize_layout(
    header: &OggHeader,
    audio_offset: u64,
    audio_length: u64,
    tags: &[TagInput],
    arts: &[OggArt],
) -> Result<RegionLayout> {
    let packet_chunks = build_packets_with_art(header, tags, arts)?;
    let mut segments: Vec<Segment> = Vec::new();
    let mut seq = 0u32;
    for (i, chunks) in packet_chunks.iter().enumerate() {
        let (segs, used) =
            crate::ogg::page::lace_chunks_to_segments(header.serial, seq, i == 0, chunks);
        segments.extend(segs);
        seq += used;
    }
    let seq_delta = seq as i64 - header.header_pages as i64;
    segments.push(Segment::OggAudio {
        offset: audio_offset,
        len: audio_length,
        seq_delta,
    });
    Ok(RegionLayout::new(segments))
}
```

- [ ] **Step 2: Update existing ogg synthesis tests to the new signature**

The Plan 1 tests call `synthesize_layout(&header, off, len, &[tag])`. Update each call to pass an empty art slice: `synthesize_layout(&header, off, len, &[tag], &[])`. Affected tests in `musefs-format/src/ogg/mod.rs`: `synthesize_opus_emits_valid_header_and_audio_segment`, `synthesize_vorbis_preserves_setup_and_rewrites_comment`, `synthesize_oggflac_keeps_seektable_replaces_comment_and_count`. (Behaviour is unchanged with no art: each packet is a single `Bytes` chunk, so the chunk lacer reproduces Plan 1's bytes — these tests must still pass, proving the refactor is behaviour-preserving.)

- [ ] **Step 3: Add an art round-trip test**

Add to the test module:
```rust
    #[test]
    fn synthesize_opus_embeds_art_that_round_trips() {
        // Build an Opus header with audio.
        let mut data = opus_headers();
        let header_len = data.len();
        let (audio, _) = crate::ogg::page::lace_packet(0x1234, 2, false, 960, &vec![0u8; 80]);
        data.extend_from_slice(&audio);
        let scan = locate_audio(&data).unwrap();
        let header = read_metadata(&data[..scan.audio_offset as usize]).unwrap();

        let image: Vec<u8> = (0..5000u32).map(|i| (i % 251) as u8).collect();
        let meta = crate::input::ArtInput {
            art_id: 7,
            mime: "image/jpeg".to_string(),
            description: String::new(),
            picture_type: 3,
            width: 64,
            height: 64,
            data_len: image.len() as u64,
        };
        let layout = synthesize_layout(
            &header,
            scan.audio_offset,
            scan.audio_length,
            &[TagInput::new("title", "Cover")],
            &[OggArt { meta: &meta, image: &image }],
        )
        .unwrap();
        let _ = header_len;

        // Materialize the header region from the layout, expanding OggArtSlice by
        // re-deriving its bytes from `image` (mirrors what read_at does).
        let mut bytes = Vec::new();
        for s in layout.segments() {
            match s {
                Segment::Inline(b) => bytes.extend_from_slice(b),
                Segment::OggArtSlice { offset, len, base64, art_total, .. } => {
                    assert!(*base64);
                    let w = b64_window(*offset, *len, *art_total);
                    let raw = &image[w.in_start as usize..(w.in_start + w.in_len) as usize];
                    bytes.extend_from_slice(&encode_b64_slice(raw, w.skip, *len as usize));
                }
                Segment::OggAudio { .. } => break, // header region ends here
                other => panic!("unexpected {other:?}"),
            }
        }

        // The materialized header must be a valid Opus header whose extracted
        // picture equals the original image.
        let pics = read_pictures(&bytes).unwrap();
        assert_eq!(pics.len(), 1);
        assert_eq!(pics[0].mime, "image/jpeg");
        assert_eq!(pics[0].data, image);
        // And every page CRC must validate.
        let h = read_header(&bytes).unwrap();
        assert_eq!(h.codec, Codec::Opus);
    }
```

- [ ] **Step 4: Run**

Run: `cargo test -p musefs-format ogg::`
Expected: PASS — the three updated text tests, the new `synthesize_opus_embeds_art_that_round_trips`, and all other ogg tests. (`read_pictures` decodes the `METADATA_BLOCK_PICTURE` and `parse_picture_block` strips the padded prefix; the image data matches because the declared `datalen` is the true length.)

- [ ] **Step 5: Commit**

```bash
git add musefs-format/src/ogg/mod.rs
git commit -m "feat(ogg): synthesize_layout embeds art via OggArtSlice (round-trips)"
```

---

## Task 7: resolve reads art bytes and passes them

**Files:**
- Modify: `musefs-core/src/mapping.rs`
- Modify: `musefs-core/src/reader.rs`

- [ ] **Step 1: Add an art-image reader to `mapping.rs`**

Add to `musefs-core/src/mapping.rs`:
```rust
/// Read each embedded image's raw bytes for synthesis (Ogg needs the bytes to
/// compute page CRCs at resolve). Parallel to `track_art_to_inputs`; returns the
/// same order. Only the Ogg synthesis path calls this — FLAC/MP3/MP4 stream art
/// via `ArtImage` and never materialize it.
pub(crate) fn track_art_images(db: &Db, inputs: &[ArtInput]) -> Result<Vec<Vec<u8>>> {
    let mut out = Vec::with_capacity(inputs.len());
    for a in inputs {
        out.push(db.read_art_chunk(a.art_id, 0, a.data_len as usize)?);
    }
    Ok(out)
}
```
Ensure `ArtInput` is imported in `mapping.rs` (it is — `track_art_to_inputs` uses it) and `track_art_images` is exported to `reader` (same crate, `pub(crate)`).

- [ ] **Step 2: Update the Ogg resolve arm**

In `musefs-core/src/reader.rs`, the `Format::Opus | Format::Vorbis | Format::OggFlac` arm currently builds the layout from `&inputs` (tags) only. Change it to also read art and pass it:
```rust
                    Format::Opus | Format::Vorbis | Format::OggFlac => {
                        let front =
                            read_front(Path::new(&track.backing_path), track.audio_offset as u64)?;
                        let header = musefs_format::ogg::read_metadata(&front)?;
                        let art_inputs = track_art_to_inputs(db, track_id)?;
                        let art_images = crate::mapping::track_art_images(db, &art_inputs)?;
                        let arts: Vec<musefs_format::ogg::OggArt> = art_inputs
                            .iter()
                            .zip(art_images.iter())
                            .map(|(meta, image)| musefs_format::ogg::OggArt {
                                meta,
                                image: image.as_slice(),
                            })
                            .collect();
                        musefs_format::ogg::synthesize_layout(
                            &header,
                            track.audio_offset as u64,
                            track.audio_length as u64,
                            &inputs,
                            &arts,
                        )?
                    }
```
Add the needed imports to `reader.rs`: `use crate::mapping::{tags_to_inputs, track_art_to_inputs};` already exists for `tags_to_inputs`/`track_art_to_inputs`; ensure `track_art_to_inputs` is imported (it is used elsewhere) and `musefs_format::ogg::OggArt` resolves (it is `pub`). The `art_images` `Vec<Vec<u8>>` must outlive the `arts` borrows — it does (both are locals in this block, `arts` borrows `art_images`, and `synthesize_layout` is called before the block ends).

- [ ] **Step 3: Verify the existing resolve test still passes**

Run: `cargo build -p musefs-core` then `cargo test -p musefs-core resolve_ogg`
Expected: compiles; `resolves_and_reads_opus_with_identical_audio` (Plan 1, no art on that track) still passes. The workspace won't fully serve `OggArtSlice` until Task 8 (the temporary `unreachable!` arm from Task 2 is still in place), but resolve + that test don't hit art.

- [ ] **Step 4: Commit**

```bash
git add musefs-core/src/mapping.rs musefs-core/src/reader.rs
git commit -m "feat(core): resolve reads art bytes and passes them to ogg synthesis"
```

---

## Task 8: serve `OggArtSlice` in `read_at`

**Files:**
- Modify: `musefs-core/src/reader.rs`

- [ ] **Step 1: Replace the temporary arm with real serving**

In `musefs-core/src/reader.rs` `read_at`, replace
```rust
                Segment::OggArtSlice { .. } => unreachable!("OggArtSlice serving lands in Task 8"),
```
with:
```rust
                Segment::OggArtSlice {
                    art_id,
                    offset,
                    base64,
                    art_total,
                    ..
                } => {
                    if *base64 {
                        // Output base64 chars [offset+within, +n) of base64(image).
                        let w = musefs_format::ogg::b64_window(*offset + within, n as u64, *art_total);
                        let raw = db.read_art_chunk(*art_id, w.in_start, w.in_len as usize)?;
                        out.extend_from_slice(&musefs_format::ogg::encode_b64_slice(
                            &raw, w.skip, n,
                        ));
                    } else {
                        // Raw image bytes (OggFLAC PICTURE block).
                        let chunk = db.read_art_chunk(*art_id, *offset + within, n)?;
                        out.extend_from_slice(&chunk);
                    }
                }
```
(`musefs_format::ogg::{b64_window, encode_b64_slice}` are `pub` from Task 1.)

- [ ] **Step 2: Write the failing test**

Add to the `ogg_serve_tests` module (or a new module) in `musefs-core/src/reader.rs`:
```rust
    #[test]
    fn read_at_serves_base64_art_slice_matching_full_encode() {
        use base64::Engine as _;
        // A synthetic layout: 4 inline bytes, then a base64 art slice covering the
        // whole image's base64, then 2 inline bytes. Read the whole thing and
        // confirm the art region equals base64(image).
        let image: Vec<u8> = (0..1000u32).map(|i| (i % 251) as u8).collect();
        let full_b64 = base64::engine::general_purpose::STANDARD
            .encode(&image)
            .into_bytes();

        let dir = tempfile::tempdir().unwrap();
        let db = musefs_db::Db::open_in_memory().unwrap();
        let art_id = db
            .upsert_art(&musefs_db::NewArt {
                mime: "image/png".to_string(),
                width: Some(1),
                height: Some(1),
                data: image.clone(),
            })
            .unwrap();
        let _ = dir;

        let layout = RegionLayout::new(vec![
            Segment::Inline(b"HEAD".to_vec()),
            Segment::OggArtSlice {
                art_id,
                offset: 0,
                len: full_b64.len() as u64,
                base64: true,
                art_total: image.len() as u64,
            },
            Segment::Inline(b"XY".to_vec()),
        ]);
        let total = layout.total_len();
        let resolved = ResolvedFile {
            layout,
            total_len: total,
            content_version: 0,
            backing_path: std::path::PathBuf::from("/dev/null"),
            mtime_secs: 0,
            ogg_index: once_cell::sync::OnceCell::new(),
            cache_bytes: 0,
        };

        // Full read.
        let got = read_at(&resolved, &db, 0, total).unwrap();
        let mut want = b"HEAD".to_vec();
        want.extend_from_slice(&full_b64);
        want.extend_from_slice(b"XY");
        assert_eq!(got, want);

        // Partial read straddling into the middle of the art slice (exercises the
        // window arithmetic at a non-4-aligned start).
        let part = read_at(&resolved, &db, 7, 23).unwrap();
        assert_eq!(part, want[7..30]);
    }
```
(`db.upsert_art` + `musefs_db::NewArt` are used by the scanner in Plan 1; confirm field names via `musefs-db` — `NewArt { mime, width, height, data }`.)

- [ ] **Step 3: Run**

Run: `cargo build` (workspace) then `cargo test -p musefs-core`
Expected: compiles (temporary arm gone); all core tests pass incl. `read_at_serves_base64_art_slice_matching_full_encode`.

- [ ] **Step 4: Commit**

```bash
git add musefs-core/src/reader.rs
git commit -m "feat(core): serve OggArtSlice (incremental base64 window / raw)"
```

---

## Task 9: end-to-end embedded-art mount test

**Files:**
- Modify: `musefs-fuse/tests/ogg_read_through.rs`

- [ ] **Step 1: Add an art-embedding e2e test**

The Plan-1 e2e (`mount_and_validate`, `make_fixture`, `read_packets`, `find_one_file`, `config`) is already in this file. Add a fixture that attaches a cover image and a validation that the synthesized file's embedded art decodes to the original.

Add near `make_fixture`:
```rust
/// Generate a fixture with an attached cover image (a tiny PNG) via ffmpeg.
/// Returns the cover bytes if encoding succeeded, else None (skip).
fn make_fixture_with_cover(
    dir: &std::path::Path,
    audio_name: &str,
    codec_args: &[&str],
) -> Option<(std::path::PathBuf, Vec<u8>)> {
    // 1x1 PNG.
    let png: &[u8] = &[
        0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1F,
        0x15, 0xC4, 0x89, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9C, 0x63, 0x00,
        0x01, 0x00, 0x00, 0x05, 0x00, 0x01, 0x0D, 0x0A, 0x2D, 0xB4, 0x00, 0x00, 0x00, 0x00, 0x49,
        0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
    ];
    let cover = dir.join("cover.png");
    std::fs::write(&cover, png).ok()?;
    let out = dir.join(audio_name);
    // anullsrc audio + attached cover picture.
    let mut cmd = std::process::Command::new("ffmpeg");
    cmd.args(["-f", "lavfi", "-i", "anullsrc=r=48000:cl=stereo", "-t", "0.3"]);
    cmd.args(["-i"]);
    cmd.arg(&cover);
    cmd.args(["-map", "0:a", "-map", "1:v"]);
    cmd.args(codec_args);
    cmd.args(["-metadata", "title=Cover", "-disposition:v", "attached_pic", "-y"]);
    cmd.arg(&out)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    let ok = cmd.status().map(|s| s.success()).unwrap_or(false) && out.exists();
    if ok {
        Some((out, png.to_vec()))
    } else {
        None
    }
}
```

Add the test:
```rust
#[test]
#[ignore = "requires /dev/fuse + libfuse + ffmpeg; run with --ignored"]
fn opus_read_through_preserves_embedded_art() {
    let backing = tempfile::tempdir().unwrap();
    let Some((src, _cover)) = make_fixture_with_cover(backing.path(), "in.opus", &["-c:a", "libopus"])
    else {
        eprintln!("ffmpeg/libopus unavailable; skipping");
        return;
    };

    // The source's own embedded art (as the scanner will ingest it).
    let source_bytes = std::fs::read(&src).unwrap();
    let src_pics = musefs_format::ogg::read_pictures(&source_bytes).unwrap();
    assert!(!src_pics.is_empty(), "fixture should carry a cover");

    let db = musefs_db::Db::open_in_memory().unwrap();
    musefs_core::scan_directory(&db, backing.path()).unwrap();
    let fs = musefs_core::Musefs::open(db, config()).unwrap();
    let mountpoint = tempfile::tempdir().unwrap();
    let session = musefs_fuse::spawn(fs, mountpoint.path(), "musefs-ogg-art").unwrap();

    let mounted = std::fs::read(find_one_file(mountpoint.path())).unwrap();
    // All pages valid (read_packets panics on bad CRC).
    let _ = read_packets(&mounted);
    // The synthesized file carries the same image bytes as the source.
    let out_pics = musefs_format::ogg::read_pictures(&mounted).unwrap();
    assert_eq!(out_pics.len(), 1);
    assert_eq!(out_pics[0].data, src_pics[0].data);

    drop(session);
}
```

- [ ] **Step 2: Run**

Run: `cargo test -p musefs-fuse --test ogg_read_through -- --ignored`
Expected (this env has /dev/fuse + ffmpeg): PASS, including `opus_read_through_preserves_embedded_art`. If ffmpeg lacks libopus or the attached-pic mux fails, the test prints the skip notice and returns. If the embedded-art assertion FAILS after a successful encode, that is a real synthesis bug — report it; do not weaken the assertion.

- [ ] **Step 3: Commit**

```bash
git add musefs-fuse/tests/ogg_read_through.rs
git commit -m "test(ogg): e2e mount preserves embedded cover art (Opus)"
```

---

## Task 10: final verification

- [ ] **Step 1: Format, lint, full test**

```bash
cargo fmt --all
cargo clippy --all-targets
cargo test
```
Expected: fmt clean; clippy **0 warnings/0 errors** (fix any — e.g. `needless_borrow`, `useless_vec` in new code; do not `#[allow]`); all non-ignored tests pass. Confirm no `dead_code` remains (e.g. an unused `b64_len` — it is used by `comment_packet_chunks`; `OggArt`/`build_packets_with_art` are used by `synthesize_layout`).

- [ ] **Step 2: Run the ignored e2e suite once (clean run)**

```bash
cargo test -p musefs-fuse --test ogg_read_through -- --ignored --test-threads=1
```
Expected: all e2e tests pass (Opus/Vorbis/OggFLAC read-through from Plan 1 + the new embedded-art test). `--test-threads=1` avoids concurrent-mount flakiness.

- [ ] **Step 3: Commit any fixups**

```bash
git add -A
git commit -m "chore(ogg): plan 2 fmt + clippy cleanup"
```

---

## Self-Review notes

- **Spec coverage:** `OggArtSlice` segment (T2); slice-based incremental streaming with read-time window arithmetic (T1, T8); no art bytes in cached layout — synthesis materializes transiently only to CRC (T4, T6); picture-prefix 3-padding so the image base64 is an independent substring (T3); Opus/Vorbis `METADATA_BLOCK_PICTURE` + OggFLAC native PICTURE (T5); resolve reads art transiently (T7); normal lacing, no padding comment, no constrained page sizes (T4 reuses `lacing_values`); e2e preserves embedded art byte-for-byte (T9). Multiple images per track are supported (the chunk list and `arts` slice iterate N images).
- **Placeholders:** none. Every step has complete code; no `todo!()`.
- **Type consistency:** `Segment::OggArtSlice { art_id: i64, offset: u64, len: u64, base64: bool, art_total: u64 }` used identically in layout/lacer/read_at; `OggArt<'a> { meta: &ArtInput, image: &[u8] }`; `synthesize_layout(header, audio_offset, audio_length, tags, arts: &[OggArt])`; `b64_window(out_offset, take, img_total) -> B64Window { in_start, in_len, skip }`; `encode_b64_slice(raw, skip, take)`; `PayloadChunk::{Bytes, Art{art_id,out,base64,art_total}}`; `lace_chunks_to_segments(serial, seq_start, bos, chunks) -> (Vec<Segment>, u32)`. The temporary `unreachable!` arm added in T2 is removed in T8.
- **Behaviour preservation:** the text-only path now routes through the chunk lacer (single `Bytes` chunk per packet), and the three Plan-1 synthesis tests are updated to the new signature and must still pass — proving the refactor didn't change text-tag output.
