# Phase 2 — Ogg Hardening Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Close the Ogg unit-test gaps and kill the ~42 real Ogg mutation survivors with additive tests, one named constant, and two dev-dependencies — no production logic changes.

**Architecture:** Pure test work plus `pub const FLAG_EOS` and `ogg`/`crc` dev-deps on `musefs-core`. The independent oracle lives in `ogg_index.rs`'s in-crate `#[cfg(test)]` module (dev-deps and crate-private `serve`/`build_index` are both reachable there). Each targeted survivor is killed using the **hand-apply** method below.

**Tech Stack:** Rust, `cargo test`, `proptest` (existing), `tempfile` (existing dev-dep), `ogg = "0.9"` + `crc = "3"` (new dev-deps on `musefs-core`), `musefs_format::ogg` helpers.

**Spec:** `docs/superpowers/specs/test-audit-remediation/2026-05-29-phase2-ogg-hardening-design.md`
**Survivor data:** `docs/superpowers/specs/test-audit-remediation/2026-05-29-mutation-inventory.md`

---

## The hand-apply verification method (use in every kill step)

cargo-mutants is not available locally. To prove a new test kills a specific
survivor, do this **for each targeted `file:line: mutation`**:

1. Run the new test → it passes (production code is correct).
2. Open the file, apply the exact mutation at that line (e.g. change `<` to `<=`),
   rerun **just that test** → it must **fail**.
3. `git checkout -- <file>` (or undo) to revert the mutation, rerun → passes again.

If step 2 still passes, the test does not kill the mutant. Either strengthen the
test, or — if the mutation provably produces identical behavior — record it as an
**equivalent mutant** (Task 7) instead of forcing a contrived test. Never leave a
mutation applied.

## Pre-flight (run once before Task 1)

- [ ] **Confirm the baseline is green and you are on the phase-2 branch**

```bash
git rev-parse --abbrev-ref HEAD          # expect: phase2-ogg-hardening
cargo test -p musefs-core ogg_index -- --nocapture
cargo test -p musefs-format --features fuzzing ogg
```
Expected: both green (existing Ogg tests pass).

---

## Task 1: `serve()` boundary tests (spec C1 — findings #1, #8)

Kills `ogg_index.rs:117`. Documents `:105` and `:113` as equivalents (Task 7).

**Files:**
- Modify (tests only): `musefs-core/src/ogg_index.rs` — inside `#[cfg(test)] mod tests` (after the existing `build_index_renumbers_…` test, before the closing `}` at line 156).

- [ ] **Step 1: Add the fixture + reference helpers**

Add to `mod tests` (the module already has `use super::*;`,
`use musefs_format::ogg::page_test_support::lace_packet_pub;`, `use std::io::Write;`):

```rust
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
        std::fs::File::create(&path).unwrap().write_all(&file_bytes).unwrap();
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
                .read_exact_at(&mut buf, audio_offset + p.region_offset + p.header.len() as u64)
                .unwrap();
            out.extend_from_slice(&buf);
        }
        out
    }

    fn serve_range(idx: &OggPageIndex, backing: &std::fs::File, audio_offset: u64, a: u64, b: u64) -> Vec<u8> {
        let mut out = Vec::new();
        serve(idx, backing, audio_offset, a, b, &mut out).unwrap();
        out
    }
```

- [ ] **Step 2: Write the boundary tests**

```rust
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
        assert_eq!(serve_range(&idx, &backing, ao, 0, hlen), want[0..hlen as usize]);
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
        assert_eq!(serve_range(&idx, &backing, ao, start, end), want[start as usize..end as usize]);
    }

    #[test]
    fn serve_spanning_header_and_payload() {
        let (_d, idx, backing, ao, _t) = serve_fixture();
        let want = reference_region(&idx, &backing, ao);
        let hlen = idx.pages[0].header.len() as u64;
        let r = (hlen - 5)..(hlen + 20);
        assert_eq!(serve_range(&idx, &backing, ao, r.start, r.end), want[r.start as usize..r.end as usize]);
    }

    #[test]
    fn serve_crossing_page_boundary() {
        let (_d, idx, backing, ao, _t) = serve_fixture();
        let want = reference_region(&idx, &backing, ao);
        // End of page 0 region into the start of page 1.
        let p0_end = idx.pages[0].header.len() as u64 + idx.pages[0].payload_len;
        let r = (p0_end - 30)..(p0_end + 40);
        assert_eq!(serve_range(&idx, &backing, ao, r.start, r.end), want[r.start as usize..r.end as usize]);
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
        assert_eq!(serve_range(&idx, &backing, ao, total - 25, total + 1000), want[(total - 25) as usize..]);
    }
```

