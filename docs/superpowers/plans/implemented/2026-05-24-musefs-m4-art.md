# musefs M4 (Art Management) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Ingest embedded cover art from backing files at scan time (content-addressed, deduplicated), synthesize it back into served FLAC PICTURE / MP3 APIC frames, and stream the image bytes from SQLite at read time so memory stays bounded.

**Architecture:** Scan-time extraction reads embedded pictures (hand-rolled FLAC PICTURE parser; `id3` crate for MP3 APIC) into the existing content-addressed `art` table and links them via `track_art`. The read path builds `ArtInput`s from art *metadata only* (never loading blobs at resolve time), so `synthesize_layout` emits `ArtImage` segments that carry only a length. `read_at` streams each image slice on demand via SQLite incremental blob I/O. This preserves the project's invariants: no audio duplication, generate-and-measure sizing, and art streamed-not-cached.

**Tech Stack:** Existing `musefs-db`/`musefs-format`/`musefs-core`; `rusqlite`'s `blob` feature for incremental blob reads; the `id3` crate (already present) for APIC extraction; hand-rolled FLAC PICTURE parsing (consistent with the rest of the FLAC code; `metaflac` stays the independent test oracle).

**Scope (this milestone):** extract + dedup + store embedded art (FLAC + MP3), synthesize PICTURE/APIC from the store, stream image bytes at read time. Enforce an art-size cap and clamp picture types at ingestion (the M3 deferrals).

**Explicitly deferred:** art *editing* / external art sources (writable mount is out of MVP); per-fd art-blob handle caching (each `read_at` opens a short-lived blob — fine for a single-threaded mount); structure-only mode / refresh / `--revalidate` (M5).

---

## File Structure

- `musefs-format/src/input.rs` — add the `EmbeddedPicture` extraction type (re-exported from lib.rs).
- `musefs-format/src/flac.rs` — add `read_pictures` (hand-rolled PICTURE block parser).
- `musefs-format/src/mp3.rs` — add `read_pictures` (APIC via the `id3` crate).
- `musefs-format/tests/flac_pictures.rs`, `musefs-format/tests/mp3_pictures.rs` — extraction tests.
- `musefs-db/Cargo.toml` — enable rusqlite's `blob` feature.
- `musefs-db/src/models.rs` — add `ArtMeta` (metadata without the blob).
- `musefs-db/src/art.rs` — add `get_art_meta` and `read_art_chunk` (incremental blob I/O).
- `musefs-db/src/lib.rs` — re-export `ArtMeta`.
- `musefs-db/tests/art.rs` — blob-streaming + metadata tests.
- `musefs-core/src/scan.rs` — ingest extracted pictures (cap, dedup, link).
- `musefs-core/src/mapping.rs` — `track_art_to_inputs` (build `ArtInput`s from metadata).
- `musefs-core/src/reader.rs` — `resolve` passes art inputs to synthesis; `read_at` gains `&Db` and streams `ArtImage` segments.
- `musefs-core/src/error.rs` — remove the now-dead `ArtNotSupported` variant.
- `musefs-core/src/facade.rs` — `read` passes `&self.db` to `read_at`.
- `musefs-fuse/src/lib.rs` — drop the `ArtNotSupported` errno arm + its test.
- `musefs-core/tests/{scan,reader,read_at,facade}.rs` — art coverage + `read_at` signature updates.

**Branch:** all work on a new branch `musefs-m4-art` cut from `main`.

---

## Task 1: `musefs-format` — `EmbeddedPicture` + `flac::read_pictures`

**Files:**
- Modify: `musefs-format/src/input.rs`, `musefs-format/src/lib.rs`, `musefs-format/src/flac.rs`
- Test: `musefs-format/tests/flac_pictures.rs`

- [ ] **Step 1: Write the failing test**

`musefs-format/tests/flac_pictures.rs`:

```rust
use musefs_format::flac::read_pictures;

fn flac_block(block_type: u8, body: &[u8], is_last: bool) -> Vec<u8> {
    let mut out = Vec::new();
    out.push((if is_last { 0x80 } else { 0 }) | (block_type & 0x7F));
    let len = body.len();
    out.push(((len >> 16) & 0xFF) as u8);
    out.push(((len >> 8) & 0xFF) as u8);
    out.push((len & 0xFF) as u8);
    out.extend_from_slice(body);
    out
}

fn streaminfo_body() -> Vec<u8> {
    let mut b = vec![
        0x10, 0x00, 0x10, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x0A, 0xC4, 0x42, 0xF0, 0x00,
        0x00, 0x00, 0x00,
    ];
    b.extend_from_slice(&[0u8; 16]);
    b
}

fn picture_body(pic_type: u32, mime: &str, desc: &str, w: u32, h: u32, data: &[u8]) -> Vec<u8> {
    let mut b = Vec::new();
    b.extend_from_slice(&pic_type.to_be_bytes());
    b.extend_from_slice(&(mime.len() as u32).to_be_bytes());
    b.extend_from_slice(mime.as_bytes());
    b.extend_from_slice(&(desc.len() as u32).to_be_bytes());
    b.extend_from_slice(desc.as_bytes());
    b.extend_from_slice(&w.to_be_bytes());
    b.extend_from_slice(&h.to_be_bytes());
    b.extend_from_slice(&0u32.to_be_bytes()); // color depth
    b.extend_from_slice(&0u32.to_be_bytes()); // colors used
    b.extend_from_slice(&(data.len() as u32).to_be_bytes());
    b.extend_from_slice(data);
    b
}

#[test]
fn extracts_picture_blocks() {
    let img = vec![0xABu8; 50];
    let mut flac = Vec::new();
    flac.extend_from_slice(b"fLaC");
    flac.extend_from_slice(&flac_block(0, &streaminfo_body(), false));
    flac.extend_from_slice(&flac_block(
        6,
        &picture_body(3, "image/png", "front", 10, 20, &img),
        true,
    ));
    flac.extend_from_slice(&[0xFFu8; 8]); // audio

    let pics = read_pictures(&flac).unwrap();
    assert_eq!(pics.len(), 1);
    let p = &pics[0];
    assert_eq!(p.picture_type, 3);
    assert_eq!(p.mime, "image/png");
    assert_eq!(p.description, "front");
    assert_eq!(p.width, 10);
    assert_eq!(p.height, 20);
    assert_eq!(p.data, img);
}

#[test]
fn no_pictures_yields_empty() {
    let mut flac = Vec::new();
    flac.extend_from_slice(b"fLaC");
    flac.extend_from_slice(&flac_block(0, &streaminfo_body(), true));
    flac.extend_from_slice(&[0xFFu8; 4]);
    assert!(read_pictures(&flac).unwrap().is_empty());
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p musefs-format --test flac_pictures`
Expected: FAIL — `read_pictures` not found / `EmbeddedPicture` not found.

