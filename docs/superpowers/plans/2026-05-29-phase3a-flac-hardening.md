# Phase 3a — FLAC Hardening Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Kill the 45 `flac.rs` mutation survivors with additive in-module tests, broaden `proptest_read_fidelity` on FLAC (finding #5), and make FLAC synthesis skip zero-byte embedded art so it never bricks a track (finding #16). One small scoped production change (the zero-byte-art skip, Task 7); everything else is additive tests. Byte-identity for audio is untouched.

**Architecture:** All `flac.rs` kills live in one new `#[cfg(test)] mod tests` inside `musefs-format/src/flac.rs`, which can call `pub`, `pub(crate)`, and private helpers directly via `use super::*` and builds byte-precise fixtures with small independent local helpers (it cannot use `musefs-format/tests/common`). The cross-cutting findings live in their own integration files: #5 in `musefs-core/tests/proptest_read_fidelity.rs`, #16 in `musefs-format/tests/synthesize_art.rs`.

**Tech Stack:** Rust, `cargo test`, `proptest` (existing dev-dep), `tempfile` (existing), `metaflac` (existing dev-dep). No new dependencies.

**Spec:** `docs/superpowers/specs/test-audit-remediation/2026-05-29-phase3a-flac-hardening-design.md`
**Survivor data:** `docs/superpowers/specs/test-audit-remediation/2026-05-29-mutation-inventory.md`

---

## The hand-apply verification method (use in every kill step)

cargo-mutants is not available locally. To prove a new test kills a specific
survivor, for each targeted `file:line: mutation`:

1. Run the new test → it passes (production code is correct).
2. Apply the exact mutation at that line, rerun **just that test** → it must **fail**
   (a failed assertion *or a panic* both count as a kill).
3. Revert (`git checkout -- <file>`), rerun → passes again.

If step 2 still passes, the test does not kill the mutant. Either strengthen it, or
— if the mutation provably yields identical behavior — record it as an **equivalent
mutant** (Task 8). Never leave a mutation applied.

**Re-verify line numbers first.** Survivor line numbers are from the phase-1
inventory; the test module is appended at the end of `flac.rs`, so the survivor
lines do not move while you work, but confirm each target by its code pattern
before hand-applying.

## Equivalent mutants (confirm green under the mutation, then record in Task 8)

Verified by reading the source; do **not** write tests to "kill" these:

- **Disjoint-bitfield `| → ^`** (operands occupy non-overlapping bits, so `|` ≡ `^` ≡ `+`):
  `parse_blocks:50, :51`; `read_vorbis_comments:200` (the `^` variant), `:201`;
  `read_pictures:290` (the `^` variant), `:291`; `push_block_header:99`
  (`0x80 | (block_type & 0x7F)`, bit 7 vs bits 6–0).
- **Inclusive-bound `> → >=` in `parse_picture_block`** (`:237`, `:245`): the only
  differing input is `*_end == body.len()`, where the original proceeds and then
  fails at the *next* `read_u32_be` → `Err(Malformed)`, identical to the mutant's
  immediate `Err(Malformed)`. Equivalent. (Their `> → ==` siblings **are** killable
  — see Task 4.)

## Pre-flight (run once before Task 1)

- [ ] **Confirm baseline green on the phase-3a branch**

```bash
git rev-parse --abbrev-ref HEAD          # expect: phase3a-flac-hardening
cargo test -p musefs-format --features fuzzing flac
cargo test -p musefs-core proptest_read_fidelity
```
Expected: both green.

---

## Task 1: test module scaffold + `read_u32_be` and `push_block_header` kills

Kills `read_u32_be:219, :224` and `push_block_header:101`. Confirms `:99` equivalent.
Establishes the local fixture helpers used by Tasks 2–4.

**Files:**
- Modify (tests only): `musefs-format/src/flac.rs` — append a `#[cfg(test)] mod tests` after `read_pictures` (end of file, line 306).

- [ ] **Step 1: Add the test module scaffold + the first two tests**

