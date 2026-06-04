# Binary Tags Phase 3 — MP4 `----` Opaque Passthrough Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make custom MP4 `----` (freeform) atoms carrying non-UTF-8 binary data survive the scan → store → synthesize → read round trip, payload-byte-identical, exactly as ID3 binary frames already do (Phase 2).

**Architecture:** Add an MP4 binary-tag parser (`mp4::read_binary_tags`) mirroring `mp3::read_binary_tags`, wire it into the two scan probe paths, and refactor MP4 synthesis so `build_udta` returns an ordered segment list (inline framing interleaved with streamed `Segment::BinaryTag`/`Segment::ArtImage`) instead of a materialized prefix. Binary `----` payloads stream from the DB at read time (never held in memory), and every enclosing box size (`----`/`ilst`/`meta`/`udta`/`moov`) accounts for them. The Phase-1/2 foundation (schema V2, `Segment::BinaryTag`, `read_binary_tag_chunk`, `BinaryTagInput`, the shared scan size-cap + persistence, the facade open-handle guard) is already merged and is format-agnostic, so no DB, reader-cache, or facade changes are needed beyond passing the binary inputs through.

**Tech Stack:** Rust workspace (`musefs-format`, `musefs-core`), SQLite via `rusqlite`, `proptest` (gated behind the `fuzzing` feature), `cargo-fuzz` (nightly).

---

## Background facts the implementer must know

These were verified in the current tree; do not re-derive them.

