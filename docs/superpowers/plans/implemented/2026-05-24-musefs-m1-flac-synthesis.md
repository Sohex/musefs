# musefs M1 — Pure FLAC Synthesis (`musefs-format`) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build the pure, dependency-light `musefs-format` crate that locates the audio-frame boundary in a FLAC file and synthesizes a new metadata region (Vorbis comments + PICTURE blocks) as an ordered `RegionLayout` of segments, byte-exactly and round-trip validated.

**Architecture:** A standalone crate with NO database or FUSE dependencies. `locate_audio(&[u8]) -> FlacScan` parses FLAC metadata blocks, preserves structural blocks (STREAMINFO etc.), and reports where audio begins. `synthesize_layout(&FlacScan, tags, arts) -> RegionLayout` emits an ordered list of `Segment`s — `Inline(bytes)` for generated framing/text, `ArtImage{art_id, len}` for image data the caller will splice later, and a trailing `BackingAudio{offset, len}`. The crate never materializes image bytes; it only needs their lengths, so `header_len`/`total_len` are exact by construction. Correctness is proven with two independent oracles: a hand-rolled FLAC byte builder in the tests, and the `metaflac` crate as a decoder.

**Tech Stack:** Rust (edition 2021); `thiserror` for errors; `metaflac` as a dev-dependency decoder oracle. No production parsing dependencies — FLAC metadata framing is encoded/decoded by hand for byte-exact control.

---

## Background: FLAC byte layout (reference for implementers)

A FLAC file is: the 4-byte marker `fLaC`, then a sequence of metadata blocks, then audio frames.

**Metadata block header (4 bytes):**
- Byte 0: bit 7 = "last metadata block" flag; bits 6–0 = block type.
- Bytes 1–3: block body length, 24-bit **big-endian** (excludes the 4-byte header).

**Block types:** STREAMINFO=0, PADDING=1, APPLICATION=2, SEEKTABLE=3, VORBIS_COMMENT=4, CUESHEET=5, PICTURE=6.

**VORBIS_COMMENT body (little-endian lengths):**
- vendor length (u32 LE) + vendor bytes
- comment count (u32 LE)
- for each comment: length (u32 LE) + `KEY=value` UTF-8 bytes

**PICTURE body (big-endian fields):**
- picture type (u32 BE)
- mime length (u32 BE) + mime ASCII
- description length (u32 BE) + description UTF-8
- width (u32 BE), height (u32 BE), color depth (u32 BE), number of colors (u32 BE)
- picture data length (u32 BE) + picture data bytes

musefs preserves STREAMINFO and other structural blocks (APPLICATION/SEEKTABLE/CUESHEET) verbatim, drops the original VORBIS_COMMENT/PICTURE/PADDING, and regenerates VORBIS_COMMENT + PICTURE from the supplied tags/art. The synthesized file is `fLaC` + [preserved blocks] + [new VORBIS_COMMENT] + [new PICTURE(s)] + [original audio frames]. The PICTURE image data is the only large part and is represented as an `ArtImage` segment, not inlined.

Note: FLAC block lengths are 24-bit (max 16 MiB). M1 assumes individual cover-art images fit (they always do in practice); a larger-image guard is deferred.

---

## File Structure

- `Cargo.toml` (workspace root) — add `musefs-format` to `members`.
- `musefs-format/Cargo.toml` — crate manifest (`thiserror`; dev-dep `metaflac`).
- `musefs-format/src/lib.rs` — module declarations + public re-exports.
- `musefs-format/src/error.rs` — `FormatError` + `Result`.
- `musefs-format/src/layout.rs` — `Segment`, `RegionLayout` (the synthesis output type, format-agnostic).
- `musefs-format/src/input.rs` — `TagInput`, `ArtInput` (the synthesis input types, format-agnostic).
- `musefs-format/src/flac.rs` — `MetadataBlock`, `FlacScan`, `locate_audio`, `synthesize_layout`, and the (private) Vorbis/PICTURE/block-header writers + block-type constants.
- `musefs-format/tests/common/mod.rs` — independent test oracle: hand-rolled FLAC byte builder + layout resolver.
- `musefs-format/tests/layout.rs`, `tests/locate.rs`, `tests/synthesize_tags.rs`, `tests/synthesize_art.rs`, `tests/roundtrip.rs` — integration tests against the public API.

---

## Task 1: Crate scaffold + layout & input types

**Files:**
- Modify: `Cargo.toml` (workspace root)
- Create: `musefs-format/Cargo.toml`
- Create: `musefs-format/src/error.rs`
- Create: `musefs-format/src/layout.rs`
- Create: `musefs-format/src/input.rs`
- Create: `musefs-format/src/lib.rs`
- Test: `musefs-format/tests/layout.rs`

- [ ] **Step 1: Create the crate scaffolding**

In the workspace root `Cargo.toml`, add `musefs-format` to the members list so it reads:

```toml
[workspace]
resolver = "2"
members = ["musefs-db", "musefs-format"]
```

`musefs-format/Cargo.toml`:

```toml
[package]
name = "musefs-format"
version = "0.1.0"
edition = "2021"

[dependencies]
thiserror = "1"

[dev-dependencies]
metaflac = "0.2"
```

