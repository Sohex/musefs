# Phase 3d — WAV Hardening Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Kill the 24 killable `wav.rs` mutation survivors (document 4 equivalents), broaden `proptest_read_fidelity` to WAV, and close finding #16 for WAV with a scoped zero-byte-art skip — all without touching the byte-identity invariant.

**Architecture:** All mutant kills go in `wav.rs`'s in-module `#[cfg(test)] mod tests` (it reaches private helpers via `use super::*`). The one production change (C2) filters `data_len == 0` arts in `wav::synthesize_layout` before delegating to `mp3::build_id3v2_segments`, mirroring FLAC's existing skip. The cross-format property work adds a `write_wav` fixture to `musefs-core/tests/common` and four WAV read-fidelity properties.

**Tech Stack:** Rust, cargo, `proptest`, `id3` crate, `hound` (independent WAV reader in existing tests). Mutation verification is manual hand-apply (cargo-mutants is not available locally).

**Source spec:** `docs/superpowers/specs/test-audit-remediation/2026-05-29-phase3d-wav-hardening-design.md`

---

## Two verification methods used in this plan

- **Hand-apply (for mutant kills, Tasks 2–11):** the production code is already
  correct, so the new test PASSES first. Then apply the exact mutation, run the
  single test, confirm it FAILS, revert, confirm it PASSES again. While a mutation
  is applied, run **only the single targeted test**, never `--workspace`. Revert with
  `git checkout -- musefs-format/src/wav.rs` immediately.
- **Red-green TDD (for the C2 production change, Task 12):** the test FAILS first
  (current code bricks the track), then the fix makes it pass.

**Line numbers drift.** Each task names the **code pattern** to locate; confirm the
current line before hand-applying.

---

## Task 1: Test-module scaffold — widen the import

**Files:**
- Modify: `musefs-format/src/wav.rs` (the `#[cfg(test)] mod tests` at the end, currently ~line 323)

> The shared test helpers (`fmt_pcm`, `wav`, `info_payload`, `inline_offset_of`) are
> **not** added here — each is introduced in the first task that uses it, so no commit
> ever lands an unused function (the pre-commit `clippy -D warnings` rejects
> `dead_code`). Do **not** add `#[allow(dead_code)]`.

- [ ] **Step 1: Widen the test-module import**

In `musefs-format/src/wav.rs`, replace the test module's import line:

```rust
    use super::{read_pictures, read_tags};
```

with:

```rust
    use super::*;
```

A glob import does not warn when only partly used, so this is safe on its own. The
existing `wav_oom_crash_artifact_is_safe` test still compiles unchanged.

- [ ] **Step 2: Confirm the crate still builds and the existing test passes**

Run: `cargo test -p musefs-format --lib wav::tests`
Expected: PASS (only `wav_oom_crash_artifact_is_safe` runs so far).

- [ ] **Step 3: Commit**

```bash
git add musefs-format/src/wav.rs
git commit -m "test(wav): widen test-module import to super::*"
```

---

## Task 2: Kill `riff_wave_start:24` (`<`→`<=` and `<`→`==`)

**Pattern:** the `buf.len() < 12` guard in `riff_wave_start`.

**Files:**
- Modify: `musefs-format/src/wav.rs` (test module)

- [ ] **Step 1: Add the two kill tests inside `mod tests`**

```rust
    #[test]
    fn riff_wave_start_accepts_exactly_twelve_bytes() {
        // :24 `< → <=`: a valid 12-byte RIFF/WAVE buffer must be accepted.
        // The `<=` mutant computes `12 <= 12` (true) and wrongly rejects it.
        let buf = b"RIFF\0\0\0\0WAVE".to_vec();
        assert_eq!(buf.len(), 12);
        assert_eq!(riff_wave_start(&buf), Ok(12));
    }

    #[test]
    fn riff_wave_start_rejects_eleven_byte_riff_without_panic() {
        // :24 `< → ==`: an 11-byte buffer that starts with "RIFF". The original
        // short-circuits on `len < 12` → NotWav. The `==` mutant computes
        // `11 == 12` (false), falls through, and indexes `buf[8..12]` on an 11-byte
        // slice → panic. Asserting the clean Err kills it (panic ≠ Err).
        let buf = b"RIFF\0\0\0\0WAV".to_vec();
        assert_eq!(buf.len(), 11);
        assert_eq!(riff_wave_start(&buf), Err(FormatError::NotWav));
    }
```

- [ ] **Step 2: Run both tests — expect PASS**

Run: `cargo test -p musefs-format --lib wav::tests::riff_wave_start`
Expected: PASS (2 tests).

- [ ] **Step 3: Hand-apply `< → <=` and confirm the accept test fails**

Edit `wav.rs`: change `if buf.len() < 12` to `if buf.len() <= 12`.
Run: `cargo test -p musefs-format --lib wav::tests::riff_wave_start_accepts_exactly_twelve_bytes`
Expected: FAIL (`Ok(12)` expected, got `Err(NotWav)`).
Revert: `git checkout -- musefs-format/src/wav.rs`

- [ ] **Step 4: Hand-apply `< → ==` and confirm the reject test fails**

Edit `wav.rs`: change `if buf.len() < 12` to `if buf.len() == 12`.
Run: `cargo test -p musefs-format --lib wav::tests::riff_wave_start_rejects_eleven_byte_riff_without_panic`
Expected: FAIL (panic: byte index 12 out of bounds / range end index 12).
Revert: `git checkout -- musefs-format/src/wav.rs`

- [ ] **Step 5: Confirm green after revert, then commit**

```bash
cargo test -p musefs-format --lib wav::tests::riff_wave_start
git add musefs-format/src/wav.rs
git commit -m "test(wav): kill riff_wave_start:24 length-guard mutants"
```

---

## Task 3: Kill `walk_chunks:47` (`+`→`-` in the advance)

**Pattern:** `let advance = 8u64 + size + (size & 1);` in `walk_chunks`.

**Files:**
- Modify: `musefs-format/src/wav.rs` (test module)

- [ ] **Step 1: Add the `wav` helper (first use) and the kill test**

Add this helper inside `mod tests` (used by this and later tasks):