Append to `musefs-format/src/flac.rs`. **No fixture helpers yet** — they are
introduced in the task that first uses them, so every commit stays free of
`dead_code` warnings (the pre-commit hook runs `clippy -D warnings`).

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_u32_be_assembles_big_endian_and_guards_length() {
        let data = [0x11u8, 0x22, 0x33, 0x44, 0x55];
        assert_eq!(read_u32_be(&data, 0).unwrap(), 0x1122_3344);
        // pins :224 (`+` -> `*`): at pos=1 the second byte is data[2]=0x33, not data[1].
        // pins :219 (`>` -> `==`/`>=`): pos+4 == len (5) is valid, so this unwrap must
        // succeed — a mutated bound returns Err here and the unwrap panics.
        assert_eq!(read_u32_be(&data, 1).unwrap(), 0x2233_4455);
        assert_eq!(read_u32_be(&data, 2), Err(FormatError::Malformed));
    }

    #[test]
    fn push_block_header_emits_24bit_length_big_endian() {
        // pins :101 (`>>16` -> `<<16`): high byte 0x12 must land in out[1].
        let mut out = Vec::new();
        push_block_header(&mut out, BLOCK_PICTURE, 0x12_3456, false);
        assert_eq!(out, vec![BLOCK_PICTURE, 0x12, 0x34, 0x56]);
        // :99 is equivalent, but exercise the is_last/0x80 path anyway.
        let mut last = Vec::new();
        push_block_header(&mut last, BLOCK_VORBIS_COMMENT, 0, true);
        assert_eq!(last, vec![0x80 | BLOCK_VORBIS_COMMENT, 0x00, 0x00, 0x00]);
    }
}
```

- [ ] **Step 2: Run the tests, expect PASS**

```bash
cargo test -p musefs-format --features fuzzing flac::tests::read_u32_be_assembles_big_endian_and_guards_length flac::tests::push_block_header_emits_24bit_length_big_endian
```
Expected: both pass.

- [ ] **Step 3: Hand-apply-verify the kills**

- `:219` `pos + 4 > data.len()` → `>=` (and `==`): rerun `read_u32_be_*` → FAIL
  (the `read_u32_be(&data, 1).is_ok()` assertion). Revert.
- `:224` `data[pos + 1]` → `data[pos * 1]`: rerun → FAIL (the `0x2233_4455`
  assertion). Revert.
- `:101` `(body_len >> 16)` → `(body_len << 16)`: rerun `push_block_header_*` →
  FAIL (`out[1]` becomes `0x00`). Revert.
- `:99` `| → ^`: rerun → still PASS (equivalent: `0x80` and `& 0x7F` are disjoint).
  Note for Task 8. Revert.

- [ ] **Step 4: Commit**

```bash
git add musefs-format/src/flac.rs
git commit -m "test(flac): read_u32_be + push_block_header kills; test scaffold"
```

---

## Task 2: `parse_blocks` kills

Kills `parse_blocks:37, :43, :49`. Confirms `:50`/`:51` equivalent.

**Files:**
- Modify (tests only): `musefs-format/src/flac.rs` `#[cfg(test)] mod tests`.

- [ ] **Step 1: Add the shared fixture helpers + the tests**

Add inside `mod tests`. The two helpers are defined here (first use) and reused by
Tasks 3–4:

```rust
    /// One FLAC metadata block: 4-byte header (last-flag, type, 24-bit BE length)
    /// + body, built independently of production framing so a mutation in
    /// `push_block_header` cannot mask a fixture. `len_override` lets a test claim a
    /// length different from `body.len()`.
    fn raw_block(block_type: u8, body: &[u8], last: bool, len_override: Option<usize>) -> Vec<u8> {
        let n = len_override.unwrap_or(body.len());
        let mut v = vec![(if last { 0x80 } else { 0 }) | (block_type & 0x7F)];
        v.push((n >> 16) as u8);
        v.push((n >> 8) as u8);
        v.push(n as u8);
        v.extend_from_slice(body);
        v
    }

    /// `fLaC` + the given blocks (no audio).
    fn flac_with(blocks: &[Vec<u8>]) -> Vec<u8> {
        let mut f = b"fLaC".to_vec();
        for b in blocks {
            f.extend_from_slice(b);
        }
        f
    }

    #[test]
    fn parse_blocks_rejects_short_and_wrong_marker() {
        // :37 `< -> ==`: 3-byte input -> original short-circuits NotFlac; the mutant
        // evaluates &data[0..4] on 3 bytes -> panic. Asserting Err(NotFlac) kills it.
        assert_eq!(parse_blocks(b"fLa"), Err(FormatError::NotFlac));
        // :37 `< -> <=`: a 4-byte fLaC-only file. Original proceeds then hits the
        // loop guard -> Malformed; the `<=` mutant short-circuits to NotFlac.
        assert_eq!(parse_blocks(b"fLaC"), Err(FormatError::Malformed));
        assert_eq!(parse_blocks(b"XXXX____"), Err(FormatError::NotFlac));
    }

    #[test]
    fn parse_blocks_guards_truncated_block_header() {
        // 5 bytes: marker + 1 header byte. Original: pos+4=8 > 5 -> Malformed.
        // :43 `+ -> -` (0 > 5 false) and `> -> ==` (8 == 5 false) both fall through
        // and panic reading data[5..8].
        assert_eq!(parse_blocks(b"fLaC\x80"), Err(FormatError::Malformed));
    }

    #[test]
    fn parse_blocks_accepts_header_flush_with_end() {
        // Single last STREAMINFO, empty body, no audio: the final header occupies the
        // last 4 bytes, so pos+4 == data.len() at the loop guard. Original (`>`)
        // proceeds and returns audio_offset == len; the :43 `> -> >=` mutant rejects.
        let file = flac_with(&[raw_block(BLOCK_STREAMINFO, &[], true, None)]);
        let meta = parse_blocks(&file).unwrap();
        assert_eq!(meta.audio_offset, 8);
    }

    #[test]
    fn parse_blocks_decodes_24bit_length_high_byte() {
        // STREAMINFO header claims length 0x010000 (high byte set) over an empty body.
        // Original: len = 65536 -> body_end > data.len() -> Malformed.
        // :49 `<<16 -> >>16`: (0x01 >> 16) = 0 -> len = 0 -> body fits -> Ok.
        // (:50/:51 `| -> ^` are equivalent here: the shifted bytes are disjoint.)
        let file = flac_with(&[raw_block(BLOCK_STREAMINFO, &[], true, Some(0x01_0000))]);
        assert_eq!(parse_blocks(&file), Err(FormatError::Malformed));
    }

    #[test]
    fn parse_blocks_preserves_structural_blocks() {
        // Positive decode: a normal STREAMINFO (34-byte body) + audio boundary.
        let si = vec![0xAA; 34];
        let file = flac_with(&[raw_block(BLOCK_STREAMINFO, &si, true, None)]);
        let meta = parse_blocks(&file).unwrap();
        assert_eq!(meta.audio_offset, 4 + 4 + 34);
        assert_eq!(meta.preserved.len(), 1);
        assert_eq!(meta.preserved[0].block_type, BLOCK_STREAMINFO);
        assert_eq!(meta.preserved[0].body, si);
    }
```