- [ ] **Step 3: Run the tests, expect PASS**

```bash
cargo test -p musefs-core ogg_index::tests::serve -- --nocapture
```
Expected: all 6 `serve_*` tests pass.

- [ ] **Step 4: Hand-apply-verify the kill for `:117`**

Edit `musefs-core/src/ogg_index.rs:117`, change the read offset
`audio_offset + p.region_offset + hlen + within` so the **last** `+ within`
becomes `- within`. Run:
```bash
cargo test -p musefs-core ogg_index::tests::serve_payload_only_read_starting_mid_payload
```
Expected: **FAIL** (served payload bytes differ from reference). Then
`git checkout -- musefs-core/src/ogg_index.rs` and rerun → PASS.

- [ ] **Step 5: Confirm `:105`/`:113` are equivalents (note for Task 7)**

Apply `:105` (`if hs < he` → `if hs <= he`); run the full `serve_*` set → still
green (when `hs == he` the slice `&header[a..a]` is empty). Revert. Repeat for
`:113` (`if ps < pe` → `<=`; `n == 0` reads nothing) → still green. Revert.
Record both as equivalents in Task 7. **Do not** add tests for them.

- [ ] **Step 6: Commit**

```bash
git add musefs-core/src/ogg_index.rs
git commit -m "test(ogg): serve() boundary coverage; kill ogg_index:117"
```

---

## Task 2: `build_index` error path + assertions (spec C2 — findings #3, #4)

**Files:**
- Modify (tests only): `musefs-core/src/ogg_index.rs` `#[cfg(test)] mod tests`.

- [ ] **Step 1: Write the consume-mismatch error test (#3)**

```rust
    #[test]
    fn build_index_errors_when_audio_length_is_not_on_a_page_boundary() {
        // One 300-byte packet -> one page of total_len T. Passing audio_length = T-5
        // makes the loop read the whole page (consumed = T) then exit with
        // consumed != audio_length.
        let (bytes, _) = lace_packet_pub(0xABCD, 0, false, 0, &vec![7u8; 300]);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.ogg");
        std::fs::File::create(&path).unwrap().write_all(&bytes).unwrap();
        let short = bytes.len() as u64 - 5;
        let err = build_index(&path, 0, short, 0);
        assert!(err.is_err(), "expected Err on non-page-boundary audio_length");
    }
```

- [ ] **Step 2: Run, expect PASS, then hand-apply-verify**

```bash
cargo test -p musefs-core ogg_index::tests::build_index_errors_when_audio_length
```
Expected: PASS. Then verify the guard is what makes it fail: at
`musefs-core/src/ogg_index.rs:72` change `if consumed != audio_length` to
`if consumed == audio_length` (the `!= -> ==` mutation if listed; otherwise change
the body to `Ok(...)`). Rerun → the test should FAIL (no error returned). Revert.

- [ ] **Step 3: Strengthen the existing renumber test for all pages (#4)**

Replace the body of `build_index_renumbers_and_preserves_payload_length` (lines
131–155) with the version below — same fixture, added assertions for
`FLAG_CONTINUED`, per-page CRC validity, `payload_len` consistency, and contiguous
`region_offset`s:

```rust
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
        std::fs::File::create(&path).unwrap().write_all(&file_bytes).unwrap();

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
```

NOTE: per-page **CRC** validity is asserted by Task 3's oracle (independent `crc`
crate); `crc32` is private to `musefs-format::ogg` and is intentionally **not**
re-exported here. This task pins seq renumbering, `payload_len`, `FLAG_CONTINUED`
on the continuation page, and contiguous offsets.

- [ ] **Step 4: Run + hand-apply-verify, then commit**

```bash
cargo test -p musefs-core ogg_index
```
Expected: PASS. This task targets findings **#3 and #4** (coverage of the
`build_index` error path and per-page invariants); `build_index` itself has **no
named survivors** in the inventory (its only core survivors are the three `serve`
mutations handled in Task 1). So there is no `build_index` mutation to hand-apply
here — the deliverable is the new error test (Step 2, whose guard you already
hand-apply-verified) plus the strengthened all-page assertions (`FLAG_CONTINUED`,
`payload_len`, contiguous offsets). Per-page CRC validity is asserted by Task 3's
oracle.