```rust
    /// Build a minimal `RIFF/WAVE` buffer from `(fourcc, payload)` chunks in order,
    /// padding odd payloads to a word boundary (the on-disk RIFF layout).
    fn wav(chunks: &[(&[u8; 4], Vec<u8>)]) -> Vec<u8> {
        let mut body = Vec::new();
        for (id, payload) in chunks {
            body.extend_from_slice(*id);
            body.extend_from_slice(&(payload.len() as u32).to_le_bytes());
            body.extend_from_slice(payload);
            if payload.len() % 2 == 1 {
                body.push(0x00);
            }
        }
        let mut out = b"RIFF".to_vec();
        out.extend_from_slice(&((body.len() + 4) as u32).to_le_bytes());
        out.extend_from_slice(b"WAVE");
        out.extend_from_slice(&body);
        out
    }

    #[test]
    fn walk_chunks_advances_past_each_payload() {
        // :47 the `8 + size (+ size&1)` advance. An odd first payload forces the
        // word-align term to matter; a wrong advance (either `+ → -`) lands off the
        // next header, so the second chunk is lost or misread.
        let buf = wav(&[(b"AAAA", vec![0x11; 3]), (b"data", vec![0xBB; 8])]);
        let ids: Vec<[u8; 4]> = walk_chunks(&buf).iter().map(|(id, _, _)| *id).collect();
        assert_eq!(ids, vec![*b"AAAA", *b"data"]);
    }
```

- [ ] **Step 2: Run — expect PASS**

Run: `cargo test -p musefs-format --lib wav::tests::walk_chunks_advances_past_each_payload`
Expected: PASS.

- [ ] **Step 3: Hand-apply and confirm failure (test each `+` once)**

The line has two `+`. Apply each mutation in turn:
- Change `8u64 + size + (size & 1)` to `8u64 - size + (size & 1)`, run the test → expect FAIL (panic: subtract overflow, or wrong ids).
- Revert, then change to `8u64 + size - (size & 1)`, run the test → expect FAIL (ids mismatch — lands 2 bytes early).

Run after each: `cargo test -p musefs-format --lib wav::tests::walk_chunks_advances_past_each_payload`
Revert after each: `git checkout -- musefs-format/src/wav.rs`

- [ ] **Step 4: Confirm green, then commit**

```bash
cargo test -p musefs-format --lib wav::tests::walk_chunks_advances_past_each_payload
git add musefs-format/src/wav.rs
git commit -m "test(wav): kill walk_chunks:47 advance mutant"
```

---

## Task 4: Kill `locate_audio:67` (`==`→`!=`) and `:71` (`>`→`<`)

**Patterns:** `id == b"fmt "` (the `has_fmt` detector, :67) and
`(off as u64).saturating_add(len) > buf.len() as u64` (oversize guard, :71).

**Files:**
- Modify: `musefs-format/src/wav.rs` (test module)

- [ ] **Step 1: Add the `fmt_pcm` helper (first use) and both kill tests**

Add this helper inside `mod tests` (used by this and later tasks):

```rust
    /// A 16-byte PCM `fmt ` payload: mono, 44.1 kHz, 16-bit.
    fn fmt_pcm() -> Vec<u8> {
        let mut f = Vec::new();
        f.extend_from_slice(&1u16.to_le_bytes());
        f.extend_from_slice(&1u16.to_le_bytes());
        f.extend_from_slice(&44_100u32.to_le_bytes());
        f.extend_from_slice(&88_200u32.to_le_bytes());
        f.extend_from_slice(&2u16.to_le_bytes());
        f.extend_from_slice(&16u16.to_le_bytes());
        f
    }

    #[test]
    fn locate_requires_fmt_chunk() {
        // :67 `== → !=`: a data-only WAV (no `fmt `). Original: `any(id == "fmt ")`
        // is false → NotWav. The `!=` mutant is true (the data chunk is != "fmt ")
        // → has_fmt, so it returns Ok/Malformed instead of NotWav.
        let buf = wav(&[(b"data", vec![0x11; 8])]);
        assert_eq!(locate_audio(&buf), Err(FormatError::NotWav));
    }

    #[test]
    fn locate_accepts_data_with_trailing_chunk() {
        // :71 `> → <`: a valid WAV with a chunk AFTER `data`, so off+len < buf.len.
        // Original `off+len > buf.len` is false → Ok. The `<` mutant is true →
        // Malformed.
        let buf = wav(&[
            (b"fmt ", fmt_pcm()),
            (b"data", vec![0x11; 8]),
            (b"junk", vec![0x00; 4]),
        ]);
        let bounds = locate_audio(&buf).unwrap();
        assert_eq!(bounds.audio_length, 8);
    }
```

- [ ] **Step 2: Run — expect PASS**

Run: `cargo test -p musefs-format --lib wav::tests::locate_`
Expected: PASS (2 tests).

- [ ] **Step 3: Hand-apply `:67` `== → !=` and confirm failure**

Edit `wav.rs`: change `chunks.iter().any(|(id, _, _)| id == b"fmt ")` to use `!=`.
Run: `cargo test -p musefs-format --lib wav::tests::locate_requires_fmt_chunk`
Expected: FAIL (`Ok(..)` returned, `Err(NotWav)` expected).
Revert: `git checkout -- musefs-format/src/wav.rs`

- [ ] **Step 4: Hand-apply `:71` `> → <` and confirm failure**

Edit `wav.rs`: change `if (off as u64).saturating_add(len) > buf.len() as u64` to `<`.
Run: `cargo test -p musefs-format --lib wav::tests::locate_accepts_data_with_trailing_chunk`
Expected: FAIL (`unwrap` on `Err(Malformed)`).
Revert: `git checkout -- musefs-format/src/wav.rs`

- [ ] **Step 5: Confirm green, then commit**

```bash
cargo test -p musefs-format --lib wav::tests::locate_
git add musefs-format/src/wav.rs
git commit -m "test(wav): kill locate_audio:67/:71 mutants"
```

---

## Task 5: Kill `info_fourcc:119-124` (6 arm deletions)

**Pattern:** the key→FourCC `match` in `info_fourcc` (artist/album/date/genre/comment/tracknumber arms; title is already covered).

