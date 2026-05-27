# Arbitrary Tag Support Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Preserve every tag on a backing file through scan → DB → synthesis for all formats, with a single canonical vocabulary so path-template fields stay consistent across formats.

**Architecture:** A new `musefs-format/src/tagmap.rs` module holds one bidirectional canonical vocabulary (canonical key ⇄ ID3 frame / MP4 atom / Vorbis field). Each format's read and write paths consult it; tags outside the vocabulary round-trip through the format's extension slot (`TXXX` / `----` / raw Vorbis field) keyed by their human name, with the ID3 read/write also passing unmapped text frames through by their frame id. The DB, `tags` schema, and resolution path are unchanged except for two small casing tweaks in the core layer.

**Tech Stack:** Rust workspace; `rust-id3` v1 (ID3 read), manual byte synthesis (ID3/MP4/Vorbis write), `mp4` v0.14 helpers already wrapped in `mp4.rs`.

**Spec:** `docs/superpowers/specs/2026-05-27-arbitrary-tags-design.md`

**Commit convention:** every commit ends with the project trailer
`Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>`. The
`cargo fmt --check` + `cargo clippy --all-targets -- -D warnings` pre-commit hook
must pass, so each task removes any code it makes dead.

**Key design facts (read before starting):**
- Synthesis always emits the *same* format as the backing file, so round-trip is
  always within one format. There is no cross-format conversion.
- Losslessness for the long tail does **not** require an exhaustive vocabulary:
  unmapped ID3 text frames round-trip via their frame id as the key; unmapped MP4
  tags round-trip via `----` freeform; unmapped Vorbis fields round-trip verbatim.
  The vocabulary exists to give common fields nice, cross-format-consistent
  canonical names.
- Stored keys: canonical lowercase for a vocabulary match; verbatim source casing
  otherwise (so `TXXX:MusicBrainz Album Id` is not mangled).

---

## Task 1: Canonical vocabulary module (`tagmap.rs`)

**Files:**
- Create: `musefs-format/src/tagmap.rs`
- Modify: `musefs-format/src/lib.rs:8` (add module declaration)

- [ ] **Step 1: Register the module**

In `musefs-format/src/lib.rs`, add after the `mod input;` line (keep alphabetical-ish grouping with the other private modules):

```rust
mod tagmap;
```

- [ ] **Step 2: Write the module with the vocabulary and lookups**

Create `musefs-format/src/tagmap.rs`:

```rust
//! Canonical tag vocabulary: the single source of truth mapping a canonical
//! (lowercase) tag key to its native representation in each container format.
//! Format modules consult this for both scanning (native -> canonical) and
//! synthesis (canonical -> native). Tags absent from the vocabulary are
//! user-defined and round-trip verbatim through each format's extension slot.

/// How a canonical key is represented inside an ID3v2 tag.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Id3Slot {
    /// A standard text information frame, e.g. `b"TIT2"`.
    Text(&'static [u8; 4]),
    /// A `TXXX` user-defined text frame with this fixed, exact-case description.
    Txxx(&'static str),
    /// The `COMM` comment frame.
    Comment,
    /// The `USLT` unsynchronised-lyrics frame.
    Lyrics,
}

/// How a canonical key is represented inside an MP4 `ilst`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Mp4Slot {
    /// A text atom, e.g. `b"\xa9nam"`.
    Text(&'static [u8; 4]),
    /// A binary number atom (`trkn`/`disk`); the `usize` is the `data` body width.
    Number(&'static [u8; 4], usize),
    /// A `----` freeform atom: `(mean, name)`.
    Freeform(&'static str, &'static str),
}

pub(crate) struct Entry {
    pub key: &'static str, // canonical, lowercase
    pub id3: Id3Slot,
    pub mp4: Mp4Slot,
    pub vorbis: &'static str, // Vorbis field name (uppercase convention)
}

const VOCAB: &[Entry] = &[
    Entry { key: "title",        id3: Id3Slot::Text(b"TIT2"), mp4: Mp4Slot::Text(b"\xa9nam"),      vorbis: "TITLE" },
    Entry { key: "artist",       id3: Id3Slot::Text(b"TPE1"), mp4: Mp4Slot::Text(b"\xa9ART"),      vorbis: "ARTIST" },
    Entry { key: "album",        id3: Id3Slot::Text(b"TALB"), mp4: Mp4Slot::Text(b"\xa9alb"),      vorbis: "ALBUM" },
    Entry { key: "albumartist",  id3: Id3Slot::Text(b"TPE2"), mp4: Mp4Slot::Text(b"aART"),         vorbis: "ALBUMARTIST" },
    Entry { key: "genre",        id3: Id3Slot::Text(b"TCON"), mp4: Mp4Slot::Text(b"\xa9gen"),      vorbis: "GENRE" },
    Entry { key: "date",         id3: Id3Slot::Text(b"TDRC"), mp4: Mp4Slot::Text(b"\xa9day"),      vorbis: "DATE" },
    Entry { key: "composer",     id3: Id3Slot::Text(b"TCOM"), mp4: Mp4Slot::Text(b"\xa9wrt"),      vorbis: "COMPOSER" },
    Entry { key: "grouping",     id3: Id3Slot::Text(b"TIT1"), mp4: Mp4Slot::Text(b"\xa9grp"),      vorbis: "GROUPING" },
    Entry { key: "tracknumber",  id3: Id3Slot::Text(b"TRCK"), mp4: Mp4Slot::Number(b"trkn", 8),    vorbis: "TRACKNUMBER" },
    Entry { key: "discnumber",   id3: Id3Slot::Text(b"TPOS"), mp4: Mp4Slot::Number(b"disk", 6),    vorbis: "DISCNUMBER" },
    Entry { key: "comment",      id3: Id3Slot::Comment,       mp4: Mp4Slot::Text(b"\xa9cmt"),      vorbis: "COMMENT" },
    Entry { key: "lyrics",       id3: Id3Slot::Lyrics,        mp4: Mp4Slot::Text(b"\xa9lyr"),      vorbis: "LYRICS" },
    Entry { key: "copyright",    id3: Id3Slot::Text(b"TCOP"), mp4: Mp4Slot::Freeform("com.apple.iTunes", "copyright"), vorbis: "COPYRIGHT" },
    Entry { key: "isrc",         id3: Id3Slot::Text(b"TSRC"), mp4: Mp4Slot::Freeform("com.apple.iTunes", "ISRC"),      vorbis: "ISRC" },
    Entry { key: "lyricist",     id3: Id3Slot::Text(b"TEXT"), mp4: Mp4Slot::Freeform("com.apple.iTunes", "LYRICIST"),  vorbis: "LYRICIST" },
    Entry { key: "conductor",    id3: Id3Slot::Text(b"TPE3"), mp4: Mp4Slot::Freeform("com.apple.iTunes", "CONDUCTOR"), vorbis: "CONDUCTOR" },
    Entry { key: "replaygain_track_gain", id3: Id3Slot::Txxx("REPLAYGAIN_TRACK_GAIN"), mp4: Mp4Slot::Freeform("com.apple.iTunes", "replaygain_track_gain"), vorbis: "REPLAYGAIN_TRACK_GAIN" },
    Entry { key: "replaygain_album_gain", id3: Id3Slot::Txxx("REPLAYGAIN_ALBUM_GAIN"), mp4: Mp4Slot::Freeform("com.apple.iTunes", "replaygain_album_gain"), vorbis: "REPLAYGAIN_ALBUM_GAIN" },
    Entry { key: "replaygain_track_peak", id3: Id3Slot::Txxx("REPLAYGAIN_TRACK_PEAK"), mp4: Mp4Slot::Freeform("com.apple.iTunes", "replaygain_track_peak"), vorbis: "REPLAYGAIN_TRACK_PEAK" },
    Entry { key: "replaygain_album_peak", id3: Id3Slot::Txxx("REPLAYGAIN_ALBUM_PEAK"), mp4: Mp4Slot::Freeform("com.apple.iTunes", "replaygain_album_peak"), vorbis: "REPLAYGAIN_ALBUM_PEAK" },
    Entry { key: "musicbrainz_albumid",  id3: Id3Slot::Txxx("MusicBrainz Album Id"),  mp4: Mp4Slot::Freeform("com.apple.iTunes", "MusicBrainz Album Id"),  vorbis: "MUSICBRAINZ_ALBUMID" },
    Entry { key: "musicbrainz_artistid", id3: Id3Slot::Txxx("MusicBrainz Artist Id"), mp4: Mp4Slot::Freeform("com.apple.iTunes", "MusicBrainz Artist Id"), vorbis: "MUSICBRAINZ_ARTISTID" },
];

/// ID3 text frame id (e.g. "TIT2") -> canonical key, for `Text` slots only.
pub(crate) fn id3_text_to_key(frame_id: &str) -> Option<&'static str> {
    VOCAB.iter().find_map(|e| match e.id3 {
        Id3Slot::Text(id) if &id[..] == frame_id.as_bytes() => Some(e.key),
        _ => None,
    })
}

/// `TXXX` description -> canonical key (case-insensitive), for `Txxx` slots only.
pub(crate) fn id3_txxx_to_key(description: &str) -> Option<&'static str> {
    VOCAB.iter().find_map(|e| match e.id3 {
        Id3Slot::Txxx(d) if d.eq_ignore_ascii_case(description) => Some(e.key),
        _ => None,
    })
}

/// Canonical key -> ID3 slot (key matched case-insensitively).
pub(crate) fn key_to_id3(key: &str) -> Option<Id3Slot> {
    let k = key.to_ascii_lowercase();
    VOCAB.iter().find(|e| e.key == k).map(|e| e.id3)
}

/// MP4 text atom -> canonical key, for `Text` slots only.
pub(crate) fn mp4_atom_to_key(atom: &[u8; 4]) -> Option<&'static str> {
    VOCAB.iter().find_map(|e| match e.mp4 {
        Mp4Slot::Text(a) if a == atom => Some(e.key),
        _ => None,
    })
}

/// MP4 `----` (mean, name) -> canonical key (case-insensitive), `Freeform` only.
pub(crate) fn mp4_freeform_to_key(mean: &str, name: &str) -> Option<&'static str> {
    VOCAB.iter().find_map(|e| match e.mp4 {
        Mp4Slot::Freeform(m, n) if m.eq_ignore_ascii_case(mean) && n.eq_ignore_ascii_case(name) => {
            Some(e.key)
        }
        _ => None,
    })
}

/// Canonical key -> MP4 slot (key matched case-insensitively).
pub(crate) fn key_to_mp4(key: &str) -> Option<Mp4Slot> {
    let k = key.to_ascii_lowercase();
    VOCAB.iter().find(|e| e.key == k).map(|e| e.mp4)
}

/// Vorbis field name -> canonical key (case-insensitive).
pub(crate) fn vorbis_to_key(field: &str) -> Option<&'static str> {
    VOCAB
        .iter()
        .find_map(|e| e.vorbis.eq_ignore_ascii_case(field).then_some(e.key))
}

/// Canonical key -> Vorbis field name (key matched case-insensitively).
pub(crate) fn key_to_vorbis(key: &str) -> Option<&'static str> {
    let k = key.to_ascii_lowercase();
    VOCAB.iter().find(|e| e.key == k).map(|e| e.vorbis)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_duplicate_canonical_keys() {
        let mut keys: Vec<&str> = VOCAB.iter().map(|e| e.key).collect();
        let n = keys.len();
        keys.sort_unstable();
        keys.dedup();
        assert_eq!(keys.len(), n, "duplicate canonical key in VOCAB");
    }

    #[test]
    fn id3_text_round_trips() {
        for e in VOCAB {
            if let Id3Slot::Text(id) = e.id3 {
                let frame = std::str::from_utf8(id).unwrap();
                assert_eq!(id3_text_to_key(frame), Some(e.key));
                assert!(matches!(key_to_id3(e.key), Some(Id3Slot::Text(_))));
            }
        }
    }

    #[test]
    fn mp4_text_round_trips() {
        for e in VOCAB {
            if let Mp4Slot::Text(a) = e.mp4 {
                assert_eq!(mp4_atom_to_key(a), Some(e.key));
            }
        }
    }

    #[test]
    fn vorbis_round_trips() {
        for e in VOCAB {
            assert_eq!(vorbis_to_key(e.vorbis), Some(e.key));
            assert_eq!(key_to_vorbis(e.key), Some(e.vorbis));
        }
    }

    #[test]
    fn txxx_and_freeform_lookups_are_case_insensitive() {
        assert_eq!(id3_txxx_to_key("musicbrainz album id"), Some("musicbrainz_albumid"));
        assert_eq!(
            mp4_freeform_to_key("com.apple.itunes", "MusicBrainz Album Id"),
            Some("musicbrainz_albumid")
        );
    }
}
```