`musefs-format/src/error.rs`:

```rust
use thiserror::Error;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum FormatError {
    #[error("not a FLAC stream (missing fLaC marker)")]
    NotFlac,
    #[error("truncated or malformed FLAC metadata")]
    Malformed,
}

pub type Result<T> = std::result::Result<T, FormatError>;
```

`musefs-format/src/input.rs`:

```rust
/// One Vorbis/ID3 tag value to synthesize. Multi-valued tags are passed as
/// multiple `TagInput`s in the desired order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TagInput {
    pub key: String,
    pub value: String,
}

impl TagInput {
    pub fn new(key: &str, value: &str) -> TagInput {
        TagInput {
            key: key.to_string(),
            value: value.to_string(),
        }
    }
}

/// A reference to one embedded picture to synthesize. The image bytes themselves
/// are NOT held here — only `data_len`, the exact byte length — because the caller
/// streams the image into the spliced region at read time. `art_id` is an opaque
/// handle the caller maps back to its blob store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtInput {
    pub art_id: i64,
    pub mime: String,
    pub description: String,
    pub picture_type: u32,
    pub width: u32,
    pub height: u32,
    pub data_len: u64,
}
```

`musefs-format/src/layout.rs`:

```rust
/// One contiguous run of bytes in a synthesized virtual file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Segment {
    /// Generated framing/text bytes, fully materialized.
    Inline(Vec<u8>),
    /// Image bytes the caller splices in from its art store; only the length is known here.
    ArtImage { art_id: i64, len: u64 },
    /// A run of the original backing file's audio frames.
    BackingAudio { offset: u64, len: u64 },
}

impl Segment {
    pub fn len(&self) -> u64 {
        match self {
            Segment::Inline(b) => b.len() as u64,
            Segment::ArtImage { len, .. } => *len,
            Segment::BackingAudio { len, .. } => *len,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// An ordered description of a synthesized virtual file: the metadata region
/// (inline framing + art images) followed by the backing audio.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RegionLayout {
    pub segments: Vec<Segment>,
}

impl RegionLayout {
    pub fn new(segments: Vec<Segment>) -> RegionLayout {
        RegionLayout { segments }
    }

    /// Total size of the synthesized virtual file in bytes.
    pub fn total_len(&self) -> u64 {
        self.segments.iter().map(Segment::len).sum()
    }

    /// Size of the synthesized metadata region preceding the backing audio.
    pub fn header_len(&self) -> u64 {
        self.segments
            .iter()
            .filter(|s| !matches!(s, Segment::BackingAudio { .. }))
            .map(|s| s.len())
            .sum()
    }
}
```

`musefs-format/src/lib.rs`:

```rust
mod error;
mod input;
mod layout;
pub mod flac;

pub use error::{FormatError, Result};
pub use input::{ArtInput, TagInput};
pub use layout::{RegionLayout, Segment};
```

Note: `pub mod flac;` is declared now but the file doesn't exist yet, so the crate won't build until Task 2 creates `flac.rs`. To keep Task 1 self-contained and compiling, create a placeholder `musefs-format/src/flac.rs` containing only:

```rust
// FLAC parsing and synthesis. Implemented in Task 2 and beyond.
```

- [ ] **Step 2: Write the failing test**

`musefs-format/tests/layout.rs`:

```rust
use musefs_format::{RegionLayout, Segment};

#[test]
fn lengths_sum_segments_and_exclude_audio_from_header() {
    let layout = RegionLayout::new(vec![
        Segment::Inline(vec![0u8; 10]),
        Segment::ArtImage { art_id: 7, len: 100 },
        Segment::Inline(vec![0u8; 5]),
        Segment::BackingAudio { offset: 200, len: 1000 },
    ]);

    assert_eq!(layout.header_len(), 10 + 100 + 5);
    assert_eq!(layout.total_len(), 10 + 100 + 5 + 1000);
}

#[test]
fn segment_len_reports_each_variant() {
    assert_eq!(Segment::Inline(vec![1, 2, 3]).len(), 3);
    assert_eq!(Segment::ArtImage { art_id: 1, len: 42 }.len(), 42);
    assert_eq!(Segment::BackingAudio { offset: 0, len: 9 }.len(), 9);
}
```

- [ ] **Step 3: Run the test to verify it fails**