- [ ] **Step 3: Add `EmbeddedPicture` and implement `flac::read_pictures`**

In `musefs-format/src/input.rs`, append:

```rust
/// An embedded picture extracted from a backing file at scan time (a FLAC PICTURE
/// block or an MP3 APIC frame), before it is content-addressed and stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmbeddedPicture {
    pub mime: String,
    pub picture_type: u32,
    pub description: String,
    pub width: u32,
    pub height: u32,
    pub data: Vec<u8>,
}
```

In `musefs-format/src/lib.rs`, extend the input re-export to:

```rust
pub use input::{ArtInput, EmbeddedPicture, TagInput};
```

In `musefs-format/src/flac.rs`, change the input import line `use crate::input::{ArtInput, TagInput};` to:

```rust
use crate::input::{ArtInput, EmbeddedPicture, TagInput};
```

Then append to `musefs-format/src/flac.rs`:

```rust
fn read_u32_be(data: &[u8], pos: usize) -> Result<u32> {
    if pos + 4 > data.len() {
        return Err(FormatError::Malformed);
    }
    Ok(u32::from_be_bytes([
        data[pos],
        data[pos + 1],
        data[pos + 2],
        data[pos + 3],
    ]))
}

fn parse_picture_block(body: &[u8]) -> Result<EmbeddedPicture> {
    let mut pos = 0usize;
    let picture_type = read_u32_be(body, pos)?;
    pos += 4;
    let mime_len = read_u32_be(body, pos)? as usize;
    pos += 4;
    let mime_end = pos + mime_len;
    if mime_end > body.len() {
        return Err(FormatError::Malformed);
    }
    let mime = String::from_utf8_lossy(&body[pos..mime_end]).into_owned();
    pos = mime_end;
    let desc_len = read_u32_be(body, pos)? as usize;
    pos += 4;
    let desc_end = pos + desc_len;
    if desc_end > body.len() {
        return Err(FormatError::Malformed);
    }
    let description = String::from_utf8_lossy(&body[pos..desc_end]).into_owned();
    pos = desc_end;
    let width = read_u32_be(body, pos)?;
    pos += 4;
    let height = read_u32_be(body, pos)?;
    pos += 4;
    let _depth = read_u32_be(body, pos)?;
    pos += 4;
    let _colors = read_u32_be(body, pos)?;
    pos += 4;
    let data_len = read_u32_be(body, pos)? as usize;
    pos += 4;
    let data_end = pos + data_len;
    if data_end > body.len() {
        return Err(FormatError::Malformed);
    }
    Ok(EmbeddedPicture {
        mime,
        picture_type,
        description,
        width,
        height,
        data: body[pos..data_end].to_vec(),
    })
}

/// Extract all PICTURE blocks from a complete FLAC file as embedded pictures, for
/// scan-time art ingestion. Returns an empty vec if there are none.
pub fn read_pictures(data: &[u8]) -> Result<Vec<EmbeddedPicture>> {
    if data.len() < 4 || &data[0..4] != FLAC_MARKER {
        return Err(FormatError::NotFlac);
    }
    let mut pos = 4usize;
    let mut out = Vec::new();
    loop {
        if pos + 4 > data.len() {
            return Err(FormatError::Malformed);
        }
        let header = data[pos];
        let is_last = (header & 0x80) != 0;
        let block_type = header & 0x7F;
        let len = ((data[pos + 1] as usize) << 16)
            | ((data[pos + 2] as usize) << 8)
            | (data[pos + 3] as usize);
        let body_start = pos + 4;
        let body_end = body_start + len;
        if body_end > data.len() {
            return Err(FormatError::Malformed);
        }
        if block_type == BLOCK_PICTURE {
            out.push(parse_picture_block(&data[body_start..body_end])?);
        }
        pos = body_end;
        if is_last {
            break;
        }
    }
    Ok(out)
}
```

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test -p musefs-format --test flac_pictures`
Expected: PASS (2 tests).

- [ ] **Step 5: Confirm zero warnings + fmt, then commit**

Run: `cargo clippy -p musefs-format --all-targets 2>&1 | grep -iE "warning|error" || echo clean` (expected `clean`).
Run: `cargo fmt -p musefs-format` then `cargo fmt --all -- --check` (expected: passes).

```bash
git add musefs-format/src/input.rs musefs-format/src/lib.rs musefs-format/src/flac.rs musefs-format/tests/flac_pictures.rs
git commit -m "$(printf 'feat(format): extract embedded FLAC PICTURE blocks\n\nCo-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>')"
```

---

## Task 2: `musefs-format` — `mp3::read_pictures`

**Files:**
- Modify: `musefs-format/src/mp3.rs`
- Test: `musefs-format/tests/mp3_pictures.rs`

- [ ] **Step 1: Write the failing test**

`musefs-format/tests/mp3_pictures.rs`:

```rust
use musefs_format::mp3::read_pictures;