**Files:**
- Modify: `musefs-format/src/wav.rs` (test module)

- [ ] **Step 1: Add the kill test (drives the private `build_info_payload`)**

```rust
    #[test]
    fn info_fourcc_emits_each_mapped_key() {
        // :119-124 arm deletions: each key must map to its INFO FourCC. A deleted
        // arm makes the key unmapped → no payload (single-tag input → None).
        let cases: [(&str, &[u8; 4]); 6] = [
            ("artist", b"IART"),
            ("album", b"IPRD"),
            ("date", b"ICRD"),
            ("genre", b"IGNR"),
            ("comment", b"ICMT"),
            ("tracknumber", b"ITRK"),
        ];
        for (key, cc) in cases {
            let payload =
                build_info_payload(&[TagInput::new(key, "X")]).expect("INFO payload for {key}");
            assert!(
                payload.windows(4).any(|w| w == &cc[..]),
                "key {key} must emit FourCC {:?}",
                std::str::from_utf8(cc).unwrap()
            );
        }
    }
```

- [ ] **Step 2: Run — expect PASS**

Run: `cargo test -p musefs-format --lib wav::tests::info_fourcc_emits_each_mapped_key`
Expected: PASS.

- [ ] **Step 3: Hand-apply one arm deletion and confirm failure**

Edit `wav.rs` `info_fourcc`: delete the `"album" => b"IPRD",` arm (it now falls to
`_ => return None`).
Run: `cargo test -p musefs-format --lib wav::tests::info_fourcc_emits_each_mapped_key`
Expected: FAIL (`expect("INFO payload..")` panics — `None` for the album case).
Revert: `git checkout -- musefs-format/src/wav.rs`

(The single deletion proves the mechanism; cargo-mutants applies each of the six
independently — the loop covers them all.)

- [ ] **Step 4: Confirm green, then commit**

```bash
cargo test -p musefs-format --lib wav::tests::info_fourcc_emits_each_mapped_key
git add musefs-format/src/wav.rs
git commit -m "test(wav): kill info_fourcc arm-deletion mutants"
```

---

## Task 6: Kill `build_info_payload:155` (`%`→`/`, `%`→`+`, `==`→`!=`)

**Pattern:** `if v.len() % 2 == 1 { payload.push(0x00); }` in `build_info_payload`.

**Files:**
- Modify: `musefs-format/src/wav.rs` (test module)

- [ ] **Step 1: Add the kill test**

```rust
    #[test]
    fn build_info_payload_word_aligns_values() {
        // :155 `v.len() % 2 == 1`. v = value bytes + NUL.
        // Value "a"  -> v.len()=2 (even, NO pad). Kills `% → /` (2/2==1 pads) and
        //               `== → !=` (2%2=0 != 1 pads).
        // Value "ab" -> v.len()=3 (odd, padded). Kills `% → +` (3+2 != 1, no pad).
        let even = build_info_payload(&[TagInput::new("title", "a")]).unwrap();
        // "INFO"(4) + "INAM"(4) + len(4) + "a\0"(2) = 14, no pad.
        assert_eq!(even.len(), 14);

        let odd = build_info_payload(&[TagInput::new("title", "ab")]).unwrap();
        // "INFO"(4) + "INAM"(4) + len(4) + "ab\0"(3) + pad(1) = 16.
        assert_eq!(odd.len(), 16);
    }
```

- [ ] **Step 2: Run — expect PASS**

Run: `cargo test -p musefs-format --lib wav::tests::build_info_payload_word_aligns_values`
Expected: PASS.

- [ ] **Step 3: Hand-apply each mutation and confirm failure**

For each, edit the `v.len() % 2 == 1` in `build_info_payload`, run the test, expect
FAIL, then revert (`git checkout -- musefs-format/src/wav.rs`):
- `% → /` (`v.len() / 2 == 1`): FAIL on `even` (len 15, expected 14).
- `% → +` (`v.len() + 2 == 1`): FAIL on `odd` (len 15, expected 16).
- `== → !=` (`v.len() % 2 != 1`): FAIL on `even` (len 15, expected 14).

Run: `cargo test -p musefs-format --lib wav::tests::build_info_payload_word_aligns_values`

- [ ] **Step 4: Confirm green, then commit**

```bash
cargo test -p musefs-format --lib wav::tests::build_info_payload_word_aligns_values
git add musefs-format/src/wav.rs
git commit -m "test(wav): kill build_info_payload:155 word-align mutants"
```

---

## Task 7: Kill `push_inline_chunk:168` (`%`→`/`, `%`→`+`)

**Pattern:** `if payload.len() % 2 == 1 { chunk.push(0x00); }` in `push_inline_chunk`.

**Files:**
- Modify: `musefs-format/src/wav.rs` (test module)

- [ ] **Step 1: Add the kill test**

```rust
    #[test]
    fn push_inline_chunk_word_aligns_payload() {
        // :168 `payload.len() % 2 == 1`.
        // Even payload (len 2): NO pad. Kills `% → /` (2/2==1 pads).
        let mut segs = Vec::new();
        push_inline_chunk(&mut segs, b"test", &[0xAA, 0xBB]);
        assert_eq!(segs.len(), 1);
        assert_eq!(segs[0].len(), 10); // "test"(4) + len(4) + payload(2)

        // Odd payload (len 3): padded. Kills `% → +` (3+2 != 1, no pad).
        let mut segs2 = Vec::new();
        push_inline_chunk(&mut segs2, b"test", &[0xAA, 0xBB, 0xCC]);
        assert_eq!(segs2[0].len(), 12); // 4 + 4 + 3 + pad(1)
    }
```

- [ ] **Step 2: Run — expect PASS**

Run: `cargo test -p musefs-format --lib wav::tests::push_inline_chunk_word_aligns_payload`
Expected: PASS.

- [ ] **Step 3: Hand-apply each mutation and confirm failure**

Edit the `payload.len() % 2 == 1` in `push_inline_chunk`, run, expect FAIL, revert:
- `% → /`: FAIL on the even case (`segs[0].len()` == 11, expected 10).
- `% → +`: FAIL on the odd case (`segs2[0].len()` == 11, expected 12).

