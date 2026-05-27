# WAV Synthesis Support Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add WAV (RIFF/WAVE) as a synthesized format so `.wav` files appear in the virtual tree with regenerated tags and art, serving the original `data` chunk payload byte-for-byte.

**Architecture:** A new `musefs-format/src/wav.rs` mirrors `mp3.rs`/`mp4.rs`: a RIFF chunk walker plus `locate_audio`, `read_structure`, `read_tags`, `read_pictures`, and `synthesize_layout`. Synthesis emits a fresh `RIFF`/`WAVE` front (`fmt `, optional `fact`, a native `LIST`/`INFO` chunk, and an embedded `id3 ` chunk) followed by a `BackingAudio` segment for the untouched `data` payload. The `id3 ` chunk reuses MP3's ID3v2 builder via an extracted `build_id3v2_segments` helper, so WAV gets full-fidelity tags + APIC art for free.

**Tech Stack:** Rust workspace (`musefs-db`, `musefs-format`, `musefs-core`, `musefs-fuse`). Tests use the `id3` crate and (new dev-dep) `hound` as independent oracles. Spec: `docs/superpowers/specs/2026-05-27-wav-support-design.md`.

---

## File Structure

- `musefs-db/src/models.rs` — add `Format::Wav` (enum + `as_str`/`parse`).
- `musefs-format/src/error.rs` — add `FormatError::NotWav`.
- `musefs-format/src/mp3.rs` — extract `build_id3v2_segments` (pure refactor, reused by WAV).
- `musefs-format/src/wav.rs` — **new**: RIFF walker, `locate_audio`, `read_structure`, `read_tags`, `read_pictures`, `synthesize_layout`, INFO mapping.
- `musefs-format/src/lib.rs` — declare `pub mod wav;`.
- `musefs-format/Cargo.toml` — add `hound` dev-dependency.
- `musefs-core/src/scan.rs` — probe `.wav`.
- `musefs-core/src/reader.rs` — `Format::Wav` synthesis arm.
- `musefs-fuse/tests/...` — extend the `#[ignore]`d end-to-end read-through test for a WAV.
- `docs/ROADMAP.md` — record WAV as delivered + the out-of-scope edges.
- Tests: `musefs-format/tests/wav_locate.rs`, `wav_synthesize.rs`, `wav_read_tags.rs`; unit tests inside `scan.rs` and `reader.rs`.

---

## Task 1: Format plumbing (`Format::Wav` + `FormatError::NotWav`)

**Files:**
- Modify: `musefs-db/src/models.rs` (enum + `as_str` + `parse` + tests)
- Modify: `musefs-format/src/error.rs:4-15`

- [ ] **Step 1: Add the failing `Format::Wav` test**

In `musefs-db/src/models.rs`, inside the existing `#[cfg(test)] mod tests`, add:

```rust
    #[test]
    fn wav_round_trips() {
        assert_eq!(Format::Wav.as_str(), "wav");
        assert_eq!(Format::parse("wav"), Some(Format::Wav));
    }
```

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo test -p musefs-db wav_round_trips`
Expected: FAIL — `no variant named Wav found for enum Format`.

- [ ] **Step 3: Add the `Wav` variant and arms**

In `musefs-db/src/models.rs`, add `Wav` to the enum and both match arms:

```rust
pub enum Format {
    Flac,
    Mp3,
    M4a,
    Opus,
    Vorbis,
    OggFlac,
    Wav,
}
```

Add to `as_str`'s match: `Format::Wav => "wav",`
Add to `parse`'s match: `"wav" => Some(Format::Wav),`

- [ ] **Step 4: Add `NotWav` to the format error enum**

In `musefs-format/src/error.rs`, add a variant before the closing brace:

```rust
    #[error("not a supported WAV/RIFF file")]
    NotWav,
