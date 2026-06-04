# Phase 3b — MP3 Hardening Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Kill the 70 `mp3.rs` mutation survivors (49 of them in `id3v2_alloc_safe`) with additive in-module tests, documenting the genuine equivalents — no production logic change expected.

**Architecture:** Extend `mp3.rs`'s existing `#[cfg(test)] mod tests` (switch its imports to `use super::*`), calling the private survivor functions directly with byte-precise ID3v2 fixtures built by small independent local helpers. Hand-apply-verify every kill; document equivalents. The big `id3v2_alloc_safe` validator is split across four tasks (header gate / high-bit checks / per-version size decode / bounds-walk).

**Tech Stack:** Rust, `cargo test`, `proptest` + `id3` + `metaflac` (existing dev-deps). No new dependencies.

**Spec:** `docs/superpowers/specs/test-audit-remediation/2026-05-29-phase3b-mp3-hardening-design.md`
**Survivor data:** `docs/superpowers/specs/test-audit-remediation/2026-05-29-mutation-inventory.md`

---

## The hand-apply verification method (use in every kill step)

cargo-mutants is not available locally. For each targeted `function: construct: mutation`:

1. Run the new test → it passes (production is correct).
2. **Locate the construct by its code pattern** (inventory line numbers have drifted
   ~10 lines vs current `main` — never trust the raw number), apply the exact
   mutation, rerun **just that test** → it must **fail** (failed assertion *or*
   panic). 
3. Revert (`git checkout -- musefs-format/src/mp3.rs`), rerun → passes again.

If step 2 still passes: strengthen the test, or — if the mutation provably yields
identical behavior — record it as an **equivalent mutant** (Task 8). Never leave a
mutation applied.

## Equivalent mutants (confirm green under the mutation, then record)

Verified by reading the source. **Only `| → ^` / `| → +` on disjoint-bitfield ORs
are equivalent** — `| → &` on the same sites is killable (AND of disjoint = 0):

- `synchsafe_decode`'s four 7-bit groups (the three joining `|`): `|→^`/`|→+` equivalent.
- `id3v2_alloc_safe`'s v2.2 24-bit frame-size decode `(d3<<16)|(d4<<8)|d5`: `|→^`
  equivalent; **`|→&` killable**, **`<<→>>` killable**.

Whole-byte OR-chains (`data[6]|data[7]|data[8]|data[9] >= 0x80`, the v2.4
`data[pos+4]|…|data[pos+7] >= 0x80`) are **not** equivalent under any `|` mutation.

## Pre-flight (run once before Task 1)

- [ ] **Confirm baseline green on the phase-3b branch**

```bash
git rev-parse --abbrev-ref HEAD          # expect: worktree-phase3b-mp3-hardening
cargo test -p musefs-format --features fuzzing mp3
```
Expected: green (existing `id3v2_guard_*`, `read_tags_*`, `synthesize_*` tests pass).

---

## Task 1: switch test imports + synchsafe codec kills (C1)

Kills `synchsafe_decode` (`<<21`, `<<14`, the `|→&`) and `syncsafe` (`>>21`, `>>14`).
Confirms the three disjoint `|→^`/`|→+` equivalent.

**Files:**
- Modify (tests only): `musefs-format/src/mp3.rs` `#[cfg(test)] mod tests`.

- [ ] **Step 1: Widen the test module imports**

Replace the existing three import lines at the top of `mod tests`:

```rust
    use super::{build_id3v2_segments, id3v2_alloc_safe, read_tags};
    use crate::input::TagInput;
    use crate::layout::Segment;
```

