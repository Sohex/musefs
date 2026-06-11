# Ogg Artwork Streaming + Shared Art Cap Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Stop Ogg synthesis from materializing whole artwork images (and their base64 copies) when building header pages, and enforce one artwork-size ceiling at resolve time for every format.

**Architecture:** Two independent components. (A) Promote the scanner's `MAX_ART_BYTES` to a resolve-time invariant enforced in `track_art_to_inputs`, with a diagnosable log. (B) Stream Ogg art end-to-end: a format-layer `ArtSource` trait (DB-backed in core, in-memory for tests/fuzz), an incremental page-CRC fed in page-bounded windows, and `PayloadChunk::Art`/`OggArt` carrying metadata only. Served bytes are byte-identical to today; only synthesis-time memory changes (from ~2.3× the image to O(one Ogg page)).

**Tech Stack:** Rust workspace (`musefs-db` → `musefs-format` → `musefs-core`), SQLite incremental blob I/O, the `log` crate, `thiserror`, libFuzzer (`fuzz/`, out of workspace).

**Spec:** `docs/superpowers/specs/2026-06-11-ogg-art-streaming-design.md`

**Tooling note:** This repo uses the Serena MCP semantic tools as the primary way to read/edit Rust. Prefer `get_symbols_overview`/`find_symbol`/`replace_symbol_body`/`insert_after_symbol` over plain Read/Edit on `.rs` files. The pre-commit hook runs fmt + clippy (`-D warnings`) + the **full workspace test suite**, so every commit must compile and pass green. The `fuzz/` crate is outside the workspace; verify it separately with `cargo +nightly fuzz build ogg`.

---

## File Structure

| File | Change | Responsibility |
| ---- | ------ | -------------- |
| `musefs-core/src/scan.rs` | modify | make `MAX_ART_BYTES` `pub(crate)` (shared ceiling) |
| `musefs-core/src/error.rs` | modify | add `CoreError::ArtTooLarge` |
| `musefs-core/src/mapping.rs` | modify | enforce cap in `track_art_to_inputs`; add `DbArtSource`; delete `track_art_images` |
| `musefs-format/src/ogg/crc.rs` | modify | add `crc32_update` (incremental CRC) |
| `musefs-format/src/error.rs` | modify | add `FormatError::ArtRead { art_id }` |
| `musefs-format/src/ogg/art_source.rs` | create | `ArtSource` trait + in-memory `MapArtSource` |
| `musefs-format/src/ogg/mod.rs` | modify | `OggArt` loses `image`; producers stop materializing; `synthesize_layout` takes `&dyn ArtSource` |
| `musefs-format/src/ogg/page.rs` | modify | `PayloadChunk::Art` loses `out`; CRC pass streams from `ArtSource` |
| `musefs-core/src/reader.rs` | modify | Ogg branch builds metadata-only `OggArt` + `DbArtSource`; drop blob pre-read |
| `musefs-core/src/ogg_index.rs` | modify | zero-art `synthesize_layout` call gets a source arg |
| `musefs-format/tests/proptest_ogg.rs` | modify | zero-art `synthesize_layout` call gets a source arg |
| `fuzz/fuzz_targets/ogg.rs` | modify | feed art through `MapArtSource`; couple image length to `data_len` |
| `docs/OGG.md`, `ARCHITECTURE.md` | modify | document the streamed invariant + shared cap |

---

## Component A — Shared resolve-time art cap

### Task 1: Enforce `MAX_ART_BYTES` at resolve for all formats

**Files:**
- Modify: `musefs-core/src/scan.rs:31` (visibility)
- Modify: `musefs-core/src/error.rs:4-47` (new variant)
- Modify: `musefs-core/src/mapping.rs` (`track_art_to_inputs`, new test)

- [ ] **Step 1: Make the cap constant shareable**

In `musefs-core/src/scan.rs`, change the constant at line 31 from private to crate-visible. Current:

```rust
const MAX_ART_BYTES: usize = 16 * 1024 * 1024 - 64 * 1024;
```

New (add a doc line noting it is now the shared ceiling):

```rust
/// The artwork-size ceiling. Enforced here at ingest (oversize scanned art is
/// dropped) and at resolve in `mapping::track_art_to_inputs` (oversize art from
/// any writer is rejected). Sized to clear FLAC's 24-bit block length with
/// headroom for the picture-block framing.
pub(crate) const MAX_ART_BYTES: usize = 16 * 1024 * 1024 - 64 * 1024;
```

- [ ] **Step 2: Add the `ArtTooLarge` error variant**

In `musefs-core/src/error.rs`, add a variant to `CoreError` (after `InvalidPictureType`, before `TrackNotFound`):