- **MP4 only flows through `probe_full` and `probe_file`** — it is *not* in `probe_prefix` (front-anchored formats only). `probe_full` reads the whole file; `probe_file` uses the M4A seek reader (`read_structure_from`) and passes `&scan.moov` to the read functions. `ilst_region` works on both a full-file buffer and a moov-only buffer (today's `read_tags`/`read_pictures` already rely on this in both arms).
- **The scan ingest path already caps and persists `Probed.binary_tags` generically.** `ingest`/`ingest_bulk` (`musefs-core/src/scan.rs:396-407`, `:466-477`) filter `Probed.binary_tags` by `!payload.is_empty() && payload.len() <= MAX_BINARY_TAG_BYTES` and call `db.set_binary_tags`. Populating `Probed.binary_tags` for MP4 is therefore the *only* scan change required — the cap and DB write come for free.
- **The reader already computes `binary_tag_inputs`.** `HeaderCache::build` (`musefs-core/src/reader.rs`) calls `crate::mapping::binary_tags_to_inputs(db, track.id)?` once before the `match track.format`. The MP4 arm just needs to pass it.
- **`cache_bytes` and the facade open-handle guard are format-agnostic.** `cache_bytes` sums only `Segment::Inline` lengths (`BinaryTag` → `_ => 0`). `layout.has_binary_tag()` already drives the facade transactional `content_version` guard for *any* layout that contains a `BinaryTag` segment. MP4 layouts gaining `BinaryTag` segments are covered automatically — **no facade.rs change**.
- **No promotion for MP4.** Unlike ID3 (`POPM`/`UFID`), MP4 has no in-scope semantic promotion. `mp4::read_binary_tags` returns opaque `EmbeddedBinaryTag`s only (a `Vec`, not a `(opaque, promoted)` tuple).
- **`Segment::len(&self) -> u64` is `pub`** (`musefs-format/src/layout.rs:46`). `RegionLayout::validate` rejects any zero-length non-audio segment (`EmptySegment`), so empty binary payloads must be skipped during synthesis (the Phase-2 code already does this for ID3).
- **Key format:** opaque MP4 keys are `----:<mean>:<name>` (raw, *not* folded through the vocabulary — folding is for text freeform only). `<name>` may itself contain `:`; parse by stripping `----:` then splitting on the **first** remaining `:`.
- **Type code is not preserved.** The stored payload is the `data` value bytes only (after the 8-byte `[type:u32][locale:u32]` header). Synthesis re-emits a binary `data` box with type code `0` (binary/implicit). Re-parse classifies type `!= 1` as opaque, so the payload round-trips byte-identically even though the original type code does not. This is the spec's payload-exact (not byte-identical-framing) fidelity contract.

## File map (what each touched file is responsible for)

- **Create:** none.
- `musefs-format/src/mp4.rs` — add `read_binary_tags` (parse) + `parse_freeform_key`/`freeform_binary_prefix` helpers; refactor `build_udta` (→ segment list) and `synthesize_layout` (signature + binary interleaving); update its in-file `tests` module.
- `musefs-core/src/scan.rs` — populate `Probed.binary_tags` in the MP4 arms of `probe_full` and `probe_file`; add a scan ingest test.
- `musefs-core/src/reader.rs` — pass `&binary_tag_inputs` to `mp4::synthesize_layout`.
- `musefs-format/tests/proptest_mp4.rs` — fix the existing signature call; add the binary round-trip property test.
- `musefs-format/tests/mp4_oracle.rs` — fix the four `synthesize_layout` calls (signature).
- `fuzz/fuzz_targets/mp4.rs` + `fuzz/src/lib.rs` — keep the fuzz target compiling (Task 2), then add real binary-tag fuzzing + a seed (Task 5).

---

## Task 1: MP4 binary-tag parser (`mp4::read_binary_tags`)

Add the scan-time extractor. Pure addition — no signature changes, compiles green on its own.

**Files:**
- Modify: `musefs-format/src/mp4.rs` (new public fn near `read_tags`/`read_pictures`; new test in the `tests` module)

- [ ] **Step 1: Write the failing test**

Add to the `tests` module in `musefs-format/src/mp4.rs` (place it after `read_pictures`-related tests). It builds a minimal moov whose `ilst` holds one binary `----` atom (type 0) and one text `----` atom (type 1), and asserts only the binary one is returned, keyed and payload-exact.

```rust
    /// Build a `----` freeform atom with an explicit data `type_code` and raw value.
    fn freeform_atom_typed(mean: &str, name: &str, type_code: u32, value: &[u8]) -> Vec<u8> {
        let mut mean_body = 0u32.to_be_bytes().to_vec();
        mean_body.extend_from_slice(mean.as_bytes());
        let mut name_body = 0u32.to_be_bytes().to_vec();
        name_body.extend_from_slice(name.as_bytes());
        let mut data_body = type_code.to_be_bytes().to_vec();
        data_body.extend_from_slice(&0u32.to_be_bytes()); // locale
        data_body.extend_from_slice(value);
        let mut inner = boxed(b"mean", &mean_body);
        inner.extend(boxed(b"name", &name_body));
        inner.extend(boxed(b"data", &data_body));
        boxed(b"----", &inner)
    }

    /// Wrap an `ilst` body in the moov/udta/meta/ilst boxes `ilst_region` expects.
    fn moov_with_ilst(ilst_body: &[u8]) -> Vec<u8> {
        let ilst = boxed(b"ilst", ilst_body);
        let mut meta = 0u32.to_be_bytes().to_vec(); // FullBox version/flags
        meta.extend(boxed(b"hdlr", &[0u8; 25]));
        meta.extend_from_slice(&ilst);
        let udta = boxed(b"udta", &boxed(b"meta", &meta));
        boxed(b"moov", &udta)
    }

    #[test]
    fn read_binary_tags_extracts_opaque_freeform_skips_text() {
        let serato = vec![0x00, 0xff, 0x10, 0x42, 0x99];
        let binary = freeform_atom_typed("com.serato.dj", "analysis", 0, &serato);
        let text = freeform_atom_typed("com.apple.iTunes", "MOOD", 1, b"calm");
        let moov = moov_with_ilst(&[binary, text].concat());

        let tags = read_binary_tags(&moov);
        assert_eq!(tags.len(), 1, "only the binary `----` is opaque");
        assert_eq!(tags[0].key, "----:com.serato.dj:analysis");
        assert_eq!(tags[0].payload, serato);

        // The text `----` is the text path's job, never opaque.
        assert!(read_binary_tags(&moov)
            .iter()
            .all(|t| t.key != "----:com.apple.iTunes:MOOD"));
    }
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p musefs-format read_binary_tags_extracts_opaque_freeform_skips_text`
Expected: FAIL — `read_binary_tags` is not defined (`cannot find function`).

- [ ] **Step 3: Implement `read_binary_tags`**

Insert after `read_pictures` (`musefs-format/src/mp4.rs`, immediately before `boxed`). Uses the existing `ilst_region`/`child_boxes`/`find_box`/`payload` helpers and `EmbeddedBinaryTag` (imported in Task 1 Step 3 above).

**Return type — note the deliberate difference from ID3.** `mp3::read_binary_tags`/`wav::read_binary_tags` return a tuple `(Vec<EmbeddedBinaryTag>, Vec<(String, String)>)` because ID3 promotes `POPM`/`UFID` to text. **MP4 has no promotion in scope**, so this function returns a **bare `Vec<EmbeddedBinaryTag>`**. Do not "align" it to a tuple — the bare-`Vec` shape is what the scan wiring (Task 3) and the tests assume. It also emits the raw `----:<mean>:<name>` key **without** folding `(mean, name)` through `tagmap::mp4_freeform_to_key` (folding is for the *text* path only; opaque keys must stay verbatim so synthesis rebuilds the exact `mean`/`name` atoms).

```rust
/// Extract opaque (non-text) MP4 `----` freeform atoms for binary-tag passthrough.
/// One `EmbeddedBinaryTag` per `----` atom whose first `data` sub-box is
/// binary-typed (type code != 1): key `----:<mean>:<name>`, payload the `data`
/// value bytes (after the 8-byte `[type][locale]` header). Text freeform atoms
/// (type 1) are handled by `read_tags`, so the two paths never double-store.
/// Lenient: malformed atoms are skipped. Only the first `data` sub-box is read
/// (multi-value freeform is rare; mirrors `read_freeform`).
pub fn read_binary_tags(buf: &[u8]) -> Vec<EmbeddedBinaryTag> {
    let Some((start, len)) = ilst_region(buf) else {
        return Vec::new();
    };
    let ilst = &buf[start..start + len];
    let mut out = Vec::new();
    for atom in child_boxes(ilst).unwrap_or_default() {
        if &atom.kind != b"----" {
            continue;
        }
        let inner = atom.payload(ilst);
        let Ok(Some(data)) = find_box(inner, b"data") else {
            continue;
        };
        let dp = data.payload(inner);
        if dp.len() < 8 {
            continue;
        }
        // `data` body is `[type: u32][locale: u32][value]`; type 1 == UTF-8 text,
        // which is the text path's job. Everything else is opaque binary.
        let type_code = u32::from_be_bytes([dp[0], dp[1], dp[2], dp[3]]);
        if type_code == 1 {
            continue;
        }
        // name/mean payloads carry a 4-byte FullBox prefix; default mean to iTunes.
        let Some(name) = find_box(inner, b"name").ok().flatten().and_then(|n| {
            let p = n.payload(inner);
            (p.len() >= 4).then(|| std::str::from_utf8(&p[4..]).ok()).flatten()
        }) else {
            continue;
        };
        let mean = find_box(inner, b"mean")
            .ok()
            .flatten()
            .map_or("com.apple.iTunes", |m| {
                let p = m.payload(inner);
                if p.len() >= 4 {
                    std::str::from_utf8(&p[4..]).unwrap_or("com.apple.iTunes")
                } else {
                    "com.apple.iTunes"
                }
            });
        out.push(EmbeddedBinaryTag {
            key: format!("----:{mean}:{name}"),
            payload: dp[8..].to_vec(),
        });
    }
    out
}
```

**Required import change:** `mp4.rs:7` currently reads `use crate::input::{ArtInput, EmbeddedPicture, TagInput};` — it does **not** import `EmbeddedBinaryTag`. Change it to:
```rust
use crate::input::{ArtInput, BinaryTagInput, EmbeddedBinaryTag, EmbeddedPicture, TagInput};
```
(Adding `BinaryTagInput` here too, ahead of Task 2 which needs it; an unused import is a warning, so if you prefer a clean Task-1 commit add only `EmbeddedBinaryTag` now and `BinaryTagInput` in Task 2. The `tests` module uses `use super::*` at `mp4.rs:678`, so both propagate to the in-file tests automatically.)

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test -p musefs-format read_binary_tags_extracts_opaque_freeform_skips_text`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add musefs-format/src/mp4.rs
git commit -m "$(cat <<'EOF'
feat(format): mp4::read_binary_tags — opaque `----` freeform extraction

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 2: MP4 synthesis refactor — `build_udta` → segment list, interleave binary `----`

The structural heart of the phase. `build_udta` changes from `(Vec<u8> prefix, u64 art_len)` to `(Vec<Segment> udta, u64 streamed_total)`; `synthesize_layout` gains a `binary_tags` parameter and splices binary payloads as streamed segments. The signature changes ripple to every caller and to the in-file tests — **all updated in this one commit so it compiles green.**

**Files:**
- Modify: `musefs-format/src/mp4.rs` (`build_udta`, `synthesize_layout`, new helpers, `tests` module)
- Modify: `musefs-core/src/reader.rs` (M4A arm)
- Modify: `musefs-format/tests/mp4_oracle.rs` (4 calls)
- Modify: `musefs-format/tests/proptest_mp4.rs` (1 call)
- Modify: `fuzz/fuzz_targets/mp4.rs` (1 call — keep compiling)

- [ ] **Step 1: Write the failing test**

Add to the `tests` module in `musefs-format/src/mp4.rs`. It drives the new `synthesize_layout` signature and asserts a binary `----` is emitted as a streamed segment, its bytes accounted for in `udta`/`moov` sizes, and the audio still served verbatim.

```rust
    #[test]
    fn synthesize_interleaves_binary_freeform_segment() {
        let buf = mk_mp4(true, b"AUDIODATA", &[42, 100]);
        let scan = read_structure(&buf).unwrap();
        let payload = vec![0xde, 0xad, 0xbe, 0xef, 0x00, 0x01];
        let bins = vec![BinaryTagInput {
            key: "----:com.serato.dj:analysis".into(),
            payload_id: 7,
            len: payload.len() as u64,
        }];
        let layout =
            synthesize_layout(&scan, &[TagInput::new("title", "T")], &bins, &[]).unwrap();

        // Exactly one streamed BinaryTag carrying our handle + length.
        let bt: Vec<_> = layout
            .segments()
            .iter()
            .filter_map(|s| match s {
                Segment::BinaryTag { payload_id, len } => Some((*payload_id, *len)),
                _ => None,
            })
            .collect();
        assert_eq!(bt, vec![(7, payload.len() as u64)]);

        // Audio is still served verbatim as the trailing BackingAudio run.
        match layout.segments().last().unwrap() {
            Segment::BackingAudio { offset, len } => {
                assert_eq!(*offset, scan.mdat_payload_offset);
                assert_eq!(*len, scan.mdat_payload_len);
            }
            _ => panic!("expected BackingAudio tail"),
        }

        // Box sizes are self-consistent: materialize the served file (binary payload
        // + backing audio substituted) and re-parse. `read_structure` validates every
        // moov/mdat box size, so a green re-parse proves the ----/ilst/meta/udta/moov
        // sizes all account for the streamed payload — and the opaque `----` survives
        // the round trip byte-identically.
        //
        // NOTE: do NOT use `inline_head`/`find_moov_in_head` here — the moov box now
        // spans multiple segments (the streamed BinaryTag splits it), so `read_box`
        // on `segments[0]` alone returns `Malformed`. Materialize the whole file.
        let mut served = Vec::new();
        for seg in layout.segments() {
            match seg {
                Segment::Inline(b) => served.extend_from_slice(b),
                Segment::BinaryTag { .. } => served.extend_from_slice(&payload),
                Segment::BackingAudio { offset, len } => {
                    let s = *offset as usize;
                    served.extend_from_slice(&buf[s..s + *len as usize]);
                }
                other => panic!("unexpected segment: {other:?}"),
            }
        }
        read_structure(&served).expect("synthesized file re-parses to a valid moov/mdat");
        // `read_binary_tags` returns a bare Vec (no promotion for MP4) and emits the
        // raw `mean:name` key WITHOUT folding through the vocabulary — `com.serato.dj`
        // is not in any vocabulary entry, so the key is preserved verbatim.
        let reparsed = read_binary_tags(&served);
        assert_eq!(reparsed.len(), 1);
        assert_eq!(reparsed[0].key, "----:com.serato.dj:analysis");
        assert_eq!(reparsed[0].payload, payload);
    }
```

`BinaryTagInput`, `Segment`, `read_structure`, and `read_binary_tags` are all reachable in the `tests` module via the `use super::*;` at `mp4.rs:678`, given the import change made in Task 1 Step 3.

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p musefs-format synthesize_interleaves_binary_freeform_segment`
Expected: FAIL to **compile** — `synthesize_layout` takes 3 args, not 4. (This is expected; the refactor in Steps 3–7 makes it compile and pass, and simultaneously fixes the other callers.)

- [ ] **Step 3: Add the two helper functions**

Insert before `build_udta` in `musefs-format/src/mp4.rs`:

```rust
/// Parse a `----:<mean>:<name>` opaque key back into `(mean, name)`. `name` may
/// contain `:` (only the first separator splits). `None` if not a freeform key.
fn parse_freeform_key(key: &str) -> Option<(&str, &str)> {
    key.strip_prefix("----:")?.split_once(':')
}

/// Inline framing for an opaque binary `----` atom whose `data` value
/// (`payload_len` bytes) streams next:
/// `[---- size][----][mean box][name box][data size][data][type 0][locale 0]`.
/// Mirrors `freeform_atom` but emits type code 0 (binary) and no value bytes — the
/// value is served from the DB as a `Segment::BinaryTag`.
fn freeform_binary_prefix(mean: &str, name: &str, payload_len: u64) -> Vec<u8> {
    let mut mean_body = 0u32.to_be_bytes().to_vec(); // version/flags
    mean_body.extend_from_slice(mean.as_bytes());
    let mean_box = boxed(b"mean", &mean_body);
    let mut name_body = 0u32.to_be_bytes().to_vec();
    name_body.extend_from_slice(name.as_bytes());
    let name_box = boxed(b"name", &name_body);

    let data_size = 8 + 8 + payload_len; // data header + type + locale + payload
    let inner_len = mean_box.len() as u64 + name_box.len() as u64 + data_size;

    let mut out = ((8 + inner_len) as u32).to_be_bytes().to_vec();
    out.extend_from_slice(b"----");
    out.extend_from_slice(&mean_box);
    out.extend_from_slice(&name_box);
    out.extend_from_slice(&(data_size as u32).to_be_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&0u32.to_be_bytes()); // type 0 = binary/implicit
    out.extend_from_slice(&0u32.to_be_bytes()); // locale
    out
}
```

- [ ] **Step 4: Replace `build_udta`**

Replace the entire `build_udta` function body and signature with:

```rust
/// Build the `udta` box as an ordered segment list: `Segment::Inline` for all box
/// framing, with each opaque `----` value and the cover image streamed from the DB
/// (`Segment::BinaryTag`/`Segment::ArtImage`) rather than materialized. Returns
/// `(segments, streamed_total)` where `streamed_total` sums every streamed payload
/// length (binary `----` values + art). Every enclosing box size
/// (`----`/`ilst`/`meta`/`udta`) accounts for `streamed_total` at the right nesting
/// depth, so the streamed bytes splice in correctly at read time.
fn build_udta(
    tags: &[TagInput],
    binary_tags: &[BinaryTagInput],
    art: Option<&ArtInput>,
) -> Result<(Vec<Segment>, u64)> {
    // Group consecutive same-key text values (the DB returns tags ordered by key).
    let mut groups: Vec<(&str, Vec<&str>)> = Vec::new();
    for t in tags {
        match groups.last_mut() {
            Some(g) if g.0 == t.key => g.1.push(&t.value),
            _ => groups.push((&t.key, vec![&t.value])),
        }
    }

    // ilst content: text atoms first (materialized), then opaque `----` (streamed),
    // then cover art (streamed). `ilst_inline` accumulates framing until a streamed
    // segment forces a flush.
    let mut ilst_inline: Vec<u8> = Vec::new();
    for (key, values) in &groups {
        match crate::tagmap::key_to_mp4(key) {
            Some(crate::tagmap::Mp4Slot::Text(atom)) => ilst_inline.extend(text_atom(atom, values)),
            Some(crate::tagmap::Mp4Slot::Number(atom, width)) => {
                if let Ok(n) = values.first().copied().unwrap_or("").parse::<u16>() {
                    ilst_inline.extend(number_atom(atom, n, width));
                }
            }
            Some(crate::tagmap::Mp4Slot::Freeform(mean, name)) => {
                ilst_inline.extend(freeform_atom(mean, name, values));
            }
            None => ilst_inline.extend(freeform_atom("com.apple.iTunes", key, values)),
        }
    }

    let mut ilst_segments: Vec<Segment> = Vec::new();
    let mut streamed_total: u64 = 0;

    for bt in binary_tags {
        if bt.len == 0 {
            // An empty BinaryTag fails `RegionLayout::validate` (EmptySegment).
            continue;
        }
        let Some((mean, name)) = parse_freeform_key(&bt.key) else {
            // Not a `----:<mean>:<name>` key; skip defensively (no double-store path).
            continue;
        };
        ilst_inline.extend_from_slice(&freeform_binary_prefix(mean, name, bt.len));
        ilst_segments.push(Segment::Inline(std::mem::take(&mut ilst_inline)));
        ilst_segments.push(Segment::BinaryTag {
            payload_id: bt.payload_id,
            len: bt.len,
        });
        streamed_total += bt.len;
    }

    if let Some(a) = art {
        let type_code: u32 = if a.mime == "image/png" { 14 } else { 13 };
        let data_size = 8 + 8 + a.data_len; // data header + type + locale + image
        let covr_size = 8 + data_size;
        ilst_inline.extend_from_slice(&(covr_size as u32).to_be_bytes());
        ilst_inline.extend_from_slice(b"covr");
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
    } else if !ilst_inline.is_empty() {
        ilst_segments.push(Segment::Inline(std::mem::take(&mut ilst_inline)));
    }

    let ilst_inline_len: u64 = ilst_segments
        .iter()
        .map(|s| match s {
            Segment::Inline(b) => b.len() as u64,
            _ => 0,
        })
        .sum();

    let mut hdlr_body = vec![0u8; 8];
    hdlr_body.extend_from_slice(b"mdir");
    hdlr_body.extend_from_slice(b"appl");
    hdlr_body.extend_from_slice(&[0u8; 9]);
    let hdlr = boxed(b"hdlr", &hdlr_body);

    // Box sizes. Each enclosing box adds its 8-byte header to the inline content of
    // its child and carries `streamed_total` through unchanged (the streamed bytes
    // live at the deepest level, inside ilst).
    let ilst_size = 8 + ilst_inline_len + streamed_total;
    let meta_inline_len = 4 + hdlr.len() as u64 + 8 + ilst_inline_len; // [vf][hdlr][ilst hdr][ilst inline]
    let meta_size = 8 + meta_inline_len + streamed_total;
    let udta_inline_len = 8 + meta_inline_len; // [meta hdr][meta inline]
    let udta_size = 8 + udta_inline_len + streamed_total;

    // MP4 box sizes are 32-bit. udta encloses all inner boxes, so guarding it bounds
    // them all; refuse oversized metadata at the format boundary rather than emit a
    // silently-truncated (corrupt) size field.
    if udta_size > u32::MAX as u64 {
        return Err(FormatError::TooLarge);
    }

    // Leading framing: everything up to the start of ilst content.
    let mut header = (udta_size as u32).to_be_bytes().to_vec();
    header.extend_from_slice(b"udta");
    header.extend_from_slice(&(meta_size as u32).to_be_bytes());
    header.extend_from_slice(b"meta");
    header.extend_from_slice(&0u32.to_be_bytes()); // meta FullBox version/flags
    header.extend_from_slice(&hdlr);
    header.extend_from_slice(&(ilst_size as u32).to_be_bytes());
    header.extend_from_slice(b"ilst");

    // Merge the header into the first ilst inline segment (always Inline when present,
    // since streamed segments are preceded by their framing).
    let mut segments: Vec<Segment> = Vec::new();
    let mut lead = header;
    for seg in ilst_segments {
        match seg {
            Segment::Inline(b) => lead.extend_from_slice(&b),
            other => {
                segments.push(Segment::Inline(std::mem::take(&mut lead)));
                segments.push(other);
            }
        }
    }
    if !lead.is_empty() {
        segments.push(Segment::Inline(lead));
    }
    Ok((segments, streamed_total))
}
```

Note: `BinaryTagInput` must be in scope. It was added to the `mp4.rs:7` import in Task 1 Step 3; if you deferred it there (to keep Task 1's commit warning-free), add it now: `use crate::input::{ArtInput, BinaryTagInput, EmbeddedBinaryTag, EmbeddedPicture, TagInput};`.

- [ ] **Step 5: Replace `synthesize_layout`**

Replace its signature and body:

```rust
/// Regenerate a re-tagged `moov` and produce the serving layout
/// `[ftyp][regenerated moov][mdat header][mdat payload]`. The mdat payload is
/// served verbatim, merely relocated, so every chunk offset shifts by a constant
/// `delta`. Patching only offset VALUES (never box sizes) means `new_moov_size` is
/// computable before `delta` — no circular dependency. Cover art and opaque `----`
/// binary tags stream from the DB at read time, splicing into the layout.
pub fn synthesize_layout(
    scan: &Mp4Scan,
    tags: &[TagInput],
    binary_tags: &[BinaryTagInput],
    arts: &[ArtInput],
) -> Result<RegionLayout> {
    let moov_payload_start = read_box(&scan.moov, 0)?.payload_start();
    let moov_payload = &scan.moov[moov_payload_start..];
    let mut kept = Vec::new();
    for b in child_boxes(moov_payload)? {
        if &b.kind != b"udta" {
            kept.extend_from_slice(&moov_payload[b.start..b.end()]);
        }
    }

    // Skip zero-byte art (an empty ArtImage segment fails layout validation).
    let art = arts.iter().find(|a| a.data_len > 0);
    let (udta_segments, _streamed_total) = build_udta(tags, binary_tags, art)?;
    let udta_total: u64 = udta_segments.iter().map(Segment::len).sum();

    let new_moov_size = 8 + kept.len() as u64 + udta_total;
    // MP4 box sizes are 32-bit; mirror build_udta's guard.
    if new_moov_size > u32::MAX as u64 {
        return Err(FormatError::TooLarge);
    }
    let new_mdat_payload_pos =
        scan.ftyp.len() as u64 + new_moov_size + scan.mdat_header.len() as u64;
    let delta = new_mdat_payload_pos as i64 - scan.mdat_payload_offset as i64;

    patch_chunk_offsets(&mut kept, delta)?;

    let mut head = Vec::new();
    head.extend_from_slice(&scan.ftyp);
    head.extend_from_slice(&(new_moov_size as u32).to_be_bytes());
    head.extend_from_slice(b"moov");
    head.extend_from_slice(&kept);

    // Splice the udta segment list after the moov head. `build_udta` guarantees a
    // non-empty list whose first element is the leading `Inline` framing (it opens
    // with the udta/meta/ilst header), so fold that into `head`; the rest (streamed
    // payloads + interleaved inline) follow. Finally append the truncated mdat header
    // to the last inline run before backing audio.
    let mut udta_iter = udta_segments.into_iter();
    let Some(Segment::Inline(first)) = udta_iter.next() else {
        // build_udta always yields a leading Inline; anything else is a producer bug.
        return Err(FormatError::InvalidLayout);
    };
    head.extend_from_slice(&first);
    let mut segments: Vec<Segment> = vec![Segment::Inline(head)];
    segments.extend(udta_iter);
    match segments.last_mut() {
        Some(Segment::Inline(b)) => b.extend_from_slice(&scan.mdat_header),
        _ => segments.push(Segment::Inline(scan.mdat_header.clone())),
    }
    segments.push(Segment::BackingAudio {
        offset: scan.mdat_payload_offset,
        len: scan.mdat_payload_len,
    });
    RegionLayout::validated(segments).map_err(|_| FormatError::InvalidLayout)
}
```

- [ ] **Step 6: Update the in-file `tests` module (build_udta callers + materialize helper)**

Add this helper to the `tests` module (near `inline_head`):

```rust
    /// Concatenate a udta segment list into a contiguous buffer, substituting `len`
    /// zero bytes for each streamed (BinaryTag/ArtImage) segment. Box-size fields
    /// already account for these, so the result parses as a complete udta box.
    /// Do NOT use for huge reserved art lengths — read the size field off `segs[0]`.
    fn materialize_udta(segments: &[Segment]) -> Vec<u8> {
        let mut out = Vec::new();
        for seg in segments {
            match seg {
                Segment::Inline(b) => out.extend_from_slice(b),
                Segment::BinaryTag { len, .. } | Segment::ArtImage { len, .. } => {
                    out.resize(out.len() + *len as usize, 0);
                }
                other => panic!("unexpected segment in udta: {other:?}"),
            }
        }
        out
    }
```

Then update each existing `build_udta` test as follows (the change is: add the `&[]` binary-tags arg, destructure `(segs, streamed)`, and derive `prefix` via `materialize_udta` — except the two that must not materialize huge art).

`build_udta_no_art_round_trips` — replace its line 921 area:
```rust
        let (segs, streamed) = build_udta(&tags, &[], None).unwrap();
        assert_eq!(streamed, 0);
        let prefix = materialize_udta(&segs);
```
(The remaining body — `read_box(&prefix, 0)`, wrap in moov, `read_tags` assertions — is unchanged.)

`build_udta_with_art_reserves_size_without_image` — replace the body after constructing `art`:
```rust
        let (segs, streamed) = build_udta(&[TagInput::new("title", "T")], &[], Some(&art)).unwrap();
        assert_eq!(streamed, 100);
        // The image streams as the final segment; the udta size field accounts for it.
        assert!(matches!(segs.last(), Some(Segment::ArtImage { len: 100, .. })));
        let inline_total: usize = segs
            .iter()
            .filter_map(|s| match s {
                Segment::Inline(b) => Some(b.len()),
                _ => None,
            })
            .sum();
        let Segment::Inline(head) = &segs[0] else {
            panic!("first udta segment is inline framing");
        };
        let declared = u32::from_be_bytes(head[0..4].try_into().unwrap()) as usize;
        assert_eq!(declared, inline_total + 100);
        // The leading inline ends right after the covr/data header (image streams next).
        assert!(head.windows(4).any(|w| w == b"covr"));
```

`build_udta_rejects_oversize_art` — add the `&[]` arg:
```rust
        assert!(matches!(
            build_udta(&[TagInput::new("title", "T")], &[], Some(&art)),
            Err(FormatError::TooLarge)
        ));
```

`build_udta_groups_multi_value_text` — replace line 984 area:
```rust
        let (segs, streamed) = build_udta(&tags, &[], None).unwrap();
        assert_eq!(streamed, 0);
        let prefix = materialize_udta(&segs);
```
(Rest of body unchanged — operates on `prefix`.)

`build_udta_empty_tags_is_valid` — replace line 1018 area:
```rust
        let (segs, streamed) = build_udta(&[], &[], None).unwrap();
        assert_eq!(streamed, 0);
        let prefix = materialize_udta(&segs);
```
(Rest unchanged.)

`build_udta_round_trips_freeform_and_vocabulary` — replace line 1403:
```rust
        let (segs, _streamed) = build_udta(&tags, &[], None).unwrap();
        let udta = materialize_udta(&segs);
```
(Rest unchanged — `boxed(b"moov", &udta)` then `read_tags`.)

`build_udta_png_art_uses_type_code_14` — replace line 1661:
```rust
            let (segs, _) = build_udta(&[TagInput::new("title", "T")], &[], Some(&art)).unwrap();
            let prefix = materialize_udta(&segs);
```
(Rest unchanged — `covr`/`data`/type-code byte inspection works on materialized bytes.)

`build_udta_art_box_sizes_are_exact` — replace line 1683:
```rust
        let (segs, _) = build_udta(&[TagInput::new("title", "T")], &[], Some(&art)).unwrap();
        let prefix = materialize_udta(&segs);
```
(Rest unchanged.)

`build_udta_udta_size_exactly_u32_max_is_ok` — this test reserves `~u32::MAX` art bytes as a *number* and must NOT materialize them. Replace its body after the `art` closure:
```rust
        // Derive the fixed overhead from the udta size field (segs[0] inline), with
        // data_len 0, without materializing any image bytes.
        let (segs0, _) = build_udta(&[TagInput::new("title", "T")], &[], Some(&art(0))).unwrap();
        let Segment::Inline(h0) = &segs0[0] else { panic!("inline head") };
        let overhead = u32::from_be_bytes(h0[0..4].try_into().unwrap()) as u64;
        let max_len = u32::MAX as u64 - overhead;

        let (segs_max, streamed) =
            build_udta(&[TagInput::new("title", "T")], &[], Some(&art(max_len))).unwrap();
        assert_eq!(streamed, max_len);
        let Segment::Inline(h_max) = &segs_max[0] else { panic!("inline head") };
        assert_eq!(u32::from_be_bytes(h_max[0..4].try_into().unwrap()), u32::MAX);

        assert!(matches!(
            build_udta(&[TagInput::new("title", "T")], &[], Some(&art(max_len + 1))),
            Err(FormatError::TooLarge)
        ));
```

Finally, update the in-file `synthesize_layout` test calls to add the `&[]` binary arg before the arts slice. These are at the lines listed below (search each and insert `&[]` as the third argument):
- `synthesize_no_art_patches_stco` → `synthesize_layout(&scan, &[TagInput::new("title", "New")], &[], &[])`
- the call at the old line ~1130 → `..., &[], &[])`
- the two art calls at old ~1163, ~1187 → `synthesize_layout(&scan, &[TagInput::new("title", "T")], &[], &[art])`
- the empty/real art call at old ~1228 → `synthesize_layout(&scan, &[TagInput::new("title", "T")], &[], &[empty, real])`
- the call at old ~1242 → `synthesize_layout(&scan, &[TagInput::new("title", "Z")], &[], &[])`
- the three u32-max art calls at old ~1806, ~1813, ~1816 → insert `&[],` before the `&[art(..)]` arg in each.

(Use `grep -nF "synthesize_layout(" musefs-format/src/mp4.rs` to confirm you have caught every test call; the definition is the only non-test occurrence.)

- [ ] **Step 7: Fix the external callers (signature ripple)**

`musefs-core/src/reader.rs` — the M4A arm (the `mp4::synthesize_layout(&scan, &inputs, &art_inputs)?` call). `binary_tag_inputs` is already in scope:
```rust
                        mp4::synthesize_layout(&scan, &inputs, &binary_tag_inputs, &art_inputs)?
```

`musefs-format/tests/mp4_oracle.rs` — insert `&[]` before the arts arg in all four calls:
- the multi-line call (~line 44): add `&[],` line before the existing `&[],` arts arg.
- `synthesize_layout(&scan, &[], &[], &[]).unwrap()` (was `&scan, &[], &[]`).
- `synthesize_layout(&scan, &[], &[], std::slice::from_ref(&art1)).unwrap()`.
- `synthesize_layout(&scan, &[], &[], &[art1, art2]).unwrap()`.

`musefs-format/tests/proptest_mp4.rs` — the existing `mp4_synthesis_preserves_audio` call:
```rust
        if let Ok(layout) = mp4::synthesize_layout(&scan, &taginputs, &[], &arts) {
```

`fuzz/fuzz_targets/mp4.rs` — keep it compiling (real binary fuzzing is Task 5):
```rust
    if let Ok(layout) = mp4::synthesize_layout(&scan, &tags, &[], &arts) {
```

- [ ] **Step 8: Run the full format + reader test suites**

Run: `cargo test -p musefs-format mp4`
Expected: PASS (including `synthesize_interleaves_binary_freeform_segment` and all updated `build_udta_*` / `synthesize_*` tests).

Run: `cargo test -p musefs-format --features fuzzing proptest_mp4`
Expected: PASS (`mp4_synthesis_preserves_audio`).

Run: `cargo build -p musefs-core && cargo test -p musefs-core reader`
Expected: PASS (M4A arm compiles and resolves).

- [ ] **Step 9: Lint + format + fuzz-compile check**

Run: `cargo clippy --all-targets`
Expected: no new warnings.

Run: `cargo +nightly fuzz build mp4`
Expected: builds (the fuzz crate is out of the workspace; this catches the signature break CI's smoke job would otherwise flag — see the `musefs-fuzz-out-of-workspace` memory).

Run: `cargo fmt --all --check`
Expected: clean (per the `musefs-prepush-checks` memory; check exit status).

- [ ] **Step 10: Commit**

```bash
git add musefs-format/src/mp4.rs musefs-core/src/reader.rs \
        musefs-format/tests/mp4_oracle.rs musefs-format/tests/proptest_mp4.rs \
        fuzz/fuzz_targets/mp4.rs
git commit -m "$(cat <<'EOF'
feat(format,core): MP4 synthesis streams opaque `----` binary tags

build_udta now returns an ordered segment list (inline framing interleaved
with streamed BinaryTag/ArtImage) instead of a materialized prefix; every
enclosing box size accounts for streamed payload lengths. synthesize_layout
gains a binary_tags parameter and the reader passes the resolved inputs.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 3: Wire the parser into scan

Populate `Probed.binary_tags` for MP4. The shared ingest path caps and persists them; no other scan change is needed.

**Files:**
- Modify: `musefs-core/src/scan.rs` (`probe_full` M4A arm, `probe_file` M4A arm, new test)

- [ ] **Step 1: Write the failing test**

Add to the `scan_unit_tests` module in `musefs-core/src/scan.rs` (mirror the existing `scan_ingests_binary_tags_and_promotes` MP3 test). It builds an m4a with a binary `----` atom, probes it, and asserts the binary tag is surfaced.

```rust
    #[test]
    fn probe_full_surfaces_mp4_binary_freeform() {
        use musefs_format::mp4;
        // A richer m4a the structure reader accepts, carrying one binary `----` atom.
        let bytes = mp4_with_binary_freeform("com.serato.dj", "analysis", &[0x00, 0xAB, 0xCD]);
        let probed = probe_full(std::path::Path::new("/x.m4a"), &bytes).expect("probed");
        assert_eq!(probed.format, Format::M4a);
        let keys: Vec<&str> = probed.binary_tags.iter().map(|b| b.key.as_str()).collect();
        assert!(
            keys.contains(&"----:com.serato.dj:analysis"),
            "binary freeform not surfaced: {keys:?}"
        );
        let bt = probed
            .binary_tags
            .iter()
            .find(|b| b.key == "----:com.serato.dj:analysis")
            .unwrap();
        assert_eq!(bt.payload, vec![0x00, 0xAB, 0xCD]);
        // Sanity: the probe also still resolves audio bounds.
        let scan = mp4::read_structure(&bytes).unwrap();
        assert_eq!(probed.audio_offset, scan.mdat_payload_offset);
    }
```

Add a fixture helper to the same module. **It must include exactly one `soun` audio track** — `probe_full`'s M4A arm calls `mp4::locate_audio`, which runs `validate_moov` (`mp4.rs:161-181`): it requires exactly one `trak` whose `mdia/hdlr` handler is `soun`, else returns `NotMp4` and `probe_full` yields `None`. A trak-less moov is therefore **rejected**. This builder adds a minimal valid `soun` trak alongside the `udta/meta/ilst` carrying the binary `----`:

```rust
    /// Minimal-but-valid m4a that `mp4::locate_audio` accepts (one `soun` trak),
    /// with a `udta/meta/ilst` carrying one binary `----` atom. `value` is the raw
    /// binary `data` payload (type code 0). Not synthesis-grade (no stco), but
    /// `probe_full` only locates audio + reads tags, never synthesizes.
    fn mp4_with_binary_freeform(mean: &str, name: &str, value: &[u8]) -> Vec<u8> {
        fn bx(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
            let mut v = ((8 + body.len()) as u32).to_be_bytes().to_vec();
            v.extend_from_slice(kind);
            v.extend_from_slice(body);
            v
        }
        // mdia/hdlr with handler type `soun` at payload offset 8..12 (FullBox
        // version/flags [0..4], pre_defined [4..8], handler_type [8..12]).
        let mut hdlr_body = vec![0u8; 8];
        hdlr_body.extend_from_slice(b"soun");
        hdlr_body.extend_from_slice(&[0u8; 12]); // reserved(12) + empty name
        let trak = bx(b"trak", &bx(b"mdia", &bx(b"hdlr", &hdlr_body)));

        // udta/meta/ilst with one binary `----` atom.
        let mut mean_body = 0u32.to_be_bytes().to_vec();
        mean_body.extend_from_slice(mean.as_bytes());
        let mut name_body = 0u32.to_be_bytes().to_vec();
        name_body.extend_from_slice(name.as_bytes());
        let mut data_body = 0u32.to_be_bytes().to_vec(); // type 0 = binary
        data_body.extend_from_slice(&0u32.to_be_bytes()); // locale
        data_body.extend_from_slice(value);
        let mut free = bx(b"mean", &mean_body);
        free.extend(bx(b"name", &name_body));
        free.extend(bx(b"data", &data_body));
        let ilst = bx(b"ilst", &bx(b"----", &free));
        let mut meta = 0u32.to_be_bytes().to_vec();
        meta.extend(bx(b"hdlr", &[0u8; 25]));
        meta.extend(ilst);
        let udta = bx(b"udta", &bx(b"meta", &meta));

        let moov = bx(b"moov", &[trak, udta].concat());
        [bx(b"ftyp", b"M4A "), moov, bx(b"mdat", b"AUDIODATA")].concat()
    }
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p musefs-core probe_full_surfaces_mp4_binary_freeform`
Expected: FAIL — `binary_tags` is empty for MP4 (the arm still sets `binary_tags: Vec::new()`).

- [ ] **Step 3: Populate `Probed.binary_tags` in both MP4 probe arms**

In `probe_full` (`musefs-core/src/scan.rs`), the M4A branch — change `binary_tags: Vec::new()` to:
```rust
            binary_tags: mp4::read_binary_tags(bytes),
```

In `probe_file` (`musefs-core/src/scan.rs`), the M4A seek-reader branch — change `binary_tags: Vec::new()` to:
```rust
            binary_tags: mp4::read_binary_tags(&scan.moov),
```

(`mp4` is already imported at the top of `scan.rs`.)

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test -p musefs-core probe_full_surfaces_mp4_binary_freeform`
Expected: PASS.

- [ ] **Step 5: Run the broader scan suite**

Run: `cargo test -p musefs-core scan`
Expected: PASS (existing `ingest_filters_empty_and_oversize_binary_tags` etc. unaffected; MP4 binary tags now flow through the same capped ingest).

- [ ] **Step 6: Commit**

```bash
git add musefs-core/src/scan.rs
git commit -m "$(cat <<'EOF'
feat(core): scan surfaces MP4 `----` binary tags into the DB

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 4: End-to-end MP4 binary round-trip property test

The spec's per-format proof: parse → store-handle → synthesize → re-parse yields byte-identical binary payloads. `build_udta` is private, so the round trip goes through the public `synthesize_layout`, materializing the layout (substituting the BinaryTag payloads from a map standing in for the DB blob store, and the backing audio from the fixture).

**Files:**
- Modify: `musefs-format/tests/proptest_mp4.rs` (new property test)

- [ ] **Step 1: Write the failing test**

Append to `musefs-format/tests/proptest_mp4.rs` inside the existing `proptest! { ... }` block (and extend the top `use` to `use musefs_format::{mp4, ArtInput, BinaryTagInput, RegionLayout, Segment, TagInput};`):

```rust
    #[test]
    fn mp4_binary_freeform_round_trips_byte_identically(
        payload_audio in proptest::collection::vec(any::<u8>(), 1..256),
        bins in proptest::collection::vec(
            ("[a-zA-Z][a-zA-Z0-9._]{0,11}", proptest::collection::vec(any::<u8>(), 1..80)),
            1..5,
        ),
    ) {
        use std::collections::HashMap;

        let file = fixtures::m4a(&payload_audio);
        let scan = mp4::read_structure(&file).unwrap();

        // Synthetic payload handles standing in for `tags` rowids; a map stands in
        // for the DB blob store the reader streams from.
        let mut inputs: Vec<BinaryTagInput> = Vec::new();
        let mut map: HashMap<i64, Vec<u8>> = HashMap::new();
        for (i, (name, bytes)) in bins.iter().enumerate() {
            let id = i as i64 + 1;
            inputs.push(BinaryTagInput {
                key: format!("----:com.apple.iTunes:{name}"),
                payload_id: id,
                len: bytes.len() as u64,
            });
            map.insert(id, bytes.clone());
        }

        let layout = mp4::synthesize_layout(&scan, &[], &inputs, &[]).unwrap();

        // Byte-identical-audio invariant WITH binary frames present (spec §Testing):
        // the original mdat payload is still served verbatim as a BackingAudio run.
        assert_backing_covers_audio(scan.mdat_payload_offset, scan.mdat_payload_len, &layout);

        // Materialize the served file: inline verbatim, BinaryTag from the map,
        // BackingAudio from the original fixture.
        fn materialize(layout: &RegionLayout, original: &[u8], map: &HashMap<i64, Vec<u8>>) -> Vec<u8> {
            let mut out = Vec::new();
            for seg in layout.segments() {
                match seg {
                    Segment::Inline(b) => out.extend_from_slice(b),
                    Segment::BinaryTag { payload_id, .. } => {
                        out.extend_from_slice(map.get(payload_id).unwrap())
                    }
                    Segment::BackingAudio { offset, len } => {
                        let s = *offset as usize;
                        out.extend_from_slice(&original[s..s + *len as usize])
                    }
                    other => panic!("unexpected segment: {other:?}"),
                }
            }
            out
        }
        let served = materialize(&layout, &file, &map);

        // Re-parse the served file: every input payload survives byte-identically.
        let reparsed = mp4::read_binary_tags(&served);
        prop_assert_eq!(reparsed.len(), inputs.len(), "binary tag count mismatch");
        for input in &inputs {
            let want = map.get(&input.payload_id).unwrap();
            let found = reparsed
                .iter()
                .find(|t| t.key == input.key && &t.payload == want);
            prop_assert!(found.is_some(), "round-trip lost {:?}", input.key);
        }
    }
```

`RegionLayout` and `Segment` are both re-exported from `musefs_format` (`musefs-format/src/lib.rs:18`), so the `use` line above is sufficient — no path qualification needed.

- [ ] **Step 2: Run the test to verify it fails first, then passes**

Run: `cargo test -p musefs-format --features fuzzing mp4_binary_freeform_round_trips_byte_identically`
Expected: PASS (the implementation from Tasks 1–2 already supports it; if it fails, the failure is a real defect in the synthesis box-size math or the parser — debug before proceeding, do not weaken the assertion).

Note: if you want to *see* it fail first (TDD discipline), temporarily stub the assertion target — but since the production code already exists, the meaningful check here is that the round trip holds; a green run is the success signal.

- [ ] **Step 3: Commit**

```bash
git add musefs-format/tests/proptest_mp4.rs
git commit -m "$(cat <<'EOF'
test(format): MP4 binary `----` round-trip proptest (payload byte-identical)

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 5: Fuzz binary-tag coverage for MP4

Exercise the new streamed-segment synthesis path under the fuzzer. `synthesize_layout` never reads binary payload *bytes* (only `len` for box sizing + emits `BinaryTag` handles), so synthetic `BinaryTagInput`s with no DB are valid fuzz inputs; `assert_backing_covers_audio` still holds.

**Files:**
- Modify: `fuzz/src/lib.rs` (new `arb_binary_tags` helper)
- Modify: `fuzz/fuzz_targets/mp4.rs` (feed binary tags)
- Modify: `fuzz/src/bin/generate_seeds.rs` (a binary-bearing m4a seed)

- [ ] **Step 1: Add `arb_binary_tags` to `fuzz/src/lib.rs`**

Extend the top import to `use musefs_format::{ArtInput, BinaryTagInput, TagInput};` and add:

```rust
/// Build a small vec of BinaryTagInputs (synthetic handles + bounded lengths; the
/// synthesis path never reads payload bytes, only `len` for box sizing).
pub fn arb_binary_tags(u: &mut Unstructured) -> arbitrary::Result<Vec<BinaryTagInput>> {
    let n = u.int_in_range(0..=4u8)?;
    let mut out = Vec::new();
    for i in 0..n {
        let name = String::arbitrary(u)?;
        out.push(BinaryTagInput {
            key: format!("----:com.apple.iTunes:{name}"),
            payload_id: i as i64 + 1,
            len: u.int_in_range(1..=4096u64)?,
        });
    }
    Ok(out)
}
```

- [ ] **Step 2: Feed binary tags in `fuzz/fuzz_targets/mp4.rs`**

```rust
use musefs_fuzz::{arb_arts, arb_binary_tags, arb_tags, MAX_INPUT};
// ...
    let tags = arb_tags(&mut u).unwrap_or_default();
    let binary = arb_binary_tags(&mut u).unwrap_or_default();
    let arts = arb_arts(&mut u).unwrap_or_default();
    if let Ok(layout) = mp4::synthesize_layout(&scan, &tags, &binary, &arts) {
        assert_backing_covers_audio(scan.mdat_payload_offset, scan.mdat_payload_len, &layout);
    }
```

- [ ] **Step 3: Add a binary-bearing m4a seed in `fuzz/src/bin/generate_seeds.rs`**

The current seed is written by `write("mp4", "seed0", &fixtures::m4a(&[9u8; 32]))` in `main()`. The fuzz target derives `BinaryTagInput`s from the *trailing* entropy of `data`, so a seed only needs extra bytes after the file for the `Unstructured` reader to produce a non-empty binary vec. Add a second m4a seed in `main()` right after the existing `write("mp4", "seed0", ...)` line:

```rust
    // m4a seed with trailing entropy so arb_binary_tags/arb_arts yield non-empty inputs.
    let mut m4a_bin = fixtures::m4a(&[9u8; 32]);
    m4a_bin.extend_from_slice(&[0x01; 64]);
    write("mp4", "seed_binary", &m4a_bin);
```

- [ ] **Step 4: Build the fuzz targets and regenerate seeds**

Run: `cargo +nightly fuzz build mp4`
Expected: builds clean.

Run: `cargo run --manifest-path fuzz/Cargo.toml --bin generate_seeds`
Expected: writes the seed corpus including the new `fuzz/corpus/mp4/seed_binary` seed, no panic.

- [ ] **Step 5: Smoke-run the fuzzer briefly**

Run: `cargo +nightly fuzz run mp4 -- -runs=20000`
Expected: completes without a crash/assertion failure (this exercises the binary-interleaving box-size math under random inputs).

- [ ] **Step 6: Commit**

```bash
git add fuzz/src/lib.rs fuzz/fuzz_targets/mp4.rs fuzz/src/bin/generate_seeds.rs
git commit -m "$(cat <<'EOF'
test(fuzz): exercise MP4 `----` binary-tag synthesis path

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 6: Full-workspace verification gate

Confirm the whole phase is green together before declaring done.

**Files:** none (verification only).

- [ ] **Step 1: Full workspace test (includes feature-unified proptests)**

Run: `cargo test --workspace`
Expected: PASS. (Feature unification pulls `musefs-format`'s `fuzzing`-gated proptests in via the workspace, per CLAUDE.md.)

- [ ] **Step 2: Explicit format proptests**

Run: `cargo test -p musefs-format --features fuzzing`
Expected: PASS (`proptest_mp4` both properties, plus the unchanged others).

- [ ] **Step 3: Lint + format**

Run: `cargo clippy --all-targets`
Expected: no new warnings.

Run: `cargo fmt --all --check`
Expected: clean (per `musefs-prepush-checks` — check exit status directly).

- [ ] **Step 4: Fuzz smoke (out-of-workspace crate)**

Run: `cargo +nightly fuzz build mp4`
Expected: builds (guards against the CI fuzz smoke job breaking, per the `musefs-fuzz-out-of-workspace` memory).

- [ ] **Step 5: Final review of the diff against `main`**

Run: `git diff main --stat`
Expected: changes confined to `musefs-format/src/mp4.rs`, `musefs-core/src/{scan,reader}.rs`, `musefs-format/tests/{proptest_mp4,mp4_oracle}.rs`, and `fuzz/{src/lib.rs,fuzz_targets/mp4.rs,src/bin/generate_seeds.rs}`. No schema, DB, facade, or layout-core changes (all foundation was merged in Phases 1–2).

---

## Out of scope for this phase (tracked for Phase 4 / Phase 5)

- **FLAC `APPLICATION`/`CUESHEET` + structural store** — Phase 4 (spec §5 FLAC, §7).
- **Cross-format interop (mutagen) `----`-atom fixture** — spec §Testing lists interop expansion under Phase 5; the MP4 `----` mutagen assertion belongs there, not here. (The format-layer round-trip proptest in Task 4 is the per-format proof required for *this* phase.)
- **Multi-value binary freeform** — like `read_freeform`, `read_binary_tags` reads only the first `data` sub-box per `----` atom; multi-`data` binary freeform is not preserved (documented in the function doc).
- **Original `data` type-code preservation** — synthesis emits type 0; only the payload is preserved (spec's payload-exact contract).
