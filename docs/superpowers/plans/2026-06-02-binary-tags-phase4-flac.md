# Binary Tags Phase 4 — FLAC Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make FLAC `APPLICATION`/`CUESHEET` blocks survive the round trip as DB-backed editable blobs, move `STREAMINFO`/`SEEKTABLE` into the read-only structural store, and eliminate the per-resolve FLAC front re-read for rescanned tracks (with a graceful front-read fallback for not-yet-rescanned legacy tracks).

**Architecture:** Scan splits FLAC's preserved metadata blocks into a structural store (`structural_blocks`: STREAMINFO/SEEKTABLE) and binary tags (`tags.value_blob`: APPLICATION/CUESHEET). `flac::synthesize_layout` stops taking a file-derived `FlacScan` and instead assembles from the structural blocks (inline) + regenerated VORBIS_COMMENT + streamed `Segment::BinaryTag` (APPLICATION/CUESHEET) + streamed `ArtImage` (pictures) + backing audio. The reader loads structural blocks from the DB; when absent (legacy track) it falls back to the existing `read_front` + `flac::read_metadata` path, carrying all preserved blocks inline exactly as today.

**Tech Stack:** Rust workspace (musefs-db / musefs-format / musefs-core), rusqlite (SQLite), metaflac (test oracle), proptest.

---

## Context the engineer needs before starting

Phases 1–3 are already merged. The shared foundation **already exists** — do not re-create it:

- `Segment::BinaryTag { payload_id: i64, len: u64 }` (`musefs-format/src/layout.rs`) and `RegionLayout::has_binary_tag()`.
- `BinaryTagInput { key, payload_id, len }` and `EmbeddedBinaryTag { key, payload }` (`musefs-format/src/input.rs`).
- `Db::set_binary_tags` / `Db::get_binary_tags` / `Db::read_binary_tag_chunk` and `BulkWriter::set_binary_tags` (`musefs-db/src/tags.rs`, `bulk.rs`).
- `Db::set_structural_blocks` / `Db::get_structural_blocks` and `StructuralBlock { kind, ordinal, body }` (`musefs-db/src/structural.rs`, `models.rs`).
- Migration V2 with the `value_blob` column and the `structural_blocks` table (`musefs-db/src/schema.rs`).
- `MAX_BINARY_TAG_BYTES` and the scan-time filter/ordinal logic for binary tags (`musefs-core/src/scan.rs`).
- `mapping::binary_tags_to_inputs(db, track_id)` (`musefs-core/src/mapping.rs`).
- The reader's `read_at` arm for `Segment::BinaryTag`, the `facade.rs` generation-gated re-resolve + transactional `content_version` guard for `has_binary_tag` layouts (the open-handle gap closed in Phase 2), and the `value_blob IS NULL` query-split filter on `tags_grouped`/`tags_for_tracks`.
- Test helpers in `musefs-format/tests/common/mod.rs`: `flac_block`, `streaminfo_body`, `vorbis_comment_body`, `make_flac`, and `resolve_layout(layout, backing, art_map, binary_tag_map)` (already supports `Segment::BinaryTag`).
- Test helpers in `musefs-core/tests/common`: `make_flac`, `streaminfo_body`, `vorbis_comment_body` (used by `metrics.rs`).

FLAC block-type constants (`musefs-format/src/flac.rs`, `pub(crate)`): `BLOCK_STREAMINFO=0`, `BLOCK_APPLICATION=2`, `BLOCK_SEEKTABLE=3`, `BLOCK_VORBIS_COMMENT=4`, `BLOCK_CUESHEET=5`, `BLOCK_PICTURE=6`.

**What this phase does NOT touch:** the `facade.rs` open-handle guard (already done in Phase 2 and covers `has_binary_tag` layouts), the `read_at` read path (already handles `BinaryTag`), the query-split filter (already in place), or any new error variant (none is needed — `read_binary_tag_chunk` reuses the art short-read error).

**Crate dependency direction (do not violate):** `musefs-db` ← `musefs-format` ← `musefs-core`. `musefs-format` may use `musefs-db` types, but the format parse output uses format-native types (`EmbeddedBinaryTag`, tuples) — the `StructuralBlock`/`BinaryTag` DB models are built in `musefs-core` (scan) and `musefs-db` (bulk), mirroring the existing art path.

**Pre-push reminder:** run `cargo fmt --all --check` before any push (CI fmt gate). The fuzz crate is out of the workspace — not built by `cargo test`; this phase touches no fuzz target.

---

## File structure

| File | Responsibility | Change |
|------|----------------|--------|
| `musefs-db/src/bulk.rs` | Batched scan writes in one transaction | **Add** `BulkWriter::set_structural_blocks` (mirror `set_binary_tags`) |
| `musefs-format/src/flac.rs` | FLAC byte-surgery: parse + synthesis | **Add** `split_preserved`, `structural_block_type`; **rewrite** `synthesize_layout` signature + body (binary-tag emission, canonical order) |
| `musefs-format/tests/{synthesize_tags,synthesize_art,roundtrip,proptest_flac}.rs` | FLAC synthesis tests | **Mechanical** update to the new `synthesize_layout` signature + **new** binary-tag round-trip tests |
| `musefs-core/src/reader.rs` | Resolve / layout build | **Rewrite** the `Format::Flac` arm: load `structural_blocks` from DB, legacy front-read fallback, pass `binary_tag_inputs` |
| `musefs-core/src/scan.rs` | Ingest backing files into the DB | **Add** `Probed.structural_blocks`; populate FLAC arms via `split_preserved`; persist structural blocks in `ingest`/`ingest_bulk` |
| `musefs-core/tests/flac_binary_tags.rs` | New integration tests | **Create**: scan→DB persistence, end-to-end serve, legacy fallback |
| `musefs-core/tests/metrics.rs` | Open/read syscall accounting | **Add** a test: rescanned FLAC resolve does zero front reads |

---

## Task 1: `BulkWriter::set_structural_blocks` (musefs-db)

`ingest_bulk` (the scan batch path) needs to write structural blocks, but `BulkWriter` only has `set_binary_tags`. Add the structural twin so the batch path can persist STREAMINFO/SEEKTABLE in the same transaction.

**Files:**
- Modify: `musefs-db/src/bulk.rs` (import line `1`; add method after `set_binary_tags`, ~`musefs-db/src/bulk.rs:95`)
- Test: `musefs-db/src/bulk.rs` (in the existing `#[cfg(test)] mod tests`)

- [ ] **Step 1: Write the failing test**

