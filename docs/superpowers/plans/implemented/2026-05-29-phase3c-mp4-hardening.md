# Phase 3c — MP4 Hardening Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Kill the 44 `mp4.rs` mutation survivors (40 missed + 4 timeout) with additive in-module tests — hand-apply-verifying every kill and recording the 4 infinite-loop survivors as timeout-detected — with no production logic change expected.

**Architecture:** Extend `mp4.rs`'s existing `#[cfg(test)] mod tests` (already `use super::*`, 30 tests). Call the private survivor fns (`box_header`, `read_box`, `child_boxes`, `read_structure_from`, `read_freeform`, `read_tags`, `read_pictures`, `build_udta`, `patch_chunk_offsets`, `synthesize_layout`) directly with byte-precise box/atom fixtures built by the existing local helpers (`bx`, `mk_mp4`, `mk_mp4_co64`, `soun_trak`, `data_atom`, `mp4_with_ilst`, `inline_head`, `find_moov_in_head`) plus the production `boxed`. Hand-apply-verify each kill; record the timeouts. Byte-identity invariant untouched (no audio-read path is changed).

**Tech Stack:** Rust, `cargo test`, `proptest` + `id3` + `metaflac` (existing dev-deps). No new dependencies.

**Spec:** `docs/superpowers/specs/test-audit-remediation/2026-05-29-phase3c-mp4-hardening-design.md`
**Survivor data:** `docs/superpowers/specs/test-audit-remediation/2026-05-29-mutation-inventory.md`

---

## The hand-apply verification method (use in every kill step)

cargo-mutants is not available locally. For each targeted `function: construct: mutation`:

1. Run the new test → it passes (production is correct).
2. **Locate the construct by its code pattern** (inventory line numbers are
   approximate — never trust the raw number), apply the exact mutation, rerun
   **just that test** → it must **fail** (a failed assertion *or* a panic).
3. Revert (`git checkout -- musefs-format/src/mp4.rs`), rerun → passes again.

If step 2 still passes: strengthen the test, or — if the mutation provably yields
identical behavior — record it as an **equivalent mutant**. Never leave a mutation
applied.

## Timeout survivors are the exception (do NOT hand-apply)