#[test]
fn extracts_apic_pictures() {
    use id3::TagLike;

    let img = vec![0x77u8; 64];
    let mut tag = id3::Tag::new();
    tag.add_frame(id3::frame::Picture {
        mime_type: "image/jpeg".to_string(),
        picture_type: id3::frame::PictureType::CoverFront,
        description: "cover".to_string(),
        data: img.clone(),
    });
    let mut bytes = Vec::new();
    tag.write_to(&mut bytes, id3::Version::Id3v24).unwrap();
    bytes.extend_from_slice(&[0xFF, 0xFB, 0, 0]);

    let pics = read_pictures(&bytes);
    assert_eq!(pics.len(), 1);
    let p = &pics[0];
    assert_eq!(p.mime, "image/jpeg");
    assert_eq!(p.picture_type, 3); // front cover
    assert_eq!(p.description, "cover");
    assert_eq!(p.data, img);
}

#[test]
fn no_tag_yields_empty() {
    let data = [0xFF, 0xFB, 0, 0, 0, 0];
    assert!(read_pictures(&data).is_empty());
}
```

Note: `id3::Tag::add_frame` accepts anything implementing `Into<Frame>`; `id3::frame::Picture` qualifies. If the compiler reports a trait/import issue, the equivalent is `tag.add_frame(picture)` where `picture` is the `id3::frame::Picture` value above — adjust imports as the compiler directs but keep the picture fields identical.

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p musefs-format --test mp3_pictures`
Expected: FAIL — `read_pictures` not found in `musefs_format::mp3`.

- [ ] **Step 3: Implement `mp3::read_pictures`**

In `musefs-format/src/mp3.rs`, change the input import to include `EmbeddedPicture`. Find the existing `use crate::input::{ArtInput, TagInput};` and change it to:

```rust
use crate::input::{ArtInput, EmbeddedPicture, TagInput};
```

Then append:

```rust
/// Extract all APIC pictures from an MP3's ID3v2 tag as embedded pictures, for
/// scan-time art ingestion. Returns empty if there is no tag or no pictures.
pub fn read_pictures(data: &[u8]) -> Vec<EmbeddedPicture> {
    let tag = match id3::Tag::read_from2(std::io::Cursor::new(data)) {
        Ok(t) => t,
        Err(_) => return Vec::new(),
    };
    tag.pictures()
        .map(|p| EmbeddedPicture {
            mime: p.mime_type.clone(),
            picture_type: u8::from(p.picture_type) as u32,
            description: p.description.clone(),
            width: 0,
            height: 0,
            data: p.data.clone(),
        })
        .collect()
}
```

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test -p musefs-format --test mp3_pictures`
Expected: PASS (2 tests).

- [ ] **Step 5: Confirm zero warnings + fmt, then commit**

Run: `cargo clippy -p musefs-format --all-targets 2>&1 | grep -iE "warning|error" || echo clean` (expected `clean`).
Run: `cargo fmt -p musefs-format` then `cargo fmt --all -- --check`.

```bash
git add musefs-format/src/mp3.rs musefs-format/tests/mp3_pictures.rs
git commit -m "$(printf 'feat(format): extract embedded MP3 APIC pictures\n\nCo-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>')"
```

---

## Task 3: `musefs-db` — incremental blob streaming + art metadata

**Files:**
- Modify: `musefs-db/Cargo.toml`, `musefs-db/src/models.rs`, `musefs-db/src/art.rs`, `musefs-db/src/lib.rs`
- Test: `musefs-db/tests/art.rs`

- [ ] **Step 1: Enable the rusqlite `blob` feature**

In `musefs-db/Cargo.toml`, change the rusqlite dependency line to:

```toml
rusqlite = { version = "0.31", features = ["bundled", "blob"] }
```

- [ ] **Step 2: Write the failing test**

`musefs-db/tests/art.rs` already exists (M0). Append:

```rust
#[test]
fn read_art_chunk_streams_a_slice() {
    let db = Db::open_in_memory().unwrap();
    let id = db
        .upsert_art(&NewArt {
            mime: "image/png".to_string(),
            width: Some(2),
            height: Some(3),
            data: vec![10, 11, 12, 13, 14, 15],
        })
        .unwrap();

    // A middle slice.
    assert_eq!(db.read_art_chunk(id, 2, 3).unwrap(), vec![12, 13, 14]);
    // From the start.
    assert_eq!(db.read_art_chunk(id, 0, 2).unwrap(), vec![10, 11]);

    // Metadata without loading the blob.
    let meta = db.get_art_meta(id).unwrap().unwrap();
    assert_eq!(meta.mime, "image/png");
    assert_eq!(meta.width, Some(2));
    assert_eq!(meta.byte_len, 6);

    assert!(db.get_art_meta(999_999).unwrap().is_none());
}
```

Confirm the top of `musefs-db/tests/art.rs` imports the needed types (it should already `use musefs_db::{...}` including `Db` and `NewArt`; if `NewArt` is not imported, add it to the existing `use musefs_db::{...}` line).

- [ ] **Step 3: Run the test to verify it fails**

Run: `cargo test -p musefs-db --test art read_art_chunk`
Expected: FAIL — `read_art_chunk` / `get_art_meta` not found (and `ArtMeta` unresolved if referenced).

- [ ] **Step 4: Implement `ArtMeta`, `get_art_meta`, `read_art_chunk`**

In `musefs-db/src/models.rs`, append:

```rust
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtMeta {
    pub mime: String,
    pub width: Option<i64>,
    pub height: Option<i64>,
    pub byte_len: i64,
}
```

In `musefs-db/src/art.rs`, change the models import at the top — find `use crate::models::{Art, NewArt, TrackArt};` and change it to:

```rust
use crate::models::{Art, ArtMeta, NewArt, TrackArt};
```

Then add these two methods inside the existing `impl Db { ... }` block in `art.rs` (e.g. after `get_art`):

```rust
    /// Art row metadata without loading the image blob — used to build synthesis
    /// inputs at resolve time without materializing art in memory.
    pub fn get_art_meta(&self, id: i64) -> Result<Option<ArtMeta>> {
        let mut stmt = self
            .conn
            .prepare("SELECT mime, width, height, byte_len FROM art WHERE id = ?1")?;
        let mut rows = stmt.query(params![id])?;
        match rows.next()? {
            Some(r) => Ok(Some(ArtMeta {
                mime: r.get(0)?,
                width: r.get(1)?,
                height: r.get(2)?,
                byte_len: r.get(3)?,
            })),
            None => Ok(None),
        }
    }

    /// Stream `len` bytes of an art blob starting at `offset` via SQLite
    /// incremental blob I/O, so image bytes are never fully materialized. The
    /// caller guarantees `offset + len` is within the blob (the segment layout
    /// is built from `byte_len`).
    pub fn read_art_chunk(&self, art_id: i64, offset: u64, len: usize) -> Result<Vec<u8>> {
        let blob =
            self.conn
                .blob_open(rusqlite::DatabaseName::Main, "art", "data", art_id, true)?;
        let mut buf = vec![0u8; len];
        blob.read_at(&mut buf, offset as usize)?;
        Ok(buf)
    }