with a single glob (brings every survivor fn plus `Mp3Bounds`, and re-globs the
parent's `FormatError`/`ArtInput`/`TagInput`/`Segment` imports):

```rust
    use super::*;
```

- [ ] **Step 2: Run the existing suite to confirm the import change compiles**

```bash
cargo test -p musefs-format --features fuzzing mp3::tests
```
Expected: the pre-existing tests still pass (no behavior change).

- [ ] **Step 3: Add the synchsafe tests**

```rust
    #[test]
    fn synchsafe_decode_assembles_7bit_groups() {
        // (1<<21)|(2<<14)|(3<<7)|4
        assert_eq!(synchsafe_decode(&[0x01, 0x02, 0x03, 0x04]), 0x0020_8184);
        // high bit of each byte masked (& 0x7F): 0xFF -> 0x7F per group.
        assert_eq!(synchsafe_decode(&[0xFF, 0xFF, 0xFF, 0xFF]), 0x0FFF_FFFF);
        // only the top group set -> pins the `<<21` (kills `<<21 -> >>21`).
        assert_eq!(synchsafe_decode(&[0x7F, 0x00, 0x00, 0x00]), 0x0FE0_0000);
        // only the second group set -> pins the `<<14` (kills `<<14 -> >>14`).
        assert_eq!(synchsafe_decode(&[0x00, 0x7F, 0x00, 0x00]), 0x001F_C000);
    }

    #[test]
    fn syncsafe_encodes_and_round_trips() {
        // pins the `>>21` and `>>14` group extraction.
        assert_eq!(syncsafe(0x0FE0_0000), [0x7F, 0x00, 0x00, 0x00]);
        assert_eq!(syncsafe(0x001F_C000), [0x00, 0x7F, 0x00, 0x00]);
        // round-trip over the full 28-bit range pins every group boundary.
        for n in [0u32, 1, 127, 128, 0x0123_4567, 0x0FFF_FFFF] {
            assert_eq!(synchsafe_decode(&syncsafe(n)), n);
        }
    }
```

- [ ] **Step 4: Run + hand-apply-verify**

```bash
cargo test -p musefs-format --features fuzzing mp3::tests::synchsafe_decode_assembles_7bit_groups mp3::tests::syncsafe_encodes_and_round_trips
```
- `synchsafe_decode` `<<21 -> >>21`: `..._assembles_*` FAILs (top-group assert). Revert.
- `synchsafe_decode` `<<14 -> >>14`: FAILs (second-group assert). Revert.
- `synchsafe_decode` first `| -> &`: FAILs (`0x0020_8184` becomes `0x184`). Revert.
- `syncsafe` `>>21 -> <<21`, `>>14 -> <<14`: `..._round_trips` FAILs. Revert.
- the three `| -> ^` (and `| -> +`) in `synchsafe_decode`: rerun → still PASS
  (disjoint groups). Record equivalent (Task 8). Revert each.

- [ ] **Step 5: Commit**

```bash
git add musefs-format/src/mp3.rs
git commit -m "test(mp3): synchsafe decode/encode kills; widen test imports"
```

---

## Task 2: `locate_audio` kills (C2)

Kills the `&&` marker guard, the `+=` footer, the `+ 1` frame-sync index, and the
`||` frame-sync chain.

**Files:**
- Modify (tests only): `musefs-format/src/mp3.rs` `#[cfg(test)] mod tests`.

- [ ] **Step 1: Add the tests**

```rust
    #[test]
    fn locate_audio_no_id3_starts_at_zero() {
        // >=10 bytes, not "ID3": original skips the ID3 block (audio at 0). The
        // `&& -> ||` mutant enters the block, decodes garbage, and returns Err — so
        // this unwrap kills it. Frame sync 0xFF 0xFB at offset 0.
        let data = [0xFF, 0xFB, 0x90, 0x00, 0, 0, 0, 0, 0, 0];
        let b = locate_audio(&data).unwrap();
        assert_eq!(b.audio_offset, 0);
        assert_eq!(b.audio_length, 10);
    }

    #[test]
    fn locate_audio_skips_id3v2_then_finds_sync() {
        // "ID3" v2.4, flags=0, synchsafe body=4 -> tag_len=14. Sync at offset 14.
        let mut data = Vec::new();
        data.extend_from_slice(b"ID3");
        data.extend_from_slice(&[0x04, 0x00, 0x00]); // major, rev, flags
        data.extend_from_slice(&[0x00, 0x00, 0x00, 0x04]); // synchsafe body=4
        data.extend_from_slice(&[0xAA, 0xBB, 0xCC, 0xDD]); // 4 body bytes
        data.extend_from_slice(&[0xFF, 0xFB, 0x90, 0x00]); // audio sync at 14
        let b = locate_audio(&data).unwrap();
        assert_eq!(b.audio_offset, 14);
        assert_eq!(b.audio_length, 4);
    }

    #[test]
    fn locate_audio_honors_footer_flag() {
        // footer flag (0x10) adds 10 to tag_len. body=0 -> tag_len = 10+0+10 = 20.
        // Sync at offset 20. The `+= -> -=`/`*=` mutant computes the wrong tag_len
        // and the sync check lands on the wrong byte -> Err (kills the `+=`).
        let mut data = Vec::new();
        data.extend_from_slice(b"ID3");
        data.extend_from_slice(&[0x04, 0x00, 0x10]); // flags: footer present
        data.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]); // synchsafe body=0
        data.extend_from_slice(&[0u8; 10]); // 10-byte footer region
        data.extend_from_slice(&[0xFF, 0xFB, 0x90, 0x00]); // sync at offset 20
        let b = locate_audio(&data).unwrap();
        assert_eq!(b.audio_offset, 20);
    }

    #[test]
    fn locate_audio_requires_frame_sync() {
        // data[0]=0xFF but data[1] lacks the 0xE0 sync bits: original rejects
        // (NotMp3). The `|| -> &&` mutant accepts (only rejects if ALL conditions
        // hold). The `+ -> *` on data[audio_offset+1] would read data[0] instead of
        // data[1]; with distinct bytes the sync decision flips.
        let data = [0xFF, 0x00, 0x00, 0x00, 0, 0, 0, 0, 0, 0];
        assert_eq!(locate_audio(&data), Err(FormatError::NotMp3));
        // 1-byte buffer: original NotMp3 (audio_offset+1 >= len). The `+ -> *`
        // mutant computes 0*1=0 >= 1 = false, falls through, and panics on data[1].
        assert_eq!(locate_audio(&[0xFF]), Err(FormatError::NotMp3));
    }
```

- [ ] **Step 2: Run + hand-apply-verify**

```bash
cargo test -p musefs-format --features fuzzing mp3::tests::locate_audio
```
- `&& -> ||` (marker guard): `..._no_id3_starts_at_zero` FAILs. Revert.
- `+= -> -=` and `+= -> *=` (footer): `..._honors_footer_flag` FAILs. Revert each.
- `+ -> *` — there are **two** `audio_offset + 1` sites, each killed by a different
  assert in `..._requires_frame_sync`:
  - the guard `audio_offset + 1 >= len`: the **1-byte** `[0xFF]` assert kills it —
    `0*1 = 0 >= 1` is false, so the mutant falls through and panics reading `data[1]`
    (original returns `NotMp3`).
  - the index `data[audio_offset + 1]`: the **10-byte** `[0xFF, 0x00, …]` assert
    kills it — `data[0*1] = data[0] = 0xFF` passes the sync bits, so the mutant
    returns `Ok`, but the test asserts `NotMp3` (original fails on `data[1] = 0x00`).
    A mutant returning `Ok` where the test expects `Err` **fails the assert** = kill.
  Revert each.
- `|| -> &&` (frame-sync chain): the 10-byte `[0xFF, 0x00, …]` assert FAILs — under
  `&&` only `c1 && c2 && c3` rejects, so a single bad condition no longer rejects and
  the mutant returns `Ok`. Revert.

- [ ] **Step 3: Commit**

```bash
git add musefs-format/src/mp3.rs
git commit -m "test(mp3): locate_audio id3-skip + frame-sync kills"
```

---

## Task 3: frame helpers — `push_frame_header`, `is_id3_text_frame_id`, `build_id3v2_segments` (C3)

Kills `push_frame_header` (`>→==`/`>=`), `is_id3_text_frame_id` (whole-fn, `!=`,
`||→&&`), and `build_id3v2_segments` (the `is_id3_text_frame_id` match guard, the
total-tag `>→>=` guard).

**Files:**
- Modify (tests only): `musefs-format/src/mp3.rs` `#[cfg(test)] mod tests`.

- [ ] **Step 1: Add the tests**

```rust
    #[test]
    fn push_frame_header_size_boundary_is_inclusive() {
        // ID3v2.4 frame size is a 28-bit syncsafe field; the guard rejects
        // data_len > 0x0FFF_FFFF. 0x0FFF_FFFF is the inclusive max (Ok); +1 errors.
        let mut out = Vec::new();
        assert!(push_frame_header(&mut out, b"TIT2", 0x0FFF_FFFF).is_ok());
        let mut over = Vec::new();
        assert_eq!(
            push_frame_header(&mut over, b"TIT2", 0x1000_0000),
            Err(FormatError::TooLarge)
        );
    }

    #[test]
    fn is_id3_text_frame_id_classifies_text_frames() {
        assert!(is_id3_text_frame_id("TPE1")); // T + 3 upper/digit, not TXXX
        assert!(is_id3_text_frame_id("TIT2"));
        assert!(!is_id3_text_frame_id("TXXX")); // excluded (kills `!= -> ==`)
        assert!(!is_id3_text_frame_id("COMM")); // not T-prefixed
        assert!(!is_id3_text_frame_id("TPE")); // wrong length
        assert!(!is_id3_text_frame_id("Txx1")); // lowercase -> false
    }

    #[test]
    fn build_id3v2_segments_emits_standard_text_frame_as_itself() {
        // A 4-char T-frame key (TPE1) must round-trip as a TPE1 frame, not TXXX.
        // The `is_id3_text_frame_id` match-guard `-> false` mutant would route it to
        // the TXXX branch, so read_tags would surface it under a different key.
        let tags = vec![TagInput::new("TPE1", "Band")];
        let (segments, _len) = build_id3v2_segments(&tags, &[]).unwrap();
        let mut buf = Vec::new();
        for seg in &segments {
            if let Segment::Inline(b) = seg {
                buf.extend_from_slice(b);
            }
        }
        // The literal frame id "TPE1" must appear in the emitted tag bytes.
        assert!(
            buf.windows(4).any(|w| w == b"TPE1"),
            "TPE1 frame not emitted: routed elsewhere"
        );
        // And it round-trips to the mapped key (artist), not a TXXX user field.
        let read = read_tags(&buf);
        assert!(read.contains(&("artist".to_string(), "Band".to_string())), "got {read:?}");
    }

    #[test]
    fn build_id3v2_segments_rejects_oversized_total_tag() {
        // The total-tag guard rejects frames_len > 0x0FFF_FFFF. An APIC art whose
        // data_len (a count, not allocated) pushes the total just over the limit
        // must error; one byte under must succeed.
        let mk = |data_len: u64| ArtInput {
            art_id: 1,
            mime: "image/png".to_string(),
            description: String::new(),
            picture_type: 3,
            width: 0,
            height: 0,
            data_len,
        };
        // Find the exact data_len that lands frames_len on the boundary by bisecting
        // around the APIC framing overhead: build with a known data_len and read the
        // returned total. Simpler: assert a clearly-over value errors and a small one
        // succeeds (the `> -> >=` exact-boundary nuance is covered by hand-apply).
        assert_eq!(
            build_id3v2_segments(&[], &[mk(0x1000_0000)]).err(),
            Some(FormatError::TooLarge)
        );
        assert!(build_id3v2_segments(&[], &[mk(16)]).is_ok());
    }
```

- [ ] **Step 2: Run + hand-apply-verify**

```bash
cargo test -p musefs-format --features fuzzing mp3::tests::push_frame_header_size_boundary_is_inclusive mp3::tests::is_id3_text_frame_id_classifies_text_frames mp3::tests::build_id3v2_segments_emits_standard_text_frame_as_itself mp3::tests::build_id3v2_segments_rejects_oversized_total_tag
```
- `push_frame_header` `> -> ==`/`>=`: boundary test FAILs (`is_ok()` becomes Err). Revert each.
- `is_id3_text_frame_id` `-> false`: classifier test FAILs (`"TPE1"` assert). Revert.
- `is_id3_text_frame_id` `!= -> ==`: FAILs (`"TXXX"` now classified true → the
  `is_id3_text_frame_id("TPE1")`... actually the `"TXXX"`→false assert flips). Revert.
- `is_id3_text_frame_id` `|| -> &&`: FAILs — `"TPE1"` has digit `1`
  (`is_upper||is_digit`); under `&&`, `1` is digit-not-upper → `.all()` false →
  returns false → the `"TPE1"` assert fails. Revert.
- `build_id3v2_segments` match-guard `-> false`: `..._emits_standard_text_frame`
  FAILs (TPE1 routed to TXXX). Revert.
- `build_id3v2_segments` total-tag `> -> >=`: confirm via hand-apply on the
  boundary; if `>=` does not flip on `mk(16)`, construct the exact-boundary
  `data_len` (compute from `build_id3v2_segments(&[], &[mk(0)]).unwrap().1` to learn
  the APIC overhead, then pick `data_len` so the total == `0x0FFF_FFFF`) and assert
  `is_ok()` vs `Err(TooLarge)` at the boundary. Revert.

- [ ] **Step 3: Commit**

```bash
git add musefs-format/src/mp3.rs
git commit -m "test(mp3): push_frame_header/is_id3_text_frame_id/build_id3v2_segments kills"
```

---

## Task 4: `id3v2_alloc_safe` — fixture helpers + header gate (C4a)

Kills the header-gate survivors: `data.len() < 10` (`<→==`/`<=`, `||→&&`), the
`major` version gate, and the `flags & 0xC0` reject. Introduces the local fixture
helpers reused by Tasks 5–7.

**Files:**
- Modify (tests only): `musefs-format/src/mp3.rs` `#[cfg(test)] mod tests`.

- [ ] **Step 1: Add the fixture helpers (first use here)**

```rust
    /// Independent synchsafe encoder for fixtures (does NOT call `syncsafe`, so a
    /// mutation there cannot mask a fixture).
    fn ss(n: u32) -> [u8; 4] {
        [
            ((n >> 21) & 0x7F) as u8,
            ((n >> 14) & 0x7F) as u8,
            ((n >> 7) & 0x7F) as u8,
            (n & 0x7F) as u8,
        ]
    }

    /// Build an ID3v2 tag: "ID3", `major`, rev=0, `flags`, synchsafe `body` size,
    /// then the raw `frames` bytes.
    fn id3v2(major: u8, flags: u8, body: u32, frames: &[u8]) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(b"ID3");
        v.push(major);
        v.push(0x00);
        v.push(flags);
        v.extend_from_slice(&ss(body));
        v.extend_from_slice(frames);
        v
    }
```

(`v23_frame` is introduced in Task 6, where it is first used, to keep this commit
free of `dead_code` warnings.)

- [ ] **Step 2: Add the header-gate tests**

```rust
    #[test]
    fn alloc_safe_accepts_minimal_valid_header() {
        // 10-byte v2.4 header, body=0, no frames -> safe. This is exactly the
        // len==10 boundary, so the `< -> <=` mutant (10<=10 -> reject) flips it.
        let tag = id3v2(0x04, 0x00, 0, &[]);
        assert_eq!(tag.len(), 10);
        assert!(id3v2_alloc_safe(&tag));
    }

    #[test]
    fn alloc_safe_rejects_short_and_non_id3() {
        // "ID3" + 2 bytes (len 5, marker correct): original returns false (len<10).
        // `< -> ==` (5==10 false) and `|| -> &&` (true && false) both fall through
        // and panic reading data[5]. Asserting `!safe` kills them.
        assert!(!id3v2_alloc_safe(b"ID3xx"));
        // Right length, wrong marker -> false.
        assert!(!id3v2_alloc_safe(b"XXX\x04\x00\x00\x00\x00\x00\x00"));
    }

    #[test]
    fn alloc_safe_rejects_bad_version_and_header_flags() {
        // major outside 2..=4 -> false (kills the `matches!(major, 2..=4)` mutations).
        assert!(!id3v2_alloc_safe(&id3v2(0x05, 0x00, 0, &[])));
        assert!(!id3v2_alloc_safe(&id3v2(0x01, 0x00, 0, &[])));
        // extended-header (0x40) or unsync (0x80) -> false (kills `& 0xC0` mutations).
        assert!(!id3v2_alloc_safe(&id3v2(0x04, 0x40, 0, &[])));
        assert!(!id3v2_alloc_safe(&id3v2(0x04, 0x80, 0, &[])));
    }
```

- [ ] **Step 3: Run + hand-apply-verify**

```bash
cargo test -p musefs-format --features fuzzing mp3::tests::alloc_safe_accepts_minimal_valid_header mp3::tests::alloc_safe_rejects_short_and_non_id3 mp3::tests::alloc_safe_rejects_bad_version_and_header_flags
```
- `< -> <=` (len gate): `..._accepts_minimal_valid_header` FAILs. Revert.
- `< -> ==`, `|| -> &&` (len gate): `..._rejects_short_and_non_id3` FAILs (panic). Revert each.
- `matches!(major, 2..=4)` mutations: `..._rejects_bad_version_*` FAILs. Revert.
- `flags & 0xC0 != 0` mutations: `..._rejects_bad_version_and_header_flags` FAILs. Revert.

- [ ] **Step 4: Commit**

```bash
git add musefs-format/src/mp3.rs
git commit -m "test(mp3): id3v2_alloc_safe header-gate kills + fixture helpers"
```

---

## Task 5: `id3v2_alloc_safe` — synchsafe high-bit checks (C4b)

Kills the whole-byte OR high-bit checks (body size bytes and the v2.4 frame-size
bytes): `| -> ^` and `| -> &` are both killable here (two `0x80` bytes diverge).

**Files:**
- Modify (tests only): `musefs-format/src/mp3.rs` `#[cfg(test)] mod tests`.

- [ ] **Step 1: Add the tests**

```rust
    #[test]
    fn alloc_safe_rejects_high_bit_in_body_size() {
        // Two body-size bytes with the high bit set: OR = 0x80 (reject). The
        // `| -> ^` mutant gives 0x80^0x80 = 0 (accept); `| -> &` gives 0x80&0x80&0&0
        // = 0 (accept). Built by hand because `ss()` would clear the high bits.
        let tag = vec![b'I', b'D', b'3', 0x04, 0x00, 0x00, 0x80, 0x80, 0x00, 0x00];
        assert!(!id3v2_alloc_safe(&tag));
        // Single high-bit byte still rejected (pins the `>= 0x80` comparison).
        let tag1 = vec![b'I', b'D', b'3', 0x04, 0x00, 0x00, 0x00, 0x00, 0x00, 0x80];
        assert!(!id3v2_alloc_safe(&tag1));
    }

    #[test]
    fn alloc_safe_rejects_high_bit_in_v24_frame_size() {
        // v2.4 frame size is synchsafe; two size bytes with the high bit set must be
        // rejected (whole-byte OR check on data[pos+4..pos+8]). The frame is 10 bytes
        // (4 id + 4 size + 2 flags), so body=10 makes tag_end == len (20): the walk
        // is entered (NOT short-circuited by `tag_end > data.len()`) and the high-bit
        // check fires.
        let mut frame = b"TIT2".to_vec();
        frame.extend_from_slice(&[0x80, 0x80, 0x00, 0x00]); // size bytes, two high bits
        frame.extend_from_slice(&[0x00, 0x00]); // frame flags
        let tag = id3v2(0x04, 0x00, 10, &frame);
        assert!(!id3v2_alloc_safe(&tag));
    }
```

- [ ] **Step 2: Run + hand-apply-verify**

```bash
cargo test -p musefs-format --features fuzzing mp3::tests::alloc_safe_rejects_high_bit_in_body_size mp3::tests::alloc_safe_rejects_high_bit_in_v24_frame_size
```
- body high-bit `| -> ^` (×3) and `| -> &` (×3): `..._body_size` FAILs (the
  two-`0x80` fixture: mutant accepts). Revert each.
- v2.4 frame-size high-bit `| -> ^`/`| -> &`: `..._v24_frame_size` FAILs. Revert each.

- [ ] **Step 3: Commit**

```bash
git add musefs-format/src/mp3.rs
git commit -m "test(mp3): id3v2_alloc_safe synchsafe high-bit-check kills"
```

---

## Task 6: `id3v2_alloc_safe` — per-version frame-size decode + CHAP/CTOC (C4c)

Kills the v2.2 24-bit decode (`<<16`/`<<8` and the killable `| -> &`; confirms the
v2.2 `| -> ^` equivalent), the v2.3/v2.4 frame-flag rejects, and the `CHAP`/`CTOC`
reject.

**Files:**
- Modify (tests only): `musefs-format/src/mp3.rs` `#[cfg(test)] mod tests`.

- [ ] **Step 1: Add the `v23_frame` helper (first use here) + the tests**

```rust
    /// A valid ID3v2.3 frame: 4-byte id, 4-byte plain big-endian size, 2 flag bytes,
    /// then `payload`.
    fn v23_frame(id: &[u8; 4], size: u32, payload: &[u8]) -> Vec<u8> {
        let mut v = id.to_vec();
        v.extend_from_slice(&size.to_be_bytes());
        v.extend_from_slice(&[0x00, 0x00]);
        v.extend_from_slice(payload);
        v
    }

    #[test]
    fn alloc_safe_v22_24bit_size_decode() {
        // v2.2 frame header is 6 bytes: 3-byte id + 3-byte 24-bit big-endian size.
        // Declare a size that the *correct* decode puts out of bounds (reject), so a
        // wrong shift/OR that shrinks the size would wrongly accept.
        // size bytes [0x00,0x01,0x00] = 256, body = 6 (header only, no room) -> reject.
        let mut f_mid = b"TT2".to_vec();
        f_mid.extend_from_slice(&[0x00, 0x01, 0x00]); // 24-bit size = 256
        assert!(!id3v2_alloc_safe(&id3v2(0x02, 0x00, 6, &f_mid))); // kills <<8 and |->&
        // size bytes [0x01,0x00,0x00] = 65536 -> reject; `<<16 -> >>16` shrinks to 0.
        let mut f_hi = b"TT2".to_vec();
        f_hi.extend_from_slice(&[0x01, 0x00, 0x00]);
        assert!(!id3v2_alloc_safe(&id3v2(0x02, 0x00, 6, &f_hi)));
        // A valid in-bounds v2.2 frame is accepted: size 4, body = 6+4 = 10.
        let mut f_ok = b"TT2".to_vec();
        f_ok.extend_from_slice(&[0x00, 0x00, 0x04]);
        f_ok.extend_from_slice(&[1, 2, 3, 4]);
        assert!(id3v2_alloc_safe(&id3v2(0x02, 0x00, 10, &f_ok)));
    }

    #[test]
    fn alloc_safe_rejects_nonzero_frame_flags() {
        // v2.3: non-zero frame flags -> reject (the v2.3 flag check).
        let mut f3 = b"TIT2".to_vec();
        f3.extend_from_slice(&4u32.to_be_bytes()); // plain size 4
        f3.extend_from_slice(&[0x00, 0x01]); // non-zero frame flags
        f3.extend_from_slice(&[1, 2, 3, 4]);
        assert!(!id3v2_alloc_safe(&id3v2(0x03, 0x00, 14, &f3)));

        // v2.4: non-zero frame flags -> reject. This is a SEPARATE code path (the
        // v2.4 `else` branch) from the v2.3 check, so it needs its own fixture.
        let mut f4 = b"TIT2".to_vec();
        f4.extend_from_slice(&ss(4)); // valid synchsafe size 4
        f4.extend_from_slice(&[0x00, 0x01]); // non-zero frame flags
        f4.extend_from_slice(&[1, 2, 3, 4]);
        assert!(!id3v2_alloc_safe(&id3v2(0x04, 0x00, 14, &f4)));
    }

    #[test]
    fn alloc_safe_rejects_chap_and_ctoc() {
        // CHAP/CTOC carry sub-frames -> recursive OOM vector -> reject (v2.3/2.4).
        let chap = v23_frame(b"CHAP", 4, &[1, 2, 3, 4]);
        assert!(!id3v2_alloc_safe(&id3v2(0x03, 0x00, 14, &chap)));
        let ctoc = v23_frame(b"CTOC", 4, &[1, 2, 3, 4]);
        assert!(!id3v2_alloc_safe(&id3v2(0x03, 0x00, 14, &ctoc)));
    }
```

- [ ] **Step 2: Run + hand-apply-verify**

```bash
cargo test -p musefs-format --features fuzzing mp3::tests::alloc_safe_v22_24bit_size_decode mp3::tests::alloc_safe_rejects_nonzero_frame_flags mp3::tests::alloc_safe_rejects_chap_and_ctoc
```
- v2.2 `<<8 -> >>8` and first `| -> &`: `..._v22_24bit_size_decode` FAILs (the
  `[0x00,0x01,0x00]` fixture: mutant decodes 0 and accepts). Revert each.
- v2.2 `<<16 -> >>16`: FAILs (the `[0x01,0x00,0x00]` fixture). Revert.
- v2.2 `| -> ^`: rerun → still PASS (disjoint; `^` == `|` here). Record equivalent
  (Task 8). Revert.
- frame-flag `!= -> ==` / `|| -> &&` — **both the v2.3 and the v2.4 flag checks**
  (two separate `if data[pos+8]!=0 || data[pos+9]!=0` sites):
  `..._rejects_nonzero_frame_flags` FAILs (it now asserts both versions). Revert each.
- `CHAP`/`CTOC` `&& -> ||` / `== -> !=` / `|| -> &&`: `..._rejects_chap_and_ctoc`
  FAILs. Revert each.

- [ ] **Step 3: Commit**

```bash
git add musefs-format/src/mp3.rs
git commit -m "test(mp3): id3v2_alloc_safe size-decode + CHAP/CTOC + frame-flag kills"
```

---

## Task 7: `id3v2_alloc_safe` — frame-bounds and walk (C4d)

This region has several constructs; each fixture below targets specific ones. The
constructs (current source, locate by pattern):

- **A** `let data_start = pos + header_len;` (`+ -> -`/`*`)
- **B** `data_start > tag_end` (`> -> ==`/`>=`)
- **C** `size > tag_end - data_start` (`> -> ==`/`>=`, and `- -> +`)
- **D** the `||` in `data_start > tag_end || size > ...` (`|| -> &&`)
- **E** `pos = data_start + size;` (`+ -> -`/`*`)
- **F** `if pos >= tag_end { break; }` (`>= -> <`/`==`)
- **G** the while guard `pos + header_len <= scan_end` (`+ -> -`/`*`)
- **I** the padding break `if data[pos] == 0` (`== -> !=`)

(Index-arithmetic `+ -> -`/`*` inside the v2.2/v2.3/v2.4 size and flag reads —
`data[pos+3..pos+9]` — are exercised by Task 6's decode/flag fixtures: a wrong index
changes the decoded size or flag byte, flipping accept/reject. Confirm those during
Task 6's hand-apply.)

**Files:**
- Modify (tests only): `musefs-format/src/mp3.rs` `#[cfg(test)] mod tests`.

- [ ] **Step 1: Add the tests**

```rust
    #[test]
    fn alloc_safe_frame_size_bounds() {
        // Frame exactly filling the body -> accept (size 4, body = 10+4 = 14).
        // data_start = 10+10 = 20, tag_end = 24, rem = 4, size 4 -> 4 > 4 is false.
        // Kills A `+ -> *` (data_start=100 -> 100>24 -> reject) and C `> -> >=`
        // (4 >= 4 -> reject).
        let ok = v23_frame(b"TIT2", 4, &[1, 2, 3, 4]);
        assert!(id3v2_alloc_safe(&id3v2(0x03, 0x00, 14, &ok)));
        // size one byte past the remainder -> reject (size 5: 5 > 24-20=4). Kills C
        // `> -> ==` (5==4 false -> accept), C `- -> +` (rem=44 -> 5>44 false ->
        // accept), D `|| -> &&` (false && true -> accept), and A `+ -> -`
        // (data_start=0 -> 5 > 24-0=24 false -> accept).
        let over = v23_frame(b"TIT2", 5, &[1, 2, 3, 4]);
        assert!(!id3v2_alloc_safe(&id3v2(0x03, 0x00, 14, &over)));
    }

    #[test]
    fn alloc_safe_data_start_equal_to_tag_end_is_ok() {
        // A size-0 frame: data_start (20) == tag_end (20). Original: `20 > 20` is
        // false -> accept. Kills B `> -> ==` (20==20 -> reject) and `> -> >=`.
        let zero = v23_frame(b"TIT2", 0, &[]);
        assert!(id3v2_alloc_safe(&id3v2(0x03, 0x00, 10, &zero)));
    }

    #[test]
    fn alloc_safe_rejects_bad_second_frame_in_body() {
        // Valid frame1 (size 2) then an out-of-bounds frame2 (size 100), both inside
        // the declared body (body=26, tag_end=36). Original walks to frame2 and
        // rejects. Kills E `+ -> *` (pos = 20*2 = 40 >= 36 -> break -> accept,
        // skipping frame2) and E `+ -> -` (pos = 20-2 = 18 -> data[18]==0 padding
        // break -> accept).
        let mut frames = v23_frame(b"TIT2", 2, &[0xAA, 0xBB]); // 12 bytes, 10..22
        frames.extend_from_slice(&v23_frame(b"TPE1", 100, &[1, 2, 3, 4])); // 14, 22..36
        assert!(!id3v2_alloc_safe(&id3v2(0x03, 0x00, 26, &frames)));
    }

    #[test]
    fn alloc_safe_stops_at_tag_body_end() {
        // A size-0 frame fills the body (tag_end=20), then a bad trailing frame
        // beyond tag_end but within the buffer. Original breaks at `pos >= tag_end`
        // (20 >= 20) and accepts without walking the trailing garbage. Kills F
        // `>= -> <` (20 < 20 false -> no break -> walks the bad frame -> reject).
        let mut frames = v23_frame(b"TIT2", 0, &[]); // 10 bytes, 10..20
        frames.extend_from_slice(&v23_frame(b"TPE1", 100, &[1, 2, 3, 4])); // 14, 20..34
        assert!(id3v2_alloc_safe(&id3v2(0x03, 0x00, 10, &frames)));
    }

    #[test]
    fn alloc_safe_walks_two_frames_and_stops_at_padding() {
        // Two valid frames (24 bytes, 10..34) then 10 padding zero bytes (34..44).
        // body=25 -> tag_end=35, so after frame2 (pos=34) `34 >= 35` is false (no
        // tag-end break); the next iteration enters (`34+10=44 <= 44`) and
        // `data[34] == 0` triggers the PADDING break. Kills I `== -> !=` (no break ->
        // walks zero bytes -> data_start past tag_end -> reject) and exercises the
        // multi-frame walk (E) and the while guard (G).
        let mut frames = v23_frame(b"TIT2", 2, &[0xAA, 0xBB]);
        frames.extend_from_slice(&v23_frame(b"TPE1", 2, &[0xCC, 0xDD]));
        frames.extend_from_slice(&[0u8; 10]); // >= header_len of padding so the walk re-enters
        assert!(id3v2_alloc_safe(&id3v2(0x03, 0x00, 25, &frames)));
    }

    #[test]
    fn alloc_safe_rejects_frame_size_exceeding_tag_end() {
        // Single frame claiming size 100 in a 14-byte body -> reject before any
        // allocation. Reinforces C.
        let huge = v23_frame(b"TIT2", 100, &[1, 2, 3, 4]);
        assert!(!id3v2_alloc_safe(&id3v2(0x03, 0x00, 14, &huge)));
    }
```

- [ ] **Step 2: Run + hand-apply-verify each construct**

```bash
cargo test -p musefs-format --features fuzzing mp3::tests::alloc_safe_frame_size_bounds mp3::tests::alloc_safe_data_start_equal_to_tag_end_is_ok mp3::tests::alloc_safe_rejects_bad_second_frame_in_body mp3::tests::alloc_safe_stops_at_tag_body_end mp3::tests::alloc_safe_walks_two_frames_and_stops_at_padding mp3::tests::alloc_safe_rejects_frame_size_exceeding_tag_end
```
Construct → killing test (apply the mutation, confirm the named test goes red, revert):

- **A** `data_start = pos + header_len` `+ -> *`: `alloc_safe_frame_size_bounds` (ok). `+ -> -`: same test (over).
- **B** `data_start > tag_end` `> -> ==`/`>=`: `alloc_safe_data_start_equal_to_tag_end_is_ok`.
- **C** `size > tag_end - data_start` `> -> ==` and `- -> +`: `..._frame_size_bounds` (over); `> -> >=`: same test (ok).
- **D** `|| -> &&`: `..._frame_size_bounds` (over).
- **E** `pos = data_start + size` `+ -> *` and `+ -> -`: `alloc_safe_rejects_bad_second_frame_in_body`.
- **F** `pos >= tag_end` `>= -> <`: `alloc_safe_stops_at_tag_body_end`. `>= -> ==`: `..._walks_two_frames_*` (pos advances past tag_end in a multi-frame walk).
- **G** while `pos + header_len` `+ -> -`/`*`: any frame fixture (a corrupted guard reads `data[pos]` out of bounds → panic, or walks wrong → reject). Confirm on `..._frame_size_bounds`.
- **I** `data[pos] == 0` `== -> !=`: `alloc_safe_walks_two_frames_and_stops_at_padding`.

If any listed mutation survives all the tests above, add a boundary fixture that
flips accept↔reject at exactly that construct and note it; do not leave it unkilled.

- [ ] **Step 3: Commit**

```bash
git add musefs-format/src/mp3.rs
git commit -m "test(mp3): id3v2_alloc_safe frame-bounds + walk kills"
```

---

## Task 8: inventory + tracking doc updates (C5)

**Files:**
- Modify: `docs/superpowers/specs/test-audit-remediation/2026-05-29-mutation-inventory.md`
- Modify: `docs/superpowers/specs/test-audit-remediation/2026-05-29-remediation-tracking.md`

- [ ] **Step 1: Annotate the `mp3.rs` rows in the inventory**

Append the outcome to each `mp3.rs:<line>` row's `Kind` column, Phase 2 convention
(`missed → **killed** (phase 3b)` / `missed → **equivalent**`):

- **Equivalent** (disjoint-bitfield `| → ^`/`| → +` only):
  the three `synchsafe_decode` `| → ^` rows, and the v2.2 24-bit decode `| → ^`
  rows in `id3v2_alloc_safe` (the `:325`/`:326` `| → ^`). **Their `| → &` siblings
  are killed, not equivalent.**
- **Killed (phase 3b):** every other `mp3.rs` row (synchsafe `<<`/`| → &`,
  `locate_audio`, `push_frame_header`, `is_id3_text_frame_id`,
  `build_id3v2_segments`, and all the killable `id3v2_alloc_safe` branches incl. the
  whole-byte high-bit `| → ^`/`| → &`).

Record any survivor that hand-apply proves genuinely equivalent beyond the list
above (note it explicitly).

- [ ] **Step 2: Mark Phase 3b complete in the tracking doc**

In `2026-05-29-remediation-tracking.md`: add "Phase 3b (MP3) complete" to the Status
line, and under Phase 3 record: 3b done — MP3 survivors killed; equivalents =
disjoint-shift `| → ^`/`| → +` in `synchsafe_decode` and the v2.2 24-bit decode
(note that the disjoint `| → &` are killed, not equivalent); no production change
(or record the scoped fix if the contingency fired).

- [ ] **Step 3: Commit**

```bash
git add docs/superpowers/specs/test-audit-remediation/2026-05-29-mutation-inventory.md \
        docs/superpowers/specs/test-audit-remediation/2026-05-29-remediation-tracking.md
git commit -m "docs(phase3b): record MP3 kills/equivalents; mark phase 3b complete"
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

- [ ] **Open the PR; `mutants.yml` `in-diff` + `canary` run on it.** After merge the
next full campaign confirms `mp3.rs` survivors dropped (excluding the documented
equivalents).

## Notes for the executor

- These are coverage gaps: **no production change is expected.** If an
  `id3v2_alloc_safe` survivor reveals a genuine off-by-one in a bound, fix it as a
  small scoped change within `mp3.rs` validation (never the audio reads — byte
  identity is untouched) and document it in Task 8.
- Inventory line numbers have **drifted**; always locate the construct by its code
  pattern before hand-applying.
- The `tag_end > data.len()` check (between the body decode and the frame walk) has
  **no listed survivor** — existing tests already cover it. The Task 4–7 reject
  fixtures exercise it incidentally; if the next campaign reports a new survivor
  there, add an explicit `tag_end == data.len()` boundary fixture.
- Only disjoint-bitfield `| → ^`/`| → +` are equivalent; disjoint `| → &` and all
  whole-byte-OR `|` mutations are **killable**. Do not over-record equivalents.
- Never leave a hand-applied mutation in the tree — always revert before the next step.
- The pre-commit hook runs `cargo fmt --check`, `clippy -D warnings`, `cargo test
  --workspace`, and `ruff`; keep each commit green.
```