- [ ] **Step 2: Run, expect PASS**

```bash
cargo test -p musefs-format --features fuzzing flac::tests::parse_blocks
```
Expected: all five pass.

- [ ] **Step 3: Hand-apply-verify the kills**

- `:37` `< → ==`: `parse_blocks_rejects_short_*` FAILs (panic on `b"fLa"`). Revert.
- `:37` `< → <=`: same test FAILs (`b"fLaC"` returns NotFlac not Malformed). Revert.
- `:43` `+ → -`: `parse_blocks_guards_truncated_block_header` FAILs (panic). Revert.
- `:43` `> → ==`: same test FAILs (panic). Revert.
- `:43` `> → >=`: `parse_blocks_accepts_header_flush_with_end` FAILs (Malformed). Revert.
- `:49` `<< → >>`: `parse_blocks_decodes_24bit_length_high_byte` FAILs (returns Ok). Revert.
- `:50` `| → ^`, `:51` `| → ^`: rerun the whole `parse_blocks` set → still PASS
  (equivalent). Note for Task 8. Revert each.

- [ ] **Step 4: Commit**

```bash
git add musefs-format/src/flac.rs
git commit -m "test(flac): parse_blocks bounds + 24-bit length kills"
```

---

## Task 3: `read_vorbis_comments` kills

Kills `read_vorbis_comments:188, :193, :199, :200` (`&` and `<<8`), `:204`. Confirms
`:200` (`^`) and `:201` equivalent.

**Files:**
- Modify (tests only): `musefs-format/src/flac.rs` `#[cfg(test)] mod tests`.

- [ ] **Step 1: Add the `vc_body` helper + the tests**

`raw_block`/`flac_with` already exist in the module (Task 2). Add `vc_body` (first
use here):

```rust
    /// A VORBIS_COMMENT body: u32-LE vendor length, vendor, u32-LE count, then each
    /// comment as u32-LE length + bytes.
    fn vc_body(vendor: &str, comments: &[&str]) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(&(vendor.len() as u32).to_le_bytes());
        v.extend_from_slice(vendor.as_bytes());
        v.extend_from_slice(&(comments.len() as u32).to_le_bytes());
        for c in comments {
            v.extend_from_slice(&(c.len() as u32).to_le_bytes());
            v.extend_from_slice(c.as_bytes());
        }
        v
    }

    #[test]
    fn read_vorbis_comments_returns_pairs_and_guards_marker() {
        // Happy path: VC block is the last block with no audio, so body_end == len.
        // This also pins :204 (`>` -> `==`/`>=`): the mutant would reject (Malformed)
        // and the unwrap below would panic.
        let vc = vc_body("v", &["TITLE=Hi", "ARTIST=Me"]);
        let file = flac_with(&[raw_block(BLOCK_VORBIS_COMMENT, &vc, true, None)]);
        let got = read_vorbis_comments(&file).unwrap();
        assert_eq!(
            got,
            vec![
                ("TITLE".to_string(), "Hi".to_string()),
                ("ARTIST".to_string(), "Me".to_string()),
            ]
        );
        // :188 `< -> ==` and `|| -> &&`: 3-byte input -> original NotFlac via
        // short-circuit; both mutants force &data[0..4] -> panic.
        assert_eq!(read_vorbis_comments(b"fLa"), Err(FormatError::NotFlac));
        // :188 `< -> <=`: 4-byte fLaC -> original Malformed; mutant NotFlac.
        assert_eq!(read_vorbis_comments(b"fLaC"), Err(FormatError::Malformed));
    }

    #[test]
    fn read_vorbis_comments_guards_block_walk() {
        // :193 `+ -> -` and `> -> ==`: truncated header -> original Malformed,
        // mutants fall through and panic.
        assert_eq!(read_vorbis_comments(b"fLaC\x80"), Err(FormatError::Malformed));
        // :193 `> -> >=`: a non-VC last block flush with end -> original returns the
        // empty vec; the `>=` mutant rejects at the loop guard.
        let file = flac_with(&[raw_block(BLOCK_STREAMINFO, &[], true, None)]);
        assert_eq!(read_vorbis_comments(&file).unwrap(), Vec::new());
    }

    #[test]
    fn read_vorbis_comments_decodes_24bit_length() {
        // :199 `<<16 -> >>16` AND :200 `| -> &`: high length byte set over a short
        // body. Original len = 0x10000 -> Malformed. `>>16` -> len 0 -> Ok; `&` ->
        // (0x10000 & 0) -> len 0 -> Ok. Either mutant returns Ok instead of Malformed.
        let hi = flac_with(&[raw_block(BLOCK_STREAMINFO, &[], true, Some(0x01_0000))]);
        assert_eq!(read_vorbis_comments(&hi), Err(FormatError::Malformed));
        // :200 `<<8 -> >>8`: mid length byte set, high byte 0. Original len = 0x100
        // -> Malformed; `>>8` -> len 0 -> Ok.
        let mid = flac_with(&[raw_block(BLOCK_STREAMINFO, &[], true, Some(0x00_0100))]);
        assert_eq!(read_vorbis_comments(&mid), Err(FormatError::Malformed));
        // (:200 `| -> ^` and :201 `| -> ^` are equivalent: disjoint shifted bytes.)
    }
```