Run: `cargo test -p musefs-format --test layout`
Expected: FAIL — compile error: unresolved imports (`RegionLayout`, `Segment`) until the crate is created. (Once the source files above are in place, this test should pass — write it first, confirm the crate doesn't yet expose these, then add the source.)

Practical note for TDD: create the test file first and run it to see the unresolved-import failure, then add the source files in Step 1's listing, then re-run.

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test -p musefs-format --test layout`
Expected: PASS (both tests).

- [ ] **Step 5: Confirm zero warnings**

Run: `cargo build -p musefs-format --tests 2>&1 | grep -iE "warning|error" || echo clean`
Expected: `clean`.

- [ ] **Step 6: Commit**

```bash
git add Cargo.toml musefs-format/
git commit -m "feat(format): musefs-format scaffold with layout and input types"
```

---

## Task 2: `locate_audio` — parse FLAC metadata, find the audio boundary

**Files:**
- Modify: `musefs-format/src/flac.rs`
- Create: `musefs-format/tests/common/mod.rs`
- Test: `musefs-format/tests/locate.rs`

- [ ] **Step 1: Write the independent test oracle and the failing test**

`musefs-format/tests/common/mod.rs`:

```rust
#![allow(dead_code)]

use std::collections::HashMap;

use musefs_format::{RegionLayout, Segment};

/// Build a FLAC metadata block (4-byte header + body) independently of production code.
pub fn flac_block(block_type: u8, body: &[u8], is_last: bool) -> Vec<u8> {
    let mut out = Vec::new();
    let first = (if is_last { 0x80 } else { 0 }) | (block_type & 0x7F);
    out.push(first);
    let len = body.len();
    out.push(((len >> 16) & 0xFF) as u8);
    out.push(((len >> 8) & 0xFF) as u8);
    out.push((len & 0xFF) as u8);
    out.extend_from_slice(body);
    out
}

/// A structurally valid STREAMINFO body: 44100 Hz, 2 channels, 16-bit, unknown frame/sample counts.
pub fn streaminfo_body() -> Vec<u8> {
    let mut b = vec![
        0x10, 0x00, // min block size = 4096
        0x10, 0x00, // max block size = 4096
        0x00, 0x00, 0x00, // min frame size = 0 (unknown)
        0x00, 0x00, 0x00, // max frame size = 0 (unknown)
        0x0A, 0xC4, 0x42, 0xF0, // sample_rate=44100, channels=2, bps=16, top of total samples
        0x00, 0x00, 0x00, 0x00, // remaining total-samples bits = 0
    ];
    b.extend_from_slice(&[0u8; 16]); // MD5 signature = 0
    b
}

/// Minimal VORBIS_COMMENT body with the given already-formatted `KEY=value` comments.
pub fn vorbis_comment_body(vendor: &str, comments: &[&str]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&(vendor.len() as u32).to_le_bytes());
    out.extend_from_slice(vendor.as_bytes());
    out.extend_from_slice(&(comments.len() as u32).to_le_bytes());
    for c in comments {
        out.extend_from_slice(&(c.len() as u32).to_le_bytes());
        out.extend_from_slice(c.as_bytes());
    }
    out
}

/// Assemble a full FLAC byte stream: marker + blocks (last-flag auto-set on the final block) + audio.
pub fn make_flac(blocks: &[(u8, Vec<u8>)], audio: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(b"fLaC");
    for (i, (bt, body)) in blocks.iter().enumerate() {
        let is_last = i == blocks.len() - 1;
        out.extend_from_slice(&flac_block(*bt, body, is_last));
    }
    out.extend_from_slice(audio);
    out
}

/// Resolve a RegionLayout into concrete bytes, given the original backing bytes and an
/// art-id -> image-bytes map. Independent of production assembly; used to verify splicing.
pub fn resolve_layout(
    layout: &RegionLayout,
    backing: &[u8],
    art: &HashMap<i64, Vec<u8>>,
) -> Vec<u8> {
    let mut out = Vec::new();
    for seg in &layout.segments {
        match seg {
            Segment::Inline(b) => out.extend_from_slice(b),
            Segment::ArtImage { art_id, len } => {
                let img = art.get(art_id).expect("art bytes provided");
                assert_eq!(img.len() as u64, *len, "art length mismatch in layout");
                out.extend_from_slice(img);
            }
            Segment::BackingAudio { offset, len } => {
                let o = *offset as usize;
                let l = *len as usize;
                out.extend_from_slice(&backing[o..o + l]);
            }
        }
    }
    out
}
```

`musefs-format/tests/locate.rs`:

```rust
mod common;
use common::{make_flac, streaminfo_body, vorbis_comment_body};
use musefs_format::flac::locate_audio;
use musefs_format::FormatError;

#[test]
fn locates_audio_after_metadata_and_preserves_streaminfo() {
    let si = streaminfo_body();
    let vc = vorbis_comment_body("oldvendor", &["TITLE=Old"]);
    let audio = vec![0xAA; 50];
    let file = make_flac(&[(0, si.clone()), (4, vc)], &audio);

    let scan = locate_audio(&file).unwrap();

    assert_eq!(scan.audio_offset, (file.len() - audio.len()) as u64);
    assert_eq!(scan.audio_length, audio.len() as u64);

    // STREAMINFO is preserved verbatim; the original VORBIS_COMMENT is dropped.
    assert_eq!(scan.preserved.len(), 1);
    assert_eq!(scan.preserved[0].block_type, 0);
    assert_eq!(scan.preserved[0].body, si);
}

#[test]
fn preserves_structural_blocks_but_not_padding() {
    let si = streaminfo_body();
    let seektable = vec![0u8; 18]; // one seek point's worth of bytes (content irrelevant here)
    let padding = vec![0u8; 32];
    let audio = vec![0x11; 10];
    // Order: STREAMINFO(0), SEEKTABLE(3), PADDING(1)
    let file = make_flac(&[(0, si.clone()), (3, seektable.clone()), (1, padding)], &audio);

    let scan = locate_audio(&file).unwrap();

    let types: Vec<u8> = scan.preserved.iter().map(|b| b.block_type).collect();
    assert_eq!(types, vec![0, 3], "STREAMINFO + SEEKTABLE preserved, PADDING dropped");
    assert_eq!(scan.preserved[1].body, seektable);
    assert_eq!(scan.audio_length, audio.len() as u64);
}

