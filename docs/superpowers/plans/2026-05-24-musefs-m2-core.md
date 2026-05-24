# musefs M2 (Core) — Read Path, Tree, Templates & Scan Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build the `musefs-core` crate — everything needed to turn the SQLite store into a navigable, byte-serving virtual filesystem — plus the small `musefs-format` read-side additions it needs, all unit-testable without FUSE.

**Architecture:** `musefs-core` composes `musefs-db` (the store) and `musefs-format` (FLAC synthesis) into a `Musefs` facade: it builds an in-memory inode tree from path templates applied to each track's tags, lazily synthesizes each file's `RegionLayout` (caching it by `(track_id, content_version)`), and resolves arbitrary `(offset, size)` reads by splicing inline framing with positioned reads of the backing file. A naive directory scanner populates the store. The read path re-reads only the file *front* (`audio_offset` bytes, known from the DB) to recover preserved metadata blocks, never the audio. FUSE will mount this single-threaded, so the facade uses `&mut self`.

**Tech Stack:** Rust (edition 2021); `musefs-db`, `musefs-format`, `thiserror`; dev-deps `tempfile`, `metaflac`. Positioned reads via `std::os::unix::fs::FileExt` (Linux/Unix target).

---

## File Structure

- `Cargo.toml` (workspace root) — add `musefs-core` to `members`.
- `musefs-format/src/flac.rs` — add `read_metadata` + `FlacMeta` (refactor shared block parsing) and `read_vorbis_comments`.
- `musefs-format/src/lib.rs` — export the new items (via `pub mod flac`, already public).
- `musefs-core/Cargo.toml` — crate manifest.
- `musefs-core/src/lib.rs` — module declarations + re-exports.
- `musefs-core/src/error.rs` — `CoreError` + `Result`.
- `musefs-core/src/template.rs` — `render_path` (path template engine).
- `musefs-core/src/mapping.rs` — `tags_to_inputs`, `tags_to_fields` (DB rows → synthesis/template inputs).
- `musefs-core/src/tree.rs` — `VirtualTree`, `Node`, `NodeKind` (inode model + collision disambiguation).
- `musefs-core/src/reader.rs` — `ResolvedFile`, `HeaderCache`, `read_at` (lazy synthesis cache + segment read resolution).
- `musefs-core/src/scan.rs` — `ScanStats`, `scan_directory` (naive FLAC scanner).
- `musefs-core/src/facade.rs` — `Musefs`, `MountConfig`, `Attr` (the composed read-only filesystem API).
- `musefs-core/tests/common/mod.rs` — FLAC fixture builders (independent) + temp-DB helpers.
- `musefs-core/tests/*.rs` — integration tests per module.

---

## Task 1: `musefs-format` — `read_metadata` (parse the metadata region only)

The mount read path knows `audio_offset` from the DB and reads only that many bytes from the file front; it must recover the preserved blocks without `locate_audio`'s requirement of the whole file (which it needs to compute `audio_length`). Refactor the shared block-walk into `parse_blocks`, keep `locate_audio` behavior identical, and add `read_metadata`.

**Files:**
- Modify: `musefs-format/src/flac.rs`
- Test: `musefs-format/tests/read_metadata.rs`

- [ ] **Step 1: Write the failing test**

`musefs-format/tests/read_metadata.rs`:

```rust
mod common;
use common::{make_flac, streaminfo_body, vorbis_comment_body};
use musefs_format::flac::{locate_audio, read_metadata};

#[test]
fn read_metadata_on_front_bytes_recovers_preserved_and_offset() {
    let si = streaminfo_body();
    let vc = vorbis_comment_body("v", &["TITLE=X"]);
    let audio = vec![0xAA; 40];
    let file = make_flac(&[(0, si.clone()), (4, vc)], &audio);

    // The DB would store audio_offset; emulate reading only the front.
    let scan = locate_audio(&file).unwrap();
    let front = &file[..scan.audio_offset as usize];

    let meta = read_metadata(front).unwrap();
    assert_eq!(meta.audio_offset, scan.audio_offset);
    assert_eq!(meta.preserved, scan.preserved); // STREAMINFO only
}

#[test]
fn locate_audio_still_reports_audio_length() {
    let si = streaminfo_body();
    let audio = vec![0x11; 99];
    let file = make_flac(&[(0, si)], &audio);
    let scan = locate_audio(&file).unwrap();
    assert_eq!(scan.audio_length, 99);
}
```

This test needs the same `tests/common/mod.rs` the M1 tests use. It already exists at `musefs-format/tests/common/mod.rs` — reuse it as-is.

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p musefs-format --test read_metadata`
Expected: FAIL — compile error: `read_metadata` / `FlacMeta` not found.

- [ ] **Step 3: Refactor and add `read_metadata`**

In `musefs-format/src/flac.rs`, add the `FlacMeta` type and refactor. Replace the existing `locate_audio` function (lines beginning `pub fn locate_audio`) with a shared `parse_blocks` helper plus thin wrappers:

```rust
/// The metadata region of a FLAC file: where audio begins and the structural
/// blocks to carry over. Unlike `FlacScan`, this does not include `audio_length`
/// (which requires the full file size), so it can be computed from the front alone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FlacMeta {
    pub audio_offset: u64,
    pub preserved: Vec<MetadataBlock>,
}

fn parse_blocks(data: &[u8]) -> Result<FlacMeta> {
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
    Ok(FlacMeta {
        audio_offset: pos as u64,
        preserved,
    })
}

/// Parse just the FLAC metadata region (the front of the file), recovering the
/// audio boundary and structural blocks. Use when the audio length is already
/// known (e.g. stored in a database) and the full file should not be read.
pub fn read_metadata(data: &[u8]) -> Result<FlacMeta> {
    parse_blocks(data)
}

/// Parse the FLAC metadata section of a complete file, returning the audio
/// boundary, audio length, and the structural blocks to carry over.
pub fn locate_audio(data: &[u8]) -> Result<FlacScan> {
    let meta = parse_blocks(data)?;
    Ok(FlacScan {
        audio_offset: meta.audio_offset,
        audio_length: data.len() as u64 - meta.audio_offset,
        preserved: meta.preserved,
    })
}
```

(Delete the old body of `locate_audio` — the loop now lives in `parse_blocks`. Keep `FlacScan`, `MetadataBlock`, and all constants unchanged.)

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p musefs-format`
Expected: PASS — the new `read_metadata` tests AND all existing M1 tests (locate/synthesize/roundtrip) still green, confirming the refactor preserved behavior.

- [ ] **Step 5: Confirm zero warnings**

Run: `cargo build -p musefs-format --tests 2>&1 | grep -iE "warning|error" || echo clean`
Expected: `clean`.

- [ ] **Step 6: Commit**

```bash
git add musefs-format/
git commit -m "feat(format): read_metadata for parsing the FLAC front without the whole file"
```

---

## Task 2: `musefs-format` — `read_vorbis_comments` (for scan tag-seeding)

The naive scanner seeds the DB with whatever tags already exist in the backing FLAC. Add a reader for the existing VORBIS_COMMENT block.

**Files:**
- Modify: `musefs-format/src/flac.rs`
- Test: `musefs-format/tests/read_comments.rs`

- [ ] **Step 1: Write the failing test**

`musefs-format/tests/read_comments.rs`:

```rust
mod common;
use common::{make_flac, streaminfo_body, vorbis_comment_body};
use musefs_format::flac::read_vorbis_comments;

#[test]
fn reads_existing_comments_including_multivalue() {
    let si = streaminfo_body();
    let vc = vorbis_comment_body("somevendor", &["TITLE=Song", "ARTIST=Alice", "ARTIST=Bob"]);
    let file = make_flac(&[(0, si), (4, vc)], &[0u8; 8]);

    let comments = read_vorbis_comments(&file).unwrap();
    assert_eq!(
        comments,
        vec![
            ("TITLE".to_string(), "Song".to_string()),
            ("ARTIST".to_string(), "Alice".to_string()),
            ("ARTIST".to_string(), "Bob".to_string()),
        ]
    );
}

#[test]
fn returns_empty_when_no_comment_block() {
    let si = streaminfo_body();
    let file = make_flac(&[(0, si)], &[0u8; 8]);
    assert_eq!(read_vorbis_comments(&file).unwrap(), Vec::new());
}

#[test]
fn skips_comment_without_equals_sign() {
    let si = streaminfo_body();
    let vc = vorbis_comment_body("v", &["NOEQUALS", "TITLE=Ok"]);
    let file = make_flac(&[(0, si), (4, vc)], &[0u8; 4]);
    assert_eq!(
        read_vorbis_comments(&file).unwrap(),
        vec![("TITLE".to_string(), "Ok".to_string())]
    );
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p musefs-format --test read_comments`
Expected: FAIL — compile error: `read_vorbis_comments` not found.

- [ ] **Step 3: Implement `read_vorbis_comments`**

Append to `musefs-format/src/flac.rs`:

```rust
/// Read the existing VORBIS_COMMENT block from a complete FLAC file, returning
/// `(FIELD, value)` pairs in order. Comments without a `=` are skipped. Returns
/// an empty vec if there is no comment block. Used by the scanner to seed tags.
pub fn read_vorbis_comments(data: &[u8]) -> Result<Vec<(String, String)>> {
    if data.len() < 4 || &data[0..4] != FLAC_MARKER {
        return Err(FormatError::NotFlac);
    }
    let mut pos = 4usize;
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
        if block_type == BLOCK_VORBIS_COMMENT {
            return parse_vorbis_comment_body(&data[body_start..body_end]);
        }
        pos = body_end;
        if is_last {
            break;
        }
    }
    Ok(Vec::new())
}

fn read_u32_le(data: &[u8], pos: usize) -> Result<u32> {
    if pos + 4 > data.len() {
        return Err(FormatError::Malformed);
    }
    Ok(u32::from_le_bytes([data[pos], data[pos + 1], data[pos + 2], data[pos + 3]]))
}

fn parse_vorbis_comment_body(body: &[u8]) -> Result<Vec<(String, String)>> {
    let vendor_len = read_u32_le(body, 0)? as usize;
    let mut pos = 4 + vendor_len;
    let count = read_u32_le(body, pos)? as usize;
    pos += 4;
    let mut out = Vec::with_capacity(count);
    for _ in 0..count {
        let clen = read_u32_le(body, pos)? as usize;
        pos += 4;
        let end = pos + clen;
        if end > body.len() {
            return Err(FormatError::Malformed);
        }
        let comment = std::str::from_utf8(&body[pos..end]).map_err(|_| FormatError::Malformed)?;
        if let Some((field, value)) = comment.split_once('=') {
            out.push((field.to_string(), value.to_string()));
        }
        pos = end;
    }
    Ok(out)
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p musefs-format --test read_comments`
Expected: PASS (all three tests).

- [ ] **Step 5: Confirm zero warnings, then commit**

Run: `cargo build -p musefs-format --tests 2>&1 | grep -iE "warning|error" || echo clean`
Expected: `clean`.

```bash
git add musefs-format/
git commit -m "feat(format): read_vorbis_comments to seed tags during scan"
```

---

## Task 3: `musefs-core` scaffold + error type

**Files:**
- Modify: `Cargo.toml` (workspace root)
- Create: `musefs-core/Cargo.toml`
- Create: `musefs-core/src/error.rs`
- Create: `musefs-core/src/lib.rs`
- Test: `musefs-core/tests/smoke.rs`

- [ ] **Step 1: Create scaffolding**

Workspace root `Cargo.toml` members:

```toml
[workspace]
resolver = "2"
members = ["musefs-db", "musefs-format", "musefs-core"]
```

`musefs-core/Cargo.toml`:

```toml
[package]
name = "musefs-core"
version = "0.1.0"
edition = "2021"

[dependencies]
musefs-db = { path = "../musefs-db" }
musefs-format = { path = "../musefs-format" }
thiserror = "1"

[dev-dependencies]
tempfile = "3"
metaflac = "0.2"
```

`musefs-core/src/error.rs`:

```rust
use thiserror::Error;

#[derive(Debug, Error)]
pub enum CoreError {
    #[error(transparent)]
    Db(#[from] musefs_db::DbError),
    #[error(transparent)]
    Format(#[from] musefs_format::FormatError),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("backing file changed since scan: {0}")]
    BackingChanged(String),
    #[error("track {0} not found")]
    TrackNotFound(i64),
    #[error("no such inode: {0}")]
    NoEntry(u64),
    #[error("inode {0} is a directory")]
    IsDir(u64),
    #[error("embedded art is not supported in this milestone")]
    ArtNotSupported,
}

pub type Result<T> = std::result::Result<T, CoreError>;
```

`musefs-core/src/lib.rs`:

```rust
mod error;
mod mapping;
mod reader;
mod scan;
mod template;
mod tree;
mod facade;

pub use error::{CoreError, Result};
pub use facade::{Attr, MountConfig, Musefs};
pub use reader::{read_at, HeaderCache, ResolvedFile};
pub use scan::{scan_directory, ScanStats};
pub use template::render_path;
pub use tree::{Node, NodeKind, VirtualTree};
```

Because `lib.rs` declares modules that don't exist yet, create empty placeholder files so the crate compiles after this task. Create each of these with the exact stub content shown (later tasks replace them):

`musefs-core/src/template.rs`:
```rust
// Path template engine. Implemented in a later task.
```
`musefs-core/src/mapping.rs`:
```rust
// DB-row to synthesis/template input mapping. Implemented in a later task.
```
`musefs-core/src/tree.rs`:
```rust
// Virtual inode tree. Implemented in a later task.
```
`musefs-core/src/reader.rs`:
```rust
// Header cache + read resolution. Implemented in a later task.
```
`musefs-core/src/scan.rs`:
```rust
// Naive FLAC scanner. Implemented in a later task.
```
`musefs-core/src/facade.rs`:
```rust
// Musefs facade. Implemented in a later task.
```

BUT the `pub use` lines reference items that don't exist yet in those stubs, which won't compile. To keep this scaffold task compiling, in this task ONLY declare the modules and the error re-export; comment out the other re-exports until their tasks land. So `lib.rs` for THIS task is:

```rust
mod error;
mod mapping;
mod reader;
mod scan;
mod template;
mod tree;
mod facade;

pub use error::{CoreError, Result};
// Re-exports below are uncommented as each module is implemented in later tasks:
// pub use facade::{Attr, MountConfig, Musefs};
// pub use reader::{read_at, HeaderCache, ResolvedFile};
// pub use scan::{scan_directory, ScanStats};
// pub use template::render_path;
// pub use tree::{Node, NodeKind, VirtualTree};
```

Each later task uncomments its own re-export line as part of its implementation.

- [ ] **Step 2: Write the failing test**

`musefs-core/tests/smoke.rs`:

```rust
#[test]
fn core_error_is_constructible_from_db_error() {
    // A DbError converts into CoreError via #[from]; this proves the crate links
    // against musefs-db and the error plumbing compiles.
    fn assert_send<T: Send>() {}
    assert_send::<musefs_core::CoreError>();
}
```

- [ ] **Step 3: Run the test to verify it fails, then make it pass**

Run: `cargo test -p musefs-core --test smoke`
Expected first: FAIL (crate doesn't exist / doesn't compile until the files above are created). After creating the scaffold files: PASS.

- [ ] **Step 4: Confirm zero warnings, then commit**