- [ ] **Step 2: Run, expect PASS**

```bash
cargo test -p musefs-format --features fuzzing flac::tests::read_vorbis_comments
```
Expected: all three pass.

- [ ] **Step 3: Hand-apply-verify the kills**

- `:188` `< → ==`, `< → <=`, `|| → &&`: `..._guards_marker` FAILs (panic or wrong error). Revert each.
- `:204` `> → ==`, `> → >=`: `..._guards_marker` FAILs (the happy unwrap panics). Revert each.
- `:193` `+ → -`, `> → ==`, `> → >=`: `..._guards_block_walk` FAILs. Revert each.
- `:199` `<< → >>`, `:200` `| → &`, `:200` `<< → >>`: `..._decodes_24bit_length` FAILs. Revert each.
- `:200` `| → ^`, `:201` `| → ^`: rerun the set → still PASS (equivalent). Note for Task 8. Revert.

- [ ] **Step 4: Commit**

```bash
git add musefs-format/src/flac.rs
git commit -m "test(flac): read_vorbis_comments block-walk + length kills"
```

---

## Task 4: `parse_picture_block` + `read_pictures` kills

Kills `parse_picture_block:237` (`==`), `:245` (`==`), `:261`; `read_pictures:277,
:283, :289, :290` (`&` and `<<8`), `:294`. Confirms `:237`/`:245` (`>=`),
`:290` (`^`), `:291` equivalent.

**Files:**
- Modify (tests only): `musefs-format/src/flac.rs` `#[cfg(test)] mod tests`.

- [ ] **Step 1: Add the `picture_body` helper + the `parse_picture_block` tests**

`raw_block`/`flac_with` already exist (Task 2). Add `picture_body` (first use here):

```rust
    /// A FLAC PICTURE block body (big-endian fields), independent of production.
    fn picture_body(ptype: u32, mime: &str, desc: &str, w: u32, h: u32, data: &[u8]) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(&ptype.to_be_bytes());
        v.extend_from_slice(&(mime.len() as u32).to_be_bytes());
        v.extend_from_slice(mime.as_bytes());
        v.extend_from_slice(&(desc.len() as u32).to_be_bytes());
        v.extend_from_slice(desc.as_bytes());
        v.extend_from_slice(&w.to_be_bytes());
        v.extend_from_slice(&h.to_be_bytes());
        v.extend_from_slice(&0u32.to_be_bytes()); // depth
        v.extend_from_slice(&0u32.to_be_bytes()); // colors
        v.extend_from_slice(&(data.len() as u32).to_be_bytes());
        v.extend_from_slice(data);
        v
    }

    #[test]
    fn parse_picture_block_roundtrips_fields() {
        let body = picture_body(3, "image/png", "desc", 4, 5, b"PIXELS");
        let p = parse_picture_block(&body).unwrap();
        assert_eq!(p.picture_type, 3);
        assert_eq!(p.mime, "image/png");
        assert_eq!(p.description, "desc");
        assert_eq!(p.width, 4);
        assert_eq!(p.height, 5);
        assert_eq!(p.data, b"PIXELS");
    }

    #[test]
    fn parse_picture_block_guards_field_bounds() {
        // :237 `> -> ==` (mime bound): claim mime_len far past the end. Original
        // Malformed; the `==` mutant falls through to slice body[8..8+mime_len] -> panic.
        let mut bad_mime = 3u32.to_be_bytes().to_vec();
        bad_mime.extend_from_slice(&16u32.to_be_bytes()); // mime_len = 16
        bad_mime.extend_from_slice(b"ab"); // only 2 bytes present
        assert_eq!(parse_picture_block(&bad_mime), Err(FormatError::Malformed));

        // :245 `> -> ==` (desc bound): valid mime, then claim desc_len past the end.
        let mut bad_desc = 3u32.to_be_bytes().to_vec();
        bad_desc.extend_from_slice(&3u32.to_be_bytes()); // mime_len = 3
        bad_desc.extend_from_slice(b"png");
        bad_desc.extend_from_slice(&16u32.to_be_bytes()); // desc_len = 16
        bad_desc.extend_from_slice(b"x"); // only 1 byte present
        assert_eq!(parse_picture_block(&bad_desc), Err(FormatError::Malformed));

        // :261 `> -> <` (data bound): a fully valid picture body with TRAILING bytes.
        // Original ignores the trailing byte (data_end < len, not >) and returns Ok;
        // the `<` mutant rejects (data_end < len -> Malformed).
        let mut trailing = picture_body(3, "png", "", 1, 1, b"DA");
        trailing.push(0xFF); // one extra trailing byte
        assert!(parse_picture_block(&trailing).is_ok());
    }
```