- [ ] **Step 3: Run the tests (some lookups are not yet used elsewhere)**

Run: `cargo test -p musefs-format tagmap`
Expected: PASS (5 tests). The module is not yet consumed, so expect
`dead_code` warnings — they are resolved by Tasks 2–6. Do **not** commit yet if
`clippy -D warnings` would fail; instead proceed to Step 4.

- [ ] **Step 4: Silence the temporary dead-code warning, then commit**

Add `#![allow(dead_code)]`-equivalent at the module top by prefixing the module
with `#[allow(dead_code)]` in `lib.rs`:

```rust
#[allow(dead_code)]
mod tagmap;
```

Run: `cargo clippy --all-targets -- -D warnings`
Expected: PASS

```bash
git add musefs-format/src/tagmap.rs musefs-format/src/lib.rs
git commit -m "feat(format): add canonical tag vocabulary module"
```

> Note: the `#[allow(dead_code)]` is removed in Task 6 once every lookup is wired in.

---

## Task 2: ID3 read — capture TXXX / COMM / USLT / unmapped frames

**Files:**
- Modify: `musefs-format/src/mp3.rs` — rewrite `read_tags` (currently lines 255–274), delete `frame_to_key` (currently lines 220–235)
- Test: `musefs-format/src/mp3.rs` `#[cfg(test)] mod tests` (create the module at end of file if absent)

- [ ] **Step 1: Write the failing test**

Add to `mp3.rs` tests:

```rust
#[test]
fn read_tags_captures_txxx_comm_uslt_and_unmapped_text() {
    use id3::frame::{Comment, ExtendedText, Lyrics};
    use id3::{Tag, TagLike, Version}; // TagLike brings set_text/add_frame into scope

    let mut tag = Tag::new();
    tag.set_text("TIT2", "Song");
    tag.set_text("TBPM", "120"); // standard frame, not in vocabulary
    tag.add_frame(ExtendedText { description: "MOOD".into(), value: "happy".into() });
    tag.add_frame(Comment { lang: "eng".into(), description: String::new(), text: "nice".into() });
    tag.add_frame(Lyrics { lang: "eng".into(), description: String::new(), text: "la la".into() });

    let mut buf = Vec::new();
    tag.write_to(&mut buf, Version::Id3v24).unwrap();

    let tags = read_tags(&buf);
    assert!(tags.contains(&("title".to_string(), "Song".to_string())));
    assert!(tags.contains(&("TBPM".to_string(), "120".to_string())));
    assert!(tags.contains(&("MOOD".to_string(), "happy".to_string())));
    assert!(tags.contains(&("comment".to_string(), "nice".to_string())));
    assert!(tags.contains(&("lyrics".to_string(), "la la".to_string())));
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p musefs-format read_tags_captures_txxx_comm_uslt_and_unmapped_text`
Expected: FAIL — current `read_tags` drops everything except the 9 mapped frames, so the `TBPM`/`MOOD`/`comment`/`lyrics` asserts fail.

- [ ] **Step 3: Rewrite `read_tags` and delete `frame_to_key`**

Replace the body of `read_tags` with:

```rust
/// Read an existing ID3v2 tag and fold it into canonical `(key, value)` pairs.
/// Text frames map via the vocabulary (NUL-separated multi-value yields one pair
/// per value); unmapped text frames pass through keyed by their frame id; `TXXX`
/// frames key on their description (folded to canonical when known); `COMM`/`USLT`
/// yield `comment`/`lyrics` (text only). Other/binary frames are skipped.
pub fn read_tags(data: &[u8]) -> Vec<(String, String)> {
    let Ok(tag) = id3::Tag::read_from2(std::io::Cursor::new(data)) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for frame in tag.frames() {
        let content = frame.content();
        if let Some(et) = content.extended_text() {
            let key = crate::tagmap::id3_txxx_to_key(&et.description)
                .map(str::to_string)
                .unwrap_or_else(|| et.description.clone());
            out.push((key, et.value.clone()));
        } else if let Some(c) = content.comment() {
            out.push(("comment".to_string(), c.text.clone()));
        } else if let Some(l) = content.lyrics() {
            out.push(("lyrics".to_string(), l.text.clone()));
        } else if let Some(text) = content.text() {
            let key = crate::tagmap::id3_text_to_key(frame.id())
                .map(str::to_string)
                .unwrap_or_else(|| frame.id().to_string());
            for value in text.split('\0').filter(|v| !v.is_empty()) {
                out.push((key.clone(), value.to_string()));
            }
        }
    }
    out
}
```

Delete the `frame_to_key` function entirely (it is now unused).

- [ ] **Step 4: Run the test and the crate suite**

Run: `cargo test -p musefs-format`
Expected: PASS (new test passes; existing tests still green). If a pre-existing test referenced `frame_to_key`, update it to call `read_tags` instead.

- [ ] **Step 5: Commit**

```bash
git add musefs-format/src/mp3.rs
git commit -m "feat(format): read all ID3 text/TXXX/COMM/USLT frames via vocabulary"
```

---

## Task 3: ID3 write — emit COMM/USLT and unmapped text frames

**Files:**
- Modify: `musefs-format/src/mp3.rs` — `build_id3v2_segments` (currently lines 127–201), add helper `comm_like_frame_data` and `is_id3_text_frame_id`, delete `key_to_frame` (currently lines 84–99)
- Test: `musefs-format/src/mp3.rs` tests

- [ ] **Step 1: Write the failing round-trip test**

Add to `mp3.rs` tests:

```rust
#[test]
fn synthesize_round_trips_arbitrary_id3_tags() {
    let tags = vec![
        TagInput::new("title", "Song"),
        TagInput::new("TBPM", "120"),            // unmapped standard frame
        TagInput::new("MyRating", "5"),          // user-defined -> TXXX
        TagInput::new("comment", "nice"),        // -> COMM
        TagInput::new("lyrics", "la la"),        // -> USLT
        TagInput::new("replaygain_track_gain", "-3.21 dB"), // -> TXXX (fixed desc)
    ];
    let (segments, _len) = build_id3v2_segments(&tags, &[]).unwrap();
    let mut buf = Vec::new();
    for seg in &segments {
        if let Segment::Inline(bytes) = seg {
            buf.extend_from_slice(bytes);
        }
    }
    let read = read_tags(&buf);
    for expected in [
        ("title", "Song"),
        ("TBPM", "120"),
        ("MyRating", "5"),
        ("comment", "nice"),
        ("lyrics", "la la"),
        ("replaygain_track_gain", "-3.21 dB"),
    ] {
        assert!(
            read.contains(&(expected.0.to_string(), expected.1.to_string())),
            "missing {expected:?} in {read:?}"
        );
    }
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p musefs-format synthesize_round_trips_arbitrary_id3_tags`
Expected: FAIL — current `build_id3v2_segments` writes `comment`/`lyrics`/`TBPM` as `TXXX`, and `read_tags` would surface them under the wrong keys (`comment` as a `TXXX` description rather than a `COMM`, etc.), so asserts fail.

- [ ] **Step 3: Add the two helpers**

Add near `txxx_frame_data` in `mp3.rs`:

```rust
/// COMM/USLT share a body layout: `[enc][lang(3)][descriptor NUL][text]`. We
/// write UTF-8 with an unknown language and empty descriptor (see Limitations).
fn comm_like_frame_data(value: &str) -> Vec<u8> {
    let mut d = vec![ENC_UTF8];
    d.extend_from_slice(b"XXX"); // language: unknown
    d.push(0x00); // empty content descriptor, NUL-terminated
    d.extend_from_slice(value.as_bytes());
    d
}

/// True if `key` is shaped like an ID3v2 text frame id (`T` + 3 upper/digit),
/// excluding `TXXX` itself. Used to round-trip unmapped standard text frames.
fn is_id3_text_frame_id(key: &str) -> bool {
    key.len() == 4
        && key != "TXXX"
        && key.starts_with('T')
        && key.bytes().all(|b| b.is_ascii_uppercase() || b.is_ascii_digit())
}
```

- [ ] **Step 4: Replace the group-emission loop in `build_id3v2_segments`**

In `build_id3v2_segments`, replace the existing `for (key, values) in &groups { match key_to_frame(key) { ... } }` block (the loop that builds text/`TXXX` frames; the `groups` construction above it and the `arts`/header code below it stay unchanged) with:

```rust
    for (key, values) in &groups {
        match crate::tagmap::key_to_id3(key) {
            Some(crate::tagmap::Id3Slot::Text(id)) => {
                let data = text_frame_data(values);
                push_frame_header(&mut buf, id, data.len())?;
                buf.extend_from_slice(&data);
                frames_len += 10 + data.len() as u64;
            }
            Some(crate::tagmap::Id3Slot::Txxx(desc)) => {
                for value in values {
                    let data = txxx_frame_data(desc, value);
                    push_frame_header(&mut buf, b"TXXX", data.len())?;
                    buf.extend_from_slice(&data);
                    frames_len += 10 + data.len() as u64;
                }
            }
            Some(crate::tagmap::Id3Slot::Comment) => {
                for value in values {
                    let data = comm_like_frame_data(value);
                    push_frame_header(&mut buf, b"COMM", data.len())?;
                    buf.extend_from_slice(&data);
                    frames_len += 10 + data.len() as u64;
                }
            }
            Some(crate::tagmap::Id3Slot::Lyrics) => {
                for value in values {
                    let data = comm_like_frame_data(value);
                    push_frame_header(&mut buf, b"USLT", data.len())?;
                    buf.extend_from_slice(&data);
                    frames_len += 10 + data.len() as u64;
                }
            }
            None if is_id3_text_frame_id(key) => {
                let id: [u8; 4] = key.as_bytes().try_into().unwrap();
                let data = text_frame_data(values);
                push_frame_header(&mut buf, &id, data.len())?;
                buf.extend_from_slice(&data);
                frames_len += 10 + data.len() as u64;
            }
            None => {
                for value in values {
                    let data = txxx_frame_data(key, value);
                    push_frame_header(&mut buf, b"TXXX", data.len())?;
                    buf.extend_from_slice(&data);
                    frames_len += 10 + data.len() as u64;
                }
            }
        }
    }
```

Delete the `key_to_frame` function entirely (now unused).

- [ ] **Step 5: Run the round-trip test and crate suite**

Run: `cargo test -p musefs-format`
Expected: PASS

- [ ] **Step 6: Commit**

```bash
git add musefs-format/src/mp3.rs
git commit -m "feat(format): synthesize COMM/USLT and unmapped ID3 text frames"
```

---

## Task 4: MP4 read — capture `----` freeform and vocabulary atoms

**Files:**
- Modify: `musefs-format/src/mp4.rs` — rewrite `read_tags` (currently lines 338–373), add helper `read_freeform`, delete `atom_to_key` (currently lines 310–321)
- Test: `musefs-format/src/mp4.rs` `#[cfg(test)] mod tests`

- [ ] **Step 1: Write the failing test**

Add to `mp4.rs` tests (the module already exists):

```rust
#[test]
fn read_freeform_extracts_name_and_value() {
    // Build a minimal `----` atom: mean + name + data(UTF-8).
    let mut mean_body = 0u32.to_be_bytes().to_vec();
    mean_body.extend_from_slice(b"com.apple.iTunes");
    let mut name_body = 0u32.to_be_bytes().to_vec();
    name_body.extend_from_slice(b"MusicBrainz Album Id");
    let mut data = 1u32.to_be_bytes().to_vec(); // type 1 = UTF-8
    data.extend_from_slice(&0u32.to_be_bytes()); // locale
    data.extend_from_slice(b"abc-123");
    let mut inner = boxed(b"mean", &mean_body);
    inner.extend(boxed(b"name", &name_body));
    inner.extend(boxed(b"data", &data));

    let (key, value) = read_freeform(&inner).unwrap();
    assert_eq!(key, "musicbrainz_albumid"); // folded via vocabulary
    assert_eq!(value, "abc-123");
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p musefs-format read_freeform_extracts_name_and_value`
Expected: FAIL — `read_freeform` does not exist (compile error).

- [ ] **Step 3: Add `read_freeform` and rewrite `read_tags`; delete `atom_to_key`**

Add the helper:

```rust
/// Parse a `----` freeform atom payload into `(key, value)`. Folds (mean, name)
/// to a canonical key via the vocabulary, else keys on the verbatim `name`. Only
/// the first `data` atom is read (multi-value freeform is rare). None if malformed.
fn read_freeform(inner: &[u8]) -> Option<(String, String)> {
    let name_box = find_box(inner, b"name").ok()??;
    let data_box = find_box(inner, b"data").ok()??;
    let np = name_box.payload(inner);
    let dp = data_box.payload(inner);
    if np.len() < 4 || dp.len() < 8 {
        return None;
    }
    let name = std::str::from_utf8(&np[4..]).ok()?;
    let value = std::str::from_utf8(&dp[8..]).ok()?;
    let mean = find_box(inner, b"mean")
        .ok()
        .flatten()
        .and_then(|m| {
            let p = m.payload(inner);
            (p.len() >= 4).then(|| std::str::from_utf8(&p[4..]).ok()).flatten()
        })
        .unwrap_or("com.apple.iTunes");
    let key = crate::tagmap::mp4_freeform_to_key(mean, name)
        .map(str::to_string)
        .unwrap_or_else(|| name.to_string());
    Some((key, value.to_string()))
}
```

Replace the body of `read_tags` with:

```rust
/// Lenient: returns empty / skips any malformed atom and never errors — this only
/// seeds metadata from existing files, so a missing or garbled tag must simply be
/// absent. Text atoms map via the vocabulary; `trkn`/`disk` yield track/disc
/// numbers; `----` freeform atoms key on their name (folded when known). Other
/// atoms are skipped.
pub fn read_tags(buf: &[u8]) -> Vec<(String, String)> {
    let Some((start, len)) = ilst_region(buf) else {
        return Vec::new();
    };
    let ilst = &buf[start..start + len];
    let mut out = Vec::new();
    for atom in child_boxes(ilst).unwrap_or_default() {
        let inner = atom.payload(ilst);
        if &atom.kind == b"----" {
            if let Some(pair) = read_freeform(inner) {
                out.push(pair);
            }
            continue;
        }
        let Ok(Some(data)) = find_box(inner, b"data") else {
            continue;
        };
        let dp = data.payload(inner);
        if dp.len() < 8 {
            continue;
        }
        let value = &dp[8..]; // skip [type 4][locale 4]
        if let Some(key) = crate::tagmap::mp4_atom_to_key(&atom.kind) {
            if let Ok(s) = std::str::from_utf8(value) {
                out.push((key.to_string(), s.to_string()));
            }
        } else if &atom.kind == b"trkn" && value.len() >= 4 {
            out.push((
                "tracknumber".into(),
                u16::from_be_bytes([value[2], value[3]]).to_string(),
            ));
        } else if &atom.kind == b"disk" && value.len() >= 4 {
            out.push((
                "discnumber".into(),
                u16::from_be_bytes([value[2], value[3]]).to_string(),
            ));
        }
    }
    out
}
```

Delete the `atom_to_key` function entirely (now unused).

- [ ] **Step 4: Run the test and crate suite**