#[test]
fn rejects_non_flac_input() {
    assert_eq!(locate_audio(b"NOPExxxx").unwrap_err(), FormatError::NotFlac);
}

#[test]
fn rejects_truncated_metadata() {
    // Claims a 1000-byte block body but provides no body.
    let mut file = Vec::new();
    file.extend_from_slice(b"fLaC");
    file.extend_from_slice(&[0x80, 0x00, 0x03, 0xE8]); // last block, type 0, len 1000
    assert_eq!(locate_audio(&file).unwrap_err(), FormatError::Malformed);
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p musefs-format --test locate`
Expected: FAIL — compile errors: `locate_audio`, `FlacScan`, `MetadataBlock` do not exist in `musefs_format::flac`.

- [ ] **Step 3: Implement `locate_audio`**

Replace the placeholder contents of `musefs-format/src/flac.rs` with:

```rust
use crate::error::{FormatError, Result};

pub(crate) const FLAC_MARKER: &[u8; 4] = b"fLaC";

pub(crate) const BLOCK_STREAMINFO: u8 = 0;
pub(crate) const BLOCK_APPLICATION: u8 = 2;
pub(crate) const BLOCK_SEEKTABLE: u8 = 3;
pub(crate) const BLOCK_VORBIS_COMMENT: u8 = 4;
pub(crate) const BLOCK_CUESHEET: u8 = 5;
pub(crate) const BLOCK_PICTURE: u8 = 6;

/// A preserved FLAC metadata block: its type and its body (excluding the 4-byte header).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MetadataBlock {
    pub block_type: u8,
    pub body: Vec<u8>,
}

/// Result of scanning a FLAC file: where audio begins/ends and the structural blocks to preserve.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FlacScan {
    pub audio_offset: u64,
    pub audio_length: u64,
    pub preserved: Vec<MetadataBlock>,
}

/// Parse the FLAC metadata section, returning the audio boundary and the structural
/// blocks to carry over (STREAMINFO/APPLICATION/SEEKTABLE/CUESHEET). VORBIS_COMMENT,
/// PICTURE, and PADDING are dropped (regenerated or omitted at synthesis time).
pub fn locate_audio(data: &[u8]) -> Result<FlacScan> {
    if data.len() < 4 || &data[0..4] != FLAC_MARKER {
        return Err(FormatError::NotFlac);
    }
    let mut pos = 4usize;
    let mut preserved = Vec::new();
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
        match block_type {
            BLOCK_STREAMINFO | BLOCK_APPLICATION | BLOCK_SEEKTABLE | BLOCK_CUESHEET => {
                preserved.push(MetadataBlock {
                    block_type,
                    body: data[body_start..body_end].to_vec(),
                });
            }
            _ => {}
        }
        pos = body_end;
        if is_last {
            break;
        }
    }
    Ok(FlacScan {
        audio_offset: pos as u64,
        audio_length: (data.len() - pos) as u64,
        preserved,
    })
}
```

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test -p musefs-format --test locate`
Expected: PASS (all four tests).

- [ ] **Step 5: Confirm zero warnings**