Add to the `tests` module at the bottom of `musefs-db/src/bulk.rs`:

```rust
    #[test]
    fn bulk_set_structural_blocks_round_trips() {
        use crate::StructuralBlock;
        let db = Db::open_in_memory().unwrap();
        let id = {
            let mut bw = db.bulk_writer().unwrap();
            let id = bw
                .upsert_track(&NewTrack {
                    backing_path: "/a.flac".into(),
                    format: Format::Flac,
                    audio_offset: 0,
                    audio_length: 1,
                    backing_size: 1,
                    backing_mtime: 0,
                })
                .unwrap();
            bw.set_structural_blocks(
                id,
                &[
                    StructuralBlock { kind: "STREAMINFO".into(), ordinal: 0, body: vec![1, 2] },
                    StructuralBlock { kind: "SEEKTABLE".into(), ordinal: 0, body: vec![3] },
                ],
            )
            .unwrap();
            bw.commit().unwrap();
            id
        };
        let got = db.get_structural_blocks(id).unwrap();
        assert_eq!(got.len(), 2);
        // get_structural_blocks orders by kind: SEEKTABLE before STREAMINFO.
        assert_eq!(got[0].kind, "SEEKTABLE");
        assert_eq!(got[1].body, vec![1, 2]);
    }
```