```bash
git add musefs-core/src/ogg_index.rs
git commit -m "test(ogg): build_index consume-mismatch error + all-page assertions"
```

---

## Task 3: Independent Ogg oracle (spec C3 — finding #2)

End-to-end per codec: synthesize the header (format), serve the renumbered audio
(core), splice, then decode the whole stream with the third-party `ogg` crate and
re-check every page CRC with the `crc` crate.

**Files:**
- Modify: `musefs-core/Cargo.toml` — add `ogg` and `crc` under `[dev-dependencies]`.
- Modify (tests only): `musefs-core/src/ogg_index.rs` `#[cfg(test)] mod tests`.

- [ ] **Step 1: Add dev-dependencies**

In `musefs-core/Cargo.toml`, under `[dev-dependencies]`, add:
```toml
ogg = "0.9"
crc = "3"
```
Verify it resolves to the versions already in the lockfile (ogg 0.9.2, crc 3.4.0):
```bash
cargo build -p musefs-core --tests
```
Expected: builds; no new lockfile churn beyond enabling the dev-deps for this crate.

- [ ] **Step 2: Add the oracle helpers**

```rust
    /// CRC-32/Ogg: poly 0x04C11DB7, init 0, no reflection, no xorout. Independent
    /// of musefs-format::ogg::crc (different table, from the `crc` crate).
    const CRC_32_OGG: crc::Algorithm<u32> = crc::Algorithm {
        width: 32,
        poly: 0x04c1_1db7,
        init: 0x0000_0000,
        refin: false,
        refout: false,
        xorout: 0x0000_0000,
        check: 0x0000_0000, // CRC-32/Ogg check value (zero-input residue); we never call .check()
        residue: 0x0000_0000,
    };

    /// Assert `stream` is a clean single Ogg bitstream: the `ogg` crate reassembles
    /// every packet without error (it validates page CRCs), and an independent CRC
    /// (the `crc` crate) matches every page's stored CRC while seq numbers run
    /// 0,1,2,… contiguously.
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
                Segment::OggAudio { offset, len, seq_delta } => {
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

    /// Run the full synth+serve pipeline for one file and assert the spliced stream
    /// is a clean bitstream.
    fn oracle_roundtrip(file: &[u8]) {
        use musefs_format::ogg::{locate_audio, read_header, synthesize_layout};
        let scan = locate_audio(file).unwrap();
        let header = read_header(file).unwrap();
        let layout = synthesize_layout(&header, scan.audio_offset, scan.audio_length, &[], &[]).unwrap();
        let (hdr, ao, alen, delta) = materialize_header_and_audio_params(&layout);

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.ogg");
        std::fs::File::create(&path).unwrap().write_all(file).unwrap();
        let idx = build_index(&path, ao, alen, delta).unwrap();
        let backing = std::fs::File::open(&path).unwrap();
        let total: u64 = idx.pages.iter().map(|p| p.header.len() as u64 + p.payload_len).sum();
        let mut audio = Vec::new();
        serve(&idx, &backing, ao, 0, total, &mut audio).unwrap();

        let mut full = hdr;
        full.extend_from_slice(&audio);
        assert_clean_bitstream(&full);
    }
```

- [ ] **Step 3: Write the three codec oracle tests**

```rust
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
```

NOTE on fixtures: the codec header byte literals only need to satisfy
`detect_codec` / `read_header` / `synthesize_layout` (magic bytes, packet counts,
parseable VorbisComment). If `read_header` rejects a stub, inspect the failing
parser path and widen the literal minimally — do not change production code. The
existing `musefs-format/src/ogg/mod.rs` tests (`opus_headers`, `vorbis_headers_with`,
`oggflac_headers`) are the reference for valid layouts.

- [ ] **Step 4: Run the oracle tests**

```bash
cargo test -p musefs-core ogg_index::tests::oracle -- --nocapture
```
Expected: all three pass. If `ogg::PacketReader` reports an error, the splice is
wrong — debug before proceeding (this is the P1 deliverable).

- [ ] **Step 5: Commit**

```bash
git add musefs-core/Cargo.toml musefs-core/src/ogg_index.rs
git commit -m "test(ogg): independent end-to-end oracle (ogg+crc) for synth+serve across codecs"
```