- [ ] **Step 2: Add the `read_pictures` tests**

```rust
    #[test]
    fn read_pictures_extracts_and_guards_marker() {
        // Happy path: one PICTURE block, last, no audio (body_end == len). Pins :294.
        let pic = picture_body(3, "image/jpeg", "front", 8, 8, b"IMG");
        let file = flac_with(&[raw_block(BLOCK_PICTURE, &pic, true, None)]);
        let pics = read_pictures(&file).unwrap();
        assert_eq!(pics.len(), 1);
        assert_eq!(pics[0].mime, "image/jpeg");
        assert_eq!(pics[0].data, b"IMG");
        // :277 `< -> ==` and `|| -> &&`: 3-byte input -> panic vs NotFlac.
        assert_eq!(read_pictures(b"fLa"), Err(FormatError::NotFlac));
        // :277 `< -> <=`: 4-byte fLaC -> Malformed vs NotFlac.
        assert_eq!(read_pictures(b"fLaC"), Err(FormatError::Malformed));
    }

    #[test]
    fn read_pictures_guards_block_walk_and_length() {
        // :283 `+ -> -`, `> -> ==`: truncated header.
        assert_eq!(read_pictures(b"fLaC\x80"), Err(FormatError::Malformed));
        // :283 `> -> >=`: non-PICTURE last block flush with end -> Ok(empty).
        let none = flac_with(&[raw_block(BLOCK_STREAMINFO, &[], true, None)]);
        assert_eq!(read_pictures(&none).unwrap(), Vec::new());
        // :289 `<<16 -> >>16` and :290 `| -> &`: high length byte over short body.
        let hi = flac_with(&[raw_block(BLOCK_STREAMINFO, &[], true, Some(0x01_0000))]);
        assert_eq!(read_pictures(&hi), Err(FormatError::Malformed));
        // :290 `<<8 -> >>8`: mid length byte, high byte 0.
        let mid = flac_with(&[raw_block(BLOCK_STREAMINFO, &[], true, Some(0x00_0100))]);
        assert_eq!(read_pictures(&mid), Err(FormatError::Malformed));
    }
```

- [ ] **Step 3: Run, expect PASS**

```bash
cargo test -p musefs-format --features fuzzing flac::tests::parse_picture_block flac::tests::read_pictures
```
Expected: all four pass.

- [ ] **Step 4: Hand-apply-verify the kills**

- `:237` `> → ==`: `parse_picture_block_guards_field_bounds` FAILs (panic). Revert.
- `:245` `> → ==`: same test FAILs (panic). Revert.
- `:261` `> → <`: same test FAILs (`trailing` returns Malformed). Revert.
- `:237` `> → >=`, `:245` `> → >=`: rerun → still PASS (equivalent — proceeding still
  hits `Malformed` at the next `read_u32_be`). Note for Task 8. Revert each.
- `:277` (`< → ==`, `< → <=`, `|| → &&`), `:294` (`> → ==`/`>=`):
  `read_pictures_extracts_and_guards_marker` FAILs. Revert each.
- `:283` (`+ → -`, `> → ==`, `> → >=`), `:289` (`<< → >>`), `:290` (`| → &`,
  `<< → >>`): `read_pictures_guards_block_walk_and_length` FAILs. Revert each.
- `:290` `| → ^`, `:291` `| → ^`: rerun → still PASS (equivalent). Note for Task 8. Revert.

- [ ] **Step 5: Commit**

```bash
git add musefs-format/src/flac.rs
git commit -m "test(flac): parse_picture_block + read_pictures kills"
```

---

## Task 5: `synthesize_layout` 24-bit `TooLarge` boundary (C3)

Kills `synthesize_layout:155` (`> → >=`).

**Files:**
- Modify (tests only): `musefs-format/src/flac.rs` `#[cfg(test)] mod tests`.

- [ ] **Step 1: Add the boundary test**