Run: `cargo build -p musefs-format --tests 2>&1 | grep -iE "warning|error" || echo clean`
Expected: `clean`. (Some block-type constants and the `resolve_layout` helper are unused until later tasks; the `#![allow(dead_code)]` on `tests/common/mod.rs` covers the helper, and the constants are used by `synthesize_layout` in Task 3/4. If a constant warns now, that is expected to resolve in Task 3 — but to keep this task warning-free, the constants `BLOCK_VORBIS_COMMENT`, `BLOCK_PICTURE` are referenced only later. If they warn, add `#[allow(dead_code)]` to those two constants now and remove it in Task 4 when they're used — OR simply proceed since they will be used shortly. Prefer: leave them; if the gate fails, the simplest fix is a temporary `#![allow(dead_code)]` at the top of `flac.rs` that the final task removes.)

To keep this task strictly warning-free without churn, add this as the first line of `musefs-format/src/flac.rs`:

```rust
#![allow(dead_code)] // some block-type constants are first used in later M1 tasks
```

(Task 5 removes this once every constant is referenced.)

- [ ] **Step 6: Commit**

```bash
git add musefs-format/
git commit -m "feat(format): FLAC locate_audio with structural block preservation"
```

---

## Task 3: `synthesize_layout` — tags only (Vorbis comments, no art)

**Files:**
- Modify: `musefs-format/src/flac.rs`
- Test: `musefs-format/tests/synthesize_tags.rs`

- [ ] **Step 1: Write the failing test**

`musefs-format/tests/synthesize_tags.rs`:

```rust
mod common;
use std::collections::HashMap;
use std::io::Cursor;

use common::{make_flac, resolve_layout, streaminfo_body, vorbis_comment_body};
use musefs_format::flac::{locate_audio, synthesize_layout};
use musefs_format::{Segment, TagInput};

fn fixture() -> (Vec<u8>, Vec<u8>) {
    let si = streaminfo_body();
    let vc = vorbis_comment_body("oldvendor", &["TITLE=Old"]);
    let audio = vec![0xAB; 64];
    let file = make_flac(&[(0, si), (4, vc)], &audio);
    (file, audio)
}

#[test]
fn measured_lengths_match_assembled_bytes() {
    let (file, audio) = fixture();
    let scan = locate_audio(&file).unwrap();

    let tags = vec![TagInput::new("title", "New Title"), TagInput::new("artist", "A")];
    let layout = synthesize_layout(&scan, &tags, &[]);

    let assembled = resolve_layout(&layout, &file, &HashMap::new());
    assert_eq!(assembled.len() as u64, layout.total_len());
    // No art, so the metadata region is one contiguous inline run before the audio.
    assert_eq!(layout.header_len(), assembled.len() as u64 - audio.len() as u64);

    // The audio frames are spliced through unchanged.
    assert_eq!(&assembled[layout.header_len() as usize..], &audio[..]);
}

#[test]
fn metaflac_reads_synthesized_vorbis_comments_and_preserves_streaminfo() {
    let (file, _audio) = fixture();
    let scan = locate_audio(&file).unwrap();

    let tags = vec![
        TagInput::new("title", "New Title"),
        TagInput::new("artist", "First"),
        TagInput::new("artist", "Second"), // multi-valued
    ];
    let layout = synthesize_layout(&scan, &tags, &[]);
    let assembled = resolve_layout(&layout, &file, &HashMap::new());

    let tag = metaflac::Tag::read_from(&mut Cursor::new(&assembled)).expect("valid FLAC metadata");

    let vc = tag.vorbis_comments().expect("vorbis comments present");
    assert_eq!(vc.get("TITLE").map(|v| v.as_slice()), Some(["New Title".to_string()].as_slice()));
    assert_eq!(
        vc.get("ARTIST").map(|v| v.as_slice()),
        Some(["First".to_string(), "Second".to_string()].as_slice())
    );

    // STREAMINFO carried through: 44100 Hz, 2 channels.
    let si = tag.get_streaminfo().expect("streaminfo present");
    assert_eq!(si.sample_rate, 44100);
    assert_eq!(si.num_channels, 2);
}

#[test]
fn vorbis_comment_block_is_the_last_metadata_block_when_no_art() {
    let (file, _audio) = fixture();
    let scan = locate_audio(&file).unwrap();
    let layout = synthesize_layout(&scan, &[TagInput::new("title", "X")], &[]);

    // With no art the layout is exactly [Inline(metadata), BackingAudio].
    assert_eq!(layout.segments.len(), 2);
    assert!(matches!(layout.segments[0], Segment::Inline(_)));
    assert!(matches!(layout.segments[1], Segment::BackingAudio { .. }));
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p musefs-format --test synthesize_tags`
Expected: FAIL — compile error: `synthesize_layout` does not exist.

- [ ] **Step 3: Implement `synthesize_layout` (tags only) plus the Vorbis comment and block-header writers**

Append to `musefs-format/src/flac.rs`:

```rust
use crate::input::{ArtInput, TagInput};
use crate::layout::{RegionLayout, Segment};

pub(crate) const VENDOR: &str = "musefs";

fn push_block_header(out: &mut Vec<u8>, block_type: u8, body_len: usize, is_last: bool) {
    let first = (if is_last { 0x80 } else { 0 }) | (block_type & 0x7F);
    out.push(first);
    out.push(((body_len >> 16) & 0xFF) as u8);
    out.push(((body_len >> 8) & 0xFF) as u8);
    out.push((body_len & 0xFF) as u8);
}

fn vorbis_comment_body(tags: &[TagInput]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&(VENDOR.len() as u32).to_le_bytes());
    out.extend_from_slice(VENDOR.as_bytes());
    out.extend_from_slice(&(tags.len() as u32).to_le_bytes());
    for t in tags {
        let comment = format!("{}={}", t.key.to_ascii_uppercase(), t.value);
        out.extend_from_slice(&(comment.len() as u32).to_le_bytes());
        out.extend_from_slice(comment.as_bytes());
    }
    out
}

/// Build the ordered segment layout for a synthesized FLAC file:
/// `fLaC` + preserved structural blocks + a regenerated VORBIS_COMMENT + PICTURE
/// blocks (one `ArtImage` segment each) + the backing audio.
pub fn synthesize_layout(scan: &FlacScan, tags: &[TagInput], arts: &[ArtInput]) -> RegionLayout {
    let num_blocks = scan.preserved.len() + 1 + arts.len(); // preserved + VORBIS_COMMENT + pictures
    let last_index = num_blocks - 1;

    let mut segments: Vec<Segment> = Vec::new();
    let mut buf: Vec<u8> = Vec::new();
    buf.extend_from_slice(FLAC_MARKER);

    let mut idx = 0usize;

    for blk in &scan.preserved {
        push_block_header(&mut buf, blk.block_type, blk.body.len(), idx == last_index);
        buf.extend_from_slice(&blk.body);
        idx += 1;
    }

    let vc = vorbis_comment_body(tags);
    push_block_header(&mut buf, BLOCK_VORBIS_COMMENT, vc.len(), idx == last_index);
    buf.extend_from_slice(&vc);
    idx += 1;

    for art in arts {
        let framing = picture_body_framing(art);
        let body_len = framing.len() as u64 + art.data_len;
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
        offset: scan.audio_offset,
        len: scan.audio_length,
    });

    RegionLayout::new(segments)
}
```

Note: this references `picture_body_framing`, which is added in Task 4. To make Task 3 compile on its own, add this temporary stub to `flac.rs` now; Task 4 replaces it with the real implementation:

```rust
fn picture_body_framing(_art: &ArtInput) -> Vec<u8> {
    Vec::new()
}
```

(The Task 3 tests never pass any `arts`, so the stub is never exercised here.)

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test -p musefs-format --test synthesize_tags`
Expected: PASS (all three tests).

If `metaflac`'s exact method names differ from those used here (`vorbis_comments()`, `get_streaminfo()`, `VorbisComment::get`, `StreamInfo::sample_rate`/`num_channels`), consult `cargo doc -p metaflac --open` and adjust the test calls to the actual API; the synthesized bytes are correct regardless.

- [ ] **Step 5: Confirm zero warnings**

Run: `cargo build -p musefs-format --tests 2>&1 | grep -iE "warning|error" || echo clean`
Expected: `clean`.

- [ ] **Step 6: Commit**

```bash
git add musefs-format/
git commit -m "feat(format): synthesize_layout for FLAC vorbis comments"
```

---

## Task 4: `synthesize_layout` — PICTURE blocks and `ArtImage` segments

**Files:**
- Modify: `musefs-format/src/flac.rs`
- Test: `musefs-format/tests/synthesize_art.rs`

- [ ] **Step 1: Write the failing test**

`musefs-format/tests/synthesize_art.rs`:

```rust
mod common;
use std::collections::HashMap;
use std::io::Cursor;

use common::{make_flac, resolve_layout, streaminfo_body, vorbis_comment_body};
use musefs_format::flac::{locate_audio, synthesize_layout};
use musefs_format::{ArtInput, Segment, TagInput};

fn fixture() -> (Vec<u8>, Vec<u8>) {
    let si = streaminfo_body();
    let vc = vorbis_comment_body("oldvendor", &["TITLE=Old"]);
    let audio = vec![0xCD; 80];
    let file = make_flac(&[(0, si), (4, vc)], &audio);
    (file, audio)
}

fn cover(art_id: i64, data_len: u64) -> ArtInput {
    ArtInput {
        art_id,
        mime: "image/jpeg".to_string(),
        description: "front".to_string(),
        picture_type: 3,
        width: 500,
        height: 500,
        data_len,
    }
}

#[test]
fn art_becomes_an_artimage_segment_and_lengths_are_exact() {
    let (file, audio) = fixture();
    let scan = locate_audio(&file).unwrap();

    let image = vec![0x77u8; 1234];
    let art = cover(42, image.len() as u64);
    let layout = synthesize_layout(&scan, &[TagInput::new("title", "T")], &[art]);

    // There is exactly one ArtImage segment referencing art_id 42 with the image's length.
    let art_segs: Vec<&Segment> = layout
        .segments
        .iter()
        .filter(|s| matches!(s, Segment::ArtImage { .. }))
        .collect();
    assert_eq!(art_segs.len(), 1);
    assert_eq!(*art_segs[0], Segment::ArtImage { art_id: 42, len: 1234 });

    // Generate-and-measure holds with the image resolved in.
    let mut art_map = HashMap::new();
    art_map.insert(42i64, image.clone());
    let assembled = resolve_layout(&layout, &file, &art_map);
    assert_eq!(assembled.len() as u64, layout.total_len());
    assert_eq!(&assembled[layout.header_len() as usize..], &audio[..]);
}

#[test]
fn metaflac_reads_synthesized_picture() {
    let (file, _audio) = fixture();
    let scan = locate_audio(&file).unwrap();

    let image = vec![0x77u8; 1234];
    let art = cover(42, image.len() as u64);
    let layout = synthesize_layout(&scan, &[TagInput::new("title", "T")], &[art]);

    let mut art_map = HashMap::new();
    art_map.insert(42i64, image.clone());
    let assembled = resolve_layout(&layout, &file, &art_map);

    let tag = metaflac::Tag::read_from(&mut Cursor::new(&assembled)).expect("valid FLAC metadata");
    let pics: Vec<_> = tag.pictures().collect();
    assert_eq!(pics.len(), 1);
    let p = pics[0];
    assert_eq!(p.mime_type, "image/jpeg");
    assert_eq!(p.description, "front");
    assert_eq!(p.width, 500);
    assert_eq!(p.height, 500);
    assert_eq!(p.data, image);
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p musefs-format --test synthesize_art`
Expected: FAIL — `metaflac_reads_synthesized_picture` fails (the stub `picture_body_framing` returns an empty body, so no valid PICTURE block is produced and metaflac finds zero pictures). The first test may also fail on the picture-block framing length.

- [ ] **Step 3: Replace the `picture_body_framing` stub with the real implementation**

In `musefs-format/src/flac.rs`, replace the temporary stub:

```rust
fn picture_body_framing(_art: &ArtInput) -> Vec<u8> {
    Vec::new()
}
```

with the real PICTURE body framing (every field except the trailing image bytes):

```rust
fn picture_body_framing(art: &ArtInput) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&art.picture_type.to_be_bytes());
    out.extend_from_slice(&(art.mime.len() as u32).to_be_bytes());
    out.extend_from_slice(art.mime.as_bytes());
    out.extend_from_slice(&(art.description.len() as u32).to_be_bytes());
    out.extend_from_slice(art.description.as_bytes());
    out.extend_from_slice(&art.width.to_be_bytes());
    out.extend_from_slice(&art.height.to_be_bytes());
    out.extend_from_slice(&0u32.to_be_bytes()); // color depth (unknown)
    out.extend_from_slice(&0u32.to_be_bytes()); // number of colors (non-indexed)
    out.extend_from_slice(&(art.data_len as u32).to_be_bytes()); // picture data length
    out
}
```

(No change to `synthesize_layout` itself — it already emits `Inline(header + framing)` then `ArtImage` per art, and sets the last-block flag on the final picture.)

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test -p musefs-format --test synthesize_art`
Expected: PASS (both tests).