```

In `musefs-db/src/lib.rs`, add `ArtMeta` to the models re-export (find the `pub use models::{...}` line and insert `ArtMeta` in alphabetical position, e.g. `pub use models::{Art, ArtMeta, Format, NewArt, NewTrack, Tag, Track, TrackArt};` — match the actual existing list, just add `ArtMeta`).

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p musefs-db --test art`
Expected: PASS — the new streaming/metadata test plus the existing art tests.

- [ ] **Step 6: Confirm zero warnings + fmt, then commit**

Run: `cargo clippy -p musefs-db --all-targets 2>&1 | grep -iE "warning|error" || echo clean` (expected `clean`).
Run: `cargo fmt -p musefs-db` then `cargo fmt --all -- --check`.

```bash
git add musefs-db/Cargo.toml Cargo.lock musefs-db/src/models.rs musefs-db/src/art.rs musefs-db/src/lib.rs musefs-db/tests/art.rs
git commit -m "$(printf 'feat(db): incremental art-blob streaming and metadata lookup\n\nCo-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>')"
```

---

## Task 4: `musefs-core` — ingest embedded art at scan time

**Files:**
- Modify: `musefs-core/src/scan.rs`
- Test: `musefs-core/tests/scan.rs`

- [ ] **Step 1: Write the failing test**

Append to `musefs-core/tests/scan.rs`:

```rust
fn flac_with_picture(comments: &[&str], img: &[u8]) -> Vec<u8> {
    use common::{flac_block, streaminfo_body, vorbis_comment_body};
    fn picture_body(pic_type: u32, mime: &str, data: &[u8]) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(&pic_type.to_be_bytes());
        b.extend_from_slice(&(mime.len() as u32).to_be_bytes());
        b.extend_from_slice(mime.as_bytes());
        b.extend_from_slice(&0u32.to_be_bytes()); // empty description
        b.extend_from_slice(&0u32.to_be_bytes()); // width
        b.extend_from_slice(&0u32.to_be_bytes()); // height
        b.extend_from_slice(&0u32.to_be_bytes()); // depth
        b.extend_from_slice(&0u32.to_be_bytes()); // colors
        b.extend_from_slice(&(data.len() as u32).to_be_bytes());
        b.extend_from_slice(data);
        b
    }
    let mut out = Vec::new();
    out.extend_from_slice(b"fLaC");
    out.extend_from_slice(&flac_block(0, &streaminfo_body(), false));
    out.extend_from_slice(&flac_block(4, &vorbis_comment_body("v", comments), false));
    out.extend_from_slice(&flac_block(6, &picture_body(3, "image/png", img), true));
    out.extend_from_slice(&[0xAAu8; 24]);
    out
}

#[test]
fn scan_ingests_and_dedups_embedded_art() {
    let dir = tempfile::tempdir().unwrap();
    let img = vec![0x42u8; 100];
    std::fs::write(dir.path().join("a.flac"), flac_with_picture(&["TITLE=A"], &img)).unwrap();
    std::fs::create_dir(dir.path().join("sub")).unwrap();
    std::fs::write(dir.path().join("sub/b.flac"), flac_with_picture(&["TITLE=B"], &img)).unwrap();

    let db = Db::open_in_memory().unwrap();
    scan_directory(&db, dir.path()).unwrap();

    let tracks = db.list_tracks().unwrap();
    assert_eq!(tracks.len(), 2);

    // Both tracks link art, and the identical image is stored once (dedup by sha256).
    let mut art_ids = std::collections::HashSet::new();
    for t in &tracks {
        let ta = db.get_track_art(t.id).unwrap();
        assert_eq!(ta.len(), 1);
        assert_eq!(ta[0].picture_type, 3);
        art_ids.insert(ta[0].art_id);
    }
    assert_eq!(art_ids.len(), 1, "identical art should dedup to one row");

    let only = *art_ids.iter().next().unwrap();
    assert_eq!(db.get_art_meta(only).unwrap().unwrap().byte_len, 100);
}
```