```

- [ ] **Step 5: Run the test to verify it passes**

Run: `cargo test -p musefs-db wav_round_trips`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add musefs-db/src/models.rs musefs-format/src/error.rs
git commit -m "$(cat <<'EOF'
feat(db,format): add Format::Wav and FormatError::NotWav

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 2: Extract `build_id3v2_segments` from MP3 synthesis

The MP3 ID3v2.4 builder is currently inline in `mp3::synthesize_layout`. Extract its body (everything except the final `BackingAudio` push) into a `pub(crate)` helper so `wav.rs` can fill an `id3 ` chunk with the identical, art-aware logic. MP3 output must stay byte-identical — the existing `mp3_synthesize` suite is the regression guard.

**Files:**
- Modify: `musefs-format/src/mp3.rs:128-208`

- [ ] **Step 1: Replace `synthesize_layout` with a thin wrapper over a new helper**

In `musefs-format/src/mp3.rs`, replace the whole `pub fn synthesize_layout(...) { ... }` (lines 128-208) with:

```rust
/// Build the ID3v2.4 tag region for `tags`/`arts`: an inline 10-byte header
/// followed by text/`TXXX` frames and `APIC` frames whose image bytes are
/// streamed as `ArtImage` segments. Returns the segments (no backing audio) and
/// the total tag length (`10 + frames_len`). Shared by MP3 synthesis and the WAV
/// `id3 ` chunk.
pub(crate) fn build_id3v2_segments(
    tags: &[TagInput],
    arts: &[ArtInput],
) -> Result<(Vec<Segment>, u64)> {
    // Group consecutive same-key values (the DB returns tags ordered by key).
    let mut groups: Vec<(String, Vec<String>)> = Vec::new();
    for t in tags {
        match groups.last_mut() {
            Some(g) if g.0 == t.key => g.1.push(t.value.clone()),
            _ => groups.push((t.key.clone(), vec![t.value.clone()])),
        }
    }

    let mut segments: Vec<Segment> = Vec::new();
    let mut buf: Vec<u8> = Vec::new();
    let mut frames_len: u64 = 0;

    for (key, values) in &groups {
        match key_to_frame(key) {
            Some(id) => {
                let data = text_frame_data(values);
                push_frame_header(&mut buf, id, data.len())?;
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

    for art in arts {
        let framing = apic_framing(art);
        let data_len = framing.len() as u64 + art.data_len;
        push_frame_header(&mut buf, b"APIC", data_len as usize)?;
        buf.extend_from_slice(&framing);
        segments.push(Segment::Inline(std::mem::take(&mut buf)));
        segments.push(Segment::ArtImage {
            art_id: art.art_id,
            len: art.data_len,
        });
        frames_len += 10 + data_len;
    }

    if !buf.is_empty() {
        segments.push(Segment::Inline(std::mem::take(&mut buf)));
    }

    // Prepend the 10-byte ID3v2.4 header now that the total frame length is known.
    let mut header = Vec::with_capacity(10);
    header.extend_from_slice(b"ID3");
    header.extend_from_slice(&[0x04, 0x00]); // version 2.4.0
    header.push(0x00); // flags: no unsync / extended header / footer

    // The total tag size is a 28-bit syncsafe field. Ingestion caps each art well
    // under this, but guard at the format boundary so an oversized tag (e.g. many
    // large pictures summing past the limit) is a hard error, not a truncated file.
    if frames_len > 0x0FFF_FFFF {
        return Err(FormatError::TooLarge);
    }
    header.extend_from_slice(&syncsafe(frames_len as u32));
    segments.insert(0, Segment::Inline(header));

    Ok((segments, 10 + frames_len))
}

/// Build the synthesized region for an MP3: a fresh ID3v2.4 tag (text frames +
/// APIC frames, with image bytes streamed as `ArtImage` segments) followed by the
/// backing audio.
pub fn synthesize_layout(
    audio_offset: u64,
    audio_length: u64,
    tags: &[TagInput],
    arts: &[ArtInput],
) -> Result<RegionLayout> {
    let (mut segments, _tag_len) = build_id3v2_segments(tags, arts)?;
    segments.push(Segment::BackingAudio {
        offset: audio_offset,
        len: audio_length,
    });
    Ok(RegionLayout::new(segments))
}
```

- [ ] **Step 2: Run the MP3 synthesis suite as the regression guard**

Run: `cargo test -p musefs-format --test mp3_synthesize`
Expected: PASS — all existing tests (segment counts, header magic, syncsafe size, oracle parse, oversized-frame errors) unchanged.

- [ ] **Step 3: Confirm the crate still builds and is lint-clean**

Run: `cargo build -p musefs-format && cargo clippy -p musefs-format --all-targets`
Expected: builds, no new warnings.

- [ ] **Step 4: Commit**

```bash
git add musefs-format/src/mp3.rs
git commit -m "$(cat <<'EOF'
refactor(format): extract build_id3v2_segments for reuse

Pure refactor; MP3 synthesized output is byte-identical (guarded by the
mp3_synthesize suite). Lets the upcoming WAV id3 chunk reuse the tag builder.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 3: `wav.rs` — RIFF walker, `locate_audio`, `read_structure`

Create the module with its types, a tolerant chunk walker, and the two read entry points the scanner and reader need. The walker tolerates a trailing chunk whose declared payload runs past the buffer (the `data` chunk in a front-only buffer).

**Files:**
- Create: `musefs-format/src/wav.rs`
- Modify: `musefs-format/src/lib.rs:1-13`
- Test: `musefs-format/tests/wav_locate.rs`

- [ ] **Step 1: Declare the module and create the file skeleton**

In `musefs-format/src/lib.rs`, add after the `pub mod mp4;` line:

```rust
pub mod wav;
```

Create `musefs-format/src/wav.rs` with:

```rust
use crate::error::{FormatError, Result};

/// The served audio bounds of a WAV: the `data` chunk's payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WavBounds {
    pub audio_offset: u64,
    pub audio_length: u64,
}

/// The structural chunks preserved for synthesis: the required `fmt ` payload and
/// the optional `fact` payload (present for non-PCM codecs).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WavScan {
    pub fmt: Vec<u8>,
    pub fact: Option<Vec<u8>>,
}

/// Validate the RIFF/WAVE container header and return the offset of the first
/// chunk (always 12). Rejects RF64/BW64 (their form id is not `RIFF`) and any
/// non-`WAVE` RIFF file.
fn riff_wave_start(buf: &[u8]) -> Result<usize> {
    if buf.len() < 12 || &buf[0..4] != b"RIFF" || &buf[8..12] != b"WAVE" {
        return Err(FormatError::NotWav);
    }
    Ok(12)
}

/// Walk the top-level WAVE chunks, returning `(fourcc, payload_offset, payload_len)`
/// for each chunk whose 8-byte header is present. Advances header-to-header with
/// RIFF word-alignment padding, skipping payloads. Stops (after recording it) when
/// a chunk's declared payload runs past the buffer — e.g. the `data` chunk in a
/// front-only buffer.
fn walk_chunks(buf: &[u8]) -> Vec<([u8; 4], usize, u64)> {
    let mut out = Vec::new();
    let mut pos = match riff_wave_start(buf) {
        Ok(p) => p,
        Err(_) => return out,
    };
    while pos + 8 <= buf.len() {
        let mut id = [0u8; 4];
        id.copy_from_slice(&buf[pos..pos + 4]);
        let size = u32::from_le_bytes([buf[pos + 4], buf[pos + 5], buf[pos + 6], buf[pos + 7]]) as u64;
        let payload_offset = pos + 8;
        out.push((id, payload_offset, size));
        let advance = 8u64 + size + (size & 1); // word-align: pad odd payloads
        match (pos as u64).checked_add(advance) {
            Some(next) if next <= buf.len() as u64 => pos = next as usize,
            _ => break,
        }
    }
    out
}

/// Borrow a chunk's payload bytes if they fit fully in `buf`.
fn chunk_slice(buf: &[u8], offset: usize, len: u64) -> Option<&[u8]> {
    let end = offset.checked_add(len as usize)?;
    buf.get(offset..end)
}

/// Parse the file and return the `data` chunk payload bounds, or an error to skip
/// it. Requires both `fmt ` and `data`, and the `data` payload must fit in `buf`.
pub fn locate_audio(buf: &[u8]) -> Result<WavBounds> {
    riff_wave_start(buf)?;
    let chunks = walk_chunks(buf);
    let has_fmt = chunks.iter().any(|(id, _, _)| id == b"fmt ");
    let data = chunks.iter().find(|(id, _, _)| id == b"data");
    match (has_fmt, data) {
        (true, Some(&(_, off, len))) => {
            if (off as u64).saturating_add(len) > buf.len() as u64 {
                return Err(FormatError::Malformed);
            }
            Ok(WavBounds {
                audio_offset: off as u64,
                audio_length: len,
            })
        }
        _ => Err(FormatError::NotWav),
    }
}

/// Read the preserved structural chunks (`fmt `, optional `fact`) from the front
/// of the file (everything before the `data` payload). Errors if `fmt ` is absent
/// or a preserved chunk's payload is truncated.
pub fn read_structure(front: &[u8]) -> Result<WavScan> {
    riff_wave_start(front)?;
    let chunks = walk_chunks(front);

    let &(_, fmt_off, fmt_len) = chunks
        .iter()
        .find(|(id, _, _)| id == b"fmt ")
        .ok_or(FormatError::NotWav)?;
    let fmt = chunk_slice(front, fmt_off, fmt_len)
        .ok_or(FormatError::Malformed)?
        .to_vec();

    let fact = match chunks.iter().find(|(id, _, _)| id == b"fact") {
        Some(&(_, off, len)) => Some(
            chunk_slice(front, off, len)
                .ok_or(FormatError::Malformed)?
                .to_vec(),
        ),
        None => None,
    };

    Ok(WavScan { fmt, fact })
}
```

- [ ] **Step 2: Write the failing locate/structure tests**

Create `musefs-format/tests/wav_locate.rs`:

```rust
use musefs_format::wav::{locate_audio, read_structure};
use musefs_format::FormatError;

/// A 16-byte PCM `fmt ` payload: mono, 44.1 kHz, 16-bit.
pub fn fmt_pcm_16bit_mono() -> Vec<u8> {
    let mut f = Vec::new();
    f.extend_from_slice(&1u16.to_le_bytes()); // wFormatTag = PCM
    f.extend_from_slice(&1u16.to_le_bytes()); // channels
    f.extend_from_slice(&44_100u32.to_le_bytes()); // sample rate
    f.extend_from_slice(&88_200u32.to_le_bytes()); // byte rate
    f.extend_from_slice(&2u16.to_le_bytes()); // block align
    f.extend_from_slice(&16u16.to_le_bytes()); // bits per sample
    f
}

/// Build a minimal valid `RIFF/WAVE` file from a list of `(fourcc, payload)` chunks.
pub fn build_wav(chunks: &[(&[u8; 4], Vec<u8>)]) -> Vec<u8> {
    let mut body = Vec::new();
    for (id, payload) in chunks {
        body.extend_from_slice(*id);
        body.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        body.extend_from_slice(payload);
        if payload.len() % 2 == 1 {
            body.push(0x00);
        }
    }
    let mut out = Vec::new();
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&((body.len() + 4) as u32).to_le_bytes());
    out.extend_from_slice(b"WAVE");
    out.extend_from_slice(&body);
    out
}

#[test]
fn locate_finds_data_bounds() {
    let data = vec![0x11u8; 10];
    let wav = build_wav(&[(b"fmt ", fmt_pcm_16bit_mono()), (b"data", data.clone())]);
    let bounds = locate_audio(&wav).unwrap();
    assert_eq!(bounds.audio_length, 10);
    assert_eq!(
        &wav[bounds.audio_offset as usize..(bounds.audio_offset + bounds.audio_length) as usize],
        data.as_slice()
    );
}

#[test]
fn locate_rejects_non_wave_and_rf64() {
    assert_eq!(locate_audio(b"not a riff file at all"), Err(FormatError::NotWav));
    let mut rf64 = build_wav(&[(b"fmt ", fmt_pcm_16bit_mono()), (b"data", vec![0u8; 4])]);
    rf64[0..4].copy_from_slice(b"RF64");
    assert_eq!(locate_audio(&rf64), Err(FormatError::NotWav));
}

#[test]
fn locate_requires_fmt_and_data() {
    let only_fmt = build_wav(&[(b"fmt ", fmt_pcm_16bit_mono())]);
    assert_eq!(locate_audio(&only_fmt), Err(FormatError::NotWav));
}

#[test]
fn read_structure_extracts_fmt_and_optional_fact() {
    let fact = 12_345u32.to_le_bytes().to_vec();
    let wav = build_wav(&[
        (b"fmt ", fmt_pcm_16bit_mono()),
        (b"fact", fact.clone()),
        (b"data", vec![0u8; 6]),
    ]);
    let scan = read_structure(&wav).unwrap();
    assert_eq!(scan.fmt, fmt_pcm_16bit_mono());
    assert_eq!(scan.fact, Some(fact));
}

#[test]
fn read_structure_works_on_front_only_buffer() {
    // Truncate to exactly the data payload start (what reader's read_front yields):
    // walk must still surface `fmt ` even though `data`'s payload is absent.
    let wav = build_wav(&[(b"fmt ", fmt_pcm_16bit_mono()), (b"data", vec![0u8; 100])]);
    let bounds = locate_audio(&wav).unwrap();
    let front = &wav[..bounds.audio_offset as usize];
    let scan = read_structure(front).unwrap();
    assert_eq!(scan.fmt, fmt_pcm_16bit_mono());
    assert_eq!(scan.fact, None);
}
```

- [ ] **Step 3: Run the tests to verify they pass**

Run: `cargo test -p musefs-format --test wav_locate`
Expected: PASS (5 tests). If `lib.rs` lacked `pub mod wav;`, the build would fail to resolve `musefs_format::wav` — confirm the declaration was added.

- [ ] **Step 4: Lint**

Run: `cargo clippy -p musefs-format --all-targets`
Expected: no new warnings. (Note: `build_wav`/`fmt_pcm_16bit_mono` are reused by later test files; the unused-warning is silenced because each integration test file that needs them defines its own copy — see Tasks 4 and 5. Keep them as plain `pub fn` here.)

- [ ] **Step 5: Commit**

```bash
git add musefs-format/src/lib.rs musefs-format/src/wav.rs musefs-format/tests/wav_locate.rs
git commit -m "$(cat <<'EOF'
feat(format): add wav module RIFF walker, locate_audio, read_structure

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 4: `wav::synthesize_layout` + INFO builder

Emit the deterministic, fully-sized RIFF: `fmt `, optional `fact`, a `LIST`/`INFO` chunk (when mappable tags exist), an `id3 ` chunk (reusing Task 2's builder), then the `data` header + `BackingAudio` + word-align pad. Validate with `hound` (real WAV reader) and the `id3` crate.

**Files:**
- Modify: `musefs-format/src/wav.rs`
- Modify: `musefs-format/Cargo.toml:12-15`
- Test: `musefs-format/tests/wav_synthesize.rs`

- [ ] **Step 1: Add the `hound` dev-dependency**

In `musefs-format/Cargo.toml`, under `[dev-dependencies]`, add:

```toml
hound = "3"
```

- [ ] **Step 2: Add synthesis code to `wav.rs`**

Append to `musefs-format/src/wav.rs`:

```rust
use crate::input::{ArtInput, TagInput};
use crate::layout::{RegionLayout, Segment};

/// Canonical (lowercase) tag key -> RIFF `INFO` subchunk FourCC. INFO is the
/// broad-compatibility surface with a small vocabulary; richer fields
/// (albumartist, disc, MusicBrainz ids) ride only in the `id3 ` chunk.
fn info_fourcc(key: &str) -> Option<&'static [u8; 4]> {
    Some(match key {
        "title" => b"INAM",
        "artist" => b"IART",
        "album" => b"IPRD",
        "date" => b"ICRD",
        "genre" => b"IGNR",
        "comment" => b"ICMT",
        "tracknumber" => b"ITRK",
        _ => return None,
    })
}

/// Build the `LIST`/`INFO` chunk payload (`"INFO"` + subchunks) from the first
/// value of each mappable tag key, in first-seen order. Returns `None` when no
/// tag maps to an INFO field (so the chunk is omitted entirely).
fn build_info_payload(tags: &[TagInput]) -> Option<Vec<u8>> {
    let mut entries: Vec<(&'static [u8; 4], &str)> = Vec::new();
    let mut used: Vec<&str> = Vec::new();
    for t in tags {
        if used.contains(&t.key.as_str()) {
            continue;
        }
        if let Some(cc) = info_fourcc(&t.key) {
            used.push(t.key.as_str());
            entries.push((cc, t.value.as_str()));
        }
    }
    if entries.is_empty() {
        return None;
    }
    let mut payload = Vec::new();
    payload.extend_from_slice(b"INFO");
    for (cc, value) in entries {
        let mut v = value.as_bytes().to_vec();
        v.push(0x00); // INFO values are NUL-terminated
        payload.extend_from_slice(cc);
        payload.extend_from_slice(&(v.len() as u32).to_le_bytes());
        payload.extend_from_slice(&v);
        if v.len() % 2 == 1 {
            payload.push(0x00); // word-align
        }
    }
    Some(payload)
}

/// Push a fully-inline chunk (`fourcc + LE size + payload + word-align pad`).
fn push_inline_chunk(segments: &mut Vec<Segment>, id: &[u8; 4], payload: &[u8]) {
    let mut chunk = Vec::with_capacity(8 + payload.len() + 1);
    chunk.extend_from_slice(id);
    chunk.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    chunk.extend_from_slice(payload);
    if payload.len() % 2 == 1 {
        chunk.push(0x00);
    }
    segments.push(Segment::Inline(chunk));
}

/// Build the synthesized WAV region: a fresh `RIFF`/`WAVE` front carrying the
/// preserved `fmt `/`fact`, a native `LIST`/`INFO` chunk, and an embedded `id3 `
/// chunk (full ID3v2 + APIC art), followed by the untouched `data` payload as a
/// `BackingAudio` segment. Every length is known up front, so the `RIFF` and
/// chunk size fields are byte-exact.
pub fn synthesize_layout(
    scan: &WavScan,
    audio_offset: u64,
    audio_length: u64,
    tags: &[TagInput],
    arts: &[ArtInput],
) -> Result<RegionLayout> {
    if audio_length > u32::MAX as u64 {
        return Err(FormatError::TooLarge); // RF64 territory; out of scope
    }

    let mut segments: Vec<Segment> = Vec::new();

    push_inline_chunk(&mut segments, b"fmt ", &scan.fmt);
    if let Some(fact) = &scan.fact {
        push_inline_chunk(&mut segments, b"fact", fact);
    }
    if let Some(info) = build_info_payload(tags) {
        push_inline_chunk(&mut segments, b"LIST", &info);
    }

    // Embedded `id3 ` chunk: 8-byte chunk header + the ID3v2 tag segments, padded.
    let (tag_segments, tag_len) = crate::mp3::build_id3v2_segments(tags, arts)?;
    let mut id3_head = Vec::with_capacity(8);
    id3_head.extend_from_slice(b"id3 ");
    id3_head.extend_from_slice(&(tag_len as u32).to_le_bytes());
    segments.push(Segment::Inline(id3_head));
    segments.extend(tag_segments);
    if tag_len % 2 == 1 {
        segments.push(Segment::Inline(vec![0x00]));
    }

    // `data` chunk: header + the original payload (BackingAudio) + word-align pad.
    let mut data_head = Vec::with_capacity(8);
    data_head.extend_from_slice(b"data");
    data_head.extend_from_slice(&(audio_length as u32).to_le_bytes());
    segments.push(Segment::Inline(data_head));
    segments.push(Segment::BackingAudio {
        offset: audio_offset,
        len: audio_length,
    });
    if audio_length % 2 == 1 {
        segments.push(Segment::Inline(vec![0x00]));
    }

    // RIFF size = (everything after the 8-byte "RIFF"+size prefix) = body + "WAVE".
    let body_len: u64 = segments.iter().map(Segment::len).sum();
    let riff_size = body_len + 4;
    if riff_size > u32::MAX as u64 {
        return Err(FormatError::TooLarge);
    }
    let mut header = Vec::with_capacity(12);
    header.extend_from_slice(b"RIFF");
    header.extend_from_slice(&(riff_size as u32).to_le_bytes());
    header.extend_from_slice(b"WAVE");
    segments.insert(0, Segment::Inline(header));

    Ok(RegionLayout::new(segments))
}
```

- [ ] **Step 3: Write the failing synthesis tests**

Create `musefs-format/tests/wav_synthesize.rs`:

```rust
use std::io::Cursor;

use id3::TagLike;
use musefs_format::wav::{synthesize_layout, WavScan};
use musefs_format::{ArtInput, RegionLayout, Segment, TagInput};

fn fmt_pcm_16bit_mono() -> Vec<u8> {
    let mut f = Vec::new();
    f.extend_from_slice(&1u16.to_le_bytes());
    f.extend_from_slice(&1u16.to_le_bytes());
    f.extend_from_slice(&44_100u32.to_le_bytes());
    f.extend_from_slice(&88_200u32.to_le_bytes());
    f.extend_from_slice(&2u16.to_le_bytes());
    f.extend_from_slice(&16u16.to_le_bytes());
    f
}

/// Flatten a layout, substituting `audio` for the backing-audio segment and the
/// matching bytes for each `ArtImage` segment.
fn assemble(layout: &RegionLayout, audio: &[u8], arts: &[(i64, &[u8])]) -> Vec<u8> {
    let mut out = Vec::new();
    for seg in &layout.segments {
        match seg {
            Segment::Inline(b) => out.extend_from_slice(b),
            Segment::BackingAudio { .. } => out.extend_from_slice(audio),
            Segment::ArtImage { art_id, .. } => {
                out.extend_from_slice(arts.iter().find(|(id, _)| id == art_id).unwrap().1)
            }
            other => unreachable!("unexpected segment in WAV layout: {other:?}"),
        }
    }
    out
}

#[test]
fn synthesizes_valid_riff_and_preserves_audio() {
    // 4 little-endian i16 PCM samples = 8 bytes of audio payload.
    let samples: Vec<i16> = vec![1000, -1000, 32000, -32000];
    let audio: Vec<u8> = samples.iter().flat_map(|s| s.to_le_bytes()).collect();
    let scan = WavScan { fmt: fmt_pcm_16bit_mono(), fact: None };
    let tags = vec![TagInput::new("title", "Wave Song"), TagInput::new("artist", "Alice")];

    let layout = synthesize_layout(&scan, 0, audio.len() as u64, &tags, &[]).unwrap();
    let bytes = assemble(&layout, &audio, &[]);

    // total_len equals the bytes actually produced (generate-and-measure).
    assert_eq!(bytes.len() as u64, layout.total_len());

    // RIFF header is well-formed and the size field == file_len - 8.
    assert_eq!(&bytes[0..4], b"RIFF");
    assert_eq!(&bytes[8..12], b"WAVE");
    let riff_size = u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]) as usize;
    assert_eq!(riff_size, bytes.len() - 8);

    // hound (an independent WAV reader) parses the container and recovers the
    // original PCM samples byte-for-byte.
    let mut reader = hound::WavReader::new(Cursor::new(&bytes)).expect("valid wav");
    assert_eq!(reader.spec().channels, 1);
    assert_eq!(reader.spec().sample_rate, 44_100);
    let decoded: Vec<i16> = reader.samples::<i16>().map(|s| s.unwrap()).collect();
    assert_eq!(decoded, samples);
}

#[test]
fn embeds_full_fidelity_id3_tag_with_art() {
    let audio = vec![0u8; 8];
    let art_bytes = vec![0xCAu8; 120];
    let scan = WavScan { fmt: fmt_pcm_16bit_mono(), fact: None };
    let tags = vec![
        TagInput::new("title", "Cover Test"),
        TagInput::new("albumartist", "Various"), // no INFO field -> id3 only
    ];
    let arts = vec![ArtInput {
        art_id: 9,
        mime: "image/jpeg".to_string(),
        description: String::new(),
        picture_type: 3,
        width: 0,
        height: 0,
        data_len: art_bytes.len() as u64,
    }];

    let layout = synthesize_layout(&scan, 0, audio.len() as u64, &tags, &arts).unwrap();
    // Art is a streamed segment, never materialized inline.
    assert!(layout
        .segments
        .iter()
        .any(|s| matches!(s, Segment::ArtImage { art_id: 9, len } if *len == 120)));

    let bytes = assemble(&layout, &audio, &[(9, &art_bytes)]);

    // Locate and parse the embedded `id3 ` chunk with the id3 crate.
    let pos = find_chunk(&bytes, b"id3 ").expect("an id3 chunk");
    let tag = id3::Tag::read_from2(Cursor::new(&bytes[pos.0..pos.0 + pos.1])).unwrap();
    assert_eq!(tag.title(), Some("Cover Test"));
    assert_eq!(tag.get("TPE2").and_then(|f| f.content().text()), Some("Various"));
    let pic = tag.pictures().next().expect("a picture frame");
    assert_eq!(pic.data, art_bytes);
}

#[test]
fn emits_native_info_chunk_for_mapped_tags() {
    let audio = vec![0u8; 8];
    let scan = WavScan { fmt: fmt_pcm_16bit_mono(), fact: None };
    let tags = vec![TagInput::new("title", "Hello"), TagInput::new("artist", "Bob")];
    let layout = synthesize_layout(&scan, 0, audio.len() as u64, &tags, &[]).unwrap();
    let bytes = assemble(&layout, &audio, &[]);

    let (off, len) = find_chunk(&bytes, b"LIST").expect("a LIST chunk");
    let body = &bytes[off..off + len];
    assert_eq!(&body[0..4], b"INFO");
    // Skip the leading "INFO" form type, then walk the subchunks.
    let sub = &body[4..];
    // INAM (title) subchunk value is NUL-terminated "Hello".
    let inam = find_chunk(sub, b"INAM").expect("an INAM subchunk");
    assert_eq!(&sub[inam.0..inam.0 + inam.1], b"Hello\0");
}

#[test]
fn pads_odd_data_payload_to_word_boundary() {
    let audio = vec![0xABu8; 7]; // odd length
    let scan = WavScan { fmt: fmt_pcm_16bit_mono(), fact: None };
    let layout = synthesize_layout(&scan, 0, audio.len() as u64, &[], &[]).unwrap();
    let bytes = assemble(&layout, &audio, &[]);
    // File length is even and total_len accounts for the pad byte.
    assert_eq!(bytes.len() % 2, 0);
    assert_eq!(bytes.len() as u64, layout.total_len());
    // The `data` chunk size field still reports the true (odd) payload length.
    let (off, _) = find_chunk(&bytes, b"data").expect("a data chunk");
    let size = u32::from_le_bytes([bytes[off - 4], bytes[off - 3], bytes[off - 2], bytes[off - 1]]);
    assert_eq!(size, 7);
}

#[test]
fn rejects_audio_over_32bit() {
    let scan = WavScan { fmt: fmt_pcm_16bit_mono(), fact: None };
    let res = synthesize_layout(&scan, 0, (u32::MAX as u64) + 1, &[], &[]);
    assert_eq!(res, Err(musefs_format::FormatError::TooLarge));
}

/// Find the first chunk with `id`, returning `(payload_offset, payload_len)`.
/// Walks the same way as the module (RIFF or bare INFO body when called on a LIST
/// payload). Skips the 12-byte RIFF header when present, else starts at 0.
fn find_chunk(buf: &[u8], id: &[u8; 4]) -> Option<(usize, usize)> {
    let mut pos = if buf.len() >= 12 && &buf[0..4] == b"RIFF" { 12 } else { 0 };
    while pos + 8 <= buf.len() {
        let cid = &buf[pos..pos + 4];
        let size = u32::from_le_bytes([buf[pos + 4], buf[pos + 5], buf[pos + 6], buf[pos + 7]]) as usize;
        if cid == id {
            return Some((pos + 8, size));
        }
        pos += 8 + size + (size & 1);
    }
    None
}
```

- [ ] **Step 4: Run the synthesis tests**

Run: `cargo test -p musefs-format --test wav_synthesize`
Expected: PASS (5 tests). The `hound` oracle confirms the RIFF/`fmt `/`data` structure is valid and PCM samples round-trip.

- [ ] **Step 5: Lint**

Run: `cargo clippy -p musefs-format --all-targets`
Expected: no new warnings.

- [ ] **Step 6: Commit**

```bash
git add musefs-format/src/wav.rs musefs-format/Cargo.toml musefs-format/tests/wav_synthesize.rs
git commit -m "$(cat <<'EOF'
feat(format): synthesize WAV with LIST/INFO + embedded id3 chunk

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 5: `wav::read_tags` + `wav::read_pictures`

Read existing WAV metadata at scan time: parse an embedded `id3 ` chunk (via `mp3::read_tags`/`read_pictures`) and a `LIST`/`INFO` chunk, merging per field with id3 winning and INFO filling gaps. Accept both `id3 ` (taglib/foobar convention) and `ID3 ` FourCC casings.

**Files:**
- Modify: `musefs-format/src/wav.rs`
- Test: `musefs-format/tests/wav_read_tags.rs`

- [ ] **Step 1: Add reading code to `wav.rs`**

Append to `musefs-format/src/wav.rs`:

```rust
use crate::input::EmbeddedPicture;
use std::collections::HashSet;

/// RIFF `INFO` subchunk FourCC -> canonical (lowercase) tag key. Inverse of
/// `info_fourcc`.
fn info_to_key(id: &[u8; 4]) -> Option<&'static str> {
    Some(match id {
        b"INAM" => "title",
        b"IART" => "artist",
        b"IPRD" => "album",
        b"ICRD" => "date",
        b"IGNR" => "genre",
        b"ICMT" => "comment",
        b"ITRK" => "tracknumber",
        _ => return None,
    })
}

/// Find the embedded ID3v2 tag chunk payload, accepting `id3 ` or `ID3 ` casing.
fn find_id3_chunk<'a>(buf: &'a [u8], chunks: &[([u8; 4], usize, u64)]) -> Option<&'a [u8]> {
    let &(_, off, len) = chunks
        .iter()
        .find(|(id, _, _)| id == b"id3 " || id == b"ID3 ")?;
    chunk_slice(buf, off, len)
}

/// Parse `LIST`/`INFO` subchunks into canonical `(key, value)` pairs. `body` is the
/// INFO payload after the leading `"INFO"` FourCC.
fn read_info_tags(body: &[u8]) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut pos = 0usize;
    while pos + 8 <= body.len() {
        let mut id = [0u8; 4];
        id.copy_from_slice(&body[pos..pos + 4]);
        let size = u32::from_le_bytes([body[pos + 4], body[pos + 5], body[pos + 6], body[pos + 7]]) as usize;
        let val_start = pos + 8;
        let val_end = val_start.saturating_add(size).min(body.len());
        if let Some(key) = info_to_key(&id) {
            let raw = String::from_utf8_lossy(&body[val_start..val_end]);
            let value = raw.trim_end_matches('\0').to_string();
            if !value.is_empty() {
                out.push((key.to_string(), value));
            }
        }
        pos = val_start + size + (size & 1);
    }
    out
}

/// Read WAV tags for scan-time seeding: an embedded `id3 ` chunk (full ID3v2) and a
/// `LIST`/`INFO` chunk, merged per field with id3 taking precedence and INFO filling
/// gaps. Walks chunk headers without reading the `data` payload.
pub fn read_tags(buf: &[u8]) -> Vec<(String, String)> {
    let chunks = walk_chunks(buf);

    let from_id3 = find_id3_chunk(buf, &chunks)
        .map(crate::mp3::read_tags)
        .unwrap_or_default();

    let from_info = chunks
        .iter()
        .find(|(id, _, _)| id == b"LIST")
        .and_then(|&(_, off, len)| chunk_slice(buf, off, len))
        .filter(|slice| slice.len() >= 4 && &slice[0..4] == b"INFO")
        .map(|slice| read_info_tags(&slice[4..]))
        .unwrap_or_default();

    let id3_keys: HashSet<&str> = from_id3.iter().map(|(k, _)| k.as_str()).collect();
    let mut out = from_id3.clone();
    for (k, v) in from_info {
        if !id3_keys.contains(k.as_str()) {
            out.push((k, v));
        }
    }
    out
}

/// Read embedded pictures for scan-time art ingestion. Pictures live only in the
/// embedded `id3 ` chunk (INFO has no picture mechanism).
pub fn read_pictures(buf: &[u8]) -> Vec<EmbeddedPicture> {
    let chunks = walk_chunks(buf);
    find_id3_chunk(buf, &chunks)
        .map(crate::mp3::read_pictures)
        .unwrap_or_default()
}
```

- [ ] **Step 2: Write the failing read/merge tests**

Create `musefs-format/tests/wav_read_tags.rs`:

```rust
use id3::{TagLike, Version};
use musefs_format::wav::{read_pictures, read_tags};

fn fmt_pcm_16bit_mono() -> Vec<u8> {
    let mut f = Vec::new();
    f.extend_from_slice(&1u16.to_le_bytes());
    f.extend_from_slice(&1u16.to_le_bytes());
    f.extend_from_slice(&44_100u32.to_le_bytes());
    f.extend_from_slice(&88_200u32.to_le_bytes());
    f.extend_from_slice(&2u16.to_le_bytes());
    f.extend_from_slice(&16u16.to_le_bytes());
    f
}

/// Build a RIFF/WAVE file from `(fourcc, payload)` chunks in order.
fn build_wav(chunks: &[(&[u8; 4], Vec<u8>)]) -> Vec<u8> {
    let mut body = Vec::new();
    for (id, payload) in chunks {
        body.extend_from_slice(*id);
        body.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        body.extend_from_slice(payload);
        if payload.len() % 2 == 1 {
            body.push(0x00);
        }
    }
    let mut out = Vec::new();
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&((body.len() + 4) as u32).to_le_bytes());
    out.extend_from_slice(b"WAVE");
    out.extend_from_slice(&body);
    out
}

/// An `INFO` payload (FourCC + NUL-terminated, word-aligned subchunk values).
fn info_payload(pairs: &[(&[u8; 4], &str)]) -> Vec<u8> {
    let mut p = b"INFO".to_vec();
    for (cc, val) in pairs {
        let mut v = val.as_bytes().to_vec();
        v.push(0x00);
        p.extend_from_slice(*cc);
        p.extend_from_slice(&(v.len() as u32).to_le_bytes());
        p.extend_from_slice(&v);
        if v.len() % 2 == 1 {
            p.push(0x00);
        }
    }
    p
}

/// A standalone ID3v2.4 tag (with a picture) built by the id3 crate.
fn id3_payload_with_picture() -> Vec<u8> {
    let mut tag = id3::Tag::new();
    tag.set_title("Id3 Title");
    tag.set_artist("Id3 Artist");
    tag.add_frame(id3::frame::Picture {
        mime_type: "image/png".to_string(),
        picture_type: id3::frame::PictureType::CoverFront,
        description: String::new(),
        data: vec![0x89, 0x50, 0x4E, 0x47, 1, 2, 3, 4],
    });
    let mut buf = Vec::new();
    tag.write_to(&mut buf, Version::Id3v24).unwrap();
    buf
}

#[test]
fn reads_info_only() {
    let wav = build_wav(&[
        (b"fmt ", fmt_pcm_16bit_mono()),
        (b"LIST", info_payload(&[(b"INAM", "Info Title"), (b"IART", "Info Artist")])),
        (b"data", vec![0u8; 4]),
    ]);
    let tags = read_tags(&wav);
    assert!(tags.contains(&("title".to_string(), "Info Title".to_string())));
    assert!(tags.contains(&("artist".to_string(), "Info Artist".to_string())));
    assert!(read_pictures(&wav).is_empty());
}

#[test]
fn reads_id3_only_including_art() {
    let wav = build_wav(&[
        (b"fmt ", fmt_pcm_16bit_mono()),
        (b"data", vec![0u8; 4]),
        (b"id3 ", id3_payload_with_picture()), // trailing metadata chunk
    ]);
    let tags = read_tags(&wav);
    assert!(tags.contains(&("title".to_string(), "Id3 Title".to_string())));
    let pics = read_pictures(&wav);
    assert_eq!(pics.len(), 1);
    assert_eq!(pics[0].mime, "image/png");
}

#[test]
fn merges_with_id3_winning_and_info_filling_gaps() {
    // id3 has title+artist; INFO has artist (loses) + genre (fills a gap).
    let wav = build_wav(&[
        (b"fmt ", fmt_pcm_16bit_mono()),
        (b"LIST", info_payload(&[(b"IART", "Info Artist"), (b"IGNR", "Ambient")])),
        (b"data", vec![0u8; 4]),
        (b"id3 ", id3_payload_with_picture()),
    ]);
    let tags = read_tags(&wav);
    // id3 artist wins; INFO artist is dropped.
    assert!(tags.contains(&("artist".to_string(), "Id3 Artist".to_string())));
    assert!(!tags.contains(&("artist".to_string(), "Info Artist".to_string())));
    // INFO genre fills the gap (no genre in id3).
    assert!(tags.contains(&("genre".to_string(), "Ambient".to_string())));
}

#[test]
fn returns_empty_when_untagged() {
    let wav = build_wav(&[(b"fmt ", fmt_pcm_16bit_mono()), (b"data", vec![0u8; 4])]);
    assert!(read_tags(&wav).is_empty());
    assert!(read_pictures(&wav).is_empty());
}
```

- [ ] **Step 3: Run the read tests**

Run: `cargo test -p musefs-format --test wav_read_tags`
Expected: PASS (4 tests).

- [ ] **Step 4: Run the whole format crate + lint**

Run: `cargo test -p musefs-format && cargo clippy -p musefs-format --all-targets`
Expected: all pass, no new warnings.

- [ ] **Step 5: Commit**

```bash
git add musefs-format/src/wav.rs musefs-format/tests/wav_read_tags.rs
git commit -m "$(cat <<'EOF'
feat(format): read WAV tags/pictures from id3 + LIST/INFO chunks

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 6: Wire WAV into the scanner

**Files:**
- Modify: `musefs-core/src/scan.rs:5` (import), `:44-52` (`is_supported_audio`), `:80-125` (`probe`)
- Test: `musefs-core/src/scan.rs` (new test in the existing test module area)

- [ ] **Step 1: Write the failing probe test**

In `musefs-core/src/scan.rs`, add a new test module after `mod ogg_probe_tests`:

```rust
#[cfg(test)]
mod wav_probe_tests {
    use super::*;
    use std::io::Write;

    fn build_wav() -> Vec<u8> {
        let mut fmt = Vec::new();
        fmt.extend_from_slice(&1u16.to_le_bytes());
        fmt.extend_from_slice(&1u16.to_le_bytes());
        fmt.extend_from_slice(&44_100u32.to_le_bytes());
        fmt.extend_from_slice(&88_200u32.to_le_bytes());
        fmt.extend_from_slice(&2u16.to_le_bytes());
        fmt.extend_from_slice(&16u16.to_le_bytes());

        let data = vec![0u8; 16];
        let mut body = Vec::new();
        for (id, payload) in [(b"fmt ", &fmt), (b"data", &data)] {
            body.extend_from_slice(id);
            body.extend_from_slice(&(payload.len() as u32).to_le_bytes());
            body.extend_from_slice(payload);
        }
        let mut out = b"RIFF".to_vec();
        out.extend_from_slice(&((body.len() + 4) as u32).to_le_bytes());
        out.extend_from_slice(b"WAVE");
        out.extend_from_slice(&body);
        out
    }

    #[test]
    fn probe_detects_wav() {
        let bytes = build_wav();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("song.wav");
        std::fs::File::create(&path).unwrap().write_all(&bytes).unwrap();

        let probed = probe(&path, &bytes).expect("wav should probe");
        assert_eq!(probed.format, Format::Wav);
        assert_eq!(probed.audio_length, 16);
    }

    #[test]
    fn scan_single_wav_file_ingests_it() {
        let bytes = build_wav();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("single.wav");
        std::fs::File::create(&path).unwrap().write_all(&bytes).unwrap();

        let db = musefs_db::Db::open_in_memory().unwrap();
        let stats = crate::scan_directory(&db, &path).unwrap();
        assert_eq!(stats.scanned, 1);
        assert_eq!(stats.skipped, 0);
    }
}
```

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo test -p musefs-core wav_probe_tests`
Expected: FAIL — `wav` unresolved import / `Format::Wav` not produced by `probe` (probe returns `None` for `.wav`, so `expect` panics).

- [ ] **Step 3: Add `wav` to the import**

In `musefs-core/src/scan.rs`, change line 5 to:

```rust
use musefs_format::{flac, mp3, mp4, ogg, wav, EmbeddedPicture};
```

- [ ] **Step 4: Add `.wav` to `is_supported_audio`**

In `is_supported_audio` (around line 44), add a clause:

```rust
        || has_ext(path, "wav")
```

- [ ] **Step 5: Add the `probe` arm**

In `probe` (in `musefs-core/src/scan.rs`), add before the final `else { None }`:

```rust
    } else if has_ext(path, "wav") {
        let bounds = wav::locate_audio(bytes).ok()?;
        Some(Probed {
            format: Format::Wav,
            audio_offset: bounds.audio_offset,
            audio_length: bounds.audio_length,
            tags: wav::read_tags(bytes),
            pictures: wav::read_pictures(bytes),
        })
```

- [ ] **Step 6: Run the tests to verify they pass**

Run: `cargo test -p musefs-core wav_probe_tests`
Expected: PASS (2 tests).

- [ ] **Step 7: Commit**

```bash
git add musefs-core/src/scan.rs
git commit -m "$(cat <<'EOF'
feat(core): scan .wav files into the store

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 7: Wire WAV into the reader (synthesis dispatch)

**Files:**
- Modify: `musefs-core/src/reader.rs:8` (import), `:252-309` (the `match track.format`)
- Test: `musefs-core/src/reader.rs` (new test in the existing `#[cfg(test)]` module that already holds `resolves_and_reads_opus_with_identical_audio`)

- [ ] **Step 1: Write the failing reader integration test**

In `musefs-core/src/reader.rs`, inside the existing test module (the one containing `resolves_and_reads_opus_with_identical_audio`, near line 552), add:

```rust
    fn build_wav_file(path: &Path) -> (u64, u64, Vec<u8>) {
        use std::io::Write;
        let mut fmt = Vec::new();
        fmt.extend_from_slice(&1u16.to_le_bytes());
        fmt.extend_from_slice(&1u16.to_le_bytes());
        fmt.extend_from_slice(&44_100u32.to_le_bytes());
        fmt.extend_from_slice(&88_200u32.to_le_bytes());
        fmt.extend_from_slice(&2u16.to_le_bytes());
        fmt.extend_from_slice(&16u16.to_le_bytes());

        let data: Vec<u8> = (0..32u8).collect();
        let mut body = Vec::new();
        for (id, payload) in [(b"fmt ", &fmt), (b"data", &data)] {
            body.extend_from_slice(id);
            body.extend_from_slice(&(payload.len() as u32).to_le_bytes());
            body.extend_from_slice(payload);
        }
        let mut bytes = b"RIFF".to_vec();
        bytes.extend_from_slice(&((body.len() + 4) as u32).to_le_bytes());
        bytes.extend_from_slice(b"WAVE");
        bytes.extend_from_slice(&body);

        let audio_offset = (bytes.len() - data.len()) as u64;
        std::fs::File::create(path).unwrap().write_all(&bytes).unwrap();
        (audio_offset, data.len() as u64, data)
    }

    #[test]
    fn resolves_and_reads_wav_with_identical_audio() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("track.wav");
        let (audio_offset, audio_length, original_data) = build_wav_file(&path);

        let db = Db::open_in_memory().unwrap();
        let meta = std::fs::metadata(&path).unwrap();
        let track_id = db
            .upsert_track(&NewTrack {
                backing_path: path.to_string_lossy().to_string(),
                format: Format::Wav,
                audio_offset: audio_offset as i64,
                audio_length: audio_length as i64,
                backing_size: meta.len() as i64,
                backing_mtime: mtime_secs(&meta),
            })
            .unwrap();
        db.replace_tags(track_id, &[Tag::new("title", "Wave One", 0)])
            .unwrap();

        let cache = HeaderCache::new(Mode::Synthesis);
        let resolved = cache.resolve(&db, track_id).unwrap();
        let out = read_at(&resolved, &db, 0, resolved.total_len).unwrap();

        // The synthesized output is a valid WAV; its data payload is byte-identical
        // to the original audio.
        let bounds = musefs_format::wav::locate_audio(&out).unwrap();
        assert_eq!(
            &out[bounds.audio_offset as usize..(bounds.audio_offset + bounds.audio_length) as usize],
            original_data.as_slice()
        );

        // The title was synthesized into the embedded id3 chunk.
        let tags = musefs_format::wav::read_tags(&out);
        assert!(tags.contains(&("title".to_string(), "Wave One".to_string())));
    }
```

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo test -p musefs-core resolves_and_reads_wav_with_identical_audio`
Expected: FAIL — `wav` unresolved / `Format::Wav` arm missing (non-exhaustive match compile error).

- [ ] **Step 3: Add `wav` to the reader import**

In `musefs-core/src/reader.rs`, change line 8 to:

```rust
use musefs_format::{mp3, mp4, wav, RegionLayout, Segment};
```

- [ ] **Step 4: Add the `Format::Wav` synthesis arm**

In `HeaderCache::resolve`'s `match track.format` (the block at lines 252-309), add an arm after the `Format::M4a => { ... }` arm:

```rust
                    Format::Wav => {
                        // Read only the front (RIFF header + fmt/fact); the data
                        // payload is served from the backing file at read time.
                        let front =
                            read_front(Path::new(&track.backing_path), track.audio_offset as u64)?;
                        let scan = wav::read_structure(&front)?;
                        wav::synthesize_layout(
                            &scan,
                            track.audio_offset as u64,
                            track.audio_length as u64,
                            &inputs,
                            &art_inputs,
                        )?
                    }
```

- [ ] **Step 5: Run the test to verify it passes**

Run: `cargo test -p musefs-core resolves_and_reads_wav_with_identical_audio`
Expected: PASS.

- [ ] **Step 6: Run the full core suite + lint**

Run: `cargo test -p musefs-core && cargo clippy -p musefs-core --all-targets`
Expected: all pass, no new warnings.

- [ ] **Step 7: Commit**

```bash
git add musefs-core/src/reader.rs
git commit -m "$(cat <<'EOF'
feat(core): synthesize WAV files in the read path

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 8: End-to-end mount test + roadmap update

**Files:**
- Modify: `musefs-fuse/tests/` — the `#[ignore]`d `end_to_end_read_through_mount` test file
- Modify: `docs/ROADMAP.md`

- [ ] **Step 1: Locate the e2e test and its WAV-fixture insertion point**

Run: `grep -rn "end_to_end_read_through_mount\|fn build_\|Format::" musefs-fuse/tests/`
Expected: identifies the test file and the existing per-format fixture pattern (how a backing file is written, scanned, and read back through the mount).

- [ ] **Step 2: Add a WAV fixture to the e2e test**

In the e2e test file, following the existing pattern for other formats, write a small valid PCM `.wav` into the backing directory before the mount, then after mounting read the corresponding virtual path and assert: (a) the served bytes start with `RIFF`/`WAVE`, and (b) `musefs_format::wav::locate_audio` on the served bytes yields a `data` region byte-identical to the original `data` payload. Use the same `build_wav` shape as Task 7 Step 1 (RIFF + `fmt ` PCM 16-bit mono + `data`). Mirror the exact scan/mount/read helper calls the existing formats use in this file (do not invent new helpers).

- [ ] **Step 3: Run the e2e test (requires /dev/fuse)**

Run: `cargo test -p musefs-fuse -- --ignored end_to_end_read_through_mount`
Expected: PASS. If the environment lacks `/dev/fuse`/libfuse, note that this test cannot run here and must be run in an environment that has them; do not mark the task complete on a skipped run.

- [ ] **Step 4: Update the roadmap**

In `docs/ROADMAP.md`, in the "Formats" delivered list (line ~11) add WAV, and in the Post-MVP "Formats" section (line ~97) record the WAV out-of-scope edges:

```markdown
- **WAV (RIFF/WAVE)** is delivered: the `data` chunk payload is served verbatim,
  with a synthesized front carrying a native `LIST`/`INFO` chunk and an embedded
  `id3 ` chunk (full ID3v2 + art). Out of scope: RF64/BW64 (>4 GiB), preserving
  non-essential chunks (`bext`/`cue `/`smpl`), and seek-based scanning of large
  files.
```

- [ ] **Step 5: Run the whole workspace test suite**

Run: `cargo test`
Expected: all crates pass (the FUSE e2e remains `#[ignore]`d in the default run).

- [ ] **Step 6: Commit**

```bash
git add musefs-fuse/tests docs/ROADMAP.md
git commit -m "$(cat <<'EOF'
test(fuse): cover WAV in end-to-end read-through mount; record in roadmap

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Final Verification

- [ ] Run `cargo test` (whole workspace) — all pass.
- [ ] Run `cargo test -p musefs-fuse -- --ignored` in an environment with `/dev/fuse` — WAV e2e passes.
- [ ] Run `cargo clippy --all-targets` — no new warnings.
- [ ] Run `cargo fmt --check` — formatted.

---

## Spec Coverage Self-Review

- §1/§3 splice model + serving layout → Task 4 (`synthesize_layout`, RIFF size, word-align).
- §2 accepted shape (require `fmt `+`data`, reject RF64/BW64) → Task 3 (`riff_wave_start`, `locate_audio`) + Task 4 (`TooLarge` >4 GiB guard).
- §4 scan-time reading, both sources, id3-wins merge, walker skips `data` → Task 5.
- §5 `build_id3v2_segments` refactor (MP3 byte-identical) → Task 2.
- §6 wiring (db `Format::Wav`, `lib.rs` module, scan probe, reader arm, structure-only free) → Tasks 1, 3, 6, 7.
- §7 scan buffering (`&[u8]`, no new I/O pattern) → Tasks 5/6 (functions take `&[u8]`).
- §8 testing (locate, walker, merge precedence, byte-exactness, oracle, e2e, MP3 regression) → Tasks 2–8.
- §9 out-of-scope recorded → Task 8 Step 4.