---

## Task 4: `page.rs` mutant-kills + `FLAG_EOS` (spec C4 — finding #14)

**Files:**
- Modify: `musefs-format/src/ogg/page.rs:8` — add the constant; `#[cfg(test)] mod tests` — add tests.

Survivor targets: `:33`, `:47` (parse_page bounds), `:122` (lace_packet payload_pos),
`:181` (read_packets `== -> !=`), `:197` (patch_page_header `< -> >`), `:263`,
`:265`, `:266` (lace_chunks_to_segments flag bits), `:298` (`- -> +`), `:337`
(emit_segments). Timeouts `:93`, `:256`, `:294` are loop blow-ups (handled in
Step 6). Run all kills with the hand-apply method.

- [ ] **Step 1: Add `FLAG_EOS`**

Edit `musefs-format/src/ogg/page.rs` line 8, after `pub const FLAG_BOS: u8 = 0x02;`:
```rust
pub const FLAG_EOS: u8 = 0x04;
```

- [ ] **Step 2: EOS-preservation + parse-bounds tests**

Add to `mod tests`:

```rust
    #[test]
    fn eos_bit_is_preserved_through_renumber() {
        // Build a one-page packet, set its EOS bit, repatch the CRC, then renumber
        // via patch_page_header and confirm header_type (incl. EOS) is unchanged.
        let (mut page, _) = lace_packet(0xEE, 3, false, 9, &vec![0x11u8; 120]);
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
```

- [ ] **Step 3: Run + hand-apply-verify `:33`, `:47`, EOS**

```bash
cargo test -p musefs-format --features fuzzing ogg::page::tests::parse_page_rejects_truncated
cargo test -p musefs-format --features fuzzing ogg::page::tests::eos_bit_is_preserved
```
For `:33` change `if pos + 27 > buf.len()` to `>=` (or `==`); the truncated-header
case must fail. For `:47` change `if table_end > buf.len()` to `>=`/`==`; the
truncated-table case must fail. Revert each.

- [ ] **Step 4: Strengthen `lace_chunks_to_segments` flag + payload assertions (`:263`, `:265`, `:266`, `:298`, `:122`)**

The existing `chunk_lacer_splits_art_across_pages_and_crcs_validate` walks pages and
checks CRCs but never asserts `header_type`. In its page-walk loop `seq_expected`
is 0 on the first iteration and is incremented later in the body, so capture
`is_first` **before** the increment. Insert this immediately after
`let h = parse_page(&flat, pos).unwrap();`:

```rust
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
```

The payload-equality assertion at the end of that test (`assert_eq!(payload,
expected)`) already pins `:122`/`:298` (payload positions); confirm via hand-apply:
change `:298` `oe - os` neighbourhood `- -> +` and `:122` `payload_pos += ... -> *=`
and verify the payload assertion fails.

`copy_payload:310` (`if os < oe` → `<=`) and `emit_segments:337` (`if os < oe` →
`<=`) both gate an empty-overlap branch. Handle each with the hand-apply method:

- `:310`: with `<=`, when `os == oe` the copy slices `&bytes[os-cs..oe-cs]` (empty)
  → no-op → identical output. Run the chunk_lacer test under the mutation; if it
  stays green, this is an **equivalent mutant** — record it in Task 7. Do not
  contrive a test.
- `:337`: with `<=`, an `os == oe` at an **art-chunk** boundary is NOT a no-op — the
  `Art` arm flushes `buf` and pushes a zero-length `OggArtSlice`, changing the
  segment structure. Add this assertion to the chunk_lacer test so the spurious
  slice is caught:
  ```rust
      // No OggArtSlice may be zero-length (kills emit_segments:337 < -> <=).
      assert!(segments.iter().all(|s| !matches!(s, Segment::OggArtSlice { len: 0, .. })));
  ```
  Hand-apply `:337` `< -> <=` and rerun. If the current fixture's chunk boundaries
  never align exactly with a page boundary (so `os == oe` never occurs at the art
  chunk), the assertion may stay green under the mutant; in that case add a second
  fixture whose art run begins exactly at a page boundary (art chunk length a
  multiple of 65 025 after a head chunk filling the remainder of a page), or record
  `:337` as equivalent if no aligned case is reachable. Document the outcome in
  Task 7.