(The existing `tests/scan.rs` imports `use musefs_core::scan_directory; use musefs_db::Db;` and `mod common;`; the `common` helpers `flac_block`/`streaminfo_body`/`vorbis_comment_body` already exist. Keep all existing tests.)

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p musefs-core --test scan scan_ingests_and_dedups`
Expected: FAIL — `get_track_art` returns empty (scan doesn't ingest art yet).

- [ ] **Step 3: Implement art ingestion in the scanner**

In `musefs-core/src/scan.rs`, update the imports and `Probed`/`probe`/`scan_directory`.

Change the db/format import line `use musefs_db::{Db, Format, NewTrack, Tag};` to:

```rust
use musefs_db::{Db, Format, NewArt, NewTrack, Tag, TrackArt};
```

Add `EmbeddedPicture` to the format import — change `use musefs_format::{flac, mp3};` to:

```rust
use musefs_format::{flac, mp3, EmbeddedPicture};
```

Add the art-size cap constant near the top of the file (after the imports):

```rust
/// Skip embedded art larger than this. The binding limit is FLAC's 24-bit PICTURE
/// block length (~16 MiB for the whole block); cover art is far smaller.
const MAX_ART_BYTES: usize = 16 * 1024 * 1024 - 1024;
```

Add a `pictures` field to `Probed`:

```rust
struct Probed {
    format: Format,
    audio_offset: u64,
    audio_length: u64,
    tags: Vec<(String, String)>,
    pictures: Vec<EmbeddedPicture>,
}
```

Update `probe` to populate `pictures` in each arm:

```rust
fn probe(path: &Path, bytes: &[u8]) -> Option<Probed> {
    if has_ext(path, "flac") {
        let scan = flac::locate_audio(bytes).ok()?;
        Some(Probed {
            format: Format::Flac,
            audio_offset: scan.audio_offset,
            audio_length: scan.audio_length,
            tags: flac::read_vorbis_comments(bytes).unwrap_or_default(),
            pictures: flac::read_pictures(bytes).unwrap_or_default(),
        })
    } else if has_ext(path, "mp3") {
        let bounds = mp3::locate_audio(bytes).ok()?;
        Some(Probed {
            format: Format::Mp3,
            audio_offset: bounds.audio_offset,
            audio_length: bounds.audio_length,
            tags: mp3::read_tags(bytes),
            pictures: mp3::read_pictures(bytes),
        })
    } else {
        None
    }
}
```

In `scan_directory`, after the `db.replace_tags(track_id, &tags)?;` line and before `stats.scanned += 1;`, insert the art-ingestion block:

```rust
        let mut track_arts = Vec::new();
        for (ordinal, pic) in probed.pictures.into_iter().enumerate() {
            if pic.data.len() > MAX_ART_BYTES {
                continue;
            }
            let art_id = db.upsert_art(&NewArt {
                mime: pic.mime,
                width: (pic.width != 0).then_some(pic.width as i64),
                height: (pic.height != 0).then_some(pic.height as i64),
                data: pic.data,
            })?;
            // Valid ID3/FLAC picture types are 0..=20; clamp anything out of range.
            let picture_type = if pic.picture_type <= 20 {
                pic.picture_type as i64
            } else {
                0
            };
            track_arts.push(TrackArt {
                art_id,
                picture_type,
                description: pic.description,
                ordinal: ordinal as i64,
            });
        }
        db.set_track_art(track_id, &track_arts)?;
```

(Note: the tag-seeding loop already consumes `probed.tags` by value; make sure the art block uses `probed.pictures` after that — `probed` is still in scope. If the borrow checker complains because `probed.tags` was moved, that's fine: moving `probed.tags` and later moving `probed.pictures` are disjoint field moves and both compile.)

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p musefs-core --test scan`
Expected: PASS — the new art-dedup test plus existing FLAC/MP3 scan tests.

- [ ] **Step 5: Confirm zero warnings + fmt, then commit**

Run: `cargo clippy -p musefs-core --all-targets 2>&1 | grep -iE "warning|error" || echo clean` (expected `clean`).
Run: `cargo fmt -p musefs-core` then `cargo fmt --all -- --check`.

```bash
git add musefs-core/src/scan.rs musefs-core/tests/scan.rs
git commit -m "$(printf 'feat(core): ingest, cap, and dedup embedded art at scan time\n\nCo-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>')"
```

---

## Task 5: `musefs-core` — build art inputs and synthesize them

**Files:**
- Modify: `musefs-core/src/mapping.rs`, `musefs-core/src/reader.rs`
- Test: `musefs-core/tests/reader.rs`

- [ ] **Step 1: Write the failing test**

Append to `musefs-core/tests/reader.rs`:

```rust
#[test]
fn resolve_includes_art_image_segments() {
    use musefs_db::{NewArt, TrackArt};
    use musefs_format::Segment;

    let (_dir, db, id) = setup();
    let art_id = db
        .upsert_art(&NewArt {
            mime: "image/png".to_string(),
            width: None,
            height: None,
            data: vec![0x9u8; 80],
        })
        .unwrap();
    db.set_track_art(
        id,
        &[TrackArt {
            art_id,
            picture_type: 3,
            description: String::new(),
            ordinal: 0,
        }],
    )
    .unwrap();

    let mut cache = HeaderCache::new();
    let resolved = cache.resolve(&db, id).unwrap();
    assert!(resolved
        .layout
        .segments
        .iter()
        .any(|s| matches!(s, Segment::ArtImage { art_id: a, len } if *a == art_id && *len == 80)));
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p musefs-core --test reader resolve_includes_art`
Expected: FAIL — `resolve` passes `&[]` for arts, so no `ArtImage` segment is produced.