Run: `cargo test -p musefs-format --lib wav::tests::push_inline_chunk_word_aligns_payload`
Revert each: `git checkout -- musefs-format/src/wav.rs`

- [ ] **Step 4: Confirm green, then commit**

```bash
cargo test -p musefs-format --lib wav::tests::push_inline_chunk_word_aligns_payload
git add musefs-format/src/wav.rs
git commit -m "test(wav): kill push_inline_chunk:168 word-align mutants"
```

---

## Task 8: Kill `info_to_key:245-249` (4 arm deletions, via `read_tags`)

**Pattern:** the FourCC→key `match` in `info_to_key` (IPRD/ICRD/ICMT/ITRK arms; INAM/IART/IGNR already covered).

**Files:**
- Modify: `musefs-format/src/wav.rs` (test module)

- [ ] **Step 1: Add the `info_payload` helper (first use) and the kill test**

Add this helper inside `mod tests`:

```rust
    /// An `INFO` payload: `"INFO"` FourCC + NUL-terminated, word-aligned subchunks.
    fn info_payload(pairs: &[(&[u8; 4], &str)]) -> Vec<u8> {
        let mut p = b"INFO".to_vec();
        for (cc, val) in pairs {
            let mut v = val.as_bytes().to_vec();
            v.push(0x00);
            p.extend_from_slice(*cc);
            p.extend_from_slice(&(v.len() as u32).to_le_bytes());
            p.extend_from_slice(&v);
            if v.len() % 2 == 1 {
                p.push(0x00);
            }
        }
        p
    }

    #[test]
    fn info_to_key_decodes_each_mapped_fourcc() {
        // :245-249 arm deletions: each INFO FourCC must decode to its tag key.
        let cases: [(&[u8; 4], &str, &str); 4] = [
            (b"IPRD", "album", "Anthology"),
            (b"ICRD", "date", "1999"),
            (b"ICMT", "comment", "Nice"),
            (b"ITRK", "tracknumber", "3"),
        ];
        for (cc, key, val) in cases {
            let buf = wav(&[
                (b"fmt ", fmt_pcm()),
                (b"LIST", info_payload(&[(cc, val)])),
                (b"data", vec![0x00; 4]),
            ]);
            let tags = read_tags(&buf);
            assert!(
                tags.contains(&(key.to_string(), val.to_string())),
                "FourCC {:?} must decode to {key}",
                std::str::from_utf8(cc).unwrap()
            );
        }
    }
```

- [ ] **Step 2: Run — expect PASS**

Run: `cargo test -p musefs-format --lib wav::tests::info_to_key_decodes_each_mapped_fourcc`
Expected: PASS.

- [ ] **Step 3: Hand-apply one arm deletion and confirm failure**

Edit `info_to_key`: delete the `b"IPRD" => "album",` arm.
Run: `cargo test -p musefs-format --lib wav::tests::info_to_key_decodes_each_mapped_fourcc`
Expected: FAIL (album case: tag missing).
Revert: `git checkout -- musefs-format/src/wav.rs`

- [ ] **Step 4: Confirm green, then commit**

```bash
cargo test -p musefs-format --lib wav::tests::info_to_key_decodes_each_mapped_fourcc
git add musefs-format/src/wav.rs
git commit -m "test(wav): kill info_to_key arm-deletion mutants"
```

---

## Task 9: Kill `read_tags:300` (`&&`→`||`)

**Pattern:** `.filter(|slice| slice.len() >= 4 && &slice[0..4] == b"INFO")` in `read_tags`.

**Files:**
- Modify: `musefs-format/src/wav.rs` (test module)

- [ ] **Step 1: Add the kill test**

```rust
    #[test]
    fn read_tags_rejects_short_list_without_panic() {
        // :300 `&& → ||`: a LIST chunk with a <4-byte payload. Original
        // short-circuits (`len >= 4` false → no INFO, empty). The `||` mutant
        // evaluates `&slice[0..4]` on the 2-byte slice → panic. Asserting the clean
        // empty result kills it (panic ≠ empty).
        let buf = wav(&[
            (b"fmt ", fmt_pcm()),
            (b"LIST", vec![0x49, 0x4E]), // "IN" — 2 bytes, < 4
            (b"data", vec![0x00; 4]),
        ]);
        assert!(read_tags(&buf).is_empty());
    }
```

- [ ] **Step 2: Run — expect PASS**

Run: `cargo test -p musefs-format --lib wav::tests::read_tags_rejects_short_list_without_panic`
Expected: PASS.

- [ ] **Step 3: Hand-apply `&& → ||` and confirm failure**

Edit `read_tags`: change `slice.len() >= 4 && &slice[0..4] == b"INFO"` to use `||`.
Run: `cargo test -p musefs-format --lib wav::tests::read_tags_rejects_short_list_without_panic`
Expected: FAIL (panic: range end index 4 out of range for slice of length 2).
Revert: `git checkout -- musefs-format/src/wav.rs`

- [ ] **Step 4: Confirm green, then commit**

```bash
cargo test -p musefs-format --lib wav::tests::read_tags_rejects_short_list_without_panic
git add musefs-format/src/wav.rs
git commit -m "test(wav): kill read_tags:300 INFO-validation mutant"
```

---

## Task 10: Kill `synthesize_layout:207` (`%`→`/`, `%`→`+`; the `id3 ` pad)

**Pattern:** `if tag_len % 2 == 1 { segments.push(Segment::Inline(vec![0x00])); }` in `synthesize_layout`.

**Files:**
- Modify: `musefs-format/src/wav.rs` (test module)

- [ ] **Step 1: Add the `inline_offset_of` helper (first use) and the kill test**

Add this helper inside `mod tests`:

```rust
    /// Byte offset, in the assembled stream, of the first `Inline` segment whose
    /// first four bytes are `fourcc`. Used to assert RIFF word-alignment.
    fn inline_offset_of(layout: &RegionLayout, fourcc: &[u8; 4]) -> u64 {
        let mut off = 0u64;
        for s in &layout.segments {
            if let Segment::Inline(b) = s {
                if b.len() >= 4 && &b[0..4] == fourcc {
                    return off;
                }
            }
            off += s.len();
        }
        panic!("no inline chunk starting with {fourcc:?}");
    }

    #[test]
    fn synthesize_word_aligns_embedded_id3_chunk() {
        // :207 `tag_len % 2 == 1` — the pad after the `id3 ` chunk. When tag_len is
        // odd, the original pads so the following `data` chunk starts on an even
        // byte (RIFF word-alignment). Both mutants (`/`, `+`) drop that pad for odd
        // tag_len, landing `data` on an odd offset.
        //
        // Find tags whose ID3v2 tag_len is odd (parity depends on id3 framing, so
        // discover it rather than hard-code). "albumartist" maps to id3 only (no
        // INFO/LIST chunk), keeping the layout simple.
        let mut tags = Vec::new();
        let mut tag_len = 0u64;
        for n in 1..64 {
            let cand = vec![TagInput::new("albumartist", &"x".repeat(n))];
            let (_, tl) = crate::mp3::build_id3v2_segments(&cand, &[]).unwrap();
            if tl % 2 == 1 {
                tags = cand;
                tag_len = tl;
                break;
            }
        }
        assert_eq!(tag_len % 2, 1, "expected to find an odd-length id3 tag");

        let scan = WavScan {
            fmt: fmt_pcm(),
            fact: None,
        };
        let layout = synthesize_layout(&scan, 0, 8, &tags, &[]).unwrap();
        assert_eq!(
            inline_offset_of(&layout, b"data") % 2,
            0,
            "the data chunk must be word-aligned"
        );
    }
```

- [ ] **Step 2: Run — expect PASS**

Run: `cargo test -p musefs-format --lib wav::tests::synthesize_word_aligns_embedded_id3_chunk`
Expected: PASS.

- [ ] **Step 3: Hand-apply each mutation and confirm failure**

Edit the `tag_len % 2 == 1` in `synthesize_layout`, run, expect FAIL, revert:
- `% → /` (`tag_len / 2 == 1`): for an odd tag_len ≥ 4, `tag_len/2 ≥ 2 != 1` → no
  pad → `data` at an odd offset → FAIL.
- `% → +` (`tag_len + 2 == 1`): always false → no pad → FAIL.

Run: `cargo test -p musefs-format --lib wav::tests::synthesize_word_aligns_embedded_id3_chunk`
Revert each: `git checkout -- musefs-format/src/wav.rs`

- [ ] **Step 4: Confirm green, then commit**

```bash
cargo test -p musefs-format --lib wav::tests::synthesize_word_aligns_embedded_id3_chunk
git add musefs-format/src/wav.rs
git commit -m "test(wav): kill synthesize_layout:207 id3-pad mutants"
```

---

## Task 11: Kill `synthesize_layout:227` (`>`→`==`) and document the 4 equivalents

**Pattern:** `if riff_size > u32::MAX as u64 { return Err(FormatError::TooLarge); }` in `synthesize_layout`.

**Files:**
- Modify: `musefs-format/src/wav.rs` (test module)

- [ ] **Step 1: Add the kill test plus the equivalent-mutant documentation**

```rust
    #[test]
    fn synthesize_rejects_riff_size_overflow() {
        // :227 `> → ==`. `BackingAudio` is virtual (no real allocation), so we can
        // pass `audio_length == u32::MAX`: it PASSES the :186 guard (`> u32::MAX` is
        // false) but makes `riff_size > u32::MAX`. Original `>` → TooLarge; the `==`
        // mutant (`riff_size == u32::MAX`) is false (riff_size is strictly greater),
        // so it wrongly proceeds and returns Ok with a truncated size.
        //
        // NOTE — this must use exactly u32::MAX, not the existing
        // `rejects_audio_over_32bit` test's `u32::MAX + 1`: that larger value is
        // caught by the :186 guard first and never reaches :227.
        let scan = WavScan {
            fmt: fmt_pcm(),
            fact: None,
        };
        let res = synthesize_layout(&scan, 0, u32::MAX as u64, &[], &[]);
        assert_eq!(res, Err(FormatError::TooLarge));
    }

    // Documented EQUIVALENT mutants in this file (no test targets them; each was
    // confirmed by hand-apply — the relevant test stays green under the mutation):
    //  * walk_chunks:49  guard `next <= buf.len()` → `true`. When `next > buf.len()`
    //    the mutant sets `pos = next`, but the `while pos + 8 <= buf.len()` test is
    //    then immediately false, so the output Vec is identical to the original's
    //    `break` (the header was pushed before the advance).
    //  * synthesize_layout:186  `audio_length > u32::MAX` (both `==` and `>=`).
    //    `body_len >= audio_length`, so whenever this would fire, `riff_size`
    //    overflows and the :227 guard returns the identical TooLarge.
    //  * synthesize_layout:227  `> → >=` only. Every synthesized chunk is
    //    word-aligned, so `riff_size` is always even; `riff_size == u32::MAX` (odd)
    //    is unreachable, the only point where `>` and `>=` differ.
```

- [ ] **Step 2: Run — expect PASS**

Run: `cargo test -p musefs-format --lib wav::tests::synthesize_rejects_riff_size_overflow`
Expected: PASS.

- [ ] **Step 3: Hand-apply `> → ==` and confirm failure**

Edit `synthesize_layout`: change `if riff_size > u32::MAX as u64` to `==`.
Run: `cargo test -p musefs-format --lib wav::tests::synthesize_rejects_riff_size_overflow`
Expected: FAIL (`Ok(..)` returned, `Err(TooLarge)` expected).
Revert: `git checkout -- musefs-format/src/wav.rs`

- [ ] **Step 4: Confirm each documented equivalent stays green under its mutation**

For each, apply the mutation, run the full wav unit suite, confirm **PASS** (proving
no existing test distinguishes it → equivalent), then revert:
- `walk_chunks:49`: change the match-guard `Some(next) if next <= buf.len() as u64` to `Some(next) if true` → run `cargo test -p musefs-format --lib wav::tests` → PASS.
- `synthesize_layout:186` `> → >=`: change `if audio_length > u32::MAX as u64` to `>=` → PASS.
- `synthesize_layout:186` `> → ==`: change it to `==` → PASS.
- `synthesize_layout:227` `> → >=`: change `if riff_size > u32::MAX as u64` to `>=` → PASS.