```rust
    #[test]
    fn synthesize_layout_picture_block_size_boundary_is_inclusive() {
        // body_len = picture_body_framing(art).len() + art.data_len. The guard at
        // flac.rs:155 rejects body_len > 0x00FF_FFFF (FLAC's 24-bit block length).
        let scan = FlacScan { audio_offset: 0, audio_length: 0, preserved: vec![] };
        let mk = |data_len: u64| ArtInput {
            art_id: 1,
            mime: "image/png".to_string(),
            description: String::new(),
            picture_type: 3,
            width: 0,
            height: 0,
            data_len,
        };
        // Derive the exact framing length from production rather than hardcoding it
        // (it is independent of the data_len *value* — that field is always 4 bytes).
        // This keeps the boundary correct regardless of the framing's field count.
        let framing_len = picture_body_framing(&mk(0)).len() as u64;
        let at_limit = 0x00FF_FFFF - framing_len; // body_len == 0x00FF_FFFF exactly
        // original `>` accepts the inclusive boundary; the `>=` mutant rejects it.
        // (data_len is only a count; no large allocation occurs.)
        assert!(synthesize_layout(&scan, &[], &[mk(at_limit)]).is_ok());
        // one byte over must still error, pinning the high side of the boundary.
        assert_eq!(
            synthesize_layout(&scan, &[], &[mk(at_limit + 1)]),
            Err(FormatError::TooLarge)
        );
    }
```

- [ ] **Step 2: Run, expect PASS**

```bash
cargo test -p musefs-format --features fuzzing flac::tests::synthesize_layout_picture_block_size_boundary_is_inclusive
```
Expected: pass. (`framing_len` is computed from `picture_body_framing` at runtime,
so the test stays correct if the framing's fixed-field count ever changes; it
asserts the inclusive boundary, not a magic file size.)

- [ ] **Step 3: Hand-apply-verify**

`:155` `body_len > 0x00FF_FFFF` → `>=`: rerun → FAIL (the `.is_ok()` assertion now
returns `Err(TooLarge)`). Revert.

- [ ] **Step 4: Commit**

```bash
git add musefs-format/src/flac.rs
git commit -m "test(flac): synthesize_layout 24-bit picture-size boundary"
```

---

## Task 6: broaden `proptest_read_fidelity` (finding #5)

**Files:**
- Modify (tests only): `musefs-core/tests/proptest_read_fidelity.rs`.

- [ ] **Step 1: Add the art-fixture helper + imports**

At the top of the file, extend the `use` lines and add `build_with_art` after the
existing `build`:

```rust
use musefs_db::{Db, Format, NewArt, NewTrack, Tag, TrackArt};
use musefs_format::Segment;
```

```rust
/// Like `build`, but also inserts an art blob and links it to the track, so the
/// resolved layout contains an `ArtImage` segment. Mirrors the insert+link pattern
/// in `musefs-core/tests/reader.rs::resolve_includes_art_image_segments`.
fn build_with_art(audio: &[u8], title: &str, art: &[u8]) -> (tempfile::TempDir, Db, i64) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("song.flac");
    let (audio_offset, audio_length) = write_flac(&path, &["TITLE=Orig"], audio);
    let meta = std::fs::metadata(&path).unwrap();
    let db = Db::open_in_memory().unwrap();
    let id = db
        .upsert_track(&NewTrack {
            backing_path: path.to_string_lossy().to_string(),
            format: Format::Flac,
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

- [ ] **Step 2: Add the three broadened properties inside the existing `proptest! { ... }` block**

```rust
    #[test]
    fn read_at_partial_windows_match_whole(
        audio in proptest::collection::vec(any::<u8>(), 1..512),
        title in "[ -~]{0,32}",
        a in 0usize..4096,
        b in 0usize..4096,
    ) {
        let (_dir, db, id, _orig) = build(&audio, &title);
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
    fn read_at_windows_spanning_header_seam(
        audio in proptest::collection::vec(any::<u8>(), 1..512),
        title in "[ -~]{0,32}",
        before in 0usize..4096,
        after in 0usize..4096,
    ) {
        let (_dir, db, id, _orig) = build(&audio, &title);
        let resolved = HeaderCache::new(Mode::Synthesis).resolve(&db, id).unwrap();
        let total = resolved.total_len;
        let hlen = resolved.layout.header_len();
        prop_assume!(hlen > 0 && hlen < total);
        let start = hlen - 1 - (before as u64 % hlen); // in [0, hlen)
        let end = hlen + 1 + (after as u64 % (total - hlen)); // in (hlen, total]
        let whole = read_at(&resolved, &db, 0, total).unwrap();
        let got = read_at(&resolved, &db, start, end - start).unwrap();
        prop_assert_eq!(&got[..], &whole[start as usize..end as usize]);
    }

    #[test]
    fn read_at_art_window_serves_blob(
        audio in proptest::collection::vec(any::<u8>(), 1..256),
        art in proptest::collection::vec(any::<u8>(), 1..256),
        a in 0usize..4096,
        b in 0usize..4096,
    ) {
        let (_dir, db, id) = build_with_art(&audio, "T", &art);
        let resolved = HeaderCache::new(Mode::Synthesis).resolve(&db, id).unwrap();
        let total = resolved.total_len;
        let whole = read_at(&resolved, &db, 0, total).unwrap();

        // Locate the ArtImage segment's exact byte offset in the assembled stream by
        // summing the serving lengths of the segments before it. (Asserting the blob
        // appears at this precise offset is robust; a `windows().any()` search would
        // false-positive when a tiny blob coincidentally matches audio bytes.)
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
                other => panic!("unexpected FLAC segment: {other:?}"),
            }
        }
        let art_len = art_len.expect("layout has an ArtImage segment");
        prop_assert_eq!(art_len, art.len() as u64);
        // The art blob is served verbatim at its segment offset.
        prop_assert_eq!(
            &whole[art_off as usize..(art_off + art_len) as usize],
            &art[..]
        );
        // A partial window over the art region matches the independently-read whole.
        let offset = (a as u64) % (total + 1);
        let len = (b as u64) % (total - offset + 1);
        let got = read_at(&resolved, &db, offset, len).unwrap();
        prop_assert_eq!(&got[..], &whole[offset as usize..(offset + len) as usize]);
    }