Run: `cargo build -p musefs-core --tests 2>&1 | grep -iE "warning|error" || echo clean`
Expected: `clean` (placeholder modules are empty, which is allowed; unused empty modules don't warn).

```bash
git add Cargo.toml musefs-core/
git commit -m "feat(core): musefs-core scaffold with error type"
```

---

## Task 4: `template.rs` — path template rendering

**Files:**
- Modify: `musefs-core/src/template.rs`
- Modify: `musefs-core/src/lib.rs` (uncomment the `template` re-export)
- Test: `musefs-core/tests/template.rs`

- [ ] **Step 1: Write the failing test**

`musefs-core/tests/template.rs`:

```rust
use std::collections::BTreeMap;
use musefs_core::render_path;

fn fields(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
}

#[test]
fn substitutes_dollar_and_braced_fields_and_appends_ext() {
    let f = fields(&[("albumartist", "Pink Floyd"), ("album", "Animals"), ("title", "Pigs")]);
    let path = render_path(
        "$albumartist/${album}/$title",
        &f,
        &BTreeMap::new(),
        "Unknown",
        "flac",
    );
    assert_eq!(path, "Pink Floyd/Animals/Pigs.flac");
}

#[test]
fn missing_field_uses_per_field_fallback_then_default() {
    let f = fields(&[("title", "Untitled Track")]);
    let fallbacks = fields(&[("albumartist", "Unknown Artist")]);
    let path = render_path("$albumartist/$album/$title", &f, &fallbacks, "Unknown", "flac");
    assert_eq!(path, "Unknown Artist/Unknown/Untitled Track.flac");
}

#[test]
fn sanitizes_path_illegal_characters_in_values() {
    let f = fields(&[("artist", "AC/DC"), ("title", "Back\u{0000}In")]);
    let path = render_path("$artist/$title", &f, &BTreeMap::new(), "Unknown", "flac");
    assert_eq!(path, "AC_DC/Back_In.flac");
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p musefs-core --test template`
Expected: FAIL — compile error: `render_path` not found (re-export still commented).

- [ ] **Step 3: Implement `render_path`**

`musefs-core/src/template.rs`:

```rust
use std::collections::BTreeMap;

/// Replace '/' and control characters in a substituted field value so it can be a
/// single path component. The template's own '/' separators are not passed through here.
fn sanitize(value: &str) -> String {
    value
        .chars()
        .map(|c| if c == '/' || (c as u32) < 0x20 { '_' } else { c })
        .collect()
}

fn is_field_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

fn resolve<'a>(
    name: &str,
    fields: &'a BTreeMap<String, String>,
    fallbacks: &'a BTreeMap<String, String>,
    default_fallback: &'a str,
) -> String {
    if let Some(v) = fields.get(name) {
        sanitize(v)
    } else if let Some(v) = fallbacks.get(name) {
        sanitize(v)
    } else {
        sanitize(default_fallback)
    }
}

/// Render a path template. `$field` and `${field}` are replaced with the field's
/// value (sanitized to a single path component). Missing fields use a per-field
/// fallback if present, otherwise `default_fallback`. The extension is appended.
pub fn render_path(
    template: &str,
    fields: &BTreeMap<String, String>,
    fallbacks: &BTreeMap<String, String>,
    default_fallback: &str,
    ext: &str,
) -> String {
    let mut out = String::new();
    let mut chars = template.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '$' {
            out.push(c);
            continue;
        }
        match chars.peek() {
            Some('{') => {
                chars.next(); // consume '{'
                let mut name = String::new();
                for nc in chars.by_ref() {
                    if nc == '}' {
                        break;
                    }
                    name.push(nc);
                }
                out.push_str(&resolve(&name, fields, fallbacks, default_fallback));
            }
            Some(&nc) if is_field_char(nc) => {
                let mut name = String::new();
                while let Some(&nc) = chars.peek() {
                    if is_field_char(nc) {
                        name.push(nc);
                        chars.next();
                    } else {
                        break;
                    }
                }
                out.push_str(&resolve(&name, fields, fallbacks, default_fallback));
            }
            _ => out.push('$'), // a literal '$' not followed by a field name
        }
    }
    out.push('.');
    out.push_str(ext);
    out
}
```

In `musefs-core/src/lib.rs`, uncomment the template re-export line so it reads:

```rust
pub use template::render_path;
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p musefs-core --test template`
Expected: PASS (all three tests).

- [ ] **Step 5: Confirm zero warnings, then commit**

Run: `cargo build -p musefs-core --tests 2>&1 | grep -iE "warning|error" || echo clean`
Expected: `clean`.

```bash
git add musefs-core/
git commit -m "feat(core): path template rendering with fallbacks and sanitization"
```

---

## Task 5: `mapping.rs` — DB rows to synthesis/template inputs

**Files:**
- Modify: `musefs-core/src/mapping.rs`
- Test: `musefs-core/tests/mapping.rs`

`mapping`'s functions are `pub(crate)` (used by `reader`/`facade`, not part of the public API). No `lib.rs` re-export.

- [ ] **Step 1: Write the failing test**

`musefs-core/tests/mapping.rs` — this tests crate-internal functions, so it goes through a tiny public test shim. Instead of exposing internals, test `tags_to_inputs`/`tags_to_fields` via a `#[cfg(test)]` unit-test module inside `mapping.rs`. Write this unit test at the bottom of `musefs-core/src/mapping.rs` (Step 3 includes it). Delete any `tests/mapping.rs` if present.

(There is no separate integration test file for this task — the unit tests live with the code because the functions are crate-private.)

- [ ] **Step 2: (covered by Step 3 — write code + unit tests together, run to fail first)**

Create the implementation WITH its `#[cfg(test)]` module, then run; if you want to see red first, comment out the function bodies' `return` and observe failure. Practically: write the test module first referring to `tags_to_inputs`/`tags_to_fields`, run `cargo test -p musefs-core mapping`, see it fail to compile, then add the functions.

- [ ] **Step 3: Implement `mapping.rs` with unit tests**

`musefs-core/src/mapping.rs`:

```rust
use std::collections::BTreeMap;

use musefs_db::Tag;
use musefs_format::TagInput;

/// Convert DB tag rows into the ordered list of synthesis inputs (one per value).
/// `Db::get_tags` already returns rows ordered by `(key, ordinal)`, so order is preserved.
pub(crate) fn tags_to_inputs(tags: &[Tag]) -> Vec<TagInput> {
    tags.iter()
        .map(|t| TagInput::new(&t.key, &t.value))
        .collect()
}

/// Build the field map used for path-template rendering: the first value (lowest
/// ordinal) of each key. Relies on `Db::get_tags` ordering by `(key, ordinal)`.
pub(crate) fn tags_to_fields(tags: &[Tag]) -> BTreeMap<String, String> {
    let mut map = BTreeMap::new();
    for t in tags {
        map.entry(t.key.clone()).or_insert_with(|| t.value.clone());
    }
    map
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tag(key: &str, value: &str, ordinal: i64) -> Tag {
        Tag::new(key, value, ordinal)
    }

    #[test]
    fn inputs_preserve_order_including_multivalue() {
        let tags = vec![
            tag("artist", "Alice", 0),
            tag("artist", "Bob", 1),
            tag("title", "Song", 0),
        ];
        let inputs = tags_to_inputs(&tags);
        assert_eq!(inputs, vec![
            TagInput::new("artist", "Alice"),
            TagInput::new("artist", "Bob"),
            TagInput::new("title", "Song"),
        ]);
    }

    #[test]
    fn fields_take_first_value_per_key() {
        let tags = vec![
            tag("artist", "Alice", 0),
            tag("artist", "Bob", 1),
            tag("album", "X", 0),
        ];
        let fields = tags_to_fields(&tags);
        assert_eq!(fields.get("artist").map(String::as_str), Some("Alice"));
        assert_eq!(fields.get("album").map(String::as_str), Some("X"));
    }
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p musefs-core mapping`
Expected: PASS (both unit tests).

- [ ] **Step 5: Confirm zero warnings, then commit**

Run: `cargo build -p musefs-core --tests 2>&1 | grep -iE "warning|error" || echo clean`
Expected: `clean`.

```bash
git add musefs-core/
git commit -m "feat(core): map DB tags to synthesis inputs and template fields"
```

---

## Task 6: `tree.rs` — virtual inode tree

**Files:**
- Modify: `musefs-core/src/tree.rs`
- Modify: `musefs-core/src/lib.rs` (uncomment the `tree` re-export)
- Test: `musefs-core/tests/tree.rs`

- [ ] **Step 1: Write the failing test**

`musefs-core/tests/tree.rs`:

```rust
use musefs_core::{NodeKind, VirtualTree};

#[test]
fn builds_directories_and_files_with_lookup() {
    let tree = VirtualTree::build(&[
        (10, "Pink Floyd/Animals/Pigs.flac".to_string()),
        (11, "Pink Floyd/Animals/Dogs.flac".to_string()),
        (12, "Pink Floyd/Meddle/Echoes.flac".to_string()),
    ]);

    let artist = tree.lookup(VirtualTree::ROOT, "Pink Floyd").expect("artist dir");
    let animals = tree.lookup(artist, "Animals").expect("album dir");
    assert!(tree.is_dir(animals));

    let pigs = tree.lookup(animals, "Pigs.flac").expect("file");
    assert_eq!(tree.track_id(pigs), Some(10));
    assert!(!tree.is_dir(pigs));

    // Animals has exactly two files.
    let kids = tree.children(animals).expect("children");
    assert_eq!(kids.len(), 2);
    assert!(kids.contains_key("Pigs.flac"));
    assert!(kids.contains_key("Dogs.flac"));
}

#[test]
fn disambiguates_colliding_file_names() {
    let tree = VirtualTree::build(&[
        (1, "A/song.flac".to_string()),
        (2, "A/song.flac".to_string()),
        (3, "A/song.flac".to_string()),
    ]);
    let a = tree.lookup(VirtualTree::ROOT, "A").unwrap();
    let kids = tree.children(a).unwrap();
    assert_eq!(kids.len(), 3);
    assert!(kids.contains_key("song.flac"));
    assert!(kids.contains_key("song (2).flac"));
    assert!(kids.contains_key("song (3).flac"));
}

#[test]
fn root_node_is_a_directory() {
    let tree = VirtualTree::build(&[]);
    assert!(tree.is_dir(VirtualTree::ROOT));
    assert_eq!(tree.node(VirtualTree::ROOT).map(|n| matches!(n.kind, NodeKind::Dir)), Some(true));
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p musefs-core --test tree`
Expected: FAIL — compile error: `VirtualTree` not found (re-export commented).

- [ ] **Step 3: Implement `tree.rs`**

`musefs-core/src/tree.rs`:

```rust
use std::collections::{BTreeMap, HashMap};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NodeKind {
    Dir,
    File { track_id: i64 },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Node {
    pub parent: u64,
    pub name: String,
    pub kind: NodeKind,
}

/// An in-memory virtual filesystem tree: directories derived from path components
/// and files mapped to track ids. Inodes are stable for the lifetime of the tree.
#[derive(Debug, Clone)]
pub struct VirtualTree {
    nodes: HashMap<u64, Node>,
    children: HashMap<u64, BTreeMap<String, u64>>,
    next_inode: u64,
}

impl VirtualTree {
    pub const ROOT: u64 = 1;

    pub fn build(entries: &[(i64, String)]) -> VirtualTree {
        let mut tree = VirtualTree {
            nodes: HashMap::new(),
            children: HashMap::new(),
            next_inode: 2,
        };
        tree.nodes.insert(
            Self::ROOT,
            Node { parent: Self::ROOT, name: String::new(), kind: NodeKind::Dir },
        );
        tree.children.insert(Self::ROOT, BTreeMap::new());
        for (track_id, path) in entries {
            tree.insert_file(*track_id, path);
        }
        tree
    }

    pub fn node(&self, inode: u64) -> Option<&Node> {
        self.nodes.get(&inode)
    }

    pub fn children(&self, inode: u64) -> Option<&BTreeMap<String, u64>> {
        self.children.get(&inode)
    }

    pub fn lookup(&self, parent: u64, name: &str) -> Option<u64> {
        self.children.get(&parent).and_then(|c| c.get(name).copied())
    }

    pub fn is_dir(&self, inode: u64) -> bool {
        matches!(self.nodes.get(&inode).map(|n| &n.kind), Some(NodeKind::Dir))
    }

    pub fn track_id(&self, inode: u64) -> Option<i64> {
        match self.nodes.get(&inode).map(|n| &n.kind) {
            Some(NodeKind::File { track_id }) => Some(*track_id),
            _ => None,
        }
    }

    fn alloc(&mut self) -> u64 {
        let inode = self.next_inode;
        self.next_inode += 1;
        inode
    }

    fn insert_file(&mut self, track_id: i64, path: &str) {
        let comps: Vec<&str> = path.split('/').filter(|c| !c.is_empty()).collect();
        if comps.is_empty() {
            return;
        }
        let mut dir = Self::ROOT;
        for comp in &comps[..comps.len() - 1] {
            dir = self.ensure_dir(dir, comp);
        }
        let name = self.disambiguate(dir, comps[comps.len() - 1]);
        let inode = self.alloc();
        self.nodes.insert(
            inode,
            Node { parent: dir, name: name.clone(), kind: NodeKind::File { track_id } },
        );
        self.children.get_mut(&dir).unwrap().insert(name, inode);
    }

    fn ensure_dir(&mut self, parent: u64, name: &str) -> u64 {
        if let Some(&existing) = self.children[&parent].get(name) {
            if self.is_dir(existing) {
                return existing;
            }
        }
        let unique = self.disambiguate(parent, name);
        let inode = self.alloc();
        self.nodes.insert(
            inode,
            Node { parent, name: unique.clone(), kind: NodeKind::Dir },
        );
        self.children.insert(inode, BTreeMap::new());
        self.children.get_mut(&parent).unwrap().insert(unique, inode);
        inode
    }

    /// Return `name` if free in `dir`, else append ` (k)` before the extension.
    fn disambiguate(&self, dir: u64, name: &str) -> String {
        let existing = &self.children[&dir];
        if !existing.contains_key(name) {
            return name.to_string();
        }
        let (stem, ext) = match name.rfind('.') {
            Some(i) if i > 0 => (&name[..i], Some(&name[i + 1..])),
            _ => (name, None),
        };
        let mut k = 2u32;
        loop {
            let candidate = match ext {
                Some(e) => format!("{stem} ({k}).{e}"),
                None => format!("{stem} ({k})"),
            };
            if !existing.contains_key(&candidate) {
                return candidate;
            }
            k += 1;
        }
    }
}
```

In `musefs-core/src/lib.rs`, uncomment the tree re-export:

```rust
pub use tree::{Node, NodeKind, VirtualTree};
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p musefs-core --test tree`
Expected: PASS (all three tests).

- [ ] **Step 5: Confirm zero warnings, then commit**

Run: `cargo build -p musefs-core --tests 2>&1 | grep -iE "warning|error" || echo clean`
Expected: `clean`.

```bash
git add musefs-core/
git commit -m "feat(core): virtual inode tree with collision disambiguation"
```

---

## Task 7: `reader.rs` — lazy header cache (`HeaderCache::resolve`)

**Files:**
- Modify: `musefs-core/src/reader.rs`
- Modify: `musefs-core/src/lib.rs` (uncomment the `reader` re-export)
- Create: `musefs-core/tests/common/mod.rs`
- Test: `musefs-core/tests/reader.rs`

- [ ] **Step 1: Write the shared fixture helpers and the failing test**

`musefs-core/tests/common/mod.rs` (independent FLAC builders + a temp-file writer; mirrors the musefs-format oracle):

```rust
#![allow(dead_code)]

use std::path::Path;

pub fn flac_block(block_type: u8, body: &[u8], is_last: bool) -> Vec<u8> {
    let mut out = Vec::new();
    out.push((if is_last { 0x80 } else { 0 }) | (block_type & 0x7F));
    let len = body.len();
    out.push(((len >> 16) & 0xFF) as u8);
    out.push(((len >> 8) & 0xFF) as u8);
    out.push((len & 0xFF) as u8);
    out.extend_from_slice(body);
    out
}

pub fn streaminfo_body() -> Vec<u8> {
    let mut b = vec![
        0x10, 0x00, 0x10, 0x00,
        0x00, 0x00, 0x00,
        0x00, 0x00, 0x00,
        0x0A, 0xC4, 0x42, 0xF0,
        0x00, 0x00, 0x00, 0x00,
    ];
    b.extend_from_slice(&[0u8; 16]);
    b
}

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

pub fn make_flac(blocks: &[(u8, Vec<u8>)], audio: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(b"fLaC");
    for (i, (bt, body)) in blocks.iter().enumerate() {
        out.extend_from_slice(&flac_block(*bt, body, i == blocks.len() - 1));
    }
    out.extend_from_slice(audio);
    out
}

/// Write a simple FLAC (STREAMINFO + optional comment + audio) to `path`,
/// returning (audio_offset, audio_length).
pub fn write_flac(path: &Path, comments: &[&str], audio: &[u8]) -> (i64, i64) {
    let si = streaminfo_body();
    let vc = vorbis_comment_body("orig", comments);
    let bytes = make_flac(&[(0, si), (4, vc)], audio);
    // audio_offset = bytes.len() - audio.len()
    let audio_offset = (bytes.len() - audio.len()) as i64;
    std::fs::write(path, &bytes).unwrap();
    (audio_offset, audio.len() as i64)
}
```

`musefs-core/tests/reader.rs`:

```rust
mod common;
use common::write_flac;
use musefs_core::HeaderCache;
use musefs_db::{Db, NewTrack, Format, Tag};

fn setup() -> (tempfile::TempDir, Db, i64) {
    let dir = tempfile::tempdir().unwrap();
    let flac = dir.path().join("song.flac");
    let audio = vec![0x5A; 120];
    let (audio_offset, audio_length) = write_flac(&flac, &["TITLE=Orig"], &audio);
    let meta = std::fs::metadata(&flac).unwrap();

    let db = Db::open_in_memory().unwrap();
    let id = db.upsert_track(&NewTrack {
        backing_path: flac.to_string_lossy().to_string(),
        format: Format::Flac,
        audio_offset,
        audio_length,
        backing_size: meta.len() as i64,
        backing_mtime: meta.modified().unwrap()
            .duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64,
    }).unwrap();
    db.replace_tags(id, &[Tag::new("title", "Real Title", 0)]).unwrap();
    (dir, db, id)
}

#[test]
fn resolve_builds_layout_and_total_len() {
    let (_dir, db, id) = setup();
    let mut cache = HeaderCache::new();
    let resolved = cache.resolve(&db, id).unwrap();
    assert!(resolved.total_len > 0);
    assert_eq!(resolved.total_len, resolved.layout.total_len());
    // header (synthesized metadata) + 120 audio bytes
    assert_eq!(resolved.total_len, resolved.layout.header_len() + 120);
}

#[test]
fn resolve_caches_until_content_version_changes() {
    let (_dir, db, id) = setup();
    let mut cache = HeaderCache::new();
    let first = cache.resolve(&db, id).unwrap();
    let first_version = first.content_version;

    // No change: same Arc instance returned.
    let again = cache.resolve(&db, id).unwrap();
    assert!(std::sync::Arc::ptr_eq(&first, &again));

    // Changing tags bumps content_version (DB trigger) and invalidates the cache.
    db.replace_tags(id, &[Tag::new("title", "Different", 0)]).unwrap();
    let updated = cache.resolve(&db, id).unwrap();
    assert!(updated.content_version > first_version);
    assert!(!std::sync::Arc::ptr_eq(&first, &updated));
}

#[test]
fn resolve_errors_when_backing_file_changes() {
    let (dir, db, id) = setup();
    let mut cache = HeaderCache::new();
    cache.resolve(&db, id).unwrap();

    // Truncate the backing file so size no longer matches the stored value.
    std::fs::write(dir.path().join("song.flac"), b"fLaC truncated").unwrap();
    let err = cache.resolve(&db, id);
    assert!(matches!(err, Err(musefs_core::CoreError::BackingChanged(_))));
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p musefs-core --test reader`
Expected: FAIL — compile error: `HeaderCache`/`ResolvedFile` not found.

- [ ] **Step 3: Implement the cache half of `reader.rs`**

`musefs-core/src/reader.rs` (the `read_at` function is added in Task 8; this task adds the cache and `ResolvedFile`):

```rust
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use musefs_db::Db;
use musefs_format::flac::{read_metadata, synthesize_layout, FlacScan};
use musefs_format::RegionLayout;

use crate::error::{CoreError, Result};
use crate::mapping::tags_to_inputs;

/// A fully resolved synthesized file: its segment layout, total size, the
/// content version it was built from, and where the backing audio lives.
#[derive(Debug)]
pub struct ResolvedFile {
    pub layout: RegionLayout,
    pub total_len: u64,
    pub content_version: i64,
    pub backing_path: PathBuf,
    pub mtime_secs: i64,
}

/// A per-mount cache of resolved files, keyed by track id and invalidated when a
/// track's `content_version` changes (the DB bumps it on any tag/art edit).
#[derive(Default)]
pub struct HeaderCache {
    map: HashMap<i64, Arc<ResolvedFile>>,
}

fn mtime_secs(meta: &std::fs::Metadata) -> i64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn read_front(path: &Path, n: u64) -> std::io::Result<Vec<u8>> {
    use std::io::Read;
    let mut f = std::fs::File::open(path)?;
    let mut buf = vec![0u8; n as usize];
    f.read_exact(&mut buf)?;
    Ok(buf)
}

impl HeaderCache {
    pub fn new() -> HeaderCache {
        HeaderCache::default()
    }

    /// Resolve a track to its synthesized layout, building (and caching) it on a
    /// content-version miss. Validates the backing file's size and mtime first.
    pub fn resolve(&mut self, db: &Db, track_id: i64) -> Result<Arc<ResolvedFile>> {
        let track = db.get_track(track_id)?.ok_or(CoreError::TrackNotFound(track_id))?;

        if let Some(cached) = self.map.get(&track_id) {
            if cached.content_version == track.content_version {
                return Ok(cached.clone());
            }
        }

        let meta = std::fs::metadata(&track.backing_path)?;
        if meta.len() != track.backing_size as u64 || mtime_secs(&meta) != track.backing_mtime {
            return Err(CoreError::BackingChanged(track.backing_path.clone()));
        }

        let front = read_front(Path::new(&track.backing_path), track.audio_offset as u64)?;
        let fmeta = read_metadata(&front)?;
        let tags = db.get_tags(track_id)?;
        let inputs = tags_to_inputs(&tags);

        let scan = FlacScan {
            audio_offset: track.audio_offset as u64,
            audio_length: track.audio_length as u64,
            preserved: fmeta.preserved,
        };
        let layout = synthesize_layout(&scan, &inputs, &[]);
        let total_len = layout.total_len();

        let resolved = Arc::new(ResolvedFile {
            layout,
            total_len,
            content_version: track.content_version,
            backing_path: PathBuf::from(&track.backing_path),
            mtime_secs: track.backing_mtime.max(track.updated_at),
        });
        self.map.insert(track_id, resolved.clone());
        Ok(resolved)
    }
}
```

In `musefs-core/src/lib.rs`, uncomment the reader re-export (the `read_at` symbol it names is added in Task 8; to compile now, re-export only the cache types this task introduces and add `read_at` in Task 8):

```rust
pub use reader::{HeaderCache, ResolvedFile};
```

(Task 8 extends this line to `pub use reader::{read_at, HeaderCache, ResolvedFile};`.)

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p musefs-core --test reader`
Expected: PASS (all three tests).

- [ ] **Step 5: Confirm zero warnings, then commit**

Run: `cargo build -p musefs-core --tests 2>&1 | grep -iE "warning|error" || echo clean`
Expected: `clean`.

```bash
git add musefs-core/
git commit -m "feat(core): lazy header cache resolving track layouts with validation"
```

---

## Task 8: `reader.rs` — segment read resolution (`read_at`)

**Files:**
- Modify: `musefs-core/src/reader.rs`
- Modify: `musefs-core/src/lib.rs` (extend the reader re-export to include `read_at`)
- Test: `musefs-core/tests/read_at.rs`

- [ ] **Step 1: Write the failing test**

`musefs-core/tests/read_at.rs`:

```rust
mod common;
use common::write_flac;
use musefs_core::{read_at, HeaderCache};
use musefs_db::{Db, Format, NewTrack, Tag};

fn setup() -> (tempfile::TempDir, Db, i64) {
    let dir = tempfile::tempdir().unwrap();
    let flac = dir.path().join("song.flac");
    let audio: Vec<u8> = (0..200u32).map(|i| (i % 251) as u8).collect();
    let (audio_offset, audio_length) = write_flac(&flac, &["TITLE=Orig"], &audio);
    let meta = std::fs::metadata(&flac).unwrap();
    let db = Db::open_in_memory().unwrap();
    let id = db.upsert_track(&NewTrack {
        backing_path: flac.to_string_lossy().to_string(),
        format: Format::Flac,
        audio_offset,
        audio_length,
        backing_size: meta.len() as i64,
        backing_mtime: meta.modified().unwrap()
            .duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64,
    }).unwrap();
    db.replace_tags(id, &[Tag::new("title", "Real", 0)]).unwrap();
    (dir, db, id)
}

#[test]
fn reading_whole_file_matches_total_len_and_splices_audio() {
    let (_dir, db, id) = setup();
    let mut cache = HeaderCache::new();
    let resolved = cache.resolve(&db, id).unwrap();

    let whole = read_at(&resolved, 0, resolved.total_len).unwrap();
    assert_eq!(whole.len() as u64, resolved.total_len);

    // The tail of the virtual file is the original audio (0,1,2,...) spliced through.
    let audio_part = &whole[resolved.layout.header_len() as usize..];
    let expected: Vec<u8> = (0..200u32).map(|i| (i % 251) as u8).collect();
    assert_eq!(audio_part, &expected[..]);

    // It is a valid FLAC: metaflac decodes the synthesized TITLE.
    let tag = metaflac::Tag::read_from(&mut std::io::Cursor::new(&whole)).unwrap();
    assert_eq!(
        tag.vorbis_comments().unwrap().get("TITLE").map(|v| v.as_slice()),
        Some(["Real".to_string()].as_slice())
    );
}

#[test]
fn random_offset_and_size_match_the_whole_read() {
    let (_dir, db, id) = setup();
    let mut cache = HeaderCache::new();
    let resolved = cache.resolve(&db, id).unwrap();
    let whole = read_at(&resolved, 0, resolved.total_len).unwrap();

    // Several windows, including ones straddling the header/audio boundary.
    for (off, size) in [(0u64, 10u64), (resolved.layout.header_len() - 5, 20), (resolved.total_len - 7, 50), (50, 0)] {
        let got = read_at(&resolved, off, size).unwrap();
        let end = (off + size).min(resolved.total_len) as usize;
        assert_eq!(got, &whole[off as usize..end], "off={off} size={size}");
    }
}

#[test]
fn reading_past_eof_returns_empty() {
    let (_dir, db, id) = setup();
    let mut cache = HeaderCache::new();
    let resolved = cache.resolve(&db, id).unwrap();
    assert!(read_at(&resolved, resolved.total_len, 100).unwrap().is_empty());
    assert!(read_at(&resolved, resolved.total_len + 5, 100).unwrap().is_empty());
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p musefs-core --test read_at`
Expected: FAIL — compile error: `read_at` not found.

- [ ] **Step 3: Implement `read_at`**

Append to `musefs-core/src/reader.rs`:

```rust
use musefs_format::Segment;

/// Read `size` bytes starting at virtual `offset` from a resolved file, splicing
/// inline framing with positioned reads of the backing audio. Returns fewer bytes
/// (possibly empty) near EOF.
pub fn read_at(resolved: &ResolvedFile, offset: u64, size: u64) -> Result<Vec<u8>> {
    use std::os::unix::fs::FileExt;

    if offset >= resolved.total_len || size == 0 {
        return Ok(Vec::new());
    }
    let end = offset.saturating_add(size).min(resolved.total_len);
    let mut out = Vec::with_capacity((end - offset) as usize);

    let mut seg_start = 0u64;
    let mut backing: Option<std::fs::File> = None;

    for seg in &resolved.layout.segments {
        let seg_len = seg.len();
        let seg_end = seg_start + seg_len;
        let ov_start = offset.max(seg_start);
        let ov_end = end.min(seg_end);
        if ov_start < ov_end {
            let within = ov_start - seg_start;
            let n = (ov_end - ov_start) as usize;
            match seg {
                Segment::Inline(bytes) => {
                    let w = within as usize;
                    out.extend_from_slice(&bytes[w..w + n]);
                }
                Segment::BackingAudio { offset: bo, .. } => {
                    if backing.is_none() {
                        backing = Some(std::fs::File::open(&resolved.backing_path)?);
                    }
                    let f = backing.as_ref().unwrap();
                    let mut buf = vec![0u8; n];
                    f.read_exact_at(&mut buf, bo + within)?;
                    out.extend_from_slice(&buf);
                }
                Segment::ArtImage { .. } => {
                    return Err(CoreError::ArtNotSupported);
                }
            }
        }
        seg_start = seg_end;
        if seg_start >= end {
            break;
        }
    }
    Ok(out)
}
```

In `musefs-core/src/lib.rs`, extend the reader re-export to:

```rust
pub use reader::{read_at, HeaderCache, ResolvedFile};
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p musefs-core --test read_at`
Expected: PASS (all three tests).

- [ ] **Step 5: Confirm zero warnings, then commit**

Run: `cargo build -p musefs-core --tests 2>&1 | grep -iE "warning|error" || echo clean`
Expected: `clean`.

```bash
git add musefs-core/
git commit -m "feat(core): segment read resolution splicing inline framing and backing audio"
```

---

## Task 9: `scan.rs` — naive FLAC scanner

**Files:**
- Modify: `musefs-core/src/scan.rs`
- Modify: `musefs-core/src/lib.rs` (uncomment the `scan` re-export)
- Test: `musefs-core/tests/scan.rs`

- [ ] **Step 1: Write the failing test**

`musefs-core/tests/scan.rs`:

```rust
mod common;
use common::{make_flac, streaminfo_body, vorbis_comment_body};
use musefs_core::scan_directory;
use musefs_db::Db;

#[test]
fn scans_flac_files_seeding_tracks_and_tags() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("sub")).unwrap();

    let a = make_flac(&[(0, streaminfo_body()), (4, vorbis_comment_body("v", &["TITLE=A", "ARTIST=X"]))], &[0xAA; 30]);
    std::fs::write(dir.path().join("a.flac"), &a).unwrap();
    let b = make_flac(&[(0, streaminfo_body()), (4, vorbis_comment_body("v", &["TITLE=B"]))], &[0xBB; 40]);
    std::fs::write(dir.path().join("sub/b.flac"), &b).unwrap();
    // A non-FLAC file is ignored.
    std::fs::write(dir.path().join("notes.txt"), b"hello").unwrap();

    let db = Db::open_in_memory().unwrap();
    let stats = scan_directory(&db, dir.path()).unwrap();
    assert_eq!(stats.scanned, 2);

    let tracks = db.list_tracks().unwrap();
    assert_eq!(tracks.len(), 2);

    // Find track "a.flac" and confirm its seeded tags (keys lowercased).
    let a_track = tracks.iter().find(|t| t.backing_path.ends_with("a.flac")).unwrap();
    let tags = db.get_tags(a_track.id).unwrap();
    assert!(tags.iter().any(|t| t.key == "title" && t.value == "A"));
    assert!(tags.iter().any(|t| t.key == "artist" && t.value == "X"));
    assert!(a_track.audio_length == 30);
}