Revert after each: `git checkout -- musefs-format/src/wav.rs`

- [ ] **Step 5: Confirm green, then commit**

```bash
cargo test -p musefs-format --lib wav::tests
git add musefs-format/src/wav.rs
git commit -m "test(wav): kill synthesize_layout:227 == mutant; document 4 equivalents"
```

---

## Task 12: C2 — zero-byte embedded art (red-green TDD)

**Files:**
- Test: `musefs-format/tests/wav_synthesize.rs` (add two tests)
- Modify: `musefs-format/src/wav.rs` (`synthesize_layout`, the `build_id3v2_segments` call)

- [ ] **Step 1: Add the two failing tests to `wav_synthesize.rs`**

Append inside `wav_synthesize.rs` (it already imports
`musefs_format::{ArtInput, RegionLayout, Segment, TagInput}` and `WavScan`, and has
`fmt_pcm_16bit_mono` + `assemble`):

```rust
#[test]
fn skips_zero_byte_art() {
    // A degenerate empty embedded picture must not brick the track: synthesis
    // succeeds, emits no ArtImage segment, and preserves the audio.
    let audio = vec![0u8; 8];
    let scan = WavScan {
        fmt: fmt_pcm_16bit_mono(),
        fact: None,
    };
    let tags = vec![TagInput::new("title", "Empty Art")];
    let arts = vec![ArtInput {
        art_id: 1,
        mime: "image/jpeg".to_string(),
        description: String::new(),
        picture_type: 3,
        width: 0,
        height: 0,
        data_len: 0,
    }];

    let layout = synthesize_layout(&scan, 0, audio.len() as u64, &tags, &arts).unwrap();
    assert!(
        !layout
            .segments
            .iter()
            .any(|s| matches!(s, Segment::ArtImage { .. })),
        "zero-byte art must not produce an ArtImage segment"
    );

    let bytes = assemble(&layout, &audio, &[]);
    let bounds = musefs_format::wav::locate_audio(&bytes).unwrap();
    assert_eq!(
        &bytes[bounds.audio_offset as usize..(bounds.audio_offset + bounds.audio_length) as usize],
        &audio[..]
    );
}

#[test]
fn keeps_real_art_when_mixed_with_empty() {
    let audio = vec![0u8; 8];
    let art_bytes = vec![0xCDu8; 64];
    let scan = WavScan {
        fmt: fmt_pcm_16bit_mono(),
        fact: None,
    };
    let tags = vec![TagInput::new("title", "Mixed")];
    let arts = vec![
        ArtInput {
            art_id: 1,
            mime: "image/jpeg".to_string(),
            description: String::new(),
            picture_type: 3,
            width: 0,
            height: 0,
            data_len: 0,
        },
        ArtInput {
            art_id: 2,
            mime: "image/jpeg".to_string(),
            description: String::new(),
            picture_type: 3,
            width: 0,
            height: 0,
            data_len: art_bytes.len() as u64,
        },
    ];

    let layout = synthesize_layout(&scan, 0, audio.len() as u64, &tags, &arts).unwrap();
    let art_segs: Vec<&Segment> = layout
        .segments
        .iter()
        .filter(|s| matches!(s, Segment::ArtImage { .. }))
        .collect();
    assert_eq!(art_segs.len(), 1, "only the real art survives");
    assert!(matches!(
        art_segs[0],
        Segment::ArtImage { art_id: 2, len: 64 }
    ));
}
```

- [ ] **Step 2: Run — expect FAIL (red)**

Run: `cargo test -p musefs-format --test wav_synthesize skips_zero_byte_art keeps_real_art_when_mixed_with_empty`
Expected: FAIL. `skips_zero_byte_art` fails with `Err(InvalidLayout)` on `.unwrap()`
(the zero-byte art becomes `ArtImage { len: 0 }`, which `RegionLayout::validate`
rejects as `EmptySegment`). If instead it PASSES, stop — the assumption is wrong and
no production change is needed; re-verify against the current `synthesize_layout`.

- [ ] **Step 3: Apply the WAV-local filter in `synthesize_layout`**

In `musefs-format/src/wav.rs`, locate the `build_id3v2_segments` call:

```rust
    let (tag_segments, tag_len) = crate::mp3::build_id3v2_segments(tags, arts)?;
```