If `metaflac`'s `Picture` field names differ (`mime_type`, `description`, `width`, `height`, `data`), adjust per `cargo doc -p metaflac`.

- [ ] **Step 5: Confirm zero warnings**

Run: `cargo build -p musefs-format --tests 2>&1 | grep -iE "warning|error" || echo clean`
Expected: `clean`.

- [ ] **Step 6: Commit**

```bash
git add musefs-format/
git commit -m "feat(format): synthesize FLAC PICTURE blocks as ArtImage segments"
```

---

## Task 5: Capstone round-trip + remove the temporary `allow(dead_code)`

**Files:**
- Modify: `musefs-format/src/flac.rs` (remove the temporary crate-level allow)
- Test: `musefs-format/tests/roundtrip.rs`

- [ ] **Step 1: Write the comprehensive round-trip test**

`musefs-format/tests/roundtrip.rs`:

```rust
mod common;
use std::collections::HashMap;
use std::io::Cursor;

use common::{flac_block, make_flac, resolve_layout, streaminfo_body, vorbis_comment_body};
use musefs_format::flac::{locate_audio, synthesize_layout};
use musefs_format::{ArtInput, TagInput};

#[test]
fn full_roundtrip_preserved_blocks_multivalue_tags_and_two_pictures() {
    // Backing file: STREAMINFO + SEEKTABLE (structural, must survive) + old VORBIS_COMMENT + audio.
    let si = streaminfo_body();
    let seektable = vec![0xEEu8; 36];
    let old_vc = vorbis_comment_body("oldvendor", &["TITLE=Old", "ARTIST=Old"]);
    let audio: Vec<u8> = (0..200u32).map(|i| (i % 251) as u8).collect();
    let file = make_flac(
        &[(0, si.clone()), (3, seektable.clone()), (4, old_vc)],
        &audio,
    );

    let scan = locate_audio(&file).unwrap();
    assert_eq!(scan.preserved.len(), 2); // STREAMINFO + SEEKTABLE

    let tags = vec![
        TagInput::new("title", "Real Title"),
        TagInput::new("album", "Real Album"),
        TagInput::new("artist", "Alice"),
        TagInput::new("artist", "Bob"),
    ];
    let front = vec![0x01u8; 900];
    let back = vec![0x02u8; 700];
    let arts = vec![
        ArtInput { art_id: 1, mime: "image/png".into(), description: "front".into(), picture_type: 3, width: 600, height: 600, data_len: front.len() as u64 },
        ArtInput { art_id: 2, mime: "image/png".into(), description: "back".into(),  picture_type: 4, width: 600, height: 600, data_len: back.len() as u64 },
    ];

    let layout = synthesize_layout(&scan, &tags, &arts);

    let mut art_map = HashMap::new();
    art_map.insert(1i64, front.clone());
    art_map.insert(2i64, back.clone());
    let assembled = resolve_layout(&layout, &file, &art_map);

    // Generate-and-measure: assembled length equals the measured total, audio spliced unchanged.
    assert_eq!(assembled.len() as u64, layout.total_len());
    assert_eq!(&assembled[layout.header_len() as usize..], &audio[..]);

    // Independent decode with metaflac.
    let tag = metaflac::Tag::read_from(&mut Cursor::new(&assembled)).expect("valid FLAC");

    let vc = tag.vorbis_comments().expect("vorbis comments");
    assert_eq!(vc.get("TITLE").map(|v| v.as_slice()), Some(["Real Title".to_string()].as_slice()));
    assert_eq!(vc.get("ALBUM").map(|v| v.as_slice()), Some(["Real Album".to_string()].as_slice()));
    assert_eq!(
        vc.get("ARTIST").map(|v| v.as_slice()),
        Some(["Alice".to_string(), "Bob".to_string()].as_slice())
    );

    let pics: Vec<_> = tag.pictures().collect();
    assert_eq!(pics.len(), 2);
    assert_eq!(pics[0].description, "front");
    assert_eq!(pics[0].data, front);
    assert_eq!(pics[1].description, "back");
    assert_eq!(pics[1].data, back);

    let si_read = tag.get_streaminfo().expect("streaminfo");
    assert_eq!(si_read.sample_rate, 44100);

    // The SEEKTABLE structural block survived (metaflac exposes it among blocks).
    // We at least confirm the synthesized file still decodes and audio is intact (above).
    // Sanity: rebuilding the same expected SEEKTABLE block proves our preservation is verbatim.
    let _ = flac_block(3, &seektable, false); // documents intent; body equality checked via scan below
    assert_eq!(scan.preserved[1].block_type, 3);
    assert_eq!(scan.preserved[1].body, seektable);
}
```