#[test]
fn rescanning_is_idempotent() {
    let dir = tempfile::tempdir().unwrap();
    let a = make_flac(&[(0, streaminfo_body()), (4, vorbis_comment_body("v", &["TITLE=A"]))], &[0xAA; 30]);
    std::fs::write(dir.path().join("a.flac"), &a).unwrap();

    let db = Db::open_in_memory().unwrap();
    scan_directory(&db, dir.path()).unwrap();
    scan_directory(&db, dir.path()).unwrap();
    assert_eq!(db.list_tracks().unwrap().len(), 1);
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p musefs-core --test scan`
Expected: FAIL — compile error: `scan_directory`/`ScanStats` not found.

- [ ] **Step 3: Implement `scan.rs`**

`musefs-core/src/scan.rs`:

```rust
use std::path::Path;

use musefs_db::{Db, Format, NewTrack, Tag};
use musefs_format::flac::{locate_audio, read_vorbis_comments};

use crate::error::Result;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanStats {
    pub scanned: u64,
    pub skipped: u64,
}

fn mtime_secs(meta: &std::fs::Metadata) -> i64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn collect_flacs(root: &Path, out: &mut Vec<std::path::PathBuf>) -> std::io::Result<()> {
    for entry in std::fs::read_dir(root)? {
        let entry = entry?;
        let path = entry.path();
        let ftype = entry.file_type()?;
        if ftype.is_dir() {
            collect_flacs(&path, out)?;
        } else if ftype.is_file()
            && path.extension().and_then(|e| e.to_str()).map(|e| e.eq_ignore_ascii_case("flac")) == Some(true)
        {
            out.push(path);
        }
    }
    Ok(())
}

/// Walk `root` recursively, inserting/updating a track row for each `.flac` file
/// (with audio bounds and validation stamps) and seeding its tags from the file's
/// existing Vorbis comments. Files that fail to parse as FLAC are skipped.
pub fn scan_directory(db: &Db, root: &Path) -> Result<ScanStats> {
    let mut files = Vec::new();
    collect_flacs(root, &mut files)?;

    let mut stats = ScanStats { scanned: 0, skipped: 0 };
    for path in files {
        let bytes = std::fs::read(&path)?;
        let scan = match locate_audio(&bytes) {
            Ok(s) => s,
            Err(_) => {
                stats.skipped += 1;
                continue;
            }
        };
        let meta = std::fs::metadata(&path)?;
        let abs = std::fs::canonicalize(&path)?;
        let track_id = db.upsert_track(&NewTrack {
            backing_path: abs.to_string_lossy().to_string(),
            format: Format::Flac,
            audio_offset: scan.audio_offset as i64,
            audio_length: scan.audio_length as i64,
            backing_size: meta.len() as i64,
            backing_mtime: mtime_secs(&meta),
        })?;

        let comments = read_vorbis_comments(&bytes).unwrap_or_default();
        let mut tags = Vec::new();
        let mut ordinals: std::collections::HashMap<String, i64> = std::collections::HashMap::new();
        for (field, value) in comments {
            let key = field.to_lowercase();
            let ord = ordinals.entry(key.clone()).or_insert(0);
            tags.push(Tag::new(&key, &value, *ord));
            *ord += 1;
        }
        db.replace_tags(track_id, &tags)?;
        stats.scanned += 1;
    }
    Ok(stats)
}
```

In `musefs-core/src/lib.rs`, uncomment the scan re-export:

```rust
pub use scan::{scan_directory, ScanStats};
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p musefs-core --test scan`
Expected: PASS (both tests).

- [ ] **Step 5: Confirm zero warnings, then commit**

Run: `cargo build -p musefs-core --tests 2>&1 | grep -iE "warning|error" || echo clean`
Expected: `clean`.

```bash
git add musefs-core/
git commit -m "feat(core): naive recursive FLAC scanner seeding tracks and tags"
```

---

## Task 10: `facade.rs` — the `Musefs` read-only filesystem API

**Files:**
- Modify: `musefs-core/src/facade.rs`
- Modify: `musefs-core/src/lib.rs` (uncomment the `facade` re-export)
- Test: `musefs-core/tests/facade.rs`

- [ ] **Step 1: Write the failing test**

`musefs-core/tests/facade.rs`:

```rust
mod common;
use std::collections::BTreeMap;
use common::make_flac;
use common::{streaminfo_body, vorbis_comment_body};
use musefs_core::{scan_directory, Musefs, MountConfig, VirtualTree};

fn config() -> MountConfig {
    MountConfig {
        template: "$artist/$title".to_string(),
        fallbacks: BTreeMap::new(),
        default_fallback: "Unknown".to_string(),
    }
}

fn scanned_db(dir: &std::path::Path) -> musefs_db::Db {
    let a = make_flac(&[(0, streaminfo_body()), (4, vorbis_comment_body("v", &["ARTIST=Alice", "TITLE=Song"]))], &[0xAB; 64]);
    std::fs::write(dir.join("a.flac"), &a).unwrap();
    let db = musefs_db::Db::open_in_memory().unwrap();
    // Use an on-disk DB? in-memory is fine; scan writes absolute backing paths.
    scan_directory(&db, dir).unwrap();
    db
}

#[test]
fn lookup_getattr_readdir_and_read_through_the_facade() {
    let dir = tempfile::tempdir().unwrap();
    let db = scanned_db(dir.path());
    let mut fs = Musefs::open(db, config()).unwrap();

    // Tree: /Alice/Song.flac
    let artist = fs.lookup(VirtualTree::ROOT, "Alice").expect("artist dir");
    let dattr = fs.getattr(artist).unwrap();
    assert!(dattr.is_dir);

    let entries = fs.readdir(artist).unwrap();
    assert_eq!(entries.len(), 1);
    let (name, file_inode, is_dir) = entries.into_iter().next().unwrap();
    assert_eq!(name, "Song.flac");
    assert!(!is_dir);

    let fattr = fs.getattr(file_inode).unwrap();
    assert!(!fattr.is_dir);
    assert!(fattr.size > 0);

    // Reading the whole file yields a valid FLAC whose TITLE is the synthesized value.
    let bytes = fs.read(file_inode, 0, fattr.size).unwrap();
    assert_eq!(bytes.len() as u64, fattr.size);
    let tag = metaflac::Tag::read_from(&mut std::io::Cursor::new(&bytes)).unwrap();
    assert_eq!(
        tag.vorbis_comments().unwrap().get("TITLE").map(|v| v.as_slice()),
        Some(["Song".to_string()].as_slice())
    );
}

#[test]
fn refresh_rebuilds_tree_after_new_tracks() {
    let dir = tempfile::tempdir().unwrap();
    let db = scanned_db(dir.path());
    let mut fs = Musefs::open(db, config()).unwrap();
    assert!(fs.lookup(VirtualTree::ROOT, "Alice").is_some());
    assert!(fs.lookup(VirtualTree::ROOT, "Bob").is_none());

    // This test only asserts refresh() runs and the tree is rebuilt from the DB;
    // adding rows would require a handle to the DB, which Musefs now owns. So we
    // simply confirm refresh() succeeds and the existing entry is still present.
    fs.refresh().unwrap();
    assert!(fs.lookup(VirtualTree::ROOT, "Alice").is_some());
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p musefs-core --test facade`
Expected: FAIL — compile error: `Musefs`/`MountConfig`/`Attr` not found.

- [ ] **Step 3: Implement `facade.rs`**

`musefs-core/src/facade.rs`:

```rust
use std::collections::BTreeMap;

use musefs_db::Db;

use crate::error::{CoreError, Result};
use crate::mapping::tags_to_fields;
use crate::reader::{read_at, HeaderCache};
use crate::template::render_path;
use crate::tree::{NodeKind, VirtualTree};

/// Per-mount configuration for rendering the virtual hierarchy.
#[derive(Debug, Clone)]
pub struct MountConfig {
    pub template: String,
    pub fallbacks: BTreeMap<String, String>,
    pub default_fallback: String,
}

/// Attributes the FUSE layer maps onto `fuser::FileAttr`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attr {
    pub inode: u64,
    pub is_dir: bool,
    pub size: u64,
    pub mtime_secs: i64,
}

/// The composed read-only filesystem: the store, the rendered tree, and the lazy
/// synthesis cache. Methods take `&mut self` (the cache mutates); the FUSE layer
/// mounts this single-threaded.
pub struct Musefs {
    db: Db,
    config: MountConfig,
    tree: VirtualTree,
    cache: HeaderCache,
}

impl Musefs {
    pub fn open(db: Db, config: MountConfig) -> Result<Musefs> {
        let tree = Self::build_tree(&db, &config)?;
        Ok(Musefs { db, config, tree, cache: HeaderCache::new() })
    }

    fn build_tree(db: &Db, config: &MountConfig) -> Result<VirtualTree> {
        let tracks = db.list_tracks()?;
        let mut entries = Vec::with_capacity(tracks.len());
        for t in &tracks {
            let tags = db.get_tags(t.id)?;
            let fields = tags_to_fields(&tags);
            let path = render_path(
                &config.template,
                &fields,
                &config.fallbacks,
                &config.default_fallback,
                t.format.as_str(),
            );
            entries.push((t.id, path));
        }
        Ok(VirtualTree::build(&entries))
    }

    /// Rebuild the tree from the current DB contents (used after external edits).
    pub fn refresh(&mut self) -> Result<()> {
        self.tree = Self::build_tree(&self.db, &self.config)?;
        Ok(())
    }

    pub fn lookup(&self, parent: u64, name: &str) -> Option<u64> {
        self.tree.lookup(parent, name)
    }

    pub fn getattr(&mut self, inode: u64) -> Result<Attr> {
        let track_id = match self.tree.node(inode) {
            None => return Err(CoreError::NoEntry(inode)),
            Some(node) => match &node.kind {
                NodeKind::Dir => {
                    return Ok(Attr { inode, is_dir: true, size: 0, mtime_secs: 0 })
                }
                NodeKind::File { track_id } => *track_id,
            },
        };
        let resolved = self.cache.resolve(&self.db, track_id)?;
        Ok(Attr {
            inode,
            is_dir: false,
            size: resolved.total_len,
            mtime_secs: resolved.mtime_secs,
        })
    }

    /// Directory entries as `(name, child_inode, is_dir)`.
    pub fn readdir(&self, inode: u64) -> Result<Vec<(String, u64, bool)>> {
        let children = self.tree.children(inode).ok_or(CoreError::NoEntry(inode))?;
        Ok(children
            .iter()
            .map(|(name, &child)| (name.clone(), child, self.tree.is_dir(child)))
            .collect())
    }

    pub fn read(&mut self, inode: u64, offset: u64, size: u64) -> Result<Vec<u8>> {
        let track_id = match self.tree.node(inode) {
            None => return Err(CoreError::NoEntry(inode)),
            Some(node) => match &node.kind {
                NodeKind::Dir => return Err(CoreError::IsDir(inode)),
                NodeKind::File { track_id } => *track_id,
            },
        };
        let resolved = self.cache.resolve(&self.db, track_id)?;
        read_at(&resolved, offset, size)
    }
}
```

In `musefs-core/src/lib.rs`, uncomment the facade re-export:

```rust
pub use facade::{Attr, MountConfig, Musefs};
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p musefs-core --test facade`
Expected: PASS (both tests).

- [ ] **Step 5: Confirm zero warnings, then commit**

Run: `cargo build -p musefs-core --tests 2>&1 | grep -iE "warning|error" || echo clean`
Expected: `clean`.

```bash
git add musefs-core/
git commit -m "feat(core): Musefs facade composing tree, cache, and read path"
```

---

## Task 11: Whole-workspace verification

**Files:** none (verification only).

- [ ] **Step 1: Run the entire workspace test suite**

Run: `cargo test`
Expected: PASS — `musefs-db`, `musefs-format` (including the refactored `locate_audio` and new readers), and `musefs-core` all green.

- [ ] **Step 2: Confirm a clean build with no warnings across the workspace**

Run: `cargo build --workspace --tests 2>&1 | grep -i warning || echo "no warnings"`
Expected: prints `no warnings`. Fix any that appear and re-run.

- [ ] **Step 3: Commit any cleanup**

```bash
git add -A
git commit -m "chore(core): M2-core cleanup, no warnings" || echo "nothing to commit"
```

---

## Self-Review Notes

- **Spec coverage (M2 core scope):** `musefs-core` tree from path templates (Tasks 4, 6, 10); lazy generate-and-measure header cache keyed by `(track_id, content_version)` with backing size/mtime validation (Task 7); segment read resolution splicing inline + positioned backing-audio reads (Task 8); DB→`musefs-format` input mapping (Task 5); naive recursive FLAC scanner seeding tracks + tags (Task 9); the read path re-reads only the file front via the new `read_metadata` (Task 1); scan seeds tags via `read_vorbis_comments` (Task 2). The `Musefs` facade (Task 10) is the seam the FUSE layer (next plan) wraps. All M2-core requirements covered.
- **Correctly deferred (not this plan):** the FUSE `fuser::Filesystem` impl, the `musefs-cli` `scan`/`mount` commands, and gated FUSE integration tests are the **M2 fuse+cli** plan. Embedded art in the read path (`read_at` returns `CoreError::ArtNotSupported` for `ArtImage` segments, which the M2 scan never produces) is M4. `data_version` polling / live refresh wiring is M5 (`refresh()` exists but isn't yet triggered automatically). Connection pooling for multithreaded FUSE is deferred (FUSE will mount single-threaded).
- **Type consistency:** `FlacMeta {audio_offset, preserved}` / `read_metadata` / `read_vorbis_comments` (format); `CoreError`/`Result`; `render_path(template, fields, fallbacks, default_fallback, ext)`; `tags_to_inputs`/`tags_to_fields`; `VirtualTree` (`ROOT`, `build(&[(i64,String)])`, `lookup`, `node`, `children`, `is_dir`, `track_id`), `Node{parent,name,kind}`, `NodeKind::{Dir, File{track_id}}`; `ResolvedFile{layout,total_len,content_version,backing_path,mtime_secs}`, `HeaderCache::{new,resolve}`, `read_at(&ResolvedFile,u64,u64)`; `ScanStats{scanned,skipped}`, `scan_directory(&Db,&Path)`; `Musefs::{open,refresh,lookup,getattr,readdir,read}`, `MountConfig{template,fallbacks,default_fallback}`, `Attr{inode,is_dir,size,mtime_secs}` — all used identically across tasks.
- **Placeholder discipline:** the only intentional placeholders are the empty module stubs created in Task 3 (each replaced by its own task) and the commented-out `lib.rs` re-export lines (each uncommented by its module's task). Every code step contains complete, compilable code.
- **Platform note:** `read_at` uses `std::os::unix::fs::FileExt::read_exact_at` (Unix/Linux). musefs targets Linux (FUSE); this is consistent with the project's platform.