- [ ] **Step 5: `read_packets` `:181` and `patch_page_header` `:197`**

`:197` (`if page.len() < h.total_len()` → `>`): add

```rust
    #[test]
    fn patch_page_header_rejects_truncated_page() {
        let (page, _) = lace_packet(0xCAFE, 1, false, 0, &vec![0x42u8; 300]);
        let h = parse_page(&page, 0).unwrap();
        // Hand a buffer shorter than total_len: original returns Err; the `>` mutant
        // would proceed and panic slicing page[..total_len].
        assert_eq!(patch_page_header(&page[..h.total_len() - 10], 2), Err(FormatError::Malformed));
    }
```
Hand-apply `:197` `< -> >` → the test must fail (panic or Ok). Revert.

`:181` (`if out.len() == want { break; }` → `!=`): this is investigative. Construct
a page whose segment table **completes the `want`-th packet but has trailing
segments after it**, so the `== want` break matters. Add:

```rust
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
```
Hand-apply `:181` `== -> !=`: with `!=`, after pushing packet A `out.len()==1==want`
so the `!=` guard is false → no break → it consumes packet B too → `pkts.len()==2` →
test fails. Revert.

- [ ] **Step 6: Timeouts `:93`, `:256`, `:294` (bounded acceptance)**

These mutate loop bounds (`while first || lace_pos < laces.len()`) and `payload_pos
+= ... -> *=`, producing non-terminating loops or huge allocations; cargo-mutants
already flags them as **timeout** (not silent survivors). Do **not** write a test
that itself hangs. Confirm the existing multi-page tests (`large_packet_spans…`,
`chunk_lacer_…`) exercise these lines, then record in Task 7 that `:93`/`:256`/`:294`
are detected-by-timeout and not pursued further.

- [ ] **Step 7: Run the whole page suite + commit**

```bash
cargo test -p musefs-format --features fuzzing ogg::page
```
Expected: green. Then:
```bash
git add musefs-format/src/ogg/page.rs
git commit -m "test(ogg): page.rs parse/lace/read/patch kills + FLAG_EOS preservation"
```

---

## Task 5: `mod.rs` mutant-kills (spec C5)

**Files:**
- Modify (tests only): `musefs-format/src/ogg/mod.rs` `#[cfg(test)] mod tests`.

Targets: `:25` detect_codec, `:36` oggflac_following_packets, `:113` comment_body,
`:121`/`:130` comment_packet_index, `:196` locate_audio, `:233`/`:235`
synthesize_layout, `:254` picture_prefix, `:304`/`:305`/`:306` build_packets_with_art,
`:409`/`:410`/`:439` oggflac_packets_with_art. **Exclude `:455`** (test-support).

- [ ] **Step 1: `detect_codec` (`:25`) + `comment_body` (`:113`) + `oggflac_following_packets` (`:36`)**

`detect_codec`/`comment_body`/`oggflac_following_packets` are private; the test
module has `use super::*;`, so call them directly.

```rust
    #[test]
    fn detect_codec_matches_each_magic_and_rejects_others() {
        assert_eq!(detect_codec(b"OpusHead........").unwrap(), Codec::Opus);
        assert_eq!(detect_codec(b"\x01vorbis...").unwrap(), Codec::Vorbis);
        assert_eq!(detect_codec(b"\x7FFLAC...").unwrap(), Codec::OggFlac);
        // Too-short and non-matching inputs must error (kills the :25 && -> || and
        // the length-guard mutations).
        assert!(detect_codec(b"OpusHea").is_err());      // 7 bytes, len guard
        assert!(detect_codec(b"XXXXXXXX").is_err());     // right length, wrong magic
        assert!(detect_codec(b"\x01vorbi").is_err());    // 6 bytes
    }

    #[test]
    fn comment_body_strips_each_codec_prefix_and_guards_length() {
        assert_eq!(comment_body(Codec::Opus, b"OpusTagsBODY").unwrap(), b"BODY");
        assert_eq!(comment_body(Codec::Vorbis, b"\x03vorbisBODY").unwrap(), b"BODY");
        assert_eq!(comment_body(Codec::OggFlac, b"\x04\x00\x00\x00BODY").unwrap(), b"BODY");
        // packet shorter than the prefix errors (kills :113 < -> ==/<=).
        assert!(comment_body(Codec::Opus, b"OpusTa").is_err());
        assert!(comment_body(Codec::OggFlac, b"\x04\x00\x00").is_err());
    }

    #[test]
    fn oggflac_following_packets_reads_be_count_and_guards_length() {
        // 0x7F"FLAC" major minor count(BE) ... ; count bytes at [7],[8].
        let pkt = b"\x7FFLAC\x01\x00\x00\x05rest";
        assert_eq!(oggflac_following_packets(pkt).unwrap(), 5);
        assert!(oggflac_following_packets(b"\x7FFLAC\x01\x00").is_err()); // 7 bytes (<9)
    }
```