```

- [ ] **Step 3: Run, expect PASS**

```bash
cargo test -p musefs-core proptest_read_fidelity
```
Expected: all four properties pass (the original `read_at_preserves_backing_audio`
plus the three new ones). If `read_at` rejects an in-range zero-length read, that is
a real bug — investigate before adjusting the test.

- [ ] **Step 4: Commit**

```bash
git add musefs-core/tests/proptest_read_fidelity.rs
git commit -m "test(read): broaden proptest_read_fidelity — partial/seam/art windows (#5)"
```

---

## Task 7: zero-byte embedded-art is skipped at synthesis (finding #16)

This is the one **production change** in 3a: a degenerate picture with no image data
must not be emitted. Today `synthesize_layout` builds `Segment::ArtImage { len: 0 }`,
which `RegionLayout::validate` (layout.rs:102, `EmptySegment`) rejects → `synthesize_layout`
returns `Err(FormatError::InvalidLayout)` → the whole track becomes unreadable. And
ingestion (`scan.rs:162`) only filters art *above* `MAX_ART_BYTES`, so a source file
with an empty PICTURE block reaches synthesis. Fix: skip zero-length art in the FLAC
synthesizer so the track still serves. Byte-identity is unaffected (metadata framing
only). TDD: write the failing test, then make the change.

**Files:**
- Test: `musefs-format/tests/synthesize_art.rs`.
- Modify (production): `musefs-format/src/flac.rs` — `synthesize_layout` (the
  `num_blocks` computation at line 129 and the `for art in arts` loop at line 149).

- [ ] **Step 1: Write the failing tests**

`cover(art_id, data_len)` and `fixture()` already exist in this file. Add:

```rust
#[test]
fn zero_byte_art_is_skipped_so_the_track_still_serves() {
    let (file, audio) = fixture();
    let scan = locate_audio(&file).unwrap();

    // A picture with no image data is degenerate; synthesis must skip it rather than
    // emit an empty PICTURE block (which would fail layout validation and brick the
    // track).
    let art = cover(7, 0); // data_len == 0
    let layout = synthesize_layout(&scan, &[TagInput::new("title", "T")], &[art]).unwrap();

    // No ArtImage segment was emitted.
    assert!(!layout
        .segments
        .iter()
        .any(|s| matches!(s, Segment::ArtImage { .. })));

    // The track still round-trips: header + verbatim audio (no art bytes needed).
    let art_map = HashMap::new();
    let assembled = resolve_layout(&layout, &file, &art_map);
    assert_eq!(assembled.len() as u64, layout.total_len());
    assert_eq!(&assembled[layout.header_len() as usize..], &audio[..]);

    // metaflac sees a valid FLAC with zero pictures.
    let tag = metaflac::Tag::read_from(&mut Cursor::new(&assembled)).expect("valid FLAC metadata");
    assert_eq!(tag.pictures().count(), 0);
}

#[test]
fn zero_byte_art_skipped_among_valid_art_keeps_block_framing_valid() {
    // Guards the `is_last` flag: filtering the empty art must not leave the final
    // real PICTURE block without its last-block bit. metaflac parsing the whole
    // chain validates that.
    let (file, _audio) = fixture();
    let scan = locate_audio(&file).unwrap();
    let image = vec![0x55u8; 64];
    let empty = cover(1, 0);
    let real = cover(2, image.len() as u64);
    let layout = synthesize_layout(&scan, &[TagInput::new("title", "T")], &[empty, real]).unwrap();

    let art_segs: Vec<_> = layout
        .segments
        .iter()
        .filter(|s| matches!(s, Segment::ArtImage { .. }))
        .collect();
    assert_eq!(art_segs.len(), 1);

    let mut art_map = HashMap::new();
    art_map.insert(2i64, image.clone());
    let assembled = resolve_layout(&layout, &file, &art_map);
    let tag = metaflac::Tag::read_from(&mut Cursor::new(&assembled)).expect("valid FLAC metadata");
    let pics: Vec<_> = tag.pictures().collect();
    assert_eq!(pics.len(), 1);
    assert_eq!(pics[0].data, image);
}
```

- [ ] **Step 2: Run, expect FAIL**

```bash
cargo test -p musefs-format --features fuzzing synthesize_art::zero_byte_art
```
Expected: FAIL — `synthesize_layout(...).unwrap()` panics today because the empty
`ArtImage` segment trips `RegionLayout::validate` → `Err(FormatError::InvalidLayout)`.

- [ ] **Step 3: Implement the skip in `synthesize_layout`**

In `musefs-format/src/flac.rs`, filter zero-length art **before** counting blocks
(so `last_index` lands on the true final block) and `continue` past it in the loop.

Change the count line (currently line 129):

```rust
    // exclude zero-byte art: an empty PICTURE block is meaningless and would fail
    // layout validation (EmptySegment), making the track unreadable.
    let nonempty_art = arts.iter().filter(|a| a.data_len > 0).count();
    let num_blocks = scan.preserved.len() + 1 + nonempty_art; // preserved + VORBIS_COMMENT + pictures