- [ ] **Step 3: Implement `track_art_to_inputs` and pass it through `resolve`**

In `musefs-core/src/mapping.rs`, add the imports it needs at the top (it currently has `use std::collections::BTreeMap; use musefs_db::Tag; use musefs_format::TagInput;`). Add:

```rust
use crate::error::Result;
use musefs_db::Db;
use musefs_format::ArtInput;
```

Then append this function:

```rust
/// Build the synthesis art inputs for a track from `track_art` + art metadata.
/// Reads metadata only (never the blob) so resolve stays memory-bounded; the
/// image bytes are streamed at read time.
pub(crate) fn track_art_to_inputs(db: &Db, track_id: i64) -> Result<Vec<ArtInput>> {
    let mut inputs = Vec::new();
    for ta in db.get_track_art(track_id)? {
        if let Some(meta) = db.get_art_meta(ta.art_id)? {
            inputs.push(ArtInput {
                art_id: ta.art_id,
                mime: meta.mime,
                description: ta.description,
                picture_type: ta.picture_type as u32,
                width: meta.width.unwrap_or(0) as u32,
                height: meta.height.unwrap_or(0) as u32,
                data_len: meta.byte_len as u64,
            });
        }
    }
    Ok(inputs)
}
```

In `musefs-core/src/reader.rs`, change the mapping import — find `use crate::mapping::tags_to_inputs;` and change it to:

```rust
use crate::mapping::{tags_to_inputs, track_art_to_inputs};
```

Then, inside `HeaderCache::resolve`, where the inputs are computed (currently `let tags = db.get_tags(track_id)?; let inputs = tags_to_inputs(&tags);`), add the art inputs right after:

```rust
        let tags = db.get_tags(track_id)?;
        let inputs = tags_to_inputs(&tags);
        let art_inputs = track_art_to_inputs(db, track_id)?;
```

Then in the `match track.format` block, replace both `&[]` art arguments with `&art_inputs`:

```rust
        let layout = match track.format {
            Format::Flac => {
                let front =
                    read_front(Path::new(&track.backing_path), track.audio_offset as u64)?;
                let fmeta = flac::read_metadata(&front)?;
                let scan = FlacScan {
                    audio_offset: track.audio_offset as u64,
                    audio_length: track.audio_length as u64,
                    preserved: fmeta.preserved,
                };
                flac::synthesize_layout(&scan, &inputs, &art_inputs)
            }
            Format::Mp3 => mp3::synthesize_layout(
                track.audio_offset as u64,
                track.audio_length as u64,
                &inputs,
                &art_inputs,
            ),
        };
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p musefs-core --test reader`
Expected: PASS — the new art-segment test plus existing reader tests.

- [ ] **Step 5: Confirm zero warnings + fmt, then commit**

Run: `cargo clippy -p musefs-core --all-targets 2>&1 | grep -iE "warning|error" || echo clean` (expected `clean`).
Run: `cargo fmt -p musefs-core` then `cargo fmt --all -- --check`.

```bash
git add musefs-core/src/mapping.rs musefs-core/src/reader.rs musefs-core/tests/reader.rs
git commit -m "$(printf 'feat(core): synthesize art frames from the store at resolve time\n\nCo-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>')"
```

---

## Task 6: `musefs-core` — stream art bytes in the read path

This adds `&Db` to `read_at` so `ArtImage` segments stream from the blob store, and removes the now-dead `CoreError::ArtNotSupported`.

**Files:**
- Modify: `musefs-core/src/reader.rs`, `musefs-core/src/facade.rs`, `musefs-core/src/error.rs`, `musefs-fuse/src/lib.rs`
- Test: `musefs-core/tests/read_at.rs`

- [ ] **Step 1: Update the existing `read_at` tests to the new signature and add a streaming test**

In `musefs-core/tests/read_at.rs`, update the three existing `read_at(&resolved, ...)` call sites to pass the `db` already in scope: `read_at(&resolved, &db, ...)`. Specifically:
- In `reading_whole_file_matches_total_len_and_splices_audio`: `read_at(&resolved, &db, 0, resolved.total_len)`.
- In `random_offset_and_size_match_the_whole_read`: both `read_at(&resolved, &db, 0, resolved.total_len)` and `read_at(&resolved, &db, off, size)`.
- In `reading_past_eof_returns_empty`: both calls → `read_at(&resolved, &db, resolved.total_len, 100)` and `read_at(&resolved, &db, resolved.total_len + 5, 100)`.

Then append a streaming test:

```rust
#[test]
fn read_at_streams_art_image_segments() {
    use musefs_core::{read_at, ResolvedFile};
    use musefs_format::{RegionLayout, Segment};

    let db = Db::open_in_memory().unwrap();
    let art = vec![1u8, 2, 3, 4, 5, 6, 7, 8];
    let art_id = db
        .upsert_art(&musefs_db::NewArt {
            mime: "image/png".to_string(),
            width: None,
            height: None,
            data: art.clone(),
        })
        .unwrap();

    let layout = RegionLayout::new(vec![
        Segment::Inline(vec![0xAA, 0xBB]),
        Segment::ArtImage {
            art_id,
            len: art.len() as u64,
        },
    ]);
    let total_len = layout.total_len();
    let resolved = ResolvedFile {
        layout,
        total_len,
        content_version: 0,
        backing_path: std::path::PathBuf::from("/unused"),
        mtime_secs: 0,
    };

    // Whole read: inline framing then the streamed art bytes.
    let whole = read_at(&resolved, &db, 0, total_len).unwrap();
    assert_eq!(whole, vec![0xAA, 0xBB, 1, 2, 3, 4, 5, 6, 7, 8]);

    // A window that lands entirely inside the art segment (offset 4 → art[2..5]).
    assert_eq!(read_at(&resolved, &db, 4, 3).unwrap(), vec![3, 4, 5]);
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p musefs-core --test read_at`
Expected: FAIL — `read_at` takes 3 args, not 4 (signature mismatch), and `ResolvedFile`/the art arm don't stream yet.