- [ ] **Step 2: `comment_packet_index` (`:121`, `:130`)**

```rust
    #[test]
    fn comment_packet_index_locates_the_comment_block() {
        // Opus/Vorbis: always packet index 1 (kills :121 -> 1 only if a non-1 case
        // exists; assert OggFLAC search to pin the skip(1)+find logic at :130).
        let opus = OggHeader { codec: Codec::Opus, serial: 1, packets: vec![vec![], vec![]], header_pages: 1, audio_offset: 0 };
        assert_eq!(comment_packet_index(&opus), 1);

        // OggFLAC: packet 0 mapping, packet 1 type 1 (non-comment), packet 2 type 4.
        let oggflac = OggHeader {
            codec: Codec::OggFlac,
            serial: 1,
            packets: vec![vec![0x7F], vec![0x01], vec![0x84]], // 0x84 & 0x7F == 4
            header_pages: 1,
            audio_offset: 0,
        };
        assert_eq!(comment_packet_index(&oggflac), 2);
        // No type-4 block -> 0 (kills the bitmask / == mutations at :130).
        let none = OggHeader {
            codec: Codec::OggFlac,
            serial: 1,
            packets: vec![vec![0x7F], vec![0x01], vec![0x05]],
            header_pages: 1,
            audio_offset: 0,
        };
        assert_eq!(comment_packet_index(&none), 0);
    }
```

- [ ] **Step 3: `locate_audio` (`:196`) — empty audio region is accepted**

The `:196` guard is `if header.audio_offset > data.len()`. After a successful
`read_header`, `audio_offset` is the end offset of a page that was parsed *within*
`data`, so it is always `<= data.len()` — the `>` branch is unreachable, which is
why truncation cannot trigger it. The reachable discriminator is
`audio_offset == data.len()` (a header-only file, no audio): the original `>` is
false → `Ok` with `audio_length == 0`, whereas the `> -> ==` and `> -> >=` mutants
both fire and wrongly return `Err`.

```rust
    #[test]
    fn locate_audio_accepts_empty_audio_region() {
        // opus_headers() is header pages only: audio_offset == data.len(). The
        // original `>` yields Ok (audio_length 0); the :196 `==`/`>=` mutants reject.
        let file = opus_headers();
        let scan = locate_audio(&file).unwrap();
        assert_eq!(scan.codec, Codec::Opus);
        assert_eq!(scan.audio_offset, file.len() as u64);
        assert_eq!(scan.audio_length, 0);
    }
```
Hand-apply `:196` `> -> >=` (then `> -> ==`): `locate_audio(&opus_headers())` must
go from `Ok` to `Err`, so the `.unwrap()` panics → test fails. Revert each.

- [ ] **Step 4: `synthesize_layout` (`:233`, `:235`), `picture_prefix` (`:254`), art builders (`:304`–`:306`, `:409`–`:439`)**

Most of these are already exercised by existing tests (`synthesize_opus_…`,
`picture_prefix_is_3_aligned_…`, `oversized_full_art_value_rejected_…`) yet
survived — strengthen the assertions:

- `:254` picture_prefix outer `% 3 -> + 3`: `pad = (3 - base % 3) % 3`. The inner
  `base % 3 -> base + 3` mutation underflows `usize` and panics (already caught by
  any call), so the survivor is the **outer** `% 3`. It diverges only when
  `base % 3 == 0`: original `pad = (3 - 0) % 3 = 0`, mutant `pad = (3 - 0) + 3 = 6`.
  The prefix length stays 3-aligned under both (6 is a multiple of 3), so
  `prefix.len() % 3 == 0` does **not** kill it — the **declared description-length
  field** does (it becomes `desc.len() + 6` instead of `desc.len()`). Add a new
  test with `base % 3 == 0` (`32 % 3 == 2`, so make `mime.len() + desc.len() ≡ 1
  (mod 3)`; `mime="image/png"` (9) + `description="x"` (1) → `base = 42`):
  ```rust
      #[test]
      fn picture_prefix_declared_desc_len_pins_padding() {
          let art = crate::input::ArtInput {
              art_id: 1,
              mime: "image/png".into(),   // 9
              description: "x".into(),     // 1 -> base = 42, 42 % 3 == 0 -> pad 0
              picture_type: 3,
              width: 1,
              height: 1,
              data_len: 100,
          };
          let prefix = picture_prefix(&art);
          assert_eq!(prefix.len() % 3, 0);
          // Declared description length lives at offset 8 + mime.len() (after
          // type[4] + mimelen[4] + mime). pad = declared - desc.len() must be 0..=2.
          let off = 8 + art.mime.len();
          let declared = u32::from_be_bytes(prefix[off..off + 4].try_into().unwrap());
          let pad = declared - art.description.len() as u32;
          assert!(pad <= 2, "pad must be 0..=2, got {pad}");
          assert_eq!(pad, 0, "base % 3 == 0 implies pad 0");
      }
  ```
  Hand-apply `:254` outer `% -> +`: `pad` becomes 6 → `declared` becomes 7 → the
  `pad <= 2` / `pad == 0` assertions fail. Revert.

- `:233` `seq += used` `+= -> *=` and `:235` `seq - header_pages` `- -> +`/`/`:
  add to `synthesize_opus_emits_valid_header_and_audio_segment` (or a new test)
  an assertion on the produced `OggAudio.seq_delta`. Compute the expected delta
  (synthesized header pages − original header pages) and assert it. Hand-apply
  `:235` and confirm the delta assertion fails.

- `:304`/`:305`/`:306` (build_packets_with_art value_len overflow guard) and
  `:409`/`:410` (oggflac body_len guard), `:439` (mapping len guard): the existing
  `oversized_…`/`sum_overflow_…` tests target these. Verify each via hand-apply;
  where a survivor persists, add a boundary case exactly at the `u32::MAX` /
  `0x00FF_FFFF` / `< 9` threshold so `>` vs `>=`/`==` diverges.

For each survivor in this step, run the hand-apply method and only add/adjust an
assertion if the existing test does not already fail under the mutation.

- [ ] **Step 5: Run the mod suite + commit**

```bash
cargo test -p musefs-format --features fuzzing ogg::tests
```
Expected: green. Then:
```bash
git add musefs-format/src/ogg/mod.rs
git commit -m "test(ogg): mod.rs codec/comment/locate/synth/art kills"
```

---

## Task 6: `b64.rs` mutant-kills (spec C6)

`b64_window:26` survivors (`take - 1` → `+`/`take`; `/ 4` → `* 4`) survive the
existing output-only test because over-reading raw input does not change the first
`take` output chars. Kill them by asserting the **`B64Window` fields directly**.

**Files:**
- Modify (tests only): `musefs-format/src/ogg/b64.rs` `#[cfg(test)] mod tests`.

- [ ] **Step 1: Field-level assertion test**

```rust
    #[test]
    fn b64_window_fields_are_exact_at_group_boundaries() {
        // out_offset and take chosen so the -1 and /4 in g1 are observable.
        // g0 = out_offset/4, g1 = (out_offset+take-1)/4,
        // in_start = g0*3, in_end = min((g1+1)*3, img_total), skip = out_offset - g0*4.
        let img_total = 1024u64;

        // take=1 at offset 0: g1 = 0 (with -1). The +1 mutant gives g1=0 too here,
        // so choose offset 3 take=1: g0=0,g1=0 vs +1 mutant g1=1 -> in_len differs.
        let w = b64_window(3, 1, img_total);
        assert_eq!(w, B64Window { in_start: 0, in_len: 3, skip: 3 });

        // take exactly fills group 0 (offset 0, take 4): g1=0; mutant take+1 -> g1=1.
        let w = b64_window(0, 4, img_total);
        assert_eq!(w, B64Window { in_start: 0, in_len: 3, skip: 0 });

        // offset 4 take 4 -> g0=1,g1=1 -> in_start=3,in_len=3,skip=0; /4->*4 mutant
        // makes g1 huge -> in_len clamps to img_total-3 (differs).
        let w = b64_window(4, 4, img_total);
        assert_eq!(w, B64Window { in_start: 3, in_len: 3, skip: 0 });

        // Window spanning two groups: offset 2 take 6 -> g0=0,g1=1 -> in 0..6.
        let w = b64_window(2, 6, img_total);
        assert_eq!(w, B64Window { in_start: 0, in_len: 6, skip: 2 });
    }
```

