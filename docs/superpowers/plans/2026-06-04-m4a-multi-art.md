# M4A Multi-Art Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Serve and ingest multiple embedded pictures in M4A (one `covr` atom, N `data` sub-atoms — the iTunes convention), and sync multiple images from the Picard/beets plugins into the DB.

**Architecture:** Rust changes are confined to `musefs-format/src/mp4.rs` (scan reads all `data` children of `covr`; synthesis emits one `data` atom per art row) plus test surface (proptest, fuzz fixture/seed, interop emitter). Python changes ripple from `musefs_common` (`ArtImage` dataclass, list-shaped `Record.art`, multi-row `replace_track_art`) into the Picard plugin (`images()` with picture-type mapping, replacing `front_cover`) and the beets plugin (single cover wrapped in the new list shape). No schema change; `track_art` already supports N rows per track.

**Tech Stack:** Rust (workspace; proptest, cargo-mutants, cargo-fuzz), Python (pytest, mutagen for interop), SQLite.

**Spec:** `docs/superpowers/specs/2026-06-04-m4a-multi-art-design.md`

**Conventions that apply to every task:**
- Run `cargo fmt --all` before each Rust commit (CI has a fmt gate).
- Stage files by name; never `git add -A`.
- Python suites: `contrib/python-musefs` runs self-contained (`cd contrib/python-musefs && python -m pytest`); beets uses its venv (`contrib/beets/.venv/bin/python -m pytest tests` from `contrib/beets`); Picard runs `cd contrib/picard && python -m pytest tests` (Qt-fixture tests skip without pytest-qt — that's expected).

---

### Task 1: `read_pictures` reads every `data` child of `covr`

**Files:**
- Modify: `musefs-format/src/mp4.rs` (`read_pictures`, ~line 431; tests near `read_pictures_recognizes_png`, ~line 1850)

- [ ] **Step 1: Write the failing tests**

Add to the `tests` module in `musefs-format/src/mp4.rs`, directly after `read_pictures_recognizes_png` (~line 1862). The helpers `bx`, `data_atom`, and `mp4_with_ilst` already exist in the module.

```rust
    #[test]
    fn read_pictures_reads_all_data_atoms_in_one_covr() {
        // iTunes convention: multiple artworks are multiple `data` children of
        // one `covr`. An unknown type code skips that child only, not its
        // siblings.
        let jpeg = [0xFF, 0xD8, 0xFF, 1];
        let png = [0x89, b'P', b'N', b'G', 2];
        let covr = bx(
            b"covr",
            &[
                data_atom(13, &jpeg),
                data_atom(99, b"skipped"), // unknown type code: this child only
                data_atom(14, &png),
            ]
            .concat(),
        );
        let buf = mp4_with_ilst(&covr, true);
        let pics = read_pictures(&buf);
        assert_eq!(pics.len(), 2);
        assert_eq!(pics[0].mime, "image/jpeg");
        assert_eq!(pics[0].data, jpeg);
        assert_eq!(pics[1].mime, "image/png");
        assert_eq!(pics[1].data, png);
    }

    #[test]
    fn read_pictures_skips_non_data_children_of_covr() {
        // A non-`data` child inside covr (rare but legal) is silently skipped.
        let png = [0x89, b'P', b'N', b'G'];
        let covr = bx(b"covr", &[bx(b"free", b"pad"), data_atom(14, &png)].concat());
        let buf = mp4_with_ilst(&covr, false);
        let pics = read_pictures(&buf);
        assert_eq!(pics.len(), 1);
        assert_eq!(pics[0].mime, "image/png");
        assert_eq!(pics[0].data, png);
    }
```

- [ ] **Step 2: Run tests to verify the first fails**

Run: `cargo test -p musefs-format read_pictures`
Expected: `read_pictures_reads_all_data_atoms_in_one_covr` FAILS (`pics.len()` is 1 — only the first `data` child is read). `read_pictures_skips_non_data_children_of_covr` passes already (`find_box` scans past non-`data` children); it stays as regression coverage for the new loop.

- [ ] **Step 3: Implement the inner loop**

Replace the body of `read_pictures` (keep the doc comment, but extend its first line):

```rust
/// Lenient: returns empty / skips any malformed atom and never errors — this only
/// seeds cover art from existing files, so a missing or garbled picture must simply be absent.
/// Every `data` child of every `covr` atom yields one picture (the iTunes
/// multiple-artwork convention); non-`data` children are skipped.
pub fn read_pictures(buf: &[u8]) -> Vec<EmbeddedPicture> {
    let Some((start, len)) = ilst_region(buf) else {
        return Vec::new();
    };
    let ilst = &buf[start..start + len];
    let mut out = Vec::new();
    for atom in child_boxes(ilst).unwrap_or_default() {
        if &atom.kind != b"covr" {
            continue;
        }
        let inner = atom.payload(ilst);
        for data in child_boxes(inner).unwrap_or_default() {
            if &data.kind != b"data" {
                continue;
            }
            let dp = data.payload(inner);
            if dp.len() < 8 {
                continue;
            }
            let mime = match u32::from_be_bytes([dp[0], dp[1], dp[2], dp[3]]) {
                13 => "image/jpeg",
                14 => "image/png",
                _ => continue,
            };
            out.push(EmbeddedPicture {
                mime: mime.to_string(),
                picture_type: 3,
                description: String::new(),
                width: 0,
                height: 0,
                data: dp[8..].to_vec(),
            });
        }
    }
    out
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p musefs-format read_pictures`
Expected: all `read_pictures*` tests PASS (including the pre-existing ones: `..._data_payload_exactly_8_is_read`, `..._recognizes_png`).

- [ ] **Step 5: Commit**

```bash
cargo fmt --all
git add musefs-format/src/mp4.rs
git commit -m "feat(format): read every covr data atom in M4A scan"
```

---

### Task 2: `build_udta` takes `arts: &[ArtInput]`, emits one `covr` with N `data` atoms

**Files:**
- Modify: `musefs-format/src/mp4.rs` (`build_udta` ~line 618, `synthesize_layout` ~line 794, and the existing tests that call `build_udta`)

- [ ] **Step 1: Write the failing test**

Add to the `tests` module after `build_udta_art_box_sizes_are_exact` (~line 1910):

```rust
    #[test]
    fn build_udta_multiple_arts_one_covr_n_data_atoms() {
        let art = |id: i64, mime: &str, len: u64| ArtInput {
            art_id: id,
            mime: mime.into(),
            description: String::new(),
            picture_type: 3,
            width: 0,
            height: 0,
            data_len: len,
        };
        let arts = [art(1, "image/jpeg", 10), art(2, "image/png", 20)];
        let (segs, streamed) =
            build_udta(&[TagInput::new("title", "T")], &[], &arts).unwrap();
        assert_eq!(streamed, 30);

        // Exactly one covr atom, sized for both data atoms: 8 + Σ(16 + len).
        let prefix = materialize_udta(&segs);
        let covr_positions: Vec<usize> = prefix
            .windows(4)
            .enumerate()
            .filter_map(|(i, w)| (w == b"covr").then_some(i))
            .collect();
        assert_eq!(covr_positions.len(), 1);
        let cpos = covr_positions[0];
        let covr_size = u32::from_be_bytes(prefix[cpos - 4..cpos].try_into().unwrap());
        assert_eq!(covr_size, 8 + (16 + 10) + (16 + 20));

        // First data atom: jpeg (type 13), size 16+10; second: png (14), 16+20.
        let d1 = cpos + 4;
        assert_eq!(&prefix[d1 + 4..d1 + 8], b"data");
        assert_eq!(u32::from_be_bytes(prefix[d1..d1 + 4].try_into().unwrap()), 26);
        assert_eq!(
            u32::from_be_bytes(prefix[d1 + 8..d1 + 12].try_into().unwrap()),
            13
        );
        let d2 = d1 + 26;
        assert_eq!(&prefix[d2 + 4..d2 + 8], b"data");
        assert_eq!(u32::from_be_bytes(prefix[d2..d2 + 4].try_into().unwrap()), 36);
        assert_eq!(
            u32::from_be_bytes(prefix[d2 + 8..d2 + 12].try_into().unwrap()),
            14
        );

        // Streamed segments: one ArtImage per art, in input order.
        let art_segs: Vec<(i64, u64)> = segs
            .iter()
            .filter_map(|s| match s {
                Segment::ArtImage { art_id, len } => Some((*art_id, *len)),
                _ => None,
            })
            .collect();
        assert_eq!(art_segs, vec![(1, 10), (2, 20)]);
    }
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p musefs-format build_udta_multiple`
Expected: COMPILE ERROR — `build_udta` takes `Option<&ArtInput>`, not `&[ArtInput]`. That is this step's "failing test".

- [ ] **Step 3: Change `build_udta`'s signature and art block**

In `build_udta`, change the parameter `art: Option<&ArtInput>` to `arts: &[ArtInput]`, and replace the `if let Some(a) = art { … } else if !ilst_inline.is_empty() { … }` block with:

```rust
    if !arts.is_empty() {
        // One covr atom; each art is its own `data` child (the iTunes
        // convention for multiple artworks).
        let covr_size: u64 = 8 + arts.iter().map(|a| 16 + a.data_len).sum::<u64>();
        ilst_inline.extend_from_slice(&(covr_size as u32).to_be_bytes());
        ilst_inline.extend_from_slice(b"covr");
        for a in arts {
            let type_code: u32 = if a.mime == "image/png" { 14 } else { 13 };
            let data_size = 8 + 8 + a.data_len; // data header + type + locale + image
            ilst_inline.extend_from_slice(&(data_size as u32).to_be_bytes());
            ilst_inline.extend_from_slice(b"data");
            ilst_inline.extend_from_slice(&type_code.to_be_bytes());
            ilst_inline.extend_from_slice(&0u32.to_be_bytes()); // locale; image streams next
            ilst_segments.push(Segment::Inline(std::mem::take(&mut ilst_inline)));
            ilst_segments.push(Segment::ArtImage {
                art_id: a.art_id,
                len: a.data_len,
            });
            streamed_total += a.data_len;
        }
    } else if !ilst_inline.is_empty() {
        ilst_segments.push(Segment::Inline(std::mem::take(&mut ilst_inline)));
    }
```

Also update `build_udta`'s doc comment: "…and the cover image streamed from the DB" → "…and each cover image streamed from the DB (one `covr` atom, one `data` child per art)".

Note `build_udta` itself does NOT filter zero-length arts — the `build_udta_udta_size_exactly_u32_max_is_ok` test passes `art(0)` deliberately to derive framing overhead. Filtering stays in `synthesize_layout` (next step).

- [ ] **Step 4: Update `synthesize_layout`'s call**

In `synthesize_layout` (~line 811), replace:

```rust
    // Skip zero-byte art (an empty ArtImage segment fails layout validation).
    let art = arts.iter().find(|a| a.data_len > 0);
    let (udta_segments, _streamed_total) = build_udta(tags, binary_tags, art)?;
```

with:

```rust
    // Skip zero-byte art (an empty ArtImage segment fails layout validation).
    let arts: Vec<ArtInput> = arts.iter().filter(|a| a.data_len > 0).cloned().collect();
    let (udta_segments, _streamed_total) = build_udta(tags, binary_tags, &arts)?;
```

Also update the `synthesize_layout` doc comment: "Cover art and opaque `----` binary tags stream from the DB" stays true — no change needed there, but the sentence "Cover art" can become "Cover art (every non-empty art row, in input order)".

- [ ] **Step 5: Update existing `build_udta` test callsites**

Mechanical updates in the `tests` module (the assertions in these tests are unchanged):

| Test | Old call | New call |
|---|---|---|
| `build_udta_no_art_round_trips` | `build_udta(&tags, &[], None)` | `build_udta(&tags, &[], &[])` |
| `build_udta_with_art_reserves_size_without_image` | `build_udta(…, Some(&art))` | `build_udta(…, &[art])` |
| `build_udta_rejects_oversize_art` | `build_udta(…, Some(&art))` | `build_udta(…, &[art])` |
| `build_udta_groups_multi_value_text` | `build_udta(&tags, &[], None)` | `build_udta(&tags, &[], &[])` |
| `build_udta_png_art_uses_type_code_14` | `build_udta(…, Some(&art))` | `build_udta(…, &[art])` |
| `build_udta_art_box_sizes_are_exact` | `build_udta(…, Some(&art))` | `build_udta(…, &[art])` |
| `build_udta_udta_size_exactly_u32_max_is_ok` (3 calls) | `build_udta(…, Some(&art(N)))` | `build_udta(…, &[art(N)])` |

Search for any further `build_udta(` callsites with `grep -n "build_udta(" musefs-format/src/mp4.rs` and convert the same way.

- [ ] **Step 6: Add the synthesize-level multi-art test**

Add after `synthesize_new_moov_size_exactly_u32_max_is_ok` (~line 2180), reusing its `mk_mp4` setup pattern:

```rust
    #[test]
    fn synthesize_layout_emits_all_nonzero_arts() {
        // Zero-byte art is filtered; both non-empty arts stream, in input order.
        let art = |id: i64, len: u64| ArtInput {
            art_id: id,
            mime: "image/jpeg".into(),
            description: String::new(),
            picture_type: 3,
            width: 0,
            height: 0,
            data_len: len,
        };
        let buf = mk_mp4(true, b"AUDIO", &[0]);
        let scan = read_structure(&buf).unwrap();
        let layout = synthesize_layout(
            &scan,
            &[TagInput::new("title", "T")],
            &[],
            &[art(1, 5), art(2, 0), art(3, 7)],
        )
        .unwrap();
        let art_segs: Vec<(i64, u64)> = layout
            .segments()
            .iter()
            .filter_map(|s| match s {
                Segment::ArtImage { art_id, len } => Some((*art_id, *len)),
                _ => None,
            })
            .collect();
        assert_eq!(art_segs, vec![(1, 5), (3, 7)]);
    }
```

- [ ] **Step 7: Run the full crate test suite**

Run: `cargo test -p musefs-format`
Expected: PASS, including `build_udta_multiple_arts_one_covr_n_data_atoms` and `synthesize_layout_emits_all_nonzero_arts`.

Run: `cargo test -p musefs-core && cargo clippy --all-targets`
Expected: PASS / no new warnings (core calls only `synthesize_layout`, whose public signature is unchanged).

- [ ] **Step 8: Commit**

```bash
cargo fmt --all
git add musefs-format/src/mp4.rs
git commit -m "feat(format): synthesize every art row as a covr data atom in M4A"
```

---

### Task 3: Round-trip test (synthesis → `read_pictures`)

**Files:**
- Modify: `musefs-format/src/mp4.rs` (tests module)

- [ ] **Step 1: Write the test**

Add after `build_udta_multiple_arts_one_covr_n_data_atoms`:

```rust
    #[test]
    fn build_udta_two_arts_round_trips_through_read_pictures() {
        // materialize_udta zero-fills streamed payloads, so assert order +
        // mime only (mime derives from the inline type code, which survives).
        let art = |id: i64, mime: &str, len: u64| ArtInput {
            art_id: id,
            mime: mime.into(),
            description: String::new(),
            picture_type: 3,
            width: 0,
            height: 0,
            data_len: len,
        };
        let arts = [art(1, "image/jpeg", 5), art(2, "image/png", 9)];
        let (segs, _) =
            build_udta(&[TagInput::new("title", "Song")], &[], &arts).unwrap();
        let prefix = materialize_udta(&segs);
        let buf = [
            bx(b"ftyp", b"M4A "),
            bx(b"moov", &prefix),
            bx(b"mdat", b"A"),
        ]
        .concat();
        let pics = read_pictures(&buf);
        assert_eq!(pics.len(), 2);
        assert_eq!(pics[0].mime, "image/jpeg");
        assert_eq!(pics[0].data.len(), 5);
        assert_eq!(pics[1].mime, "image/png");
        assert_eq!(pics[1].data.len(), 9);
    }
```

- [ ] **Step 2: Run it**

Run: `cargo test -p musefs-format round_trips_through_read_pictures`
Expected: PASS (Tasks 1+2 already landed; this is integration coverage binding the two together — wrap pattern mirrors `build_udta_no_art_round_trips`).

- [ ] **Step 3: Commit**

```bash
cargo fmt --all
git add musefs-format/src/mp4.rs
git commit -m "test(format): M4A multi-art synthesis round-trips through read_pictures"
```

---

### Task 4: Generate arts in `proptest_mp4`

**Files:**
- Modify: `musefs-format/tests/proptest_mp4.rs:9-21` (`mp4_synthesis_preserves_audio`)

- [ ] **Step 1: Replace the hardcoded empty `arts`**

Replace the `mp4_synthesis_preserves_audio` property with:

```rust
    #[test]
    fn mp4_synthesis_preserves_audio(
        payload in proptest::collection::vec(any::<u8>(), 1..256),
        tags in proptest::collection::vec(("[A-Z]{1,12}", "[ -~]{0,40}"), 0..8),
        arts in proptest::collection::vec((1..3u8, 0..500u64), 0..3),
    ) {
        let file = fixtures::m4a(&payload);
        let scan = mp4::read_structure(&file).unwrap();
        let taginputs: Vec<TagInput> = tags.iter().map(|(k, v)| TagInput::new(k, v)).collect();
        // (kind, len) pairs: kind 1 = jpeg, 2 = png; len 0 exercises the
        // zero-byte filter in synthesize_layout.
        let arts: Vec<ArtInput> = arts
            .iter()
            .enumerate()
            .map(|(i, (kind, len))| ArtInput {
                art_id: i as i64 + 1,
                mime: if *kind == 1 { "image/jpeg".into() } else { "image/png".into() },
                description: String::new(),
                picture_type: 3,
                width: 0,
                height: 0,
                data_len: *len,
            })
            .collect();
        if let Ok(layout) = mp4::synthesize_layout(&scan, &taginputs, &[], &arts) {
            assert_backing_covers_audio(scan.mdat_payload_offset, scan.mdat_payload_len, &layout);
        }
    }
```

(The file's existing `use` line already imports `ArtInput`.)

- [ ] **Step 2: Run the property tests**

Run: `cargo test -p musefs-format --features fuzzing --test proptest_mp4`
Expected: PASS (128 cases per property).

- [ ] **Step 3: Commit**

```bash
cargo fmt --all
git add musefs-format/tests/proptest_mp4.rs
git commit -m "test(format): generate multi-art inputs in the mp4 audio-fidelity proptest"
```

---

### Task 5: Multi-cover fuzz fixture + seed + smoke build

**Files:**
- Modify: `musefs-format/src/fuzz_check.rs` (`fixtures::m4a`, ~line 117)
- Modify: `fuzz/src/bin/generate_seeds.rs` (mp4 seeds, ~line 24)

- [ ] **Step 1: Refactor `fixtures::m4a` and add `m4a_two_covers`**

In `musefs-format/src/fuzz_check.rs`, `fixtures` module: rename the body of `m4a` into a private parameterized builder and re-expose both. The byte output of `m4a(payload)` must be IDENTICAL to before (it's used by many tests):

```rust
    /// Minimal moov-first M4A (ported verbatim from tests/common::minimal_m4a).
    pub fn m4a(mdat_payload: &[u8]) -> Vec<u8> {
        m4a_with_extra_ilst(&[], mdat_payload)
    }

    /// `m4a` plus a `covr` atom holding two `data` children (jpeg + png) — the
    /// iTunes multiple-artwork convention. Seeds the fuzzers' multi-art read
    /// path; `m4a`'s byte output is unchanged.
    pub fn m4a_two_covers(mdat_payload: &[u8]) -> Vec<u8> {
        let covr = bx(
            b"covr",
            &[
                m4a_data_atom(13, &[0xFF, 0xD8, 0xFF, 0xE0, 1, 2, 3]),
                m4a_data_atom(14, &[0x89, b'P', b'N', b'G', 4, 5]),
            ]
            .concat(),
        );
        m4a_with_extra_ilst(&covr, mdat_payload)
    }

    fn m4a_with_extra_ilst(extra_ilst_atoms: &[u8], mdat_payload: &[u8]) -> Vec<u8> {
        let ilst_atoms = [
            bx(b"\xa9nam", &m4a_data_atom(1, b"Orig M4A")),
            bx(b"\xa9ART", &m4a_data_atom(1, b"Orig Artist")),
            extra_ilst_atoms.to_vec(),
        ]
        .concat();
        let ilst = bx(b"ilst", &ilst_atoms);
        let mut meta_hdlr = vec![0u8; 8];
        meta_hdlr.extend_from_slice(b"mdir");
        meta_hdlr.extend_from_slice(b"appl");
        meta_hdlr.extend_from_slice(&[0u8; 9]);
        let mut meta = vec![0u8; 4];
        meta.extend(bx(b"hdlr", &meta_hdlr));
        meta.extend(ilst);
        let udta = bx(b"udta", &bx(b"meta", &meta));
        let mut soun_hdlr = vec![0u8; 8];
        soun_hdlr.extend_from_slice(b"soun");
        soun_hdlr.extend_from_slice(&[0u8; 12]);
        let mut stco = vec![0u8; 4];
        stco.extend_from_slice(&1u32.to_be_bytes());
        stco.extend_from_slice(&0u32.to_be_bytes());
        let minf = bx(b"minf", &bx(b"stbl", &bx(b"stco", &stco)));
        let trak = bx(
            b"trak",
            &bx(b"mdia", &[bx(b"hdlr", &soun_hdlr), minf].concat()),
        );
        let moov = bx(b"moov", &[bx(b"mvhd", &[0u8; 8]), trak, udta].concat());
        let mut out = [bx(b"ftyp", b"M4A isom"), moov, bx(b"mdat", mdat_payload)].concat();
        // Point the single `stco` chunk offset at the real `mdat` payload start. A real
        // M4A's chunk offsets are absolute file positions; leaving it 0 means a retag
        // that shrinks the `moov` patches the offset below zero and synthesis fails
        // (TooLarge). With the true offset, the patched value lands at the new payload
        // position. The first `stco` occurrence is the box type (it precedes `mdat`).
        let mdat_payload_offset = (out.len() - mdat_payload.len()) as u32;
        let stco = out
            .windows(4)
            .position(|w| w == b"stco")
            .expect("stco present");
        let entry = stco + 4 + 4 + 4; // past "stco" type + version/flags + entry count
        out[entry..entry + 4].copy_from_slice(&mdat_payload_offset.to_be_bytes());
        out
    }
```

(The `m4a_with_extra_ilst` body past `ilst_atoms` is the original `m4a()` body verbatim — the comments included. The doc comment "ported verbatim from tests/common::minimal_m4a" stays on `m4a`.)

- [ ] **Step 2: Verify the refactor is byte-identical**

Run: `cargo test -p musefs-format && cargo test -p musefs-core`
Expected: PASS — every existing test that consumes `fixtures::m4a` still passes (any byte drift would break `read_structure`/offset assertions).

- [ ] **Step 3: Add the fuzz seed**

In `fuzz/src/bin/generate_seeds.rs`, after the `seed_binary` mp4 line:

```rust
    // Multi-art seed: a covr atom with two `data` children reaches the
    // read_pictures inner loop from the corpus, not only via mutation.
    write("mp4", "seed_two_covers", &fixtures::m4a_two_covers(&[9u8; 32]));
```

- [ ] **Step 4: Regenerate seeds and smoke-build the fuzz target**

```bash
cargo run --manifest-path fuzz/Cargo.toml --bin generate_seeds
cargo +nightly fuzz build mp4
```

Expected: seeds written under `fuzz/corpus/`; fuzz target builds cleanly (the fuzz crate is outside the workspace, so workspace builds don't cover it).

- [ ] **Step 5: Commit**

```bash
cargo fmt --all
git add musefs-format/src/fuzz_check.rs fuzz/src/bin/generate_seeds.rs fuzz/corpus/mp4/seed_two_covers
git commit -m "test(fuzz): add a two-cover M4A fixture and mp4 corpus seed"
```

(The corpus seeds are git-tracked — `fuzz/corpus/mp4/seed0` etc. are committed — so the new seed file is staged by name.)

---

### Task 6: Interop Property 5 — mutagen reads both covers

**Files:**
- Modify: `musefs-core/tests/interop_emit.rs` (`ManifestRow` ~line 110, `emit` ~line 140, manifest rows + serialization ~lines 210–445)
- Modify: `tests/interop/test_mutagen_roundtrip.py`

- [ ] **Step 1: Seed art through `emit`**

Cover art here must come from the DB, not the fixture bytes — `emit()` hand-seeds the store and never scans. Changes in `interop_emit.rs`:

1. Extend the imports: `use musefs_db::{BinaryTag, Db, Format, NewArt, NewTrack, Tag, TrackArt};`
2. Add constants near the top (after the helpers):

```rust
// Mirrored byte-for-byte in tests/interop/test_mutagen_roundtrip.py
// (COVR_JPEG / COVR_PNG): mutagen must read these exact images back.
const COVR_JPEG: &[u8] = b"\xFF\xD8\xFF\xE0interop-jpeg-cover";
const COVR_PNG: &[u8] = b"\x89PNG\r\n\x1a\ninterop-png-cover";
```

3. Add a final parameter to `emit` — `arts: &[(&[u8], &str)]` (data, mime) — and after the `db.replace_tags(…)` call insert:

```rust
    let links: Vec<TrackArt> = arts
        .iter()
        .enumerate()
        .map(|(i, (data, mime))| {
            let art_id = db
                .upsert_art(&NewArt {
                    mime: (*mime).to_string(),
                    width: None,
                    height: None,
                    data: data.to_vec(),
                })
                .unwrap();
            TrackArt {
                art_id,
                picture_type: 3,
                description: String::new(),
                ordinal: i as i64,
            }
        })
        .collect();
    if !links.is_empty() {
        db.set_track_art(id, &links).unwrap();
    }
```

4. Update every `emit(` callsite: all formats pass `&[]` EXCEPT the MP4 block, which passes `&[(COVR_JPEG, "image/jpeg"), (COVR_PNG, "image/png")]`. Find them with `grep -n "= emit(" musefs-core/tests/interop_emit.rs`.

- [ ] **Step 2: Record the cover count in the manifest**

1. Add `covr_count: usize` to `ManifestRow`.
2. Add `covr_count: 0` to every row literal except the MP4 row, which gets `covr_count: 2`.
3. In the manifest serialization `format!` (~line 428), append `,\"covr_count\":{covr_count}` before the closing `}}` and add `covr_count = row.covr_count,` to the bindings.

- [ ] **Step 3: Add the mutagen-side test**

In `tests/interop/test_mutagen_roundtrip.py`, add after the imports:

```python
# Mirrored byte-for-byte from musefs-core/tests/interop_emit.rs (COVR_JPEG/COVR_PNG).
COVR_JPEG = b"\xff\xd8\xff\xe0interop-jpeg-cover"
COVR_PNG = b"\x89PNG\r\n\x1a\ninterop-png-cover"
```

and at the end of the file:

```python
def test_m4a_multi_cover_art():
    """Every track_art row is served as a covr `data` atom (iTunes convention);
    mutagen reads them back in ordinal order, bytes intact."""
    base = os.environ["MUSEFS_INTEROP_DIR"]
    with open(os.path.join(base, "manifest.json")) as fh:
        manifest = json.load(fh)
    rows = [r for r in manifest if r.get("covr_count")]
    assert rows, "no manifest row declares cover art"
    for row in rows:
        f = mutagen.mp4.MP4(os.path.join(base, row["file"]))
        covr = f.tags.get("covr") if f.tags else None
        assert covr is not None, f"{row['file']}: no covr tag"
        assert len(covr) == row["covr_count"]
        assert bytes(covr[0]) == COVR_JPEG
        assert covr[0].imageformat == mutagen.mp4.MP4Cover.FORMAT_JPEG
        assert bytes(covr[1]) == COVR_PNG
        assert covr[1].imageformat == mutagen.mp4.MP4Cover.FORMAT_PNG
```

- [ ] **Step 4: Run the interop pass end to end**

```bash
MUSEFS_INTEROP_DIR=/tmp/i cargo test -p musefs-core --test interop_emit -- --ignored emit_interop_fixtures
MUSEFS_INTEROP_DIR=/tmp/i python -m pytest tests/interop
```

Expected: emit writes fixtures + manifest; all interop tests PASS including `test_m4a_multi_cover_art` (mutagen is the independent reader confirming the wire format).

- [ ] **Step 5: Commit**

```bash
cargo fmt --all
git add musefs-core/tests/interop_emit.rs tests/interop/test_mutagen_roundtrip.py
git commit -m "test(interop): assert mutagen reads both synthesized M4A covers"
```

---

### Task 7: `musefs_common` — `ArtImage`, list-shaped `Record.art`, multi-row `replace_track_art`

**Files:**
- Modify: `contrib/python-musefs/src/musefs_common/sync.py`
- Modify: `contrib/python-musefs/src/musefs_common/store.py` (`replace_track_art`, ~line 107)
- Modify: `contrib/python-musefs/src/musefs_common/__init__.py`
- Modify: `contrib/python-musefs/tests/test_sync.py`, `tests/test_store_art.py`, `tests/test_public_api.py`

All commands in this task run from `contrib/python-musefs/`.

- [ ] **Step 1: Write the failing tests**

In `tests/test_store_art.py`, add after `test_replace_track_art_sets_and_replaces_front_cover`:

```python
def test_replace_track_art_multiple_rows_ordered(db_path):
    conn = connect(db_path)
    try:
        tid = insert_track(conn, "/m/a.flac")
        a = upsert_art(conn, JPEG, "image/jpeg")
        b = upsert_art(conn, PNG, "image/png")
        replace_track_art(conn, tid, [(a, 3, ""), (b, 4, "back")])
        conn.commit()
        rows = conn.execute(
            "SELECT art_id, picture_type, description, ordinal FROM track_art "
            "WHERE track_id=? ORDER BY ordinal",
            (tid,),
        ).fetchall()
        assert rows == [(a, 3, "", 0), (b, 4, "back", 1)]
        replace_track_art(conn, tid, [(b, 3, "")])
        conn.commit()
        rows = conn.execute(
            "SELECT art_id FROM track_art WHERE track_id=?", (tid,)
        ).fetchall()
        assert rows == [(b,)]
    finally:
        conn.close()
```

(`test_store_art.py` already imports `connect`, `replace_track_art`, `upsert_art` and defines `PNG`; `JPEG`/`insert_track` come from conftest — check its imports and extend if needed.)

In `tests/test_sync.py`, add `ArtImage` to the `musefs_common` import line, add a local `PNG = b"\x89PNG\r\n\x1a\n" + b"\x00" * 16`, and add:

```python
def test_sync_one_multiple_images_written_in_order(db_path):
    conn, tid = _seed(db_path)
    try:
        stats = SyncStats()
        sync_one(
            conn,
            Record(
                key="/m/a.flac",
                pairs=[],
                art=[ArtImage(JPEG, "image/jpeg"), ArtImage(PNG, "image/png", 4, "back")],
            ),
            stats,
        )
        conn.commit()
        assert stats.art_linked == 1  # track count, not image count
        rows = conn.execute(
            "SELECT picture_type, description, ordinal FROM track_art "
            "WHERE track_id=? ORDER BY ordinal",
            (tid,),
        ).fetchall()
        assert rows == [(3, "", 0), (4, "back", 1)]
    finally:
        conn.close()


def test_sync_one_per_image_cap_keeps_survivors(db_path):
    conn, tid = _seed(db_path)
    try:
        big = b"x" * (MAX_ART_BYTES + 1)
        stats = SyncStats()
        sync_one(
            conn,
            Record(
                key="/m/a.flac",
                pairs=[],
                art=[ArtImage(big, "image/jpeg"), ArtImage(JPEG, "image/jpeg")],
            ),
            stats,
        )
        conn.commit()
        assert stats.skipped_art == 1
        assert stats.art_linked == 1
        rows = conn.execute(
            "SELECT ordinal FROM track_art WHERE track_id=?", (tid,)
        ).fetchall()
        assert rows == [(0,)]  # only the survivor, ordinals re-packed from 0
    finally:
        conn.close()


def test_sync_one_all_images_over_cap_leaves_existing_art(db_path):
    conn, tid = _seed(db_path)
    try:
        conn.execute(
            "INSERT INTO art (sha256, mime, byte_len, data) VALUES "
            "('deadbeef', 'image/jpeg', 3, X'aabbcc')"
        )
        art_id = conn.execute("SELECT id FROM art WHERE sha256='deadbeef'").fetchone()[0]
        conn.execute(
            "INSERT INTO track_art (track_id, art_id) VALUES (?, ?)", (tid, art_id)
        )
        conn.commit()
        big = b"x" * (MAX_ART_BYTES + 1)
        stats = SyncStats()
        sync_one(
            conn,
            Record(
                key="/m/a.flac",
                pairs=[],
                art=[ArtImage(big, "image/jpeg"), ArtImage(big + b"y", "image/png")],
            ),
            stats,
        )
        conn.commit()
        assert stats.skipped_art == 2
        assert stats.art_linked == 0
        row = conn.execute(
            "SELECT art_id FROM track_art WHERE track_id=?", (tid,)
        ).fetchone()
        assert row == (art_id,)  # scan-seeded art untouched
    finally:
        conn.close()
```

- [ ] **Step 2: Run to verify failure**

Run: `python -m pytest tests/test_store_art.py tests/test_sync.py -x`
Expected: FAIL — `ImportError: cannot import name 'ArtImage'` (or the multi-row store test fails on the old single-`art_id` signature).

- [ ] **Step 3: Implement `store.replace_track_art`**

Replace the function in `src/musefs_common/store.py`:

```python
def replace_track_art(conn, track_id, arts):
    """Replace the track's art rows. ``arts`` is an ordered list of
    ``(art_id, picture_type, description)``; each row's ``ordinal`` is its
    list index."""
    conn.execute("DELETE FROM track_art WHERE track_id = ?", (track_id,))
    conn.executemany(
        "INSERT INTO track_art (track_id, art_id, picture_type, description, "
        "ordinal) VALUES (?, ?, ?, ?, ?)",
        [
            (track_id, art_id, picture_type, description, i)
            for i, (art_id, picture_type, description) in enumerate(arts)
        ],
    )
```

- [ ] **Step 4: Implement `ArtImage` and the new `sync_one`**

In `src/musefs_common/sync.py`:

```python
@dataclass(frozen=True)
class ArtImage:
    """One embedded picture to sync: raw bytes, mime, ID3/FLAC picture type
    (3 = front cover), and free-text description."""

    data: bytes
    mime: str
    picture_type: int = 3
    description: str = ""


@dataclass
class Record:
    """One file's sync inputs: the realpath key, the (key, value) tag pairs, and
    pre-resolved art as a list of ``ArtImage``s (``None``/empty list = no art
    from the host)."""

    key: str
    pairs: list = field(default_factory=list)
    art: object = None  # list[ArtImage] | None


@dataclass
class SyncStats:
    synced: int = 0
    skipped: int = 0  # path had no matching track row
    art_linked: int = 0  # tracks that had at least one art row written
    skipped_art: int = 0  # images over the size cap (or, in the beets adapter, unreadable)

    def summary(self):
        return (
            f"synced={self.synced} skipped={self.skipped} "
            f"art_linked={self.art_linked} skipped_art={self.skipped_art}"
        )


def sync_one(conn, record, stats, *, dry_run=False):
    """Sync one ``Record`` into the DB, mutating ``stats``. Caller owns the
    transaction. Tags are always fully replaced (scanner-written binary tags
    survive — see ``replace_tags``). Art is replaced when at least one image is
    within ``MAX_ART_BYTES``; each over-cap image bumps ``skipped_art``, and if
    every provided image is over cap any scan-seeded ``track_art`` is left
    untouched."""
    track_id = track_id_for_path(conn, record.key)
    if track_id is None:
        stats.skipped += 1
        return

    kept = []
    for img in record.art or []:
        if len(img.data) > MAX_ART_BYTES:
            stats.skipped_art += 1
        else:
            kept.append(img)
    will_link_art = bool(kept)

    if not dry_run:
        replace_tags(conn, track_id, record.pairs)
        if will_link_art:
            arts = [
                (upsert_art(conn, img.data, img.mime), img.picture_type, img.description)
                for img in kept
            ]
            replace_track_art(conn, track_id, arts)

    if will_link_art:
        stats.art_linked += 1
    stats.synced += 1
```

(`sync_files` is unchanged.)

- [ ] **Step 5: Export `ArtImage`**

In `src/musefs_common/__init__.py`: change the sync import line to
`from .sync import ArtImage, Record, SyncStats, sync_files, sync_one` and add `"ArtImage",` to `__all__` (next to `"Record"`).

In `tests/test_public_api.py`: add `"ArtImage",` to the `expected` set.

- [ ] **Step 6: Update the existing test callsites**

In `tests/test_sync.py`, convert every tuple-shaped art:
- `art=(JPEG, "image/jpeg")` → `art=[ArtImage(JPEG, "image/jpeg")]` (in `test_sync_one_writes_tags_and_art` and `test_sync_one_dry_run_counts_without_writing`)
- in `test_sync_one_over_cap_art_skipped_not_linked`: `art=(big, "image/jpeg")` → `art=[ArtImage(big, "image/jpeg")]`

- [ ] **Step 7: Run the suite**

Run: `python -m pytest && ruff check . && ruff format --check .`
Expected: all PASS.

- [ ] **Step 8: Commit**

```bash
git add src/musefs_common/sync.py src/musefs_common/store.py src/musefs_common/__init__.py tests/test_sync.py tests/test_store_art.py tests/test_public_api.py
git commit -m "feat(python-musefs): multi-image art sync (ArtImage list, per-image cap)"
```

---

### Task 8: Picard — `images()` replaces `front_cover`

**Files:**
- Modify: `contrib/picard/musefs/_core.py` (`front_cover`, ~line 96)
- Modify: `contrib/picard/musefs/__init__.py` (import ~line 30, callsite ~line 144)
- Modify: `contrib/picard/tests/conftest.py` (`FakeImage`)
- Rename+rewrite: `contrib/picard/tests/test_front_cover.py` → `tests/test_images.py`
- Modify: `contrib/picard/tests/test_sync_roundtrip.py`
- Regenerate: `contrib/picard/musefs/_common/` (vendored copy)

All commands run from the repo root unless noted.

- [ ] **Step 1: Re-vendor the shared lib**

```bash
python contrib/python-musefs/vendor_to_picard.py
```

Expected: `contrib/picard/musefs/_common/sync.py` now contains `ArtImage`. (The drift-guard test `tests/test_vendor_sync.py` enforces this.)

- [ ] **Step 2: Extend `FakeImage` in conftest**

In `contrib/picard/tests/conftest.py`:

```python
class FakeImage:
    """Stand-in for a Picard CoverArtImage: is_front_image() + data + mimetype
    (+ optional maintype / comment / can_be_saved_to_tags)."""

    def __init__(
        self,
        data,
        mimetype,
        front=True,
        maintype=None,
        comment="",
        can_be_saved_to_tags=True,
    ):
        self.data = data
        self.mimetype = mimetype
        self._front = front
        if maintype is not None:
            self.maintype = maintype  # absent attribute exercises the fallback
        self.comment = comment
        self.can_be_saved_to_tags = can_be_saved_to_tags

    def is_front_image(self):
        return self._front
```

- [ ] **Step 3: Write the failing tests**

`git mv contrib/picard/tests/test_front_cover.py contrib/picard/tests/test_images.py` and replace its contents:

```python
from musefs._common.sync import ArtImage
from musefs._core import images


def test_no_images_returns_empty_list(fake_metadata):
    assert images(fake_metadata()) == []


def test_all_images_returned_in_order_with_types(fake_metadata, fake_image):
    front = fake_image(b"FRONT", "image/jpeg", front=True, maintype="front")
    back = fake_image(b"BACK", "image/png", front=False, maintype="back")
    assert images(fake_metadata(images=[front, back])) == [
        ArtImage(b"FRONT", "image/jpeg", 3, ""),
        ArtImage(b"BACK", "image/png", 4, ""),
    ]


def test_unknown_maintype_front_image_maps_to_3(fake_metadata, fake_image):
    img = fake_image(b"X", "image/jpeg", front=True, maintype="obi")
    assert images(fake_metadata(images=[img]))[0].picture_type == 3


def test_unknown_maintype_non_front_maps_to_0(fake_metadata, fake_image):
    img = fake_image(b"X", "image/jpeg", front=False, maintype="obi")
    assert images(fake_metadata(images=[img]))[0].picture_type == 0


def test_missing_maintype_falls_back_to_front_detection(fake_metadata, fake_image):
    img = fake_image(b"X", "image/jpeg", front=True)  # no maintype attribute
    assert images(fake_metadata(images=[img]))[0].picture_type == 3


def test_unsavable_image_skipped(fake_metadata, fake_image):
    hidden = fake_image(b"X", "image/jpeg", can_be_saved_to_tags=False)
    keep = fake_image(b"Y", "image/png", maintype="front")
    out = images(fake_metadata(images=[hidden, keep]))
    assert [i.data for i in out] == [b"Y"]


def test_comment_becomes_description(fake_metadata, fake_image):
    img = fake_image(b"X", "image/jpeg", maintype="booklet", comment="page 1")
    a = images(fake_metadata(images=[img]))[0]
    assert a.picture_type == 5
    assert a.description == "page 1"
```

- [ ] **Step 4: Run to verify failure**

Run: `cd contrib/picard && python -m pytest tests/test_images.py`
Expected: FAIL — `ImportError: cannot import name 'images'`.

- [ ] **Step 5: Implement `images()` in `_core.py`**

In `contrib/picard/musefs/_core.py`, add near the top (after the existing imports):

```python
from ._common.sync import ArtImage
```

and replace `front_cover` with:

```python
# Picard maintype → ID3 picture type (mirrors Picard's own ID3 image-type
# map). An unrecognized maintype falls back to front-image detection (the more
# reliable signal), then to 0 (Other).
_ID3_PICTURE_TYPES = {
    "front": 3,
    "back": 4,
    "booklet": 5,
    "medium": 6,
}


def _picture_type(img):
    maintype = getattr(img, "maintype", None)
    if maintype in _ID3_PICTURE_TYPES:
        return _ID3_PICTURE_TYPES[maintype]
    is_front = getattr(img, "is_front_image", None)
    if is_front is not None and is_front():
        return 3
    return 0


def images(metadata):
    """Return an ``ArtImage`` per syncable image in a Picard Metadata, in
    Picard order. Duck-typed: images expose ``data`` and ``mimetype``, and
    optionally ``maintype``, ``comment``, ``can_be_saved_to_tags``, and
    ``is_front_image()``."""
    out = []
    for img in getattr(metadata, "images", None) or []:
        if not getattr(img, "can_be_saved_to_tags", True):
            continue
        out.append(
            ArtImage(
                data=img.data,
                mime=img.mimetype,
                picture_type=_picture_type(img),
                description=getattr(img, "comment", "") or "",
            )
        )
    return out
```

Also update `_core.py`'s module docstring: "extracts the front cover" → "extracts the cover images".

- [ ] **Step 6: Update the plugin entry point**

In `contrib/picard/musefs/__init__.py`:
- import line: `front_cover,` → `images,` (keep the alphabetical-ish order of the existing import block)
- callsite (~line 144): `art = front_cover(f.metadata)` → `art = images(f.metadata)`
- module docstring: "tags + front cover" → "tags + cover images"

- [ ] **Step 7: Extend the roundtrip test**

In `contrib/picard/tests/test_sync_roundtrip.py`, in `test_do_sync_writes_tags_and_art`, change the metadata construction and art assertion:

```python
    PNG = b"\x89PNG\r\n\x1a\n" + b"\x00" * 16
    meta = fake_metadata(
        images=[
            fake_image(JPEG, "image/jpeg"),
            fake_image(PNG, "image/png", front=False, maintype="back"),
        ],
        title="Song",
        artist="Band",
    )
```

and replace the `COUNT(*) == 1` assertion with:

```python
        rows = conn.execute(
            "SELECT picture_type, ordinal FROM track_art WHERE track_id=? ORDER BY ordinal",
            (tid,),
        ).fetchall()
        assert rows == [(3, 0), (4, 1)]
```

- [ ] **Step 8: Run the Picard suite**

Run: `cd contrib/picard && python -m pytest tests`
Expected: PASS (including `test_vendor_sync.py`; Qt-fixture tests skip without pytest-qt; `test_sync_roundtrip.py` skips if Picard isn't importable — also run the system-package variant from the project memory if Picard coverage is wanted now: `/usr/bin/python3 -m pytest tests` with `PYTHONPATH` pointing at `/usr/lib/picard` and dist-packages PyQt5).

- [ ] **Step 9: Commit**

```bash
git add contrib/picard/musefs/_core.py contrib/picard/musefs/__init__.py contrib/picard/musefs/_common contrib/picard/tests/conftest.py contrib/picard/tests/test_images.py contrib/picard/tests/test_sync_roundtrip.py
git commit -m "feat(picard): sync every cover image with mapped picture types"
```

(`git mv` already staged the rename; `git add` on the new path covers the rewrite.)

---

### Task 9: beets — adapt to the `ArtImage` list shape

**Files:**
- Modify: `contrib/beets/beetsplug/_core.py` (imports ~line 10, `_read_album_art` docstring ~line 113, `build_records` ~line 142)
- Modify: `contrib/beets/tests/test_build_records.py`

All test commands run from `contrib/beets/` using the venv (`./.venv/bin/python`; the editable installs pick up the Task 7 lib changes automatically).

- [ ] **Step 1: Update the failing test**

In `tests/test_build_records.py`, `test_build_records_reads_album_art`, replace the art assertions:

```python
    assert records[0].art is not None
    (img,) = records[0].art
    assert img.mime == "image/jpeg"
    assert img.picture_type == 3
    assert img.description == ""
    assert stats.skipped_art == 0
```

- [ ] **Step 2: Run to verify failure**

Run: `./.venv/bin/python -m pytest tests/test_build_records.py`
Expected: FAIL — `records[0].art` is still a `(data, mime)` tuple, so unpacking `(img,)` fails.

- [ ] **Step 3: Implement**

In `contrib/beets/beetsplug/_core.py`:
- import line: `from musefs_common import MAX_ART_BYTES, ArtImage, Record, realpath_key, sniff_mime`
- in `build_records`, wrap the cover at the `Record` construction site:

```python
    records = []
    art_cache = {}
    for item in items:
        cover = _read_album_art(item, art_cache, stats)
        records.append(
            Record(
                key=realpath_key(item.path),
                pairs=map_fields(item, fields),
                art=[ArtImage(*cover)] if cover else None,
            )
        )
    return records
```

- append to `_read_album_art`'s docstring: `Also size-capped here (not only in sync_one) so a shared over-cap cover is counted once per distinct file — the double enforcement is intentional, not dead code.`

- [ ] **Step 4: Run the beets suite**

Run: `./.venv/bin/python -m pytest tests`
Expected: PASS (the e2e tests assert mount-level art sha256s and are unaffected; FUSE-dependent ones skip where applicable).

- [ ] **Step 5: Commit**

```bash
git add contrib/beets/beetsplug/_core.py contrib/beets/tests/test_build_records.py
git commit -m "feat(beets): adapt art sync to the ArtImage list shape"
```

---

### Task 10: Full verification + mutation gate

**Files:** none (verification only)

- [ ] **Step 1: Rust gates**

```bash
cargo fmt --all --check
cargo clippy --all-targets
cargo test
cargo test -p musefs-format --features fuzzing
```

Expected: all clean/PASS.

- [ ] **Step 2: Python suites**

```bash
(cd contrib/python-musefs && python -m pytest && ruff check . && ruff format --check .)
(cd contrib/beets && ./.venv/bin/python -m pytest tests)
(cd contrib/picard && python -m pytest tests)
```

Expected: all PASS (Qt-dependent Picard tests may skip).

- [ ] **Step 3: Interop pass**

```bash
MUSEFS_INTEROP_DIR=/tmp/i cargo test -p musefs-core --test interop_emit -- --ignored emit_interop_fixtures
MUSEFS_INTEROP_DIR=/tmp/i python -m pytest tests/interop
```

Expected: PASS.

- [ ] **Step 4: In-diff mutation gate (CI parity)**

```bash
git diff "$(git merge-base main HEAD)...HEAD" -- '*.rs' > mutants.diff
grep -q '^@@ ' mutants.diff   # MUST succeed — an empty diff is a silent false pass
cargo mutants --in-diff mutants.diff -j2 --exclude 'musefs-latencyfs/**' --output /tmp/mutants-out/in-diff
```

Expected: `grep` exits 0; cargo-mutants reports no missed mutants. If mutants survive, add targeted tests (the size-arithmetic assertions in Task 2's test are designed to pin the `covr_size`/`data_size` arithmetic; survivors will most likely be in `read_pictures`' kind filters — extend Task 1's tests accordingly).

- [ ] **Step 5: Fuzz smoke (already built in Task 5 — rerun to be sure)**

```bash
cargo +nightly fuzz build mp4
```

Expected: builds cleanly.