Replace it with a filtered pass (mirrors `flac.rs`'s `data_len > 0` skip):

```rust
    // Skip degenerate zero-byte art: an empty picture would become an
    // `ArtImage { len: 0 }`, which `RegionLayout::validate` rejects, bricking the
    // whole track. Filtering keeps byte-identity untouched (only whether an empty
    // APIC is emitted changes). Mirrors `flac.rs::synthesize_layout`.
    let nonempty_arts: Vec<ArtInput> = arts.iter().filter(|a| a.data_len > 0).cloned().collect();
    let (tag_segments, tag_len) = crate::mp3::build_id3v2_segments(tags, &nonempty_arts)?;
```

(`ArtInput` is already imported at the top of `wav.rs` via
`use crate::input::{ArtInput, TagInput};` and derives `Clone`.)

- [ ] **Step 4: Run — expect PASS (green)**

Run: `cargo test -p musefs-format --test wav_synthesize`
Expected: PASS (all wav_synthesize tests, including the two new ones and the
existing `embeds_full_fidelity_id3_tag_with_art`).

- [ ] **Step 5: Commit**

```bash
git add musefs-format/src/wav.rs musefs-format/tests/wav_synthesize.rs
git commit -m "fix(wav): skip zero-byte embedded art in synthesize_layout (#16)"
```

---

## Task 13: C3 fixture — `write_wav` in core test common

**Files:**
- Modify: `musefs-core/tests/common/mod.rs` (add `write_wav`)

- [ ] **Step 1: Add `write_wav` after `write_flac`**

```rust
/// Write a minimal valid PCM WAV (`fmt ` + `data`) to `path`, returning
/// (audio_offset, audio_length) of the `data` payload. Tags are applied via the DB
/// by the caller (mirrors how `write_flac` is paired with `replace_tags`).
pub fn write_wav(path: &Path, audio: &[u8]) -> (i64, i64) {
    let mut fmt = Vec::new();
    fmt.extend_from_slice(&1u16.to_le_bytes()); // PCM
    fmt.extend_from_slice(&1u16.to_le_bytes()); // mono
    fmt.extend_from_slice(&44_100u32.to_le_bytes()); // sample rate
    fmt.extend_from_slice(&88_200u32.to_le_bytes()); // byte rate
    fmt.extend_from_slice(&2u16.to_le_bytes()); // block align
    fmt.extend_from_slice(&16u16.to_le_bytes()); // bits per sample

    let mut body = Vec::new();
    for (id, payload) in [(&b"fmt "[..], &fmt[..]), (&b"data"[..], audio)] {
        body.extend_from_slice(id);
        body.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        body.extend_from_slice(payload);
    }
    let mut bytes = b"RIFF".to_vec();
    bytes.extend_from_slice(&((body.len() + 4) as u32).to_le_bytes());
    bytes.extend_from_slice(b"WAVE");
    bytes.extend_from_slice(&body);

    let audio_offset = (bytes.len() - audio.len()) as i64;
    std::fs::write(path, &bytes).unwrap();
    (audio_offset, audio.len() as i64)
}
```

- [ ] **Step 2: Confirm the crate's tests still compile**

Run: `cargo test -p musefs-core --test proptest_read_fidelity --no-run`
Expected: compiles (the helper is `pub` in a `#![allow(dead_code)]` module, so being
unused yet is fine).

- [ ] **Step 3: Commit**

```bash
git add musefs-core/tests/common/mod.rs
git commit -m "test(core): add write_wav fixture helper"
```

---

## Task 14: C3 — four WAV read-fidelity properties

**Files:**
- Modify: `musefs-core/tests/proptest_read_fidelity.rs`

- [ ] **Step 1: Import `write_wav` and add the two WAV builders**

Change the import near the top:

```rust
use common::write_flac;
```

to:

```rust
use common::{write_flac, write_wav};
```

Then add, after the existing `build_with_art` function (before the `proptest!`
block):

```rust
/// Like `build`, but writes a WAV backing file and registers it as `Format::Wav`.
fn build_wav(audio: &[u8], title: &str) -> (tempfile::TempDir, Db, i64, Vec<u8>) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("song.wav");
    let (audio_offset, audio_length) = write_wav(&path, audio);
    let meta = std::fs::metadata(&path).unwrap();
    let db = Db::open_in_memory().unwrap();
    let id = db
        .upsert_track(&NewTrack {
            backing_path: path.to_string_lossy().to_string(),
            format: Format::Wav,
            audio_offset,
            audio_length,
            backing_size: meta.len() as i64,
            backing_mtime: meta
                .modified()
                .unwrap()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs() as i64,
        })
        .unwrap();
    db.replace_tags(id, &[Tag::new("title", title, 0)]).unwrap();
    (dir, db, id, audio.to_vec())
}

/// Like `build_with_art`, but for a WAV backing file. WAV embeds art via the shared
/// id3 path, so the resolved layout contains an `ArtImage` segment.
fn build_wav_with_art(audio: &[u8], title: &str, art: &[u8]) -> (tempfile::TempDir, Db, i64) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("song.wav");
    let (audio_offset, audio_length) = write_wav(&path, audio);
    let meta = std::fs::metadata(&path).unwrap();
    let db = Db::open_in_memory().unwrap();
    let id = db
        .upsert_track(&NewTrack {
            backing_path: path.to_string_lossy().to_string(),
            format: Format::Wav,
            audio_offset,
            audio_length,
            backing_size: meta.len() as i64,
            backing_mtime: meta
                .modified()
                .unwrap()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs() as i64,
        })
        .unwrap();
    db.replace_tags(id, &[Tag::new("title", title, 0)]).unwrap();
    let art_id = db
        .upsert_art(&NewArt {
            mime: "image/png".to_string(),
            width: Some(8),
            height: Some(8),
            data: art.to_vec(),
        })
        .unwrap();
    db.set_track_art(
        id,
        &[TrackArt {
            art_id,
            picture_type: 3,
            description: "front".to_string(),
            ordinal: 0,
        }],
    )
    .unwrap();
    (dir, db, id)
}
```

- [ ] **Step 2: Add the four WAV properties inside the `proptest!` block**

Add these after `read_at_art_window_serves_blob`, still inside the `proptest! { .. }`
block (before its closing brace):

```rust
    #[test]
    fn wav_read_at_preserves_backing_audio(
        audio in proptest::collection::vec(any::<u8>(), 1..512),
        title in "[ -~]{0,32}",
    ) {
        let (_dir, db, id, original) = build_wav(&audio, &title);
        let resolved = HeaderCache::new(Mode::Synthesis).resolve(&db, id).unwrap();
        let whole = read_at(&resolved, &db, 0, resolved.total_len).unwrap();
        prop_assert_eq!(whole.len() as u64, resolved.total_len);
        // The served `data` payload is byte-identical to the original audio. It is
        // not the trailing bytes (a word-align pad may follow), so locate it.
        let bounds = musefs_format::wav::locate_audio(&whole).unwrap();
        prop_assert_eq!(
            &whole[bounds.audio_offset as usize..(bounds.audio_offset + bounds.audio_length) as usize],
            &original[..]
        );
    }

    #[test]
    fn wav_read_at_partial_windows_match_whole(
        audio in proptest::collection::vec(any::<u8>(), 1..512),
        title in "[ -~]{0,32}",
        a in 0usize..4096,
        b in 0usize..4096,
    ) {
        let (_dir, db, id, _orig) = build_wav(&audio, &title);
        let resolved = HeaderCache::new(Mode::Synthesis).resolve(&db, id).unwrap();
        let total = resolved.total_len;
        let whole = read_at(&resolved, &db, 0, total).unwrap();
        let offset = (a as u64) % (total + 1);
        let len = (b as u64) % (total - offset + 1);
        let got = read_at(&resolved, &db, offset, len).unwrap();
        prop_assert_eq!(got.len() as u64, len);
        prop_assert_eq!(&got[..], &whole[offset as usize..(offset + len) as usize]);
    }

    #[test]
    fn wav_read_at_windows_spanning_header_seam(
        audio in proptest::collection::vec(any::<u8>(), 1..512),
        title in "[ -~]{0,32}",
        before in 0usize..4096,
        after in 0usize..4096,
    ) {
        let (_dir, db, id, _orig) = build_wav(&audio, &title);
        let resolved = HeaderCache::new(Mode::Synthesis).resolve(&db, id).unwrap();
        let total = resolved.total_len;
        let hlen = resolved.layout.header_len();
        prop_assume!(hlen > 0 && hlen < total);
        let start = hlen - 1 - (before as u64 % hlen);
        let end = hlen + 1 + (after as u64 % (total - hlen));
        let whole = read_at(&resolved, &db, 0, total).unwrap();
        let got = read_at(&resolved, &db, start, end - start).unwrap();
        prop_assert_eq!(&got[..], &whole[start as usize..end as usize]);
    }

    #[test]
    fn wav_read_at_art_window_serves_blob(
        audio in proptest::collection::vec(any::<u8>(), 1..256),
        art in proptest::collection::vec(any::<u8>(), 1..256),
        a in 0usize..4096,
        b in 0usize..4096,
    ) {
        let (_dir, db, id) = build_wav_with_art(&audio, "T", &art);
        let resolved = HeaderCache::new(Mode::Synthesis).resolve(&db, id).unwrap();
        let total = resolved.total_len;
        let whole = read_at(&resolved, &db, 0, total).unwrap();

        // Locate the ArtImage segment's byte offset by summing the serving lengths
        // of the segments before it.
        let mut art_off = 0u64;
        let mut art_len = None;
        for s in &resolved.layout.segments {
            match s {
                Segment::ArtImage { len, .. } => {
                    art_len = Some(*len);
                    break;
                }
                Segment::Inline(bytes) => art_off += bytes.len() as u64,
                Segment::BackingAudio { len, .. } => art_off += *len,
                other => panic!("unexpected WAV segment: {other:?}"),
            }
        }
        let art_len = art_len.expect("layout has an ArtImage segment");
        prop_assert_eq!(art_len, art.len() as u64);
        prop_assert_eq!(
            &whole[art_off as usize..(art_off + art_len) as usize],
            &art[..]
        );
        let local_off = (a as u64) % (art_len + 1);
        let offset = art_off + local_off;
        let len = (b as u64) % (art_len - local_off + 1);
        let got = read_at(&resolved, &db, offset, len).unwrap();
        prop_assert_eq!(&got[..], &whole[offset as usize..(offset + len) as usize]);
    }
```

- [ ] **Step 3: Run the property suite — expect PASS**

Run: `cargo test -p musefs-core --test proptest_read_fidelity`
Expected: PASS (all 8 properties — 4 FLAC + 4 WAV). If `wav_read_at_art_window_serves_blob`
ever fails the `expect("layout has an ArtImage segment")`, that means WAV art is not
producing an `ArtImage` segment — investigate `synthesize_layout` / Task 12 before
proceeding.

- [ ] **Step 4: Commit**

```bash
git add musefs-core/tests/proptest_read_fidelity.rs
git commit -m "test(core): broaden read-fidelity proptests to WAV (#5)"
```

---

## Task 15: C4 — inventory annotations, tracking doc, final verification

**Files:**
- Modify: `docs/superpowers/specs/test-audit-remediation/2026-05-29-mutation-inventory.md`
- Modify: `docs/superpowers/specs/test-audit-remediation/2026-05-29-remediation-tracking.md`

- [ ] **Step 1: Annotate the `wav.rs` rows in the inventory**

In `2026-05-29-mutation-inventory.md`, append a status to each `wav.rs` survivor row
in the `musefs-format` survivor table, matching the 3a `flac.rs` convention
(`missed → **killed (phase 3d)**` or `missed → **equivalent**`). Specifically:

- `wav.rs:24` (×2), `:47`, `:67`, `:71`, `:155` (×3), `:168` (×2), `:207` (×2),
  `:245`, `:246`, `:248`, `:249`, `:300`, and the six `:119`–`:124` arm rows, plus
  `:227` `> → ==` → `**killed (phase 3d)**`.
- `wav.rs:186` (×2) and `wav.rs:227` `> → >=` → `**equivalent**`.

(`wav.rs:49` is not a row in the inventory's survivor table — it appears only as a
caught/structural note; if a `walk_chunks:49` row exists, mark it `**equivalent**`.)

- [ ] **Step 2: Update the tracking doc**

In `2026-05-29-remediation-tracking.md`, update the Phase 3 section: mark **3d done**,
recording that the 24 killable `wav.rs` survivors are killed, the four equivalents
(`walk_chunks:49`, `synthesize_layout:186` ×2, `synthesize_layout:227` `>=`) are
documented, and the WAV dimensions of findings #5 (read-fidelity proptests) and #16
(zero-byte-art skip) are addressed. If 3d completes all non-Ogg formats, note Phase 3
status accordingly (3b MP3 / 3c MP4 status is unchanged by this work).

- [ ] **Step 3: Full workspace verification**

Run each and confirm all green:

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test --workspace
cargo test -p musefs-format --features fuzzing
```

Expected: all PASS. Pay attention to the new `wav::tests::*`, the `wav_synthesize`
tests, and the eight `proptest_read_fidelity` properties.

- [ ] **Step 4: Commit**

```bash
git add docs/superpowers/specs/test-audit-remediation/2026-05-29-mutation-inventory.md docs/superpowers/specs/test-audit-remediation/2026-05-29-remediation-tracking.md
git commit -m "docs(phase3d): annotate killed/equivalent wav mutants; mark phase 3d done"
```

---

## Notes for the implementer

- **Never leave a mutation applied.** Every hand-apply step is paired with a
  `git checkout -- musefs-format/src/wav.rs`. If a later test behaves strangely, first
  confirm no mutation is still in the working tree (`git diff`).
- **Run targeted tests during hand-apply**, the full suite only at task boundaries.
- The in-`wav.rs` test names use the path `wav::tests::<name>`; filter with
  `cargo test -p musefs-format --lib wav::tests::<name>`.
- If `cargo fmt` reshapes any added block, accept its formatting — do not fight it.
```