The 4 timeout survivors (`BoxRef::end` ×3, `read_structure_from`'s `pos += total`)
make a box-walk non-terminating — hand-applying them **hangs the test run**.
For these: confirm a covering walk test exists (by reading it), then record them as
**timeout-detected** in the inventory (Task 7). Never apply them locally.

## Equivalent mutants

**MP4 has none to pre-record.** Every multi-byte field uses `from_be_bytes`, so there
is no disjoint-bitfield `|→^` site. The only `|` mutants are the bool `|=`
dup-accumulators in `read_structure_from`, which are killable (Task 2). If a kill
attempt during implementation proves a mutant equivalent, record it then with the
hand-apply evidence; otherwise record none.

## Pre-flight (run once before Task 1)

- [ ] **Confirm baseline green on the phase-3c branch**

```bash
git rev-parse --abbrev-ref HEAD          # expect: worktree-phase3c-mp4-hardening
cargo test -p musefs-format --features fuzzing --lib mp4
```
Expected: green — `running 31 tests` (the `--lib mp4` filter matches the 30
`mp4::tests` plus one substring-matching test from another module), 0 failed.

---

## Task 1: C1 — box primitives (`box_header`, `read_box`, `BoxRef::end`)

Kills `box_header`'s `<→<=` size bound and `read_box`'s size-0 `-→+`/`-→/`.
Confirms (does not apply) the three `BoxRef::end` timeouts.

**Files:**
- Modify (tests only): `musefs-format/src/mp4.rs` `#[cfg(test)] mod tests`.

- [ ] **Step 1: Add the box-primitive tests**

Append inside `mod tests`:

```rust
    #[test]
    fn box_header_accepts_empty_payload_box() {
        // total_len == header_len (an 8-byte box, no payload) must be accepted.
        // `< -> <=` would make the equal case reject.
        let mut h = 8u32.to_be_bytes().to_vec();
        h.extend_from_slice(b"free");
        let bh = box_header(&h, 1000).unwrap();
        assert_eq!(bh.header_len, 8);
        assert_eq!(bh.total_len, 8);
    }

    #[test]
    fn read_box_size0_extends_to_end_from_offset() {
        // A size-0 box ("extends to EOF") at pos > 0: total_len must be
        // buf.len() - pos. `- -> +` (buf.len() + pos) and `- -> /` (buf.len() / pos)
        // both diverge. The box is placed at pos = 8 with pos + 8 <= buf.len() so the
        // be_u32 size read and the kind slice both succeed BEFORE the size-0 branch.
        let mut buf = bx(b"free", b""); // 8-byte box at pos 0
        buf.extend_from_slice(&0u32.to_be_bytes()); // size32 = 0 at pos 8
        buf.extend_from_slice(b"mdat"); // kind at pos 12..16
        buf.extend_from_slice(b"AUDIOPAYLOAD"); // 12 payload bytes
        assert_eq!(buf.len(), 28);
        let b = read_box(&buf, 8).unwrap();
        assert_eq!(&b.kind, b"mdat");
        assert_eq!(b.total_len, buf.len() - 8); // 20
    }
```

- [ ] **Step 2: Run the two tests — confirm green**

```bash
cargo test -p musefs-format --features fuzzing --lib \
  mp4::tests::box_header_accepts_empty_payload_box \
  mp4::tests::read_box_size0_extends_to_end_from_offset
```
Expected: PASS (2 passed).

- [ ] **Step 3: Hand-apply-verify each kill**

For each row: apply the mutation, run the named test, confirm FAIL, then
`git checkout -- musefs-format/src/mp4.rs`.

| Construct (locate by pattern) | Mutation | Test that must FAIL |
|---|---|---|
| `box_header`: `if total_len < header_len` | `<` → `<=` | `box_header_accepts_empty_payload_box` (8 ≤ 8 → `Malformed`, unwrap panics) |
| `read_box`: `0 => (8usize, (buf.len() - pos) as u64)` | `-` → `+` | `read_box_size0_extends_to_end_from_offset` (end > buf.len() → `Malformed`) |
| same line | `-` → `/` | `read_box_size0_extends_to_end_from_offset` (total 3 < header 8 → `Malformed`) |

- [ ] **Step 4: Confirm the `BoxRef::end` timeouts are covered (do NOT apply)**

Read `walks_top_level_boxes` — it calls `child_boxes` over a 2-box buffer, whose
loop advances with `pos = b.end()`. `BoxRef::end -> 0` and `+ -> *` pin `pos` at 0
→ non-terminating. **Do not hand-apply** (it hangs). These three (`-> 0`, `-> 1`,
`+ -> *`) are recorded as timeout-detected in Task 7.

- [ ] **Step 5: Commit**

```bash
git add musefs-format/src/mp4.rs
git commit -m "$(cat <<'EOF'
test(mp4): phase 3c C1 — box-primitive mutation kills

box_header empty-payload boundary (< -> <=); read_box size-0 arithmetic
(- -> +/ /). BoxRef::end timeouts confirmed covered by walks_top_level_boxes.

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>
EOF
)"
```

---

## Task 2: C2 — `read_structure_from` structural walk

Kills the `remaining` `-→+`, the `moof`-arm delete, and the three dup `|=→&=`.
Confirms (does not apply) the `pos += total` timeout. All via `std::io::Cursor`.

**Files:**
- Modify (tests only): `musefs-format/src/mp4.rs` `#[cfg(test)] mod tests`.

- [ ] **Step 1: Add the structural-walk tests**

```rust
    #[test]
    fn read_structure_from_rejects_box_overrunning_eof() {
        // box_header's `remaining` arg is `file_len - pos`. Inflating the mdat box's
        // declared size past the bytes remaining must be rejected. `- -> +` inflates
        // `remaining` to `file_len + pos`, wrongly accepting the overrun (returns Ok).
        let mut buf = mk_mp4(true, b"AUDIO", &[0]); // [ftyp][moov][mdat], mdat last
        let scan = read_structure(&buf).unwrap();
        let mdat_start = (scan.mdat_payload_offset - scan.mdat_header.len() as u64) as usize;
        let real = u32::from_be_bytes(buf[mdat_start..mdat_start + 4].try_into().unwrap());
        buf[mdat_start..mdat_start + 4].copy_from_slice(&(real + 100).to_be_bytes());
        let mut cur = std::io::Cursor::new(buf.clone());
        assert!(read_structure_from(&mut cur, buf.len() as u64).is_err());
    }

    #[test]
    fn read_structure_from_rejects_moof() {
        // A `moof` (fragmented MP4) top-level box must be rejected via the seeking
        // path. Deleting the `b"moof"` match arm drops it to `_ => {}` and accepts.
        let mut buf = mk_mp4(true, b"AUDIO", &[0]);
        buf.extend(bx(b"moof", b"\x00\x00\x00\x00"));
        let mut cur = std::io::Cursor::new(buf.clone());
        assert!(read_structure_from(&mut cur, buf.len() as u64).is_err());
    }

    #[test]
    fn read_structure_from_rejects_duplicate_top_level_boxes() {
        // Each `dup |= X.replace(..).is_some()` accumulates a duplicate. `|= -> &=`
        // can never set `dup` (it starts false), so a duplicate box is wrongly
        // accepted. One duplicated box per kind isolates each of the three `|=` lines.
        let dup = |extra: Vec<u8>| {
            let mut buf = mk_mp4(true, b"AUDIO", &[0]);
            buf.extend(extra);
            let mut cur = std::io::Cursor::new(buf.clone());
            read_structure_from(&mut cur, buf.len() as u64).is_err()
        };
        assert!(dup(bx(b"ftyp", b"M4A isom")), "duplicate ftyp must reject"); // ftyp |= line
        // duplicate moov: reuse the moov from a fresh fixture so it is structurally valid.
        let extra_moov = {
            let other = mk_mp4(true, b"AUDIO", &[0]);
            let s = read_structure(&other).unwrap();
            s.moov
        };
        assert!(dup(extra_moov), "duplicate moov must reject"); // moov |= line
        assert!(dup(bx(b"mdat", b"Y")), "duplicate mdat must reject"); // mdat |= line
    }
```

- [ ] **Step 2: Run the three tests — confirm green**

```bash
cargo test -p musefs-format --features fuzzing --lib \
  mp4::tests::read_structure_from_rejects_box_overrunning_eof \
  mp4::tests::read_structure_from_rejects_moof \
  mp4::tests::read_structure_from_rejects_duplicate_top_level_boxes
```
Expected: PASS (3 passed).

- [ ] **Step 3: Hand-apply-verify each kill**

| Construct (locate by pattern) | Mutation | Test that must FAIL |
|---|---|---|
| `read_structure_from`: `box_header(&hdr, file_len - pos)?` | `-` → `+` | `..._rejects_box_overrunning_eof` (mutant accepts → returns Ok) |
| `read_structure_from`: `b"moof" => return Err(FormatError::NotMp4.into()),` | delete arm | `..._rejects_moof` (mutant ignores moov → returns Ok) |
| `read_structure_from`: `dup |= ftyp.replace((pos, bh)).is_some(),` | `\|=` → `&=` | `..._rejects_duplicate_top_level_boxes` (1st `assert!` — dup ftyp) |
| `read_structure_from`: `dup |= moov.replace((pos, bh)).is_some(),` | `\|=` → `&=` | `..._rejects_duplicate_top_level_boxes` (2nd `assert!` — dup moov) |
| `read_structure_from`: `dup |= mdat.replace((pos, bh)).is_some(),` | `\|=` → `&=` | `..._rejects_duplicate_top_level_boxes` (3rd `assert!` — dup mdat) |

After each: `git checkout -- musefs-format/src/mp4.rs`.

- [ ] **Step 4: Confirm the `pos += total` timeout is covered (do NOT apply)**

Read `read_structure_from_matches_buffer_path` — it walks every top-level box via
`pos += total`. `+= -> *=` keeps `pos` at 0 (`0 *= total`) → non-terminating.
**Do not hand-apply** (it hangs). Recorded as timeout-detected in Task 7.

- [ ] **Step 5: Commit**

```bash
git add musefs-format/src/mp4.rs
git commit -m "$(cat <<'EOF'
test(mp4): phase 3c C2 — read_structure_from walk kills

remaining `file_len - pos` (- -> +), moof-arm reject, and the three dup
`|= -> &=` accumulators. pos += total timeout confirmed covered.

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>
EOF
)"
```

---

## Task 3: C3 — metadata read (`read_freeform`, `read_tags`, `read_pictures`)

Kills the length-guard boundaries, the `||`/`&&` short-circuits, the trkn/disk
branches, and the PNG `data`-type arm.

**Files:**
- Modify (tests only): `musefs-format/src/mp4.rs` `#[cfg(test)] mod tests`.

- [ ] **Step 1: Add the metadata-read tests**

```rust
    #[test]
    fn read_freeform_accepts_minimal_name_and_data() {
        // name payload == 4 (empty name) and data payload == 8 (empty value) is the
        // boundary of `np.len() < 4 || dp.len() < 8`. Both operands at the boundary,
        // so flipping EITHER `<` to `==`/`<=` makes that side true -> None.
        let name_body = 0u32.to_be_bytes().to_vec(); // exactly 4 bytes
        let mut data = 1u32.to_be_bytes().to_vec(); // type 1 = UTF-8
        data.extend_from_slice(&0u32.to_be_bytes()); // locale -> dp.len() == 8
        let mut inner = boxed(b"name", &name_body);
        inner.extend(boxed(b"data", &data));
        let (key, value) = read_freeform(&inner).unwrap();
        assert_eq!(key, ""); // empty name, not in vocabulary -> verbatim ""
        assert_eq!(value, "");
    }

    #[test]
    fn read_freeform_short_name_returns_none() {
        // name payload 3 bytes (< 4) with a valid 8-byte data payload. `|| -> &&`
        // makes `true && false == false`, falling through to `&np[4..]` (out of bounds
        // -> panic).
        let name_body = vec![0u8, 0, 0]; // 3 bytes
        let mut data = 1u32.to_be_bytes().to_vec();
        data.extend_from_slice(&0u32.to_be_bytes());
        let mut inner = boxed(b"name", &name_body);
        inner.extend(boxed(b"data", &data));
        assert!(read_freeform(&inner).is_none());
    }

    #[test]
    fn read_freeform_mean_payload_exactly_4_uses_empty_mean() {
        // mean payload == 4 (FullBox prefix, empty mean). `p.len() >= 4` must take the
        // utf8 branch (mean ""), so the vocabulary does NOT fold the iTunes name.
        // `>= -> <` falls to the default "com.apple.iTunes" mean and wrongly folds.
        let mean_body = vec![0u8, 0, 0, 0]; // exactly 4 bytes
        let mut name_body = 0u32.to_be_bytes().to_vec();
        name_body.extend_from_slice(b"MusicBrainz Album Id");
        let mut data = 1u32.to_be_bytes().to_vec();
        data.extend_from_slice(&0u32.to_be_bytes());
        data.extend_from_slice(b"abc-123");
        let mut inner = boxed(b"mean", &mean_body);
        inner.extend(boxed(b"name", &name_body));
        inner.extend(boxed(b"data", &data));
        let (key, value) = read_freeform(&inner).unwrap();
        assert_eq!(key, "MusicBrainz Album Id"); // empty mean -> not folded
        assert_eq!(value, "abc-123");
    }

    #[test]
    fn read_tags_data_payload_exactly_8_is_read() {
        // A `data` payload of exactly 8 bytes (type+locale, empty value) is the
        // boundary of `dp.len() < 8`. The (empty) value must be read; `< -> ==`/`<= `
        // would skip it.
        let atoms = bx(b"\xa9nam", &data_atom(1, b"")); // dp.len() == 8
        let buf = mp4_with_ilst(&atoms, true);
        assert!(read_tags(&buf).contains(&("title".into(), "".into())));
    }

    #[test]
    fn read_tags_disk_exact_4_byte_value_yields_discnumber() {
        // disk atom, value exactly 4 bytes: `kind == disk` (== branch) `&&`
        // `value.len() >= 4` (>= branch). Kills `== -> !=` (mutant skips a real disk)
        // and `>= -> <` (mutant skips the boundary length).
        let atoms = bx(b"disk", &data_atom(0, &[0, 0, 0, 2])); // disc 2, value len 4
        let buf = mp4_with_ilst(&atoms, true);
        assert!(read_tags(&buf).contains(&("discnumber".into(), "2".into())));
    }

    #[test]
    fn read_tags_disk_short_value_is_skipped() {
        // disk with a value shorter than 4 bytes: the guard is false. `&& -> ||`
        // makes it true and indexes value[2]/value[3] out of bounds (panic).
        let atoms = bx(b"disk", &data_atom(0, &[0, 0])); // value len 2
        let buf = mp4_with_ilst(&atoms, true);
        assert!(!read_tags(&buf).iter().any(|(k, _)| k == "discnumber"));
    }

    #[test]
    fn read_tags_trkn_short_value_is_skipped() {
        // trkn with a value shorter than 4 bytes: `kind == trkn && value.len() >= 4`
        // is false. `&& -> ||` makes it true and indexes value[2]/value[3] (panic).
        let atoms = bx(b"trkn", &data_atom(0, &[0, 0])); // value len 2
        let buf = mp4_with_ilst(&atoms, true);
        assert!(!read_tags(&buf).iter().any(|(k, _)| k == "tracknumber"));
    }

    #[test]
    fn read_pictures_data_payload_exactly_8_is_read() {
        // covr/data payload of exactly 8 bytes (type+locale, empty image) is the
        // boundary of `dp.len() < 8`; the (empty) picture must be read.
        let buf = mp4_with_ilst(&bx(b"covr", &data_atom(13, b"")), true);
        let pics = read_pictures(&buf);
        assert_eq!(pics.len(), 1);
        assert_eq!(pics[0].mime, "image/jpeg");
        assert!(pics[0].data.is_empty());
    }

    #[test]
    fn read_pictures_recognizes_png() {
        // A covr `data` atom with type code 14 is PNG. Deleting the `14 =>` match arm
        // drops it to `_ => continue` and yields no picture.
        let png = [0x89, b'P', b'N', b'G', 1, 2, 3];
        let buf = mp4_with_ilst(&bx(b"covr", &data_atom(14, &png)), false);
        let pics = read_pictures(&buf);
        assert_eq!(pics.len(), 1);
        assert_eq!(pics[0].mime, "image/png");
        assert_eq!(pics[0].data, png);
    }
```

- [ ] **Step 2: Run the C3 tests — confirm green**

```bash
cargo test -p musefs-format --features fuzzing --lib mp4::tests::read_freeform
cargo test -p musefs-format --features fuzzing --lib mp4::tests::read_tags
cargo test -p musefs-format --features fuzzing --lib mp4::tests::read_pictures
```
Expected: each PASS (the new tests plus the pre-existing `read_*` tests).

Hand-apply-verify in three per-function sub-batches (Steps 3a–3c). 15 mutations
total; keeping them grouped by function bounds the apply/revert cycle and the
risk of leaving a stray edit. **After every single mutation:**
`git checkout -- musefs-format/src/mp4.rs`, then rerun the named test to confirm it
is green again before applying the next.

> Note: the `trkn`/`disk` `&& -> ||` and `read_freeform` `|| -> &&` kills are
> **panics** (out-of-bounds slice), which count as kills per the verification method.

- [ ] **Step 3a: Hand-apply-verify the `read_freeform` group (6 mutations)**

| Construct (locate by pattern) | Mutation | Test that must FAIL |
|---|---|---|
| `read_freeform`: `if np.len() < 4 \|\| dp.len() < 8` (the `np.len()` `<`) | `<` → `==` | `read_freeform_accepts_minimal_name_and_data` |
| same `<` | `<` → `<=` | `read_freeform_accepts_minimal_name_and_data` |
| `read_freeform`: same line (the `dp.len()` `<`) | `<` → `==` | `read_freeform_accepts_minimal_name_and_data` |
| same `<` | `<` → `<=` | `read_freeform_accepts_minimal_name_and_data` |
| `read_freeform`: the `\|\|` on that line | `\|\|` → `&&` | `read_freeform_short_name_returns_none` (panics on `np[4..]`) |
| `read_freeform`: `if p.len() >= 4` (mean) | `>=` → `<` | `read_freeform_mean_payload_exactly_4_uses_empty_mean` |

Run between mutations: `cargo test -p musefs-format --features fuzzing --lib mp4::tests::read_freeform`

- [ ] **Step 3b: Hand-apply-verify the `read_tags` group (6 mutations)**

| Construct (locate by pattern) | Mutation | Test that must FAIL |
|---|---|---|
| `read_tags`: `if dp.len() < 8` | `<` → `==` | `read_tags_data_payload_exactly_8_is_read` |
| same `<` | `<` → `<=` | `read_tags_data_payload_exactly_8_is_read` |
| `read_tags`: `&atom.kind == b"trkn" && value.len() >= 4` (the `&&`) | `&&` → `\|\|` | `read_tags_trkn_short_value_is_skipped` (panics) |
| `read_tags`: `&atom.kind == b"disk" && value.len() >= 4` (the `==`) | `==` → `!=` | `read_tags_disk_exact_4_byte_value_yields_discnumber` |
| same line (the `&&`) | `&&` → `\|\|` | `read_tags_disk_short_value_is_skipped` (panics) |
| same line (the `>=`) | `>=` → `<` | `read_tags_disk_exact_4_byte_value_yields_discnumber` |

Run between mutations: `cargo test -p musefs-format --features fuzzing --lib mp4::tests::read_tags`

- [ ] **Step 3c: Hand-apply-verify the `read_pictures` group (3 mutations)**

| Construct (locate by pattern) | Mutation | Test that must FAIL |
|---|---|---|
| `read_pictures`: `if dp.len() < 8` | `<` → `==` | `read_pictures_data_payload_exactly_8_is_read` |
| same `<` | `<` → `<=` | `read_pictures_data_payload_exactly_8_is_read` |
| `read_pictures`: `14 => "image/png",` | delete arm | `read_pictures_recognizes_png` |

Run between mutations: `cargo test -p musefs-format --features fuzzing --lib mp4::tests::read_pictures`

- [ ] **Step 4: Commit**

```bash
git add musefs-format/src/mp4.rs
git commit -m "$(cat <<'EOF'
test(mp4): phase 3c C3 — metadata-read mutation kills

read_freeform/read_tags/read_pictures length-guard boundaries, the ||/&&
short-circuits, the trkn/disk branches, and the PNG data-type arm.

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>
EOF
)"
```

---

## Task 4: C4a — `build_udta`

Kills the png type-code `==→!=`, the covr/data size arithmetic `+→-/*`, and the
`udta_size > u32::MAX` guard (`>→>=`) via a cheap `data_len` boundary.

**Files:**
- Modify (tests only): `musefs-format/src/mp4.rs` `#[cfg(test)] mod tests`.

- [ ] **Step 1: Add the build_udta tests**

```rust
    #[test]
    fn build_udta_png_art_uses_type_code_14() {
        // PNG art => covr/data type code 14; JPEG => 13. `== -> !=` flips them.
        for (mime, expected) in [("image/png", 14u32), ("image/jpeg", 13u32)] {
            let art = ArtInput {
                art_id: 1,
                mime: mime.into(),
                description: String::new(),
                picture_type: 3,
                width: 0,
                height: 0,
                data_len: 10,
            };
            let (prefix, _) = build_udta(&[TagInput::new("title", "T")], Some(&art)).unwrap();
            // covr layout: [covr_size u32]["covr"][data_size u32]["data"][type u32][locale u32]
            let cpos = prefix.windows(4).position(|w| w == b"covr").expect("covr");
            assert_eq!(&prefix[cpos + 8..cpos + 12], b"data");
            let type_code = u32::from_be_bytes(prefix[cpos + 12..cpos + 16].try_into().unwrap());
            assert_eq!(type_code, expected, "mime {mime}");
        }
    }

    #[test]
    fn build_udta_art_box_sizes_are_exact() {
        // data_size = 8 + 8 + data_len; covr_size = 8 + data_size. The `+ -> -`/`+ -> *`
        // mutations change the emitted box sizes.
        let art = ArtInput {
            art_id: 1,
            mime: "image/jpeg".into(),
            description: String::new(),
            picture_type: 3,
            width: 0,
            height: 0,
            data_len: 10,
        };
        let (prefix, _) = build_udta(&[TagInput::new("title", "T")], Some(&art)).unwrap();
        let cpos = prefix.windows(4).position(|w| w == b"covr").expect("covr");
        let covr_size = u32::from_be_bytes(prefix[cpos - 4..cpos].try_into().unwrap());
        let data_size = u32::from_be_bytes(prefix[cpos + 4..cpos + 8].try_into().unwrap());
        assert_eq!(data_size, 8 + 8 + 10); // 26
        assert_eq!(covr_size, 8 + data_size); // 34
    }

    #[test]
    fn build_udta_udta_size_exactly_u32_max_is_ok() {
        // The guard is `udta_size > u32::MAX` (strict). udta_size == u32::MAX must be
        // accepted; `> -> >=` rejects the exact boundary. data_len is reserved as a
        // number (no image bytes), so the boundary is cheap to hit.
        fn art(data_len: u64) -> ArtInput {
            ArtInput {
                art_id: 1,
                mime: "image/jpeg".into(),
                description: String::new(),
                picture_type: 3,
                width: 0,
                height: 0,
                data_len,
            }
        }
        // Derive the fixed overhead: with data_len 0, udta_size == overhead.
        let (p0, _) = build_udta(&[TagInput::new("title", "T")], Some(&art(0))).unwrap();
        let overhead = u32::from_be_bytes(p0[0..4].try_into().unwrap()) as u64;
        let max_len = u32::MAX as u64 - overhead;

        let (p_max, art_len) =
            build_udta(&[TagInput::new("title", "T")], Some(&art(max_len))).unwrap();
        assert_eq!(art_len, max_len);
        assert_eq!(u32::from_be_bytes(p_max[0..4].try_into().unwrap()), u32::MAX);

        assert!(matches!(
            build_udta(&[TagInput::new("title", "T")], Some(&art(max_len + 1))),
            Err(FormatError::TooLarge)
        ));
    }
```

- [ ] **Step 2: Run the build_udta tests — confirm green**

```bash
cargo test -p musefs-format --features fuzzing --lib mp4::tests::build_udta
```
Expected: PASS (the new tests plus the pre-existing `build_udta_*`).

- [ ] **Step 3: Hand-apply-verify each kill**

| Construct (locate by pattern) | Mutation | Test that must FAIL |
|---|---|---|
| `build_udta`: `let type_code: u32 = if a.mime == "image/png"` | `==` → `!=` | `build_udta_png_art_uses_type_code_14` |
| `build_udta`: `let data_size = 8 + 8 + a.data_len;` (first `+`) | `+` → `-` | `build_udta_art_box_sizes_are_exact` |
| same line (either `+`) | `+` → `*` | `build_udta_art_box_sizes_are_exact` |
| `build_udta`: `let covr_size = 8 + data_size;` | `+` → `*` | `build_udta_art_box_sizes_are_exact` |
| `build_udta`: `if udta_size > u32::MAX as u64` | `>` → `>=` | `build_udta_udta_size_exactly_u32_max_is_ok` (boundary rejected → unwrap panics) |

After each: `git checkout -- musefs-format/src/mp4.rs`.

- [ ] **Step 4: Commit**

```bash
git add musefs-format/src/mp4.rs
git commit -m "$(cat <<'EOF'
test(mp4): phase 3c C4a — build_udta mutation kills

png type-code (== -> !=), covr/data size arithmetic (+ -> -/*), and the
udta_size > u32::MAX guard via a cheap data_len boundary.

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>
EOF
)"
```

---

## Task 5: C4b — `patch_chunk_offsets`

Kills all 9 survivors: the `:595` stco overflow/underflow guard (5), the `:590`
bounds check (2), and the `:601` co64 `v < 0` (2).

**Files:**
- Modify (tests only): `musefs-format/src/mp4.rs` `#[cfg(test)] mod tests`.

- [ ] **Step 1: Add the patch_chunk_offsets tests**

```rust
    #[test]
    fn patch_chunk_offsets_stco_overflow_and_underflow_boundaries() {
        // kept = a single soun trak with one stco entry (offset 0). v = 0 + delta is
        // guarded by `v < 0 || v > u32::MAX`. Boundary deltas pin every guard mutant;
        // delta 0 (accepted) also pins the `:590` `+ -> *` bound at i = 0.
        let mut k = soun_trak();
        assert!(patch_chunk_offsets(&mut k, 0).is_ok()); // v == 0

        let mut k = soun_trak();
        assert!(patch_chunk_offsets(&mut k, u32::MAX as i64).is_ok()); // v == u32::MAX

        let mut k = soun_trak();
        assert!(matches!(
            patch_chunk_offsets(&mut k, u32::MAX as i64 + 1), // v == u32::MAX + 1
            Err(FormatError::TooLarge)
        ));

        let mut k = soun_trak();
        assert!(matches!(
            patch_chunk_offsets(&mut k, -1), // v == -1
            Err(FormatError::TooLarge)
        ));
    }

    #[test]
    fn patch_chunk_offsets_rejects_count_past_table() {
        // stco declares 2 entries but only 1 entry's bytes are present (followed by an
        // unrelated `free` box for padding). `pos + entry > start + len` must reject
        // the 2nd entry. `+ -> -` shrinks the bound and reads into the `free` box
        // instead of erroring (returns Ok).
        let mut stco = vec![0u8; 4]; // version/flags
        stco.extend_from_slice(&2u32.to_be_bytes()); // count = 2 (a lie)
        stco.extend_from_slice(&0u32.to_be_bytes()); // only 1 entry present
        let stbl = bx(b"stbl", &[bx(b"stco", &stco), bx(b"free", &[0u8; 8])].concat());
        let mut kept = bx(b"trak", &bx(b"mdia", &bx(b"minf", &stbl)));
        assert!(matches!(
            patch_chunk_offsets(&mut kept, 0),
            Err(FormatError::Malformed)
        ));
    }

    #[test]
    fn patch_chunk_offsets_co64_zero_offset_is_ok() {
        // co64 path guard is `v < 0`. offset 0 + delta 0 => v == 0 must be accepted;
        // `< -> ==`/`<= ` reject the boundary.
        let mut co64 = vec![0u8; 4]; // version/flags
        co64.extend_from_slice(&1u32.to_be_bytes()); // count 1
        co64.extend_from_slice(&0u64.to_be_bytes()); // offset 0
        let stbl = bx(b"stbl", &bx(b"co64", &co64));
        let mut kept = bx(b"trak", &bx(b"mdia", &bx(b"minf", &stbl)));
        assert!(patch_chunk_offsets(&mut kept, 0).is_ok());
    }
```

- [ ] **Step 2: Run the patch_chunk_offsets tests — confirm green**

```bash
cargo test -p musefs-format --features fuzzing --lib mp4::tests::patch_chunk_offsets
```
Expected: PASS (3 passed).

- [ ] **Step 3: Hand-apply-verify each kill**

| Construct (locate by pattern) | Mutation | Test that must FAIL |
|---|---|---|
| `patch_chunk_offsets`: `if pos + entry > start + len` | `+` (pos+entry) → `-` | `patch_chunk_offsets_rejects_count_past_table` (mutant returns Ok) |
| same `+` (pos+entry) | `+` → `*` | `patch_chunk_offsets_stco_overflow_and_underflow_boundaries` (delta 0 → `Malformed` at i=0) |
| `patch_chunk_offsets`: `if v < 0 \|\| v > u32::MAX as i64` (the `v < 0`) | `<` → `==` | `..._stco_overflow_and_underflow_boundaries` (delta 0: 0 == 0 → `TooLarge`) |
| same `<` | `<` → `<=` | `..._stco_overflow_and_underflow_boundaries` (delta 0: 0 ≤ 0 → `TooLarge`) |
| same line (the `\|\|`) | `\|\|` → `&&` | `..._stco_overflow_and_underflow_boundaries` (delta u32::MAX+1 wrongly accepted → not Err) |
| same line (the `v > u32::MAX`) | `>` → `==` | `..._stco_overflow_and_underflow_boundaries` (delta u32::MAX: == → `TooLarge`) |
| same `>` | `>` → `>=` | `..._stco_overflow_and_underflow_boundaries` (delta u32::MAX: ≥ → `TooLarge`) |
| `patch_chunk_offsets`: co64 `if v < 0` | `<` → `==` | `patch_chunk_offsets_co64_zero_offset_is_ok` (0 == 0 → `Malformed`) |
| same co64 `<` | `<` → `<=` | `patch_chunk_offsets_co64_zero_offset_is_ok` (0 ≤ 0 → `Malformed`) |

After each: `git checkout -- musefs-format/src/mp4.rs`.

- [ ] **Step 4: Commit**

```bash
git add musefs-format/src/mp4.rs
git commit -m "$(cat <<'EOF'
test(mp4): phase 3c C4b — patch_chunk_offsets mutation kills

stco overflow/underflow guard, the count-past-table bounds check, and the
co64 v < 0 boundary.

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>
EOF
)"
```

---

## Task 6: C4c — `synthesize_layout`

Kills the `new_moov_size > u32::MAX` guard (`>→==`, `>→>=`) via a cheap `data_len`
boundary driven through the full synthesis path.

**Files:**
- Modify (tests only): `musefs-format/src/mp4.rs` `#[cfg(test)] mod tests`.

- [ ] **Step 1: Add the synthesize_layout test**

```rust
    #[test]
    fn synthesize_new_moov_size_exactly_u32_max_is_ok() {
        // `if new_moov_size > u32::MAX` is strict. new_moov_size == u32::MAX must be
        // accepted; `> -> ==`/`>= ` reject the exact boundary. data_len (the art size)
        // is reserved as a number, so the boundary is cheap. new_moov_size == overhead
        // + data_len, so probe overhead with data_len 0 first.
        fn art(data_len: u64) -> ArtInput {
            ArtInput {
                art_id: 1,
                mime: "image/jpeg".into(),
                description: String::new(),
                picture_type: 3,
                width: 0,
                height: 0,
                data_len,
            }
        }
        let buf = mk_mp4(true, b"AUDIO", &[0]);
        let scan = read_structure(&buf).unwrap();

        let layout0 =
            synthesize_layout(&scan, &[TagInput::new("title", "T")], &[art(0)]).unwrap();
        let head0 = inline_head(&layout0);
        let overhead = find_moov_in_head(&head0).total_len as u64; // == new_moov_size at data_len 0
        let max_len = u32::MAX as u64 - overhead;

        assert!(
            synthesize_layout(&scan, &[TagInput::new("title", "T")], &[art(max_len)]).is_ok()
        );
        assert!(matches!(
            synthesize_layout(&scan, &[TagInput::new("title", "T")], &[art(max_len + 1)]),
            Err(FormatError::TooLarge)
        ));
    }
```

- [ ] **Step 2: Run the test — confirm green**

```bash
cargo test -p musefs-format --features fuzzing --lib \
  mp4::tests::synthesize_new_moov_size_exactly_u32_max_is_ok
```
Expected: PASS.

- [ ] **Step 3: Hand-apply-verify each kill**

| Construct (locate by pattern) | Mutation | Test that must FAIL |
|---|---|---|
| `synthesize_layout`: `if new_moov_size > u32::MAX as u64` | `>` → `==` | `synthesize_new_moov_size_exactly_u32_max_is_ok` (boundary → `TooLarge`, unwrap/is_ok panics) |
| same `>` | `>` → `>=` | `synthesize_new_moov_size_exactly_u32_max_is_ok` |

After each: `git checkout -- musefs-format/src/mp4.rs`.

- [ ] **Step 4: Commit**

```bash
git add musefs-format/src/mp4.rs
git commit -m "$(cat <<'EOF'
test(mp4): phase 3c C4c — synthesize_layout mutation kills

new_moov_size > u32::MAX guard (> -> ==/>=) via a cheap data_len boundary
driven through the full synthesis path.

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>
EOF
)"
```

---

## Task 7: C5 — inventory + tracking docs

Annotate the killed/timeout-detected survivors and mark Phase 3c complete.

**Files:**
- Modify: `docs/superpowers/specs/test-audit-remediation/2026-05-29-mutation-inventory.md`
- Modify: `docs/superpowers/specs/test-audit-remediation/2026-05-29-remediation-tracking.md`

- [ ] **Step 1: Annotate the `mp4.rs` rows in the inventory**

In `2026-05-29-mutation-inventory.md`, edit the 44 `mp4.rs` survivor rows
(`mp4.rs:35` … `mp4.rs:638`) in the `musefs-format` survivor table. For each
**missed** row, change the `Kind` cell `missed` → `missed → **killed** (phase 3c)`.
For the 4 **timeout** rows (`mp4.rs:35` ×3 = `BoxRef::end`; `mp4.rs:285` =
`read_structure_from`'s `pos += total`), change `timeout` →
`timeout → **timeout-detected**` (matching the Phase 2 `ogg/page.rs` convention).
Record that the only `|` mutants (`read_structure_from` `|= -> &=`) are **killed**,
not equivalent — no mp4.rs mutant is recorded as equivalent.

- [ ] **Step 2: Mark Phase 3c complete in the tracking doc**

In `2026-05-29-remediation-tracking.md`:
- Update the `**Status:**` line to note Phase 3c (MP4) complete.
- In the **Phase 3** section, mark the MP4 slice complete (mirror how 3a/3b are
  annotated), noting WAV (3d) remains and that the non-FLAC read-fidelity dimension
  of finding #5 is still tracked separately.

- [ ] **Step 3: Run the full workspace suite + lints (acceptance gate)**

```bash
cargo test --workspace
cargo test -p musefs-format --features fuzzing
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```
Expected: all green. (The pre-commit hook also runs these.)

- [ ] **Step 4: Commit**

```bash
git add docs/superpowers/specs/test-audit-remediation/2026-05-29-mutation-inventory.md \
        docs/superpowers/specs/test-audit-remediation/2026-05-29-remediation-tracking.md
git commit -m "$(cat <<'EOF'
docs(phase3c): record MP4 kills + mark phase complete

Annotate the 44 mp4.rs survivors (40 missed -> killed, 4 timeout ->
timeout-detected) and mark Phase 3c complete in the tracking doc. No mp4.rs
mutant is equivalent (the |= dup-accumulators are killed).

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>
EOF
)"
```

---

## Coverage map (every survivor → its kill)

| Survivor(s) | Task | Test |
|---|---|---|
| `BoxRef::end:35` ×3 (`→0`,`→1`,`+→*`) | 1 | timeout-detected (covered by `walks_top_level_boxes`) |
| `box_header:75` `<→<=` | 1 | `box_header_accepts_empty_payload_box` |
| `read_box:105` `-→+`, `-→/` | 1 | `read_box_size0_extends_to_end_from_offset` |
| `read_structure_from:276` `-→+` | 2 | `read_structure_from_rejects_box_overrunning_eof` |
| `read_structure_from:279` `moof` arm | 2 | `read_structure_from_rejects_moof` |
| `read_structure_from:280/281/282` `|=→&=` ×3 | 2 | `read_structure_from_rejects_duplicate_top_level_boxes` |
| `read_structure_from:285` `+=→*=` | 2 | timeout-detected (covered by `read_structure_from_matches_buffer_path`) |
| `read_freeform:337` `<→==`/`<→<=` ×4, `||→&&` | 3 | `read_freeform_accepts_minimal_name_and_data`, `read_freeform_short_name_returns_none` |
| `read_freeform:354` `>=→<` | 3 | `read_freeform_mean_payload_exactly_4_uses_empty_mean` |
| `read_tags:388` `<→==`/`<→<=` | 3 | `read_tags_data_payload_exactly_8_is_read` |
| `read_tags:396` `&&→||` | 3 | `read_tags_trkn_short_value_is_skipped` |
| `read_tags:401` `==→!=`, `&&→||`, `>=→<` | 3 | `read_tags_disk_exact_4_byte_value_yields_discnumber`, `read_tags_disk_short_value_is_skipped` |
| `read_pictures:428` `<→==`/`<→<=` | 3 | `read_pictures_data_payload_exactly_8_is_read` |
| `read_pictures:433` arm `14` | 3 | `read_pictures_recognizes_png` |
| `build_udta:539` `==→!=` | 4 | `build_udta_png_art_uses_type_code_14` |
| `build_udta:540` `+→-`/`+→*` ×3, `:541` `+→*` | 4 | `build_udta_art_box_sizes_are_exact` |
| `build_udta:566` `>→>=` | 4 | `build_udta_udta_size_exactly_u32_max_is_ok` |
| `patch_chunk_offsets:590` `+→-`, `+→*` | 5 | `..._rejects_count_past_table`, `..._stco_overflow_and_underflow_boundaries` |
| `patch_chunk_offsets:595` `<→==`/`<→<=`, `||→&&`, `>→==`/`>→>=` | 5 | `patch_chunk_offsets_stco_overflow_and_underflow_boundaries` |
| `patch_chunk_offsets:601` `<→==`/`<→<=` | 5 | `patch_chunk_offsets_co64_zero_offset_is_ok` |
| `synthesize_layout:638` `>→==`/`>→>=` | 6 | `synthesize_new_moov_size_exactly_u32_max_is_ok` |

40 missed killed by tests; 4 timeouts recorded. Total survivors addressed: 44.