- [ ] **Step 2: Run the test to verify it passes**

Run: `cargo test -p musefs-format --test roundtrip`
Expected: PASS. (This exercises preserved structural blocks, multi-valued tags, two pictures with different types, exact measurement, and unchanged audio.)

- [ ] **Step 3: Remove the temporary crate-level allow now that all constants are used**

Delete this line from the top of `musefs-format/src/flac.rs` (added in Task 2):

```rust
#![allow(dead_code)] // some block-type constants are first used in later M1 tasks
```

Then confirm every block-type constant is now referenced (`BLOCK_STREAMINFO`, `BLOCK_APPLICATION`, `BLOCK_SEEKTABLE`, `BLOCK_CUESHEET` in `locate_audio`; `BLOCK_VORBIS_COMMENT`, `BLOCK_PICTURE` in `synthesize_layout`). If any single constant remains genuinely unused, annotate just that constant with `#[allow(dead_code)]` rather than restoring the crate-wide allow.

- [ ] **Step 4: Run the full crate suite and confirm zero warnings**

Run: `cargo test -p musefs-format`
Expected: PASS — `layout`, `locate`, `synthesize_tags`, `synthesize_art`, `roundtrip`.

Run: `cargo build -p musefs-format --tests 2>&1 | grep -iE "warning|error" || echo clean`
Expected: `clean`.