Run: `cargo test -p musefs-format`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add musefs-format/src/mp4.rs
git commit -m "feat(format): read MP4 ---- freeform and vocabulary atoms"
```

---

## Task 5: MP4 write — emit `----` freeform for unmapped tags

**Files:**
- Modify: `musefs-format/src/mp4.rs` — `build_udta` (currently lines 457–532), add helper `freeform_atom`, delete `meta_key` (currently lines 444–455)
- Test: `musefs-format/src/mp4.rs` tests

- [ ] **Step 1: Write the failing round-trip test**

Add to `mp4.rs` tests:

```rust
#[test]
fn build_udta_round_trips_freeform_and_vocabulary() {
    let tags = vec![
        TagInput::new("title", "Song"),
        TagInput::new("tracknumber", "3"),
        TagInput::new("MyRating", "5"),                     // user-defined -> ----
        TagInput::new("musicbrainz_albumid", "abc-123"),    // vocabulary -> ----
    ];
    let (udta, _art_len) = build_udta(&tags, None).unwrap();
    // build_udta returns a full `udta` box; read_tags expects a buffer containing
    // moov/udta/meta/ilst, so wrap udta in a minimal moov for the round trip.
    let moov = boxed(b"moov", &udta);

    let tags = read_tags(&moov);
    for expected in [
        ("title", "Song"),
        ("tracknumber", "3"),
        ("MyRating", "5"),
        ("musicbrainz_albumid", "abc-123"),
    ] {
        assert!(
            tags.contains(&(expected.0.to_string(), expected.1.to_string())),
            "missing {expected:?} in {tags:?}"
        );
    }
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p musefs-format build_udta_round_trips_freeform_and_vocabulary`
Expected: FAIL — current `build_udta` silently drops `MyRating` and `musicbrainz_albumid` (no `meta_key` match), so those asserts fail.

- [ ] **Step 3: Add `freeform_atom`**

Add near `text_atom` in `mp4.rs`:

```rust
fn freeform_atom(mean: &str, name: &str, values: &[&str]) -> Vec<u8> {
    let mut inner = Vec::new();
    let mut mean_body = 0u32.to_be_bytes().to_vec(); // version/flags
    mean_body.extend_from_slice(mean.as_bytes());
    inner.extend(boxed(b"mean", &mean_body));
    let mut name_body = 0u32.to_be_bytes().to_vec();
    name_body.extend_from_slice(name.as_bytes());
    inner.extend(boxed(b"name", &name_body));
    for v in values {
        let mut data = 1u32.to_be_bytes().to_vec(); // type 1 = UTF-8
        data.extend_from_slice(&0u32.to_be_bytes()); // locale
        data.extend_from_slice(v.as_bytes());
        inner.extend(boxed(b"data", &data));
    }
    boxed(b"----", &inner)
}
```

- [ ] **Step 4: Replace the tag-emission code in `build_udta`**

In `build_udta`, replace everything from the `// Group consecutive same-key text values...` comment through the end of the `tracknumber`/`discnumber` loop (i.e. the block that builds `text` groups, the `for (key, values) in &text` loop, and the separate `trkn`/`disk` loop) with the unified dispatch below. The `hdlr`/`art`/`meta`/`udta` assembly that follows stays unchanged:

```rust
    // Group consecutive same-key values (the DB returns tags ordered by key).
    let mut groups: Vec<(&str, Vec<&str>)> = Vec::new();
    for t in tags {
        match groups.last_mut() {
            Some(g) if g.0 == t.key => g.1.push(&t.value),
            _ => groups.push((&t.key, vec![&t.value])),
        }
    }

    let mut ilst = Vec::new();
    for (key, values) in &groups {
        match crate::tagmap::key_to_mp4(key) {
            Some(crate::tagmap::Mp4Slot::Text(atom)) => ilst.extend(text_atom(atom, values)),
            Some(crate::tagmap::Mp4Slot::Number(atom, width)) => {
                if let Ok(n) = values[0].parse::<u16>() {
                    ilst.extend(number_atom(atom, n, width));
                }
            }
            Some(crate::tagmap::Mp4Slot::Freeform(mean, name)) => {
                ilst.extend(freeform_atom(mean, name, values));
            }
            None => ilst.extend(freeform_atom("com.apple.iTunes", key, values)),
        }
    }
```

Delete the `meta_key` function entirely (now unused).

- [ ] **Step 5: Run the round-trip test and crate suite**

Run: `cargo test -p musefs-format`
Expected: PASS. The existing m4a synthesis/structure tests must remain green; if one asserted exact `udta` bytes for a now-reordered/extended tag set, update its expected bytes to match the unified emission order (vocabulary order then freeform).

- [ ] **Step 6: Commit**

```bash
git add musefs-format/src/mp4.rs
git commit -m "feat(format): synthesize MP4 ---- freeform for unmapped tags"
```

---

## Task 6: Vorbis canonicalization (FLAC + Ogg)

**Files:**
- Modify: `musefs-format/src/vorbiscomment.rs` — `build` (lines 12–23) and `parse` (lines 40–60)
- Modify: `musefs-format/src/lib.rs` — remove the temporary `#[allow(dead_code)]` from `mod tagmap;`
- Test: `musefs-format/src/vorbiscomment.rs` tests

- [ ] **Step 1: Write the failing test**

Add to `vorbiscomment.rs` tests:

```rust
#[test]
fn parse_canonicalizes_known_fields_and_preserves_unknown() {
    // Vendor "musefs", count 2, then ALBUMARTIST and a custom field.
    let tags = vec![
        TagInput::new("albumartist", "VA"),
        TagInput::new("custom_thing", "x"),
    ];
    let body = build(&tags);
    let parsed = parse(&body).unwrap();
    // build upper-cases; parse folds known fields to canonical, keeps unknown verbatim.
    assert_eq!(parsed[0], ("albumartist".to_string(), "VA".to_string()));
    assert_eq!(parsed[1], ("CUSTOM_THING".to_string(), "x".to_string()));
}
```

Update the existing `build_then_parse_round_trips` test's expectation: with
canonicalization, `parse` now returns `("artist", ...)` / `("title", ...)`
(canonical lowercase) rather than `("ARTIST", ...)` / `("TITLE", ...)`:

```rust
        assert_eq!(
            parsed,
            vec![
                ("artist".to_string(), "Boards of Canada".to_string()),
                ("title".to_string(), "Roygbiv".to_string()),
            ]
        );
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p musefs-format vorbiscomment`
Expected: FAIL — `parse` currently returns the raw upper-cased field name, so both the new test and the updated round-trip test fail.

- [ ] **Step 3: Route `build` and `parse` through the vocabulary**

In `build`, change the comment line to use the Vorbis field for canonical keys, falling back to the upper-cased key:

```rust
    for t in tags {
        let field = crate::tagmap::key_to_vorbis(&t.key)
            .map(str::to_string)
            .unwrap_or_else(|| t.key.to_ascii_uppercase());
        let comment = format!("{field}={}", t.value);
        out.extend_from_slice(&(comment.len() as u32).to_le_bytes());
        out.extend_from_slice(comment.as_bytes());
    }
```

In `parse`, fold known field names to canonical, keep unknown verbatim:

```rust
        if let Some((field, value)) = comment.split_once('=') {
            let key = crate::tagmap::vorbis_to_key(field)
                .map(str::to_string)
                .unwrap_or_else(|| field.to_string());
            out.push((key, value.to_string()));
        }
```

- [ ] **Step 4: Remove the temporary allow**

In `lib.rs`, change `#[allow(dead_code)]\nmod tagmap;` back to just:

```rust
mod tagmap;
```

(Every vocabulary lookup is now consumed by Tasks 2–6.)

- [ ] **Step 5: Run the crate suite and clippy**

Run: `cargo test -p musefs-format && cargo clippy --all-targets -- -D warnings`
Expected: PASS (no dead-code warnings). FLAC and Ogg inherit canonicalization because `flac::synthesize_layout` calls `vorbiscomment::build`, and `flac::read_vorbis_comments` / `ogg::read_tags` call `vorbiscomment::parse`.

- [ ] **Step 6: Commit**

```bash
git add musefs-format/src/vorbiscomment.rs musefs-format/src/lib.rs
git commit -m "feat(format): canonicalize Vorbis field names via vocabulary"
```

---

## Task 7: Core casing — stop blanket-lowercasing, lowercase template fields only

**Files:**
- Modify: `musefs-core/src/scan.rs` — `ingest` (currently lines 134–185; the tag-building loop ~146–153)
- Modify: `musefs-core/src/mapping.rs` — `tags_to_fields` (lines 15–23)
- Test: `musefs-core/src/mapping.rs` tests; `musefs-core/src/scan.rs` tests

- [ ] **Step 1: Write the failing test for `tags_to_fields`**

Add to `mapping.rs` tests:

```rust
#[test]
fn tags_to_fields_lowercases_keys_for_template_lookup() {
    let tags = vec![
        Tag::new("MyRating", "5", 0),       // verbatim user-defined key
        Tag::new("albumartist", "VA", 0),
    ];
    let fields = tags_to_fields(&tags);
    assert_eq!(fields.get("myrating"), Some(&"5".to_string()));
    assert_eq!(fields.get("albumartist"), Some(&"VA".to_string()));
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p musefs-core tags_to_fields_lowercases_keys_for_template_lookup`
Expected: FAIL — `tags_to_fields` keys on `t.key` verbatim, so `"myrating"` is absent (the entry is under `"MyRating"`).

- [ ] **Step 3: Lowercase keys in `tags_to_fields`**

Change the loop in `tags_to_fields`:

```rust
    for t in tags {
        map.entry(t.key.to_lowercase())
            .or_insert_with(|| t.value.clone());
    }
```

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test -p musefs-core tags_to_fields_lowercases_keys_for_template_lookup`
Expected: PASS

- [ ] **Step 5: Stop lowercasing keys in `ingest`**

In `scan.rs::ingest`, change the tag-building loop so the key is stored as the
format layer returned it (canonical lowercase for vocabulary matches, verbatim
otherwise) rather than force-lowercased:

```rust
    let mut tags = Vec::new();
    let mut ordinals: HashMap<String, i64> = HashMap::new();
    for (key, value) in probed.tags {
        let ord = ordinals.entry(key.clone()).or_insert(0);
        tags.push(Tag::new(&key, &value, *ord));
        *ord += 1;
    }
    db.replace_tags(track_id, &tags)?;
```

- [ ] **Step 6: Run the core suite**

Run: `cargo test -p musefs-core`
Expected: PASS. If a scan test asserted a lower-cased key for a previously
upper-cased source field (e.g. expecting `"artist"` from a FLAC `ARTIST`), it
still holds — `vorbiscomment::parse` now returns canonical `"artist"`. Update any
test that asserted a *user-defined* key was lowercased.

- [ ] **Step 7: Commit**

```bash
git add musefs-core/src/scan.rs musefs-core/src/mapping.rs
git commit -m "feat(core): preserve user-defined tag key casing; lowercase template fields"
```

---

## Task 8: Document tag handling and limitations in the README

**Files:**
- Modify: `README.md` (add a "Tag handling" section)

- [ ] **Step 1: Add the section**

Append a `## Tag handling` section to `README.md` with this content:

```markdown
## Tag handling

musefs preserves the tags it reads from a backing file when it synthesizes the
served file (always in the same format — it never converts between formats).

**Round-trips losslessly:**

- All text tags. Common fields use a shared canonical vocabulary (so
  `$albumartist`, `$date`, etc. work the same regardless of source format);
  everything else round-trips through the format's extension slot — ID3 `TXXX`,
  MP4 `----` freeform, or a raw Vorbis field — keyed by its own name. Unmapped
  standard ID3 text frames round-trip by their frame id.
- Comments and lyrics (text content).
- User-defined keys keep their original casing (e.g. `MusicBrainz Album Id`).

**Known limitations (lossy edges):**

- All ID3v2.x tags are normalized to **ID3v2.4** on synthesis. Legacy date
  frames (`TYER`, `TDAT`) fold to `date` and are re-emitted as `TDRC`.
- ID3 `COMM`/`USLT` language code and short description are not preserved; they
  are written back with language `XXX` and an empty description. Multiple
  comments/lyrics distinguished only by those collapse to one.
- MP4 `----` `mean` is normalized to `com.apple.iTunes` on write.
- Binary / extended frames are **not** round-tripped and are dropped on scan:
  ID3 `POPM` (ratings), `UFID`, and other non-text frames; MP4 binary atoms
  beyond `trkn`/`disk` (e.g. `tmpo`, `cpil`). Embedded cover art is handled by a
  separate dedicated path, not the tag path.
- If several source tags map to one canonical key (e.g. a `TXXX` whose
  description is `comment` alongside a real `COMM` frame), they merge into a
  single multi-value tag and are re-emitted via that key's native slot.
```

- [ ] **Step 2: Commit**

```bash
git add README.md
git commit -m "docs: document tag handling and round-trip limitations"
```

---

## Self-Review (completed during planning)

**Spec coverage:**
- Canonical vocabulary module → Task 1.
- Native slot as tagged representation (Text / Txxx / Freeform / Number) → Task 1 (`Id3Slot`, `Mp4Slot`).
- Passthrough by human name; verbatim casing → Tasks 2–6 + Task 7.
- ID3 read of TXXX/COMM/USLT + unmapped frames → Task 2.
- ID3 write of COMM/USLT + unmapped frames → Task 3.
- MP4 read of `----` + vocabulary atoms → Task 4.
- MP4 write of `----` → Task 5.
- Vorbis canonicalization (FLAC/Ogg) → Task 6.
- Core casing rule (ingest + tags_to_fields) → Task 7.
- Collision/folding rule → realized naturally (a folded key equals the canonical key string; Tasks 2/4/6 fold via the `*_to_key` lookups) and documented in Task 8.
- README limitations (COMM/USLT qualifiers, MP4 mean, ID3 v2.4 normalization, binary frames, collisions) → Task 8.

**Multi-value:** preserved via ordinals (DB) and per-value frame/atom emission in Tasks 3 and 5.

**Type consistency:** `Id3Slot`/`Mp4Slot` variant names and the `tagmap::*` function names are used identically across Tasks 1–6. `read_freeform`, `freeform_atom`, `comm_like_frame_data`, `is_id3_text_frame_id` are each defined in exactly one task and referenced only after definition.

**Note on scope:** the vocabulary ships with a common, extensible set (Task 1). Losslessness for the long tail does not depend on its breadth (ID3 frame-id passthrough, MP4 `----`, raw Vorbis fields cover it); adding entries later is purely additive (append to `VOCAB`) and only improves canonical naming.
```