- [ ] **Step 2: Run + hand-apply-verify all three `:26` mutations**

```bash
cargo test -p musefs-format --features fuzzing ogg::b64::tests::b64_window_fields_are_exact
```
Expected: PASS. Then, one at a time at line 26:
- `take - 1` → `take + 1`: rerun → must FAIL on a `take`-fills-group case. Revert.
- `take - 1` → `take` (i.e. `- 1` removed via `/ 1`): rerun → must FAIL. Revert.
- `/ 4` → `* 4`: rerun → must FAIL on the `offset 4 take 4` case. Revert.

If any mutation does not fail, add a field-assertion case that distinguishes it
(the structured `B64Window { .. }` equality is the lever).

- [ ] **Step 3: Commit**

```bash
git add musefs-format/src/ogg/b64.rs
git commit -m "test(ogg): b64_window field-level boundary assertions"
```

---

## Task 7: Inventory + tracking updates (spec C7)

**Files:**
- Modify: `docs/superpowers/specs/test-audit-remediation/2026-05-29-mutation-inventory.md`
- Modify: `docs/superpowers/specs/test-audit-remediation/2026-05-29-remediation-tracking.md`

- [ ] **Step 1: Annotate equivalents and timeouts in the inventory**

In the musefs-core survivor table, append a `Kind`/note for the equivalents and add
a short "Ogg equivalent mutants" note section:
- `ogg_index.rs:105`, `ogg_index.rs:113` → **equivalent** (empty-overlap no-op;
  confirmed by hand-apply in Task 1).
- `ogg/page.rs:310` (copy_payload) → **equivalent** if Task 4 confirmed it (empty
  slice no-op). Record the hand-apply outcome.
- `ogg/page.rs:337` (emit_segments) → killed by the zero-length-`OggArtSlice`
  assertion if Task 4 reached an aligned case; otherwise **equivalent**. Record
  which.
- `ogg/page.rs:93`, `ogg/page.rs:256`, `ogg/page.rs:294` → **detected-by-timeout**
  (loop/allocation blow-up; not silent survivors).

Add a line under "Phase routing" or a new "Resolved (phase 2)" note recording that
finding #7 (crc.rs) had 0 survivors and #14 was reframed to EOS preservation.

- [ ] **Step 2: Flip Phase 2 status in the tracking doc**

In `2026-05-29-remediation-tracking.md`, change the Phase 2 line to
`⟶ STATUS: complete` and the top-of-file Status line to note Phase 2 done. Record
the residual: `:105`/`:113` equivalent, `:93`/`:256`/`:294` timeout-detected,
remaining survivors expected to drop in the next campaign.

- [ ] **Step 3: Commit**

```bash
git add docs/superpowers/specs/test-audit-remediation/2026-05-29-mutation-inventory.md \
        docs/superpowers/specs/test-audit-remediation/2026-05-29-remediation-tracking.md
git commit -m "docs(phase2): record Ogg equivalents/timeouts; mark phase 2 complete"
```

---

## Final verification (after all tasks)

- [ ] **Full workspace test run**

```bash
cargo test --workspace
cargo test -p musefs-format --features fuzzing
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```
Expected: all green (the pre-commit hook runs these too).

- [ ] **Open the PR; the `mutants.yml` `in-diff` + `canary` jobs run on it.** After
merge, the next scheduled/dispatched full campaign confirms the Ogg survivor counts
dropped (excluding the documented equivalents/timeouts). This is the authoritative
end-to-end check.

## Notes for the executor

- All production code is correct; these are coverage gaps. The **only** non-test
  production change in this plan is `pub const FLAG_EOS` (Task 4 Step 1).
- Never leave a hand-applied mutation in the tree — always revert before the next
  step.
- If a survivor turns out to be a genuine equivalent (hand-apply can't make a test
  fail without contrivance), record it in Task 7 rather than forcing a test.
- The pre-commit hook runs `cargo fmt --check`, `clippy -D warnings`, `cargo test
  --workspace`, and `ruff`; keep each commit green.