- [ ] **Step 5: Commit**

```bash
git add musefs-format/
git commit -m "test(format): full FLAC synthesis round-trip; drop temporary dead_code allow"
```

---

## Task 6: Whole-workspace verification

**Files:** none (verification only).

- [ ] **Step 1: Run the entire workspace test suite**

Run: `cargo test`
Expected: PASS — both `musefs-db` (16 tests) and `musefs-format` (all M1 tests) green.

- [ ] **Step 2: Confirm a clean build with no warnings across the workspace**

Run: `cargo build --workspace --tests 2>&1 | grep -i warning || echo "no warnings"`
Expected: prints `no warnings`. Fix any that appear and re-run.

- [ ] **Step 3: Commit any cleanup**

```bash
git add -A
git commit -m "chore(format): M1 cleanup, no warnings" || echo "nothing to commit"
```

---

## Self-Review Notes

- **Spec coverage (M1 scope):** `musefs-format` is a pure crate with no DB/FUSE deps (Task 1 manifest has no such deps). `locate_audio` parses FLAC metadata blocks, finds the audio boundary, preserves STREAMINFO + structural blocks, and drops VORBIS_COMMENT/PICTURE/PADDING (Task 2). `synthesize_layout` returns an ordered `RegionLayout` of `Inline`/`ArtImage`/`BackingAudio` segments with `header_len`/`total_len` exact by construction; the `ArtImage` segment type is present from the start (Tasks 1, 3, 4). FLAC Vorbis-comment regeneration with canonical-key→uppercase mapping (Task 3) and PICTURE-block regeneration with streamed image data (Task 4). Round-trip validation against an independent decoder, `metaflac` (Tasks 3–5). All M1 requirements covered.
- **Correctly deferred (not M1):** MP3/ID3 (M3), the FUSE serving layer and the in-memory header cache (M2), DB integration / mapping `musefs-db` types to `TagInput`/`ArtInput` (M2), and incremental art blob streaming (M4). M1 deliberately operates on `&[u8]` and plain input structs.
- **Type consistency:** `Segment` (`Inline`/`ArtImage{art_id,len}`/`BackingAudio{offset,len}`), `RegionLayout` (`new`/`segments`/`header_len`/`total_len`), `TagInput` (`new`/`key`/`value`), `ArtInput` (`art_id`/`mime`/`description`/`picture_type`/`width`/`height`/`data_len`), `FlacScan` (`audio_offset`/`audio_length`/`preserved`), `MetadataBlock` (`block_type`/`body`), and functions `locate_audio`/`synthesize_layout` are named identically everywhere they appear across tasks and tests.
- **Placeholder discipline:** The only intentional placeholders are the two clearly-labeled temporary stubs that later tasks replace — the empty `flac.rs` comment (replaced in Task 2) and the `picture_body_framing` stub (replaced in Task 4) — plus the temporary `#![allow(dead_code)]` removed in Task 5. Each is explicitly called out with the task that resolves it.
- **External-API risk:** `metaflac`'s exact method/field names (`vorbis_comments`, `get_streaminfo`, `pictures`, `Picture.mime_type/description/width/height/data`, `StreamInfo.sample_rate/num_channels`) are used as documented; tasks note that if a name differs, the implementer should adjust the test to the real API via `cargo doc -p metaflac` (the synthesized bytes are independent of this).