Check the existing `tests` module imports at the top of the module; if `NewTrack`/`Format` are not already imported there, add `use crate::{Db, Format, NewTrack};` to that module (match the existing `bulk_set_binary_tags_round_trips` test's imports). `commit` is the existing `BulkWriter` method — confirm its name in the file (`bw.commit()`).

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p musefs-db bulk_set_structural_blocks_round_trips`
Expected: FAIL — `no method named set_structural_blocks found for struct BulkWriter`.

- [ ] **Step 3: Add the method**

In `musefs-db/src/bulk.rs`, change the import on line `1` to include `StructuralBlock`:

```rust
use crate::models::{BinaryTag, NewArt, NewTrack, StructuralBlock, Tag, TrackArt};
```

Insert this method into `impl BulkWriter<'_>` immediately after `set_binary_tags` (after `musefs-db/src/bulk.rs:95`):

```rust
    pub fn set_structural_blocks(
        &mut self,
        track_id: i64,
        blocks: &[StructuralBlock],
    ) -> Result<()> {
        self.tx.execute(
            "DELETE FROM structural_blocks WHERE track_id = ?1",
            params![track_id],
        )?;
        let mut stmt = self.tx.prepare(
            "INSERT INTO structural_blocks (track_id, kind, ordinal, body) \
             VALUES (?1, ?2, ?3, ?4)",
        )?;
        for b in blocks {
            stmt.execute(params![track_id, b.kind, b.ordinal, b.body])?;
        }
        Ok(())
    }
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p musefs-db bulk_set_structural_blocks_round_trips`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add musefs-db/src/bulk.rs
git commit -m "feat(db): BulkWriter::set_structural_blocks for the scan batch path

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 2: `flac::split_preserved` + `flac::structural_block_type` (musefs-format)

The scan path needs to split FLAC's already-parsed preserved blocks into (a) structural blocks (STREAMINFO/SEEKTABLE → `(kind, body)` pairs) and (b) binary tags (APPLICATION/CUESHEET → `EmbeddedBinaryTag`). The reader needs to map a stored structural `kind` string back to a FLAC block type when rebuilding inline blocks. Both keep all FLAC block-type knowledge inside `flac.rs`. These are pure additions — no existing behavior changes.

**Files:**
- Modify: `musefs-format/src/flac.rs` (import line `146`; add two `pub fn`s near the synthesis helper `push_block_header`)
- Test: `musefs-format/src/flac.rs` (in the existing `#[cfg(test)] mod tests`)

- [ ] **Step 1: Write the failing test**

Add to the `tests` module in `musefs-format/src/flac.rs`:

```rust
    #[test]
    fn split_preserved_classifies_structural_and_binary() {
        use super::{split_preserved, structural_block_type, MetadataBlock};
        // STREAMINFO(0), APPLICATION(2), SEEKTABLE(3), CUESHEET(5) in arbitrary order.
        let blocks = vec![
            MetadataBlock { block_type: 0, body: vec![0xAA] },
            MetadataBlock { block_type: 2, body: b"testDATA".to_vec() },
            MetadataBlock { block_type: 3, body: vec![0xBB] },
            MetadataBlock { block_type: 5, body: vec![0xCC; 4] },
        ];
        let (structural, binary) = split_preserved(&blocks);

        assert_eq!(
            structural,
            vec![
                ("STREAMINFO".to_string(), vec![0xAA]),
                ("SEEKTABLE".to_string(), vec![0xBB]),
            ]
        );
        assert_eq!(binary.len(), 2);
        assert_eq!(binary[0].key, "APPLICATION");
        assert_eq!(binary[0].payload, b"testDATA");
        assert_eq!(binary[1].key, "CUESHEET");
        assert_eq!(binary[1].payload, vec![0xCC; 4]);

        assert_eq!(structural_block_type("STREAMINFO"), Some(0));
        assert_eq!(structural_block_type("SEEKTABLE"), Some(3));
        assert_eq!(structural_block_type("APPLICATION"), None);
        assert_eq!(structural_block_type("bogus"), None);
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p musefs-format split_preserved_classifies_structural_and_binary`
Expected: FAIL — `cannot find function split_preserved` / `structural_block_type`.

- [ ] **Step 3: Add the functions**

In `musefs-format/src/flac.rs`, extend the import on line `146`:

```rust
use crate::input::{ArtInput, BinaryTagInput, EmbeddedBinaryTag, EmbeddedPicture, TagInput};
```

Add these two functions next to the synthesis helper `push_block_header` (after `musefs-format/src/flac.rs:154`), so they sit with the synthesis-side code that pairs with them rather than in the parse region. (Insertion order does not affect compilation — Rust `use` is not order-sensitive within a module — but locality keeps the file readable.)

```rust
/// Map a stored structural-block `kind` string back to its FLAC block type.
/// Only STREAMINFO/SEEKTABLE live in the structural store; everything else
/// returns `None` (APPLICATION/CUESHEET are binary tags, not structural).
pub fn structural_block_type(kind: &str) -> Option<u8> {
    match kind {
        "STREAMINFO" => Some(BLOCK_STREAMINFO),
        "SEEKTABLE" => Some(BLOCK_SEEKTABLE),
        _ => None,
    }
}

/// Split a FLAC file's preserved metadata blocks into the read-only structural
/// store (STREAMINFO/SEEKTABLE, as `(kind, body)` pairs in file order) and the
/// editable binary tags (APPLICATION/CUESHEET, as `EmbeddedBinaryTag`s keyed by
/// block name; `payload` is the full block body, including APPLICATION's 4-byte
/// app id). Blocks of any other type are ignored (PICTURE/VORBIS_COMMENT are
/// handled by their own paths and are never in `preserved`).
pub fn split_preserved(blocks: &[MetadataBlock]) -> (Vec<(String, Vec<u8>)>, Vec<EmbeddedBinaryTag>) {
    let mut structural = Vec::new();
    let mut binary = Vec::new();
    for blk in blocks {
        match blk.block_type {
            BLOCK_STREAMINFO => structural.push(("STREAMINFO".to_string(), blk.body.clone())),
            BLOCK_SEEKTABLE => structural.push(("SEEKTABLE".to_string(), blk.body.clone())),
            BLOCK_APPLICATION => binary.push(EmbeddedBinaryTag {
                key: "APPLICATION".to_string(),
                payload: blk.body.clone(),
            }),
            BLOCK_CUESHEET => binary.push(EmbeddedBinaryTag {
                key: "CUESHEET".to_string(),
                payload: blk.body.clone(),
            }),
            _ => {}
        }
    }
    (structural, binary)
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p musefs-format split_preserved_classifies_structural_and_binary`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add musefs-format/src/flac.rs
git commit -m "feat(format): flac::split_preserved + structural_block_type

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 3: Rewrite `flac::synthesize_layout` + reader FLAC arm

This is the core change. `synthesize_layout` drops `FlacScan` and takes structural blocks (inline) + explicit audio bounds + a `binary_tags` slice (streamed APPLICATION/CUESHEET). The canonical block order becomes: `fLaC` + STREAMINFO + (other structural) + VORBIS_COMMENT + APPLICATION/CUESHEET (streamed) + PICTURE (streamed) + backing audio. The reader's `Format::Flac` arm loads structural blocks from the DB and, when empty (legacy track not yet rescanned under V2), falls back to the existing `read_front` + `flac::read_metadata` path, carrying *all* preserved blocks (incl. APPLICATION/CUESHEET) inline — exactly today's behavior.

After this task, **scan does not yet populate `structural_blocks`** (Task 4), so every FLAC track resolves via the legacy fallback. That is correct and tests pass green; Task 4 then activates the fast path.

**Behavior change to call out for reviewers:** re-synthesized FLAC block order is the canonical order above, which may differ from the source file's order. The FLAC spec only mandates STREAMINFO first, so this is valid. Round-trip tests assert payload/round-trip fidelity, **not** byte-identical block ordering.

**Files:**
- Modify: `musefs-format/src/flac.rs` — `synthesize_layout` (`musefs-format/src/flac.rs:171-238`)
- Modify: `musefs-core/src/reader.rs` — import line `6`; `Format::Flac` arm (`musefs-core/src/reader.rs:261-271`)
- Modify (mechanical): `musefs-format/tests/synthesize_tags.rs`, `synthesize_art.rs`, `roundtrip.rs`, `proptest_flac.rs`, and the in-file test `synthesize_layout_picture_block_size_boundary_is_inclusive`
- Test: `musefs-format/tests/roundtrip.rs` (new binary-tag round-trip tests)

- [ ] **Step 1: Write the failing tests (new binary-tag round-trip)**

Append to `musefs-format/tests/roundtrip.rs` (it already has `mod common;` and the metaflac dev-dep). Add the needed imports at the top — `use musefs_format::{ArtInput, BinaryTagInput, Segment, TagInput};` (extend the existing `use musefs_format::{ArtInput, TagInput};` line on `musefs-format/tests/roundtrip.rs:6`) and `use musefs_format::flac::MetadataBlock;`:

```rust
#[test]
fn application_block_streams_and_metaflac_reads_it() {
    let si = streaminfo_body();
    let app_body = b"testAPPDATA".to_vec(); // 4-byte app id "test" + payload "APPDATA"
    let audio = vec![0xABu8; 48];

    let structural = vec![MetadataBlock { block_type: 0, body: si }];
    let binary = vec![BinaryTagInput {
        key: "APPLICATION".into(),
        payload_id: 100,
        len: app_body.len() as u64,
    }];
    let layout = synthesize_layout(
        &structural,
        0,
        audio.len() as u64,
        &[TagInput::new("title", "T")],
        &binary,
        &[],
    )
    .unwrap();

    // The APPLICATION payload is streamed, never materialized into an Inline segment.
    let bt = layout
        .segments()
        .iter()
        .filter(|s| matches!(s, Segment::BinaryTag { .. }))
        .count();
    assert_eq!(bt, 1);

    let mut bt_map = std::collections::HashMap::new();
    bt_map.insert(100i64, app_body.clone());
    let assembled = resolve_layout(&layout, &audio, &HashMap::new(), &bt_map);

    let tag =
        metaflac::Tag::read_from(&mut Cursor::new(&assembled)).expect("valid FLAC metadata");
    let (id, data) = tag
        .blocks()
        .find_map(|b| match b {
            metaflac::Block::Application(a) => Some((a.id, a.data.clone())),
            _ => None,
        })
        .expect("application block present");
    assert_eq!(&id, b"test");
    assert_eq!(data, b"APPDATA");
}

#[test]
fn application_and_cuesheet_framing_is_valid_and_last_block_correct() {
    // Two streamed binary blocks and no art: the FINAL binary block (CUESHEET)
    // must carry the last-metadata-block flag, or a re-parse walks into the audio.
    let si = streaminfo_body();
    let app_body = b"testAPP1".to_vec();
    let cue_body = vec![0x11u8; 40];
    let audio = vec![0xCDu8; 32];

    let structural = vec![MetadataBlock { block_type: 0, body: si }];
    let binary = vec![
        BinaryTagInput { key: "APPLICATION".into(), payload_id: 1, len: app_body.len() as u64 },
        BinaryTagInput { key: "CUESHEET".into(), payload_id: 2, len: cue_body.len() as u64 },
    ];
    let layout = synthesize_layout(
        &structural,
        0,
        audio.len() as u64,
        &[TagInput::new("title", "T")],
        &binary,
        &[],
    )
    .unwrap();

    assert_eq!(
        layout
            .segments()
            .iter()
            .filter(|s| matches!(s, Segment::BinaryTag { .. }))
            .count(),
        2
    );

    let mut bt_map = std::collections::HashMap::new();
    bt_map.insert(1i64, app_body);
    bt_map.insert(2i64, cue_body);
    let assembled = resolve_layout(&layout, &audio, &HashMap::new(), &bt_map);

    // The metadata walk (header lengths + last-block flag) must land exactly on the
    // audio boundary. locate_audio does not interpret cuesheet semantics.
    let rescan = locate_audio(&assembled).expect("synthesized FLAC must parse");
    assert_eq!(rescan.audio_offset, layout.header_len());
}

#[test]
fn binary_tag_over_24bit_limit_errors() {
    use musefs_format::FormatError;
    let structural = vec![MetadataBlock { block_type: 0, body: streaminfo_body() }];
    let binary = vec![BinaryTagInput {
        key: "APPLICATION".into(),
        payload_id: 1,
        len: 0x0100_0000, // one over the 24-bit FLAC block-length limit (count only; no allocation)
    }];
    assert_eq!(
        synthesize_layout(&structural, 0, 0, &[], &binary, &[]),
        Err(FormatError::TooLarge)
    );
}
```

Note `roundtrip.rs` already imports `Cursor` and `HashMap` at the top; keep one canonical import (remove the inline `std::collections::HashMap` qualifications if the top-level `use std::collections::HashMap;` is present — check the file head and prefer the existing imports).

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p musefs-format --test roundtrip`
Expected: FAIL to compile — `synthesize_layout` takes 3 args, not 6 (old signature).

- [ ] **Step 3: Rewrite `synthesize_layout`**

Replace the body of `synthesize_layout` (`musefs-format/src/flac.rs:171-238`) with:

```rust
/// Build the ordered segment layout for a synthesized FLAC file. Canonical block
/// order: `fLaC` + STREAMINFO (first, per the FLAC spec) + any other structural
/// blocks (inline, verbatim) + a regenerated VORBIS_COMMENT + APPLICATION/CUESHEET
/// (streamed `Segment::BinaryTag`) + PICTURE blocks (streamed `Segment::ArtImage`)
/// + the backing audio. The source file's original block order is intentionally
/// NOT preserved (round-trips assert payload fidelity, not block ordering).
///
/// `structural` carries the inline blocks to emit verbatim: STREAMINFO/SEEKTABLE
/// from the structural store, or — on the legacy fallback path — every preserved
/// block (incl. APPLICATION/CUESHEET) when binary rows are not yet in the DB.
pub fn synthesize_layout(
    structural: &[MetadataBlock],
    audio_offset: u64,
    audio_length: u64,
    tags: &[TagInput],
    binary_tags: &[BinaryTagInput],
    arts: &[ArtInput],
) -> Result<RegionLayout> {
    // STREAMINFO must come first; sort the rest deterministically by block type
    // (STREAMINFO=0 < APPLICATION=2 < SEEKTABLE=3 < CUESHEET=5).
    let mut ordered: Vec<&MetadataBlock> = structural.iter().collect();
    ordered.sort_by_key(|b| b.block_type);

    let nonempty_art = arts.iter().filter(|a| a.data_len > 0).count();
    let num_blocks = ordered.len() + 1 + binary_tags.len() + nonempty_art; // +1 VORBIS_COMMENT
    let last_index = num_blocks - 1;

    let mut segments: Vec<Segment> = Vec::new();
    let mut buf: Vec<u8> = Vec::new();
    buf.extend_from_slice(FLAC_MARKER);
    let mut idx = 0usize;

    for blk in &ordered {
        push_block_header(&mut buf, blk.block_type, blk.body.len(), idx == last_index);
        buf.extend_from_slice(&blk.body);
        idx += 1;
    }

    let vc = crate::vorbiscomment::build(tags);
    push_block_header(&mut buf, BLOCK_VORBIS_COMMENT, vc.len(), idx == last_index);
    buf.extend_from_slice(&vc);
    idx += 1;

    for bt in binary_tags {
        let block_type = match bt.key.as_str() {
            "APPLICATION" => BLOCK_APPLICATION,
            "CUESHEET" => BLOCK_CUESHEET,
            // Unknown opaque key for FLAC: skip defensively rather than emit a
            // block with a bogus type. (Scan only ever writes the two keys above.)
            _ => continue,
        };
        // FLAC metadata block lengths are 24-bit; guard at the format boundary so an
        // oversized block is a hard error rather than a silently-truncated file.
        if bt.len > 0x00FF_FFFF {
            return Err(FormatError::TooLarge);
        }
        push_block_header(&mut buf, block_type, bt.len as usize, idx == last_index);
        segments.push(Segment::Inline(std::mem::take(&mut buf)));
        segments.push(Segment::BinaryTag {
            payload_id: bt.payload_id,
            len: bt.len,
        });
        idx += 1;
    }

    for art in arts {
        if art.data_len == 0 {
            continue; // skip degenerate empty art (see nonempty_art above)
        }
        let framing = picture_body_framing(art);
        let body_len = framing.len() as u64 + art.data_len;
        if body_len > 0x00FF_FFFF {
            return Err(FormatError::TooLarge);
        }
        push_block_header(&mut buf, BLOCK_PICTURE, body_len as usize, idx == last_index);
        buf.extend_from_slice(&framing);
        segments.push(Segment::Inline(std::mem::take(&mut buf)));
        segments.push(Segment::ArtImage {
            art_id: art.art_id,
            len: art.data_len,
        });
        idx += 1;
    }

    if !buf.is_empty() {
        segments.push(Segment::Inline(buf));
    }
    segments.push(Segment::BackingAudio {
        offset: audio_offset,
        len: audio_length,
    });

    RegionLayout::validated(segments).map_err(|_| FormatError::InvalidLayout)
}
```

- [ ] **Step 4: Update the in-format callers (test fallout)**

The signature changed from `synthesize_layout(&scan, &tags, &arts)` to `synthesize_layout(&structural, audio_offset, audio_length, &tags, &binary_tags, &arts)`. `FlacScan` keeps its `preserved`/`audio_offset`/`audio_length` fields (unchanged), so each call site that has a `scan` from `locate_audio` becomes `synthesize_layout(&scan.preserved, scan.audio_offset, scan.audio_length, &tags, &[], &arts)`.

`musefs-format/tests/synthesize_tags.rs` — three call sites (`:26`, `:47`, `:71`):
```rust
let layout = synthesize_layout(&scan.preserved, scan.audio_offset, scan.audio_length, &tags, &[], &[]).unwrap();
```
and (`:71`):
```rust
let layout = synthesize_layout(&scan.preserved, scan.audio_offset, scan.audio_length, &[TagInput::new("title", "X")], &[], &[]).unwrap();
```

`musefs-format/tests/synthesize_art.rs`:
- `:36`, `:66`: `synthesize_layout(&scan.preserved, scan.audio_offset, scan.audio_length, &[TagInput::new("title", "T")], &[], &[art]).unwrap();`
- `:117`: same shape with `&[art]` where `art = cover(7, 0)`.
- `:146`: `synthesize_layout(&scan.preserved, scan.audio_offset, scan.audio_length, &[TagInput::new("title", "T")], &[], &[empty, real]).unwrap();`
- `:84-105` (`synthesize_errors_on_oversized_picture`) builds a `FlacScan` manually; replace the call (`:102-105`) with the structural-slice form and drop the now-unused `FlacScan`:
```rust
#[test]
fn synthesize_errors_on_oversized_picture() {
    use musefs_format::FormatError;
    let art = ArtInput {
        art_id: 1,
        mime: "image/png".to_string(),
        description: String::new(),
        picture_type: 3,
        width: 0,
        height: 0,
        data_len: 0x0100_0000, // just over the 24-bit FLAC PICTURE block limit
    };
    assert_eq!(
        synthesize_layout(&[], 0, 0, &[], &[], &[art]),
        Err(FormatError::TooLarge)
    );
}
```
(Remove the `use musefs_format::flac::FlacScan;` line from this test.)

`musefs-format/tests/roundtrip.rs` (`:52`):
```rust
let layout = synthesize_layout(&scan.preserved, scan.audio_offset, scan.audio_length, &tags, &[], &arts).unwrap();
```

`musefs-format/tests/proptest_flac.rs` (`:22` and `:36`):
```rust
if let Ok(layout) = flac::synthesize_layout(&scan.preserved, scan.audio_offset, scan.audio_length, &taginputs, &[], &arts) {
```
```rust
let Ok(layout) = flac::synthesize_layout(&scan.preserved, scan.audio_offset, scan.audio_length, &taginputs, &[], &arts) else {
```

In-file test `synthesize_layout_picture_block_size_boundary_is_inclusive` (`musefs-format/src/flac.rs:817-848`) builds a `FlacScan { audio_offset: 0, audio_length: …, preserved: … }` and calls `synthesize_layout(&scan, &[], &[mk(at_limit)])`. Replace both calls with the structural-slice form, passing `&scan.preserved` (or `&[]` if it builds an empty `preserved`), `scan.audio_offset`, `scan.audio_length`, `&[]` tags, `&[]` binary, and the art slice. Read the test body first and keep its existing `at_limit`/`mk` logic; only the two `synthesize_layout(...)` calls change shape.

- [ ] **Step 5: Run the format tests**

Run: `cargo test -p musefs-format`
Expected: PASS (including the three new tests from Step 1 and `--features fuzzing` paths via the workspace; if `proptest_flac` is gated, also run `cargo test -p musefs-format --features fuzzing`).
Run: `cargo test -p musefs-format --features fuzzing --test proptest_flac`
Expected: PASS.

- [ ] **Step 6: Rewrite the reader `Format::Flac` arm**

In `musefs-core/src/reader.rs`, change the import on line `6` from:
```rust
use musefs_format::flac::{self, FlacScan};
```
to:
```rust
use musefs_format::flac::{self, MetadataBlock};
```

Replace the `Format::Flac =>` arm (`musefs-core/src/reader.rs:261-271`) with:

```rust
                    Format::Flac => {
                        // STREAMINFO/SEEKTABLE come from the structural store, so no
                        // backing-file read is needed for rescanned tracks. A FLAC
                        // track migrated to V2 but not yet rescanned has no structural
                        // rows: fall back to the front re-read and carry every
                        // preserved block (incl. APPLICATION/CUESHEET) inline, exactly
                        // as before this phase. The next scan backfills the stores.
                        let structural: Vec<MetadataBlock> = {
                            let rows = db.get_structural_blocks(track.id)?;
                            if rows.is_empty() {
                                let front = read_front(
                                    Path::new(&track.backing_path),
                                    track.audio_offset as u64,
                                )?;
                                flac::read_metadata(&front)?.preserved
                            } else {
                                rows.into_iter()
                                    .filter_map(|b| {
                                        flac::structural_block_type(&b.kind)
                                            .map(|block_type| MetadataBlock {
                                                block_type,
                                                body: b.body,
                                            })
                                    })
                                    .collect()
                            }
                        };
                        flac::synthesize_layout(
                            &structural,
                            track.audio_offset as u64,
                            track.audio_length as u64,
                            &inputs,
                            &binary_tag_inputs,
                            &art_inputs,
                        )?
                    }
```

`binary_tag_inputs` is already computed just above the `match` (`musefs-core/src/reader.rs:255`). On the legacy fallback path it is empty (no `value_blob` rows yet), and APPLICATION/CUESHEET ride through `structural`; on the rescanned path `structural` holds only STREAMINFO/SEEKTABLE and `binary_tag_inputs` carries APPLICATION/CUESHEET.

- [ ] **Step 7: Build and test the workspace**

Run: `cargo build --workspace --all-targets`
Expected: clean build (every FLAC resolve currently uses the legacy fallback — correct).
Run: `cargo test -p musefs-core`
Expected: PASS.

- [ ] **Step 8: Commit**

```bash
git add musefs-format/src/flac.rs musefs-format/tests/synthesize_tags.rs \
        musefs-format/tests/synthesize_art.rs musefs-format/tests/roundtrip.rs \
        musefs-format/tests/proptest_flac.rs musefs-core/src/reader.rs
git commit -m "feat(format,core): FLAC synthesis from structural store + streamed binary tags

flac::synthesize_layout drops FlacScan for structural blocks + audio bounds +
streamed APPLICATION/CUESHEET; reader loads structural_blocks from the DB with a
legacy front-read fallback when absent.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 4: Scan persists FLAC structural blocks + binary tags

Make scan split FLAC's preserved blocks into the structural store and binary tags, so rescanned tracks use the fast (zero-front-read) resolve path. Add `Probed.structural_blocks`, populate it (and `binary_tags`) in both FLAC probe arms via `split_preserved`, and persist structural blocks in `ingest`/`ingest_bulk`.

**Files:**
- Modify: `musefs-core/src/scan.rs` — `Probed` struct (`:83-92`); FLAC arms in `probe_full` (`:99-110`) and `probe_prefix` (`:266-273`); all other `Probed { … }` literals; `ingest` (`:374-437`) and `ingest_bulk` (`:439-504`); plus the two test-helper `Probed` literals
- Test: `musefs-core/tests/flac_binary_tags.rs` (new)

- [ ] **Step 1: Write the failing test (scan → DB persistence)**

Create `musefs-core/tests/flac_binary_tags.rs`:

```rust
mod common;
use common::{make_flac, streaminfo_body, vorbis_comment_body};
use musefs_core::scan_directory;

// Block types: STREAMINFO=0, APPLICATION=2, SEEKTABLE=3, VORBIS_COMMENT=4, CUESHEET=5.
fn fixture() -> Vec<u8> {
    let blocks = vec![
        (0u8, streaminfo_body()),
        (2u8, b"testAPPLICATION-PAYLOAD".to_vec()),
        (3u8, vec![0xEE; 36]), // SEEKTABLE
        (4u8, vorbis_comment_body("v", &["ARTIST=Alice", "TITLE=Song"])),
        (5u8, vec![0x11; 48]), // CUESHEET
    ];
    make_flac(&blocks, &vec![0xCD; 4096])
}

#[test]
fn scan_splits_flac_into_structural_store_and_binary_tags() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.flac"), fixture()).unwrap();

    let db = musefs_db::Db::open_in_memory().unwrap();
    scan_directory(&db, dir.path()).unwrap();

    // Exactly one track; fetch its id via the structural query over both tracks.
    let track_id = 1i64; // first upserted track

    let structural = db.get_structural_blocks(track_id).unwrap();
    let kinds: Vec<&str> = structural.iter().map(|b| b.kind.as_str()).collect();
    assert!(kinds.contains(&"STREAMINFO"));
    assert!(kinds.contains(&"SEEKTABLE"));
    assert_eq!(structural.len(), 2);

    let binary = db.get_binary_tags(track_id).unwrap();
    let bkeys: Vec<&str> = binary.iter().map(|b| b.key.as_str()).collect();
    assert!(bkeys.contains(&"APPLICATION"));
    assert!(bkeys.contains(&"CUESHEET"));
    assert_eq!(binary.len(), 2);
}
```

`musefs-core/tests/common/mod.rs` already exports `make_flac(blocks: &[(u8, Vec<u8>)], audio: &[u8])`, `streaminfo_body`, and `vorbis_comment_body` (used by `metrics.rs`) — the same helpers as the format-layer `common`. Use them directly as written above. `get_binary_tags` returns `Vec<BinaryTagRow>` whose field is `key` (`musefs-db/src/tags.rs:123`).

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p musefs-core --test flac_binary_tags scan_splits_flac_into_structural_store_and_binary_tags`
Expected: FAIL — `get_structural_blocks` returns empty / `binary` empty (scan does not yet populate them). If the test does not compile due to a missing `common` helper, fix the helper wiring first (Step 1 note) until it compiles and then fails on the assertions.

- [ ] **Step 3: Add `structural_blocks` to `Probed`**

In `musefs-core/src/scan.rs`, change the `Probed` struct (`:83-92`) to add a field:

```rust
pub(crate) struct Probed {
    format: Format,
    audio_offset: u64,
    audio_length: u64,
    tags: Vec<(String, String)>,
    pictures: Vec<EmbeddedPicture>,
    binary_tags: Vec<EmbeddedBinaryTag>,
    /// FLAC STREAMINFO/SEEKTABLE as (kind, body) pairs; empty for other formats.
    structural_blocks: Vec<(String, Vec<u8>)>,
}
```

- [ ] **Step 4: Populate the FLAC probe arms; add the field to every other literal**

`probe_full` FLAC arm (`:99-110`):
```rust
    if has_ext(path, "flac") {
        let scan = flac::locate_audio(bytes).ok()?;
        let (structural_blocks, binary_tags) = flac::split_preserved(&scan.preserved);
        Some(Probed {
            format: Format::Flac,
            audio_offset: scan.audio_offset,
            audio_length: scan.audio_length,
            tags: flac::read_vorbis_comments(bytes).unwrap_or_default(),
            pictures: flac::read_pictures(bytes).unwrap_or_default(),
            binary_tags,
            structural_blocks,
        })
    } else if has_ext(path, "mp3") {
```

`probe_prefix` FLAC arm (`:266-273`):
```rust
            Ok(Extent::Complete(meta)) => {
                let (structural_blocks, binary_tags) = flac::split_preserved(&meta.preserved);
                Probe::Done(Probed {
                    format: Format::Flac,
                    audio_offset: meta.audio_offset,
                    audio_length: file_len - meta.audio_offset,
                    tags: flac::read_vorbis_comments(prefix).unwrap_or_default(),
                    pictures: flac::read_pictures(prefix).unwrap_or_default(),
                    binary_tags,
                    structural_blocks,
                })
            }
```

Add `structural_blocks: Vec::new(),` to **every other** `Probed { … }` literal (non-FLAC formats never have structural blocks):
- `probe_full`: mp3 (`:113`), m4a (`:123`), ogg (`:138`), wav (`:151`)
- `probe_file`: m4a (`:216`)
- `probe_prefix`: mp3 (`:283`), ogg (`:303`), wav (`:321`)
- test helpers: `:903`, `:914`, `:1387` (the `probed_with_mixed_binary_tags` helper)

- [ ] **Step 5: Persist structural blocks in `ingest` and `ingest_bulk`**

In `ingest` (`musefs-core/src/scan.rs:374-437`), after the `db.set_binary_tags(track_id, &binary_tags)?;` line, add:

```rust
    let mut sb_ordinals: HashMap<String, i64> = HashMap::new();
    let structural_blocks: Vec<musefs_db::StructuralBlock> = probed
        .structural_blocks
        .into_iter()
        .map(|(kind, body)| {
            let ord = sb_ordinals.entry(kind.clone()).or_insert(0);
            let sb = musefs_db::StructuralBlock {
                kind,
                ordinal: *ord,
                body,
            };
            *ord += 1;
            sb
        })
        .collect();
    db.set_structural_blocks(track_id, &structural_blocks)?;
```

In `ingest_bulk` (`musefs-core/src/scan.rs:439-504`, which takes `probed: &Probed`), after `bw.set_binary_tags(track_id, &binary_tags)?;` add the by-reference form:

```rust
    let mut sb_ordinals: HashMap<String, i64> = HashMap::new();
    let structural_blocks: Vec<musefs_db::StructuralBlock> = probed
        .structural_blocks
        .iter()
        .map(|(kind, body)| {
            let ord = sb_ordinals.entry(kind.clone()).or_insert(0);
            let sb = musefs_db::StructuralBlock {
                kind: kind.clone(),
                ordinal: *ord,
                body: body.clone(),
            };
            *ord += 1;
            sb
        })
        .collect();
    bw.set_structural_blocks(track_id, &structural_blocks)?;
```

`HashMap` is already imported at `musefs-core/src/scan.rs:1`. `musefs_db::StructuralBlock` is referenced fully-qualified (matching the existing `musefs_db::BinaryTag` style); no import change needed.

- [ ] **Step 6: Run the persistence test**

Run: `cargo test -p musefs-core --test flac_binary_tags scan_splits_flac_into_structural_store_and_binary_tags`
Expected: PASS.

- [ ] **Step 7: Run the full core + scan suites**

Run: `cargo test -p musefs-core`
Expected: PASS (the `probe_full` vs bounded equivalence test still holds — both paths feed `split_preserved` the same `preserved` blocks).

- [ ] **Step 8: Commit**

```bash
git add musefs-core/src/scan.rs musefs-core/tests/flac_binary_tags.rs musefs-core/tests/common
git commit -m "feat(core): scan persists FLAC structural blocks + APPLICATION/CUESHEET binary tags

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 5: End-to-end serve, legacy fallback, and zero-front-read tests

Lock the full chain: a rescanned FLAC serves a valid file with binary tags streamed and zero backing-file reads at resolve; a legacy track (no structural rows) serves correctly via the front-read fallback.

**Files:**
- Modify: `musefs-core/tests/flac_binary_tags.rs` (add serve + legacy tests)
- Modify: `musefs-core/tests/metrics.rs` (add zero-front-read test)

- [ ] **Step 1: Write the end-to-end serve test**

Append to `musefs-core/tests/flac_binary_tags.rs`:

```rust
use musefs_core::{MountConfig, Musefs, VirtualTree};
use std::collections::BTreeMap;

fn config() -> MountConfig {
    MountConfig {
        template: "$artist/$title".to_string(),
        fallbacks: BTreeMap::new(),
        default_fallback: "Unknown".to_string(),
        mode: musefs_core::Mode::Synthesis,
        poll_interval: std::time::Duration::ZERO,
    }
}

fn read_whole(fs: &Musefs, inode: u64) -> Vec<u8> {
    let size = fs.getattr(inode).unwrap().size;
    let fh = fs.open_handle(inode).unwrap();
    let mut out = Vec::new();
    let mut off = 0u64;
    while off < size {
        let got = fs.read(inode, fh, off, 64 * 1024).unwrap();
        if got.is_empty() {
            break;
        }
        off += got.len() as u64;
        out.extend_from_slice(&got);
    }
    fs.release_handle(fh);
    out
}

#[test]
fn rescanned_flac_serves_valid_file_with_binary_tags() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.flac"), fixture()).unwrap();
    let db = musefs_db::Db::open_in_memory().unwrap();
    scan_directory(&db, dir.path()).unwrap();
    let fs = Musefs::open(db, config()).unwrap();

    let artist = fs.lookup(VirtualTree::ROOT, "Alice").unwrap();
    let (_, inode, _) = fs.readdir(artist).unwrap().into_iter().next().unwrap();
    let served = read_whole(&fs, inode);

    // Valid FLAC framing, and the APPLICATION block survived the round trip.
    let tag = metaflac::Tag::read_from(&mut std::io::Cursor::new(&served)).expect("valid FLAC");
    let (id, data) = tag
        .blocks()
        .find_map(|b| match b {
            metaflac::Block::Application(a) => Some((a.id, a.data.clone())),
            _ => None,
        })
        .expect("application block present");
    assert_eq!(&id, b"test");
    assert_eq!(data, b"APPLICATION-PAYLOAD");
}
```

`metaflac` is already in `musefs-core`'s `[dev-dependencies]` (`musefs-core/Cargo.toml:25`). `MountConfig`/`Musefs::read(inode, fh, offset, size)`/`open_handle`/`release_handle`/`getattr(...).size`/`lookup` are used exactly as in `musefs-core/tests/metrics.rs` (the canonical usage) and `facade.rs`.

- [ ] **Step 2: Write the legacy-fallback serve test**

Append:

```rust
use musefs_db::{Format, NewTrack, Tag};

#[test]
fn legacy_flac_without_structural_rows_serves_via_front_read_fallback() {
    let dir = tempfile::tempdir().unwrap();
    let bytes = fixture();
    let path = dir.path().join("legacy.flac");
    std::fs::write(&path, &bytes).unwrap();
    let meta = std::fs::metadata(&path).unwrap();

    // Simulate a V1-scanned track: a track row + text tags, but NO structural_blocks
    // and NO binary value_blob rows. Resolve must fall back to the front re-read.
    let scan = musefs_format::flac::locate_audio(&bytes).unwrap();
    let db = musefs_db::Db::open_in_memory().unwrap();
    let id = db
        .upsert_track(&NewTrack {
            backing_path: path.to_string_lossy().into_owned(),
            format: Format::Flac,
            audio_offset: scan.audio_offset as i64,
            audio_length: scan.audio_length as i64,
            backing_size: meta.len() as i64,
            backing_mtime: meta
                .modified()
                .unwrap()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs() as i64,
        })
        .unwrap();
    // Lowercase keys are intentional: synthesis regenerates VORBIS_COMMENT from
    // these DB tags, not from the file's embedded `ARTIST=...` block (VORBIS_COMMENT
    // is type 4 and never enters the preserved/structural set). Not a casing bug.
    db.replace_tags(
        id,
        &[Tag::new("artist", "Alice", 0), Tag::new("title", "Song", 0)],
    )
    .unwrap();
    assert!(db.get_structural_blocks(id).unwrap().is_empty());

    let fs = Musefs::open(db, config()).unwrap();
    let artist = fs.lookup(VirtualTree::ROOT, "Alice").unwrap();
    let (_, inode, _) = fs.readdir(artist).unwrap().into_iter().next().unwrap();
    let served = read_whole(&fs, inode);

    // The legacy path carries every preserved block inline, including APPLICATION.
    let tag = metaflac::Tag::read_from(&mut std::io::Cursor::new(&served)).expect("valid FLAC");
    assert!(tag
        .blocks()
        .any(|b| matches!(b, metaflac::Block::Application(_))));
}
```

Confirm `backing_mtime` matches what scan stores (`mtime_secs` in `scan.rs` computes seconds since the epoch); if the reader's backing-validation compares against a different unit, mirror `scan::mtime_secs` exactly to avoid a spurious `BackingChanged`.

- [ ] **Step 3: Run the serve + legacy tests**

Run: `cargo test -p musefs-core --test flac_binary_tags`
Expected: PASS (all four tests).

- [ ] **Step 4: Add the zero-front-read metrics test**

Append to `musefs-core/tests/metrics.rs` (already `#![cfg(feature = "metrics")]`; reuse `METRICS_LOCK`, `config`, and the `common` helpers):

```rust
#[test]
fn rescanned_flac_resolve_does_no_front_read() {
    let _guard = METRICS_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let dir = tempfile::tempdir().unwrap();
    let flac = make_flac(
        &[
            (0, streaminfo_body()),
            (2, b"testAPPDATA".to_vec()), // APPLICATION -> stored as a binary tag
            (4, vorbis_comment_body("v", &["ARTIST=Alice", "TITLE=Song"])),
        ],
        &vec![0xCD_u8; 64 * 1024],
    );
    std::fs::write(dir.path().join("a.flac"), &flac).unwrap();

    let db = musefs_db::Db::open_in_memory().unwrap();
    scan_directory(&db, dir.path()).unwrap();
    let fs = Musefs::open(db, config()).unwrap();

    let artist = fs.lookup(VirtualTree::ROOT, "Alice").unwrap();
    let (_, inode, _) = fs.readdir(artist).unwrap().into_iter().next().unwrap();

    // Cold cache: this open_handle forces a resolve. For a rescanned FLAC the
    // structural store supplies STREAMINFO/SEEKTABLE, so the only open() is the
    // handle's read fd — NOT a synthesis front re-read (which would make it 2).
    metrics::reset();
    let fh = fs.open_handle(inode).unwrap();
    let s = metrics::snapshot();
    fs.release_handle(fh);
    assert_eq!(
        s.opens, 1,
        "rescanned FLAC resolve must not re-read the backing front"
    );
}
```

The `common` import at `musefs-core/tests/metrics.rs:4` already brings in `make_flac` (it accepts a `(u8, Vec<u8>)` block list — `baseline_one_open_per_read_call` uses it), `streaminfo_body`, `vorbis_comment_body`. `open_handle` opens the read fd eagerly (`facade.rs:698`: `cache.resolve(...)` then `metrics::on_open()` + `File::open`), so a rescanned FLAC yields exactly 1 open (the fd) and a legacy track would yield 2 (front read + fd). The `s.opens == 1` assertion is correct as written.

- [ ] **Step 5: Run the metrics test**

Run: `cargo test -p musefs-core --features metrics --test metrics rescanned_flac_resolve_does_no_front_read`
Expected: PASS.

- [ ] **Step 6: Full workspace verification**

Run: `cargo test --workspace`
Run: `cargo test -p musefs-format --features fuzzing`
Run: `cargo clippy --all-targets`
Run: `cargo fmt --all --check`
Expected: all green / no diff.

- [ ] **Step 7: Commit**

```bash
git add musefs-core/tests/flac_binary_tags.rs musefs-core/tests/metrics.rs
git commit -m "test(core): FLAC binary-tag serve, legacy front-read fallback, zero-front-read resolve

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Self-Review (verification against the spec §3–§7, Phase 4)

**Spec coverage:**
- §5 FLAC parse split (STREAMINFO/SEEKTABLE → structural; APPLICATION/CUESHEET → binary) → Task 2 (`split_preserved`) + Task 4 (scan persistence).
- §5 `synthesize_layout` signature change (FlacScan → structural blocks + binary inputs) + fixed canonical order + is-last rule → Task 3.
- §5 APPLICATION/CUESHEET bounded by the 24-bit FLAC block guard → Task 3 (`bt.len > 0x00FF_FFFF`), tested by `binary_tag_over_24bit_limit_errors`.
- §1 migration backfill + front-read fallback when `structural_blocks` empty → Task 3 reader arm; tested by `legacy_flac_without_structural_rows_serves_via_front_read_fallback`. Backfill on rescan: a plain `scan` re-ingests every file (upsert), populating both stores; tested implicitly by the persistence + serve tests.
- §7 FLAC resolve re-read elimination (zero front reads for rescanned tracks) → Task 3 + Task 4; tested by `rescanned_flac_resolve_does_no_front_read`.
- §7 deletion/refactor fallout: `Format::Flac` arm stops unconditionally constructing a `FlacScan` from `read_front` (Task 3); `read_front` retained (still called by the legacy fallback + WAV/Ogg); `synthesize_layout` caller/test fallout enumerated (Task 3, Step 4).
- §Testing FLAC round-trip locking APPLICATION/CUESHEET + structural store → Task 3 (format round-trip) + Task 5 (serve). Block-reorder-tolerant assertions (payload fidelity, `locate_audio` framing), not byte-identical ordering.
- §Error handling: no new `CoreError`/`FormatError` variant; reuses `TooLarge`/`InvalidLayout` + the art short-read error. The FUSE `errno()` match is untouched. ✔

**Out of scope (correctly omitted):** Ogg (no binary gap), any payload interpretation, `.cue` ingestion, `facade.rs` open-handle guard (closed in Phase 2, already covers `has_binary_tag`).

**Type/signature consistency:** `synthesize_layout(structural: &[MetadataBlock], audio_offset: u64, audio_length: u64, tags: &[TagInput], binary_tags: &[BinaryTagInput], arts: &[ArtInput])` — used identically in the reader arm, all four test files, and the in-file boundary test. `split_preserved(&[MetadataBlock]) -> (Vec<(String, Vec<u8>)>, Vec<EmbeddedBinaryTag>)` and `structural_block_type(&str) -> Option<u8>` match their call sites in scan + reader. `Probed.structural_blocks: Vec<(String, Vec<u8>)>` matches the `ingest`/`ingest_bulk` consumers.

**Assumptions to verify during execution (the plan flags each inline):** the core `tests/common` `make_flac` block-list helper and `metaflac` dev-dep availability for `musefs-core`; exact `Musefs::read`/`open_handle` argument order and open-accounting (mirror `metrics.rs`); `BinaryTagRow.key` field name; `backing_mtime` unit parity with `scan::mtime_secs`.