- [ ] **Step 3: Change `read_at` to stream art, drop `ArtNotSupported`**

In `musefs-core/src/reader.rs`, change the `read_at` signature and the `ArtImage` arm. The signature becomes:

```rust
pub fn read_at(resolved: &ResolvedFile, db: &Db, offset: u64, size: u64) -> Result<Vec<u8>> {
```

And replace the `ArtImage` match arm — currently:

```rust
                Segment::ArtImage { .. } => {
                    return Err(CoreError::ArtNotSupported);
                }
```

with:

```rust
                Segment::ArtImage { art_id, .. } => {
                    let chunk = db.read_art_chunk(*art_id, within, n)?;
                    out.extend_from_slice(&chunk);
                }
```

(`within` is the `u64` offset into the segment and `n` is the `usize` byte count, both already computed in the surrounding loop for the other arms.)

In `musefs-core/src/error.rs`, remove the `ArtNotSupported` variant:

```rust
    #[error("embedded art is not supported in this milestone")]
    ArtNotSupported,
```

Delete those two lines.

In `musefs-core/src/facade.rs`, update `Musefs::read`'s final line from `read_at(&resolved, offset, size)` to:

```rust
        read_at(&resolved, &self.db, offset, size)
```

In `musefs-fuse/src/lib.rs`, remove `ArtNotSupported` from the `errno` match. The EIO arm currently reads:

```rust
        CoreError::BackingChanged(_)
        | CoreError::Db(_)
        | CoreError::Format(_)
        | CoreError::ArtNotSupported => libc::EIO,
```

Change it to:

```rust
        CoreError::BackingChanged(_) | CoreError::Db(_) | CoreError::Format(_) => libc::EIO,
```

And in the `errno` unit test in the same file, delete the line:

```rust
        assert_eq!(errno(&CoreError::ArtNotSupported), libc::EIO);
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p musefs-core --test read_at` (expected PASS).
Run: `cargo test -p musefs-fuse` (expected PASS — errno test still green without the removed arm).

- [ ] **Step 5: Confirm zero warnings + fmt, then commit**

Run: `cargo clippy -p musefs-core -p musefs-fuse --all-targets 2>&1 | grep -iE "warning|error" || echo clean` (expected `clean`).
Run: `cargo fmt -p musefs-core -p musefs-fuse` then `cargo fmt --all -- --check`.

```bash
git add musefs-core/src/reader.rs musefs-core/src/facade.rs musefs-core/src/error.rs musefs-fuse/src/lib.rs musefs-core/tests/read_at.rs
git commit -m "$(printf 'feat(core): stream embedded art from the store in the read path\n\nCo-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>')"
```

---

## Task 7: End-to-end art through the facade (FLAC + MP3)

**Files:**
- Test: `musefs-core/tests/facade.rs`

- [ ] **Step 1: Write the failing test**

Append to `musefs-core/tests/facade.rs`:

```rust
#[test]
fn serves_flac_with_embedded_art_through_the_facade() {
    let dir = tempfile::tempdir().unwrap();
    let img = vec![0xC3u8; 120];

    // Build a FLAC with a PICTURE block.
    fn picture_body(mime: &str, data: &[u8]) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(&3u32.to_be_bytes()); // front cover
        b.extend_from_slice(&(mime.len() as u32).to_be_bytes());
        b.extend_from_slice(mime.as_bytes());
        b.extend_from_slice(&0u32.to_be_bytes()); // description
        b.extend_from_slice(&0u32.to_be_bytes()); // width
        b.extend_from_slice(&0u32.to_be_bytes()); // height
        b.extend_from_slice(&0u32.to_be_bytes()); // depth
        b.extend_from_slice(&0u32.to_be_bytes()); // colors
        b.extend_from_slice(&(data.len() as u32).to_be_bytes());
        b.extend_from_slice(data);
        b
    }
    let mut flac = Vec::new();
    flac.extend_from_slice(b"fLaC");
    flac.extend_from_slice(&common::flac_block(0, &common::streaminfo_body(), false));
    flac.extend_from_slice(&common::flac_block(
        4,
        &common::vorbis_comment_body("v", &["ARTIST=Art", "TITLE=Cover"]),
        false,
    ));
    flac.extend_from_slice(&common::flac_block(6, &picture_body("image/png", &img), true));
    flac.extend_from_slice(&[0x5Au8; 40]);
    std::fs::write(dir.path().join("c.flac"), &flac).unwrap();

    let db = musefs_db::Db::open_in_memory().unwrap();
    scan_directory(&db, dir.path()).unwrap();
    let mut fs = Musefs::open(db, config()).unwrap();

    let artist = fs.lookup(VirtualTree::ROOT, "Art").unwrap();
    let (_name, file_inode, _) = fs.readdir(artist).unwrap().into_iter().next().unwrap();
    let attr = fs.getattr(file_inode).unwrap();
    let whole = fs.read(file_inode, 0, attr.size).unwrap();
    assert_eq!(whole.len() as u64, attr.size);

    // The synthesized FLAC carries the embedded picture with the original bytes.
    let tag = metaflac::Tag::read_from(&mut std::io::Cursor::new(&whole)).unwrap();
    let pic = tag.pictures().next().expect("a picture");
    assert_eq!(pic.data, img);
    assert_eq!(pic.mime_type, "image/png");
}
```

(The existing `facade.rs` test imports — `scan_directory, CoreError, MountConfig, Musefs, VirtualTree`, `mod common`, the `config()` helper, and `metaflac` (dev-dep) — are already present.)