```rust
    #[error(
        "track {track_id} art {art_id} is {byte_len} bytes, exceeds the {cap}-byte art cap"
    )]
    ArtTooLarge {
        track_id: i64,
        art_id: i64,
        byte_len: u64,
        cap: u64,
    },
```

- [ ] **Step 3: Write the failing boundary test**

In `musefs-core/src/mapping.rs`, inside `mod tests`, add this test. It inserts art blobs at exactly the cap (accepted) and one byte over (rejected). `upsert_art` derives `byte_len` from the blob length, and the schema enforces `byte_len = length(data)`, so the blob size *is* the tested `data_len`.

```rust
    #[test]
    fn track_art_to_inputs_enforces_art_cap() {
        use musefs_db::{NewArt, TrackArt};
        let cap = crate::scan::MAX_ART_BYTES;
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(dir.path().join("cap.db")).unwrap();
        let tid = db
            .upsert_track(&NewTrack {
                backing_path: "/a.opus".into(),
                format: Format::Opus,
                audio_offset: 0,
                audio_length: 0,
                backing_size: 0,
                backing_mtime: 0,
            })
            .unwrap();

        // Exactly at the cap: accepted.
        let at_cap = db
            .upsert_art(&NewArt {
                mime: "image/png".into(),
                width: None,
                height: None,
                data: vec![0u8; cap],
            })
            .unwrap();
        db.set_track_art(
            tid,
            &[TrackArt { art_id: at_cap, picture_type: 3, description: String::new(), ordinal: 0 }],
        )
        .unwrap();
        let ok = super::track_art_to_inputs(&db, tid).unwrap();
        assert_eq!(ok.len(), 1, "art exactly at the cap must be accepted");

        // One byte over the cap: rejected with ArtTooLarge naming the offending ids.
        let over = db
            .upsert_art(&NewArt {
                mime: "image/png".into(),
                width: None,
                height: None,
                data: vec![0u8; cap + 1],
            })
            .unwrap();
        db.set_track_art(
            tid,
            &[TrackArt { art_id: over, picture_type: 3, description: String::new(), ordinal: 0 }],
        )
        .unwrap();
        let err = super::track_art_to_inputs(&db, tid).unwrap_err();
        assert!(
            matches!(
                err,
                CoreError::ArtTooLarge { track_id, art_id, byte_len, cap: c }
                    if track_id == tid && art_id == over
                        && byte_len == (cap as u64) + 1 && c == cap as u64
            ),
            "oversize art must yield ArtTooLarge with the offending ids, got {err:?}"
        );
    }
```

- [ ] **Step 4: Run the test, verify it fails to compile / fail**

Run: `cargo test -p musefs-core track_art_to_inputs_enforces_art_cap`
Expected: FAIL — `MAX_ART_BYTES` not visible and/or `ArtTooLarge` unknown, then (once those compile) the rejection assertion fails because no cap is enforced yet.

- [ ] **Step 5: Enforce the cap in `track_art_to_inputs`**

In `musefs-core/src/mapping.rs`, in `track_art_to_inputs`, after the `data_len` is derived (the `let Some(data_len) = ... else { continue; }` block) and before `inputs.push(ArtInput { ... })`, insert the cap check:

```rust
        if data_len.get() > crate::scan::MAX_ART_BYTES as u64 {
            log::warn!(
                "track {track_id} art {} is {} bytes, exceeds the {}-byte art cap; refusing to serve",
                ta.art_id,
                data_len.get(),
                crate::scan::MAX_ART_BYTES,
            );
            return Err(crate::error::CoreError::ArtTooLarge {
                track_id,
                art_id: ta.art_id,
                byte_len: data_len.get(),
                cap: crate::scan::MAX_ART_BYTES as u64,
            });
        }
```

- [ ] **Step 6: Run the test, verify it passes**

Run: `cargo test -p musefs-core track_art_to_inputs_enforces_art_cap`
Expected: PASS

- [ ] **Step 7: Run the full crate suite + clippy**

Run: `cargo test -p musefs-core && cargo clippy --all-targets -- -D warnings`
Expected: PASS (no warnings). The `as u64`/`as i64` casts mirror the existing style; if clippy flags a cast, use the crate's `convert` helper as neighboring code does.

- [ ] **Step 8: Commit**