```

And guard the loop body (currently the `for art in arts {` at line 149), as its
first statement:

```rust
    for art in arts {
        if art.data_len == 0 {
            continue; // skip degenerate empty art (see nonempty_art above)
        }
        let framing = picture_body_framing(art);
        // ... unchanged ...
    }
```

- [ ] **Step 4: Run, expect PASS (and the existing art tests still pass)**

```bash
cargo test -p musefs-format --features fuzzing synthesize_art
```
Expected: all `synthesize_art` tests pass, including the existing
`art_becomes_an_artimage_segment_and_lengths_are_exact`,
`metaflac_reads_synthesized_picture`, and `synthesize_errors_on_oversized_picture`
(non-empty art is unaffected).

- [ ] **Step 5: Commit**

```bash
git add musefs-format/src/flac.rs musefs-format/tests/synthesize_art.rs
git commit -m "fix(flac): skip zero-byte art at synthesis; boundary test (#16)"
```

---

## Task 8: inventory + tracking doc updates (C6)

**Files:**
- Modify: `docs/superpowers/specs/test-audit-remediation/2026-05-29-mutation-inventory.md`
- Modify: `docs/superpowers/specs/test-audit-remediation/2026-05-29-remediation-tracking.md`

- [ ] **Step 1: Annotate the `flac.rs` rows in the inventory**

For each `flac.rs:<line>` row, append the outcome to its `Kind` column, matching the
Phase 2 convention (`missed → **killed** (phase 3a)` or `missed → **equivalent**`):

- **Equivalent** (do not count as killed):
  `:50`, `:51` (`| → ^`); `:99` (`| → ^`); `:200` (`| → ^` row only),
  `:201` (`| → ^`); `:290` (`| → ^` row only), `:291` (`| → ^`);
  `:237` (`> → >=` row only), `:245` (`> → >=` row only).
- **Killed (phase 3a):** every other `flac.rs` row, including `:200`/`:290` (`| → &`
  and `<< → >>` rows), `:237`/`:245` (`> → ==` rows), and `:261`.

- [ ] **Step 2: Mark Phase 3a complete in the tracking doc**

In `2026-05-29-remediation-tracking.md`:
- Top-of-file Status line: add "Phase 3a (FLAC) complete".
- Under Phase 3, record: 3a done — FLAC survivors killed (equivalents:
  disjoint-bitfield `| → ^` at `:50/:51/:99/:200/:201/:290/:291` and inclusive-bound
  `> → >=` at `parse_picture_block:237/:245`); finding #16 resolved by **skipping
  zero-byte art at FLAC synthesis** (small production fix in `flac.rs::synthesize_layout`),
  with a cross-cutting follow-up noted: apply the same skip to mp3/mp4/ogg/wav
  synthesis (their sub-phases) or filter empty art once at ingestion (`scan.rs`);
  finding #5 broadened on FLAC (partial/seam/art windows), with the non-FLAC
  dimension tracked into 3b/3c/3d.

- [ ] **Step 3: Commit**

```bash
git add docs/superpowers/specs/test-audit-remediation/2026-05-29-mutation-inventory.md \
        docs/superpowers/specs/test-audit-remediation/2026-05-29-remediation-tracking.md
git commit -m "docs(phase3a): record FLAC kills/equivalents; mark phase 3a complete"
```

---

## Final verification (after all tasks)

- [ ] **Full workspace + lint + format**

```bash
cargo test --workspace
cargo test -p musefs-format --features fuzzing
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```
Expected: all green (the pre-commit hook runs these too).

- [ ] **Open the PR; `mutants.yml` `in-diff` + `canary` run on it.** After merge, the
next full campaign confirms `flac.rs` survivors dropped (excluding the documented
equivalents).

## Notes for the executor

- The **only** production change is the zero-byte-art skip in `flac.rs::synthesize_layout`
  (Task 7); every other change is additive tests. Byte-identity for audio is never
  touched (the change only affects whether an empty PICTURE block is emitted).
- Never leave a hand-applied mutation in the tree — always revert before the next step.
- If a survivor turns out to be a genuine equivalent the plan did not anticipate,
  record it in Task 8 rather than forcing a contrived test. The plan already marks
  the `| → ^` and `parse_picture_block` `> → >=` mutations equivalent.
- The pre-commit hook runs `cargo fmt --check`, `clippy -D warnings`, `cargo test
  --workspace`, and `ruff`; keep each commit green.
```