- [ ] **Step 2: Run the test to verify it fails or passes**

Run: `cargo test -p musefs-core --test facade serves_flac_with_embedded_art`
Expected: PASS — Tasks 1–6 already wired the full path (scan ingest → resolve synth → read stream). If it FAILS, the failure pinpoints an integration gap across the prior tasks; fix there rather than weakening the test. (This task is primarily an end-to-end guard; it should pass on the work already done.)

- [ ] **Step 3: Confirm zero warnings + fmt, then commit**

Run: `cargo clippy -p musefs-core --all-targets 2>&1 | grep -iE "warning|error" || echo clean` (expected `clean`).
Run: `cargo fmt --all -- --check`.

```bash
git add musefs-core/tests/facade.rs
git commit -m "$(printf 'test(core): end-to-end FLAC cover art through the mount facade\n\nCo-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>')"
```

---

## Task 8: Whole-workspace verification

**Files:** none (verification only).

- [ ] **Step 1: Run the entire workspace test suite**

Run: `cargo test`
Expected: PASS across all crates (db art streaming, format extraction, core scan/resolve/read/facade art, fuse, cli).

- [ ] **Step 2: Run the gated FUSE mount test (regression)**

Run: `cargo test -p musefs-fuse -- --ignored`
Expected: PASS (`end_to_end_read_through_mount`).

- [ ] **Step 3: Confirm a clean, warning-free, formatted workspace**

Run: `cargo clippy --workspace --all-targets 2>&1 | grep -iE "warning|error" || echo "clean"` (expected `clean`).
Run: `cargo fmt --all -- --check && echo "fmt clean"` (expected `fmt clean`).

- [ ] **Step 4: Manual end-to-end smoke (optional, real binary + mount)**

```bash
cargo build -p musefs-cli
BACK=$(mktemp -d); DB=$(mktemp -u --suffix=.db); MNT=$(mktemp -d)
# place a real .flac/.mp3 with embedded cover art in $BACK
./target/debug/musefs scan "$BACK" --db "$DB"
./target/debug/musefs mount "$MNT" --db "$DB" --template '$artist/$title'
# in another terminal: open a file in $MNT in a player/tag editor, confirm cover art shows; then:
fusermount3 -u "$MNT"
```

Expected: the mounted files show embedded cover art; `st_size` matches bytes read. Documentation for the operator; not automated.

- [ ] **Step 5: Commit any cleanup**

```bash
git add -A
git commit -m "$(printf 'chore: M4 art cleanup, no warnings\n\nCo-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>')" || echo "nothing to commit"
```

---

## Self-Review Notes

- **Spec coverage (M4 scope):** `art`/`track_art` population + content-addressed dedup (Task 4, using the existing `upsert_art` sha256 dedup); embedded-art extraction (Tasks 1–2: hand-rolled FLAC PICTURE, `id3` APIC); incremental-blob streaming at read time (Task 3 `read_art_chunk` + Task 6 `read_at`); PICTURE/APIC synthesis from the store (Task 5 builds `ArtInput`s from metadata only; the framing already existed from M1/M3); art-size cap + picture-type clamp at ingestion (Task 4 — the M3 deferrals); end-to-end verification (Tasks 7–8). The `ArtImage` segment model (present since M1) now carries real data.
- **Streamed-not-cached invariant upheld:** resolve builds `ArtInput`s from `get_art_meta` (metadata only — no blob load); the synthesized layout holds only `ArtImage { art_id, len }`; `read_at` streams each requested slice via `read_art_chunk` (SQLite incremental blob I/O). No code path materializes a full art blob into the header cache.
- **Correctly deferred:** art *editing*/external sources (writable mount is post-MVP); per-fd blob-handle caching (single-threaded mount opens a short-lived blob per read — acceptable); structure-only / refresh / `--revalidate` (M5).
- **Trust-boundary / robustness:** `MAX_ART_BYTES` caps ingestion below FLAC's 24-bit PICTURE limit so synthesis can't overflow (resolving the M3 release-mode `debug_assert!` concern at the source); picture types are clamped to 0..=20 at ingestion so the synthesis `as u8` cast is always valid (the other M3 deferral); `read_art_chunk` is only ever called with in-bounds `(offset, len)` derived from the stored `byte_len`.
- **Type consistency:** `EmbeddedPicture { mime, picture_type: u32, description, width: u32, height: u32, data }` (extraction); `ArtInput { art_id, mime, description, picture_type: u32, width: u32, height: u32, data_len: u64 }` (synthesis); `ArtMeta { mime, width: Option<i64>, height: Option<i64>, byte_len: i64 }`; `NewArt`/`TrackArt`/`Art` unchanged (M0). `read_at(&ResolvedFile, &Db, u64, u64)` — the `&Db` addition ripples to `Musefs::read` and the `read_at` tests, all updated in Task 6. `track_art_to_inputs(&Db, i64) -> Result<Vec<ArtInput>>`. `read_art_chunk(i64, u64, usize) -> Result<Vec<u8>>`, `get_art_meta(i64) -> Result<Option<ArtMeta>>`. `id3` API: `u8::from(PictureType)`, `Picture { mime_type, picture_type, description, data }`, `Tag::add_frame`.
- **Placeholder discipline:** every code step ships complete, compilable code; no stubs. The only behavior removal is the dead `ArtNotSupported` variant (Task 6), cleaned up across `musefs-core` + `musefs-fuse` in one task so nothing references it afterward.
- **Dependency note:** only change is enabling rusqlite's `blob` feature (no new crate). FLAC art reading is hand-rolled (no new runtime dep); `metaflac` remains a dev-only oracle and validates the synthesized PICTURE in Task 7.
```