```bash
git add musefs-core/src/scan.rs musefs-core/src/error.rs musefs-core/src/mapping.rs
git commit -m "$(cat <<'EOF'
feat(core): enforce MAX_ART_BYTES at resolve for all formats (#266)

Promote the scanner's art ceiling to a resolve-time invariant in
track_art_to_inputs, rejecting oversize art (from any writer, not just the
scanner) with a logged CoreError::ArtTooLarge. Closes the external-writer
bypass uniformly; Ogg streaming follows.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Component B — Stream Ogg art end-to-end

### Task 2: Incremental CRC (`crc32_update`)

**Files:**
- Modify: `musefs-format/src/ogg/crc.rs` (`crc32`, new `crc32_update`, new test)

- [ ] **Step 1: Write the failing test**

In `musefs-format/src/ogg/crc.rs`, inside `mod tests`, add:

```rust
    #[test]
    fn crc32_update_matches_oneshot_across_a_split() {
        let data: Vec<u8> = (0..1000u32).map(|i| (i % 251) as u8).collect();
        for split in [0usize, 1, 3, 255, 256, 700, 1000] {
            let (a, b) = data.split_at(split);
            let streamed = super::crc32_update(super::crc32_update(0, a), b);
            assert_eq!(streamed, super::crc32(&data), "split at {split}");
        }
    }
```

- [ ] **Step 2: Run the test, verify it fails**

Run: `cargo test -p musefs-format crc32_update_matches_oneshot_across_a_split`
Expected: FAIL — `crc32_update` not defined.

- [ ] **Step 3: Add `crc32_update` and refactor `crc32` to use it**

In `musefs-format/src/ogg/crc.rs`, replace the body of `crc32` and add `crc32_update`. The Ogg CRC starts at state 0 with no input/output inversion, so feeding `0` as the initial state reproduces a one-shot CRC and the split identity holds.

```rust
/// Fold `buf` into a running CRC `state`. `crc32(x) == crc32_update(0, x)` and
/// `crc32(a ++ b) == crc32_update(crc32_update(0, &a), &b)`, so a page CRC can be
/// computed in bounded windows without assembling the whole page.
pub(crate) fn crc32_update(mut state: u32, buf: &[u8]) -> u32 {
    for &b in buf {
        state = (state << 8) ^ TABLE[(((state >> 24) as u8) ^ b) as usize];
    }
    state
}

pub fn crc32(buf: &[u8]) -> u32 {
    crc32_update(0, buf)
}
```

- [ ] **Step 4: Run the test + existing CRC tests, verify they pass**

Run: `cargo test -p musefs-format ogg::crc`
Expected: PASS (the existing `crc32` reference tests and the new split test).

- [ ] **Step 5: Commit**

```bash
git add musefs-format/src/ogg/crc.rs
git commit -m "$(cat <<'EOF'
feat(ogg): add incremental crc32_update for streamed page CRCs (#266)

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

### Task 3: The streaming refactor (single atomic commit)

This task changes `PayloadChunk::Art` and `OggArt` to drop their byte buffers, so producers, consumers, and all callers must change together — Rust will not compile an intermediate state. Build it up in the ordered steps below, then compile, lint, test, and fuzz-build once, then commit once. Do **not** commit between sub-steps; the tree does not compile until Step 13.

**Files:** `musefs-format/src/error.rs`, `musefs-format/src/ogg/art_source.rs` (new), `musefs-format/src/ogg/mod.rs`, `musefs-format/src/ogg/page.rs`, `musefs-core/src/mapping.rs`, `musefs-core/src/reader.rs`, `musefs-core/src/ogg_index.rs`, `musefs-format/tests/proptest_ogg.rs`, `fuzz/fuzz_targets/ogg.rs`.

- [ ] **Step 1: Add the `FormatError::ArtRead` variant**

In `musefs-format/src/error.rs`, add to `FormatError` (it derives `PartialEq, Eq`; the `i64` payload keeps the derive valid):

```rust
    #[error("failed to read art {art_id} bytes for synthesis")]
    ArtRead { art_id: i64 },
```

- [ ] **Step 2: Create the `ArtSource` trait + in-memory `MapArtSource`**

Create `musefs-format/src/ogg/art_source.rs`:

```rust
use crate::error::{FormatError, Result};
use std::collections::HashMap;

/// A source of raw art bytes used during synthesis to compute page CRCs. The
/// production implementation (in `musefs-core`) streams from the SQLite blob
/// store; tests and fuzzing use [`MapArtSource`]. `offset` and `buf.len()` are in
/// raw-image coordinates; a read past the stored image is an error (mirrors the
/// short-read semantics of the DB blob path), surfaced as `FormatError::ArtRead`.
pub trait ArtSource {
    fn read_window(&self, art_id: i64, offset: u64, buf: &mut [u8]) -> Result<()>;
}

/// In-memory `ArtSource` over an `art_id -> image bytes` map. For tests/fuzz.
#[derive(Default)]
pub struct MapArtSource {
    images: HashMap<i64, Vec<u8>>,
}

impl MapArtSource {
    pub fn new(images: impl IntoIterator<Item = (i64, Vec<u8>)>) -> Self {
        Self { images: images.into_iter().collect() }
    }
}

impl ArtSource for MapArtSource {
    fn read_window(&self, art_id: i64, offset: u64, buf: &mut [u8]) -> Result<()> {
        let img = self.images.get(&art_id).ok_or(FormatError::ArtRead { art_id })?;
        let start = crate::convert::usize_from(offset);
        let end = start
            .checked_add(buf.len())
            .filter(|&e| e <= img.len())
            .ok_or(FormatError::ArtRead { art_id })?;
        buf.copy_from_slice(&img[start..end]);
        Ok(())
    }
}
```

In `musefs-format/src/ogg/mod.rs`, register the module and re-export the public items. Add near the other `mod` declarations at the top of the file:

```rust
mod art_source;
pub use art_source::{ArtSource, MapArtSource};
```

- [ ] **Step 3: Shrink `OggArt` to metadata only**

In `musefs-format/src/ogg/mod.rs`, replace the `OggArt` struct (currently `{ meta, image }`):

```rust
/// One image to embed: its metadata. Bytes are read from an `ArtSource` only to
/// compute page CRCs at synthesis time; they are never retained in the layout.
#[derive(Clone, Copy)]
pub struct OggArt<'a> {
    pub meta: &'a crate::input::ArtInput,
}
```

- [ ] **Step 4: Shrink `PayloadChunk::Art` and compute `out_len` from metadata**

In `musefs-format/src/ogg/page.rs`, replace the `PayloadChunk` enum's `Art` variant and the `out_len` method:

```rust
/// One span of a packet's payload during chunk-aware lacing.
pub(crate) enum PayloadChunk {
    /// Literal bytes copied verbatim into the layout as `Inline`.
    Bytes(Vec<u8>),
    /// An art run. Carries no bytes: its OUTPUT length is derived from `art_total`
    /// (base64-expanded when `base64`), and its bytes are streamed from an
    /// `ArtSource` to compute page CRCs, then never stored — the layout keeps an
    /// `OggArtSlice` referencing `art_id`. `art_total` is the raw image length.
    Art { art_id: i64, base64: bool, art_total: u64 },
}

impl PayloadChunk {
    fn out_len(&self) -> usize {
        match self {
            PayloadChunk::Bytes(b) => b.len(),
            PayloadChunk::Art { base64, art_total, .. } => {
                let n = if *base64 {
                    crate::ogg::b64::b64_len(*art_total)
                } else {
                    *art_total
                };
                crate::convert::usize_from(n)
            }
        }
    }
}
```

- [ ] **Step 5: Replace `copy_payload` with a streaming CRC feeder**

In `musefs-format/src/ogg/page.rs`, delete `copy_payload` and add these two functions. They feed page bytes into a running CRC, reading art from the `ArtSource` in page-bounded windows (output span per page ≤ 255×255, so each buffer is ≤ ~64 KiB).

First update the imports at the top of `page.rs`. The file already has `use super::crc::{crc_shift_zeros, crc32};` — add `crc32_update` to it and import the trait:

```rust
use super::art_source::ArtSource;
use super::crc::{crc_shift_zeros, crc32, crc32_update};
```

Then add the two functions:

```rust
/// Fold payload bytes `[p0, p0+plen)` (packet-payload coordinates) into `crc`,
/// reading art runs from `src` instead of materializing them.
fn crc_feed_payload(
    crc: &mut u32,
    chunks: &[PayloadChunk],
    src: &dyn ArtSource,
    p0: usize,
    plen: usize,
) -> crate::error::Result<()> {
    let end = p0 + plen;
    let mut cs = 0usize;
    for c in chunks {
        let ce = cs + c.out_len();
        let os = p0.max(cs);
        let oe = end.min(ce);
        if os < oe {
            match c {
                PayloadChunk::Bytes(b) => {
                    *crc = crc32_update(*crc, &b[os - cs..oe - cs]);
                }
                PayloadChunk::Art { art_id, base64, art_total } => {
                    crc_feed_art(crc, src, *art_id, *base64, *art_total, (os - cs) as u64, oe - os)?;
                }
            }
        }
        cs = ce;
    }
    Ok(())
}

/// Fold one art window — output bytes `[out_off, out_off+out_len)` of the run —
/// into `crc`, base64-encoding on the fly when `base64`. `out_len` is page-bounded.
fn crc_feed_art(
    crc: &mut u32,
    src: &dyn ArtSource,
    art_id: i64,
    base64: bool,
    art_total: u64,
    out_off: u64,
    out_len: usize,
) -> crate::error::Result<()> {
    if base64 {
        let w = crate::ogg::b64::b64_window(out_off, out_len as u64, art_total);
        let mut raw = vec![0u8; crate::convert::usize_from(w.in_len)];
        src.read_window(art_id, w.in_start, &mut raw)?;
        let enc = crate::ogg::b64::encode_b64_slice(&raw, w.skip, out_len);
        *crc = crc32_update(*crc, &enc);
    } else {
        let mut raw = vec![0u8; out_len];
        src.read_window(art_id, out_off, &mut raw)?;
        *crc = crc32_update(*crc, &raw);
    }
    Ok(())
}
```

- [ ] **Step 6: Thread `ArtSource` through `lace_chunks_to_segments` and stream the page CRC**

In `musefs-format/src/ogg/page.rs`, replace `lace_chunks_to_segments`. It now takes `src`, returns a `Result`, and builds only the page header+table (not the payload) before streaming the CRC:

```rust
/// Lace one packet (a chunk list) into pages from sequence `seq_start`, emitting
/// layout segments: page headers + literal payload as `Inline` (CRCs baked in),
/// art runs as `OggArtSlice` (no bytes stored). Art bytes are streamed from `src`
/// only to compute page CRCs. Returns `(segments, pages_used)`.
pub(crate) fn lace_chunks_to_segments(
    serial: u32,
    seq_start: u32,
    bos: bool,
    chunks: &[PayloadChunk],
    src: &dyn ArtSource,
) -> crate::error::Result<(Vec<Segment>, u32)> {
    let total: usize = chunks.iter().map(PayloadChunk::out_len).sum();
    let laces = lacing_values(total);

    let mut segments: Vec<Segment> = Vec::new();
    let mut seq = seq_start;
    let mut lace_pos = 0usize;
    let mut payload_pos = 0usize;
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

        // Build the page header + lacing table only (CRC field zeroed), then stream
        // the page CRC over header+payload without materializing the payload.
        let header_len = 27 + seg_count;
        let mut header = Vec::with_capacity(header_len);
        header.extend_from_slice(CAPTURE);
        header.push(0);
        header.push(header_type);
        header.extend_from_slice(&0u64.to_le_bytes()); // granule 0 (header page)
        header.extend_from_slice(&serial.to_le_bytes());
        header.extend_from_slice(&seq.to_le_bytes());
        header.extend_from_slice(&0u32.to_le_bytes()); // CRC placeholder
        header.push(u8::try_from(seg_count).expect("seg_count is .min(255) so fits in u8"));
        header.extend_from_slice(table);

        let mut crc = crc32_update(0, &header);
        crc_feed_payload(&mut crc, chunks, src, payload_pos, page_payload)?;
        header[22..26].copy_from_slice(&crc.to_le_bytes());

        emit_segments(&mut segments, &header, chunks, payload_pos, page_payload);

        payload_pos += page_payload;
        lace_pos += seg_count;
        seq += 1;
        first = false;
    }
    Ok((segments, seq - seq_start))
}
```

(`emit_segments` is unchanged — it already reads only `art_id`/`base64`/`art_total`, never bytes.)

- [ ] **Step 7: Stop materializing art in the packet producers**

In `musefs-format/src/ogg/mod.rs`, in `comment_packet_chunks`, replace the `PayloadChunk::Art` it pushes (the `out: b64_encode(art.image)` form):

```rust
        chunks.push(PayloadChunk::Art {
            art_id: art.meta.art_id,
            base64: true,
            art_total: art.meta.data_len.get(),
        });
```

In `oggflac_packets_with_art`, replace its `PayloadChunk::Art` (the `out: art.image.to_vec()` form):

```rust
            PayloadChunk::Art {
                art_id: art.meta.art_id,
                base64: false,
                art_total: art.meta.data_len.get(),
            },
```

- [ ] **Step 8: Thread `ArtSource` through `synthesize_layout`**

In `musefs-format/src/ogg/mod.rs`, update `synthesize_layout`'s signature and its lacing loop:

```rust
pub fn synthesize_layout(
    header: &OggHeader,
    audio_offset: u64,
    audio_length: u64,
    tags: &[TagInput],
    arts: &[OggArt],
    src: &dyn ArtSource,
) -> Result<RegionLayout> {
    let arts: Vec<OggArt> = arts.to_vec();
    let packet_chunks = build_packets_with_art(header, tags, &arts)?;
    let mut segments: Vec<Segment> = Vec::new();
    let mut seq = 0u32;
    for (i, chunks) in packet_chunks.iter().enumerate() {
        let (segs, used) =
            crate::ogg::page::lace_chunks_to_segments(header.serial, seq, i == 0, chunks, src)?;
        segments.extend(segs);
        seq += used;
    }
    let seq_delta = i64::from(seq) - i64::from(header.header_pages);
    segments.push(Segment::OggAudio {
        offset: audio_offset,
        len: audio_length,
        seq_delta,
    });
    Ok(RegionLayout::validated(segments)?)
}
```

`build_packets_with_art` keeps its signature (it consumes only `art.meta`); no change beyond Step 7's producer edits.

- [ ] **Step 9: Add the core `DbArtSource` and delete `track_art_images`**

In `musefs-core/src/mapping.rs`, delete `track_art_images` (the `pub(crate) fn track_art_images<M>(...)` at ~line 90) and its test `track_art_images_reads_stored_blob_bytes` (~line 412). Add the DB-backed source (it logs the typed `DbError` at the failure point, then returns the Eq-friendly `ArtRead { art_id }`):

```rust
/// `ArtSource` over the SQLite blob store, used by Ogg synthesis to stream art
/// bytes for page CRCs. Read failures (e.g. a deleted/short blob) are logged with
/// the underlying DB error and surfaced as `FormatError::ArtRead`.
pub(crate) struct DbArtSource<'a, M>(pub &'a Db<M>);

impl<M> musefs_format::ogg::ArtSource for DbArtSource<'_, M> {
    fn read_window(&self, art_id: i64, offset: u64, buf: &mut [u8]) -> musefs_format::Result<()> {
        self.0.read_art_chunk_into(art_id, offset, buf).map_err(|e| {
            log::warn!("ogg synthesis: art {art_id} read failed at offset {offset}: {e}");
            musefs_format::FormatError::ArtRead { art_id }
        })
    }
}
```

- [ ] **Step 10: Update the `reader.rs` Ogg branch**

In `musefs-core/src/reader.rs`, in `HeaderCache::build`, replace the `Format::Opus | Format::Vorbis | Format::OggFlac` arm body (currently reads `art_images` via `track_art_images` and zips into `OggArt { meta, image }`):

```rust
                    Format::Opus | Format::Vorbis | Format::OggFlac => {
                        let front = read_front(
                            Path::new(&track.backing_path),
                            track.bounds.audio_offset(),
                        )?;
                        let header = musefs_format::ogg::read_metadata(&front)?;
                        let arts: Vec<musefs_format::ogg::OggArt> = art_inputs
                            .iter()
                            .map(|meta| musefs_format::ogg::OggArt { meta })
                            .collect();
                        let src = crate::mapping::DbArtSource(db);
                        musefs_format::ogg::synthesize_layout(
                            &header,
                            track.bounds.audio_offset(),
                            track.bounds.audio_length(),
                            &inputs,
                            &arts,
                            &src,
                        )?
                    }
```

- [ ] **Step 11: Update the in-format and zero-art callers**

`musefs-core/src/ogg_index.rs:339` — update the zero-art call to pass an empty source:

```rust
            synthesize_layout(
                &header,
                scan.audio_offset,
                scan.audio_length,
                &[],
                &[],
                &musefs_format::ogg::MapArtSource::default(),
            )
            .unwrap();
```

(Confirm the import path of `synthesize_layout` in that file; if it is `use musefs_format::ogg::synthesize_layout`, the `MapArtSource` path above is correct.)

`musefs-format/tests/proptest_ogg.rs:18` — pass an empty source:

```rust
            ogg::synthesize_layout(
                &header,
                scan.audio_offset,
                scan.audio_length,
                &taginputs,
                &[],
                &ogg::MapArtSource::default(),
            )
```

- [ ] **Step 12: Migrate the format-crate tests in `mod.rs` and `page.rs`**

Apply this mechanical rule everywhere a test constructs `OggArt`:

- `OggArt { meta: &m, image: <anything> }` → `OggArt { meta: &m }` (drop the `image` field).

For each test that calls `synthesize_layout(...)` with non-empty `arts`, add a trailing `src` argument built from the same images the test verifies against:

```rust
        let src = musefs_format::ogg::MapArtSource::new([(meta.art_id, image.clone())]);
        // ...
        let layout = synthesize_layout(
            &header, scan.audio_offset, scan.audio_length, &tags, &[OggArt { meta: &meta }], &src,
        ).unwrap();
```

(Inside the `ogg` module use `MapArtSource` directly, not the `musefs_format::ogg::` prefix.)

Sites to update (verified by grep; `synthesize_layout` callers in `ogg/mod.rs` tests at approx. lines 618, 725, 857, 913, 1022, 1053, 1085 and the `OggArt` constructions at 918, 1027, 1058, 1091, 1095, 1123, 1151, 1186, 1320, 1327). Tests calling `build_packets_with_art` directly (the `oversized_full_art_value_rejected_by_build_packets`, `sum_overflow_art_value_rejected_by_build_packets`, `art_value_at_u32_max_boundary_is_accepted_by_build_packets` at ~1110-1200, and ~1320) only need the `image` field dropped — `build_packets_with_art` takes no source and reads no bytes.

Then rewrite the lacer test in `page.rs` (`chunk_lacer_splits_art_across_pages_and_crcs_validate`, ~line 585) so the art is byte-consistent (image length == `art_total`) and base64 output spans pages. Replace its `chunks`/setup and the `lace_chunks_to_segments` call:

```rust
        let head = vec![0xA0u8; 50];
        // 60_000 raw bytes -> b64 output ~80_000 > one page (65025), so it spans pages.
        let image: Vec<u8> = (0..60_000u32).map(|i| (i % 251) as u8).collect();
        let art_out = crate::ogg::b64::encode_b64_slice(
            &image, 0, crate::convert::usize_from(crate::ogg::b64::b64_len(image.len() as u64)),
        );
        let tail = vec![0xB0u8; 10];
        let chunks = vec![
            PayloadChunk::Bytes(head.clone()),
            PayloadChunk::Art { art_id: 42, base64: true, art_total: image.len() as u64 },
            PayloadChunk::Bytes(tail.clone()),
        ];
        let src = crate::ogg::MapArtSource::new([(42i64, image.clone())]);
        let (segments, pages) =
            lace_chunks_to_segments(0x1234, 0, true, &chunks, &src).unwrap();
        assert!(pages >= 2, "art run should span multiple pages");
```

The rest of that test (the `flatten(&segments, &art_out)` reconstruction and CRC validation) is unchanged — `flatten` already indexes `art_out` (now the base64 output) by the segment's output offset/len. Any later `assert_eq!(art_served, art_out.len() as u64)` stays valid because `art_out` is the run's full base64 output.

- [ ] **Step 13: Add a bounded-window regression test**

In `musefs-format/src/ogg/mod.rs` `mod tests`, add a counting source asserting no read exceeds one page (proves O(page) streaming, independent of art size):

```rust
    #[test]
    fn synthesis_reads_art_in_page_bounded_windows() {
        use std::cell::Cell;
        struct Counting<'a> { inner: MapArtSource, max: &'a Cell<usize> }
        impl ArtSource for Counting<'_> {
            fn read_window(&self, art_id: i64, offset: u64, buf: &mut [u8]) -> crate::Result<()> {
                self.max.set(self.max.get().max(buf.len()));
                self.inner.read_window(art_id, offset, buf)
            }
        }

        let (header, scan) = opus_header_and_scan(); // existing test helper that yields an Opus header
        let image: Vec<u8> = (0..500_000u32).map(|i| (i % 251) as u8).collect();
        let meta = crate::input::ArtInput {
            art_id: 7,
            mime: "image/jpeg".to_string(),
            description: String::new(),
            picture_type: crate::input::PictureType::new(3).unwrap(),
            width: 0,
            height: 0,
            data_len: crate::input::BlobLen::new(image.len() as u64).unwrap(),
        };
        let max = Cell::new(0usize);
        let src = Counting { inner: MapArtSource::new([(7i64, image.clone())]), max: &max };
        synthesize_layout(&header, scan.audio_offset, scan.audio_length, &[], &[OggArt { meta: &meta }], &src).unwrap();
        // One Ogg page's payload is at most 255*255 = 65025 bytes; raw windows are
        // <= that. The 500 KB image is never read in a single call.
        assert!(max.get() > 0 && max.get() <= 65_025, "max single read was {}", max.get());
    }
```

If no `opus_header_and_scan()` helper exists, build the header inline the way the neighboring `synthesize_layout` Opus tests do (reuse their fixture-construction lines verbatim).

- [ ] **Step 14: Migrate the fuzz target**

Replace the art section of `fuzz/fuzz_targets/ogg.rs` (the comment about lengths being "free to disagree", the `images`/`arts` build, and the `synthesize_layout` call). Couple each image's length to its `data_len` so the streamed model is exercised faithfully; `Unstructured::fill_buffer` always yields exactly `data_len` bytes (zero-padding if input is short, never erroring):

```rust
    let arts_meta = arb_arts(&mut u).unwrap_or_default();
    // Streaming couples image length to data_len (production enforces
    // `byte_len = length(data)`), so generate exactly data_len bytes per image.
    let images: Vec<Vec<u8>> = arts_meta
        .iter()
        .map(|m| {
            let mut img = vec![0u8; m.data_len.get() as usize];
            let _ = u.fill_buffer(&mut img);
            img
        })
        .collect();
    let src = ogg::MapArtSource::new(
        arts_meta.iter().zip(images.iter()).map(|(m, img)| (m.art_id, img.clone())),
    );
    let arts: Vec<OggArt> = arts_meta.iter().map(|m| OggArt { meta: m }).collect();

    if let Ok(layout) = ogg::synthesize_layout(
        &header, scan.audio_offset, scan.audio_length, &tags, &arts, &src,
    ) {
        assert_backing_covers_audio(scan.audio_offset, scan.audio_length, &layout);
    }
```

Add `use musefs_format::ogg::MapArtSource;` (or reference `ogg::MapArtSource` as above) to the imports.

- [ ] **Step 15: Compile, lint, test the workspace**

Run: `cargo build --all-targets`
Expected: PASS (no errors). Fix any remaining call sites the compiler flags using the same rules above.

Run: `cargo clippy --all-targets -- -D warnings`
Expected: PASS. Note `--all-targets` also compiles `benches/` and crate `tests/` (hidden API consumers); fix any there too.

Run: `cargo test`
Expected: PASS — crucially the existing Ogg byte-exactness tests (round-trip / `read_pictures` / CRC validation) stay green, proving served bytes are unchanged.

- [ ] **Step 16: Build the fuzz target (out of workspace)**

Run: `cargo +nightly fuzz build ogg`
Expected: PASS (links). This catches format-layer signature drift the workspace build misses.

- [ ] **Step 17: Commit**

```bash
git add musefs-format/src/error.rs musefs-format/src/ogg/art_source.rs \
        musefs-format/src/ogg/mod.rs musefs-format/src/ogg/page.rs \
        musefs-core/src/mapping.rs musefs-core/src/reader.rs musefs-core/src/ogg_index.rs \
        musefs-format/tests/proptest_ogg.rs fuzz/fuzz_targets/ogg.rs
git commit -m "$(cat <<'EOF'
feat(ogg): stream artwork during layout synthesis instead of materializing it (#266)

Introduce a format-layer ArtSource trait (DB-backed in core, in-memory for
tests/fuzz) and an incremental page CRC fed in page-bounded windows. PayloadChunk
and OggArt carry metadata only; comment/oggflac producers no longer buffer the
image or its base64 copy. Served bytes are byte-identical; synthesis peak memory
drops from ~2.3x the image to O(one Ogg page). Read failures surface as
FormatError::ArtRead.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

### Task 4: Documentation

**Files:** `docs/OGG.md`, `ARCHITECTURE.md`

- [ ] **Step 1: Update `docs/OGG.md`**

Find the section describing art handling/synthesis. State that artwork is now streamed at synthesis (page CRCs are computed from page-bounded `ArtSource` windows; the full image and its base64 copy are never materialized), and that art exceeding `MAX_ART_BYTES` (16 MiB − 64 KiB) is rejected at resolve. Keep the existing read-time base64-window description.

- [ ] **Step 2: Update `ARCHITECTURE.md`**

In the segment-model / freshness section, update the "art is streamed at read time, never materialized whole" invariant to note it now also holds for Ogg synthesis (previously the documented exception). In the external-writer-contract section, note the resolve-time `MAX_ART_BYTES` ceiling applies to all formats (oversize art is rejected with `ArtTooLarge`), and cross-reference #284 for the scanner's ingest-time drop.

- [ ] **Step 3: Verify docs-only commit skips the cargo gate**

Run: `git add docs/OGG.md ARCHITECTURE.md && git commit -m "docs: Ogg art is streamed at synthesis; document shared art cap (#266)"`
Expected: the pre-commit hook prints "docs-only commit — skipping cargo" and passes.

---

## Self-Review Notes (for the implementer)

- **Byte-output invariance is the correctness bar.** The existing Ogg round-trip/`read_pictures`/CRC-validation tests must pass unchanged after Task 3. If a CRC test fails, the streamed base64 window does not equal the whole-image base64 substring — check `crc_feed_art`'s use of `b64_window`/`encode_b64_slice` against the read path in `reader.rs` (`OggArtSlice` arm), which is the proven reference.
- **The cap (Task 1) and streaming (Tasks 2-3) are independent** and can be reviewed/landed separately; Task 1 ships value on its own.
- **`CoreError` converts `FormatError` via `#[from]`** (`error.rs:14`), so the new `FormatError::ArtRead` reaches the FUSE layer with no conversion code to add.
- **No schema change**, so no Python schema-mirror regeneration is needed.
- **Mutation gate:** the cap test pins the exact boundary (`MAX_ART_BYTES` accepted, `+1` rejected) so a `>`→`>=` mutant is caught.
