# Fuzzing Tests Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add coverage-guided fuzzing (`cargo-fuzz`) and property-based tests (`proptest`) to the `musefs-format` byte-surgery layer and the `musefs-core` read path, guarding panic-freedom, the byte-identical audio invariant, tag round-trip, and ecosystem interop.

**Architecture:** Pure check helpers live in `musefs-format::fuzz_check` behind a non-default `fuzzing` feature (`#[cfg(any(test, feature = "fuzzing"))]`) so they are shared by the crate's own proptests, the standalone `fuzz/` crate, and `musefs-core`'s tests. Fuzz targets feed arbitrary bytes to the format parsers (panic-freedom) and, on a successful parse, assert the structural byte-identity property. proptest covers the same invariants on stable plus an end-to-end read-fidelity property. A separate batch interop test emits synthesized files and reads them back with Python `mutagen`.

**Tech Stack:** Rust, `cargo-fuzz` (libFuzzer, nightly), `arbitrary`, `proptest`; existing dev-deps `metaflac`/`mp4`/`hound` for fixtures and independent reads; Python `mutagen` + `pytest` for interop; GitHub Actions.

---

## Spec reference

`docs/superpowers/specs/2026-05-27-fuzzing-tests-design.md`. Properties: **A** structural byte-identity, **3** tag round-trip (normalization fixed-point), **B** end-to-end read fidelity, **5** independent-reader (mutagen) interop.

## File structure

- `Cargo.toml` (root) — add `exclude = ["fuzz"]`.
- `musefs-format/Cargo.toml` — `fuzzing` feature; `proptest` dev-dep.
- `musefs-format/src/lib.rs` — declare `fuzz_check` module.
- `musefs-format/src/fuzz_check.rs` — **new**: assertions + shared minimal-file fixtures.
- `musefs-format/tests/proptest_{flac,mp3,mp4,ogg,wav}.rs` — **new**: per-format proptests (Property A + tag round-trip).
- `musefs-core/Cargo.toml` — `proptest` dev-dep + `musefs-format` dev-dep with `features=["fuzzing"]`.
- `musefs-core/tests/proptest_read_fidelity.rs` — **new**: Property B.
- `musefs-core/tests/interop_emit.rs` — **new**: emits synthesized fixtures + manifest (`#[ignore]`).
- `tests/interop/test_mutagen_roundtrip.py` + `tests/interop/requirements.txt` — **new**: Property 5.
- `fuzz/` — **new** standalone crate: `Cargo.toml`, `fuzz_targets/*.rs`, `src/bin/generate_seeds.rs`, `corpus/<target>/`.
- `.github/workflows/fuzz.yml` — **new**.
- `.github/workflows/ci.yml` — add interop job.
- Docs: `CLAUDE.md`, `README.md`, `CHANGELOG.md`, `docs/ROADMAP.md`.

## Key APIs (verified — reference, do not redefine)

Format parsers (all `&[u8] -> Result<_, FormatError>` unless noted):
- FLAC: `flac::locate_audio(data) -> Result<FlacScan>` (`FlacScan{audio_offset,audio_length,preserved}`); `flac::read_pictures`; `flac::synthesize_layout(&FlacScan, &[TagInput], &[ArtInput]) -> Result<RegionLayout>`.
- MP3: `mp3::locate_audio(data) -> Result<Mp3Bounds>` (`{audio_offset,audio_length}`); `mp3::read_tags(&[u8]) -> Vec<(String,String)>`; `mp3::read_pictures(&[u8]) -> Vec<EmbeddedPicture>`; `mp3::synthesize_layout(audio_offset, audio_length, &[TagInput], &[ArtInput]) -> Result<RegionLayout>`.
- MP4: `mp4::locate_audio(buf) -> Result<Mp4Bounds>`; `mp4::read_structure(buf) -> Result<Mp4Scan>`; `mp4::read_tags`/`read_pictures`; `mp4::synthesize_layout(&Mp4Scan, &[TagInput], &[ArtInput]) -> Result<RegionLayout>` (audio = `scan.mdat_payload_offset`/`mdat_payload_len`).
- Ogg: `ogg::locate_audio(data) -> Result<OggScan>` (`{codec,audio_offset,audio_length}`); `ogg::read_metadata(front) -> Result<OggHeader>`; `ogg::read_tags`/`read_pictures`; `ogg::synthesize_layout(&OggHeader, audio_offset, audio_length, &[TagInput], &[OggArt]) -> Result<RegionLayout>`; `ogg::page::parse_page`; `ogg::b64::{b64_window,encode_b64_slice,b64_len}`.
- WAV: `wav::locate_audio(buf) -> Result<WavBounds>` (`{audio_offset,audio_length}`); `wav::read_structure(front) -> Result<WavScan>`; `wav::read_tags`/`read_pictures`; `wav::synthesize_layout(&WavScan, audio_offset, audio_length, &[TagInput], &[ArtInput]) -> Result<RegionLayout>`.

Layout (`musefs_format`): `RegionLayout{segments}`, `.segments()`, `.total_len()`, `.header_len()`; `Segment::{Inline(Vec<u8>), ArtImage{art_id,len}, BackingAudio{offset,len}, OggAudio{offset,len,seq_delta}, OggArtSlice{..}}`; `Segment::len()`. Inputs: `TagInput{key,value}` + `TagInput::new(&str,&str)`; `ArtInput{art_id,mime,description,picture_type,width,height,data_len}`.

Core read path (`musefs_core`): `HeaderCache::new(Mode::Synthesis)`; `cache.resolve(&db, track_id) -> Result<Arc<ResolvedFile>>`; `read_at(&ResolvedFile, &Db, offset, size) -> Result<Vec<u8>>`; `ResolvedFile{layout,total_len,content_version,backing_path,mtime_secs,ogg_index,cache_bytes}`. The audio sub-range of a whole read is `whole[resolved.layout.header_len() as usize..]`.

DB (`musefs_db`): `Db::open_in_memory()`; `db.upsert_track(&NewTrack{backing_path,format,audio_offset,audio_length,backing_size,backing_mtime}) -> i64`; `db.replace_tags(id, &[Tag::new(key,value,ordinal)])`; `db.upsert_art(&NewArt{mime,width,height,data}) -> i64`; `Format::{Flac,Mp3,Mp4,Ogg,Wav}`.

Existing fixtures to reuse: `musefs-core/tests/common/mod.rs::{write_flac, make_flac, flac_block, streaminfo_body, vorbis_comment_body, minimal_m4a}`; per-format minimal valid-file builders already exist in each `musefs-format/src/<fmt>.rs` `#[cfg(test)] mod tests`.

---

## Phase 1 — Format-layer foundation (proptest + shared helpers)

### Task 1.1: Wire `fuzzing` feature, `proptest` dev-dep, and the module

**Files:**
- Modify: `Cargo.toml` (root)
- Modify: `musefs-format/Cargo.toml`
- Modify: `musefs-format/src/lib.rs`

- [ ] **Step 1: Exclude the (not-yet-created) fuzz crate from the workspace**

In root `Cargo.toml`, under `[workspace]`:

```toml
[workspace]
resolver = "2"
members = ["musefs-db", "musefs-format", "musefs-core", "musefs-fuse", "musefs-cli"]
exclude = ["fuzz"]
```

- [ ] **Step 2: Add the feature and dev-dep to `musefs-format/Cargo.toml`**

```toml
[features]
# Exposes pure check helpers + minimal-file fixtures for proptest, the fuzz
# crate, and musefs-core's tests. Off by default.
fuzzing = []

[dev-dependencies]
metaflac = "0.2"
mp4 = "0.14"
crc = "3"
hound = "3"
proptest = "1"
```

- [ ] **Step 3: Declare the module in `musefs-format/src/lib.rs`**

```rust
#[cfg(any(test, feature = "fuzzing"))]
pub mod fuzz_check;
```

- [ ] **Step 4: Verify it compiles both ways**

Run: `cargo build -p musefs-format && cargo build -p musefs-format --features fuzzing`
Expected: both succeed (module file added next task; for this step temporarily allow an empty file).

- [ ] **Step 5: Create an empty module file so the build passes**

Create `musefs-format/src/fuzz_check.rs` with `// populated in Task 1.2`.

Run: `cargo build -p musefs-format --features fuzzing`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add Cargo.toml musefs-format/Cargo.toml musefs-format/src/lib.rs musefs-format/src/fuzz_check.rs
git commit -m "build(format): add fuzzing feature and proptest dev-dep

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

### Task 1.2: Implement the structural byte-identity assertion (Property A)

**Files:**
- Modify: `musefs-format/src/fuzz_check.rs`

- [ ] **Step 1: Write the failing test (incl. a planted-bug case proving the assertion can fail)**

Append to `musefs-format/src/fuzz_check.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::{RegionLayout, Segment};

    #[test]
    fn accepts_a_faithful_layout() {
        // header (inline) + a single backing run [100, 100+50).
        let layout = RegionLayout::new(vec![
            Segment::Inline(vec![0u8; 12]),
            Segment::BackingAudio { offset: 100, len: 50 },
        ]);
        assert_backing_covers_audio(100, 50, &layout);
    }

    #[test]
    fn accepts_contiguous_ogg_runs() {
        let layout = RegionLayout::new(vec![
            Segment::Inline(vec![0u8; 4]),
            Segment::OggAudio { offset: 200, len: 30, seq_delta: 1 },
            Segment::OggAudio { offset: 230, len: 70, seq_delta: 1 },
        ]);
        assert_backing_covers_audio(200, 100, &layout);
    }

    #[test]
    #[should_panic(expected = "backing coverage")]
    fn rejects_dropped_backing_bytes() {
        // Planted bug: layout only covers 40 of the 50 audio bytes.
        let layout = RegionLayout::new(vec![
            Segment::Inline(vec![0u8; 12]),
            Segment::BackingAudio { offset: 100, len: 40 },
        ]);
        assert_backing_covers_audio(100, 50, &layout);
    }

    #[test]
    #[should_panic(expected = "contiguous")]
    fn rejects_shifted_backing_offset() {
        let layout = RegionLayout::new(vec![
            Segment::BackingAudio { offset: 101, len: 50 },
        ]);
        assert_backing_covers_audio(100, 50, &layout);
    }
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p musefs-format --features fuzzing fuzz_check`
Expected: FAIL — `assert_backing_covers_audio` not found.

- [ ] **Step 3: Implement the assertion**

Prepend to `musefs-format/src/fuzz_check.rs` (above the test module):

```rust
//! Pure assertions and minimal-file fixtures shared by proptest, the fuzz
//! crate, and musefs-core tests. Gated behind `cfg(test)` or the `fuzzing`
//! feature so it never ships in release builds.

use crate::layout::{RegionLayout, Segment};

/// Property A — the synthesized layout serves the backing audio range
/// `[audio_offset, audio_offset + audio_length)` exactly once, contiguously,
/// with no metadata segment after audio, and the served length is
/// `header_len + audio_length`. Holds for every format and any tags/art.
pub fn assert_backing_covers_audio(audio_offset: u64, audio_length: u64, layout: &RegionLayout) {
    let mut expected = audio_offset;
    let mut covered = 0u64;
    let mut seen_backing = false;
    for seg in layout.segments() {
        match seg {
            Segment::BackingAudio { offset, len } | Segment::OggAudio { offset, len, .. } => {
                assert_eq!(*offset, expected, "backing segment not contiguous at {expected}");
                expected += *len;
                covered += *len;
                seen_backing = true;
            }
            _ => assert!(!seen_backing, "metadata segment after backing audio"),
        }
    }
    assert!(seen_backing, "no backing audio segment present");
    assert_eq!(covered, audio_length, "backing coverage {covered} != audio length {audio_length}");
    assert_eq!(
        layout.total_len(),
        layout.header_len() + audio_length,
        "total_len != header_len + audio_length",
    );
}
```

- [ ] **Step 4: Run to verify pass (incl. planted-bug panics)**

Run: `cargo test -p musefs-format --features fuzzing fuzz_check`
Expected: PASS (the two `#[should_panic]` cases confirm the assertion bites).

- [ ] **Step 5: Commit**

```bash
git add musefs-format/src/fuzz_check.rs
git commit -m "test(format): add structural byte-identity assertion (Property A)

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

### Task 1.3: Consolidate minimal-file fixtures into `fuzz_check::fixtures`

**Files:**
- Modify: `musefs-format/src/fuzz_check.rs`

Reuses existing, already-tested builders rather than inventing byte layouts (DRY).

- [ ] **Step 1: Locate the existing minimal valid-file builders**

Run: `grep -rn "fn .*-> Vec<u8>\|b\"fLaC\"\|OggS\|RIFF\|to_be_bytes" musefs-format/src/mp3.rs musefs-format/src/ogg/mod.rs musefs-format/src/wav.rs musefs-format/src/mp4.rs`
Read the `#[cfg(test)] mod tests` builder(s) each format uses to construct a parseable file. Note their names/bodies — these are the source for the fixtures below.

- [ ] **Step 2: Write the fixtures module + a test that every fixture parses**

Append to `musefs-format/src/fuzz_check.rs`:

```rust
/// Minimal valid files per format, for proptest/fuzz seeds/interop. FLAC and
/// M4A are ported from `musefs-core/tests/common/mod.rs`; WAV uses `hound`;
/// MP3 and Ogg are lifted from each module's existing `#[cfg(test)] mod tests`
/// builder (see Step 1).
pub mod fixtures {
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
            0x10, 0x00, 0x10, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x0A, 0xC4, 0x42, 0xF0,
            0x00, 0x00, 0x00, 0x00,
        ];
        b.extend_from_slice(&[0u8; 16]);
        b
    }

    fn vorbis_comment_body(vendor: &str, comments: &[&str]) -> Vec<u8> {
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

    /// FLAC = `fLaC` + STREAMINFO + VORBIS_COMMENT + `audio`.
    pub fn flac(audio: &[u8]) -> Vec<u8> {
        let mut out = b"fLaC".to_vec();
        out.extend(flac_block(0, &streaminfo_body(), false));
        out.extend(flac_block(4, &vorbis_comment_body("orig", &["TITLE=Orig"]), true));
        out.extend_from_slice(audio);
        out
    }

    fn bx(kind: &[u8; 4], payload: &[u8]) -> Vec<u8> {
        let mut v = ((8 + payload.len()) as u32).to_be_bytes().to_vec();
        v.extend_from_slice(kind);
        v.extend_from_slice(payload);
        v
    }
    fn m4a_data_atom(type_code: u32, value: &[u8]) -> Vec<u8> {
        let mut p = type_code.to_be_bytes().to_vec();
        p.extend_from_slice(&0u32.to_be_bytes());
        p.extend_from_slice(value);
        bx(b"data", &p)
    }

    /// Minimal moov-first M4A (ported verbatim from tests/common::minimal_m4a).
    pub fn m4a(mdat_payload: &[u8]) -> Vec<u8> {
        let ilst_atoms = [
            bx(b"\xa9nam", &m4a_data_atom(1, b"Orig M4A")),
            bx(b"\xa9ART", &m4a_data_atom(1, b"Orig Artist")),
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
        let trak = bx(b"trak", &bx(b"mdia", &[bx(b"hdlr", &soun_hdlr), minf].concat()));
        let moov = bx(b"moov", &[bx(b"mvhd", &[0u8; 8]), trak, udta].concat());
        [bx(b"ftyp", b"M4A isom"), moov, bx(b"mdat", mdat_payload)].concat()
    }

    /// 16-bit PCM mono WAV via hound, into a byte buffer.
    pub fn wav(samples: &[i16]) -> Vec<u8> {
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: 44_100,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut cursor = std::io::Cursor::new(Vec::new());
        {
            let mut w = hound::WavWriter::new(&mut cursor, spec).unwrap();
            for &s in samples {
                w.write_sample(s).unwrap();
            }
            w.finalize().unwrap();
        }
        cursor.into_inner()
    }

    // mp3() and ogg() are lifted from the existing module test builders found in
    // Step 1; paste each builder body here, renamed, returning Vec<u8>.
    // pub fn mp3() -> Vec<u8> { /* from musefs-format/src/mp3.rs mod tests */ }
    // pub fn ogg_opus() -> Vec<u8> { /* from musefs-format/src/ogg/mod.rs mod tests */ }
}

#[cfg(test)]
mod fixtures_tests {
    use super::fixtures;

    #[test]
    fn flac_fixture_parses() {
        let f = fixtures::flac(&[1, 2, 3, 4, 5, 6, 7, 8]);
        let scan = crate::flac::locate_audio(&f).unwrap();
        assert_eq!(scan.audio_length, 8);
    }

    #[test]
    fn m4a_fixture_parses() {
        let f = fixtures::m4a(&[9u8; 16]);
        let b = crate::mp4::locate_audio(&f).unwrap();
        assert_eq!(b.audio_length, 16);
    }

    #[test]
    fn wav_fixture_parses() {
        let f = fixtures::wav(&[0i16, 1, -1, 100, -100]);
        let b = crate::wav::locate_audio(&f).unwrap();
        assert_eq!(b.audio_length, 10);
    }
}
```

- [ ] **Step 3: Replace the `mp3`/`ogg` placeholders with the real builders**

Paste the builder bodies you found in Step 1, exposing `fixtures::mp3() -> Vec<u8>` and `fixtures::ogg_opus() -> Vec<u8>`. Add matching `*_fixture_parses` tests asserting `mp3::locate_audio` / `ogg::locate_audio` succeed.

- [ ] **Step 4: Run the fixtures tests**

Run: `cargo test -p musefs-format --features fuzzing fixtures`
Expected: PASS for all five formats.

- [ ] **Step 5: De-duplicate `tests/common`**

In `musefs-core/tests/common/mod.rs`, keep its builders (integration tests cannot depend on `musefs-format`'s `fuzzing`-gated module without the dev-dep; that dev-dep is added in Task 2.1). Leave as-is for now; Task 2.2 may switch core tests to `fuzz_check::fixtures` once the dev-dep exists.

- [ ] **Step 6: Commit**

```bash
git add musefs-format/src/fuzz_check.rs
git commit -m "test(format): add shared minimal-file fixtures for fuzz/proptest

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

### Task 1.4: FLAC proptest — Property A + tag round-trip (normalization fixed-point)

**Files:**
- Create: `musefs-format/tests/proptest_flac.rs`

- [ ] **Step 1: Write the failing proptest**

```rust
#![cfg(feature = "fuzzing")]
use musefs_format::fuzz_check::{assert_backing_covers_audio, fixtures};
use musefs_format::{flac, ArtInput, TagInput};
use proptest::prelude::*;

fn tags_strategy() -> impl Strategy<Value = Vec<(String, String)>> {
    proptest::collection::vec(
        ("[A-Z]{1,12}", "[ -~]{0,40}").prop_map(|(k, v)| (k, v)),
        0..8,
    )
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    // Property A: any tags + a valid FLAC -> backing audio covered exactly.
    #[test]
    fn flac_synthesis_preserves_audio(audio in proptest::collection::vec(any::<u8>(), 1..256),
                                      tags in tags_strategy()) {
        let file = fixtures::flac(&audio);
        let scan = flac::locate_audio(&file).unwrap();
        let taginputs: Vec<TagInput> =
            tags.iter().map(|(k, v)| TagInput::new(k, v)).collect();
        let arts: Vec<ArtInput> = Vec::new();
        if let Ok(layout) = flac::synthesize_layout(&scan, &taginputs, &arts) {
            assert_backing_covers_audio(scan.audio_offset, scan.audio_length, &layout);
        }
    }
}
```

- [ ] **Step 2: Run to verify it fails to compile / find the gap**

Run: `cargo test -p musefs-format --features fuzzing --test proptest_flac`
Expected: FAIL or compile error until public re-exports exist. If `musefs_format::{ArtInput,TagInput,flac}` are not public at crate root, use the actual paths (`musefs_format::input::{...}`, `musefs_format::flac`). Confirm with `cargo doc -p musefs-format --features fuzzing --no-deps` or `grep -n "pub use\|pub mod" musefs-format/src/lib.rs`.

- [ ] **Step 3: Fix paths so it compiles and passes**

Adjust imports to the real public paths (verified in Step 2). The fixtures and `assert_backing_covers_audio` are public under the `fuzzing` feature.

- [ ] **Step 4: Run to verify pass**

Run: `cargo test -p musefs-format --features fuzzing --test proptest_flac`
Expected: PASS (128 cases).

- [ ] **Step 5: Add the tag round-trip (normalization fixed-point) case**

Add inside `proptest! { ... }`:

```rust
    // Property 3: our own parser reads back what we synthesized (fixed-point).
    #[test]
    fn flac_tag_roundtrip_is_stable(audio in proptest::collection::vec(any::<u8>(), 1..64),
                                    tags in tags_strategy()) {
        let file = fixtures::flac(&audio);
        let scan = flac::locate_audio(&file).unwrap();
        let taginputs: Vec<TagInput> =
            tags.iter().map(|(k, v)| TagInput::new(k, v)).collect();
        let arts: Vec<ArtInput> = Vec::new();
        let layout = match flac::synthesize_layout(&scan, &taginputs, &arts) {
            Ok(l) => l,
            Err(_) => return Ok(()),
        };
        // Assemble the synthesized front (inline segments only) + reparse.
        let mut front = Vec::new();
        for seg in layout.segments() {
            if let musefs_format::Segment::Inline(b) = seg {
                front.extend_from_slice(b);
            }
        }
        // Reparse must succeed and locate the same audio offset (fixed-point of
        // the metadata region; normalized representation, not raw bytes).
        let meta = flac::read_metadata(&front).unwrap();
        prop_assert_eq!(meta.audio_offset, layout.header_len());
    }
```

- [ ] **Step 6: Run and commit**

Run: `cargo test -p musefs-format --features fuzzing --test proptest_flac`
Expected: PASS.

```bash
git add musefs-format/tests/proptest_flac.rs
git commit -m "test(format): FLAC proptest for audio-preservation + tag round-trip

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

### Task 1.5: MP3 proptest

**Files:**
- Create: `musefs-format/tests/proptest_mp3.rs`

- [ ] **Step 1: Write the proptest (MP3 takes bounds directly, not a scan struct)**

```rust
#![cfg(feature = "fuzzing")]
use musefs_format::fuzz_check::{assert_backing_covers_audio, fixtures};
use musefs_format::{mp3, ArtInput, TagInput};
use proptest::prelude::*;

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    #[test]
    fn mp3_synthesis_preserves_audio(
        tags in proptest::collection::vec(("[A-Z]{1,12}", "[ -~]{0,40}"), 0..8),
    ) {
        let file = fixtures::mp3();
        let bounds = mp3::locate_audio(&file).unwrap();
        let taginputs: Vec<TagInput> = tags.iter().map(|(k, v)| TagInput::new(k, v)).collect();
        let arts: Vec<ArtInput> = Vec::new();
        if let Ok(layout) =
            mp3::synthesize_layout(bounds.audio_offset, bounds.audio_length, &taginputs, &arts)
        {
            assert_backing_covers_audio(bounds.audio_offset, bounds.audio_length, &layout);
        }
    }
}
```

- [ ] **Step 2: Run, fix import paths if needed, verify pass**

Run: `cargo test -p musefs-format --features fuzzing --test proptest_mp3`
Expected: PASS.

- [ ] **Step 3: Commit**

```bash
git add musefs-format/tests/proptest_mp3.rs
git commit -m "test(format): MP3 audio-preservation proptest

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

### Task 1.6: MP4 proptest

**Files:**
- Create: `musefs-format/tests/proptest_mp4.rs`

- [ ] **Step 1: Write the proptest (MP4 needs `read_structure` for the scan; audio = mdat payload)**

```rust
#![cfg(feature = "fuzzing")]
use musefs_format::fuzz_check::{assert_backing_covers_audio, fixtures};
use musefs_format::{mp4, ArtInput, TagInput};
use proptest::prelude::*;

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    #[test]
    fn mp4_synthesis_preserves_audio(
        payload in proptest::collection::vec(any::<u8>(), 1..256),
        tags in proptest::collection::vec(("[A-Z]{1,12}", "[ -~]{0,40}"), 0..8),
    ) {
        let file = fixtures::m4a(&payload);
        let scan = mp4::read_structure(&file).unwrap();
        let taginputs: Vec<TagInput> = tags.iter().map(|(k, v)| TagInput::new(k, v)).collect();
        let arts: Vec<ArtInput> = Vec::new();
        if let Ok(layout) = mp4::synthesize_layout(&scan, &taginputs, &arts) {
            assert_backing_covers_audio(scan.mdat_payload_offset, scan.mdat_payload_len, &layout);
        }
    }
}
```

- [ ] **Step 2: Run, fix paths, verify pass**

Run: `cargo test -p musefs-format --features fuzzing --test proptest_mp4`
Expected: PASS.

- [ ] **Step 3: Commit**

```bash
git add musefs-format/tests/proptest_mp4.rs
git commit -m "test(format): MP4 audio-preservation proptest

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

### Task 1.7: Ogg proptest

**Files:**
- Create: `musefs-format/tests/proptest_ogg.rs`

- [ ] **Step 1: Write the proptest (Ogg needs `read_metadata` for the header; arts is `&[OggArt]`, use empty)**

```rust
#![cfg(feature = "fuzzing")]
use musefs_format::fuzz_check::{assert_backing_covers_audio, fixtures};
use musefs_format::{ogg, TagInput};
use proptest::prelude::*;

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn ogg_synthesis_preserves_audio(
        tags in proptest::collection::vec(("[A-Z]{1,12}", "[ -~]{0,40}"), 0..8),
    ) {
        let file = fixtures::ogg_opus();
        let scan = ogg::locate_audio(&file).unwrap();
        let header = ogg::read_metadata(&file).unwrap();
        let taginputs: Vec<TagInput> = tags.iter().map(|(k, v)| TagInput::new(k, v)).collect();
        // arts is &[OggArt]; audio coverage is independent of art, so use empty.
        if let Ok(layout) = ogg::synthesize_layout(
            &header, scan.audio_offset, scan.audio_length, &taginputs, &[],
        ) {
            assert_backing_covers_audio(scan.audio_offset, scan.audio_length, &layout);
        }
    }
}
```

- [ ] **Step 2: Run, fix paths, verify pass**

Run: `cargo test -p musefs-format --features fuzzing --test proptest_ogg`
Expected: PASS. (Note: `read_metadata` may want only the front of the file; if it errors on the whole file, pass `&file[..front_len]` — confirm against `ogg::read_metadata`'s doc which says it takes `front`.)

- [ ] **Step 3: Commit**

```bash
git add musefs-format/tests/proptest_ogg.rs
git commit -m "test(format): Ogg audio-preservation proptest

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

### Task 1.8: WAV proptest

**Files:**
- Create: `musefs-format/tests/proptest_wav.rs`

- [ ] **Step 1: Write the proptest (WAV needs both `read_structure` (scan) and `locate_audio` (bounds))**

```rust
#![cfg(feature = "fuzzing")]
use musefs_format::fuzz_check::{assert_backing_covers_audio, fixtures};
use musefs_format::{wav, ArtInput, TagInput};
use proptest::prelude::*;

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    #[test]
    fn wav_synthesis_preserves_audio(
        samples in proptest::collection::vec(any::<i16>(), 1..128),
        tags in proptest::collection::vec(("[A-Z]{1,12}", "[ -~]{0,40}"), 0..8),
    ) {
        let file = fixtures::wav(&samples);
        let scan = wav::read_structure(&file).unwrap();
        let bounds = wav::locate_audio(&file).unwrap();
        let taginputs: Vec<TagInput> = tags.iter().map(|(k, v)| TagInput::new(k, v)).collect();
        let arts: Vec<ArtInput> = Vec::new();
        if let Ok(layout) = wav::synthesize_layout(
            &scan, bounds.audio_offset, bounds.audio_length, &taginputs, &arts,
        ) {
            assert_backing_covers_audio(bounds.audio_offset, bounds.audio_length, &layout);
        }
    }
}
```

- [ ] **Step 2: Run, fix paths, verify pass**

Run: `cargo test -p musefs-format --features fuzzing --test proptest_wav`
Expected: PASS.

- [ ] **Step 3: Run the whole format proptest suite and commit**

Run: `cargo test -p musefs-format --features fuzzing`
Expected: PASS (all five formats + fuzz_check unit tests).

```bash
git add musefs-format/tests/proptest_wav.rs
git commit -m "test(format): WAV audio-preservation proptest

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Phase 2 — Core end-to-end (Property B) + interop (Property 5)

### Task 2.1: Core dev-deps

**Files:**
- Modify: `musefs-core/Cargo.toml`

- [ ] **Step 1: Add proptest + the fuzzing-featured format dev-dep**

In `[dev-dependencies]`:

```toml
proptest = "1"
musefs-format = { path = "../musefs-format", features = ["fuzzing"] }
```

(`musefs-format` is already a normal dependency; the dev-dependency line with the feature makes `fuzz_check` available to tests. Cargo unifies features, so confirm the non-dev build is unaffected: `fuzzing` only adds pure code.)

- [ ] **Step 2: Verify**

Run: `cargo test -p musefs-core --no-run`
Expected: compiles; `musefs_format::fuzz_check` resolvable from tests.

- [ ] **Step 3: Commit**

```bash
git add musefs-core/Cargo.toml
git commit -m "build(core): add proptest and fuzzing-featured format dev-dep

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

### Task 2.2: End-to-end read-fidelity proptest (Property B)

**Files:**
- Create: `musefs-core/tests/proptest_read_fidelity.rs`
- Reuse: `musefs-core/tests/common/mod.rs` (`write_flac`)

- [ ] **Step 1: Write the proptest (mirrors `tests/read_at.rs::setup`)**

```rust
mod common;
use common::write_flac;
use musefs_core::{read_at, HeaderCache, Mode};
use musefs_db::{Db, Format, NewTrack, Tag};
use proptest::prelude::*;

fn build(audio: &[u8], title: &str) -> (tempfile::TempDir, Db, i64, Vec<u8>) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("song.flac");
    let (audio_offset, audio_length) = write_flac(&path, &["TITLE=Orig"], audio);
    let meta = std::fs::metadata(&path).unwrap();
    let db = Db::open_in_memory().unwrap();
    let id = db
        .upsert_track(&NewTrack {
            backing_path: path.to_string_lossy().to_string(),
            format: Format::Flac,
            audio_offset,
            audio_length,
            backing_size: meta.len() as i64,
            backing_mtime: meta
                .modified()
                .unwrap()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs() as i64,
        })
        .unwrap();
    db.replace_tags(id, &[Tag::new("title", title, 0)]).unwrap();
    (dir, db, id, audio.to_vec())
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    // Property B: the spliced read serves the original audio bytes verbatim.
    #[test]
    fn read_at_preserves_backing_audio(
        audio in proptest::collection::vec(any::<u8>(), 1..512),
        title in "[ -~]{0,32}",
    ) {
        let (_dir, db, id, original) = build(&audio, &title);
        let resolved = HeaderCache::new(Mode::Synthesis).resolve(&db, id).unwrap();
        let whole = read_at(&resolved, &db, 0, resolved.total_len).unwrap();
        prop_assert_eq!(whole.len() as u64, resolved.total_len);
        let served_audio = &whole[resolved.layout.header_len() as usize..];
        prop_assert_eq!(served_audio, &original[..]);
    }
}
```

- [ ] **Step 2: Run to verify pass**

Run: `cargo test -p musefs-core --test proptest_read_fidelity`
Expected: PASS (64 cases).

- [ ] **Step 3: Confirm the assertion can fail (planted-bug check)**

Temporarily change `&original[..]` to `&original[1..]` (or assert against a mutated copy), run, confirm FAIL, then revert.

Run: `cargo test -p musefs-core --test proptest_read_fidelity` (with the mutation)
Expected: FAIL. Revert and re-run → PASS.

- [ ] **Step 4: Commit**

```bash
git add musefs-core/tests/proptest_read_fidelity.rs
git commit -m "test(core): end-to-end read-fidelity proptest (Property B)

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

### Task 2.3: Interop emit helper (Rust → files + manifest)

**Files:**
- Create: `musefs-core/tests/interop_emit.rs`
- Reuse: `musefs-core/tests/common/mod.rs`

Emits one synthesized file per format with known tags, plus a JSON manifest, into a directory given by `MUSEFS_INTEROP_DIR`. Marked `#[ignore]` so it only runs when the interop job invokes it explicitly. Uses `read_at` to assemble (no FUSE mount).

- [ ] **Step 1: Write the emitter (start with FLAC; extend per format)**

```rust
mod common;
use common::write_flac;
use musefs_core::{read_at, HeaderCache, Mode};
use musefs_db::{Db, Format, NewTrack, Tag};
use std::io::Write;

fn assemble(db: &Db, id: i64) -> Vec<u8> {
    let resolved = HeaderCache::new(Mode::Synthesis).resolve(db, id).unwrap();
    read_at(&resolved, db, 0, resolved.total_len).unwrap()
}

#[test]
#[ignore = "interop fixture emitter; run explicitly with MUSEFS_INTEROP_DIR set"]
fn emit_interop_fixtures() {
    let out = std::env::var("MUSEFS_INTEROP_DIR").expect("set MUSEFS_INTEROP_DIR");
    let out = std::path::Path::new(&out);
    std::fs::create_dir_all(out).unwrap();
    let mut manifest: Vec<(String, String, String)> = Vec::new(); // (file, title, artist)

    // --- FLAC ---
    {
        let src = out.join("src.flac");
        let audio: Vec<u8> = (0..400u32).map(|i| (i % 251) as u8).collect();
        let (audio_offset, audio_length) = write_flac(&src, &["TITLE=Orig"], &audio);
        let meta = std::fs::metadata(&src).unwrap();
        let db = Db::open_in_memory().unwrap();
        let id = db
            .upsert_track(&NewTrack {
                backing_path: src.to_string_lossy().to_string(),
                format: Format::Flac,
                audio_offset,
                audio_length,
                backing_size: meta.len() as i64,
                backing_mtime: 0,
            })
            .unwrap();
        db.replace_tags(
            id,
            &[Tag::new("title", "Interop Title", 0), Tag::new("artist", "Interop Artist", 0)],
        )
        .unwrap();
        let bytes = assemble(&db, id);
        std::fs::write(out.join("out.flac"), &bytes).unwrap();
        manifest.push(("out.flac".into(), "Interop Title".into(), "Interop Artist".into()));
    }

    // --- MP3 / MP4 / Ogg / WAV: repeat the pattern, building the backing file
    // with fuzz_check::fixtures (write the fixture bytes to `src.<ext>`, scan it
    // via musefs_core::scan_directory or upsert_track with bounds from the
    // matching musefs_format::<fmt>::locate_audio), then assemble + record. ---

    let mut f = std::fs::File::create(out.join("manifest.json")).unwrap();
    write!(f, "{}", to_json(&manifest)).unwrap();
}

fn to_json(rows: &[(String, String, String)]) -> String {
    let items: Vec<String> = rows
        .iter()
        .map(|(file, title, artist)| {
            format!("{{\"file\":{file:?},\"title\":{title:?},\"artist\":{artist:?}}}")
        })
        .collect();
    format!("[{}]", items.join(","))
}
```

- [ ] **Step 2: Extend to MP3/MP4/Ogg/WAV**

For each remaining format: write the `fuzz_check::fixtures::<fmt>()` bytes to a `src.<ext>`, derive `audio_offset`/`audio_length` from `musefs_format::<fmt>::locate_audio` (FLAC/MP3/Ogg/WAV) or `read_structure` (MP4 `mdat_payload_offset`/`len`), `upsert_track` with the matching `Format`, set the same known title/artist tags, `assemble`, write `out.<ext>`, and push to the manifest. Add `musefs-format = { features=["fuzzing"] }` is already a dev-dep (Task 2.1) so `fixtures` is available.

- [ ] **Step 3: Smoke-run the emitter locally**

Run: `MUSEFS_INTEROP_DIR=/tmp/musefs-interop cargo test -p musefs-core --test interop_emit -- --ignored emit_interop_fixtures`
Expected: PASS; `/tmp/musefs-interop/out.*` and `manifest.json` exist.

- [ ] **Step 4: Commit**

```bash
git add musefs-core/tests/interop_emit.rs
git commit -m "test(core): emit synthesized interop fixtures + manifest

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

### Task 2.4: mutagen interop test (Property 5)

**Files:**
- Create: `tests/interop/test_mutagen_roundtrip.py`
- Create: `tests/interop/requirements.txt`

- [ ] **Step 1: Pin the dependency**

`tests/interop/requirements.txt`:

```
mutagen==1.47.0
```

- [ ] **Step 2: Write the interop test**

`tests/interop/test_mutagen_roundtrip.py`:

```python
import json
import os

import mutagen


def _read_tag(path, keys):
    f = mutagen.File(path, easy=True)
    assert f is not None, f"mutagen could not open {path}"
    for k in keys:
        vals = f.get(k)
        if vals:
            return vals[0]
    return None


def test_ecosystem_reads_synthesized_tags():
    base = os.environ["MUSEFS_INTEROP_DIR"]
    with open(os.path.join(base, "manifest.json")) as fh:
        manifest = json.load(fh)
    assert manifest, "empty manifest"
    for row in manifest:
        path = os.path.join(base, row["file"])
        title = _read_tag(path, ["title"])
        artist = _read_tag(path, ["artist"])
        assert title == row["title"], f"{row['file']}: title {title!r} != {row['title']!r}"
        assert artist == row["artist"], f"{row['file']}: artist {artist!r} != {row['artist']!r}"
```

- [ ] **Step 3: Run end-to-end locally**

```bash
python -m venv /tmp/interop-venv && /tmp/interop-venv/bin/pip install -r tests/interop/requirements.txt pytest
MUSEFS_INTEROP_DIR=/tmp/musefs-interop cargo test -p musefs-core --test interop_emit -- --ignored emit_interop_fixtures
MUSEFS_INTEROP_DIR=/tmp/musefs-interop /tmp/interop-venv/bin/pytest tests/interop -v
```

Expected: pytest PASS for every format in the manifest. (If a format's `easy` keys differ, adjust `_read_tag` keys; mutagen `easy=True` normalizes `title`/`artist` across formats.)

- [ ] **Step 4: Commit**

```bash
git add tests/interop/test_mutagen_roundtrip.py tests/interop/requirements.txt
git commit -m "test(interop): mutagen reads synthesized tags across formats (Property 5)

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Phase 3 — Fuzz crate (`cargo-fuzz`)

### Task 3.1: Scaffold the fuzz crate

**Files:**
- Create: `fuzz/Cargo.toml`, `fuzz/.gitignore`

- [ ] **Step 1: Install cargo-fuzz and init**

```bash
cargo install cargo-fuzz
cargo +nightly fuzz init   # from repo root; creates fuzz/ as a standalone crate
```

- [ ] **Step 2: Set `fuzz/Cargo.toml` dependencies**

```toml
[package]
name = "musefs-fuzz"
version = "0.0.0"
publish = false
edition = "2021"

[package.metadata]
cargo-fuzz = true

[dependencies]
libfuzzer-sys = "0.4"
arbitrary = { version = "1", features = ["derive"] }
musefs-format = { path = "../musefs-format", features = ["fuzzing"] }

[dependencies.metaflac]
version = "0.2"
[dependencies.hound]
version = "3"

[[bin]]
name = "generate_seeds"
path = "src/bin/generate_seeds.rs"
test = false
doc = false

# Fuzz target bins are appended by `cargo fuzz add` (Tasks 3.3+).

[workspace]
# Standalone: detached from the parent workspace (which also sets exclude=["fuzz"]).
```

- [ ] **Step 3: Verify the crate builds**

Run: `cargo +nightly fuzz build` (no targets yet → builds nothing but validates config) or `cargo build --manifest-path fuzz/Cargo.toml`
Expected: PASS.

- [ ] **Step 4: Commit**

```bash
git add fuzz/Cargo.toml fuzz/.gitignore
git commit -m "build(fuzz): scaffold standalone cargo-fuzz crate

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

### Task 3.2: Arbitrary input generators

**Files:**
- Create: `fuzz/src/lib.rs`

- [ ] **Step 1: Define arbitrary tag/art builders shared by targets**

```rust
use arbitrary::{Arbitrary, Unstructured};
use musefs_format::{ArtInput, TagInput};

pub const MAX_INPUT: usize = 128 * 1024; // reviewer #1: cap input size

/// Build a small vec of TagInputs from fuzzer entropy.
pub fn arb_tags(u: &mut Unstructured) -> arbitrary::Result<Vec<TagInput>> {
    let n = u.int_in_range(0..=8u8)?;
    let mut out = Vec::new();
    for _ in 0..n {
        let key = String::arbitrary(u)?;
        let value = String::arbitrary(u)?;
        out.push(TagInput::new(&key, &value));
    }
    Ok(out)
}

/// Build a small vec of ArtInputs (data_len bounded so synthesis stays cheap).
pub fn arb_arts(u: &mut Unstructured) -> arbitrary::Result<Vec<ArtInput>> {
    let n = u.int_in_range(0..=2u8)?;
    let mut out = Vec::new();
    for i in 0..n {
        out.push(ArtInput {
            art_id: i as i64,
            mime: "image/png".to_string(),
            description: String::arbitrary(u)?,
            picture_type: u.int_in_range(0..=20u32)?,
            width: u.int_in_range(0..=4096u32)?,
            height: u.int_in_range(0..=4096u32)?,
            data_len: u.int_in_range(0..=8192u64)?,
        });
    }
    Ok(out)
}
```

- [ ] **Step 2: Verify**

Run: `cargo build --manifest-path fuzz/Cargo.toml`
Expected: PASS.

- [ ] **Step 3: Commit**

```bash
git add fuzz/src/lib.rs
git commit -m "feat(fuzz): arbitrary tag/art generators + input-size cap

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

### Task 3.3: Per-format fuzz targets (×5)

**Files:**
- Create: `fuzz/fuzz_targets/{flac,mp3,mp4,ogg,wav}.rs`

Each is added with `cargo fuzz add <name>` (appends the `[[bin]]`), then the body replaced. FLAC shown in full; the others follow the same shape with that format's parse + synthesize entry points (signatures in "Key APIs").

- [ ] **Step 1: FLAC target**

`cargo fuzz add flac`, then `fuzz/fuzz_targets/flac.rs`:

```rust
#![no_main]
use libfuzzer_sys::fuzz_target;
use arbitrary::Unstructured;
use musefs_fuzz::{arb_arts, arb_tags, MAX_INPUT};
use musefs_format::{flac, fuzz_check::assert_backing_covers_audio};

fuzz_target!(|data: &[u8]| {
    if data.len() > MAX_INPUT { return; }
    // Panic-freedom: parse arbitrary bytes.
    let _ = flac::read_pictures(data);
    let scan = match flac::locate_audio(data) {
        Ok(s) => s,
        Err(_) => return,
    };
    // Structural byte-identity with arbitrary tags/art.
    let mut u = Unstructured::new(data);
    let tags = arb_tags(&mut u).unwrap_or_default();
    let arts = arb_arts(&mut u).unwrap_or_default();
    if let Ok(layout) = flac::synthesize_layout(&scan, &tags, &arts) {
        assert_backing_covers_audio(scan.audio_offset, scan.audio_length, &layout);
    }
});
```

- [ ] **Step 2: MP3 target**

`cargo fuzz add mp3`, then `fuzz/fuzz_targets/mp3.rs`:

```rust
#![no_main]
use libfuzzer_sys::fuzz_target;
use arbitrary::Unstructured;
use musefs_fuzz::{arb_arts, arb_tags, MAX_INPUT};
use musefs_format::{mp3, fuzz_check::assert_backing_covers_audio};

fuzz_target!(|data: &[u8]| {
    if data.len() > MAX_INPUT { return; }
    let _ = mp3::read_tags(data);
    let _ = mp3::read_pictures(data);
    let bounds = match mp3::locate_audio(data) {
        Ok(b) => b,
        Err(_) => return,
    };
    let mut u = Unstructured::new(data);
    let tags = arb_tags(&mut u).unwrap_or_default();
    let arts = arb_arts(&mut u).unwrap_or_default();
    if let Ok(layout) =
        mp3::synthesize_layout(bounds.audio_offset, bounds.audio_length, &tags, &arts)
    {
        assert_backing_covers_audio(bounds.audio_offset, bounds.audio_length, &layout);
    }
});
```

- [ ] **Step 3: MP4 target**

`cargo fuzz add mp4`, then `fuzz/fuzz_targets/mp4.rs`:

```rust
#![no_main]
use libfuzzer_sys::fuzz_target;
use arbitrary::Unstructured;
use musefs_fuzz::{arb_arts, arb_tags, MAX_INPUT};
use musefs_format::{mp4, fuzz_check::assert_backing_covers_audio};

fuzz_target!(|data: &[u8]| {
    if data.len() > MAX_INPUT { return; }
    let _ = mp4::locate_audio(data);
    let _ = mp4::read_tags(data);
    let _ = mp4::read_pictures(data);
    let scan = match mp4::read_structure(data) {
        Ok(s) => s,
        Err(_) => return,
    };
    let mut u = Unstructured::new(data);
    let tags = arb_tags(&mut u).unwrap_or_default();
    let arts = arb_arts(&mut u).unwrap_or_default();
    if let Ok(layout) = mp4::synthesize_layout(&scan, &tags, &arts) {
        assert_backing_covers_audio(scan.mdat_payload_offset, scan.mdat_payload_len, &layout);
    }
});
```

- [ ] **Step 4: Ogg target**

`cargo fuzz add ogg`, then `fuzz/fuzz_targets/ogg.rs`:

```rust
#![no_main]
use libfuzzer_sys::fuzz_target;
use arbitrary::Unstructured;
use musefs_fuzz::{arb_tags, MAX_INPUT};
use musefs_format::{ogg, fuzz_check::assert_backing_covers_audio};

fuzz_target!(|data: &[u8]| {
    if data.len() > MAX_INPUT { return; }
    let _ = ogg::read_tags(data);
    let _ = ogg::read_pictures(data);
    let scan = match ogg::locate_audio(data) {
        Ok(s) => s,
        Err(_) => return,
    };
    let header = match ogg::read_metadata(data) {
        Ok(h) => h,
        Err(_) => return,
    };
    let mut u = Unstructured::new(data);
    let tags = arb_tags(&mut u).unwrap_or_default();
    if let Ok(layout) =
        ogg::synthesize_layout(&header, scan.audio_offset, scan.audio_length, &tags, &[])
    {
        assert_backing_covers_audio(scan.audio_offset, scan.audio_length, &layout);
    }
});
```

- [ ] **Step 5: WAV target**

`cargo fuzz add wav`, then `fuzz/fuzz_targets/wav.rs`:

```rust
#![no_main]
use libfuzzer_sys::fuzz_target;
use arbitrary::Unstructured;
use musefs_fuzz::{arb_arts, arb_tags, MAX_INPUT};
use musefs_format::{wav, fuzz_check::assert_backing_covers_audio};

fuzz_target!(|data: &[u8]| {
    if data.len() > MAX_INPUT { return; }
    let _ = wav::read_tags(data);
    let _ = wav::read_pictures(data);
    let scan = match wav::read_structure(data) {
        Ok(s) => s,
        Err(_) => return,
    };
    let bounds = match wav::locate_audio(data) {
        Ok(b) => b,
        Err(_) => return,
    };
    let mut u = Unstructured::new(data);
    let tags = arb_tags(&mut u).unwrap_or_default();
    let arts = arb_arts(&mut u).unwrap_or_default();
    if let Ok(layout) =
        wav::synthesize_layout(&scan, bounds.audio_offset, bounds.audio_length, &tags, &arts)
    {
        assert_backing_covers_audio(bounds.audio_offset, bounds.audio_length, &layout);
    }
});
```

- [ ] **Step 6: Build all and short-run each**

Run: `cargo +nightly fuzz build`
Expected: all five targets compile.

Run (each): `cargo +nightly fuzz run flac -- -max_len=131072 -runs=20000`
Expected: no crash; repeat for mp3/mp4/ogg/wav.

- [ ] **Step 7: Commit**

```bash
git add fuzz/Cargo.toml fuzz/fuzz_targets/flac.rs fuzz/fuzz_targets/mp3.rs fuzz/fuzz_targets/mp4.rs fuzz/fuzz_targets/ogg.rs fuzz/fuzz_targets/wav.rs
git commit -m "feat(fuzz): per-format parse + structural-identity targets

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

### Task 3.4: Primitive fuzz targets

**Files:**
- Create: `fuzz/fuzz_targets/{ogg_page,b64,vorbiscomment,tagmap}.rs`

- [ ] **Step 1: ogg_page target (panic-freedom on `parse_page`)**

`cargo fuzz add ogg_page`, then:

```rust
#![no_main]
use libfuzzer_sys::fuzz_target;
use musefs_fuzz::MAX_INPUT;

fuzz_target!(|data: &[u8]| {
    if data.len() > MAX_INPUT { return; }
    // parse_page must never panic on arbitrary bytes.
    let _ = musefs_format::ogg::page::parse_page(data);
});
```

(If `ogg::page` is not publicly re-exported, add `pub` to the `page` module in `musefs-format/src/ogg/mod.rs` or expose `parse_page` via a `#[cfg(feature="fuzzing")] pub use`. Confirm with `grep -n "mod page\|pub use" musefs-format/src/ogg/mod.rs` and adjust in the format crate, committing that small visibility change with this task.)

- [ ] **Step 2: b64 target (window equals full-encode slice)**

`cargo fuzz add b64`, then:

```rust
#![no_main]
use libfuzzer_sys::fuzz_target;
use arbitrary::{Arbitrary, Unstructured};
use musefs_fuzz::MAX_INPUT;
use musefs_format::ogg::b64::{b64_len, b64_window, encode_b64_slice};

fuzz_target!(|data: &[u8]| {
    if data.len() > MAX_INPUT || data.is_empty() { return; }
    let mut u = Unstructured::new(data);
    let img: Vec<u8> = Vec::arbitrary(&mut u).unwrap_or_default();
    if img.is_empty() { return; }
    let total_chars = b64_len(img.len() as u64);
    if total_chars == 0 { return; }
    let out_off = u.int_in_range(0..=total_chars - 1).unwrap_or(0);
    let take = u.int_in_range(1..=total_chars - out_off).unwrap_or(1);
    let win = b64_window(out_off, take, img.len() as u64);
    // Windowed encode must equal the corresponding slice of the full encode.
    let windowed = encode_b64_slice(&img, &win, take);
    let full = base64_full(&img);
    assert_eq!(
        windowed.as_slice(),
        &full.as_bytes()[out_off as usize..(out_off + take) as usize],
    );
});

// Reference full base64 (standard, no padding stripping beyond b64_len's model).
fn base64_full(img: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(img)
}
```

(Verify `encode_b64_slice`'s exact return type and the padding model against `musefs-format/src/ogg/b64.rs`; adjust the reference accordingly so the equivalence is apples-to-apples. Add `base64 = "0.22"` to `fuzz/Cargo.toml` if used.)

- [ ] **Step 3: vorbiscomment + tagmap targets (panic-freedom)**

`cargo fuzz add vorbiscomment` and `cargo fuzz add tagmap`. Each calls the module's public parse/map function on arbitrary bytes/strings (confirm exact names with `grep -n "pub fn" musefs-format/src/vorbiscomment.rs musefs-format/src/tagmap.rs`) and asserts no panic. Example skeleton:

```rust
#![no_main]
use libfuzzer_sys::fuzz_target;
use musefs_fuzz::MAX_INPUT;

fuzz_target!(|data: &[u8]| {
    if data.len() > MAX_INPUT { return; }
    let _ = musefs_format::vorbiscomment::parse(data); // use the real fn name
});
```

- [ ] **Step 4: Build + short-run each**

Run: `cargo +nightly fuzz build`
Then `cargo +nightly fuzz run <target> -- -max_len=131072 -runs=20000` for each primitive target.
Expected: no crash.

- [ ] **Step 5: Commit**

```bash
git add fuzz/Cargo.toml fuzz/fuzz_targets/ogg_page.rs fuzz/fuzz_targets/b64.rs fuzz/fuzz_targets/vorbiscomment.rs fuzz/fuzz_targets/tagmap.rs musefs-format/src/ogg/mod.rs
git commit -m "feat(fuzz): primitive targets (ogg page, b64, vorbiscomment, tagmap)

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

### Task 3.5: Seed generator + seed corpus

**Files:**
- Create: `fuzz/src/bin/generate_seeds.rs`
- Create: `fuzz/corpus/<target>/seed0` (generated, committed)

- [ ] **Step 1: Write the generator**

`fuzz/src/bin/generate_seeds.rs`:

```rust
use musefs_format::fuzz_check::fixtures;
use std::fs;
use std::path::Path;

fn write(target: &str, name: &str, bytes: &[u8]) {
    let dir = Path::new("fuzz/corpus").join(target);
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join(name), bytes).unwrap();
}

fn main() {
    write("flac", "seed0", &fixtures::flac(&[1, 2, 3, 4, 5, 6, 7, 8]));
    write("mp3", "seed0", &fixtures::mp3());
    write("mp4", "seed0", &fixtures::m4a(&[9u8; 32]));
    write("ogg", "seed0", &fixtures::ogg_opus());
    write("wav", "seed0", &fixtures::wav(&[0i16, 1, -1, 100, -100, 32767, -32768]));
    // Primitives share the relevant container seeds.
    write("ogg_page", "seed0", &fixtures::ogg_opus());
    write("vorbiscomment", "seed0", &fixtures::ogg_opus());
    println!("seeds written under fuzz/corpus/");
}
```

- [ ] **Step 2: Generate seeds**

Run: `cargo run --manifest-path fuzz/Cargo.toml --bin generate_seeds`
Expected: `fuzz/corpus/<target>/seed0` files created (each a few KB).

- [ ] **Step 3: Confirm seeds drive the targets**

Run: `cargo +nightly fuzz run flac fuzz/corpus/flac -- -runs=0`
Expected: libFuzzer reads the seed without crashing (`-runs=0` just loads the corpus).

- [ ] **Step 4: Commit (seeds + generator)**

```bash
git add fuzz/src/bin/generate_seeds.rs fuzz/corpus
git commit -m "feat(fuzz): seed corpus generator and committed seeds

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

### Task 3.6: Coverage check (reviewer #3)

- [ ] **Step 1: Generate a coverage report per format target**

```bash
cargo +nightly fuzz coverage flac
cargo cov -- show target/.../coverage/flac/... # or use `cargo +nightly fuzz coverage` HTML output
```

- [ ] **Step 2: Confirm penetration past the magic-byte check**

Inspect that `synthesize_layout` / inner parse lines are covered, not just the `fLaC`/`OggS`/`RIFF` marker check. If a target is stuck at the marker, add a richer seed (e.g. a tagged file with art) to `fuzz/corpus/<target>/` and regenerate. No commit unless seeds change.

---

## Phase 4 — CI + documentation

### Task 4.1: Fuzz workflow

**Files:**
- Create: `.github/workflows/fuzz.yml`

- [ ] **Step 1: Write the workflow (PR build/smoke + scheduled with dynamic-key cache + cmin)**

```yaml
name: Fuzz

on:
  pull_request:
    paths:
      - 'musefs-format/**'
      - 'fuzz/**'
      - '.github/workflows/fuzz.yml'
  push:
    branches: [main]
  schedule:
    - cron: '0 5 * * 1'  # Mondays 05:00 UTC

concurrency:
  group: fuzz-${{ github.ref }}
  cancel-in-progress: true

jobs:
  smoke:
    # Per-PR: build all targets + a few seconds each to catch broken targets.
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@nightly
      - uses: Swatinem/rust-cache@v2
      - name: Install cargo-fuzz
        run: cargo install cargo-fuzz --locked
      - name: Build targets
        run: cargo +nightly fuzz build
      - name: Smoke-run each target
        run: |
          for t in flac mp3 mp4 ogg wav ogg_page b64 vorbiscomment tagmap; do
            echo "== $t =="
            cargo +nightly fuzz run "$t" -- -max_len=131072 -rss_limit_mb=2048 -max_total_time=15
          done

  scheduled:
    if: github.event_name == 'schedule'
    runs-on: ubuntu-latest
    strategy:
      matrix:
        target: [flac, mp3, mp4, ogg, wav, ogg_page, b64, vorbiscomment, tagmap]
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@nightly
      - uses: Swatinem/rust-cache@v2
      - name: Install cargo-fuzz
        run: cargo install cargo-fuzz --locked
      # reviewer #2: actions/cache is immutable per key. Use a dynamic key so the
      # updated corpus is always saved, restoring the most recent prior corpus.
      - name: Restore corpus
        uses: actions/cache@v4
        with:
          path: fuzz/corpus/${{ matrix.target }}
          key: fuzz-corpus-${{ matrix.target }}-${{ github.run_id }}
          restore-keys: |
            fuzz-corpus-${{ matrix.target }}-
      - name: Fuzz
        run: cargo +nightly fuzz run ${{ matrix.target }} -- -max_len=131072 -rss_limit_mb=2048 -timeout=25 -max_total_time=180
      - name: Minimize corpus
        if: always()
        run: cargo +nightly fuzz cmin ${{ matrix.target }} -- -max_len=131072
      - name: Upload crashes
        if: failure()
        uses: actions/upload-artifact@v4
        with:
          name: fuzz-crash-${{ matrix.target }}
          path: fuzz/artifacts/${{ matrix.target }}
          if-no-files-found: ignore
```

- [ ] **Step 2: Validate YAML locally**

Run: `python -c "import yaml,sys; yaml.safe_load(open('.github/workflows/fuzz.yml'))"`
Expected: no error.

- [ ] **Step 3: Commit**

```bash
git add .github/workflows/fuzz.yml
git commit -m "ci: scheduled fuzzing + per-PR build/smoke

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

### Task 4.2: Interop CI job

**Files:**
- Modify: `.github/workflows/ci.yml`

- [ ] **Step 1: Add an interop job after `check`**

```yaml
  interop:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - name: Install libfuse3
        run: sudo apt-get update && sudo apt-get install -y fuse3 libfuse3-dev pkg-config
      - uses: dtolnay/rust-toolchain@stable
      - uses: Swatinem/rust-cache@v2
      - uses: actions/setup-python@v5
        with:
          python-version: '3.x'
      - name: Install mutagen + pytest
        run: pip install -r tests/interop/requirements.txt pytest
      - name: Emit synthesized fixtures
        run: MUSEFS_INTEROP_DIR="$RUNNER_TEMP/interop" cargo test -p musefs-core --test interop_emit -- --ignored emit_interop_fixtures
      - name: mutagen interop assertions
        run: MUSEFS_INTEROP_DIR="$RUNNER_TEMP/interop" pytest tests/interop -v
```

- [ ] **Step 2: Validate + commit**

Run: `python -c "import yaml; yaml.safe_load(open('.github/workflows/ci.yml'))"`
Expected: no error.

```bash
git add .github/workflows/ci.yml
git commit -m "ci: mutagen interop job (Property 5)

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

### Task 4.3: Documentation

**Files:**
- Modify: `CLAUDE.md`, `README.md`, `CHANGELOG.md`, `docs/ROADMAP.md`

- [ ] **Step 1: `CLAUDE.md` — Commands section**

Add under the existing command block:

```bash
# Property tests (run with the normal suite):
cargo test -p musefs-format --features fuzzing
cargo test -p musefs-core --test proptest_read_fidelity

# Coverage-guided fuzzing (nightly + cargo-fuzz):
cargo +nightly fuzz run <flac|mp3|mp4|ogg|wav|ogg_page|b64|vorbiscomment|tagmap>
cargo +nightly fuzz coverage <target>      # penetration check
cargo run --manifest-path fuzz/Cargo.toml --bin generate_seeds

# Independent-reader interop (mutagen):
MUSEFS_INTEROP_DIR=/tmp/i cargo test -p musefs-core --test interop_emit -- --ignored emit_interop_fixtures
MUSEFS_INTEROP_DIR=/tmp/i pytest tests/interop
```

- [ ] **Step 2: `CLAUDE.md` — "Adding a format" checklist**

Append to the existing "Adding a format" bullet: "…then add a `fuzz/fuzz_targets/<fmt>.rs` target, a `fuzz_check::fixtures::<fmt>()` builder + a seed in `generate_seeds`, a `musefs-format/tests/proptest_<fmt>.rs`, and a manifest row in `interop_emit.rs`."

- [ ] **Step 3: `README.md` — Development section**

Add a "Fuzzing & property tests" subsection documenting: proptest runs under `cargo test`; nightly `cargo-fuzz` prerequisite and how to run a target; `cargo fuzz coverage` to confirm penetration; the mutagen interop test.

- [ ] **Step 4: `CHANGELOG.md` — Unreleased**

Add at the top:

```markdown
## [Unreleased]

### Added

- **Fuzzing & property tests:** coverage-guided `cargo-fuzz` targets for every
  format parser (FLAC/MP3/MP4/Ogg/WAV) and primitives (Ogg page, base64,
  VorbisComment, tagmap), plus `proptest` invariants — panic-freedom, the
  byte-identical audio guarantee, and tag round-trip — an end-to-end read
  fidelity property, and a `mutagen` interop test asserting the ecosystem reads
  what we synthesize.
```

- [ ] **Step 5: `docs/ROADMAP.md` — Delivered**

Add a bullet under "Delivered since v0.1.0" summarizing the fuzzing/property-test hardening.

- [ ] **Step 6: Commit**

```bash
git add CLAUDE.md README.md CHANGELOG.md docs/ROADMAP.md
git commit -m "docs: document fuzzing, property tests, and interop

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Final verification

- [ ] `cargo test --workspace` — green (includes core proptests; format proptests need the feature, see next).
- [ ] `cargo test -p musefs-format --features fuzzing` — all five format proptests + `fuzz_check` unit tests green.
- [ ] `cargo clippy --all-targets -- -D warnings` and `cargo fmt --all -- --check` — clean (pre-commit hook enforces these on every commit).
- [ ] `cargo +nightly fuzz build` — all nine targets compile.
- [ ] `cargo +nightly fuzz run <target> -- -max_len=131072 -runs=100000` — clean for each target.
- [ ] `cargo +nightly fuzz coverage flac` — coverage reaches `synthesize_layout`, not just the marker check.
- [ ] Interop: emit fixtures + `pytest tests/interop` — green for all five formats.
- [ ] Planted-bug confirmations performed for `assert_backing_covers_audio` (Task 1.2) and Property B (Task 2.2 Step 3).

## Self-review notes

- **Spec coverage:** Property A → Tasks 1.2–1.8 + 3.3; Property 3 → Task 1.4 Step 5 (extend to other formats as a fast follow if desired; FLAC carries the pattern); Property B → Task 2.2; Property 5 → Tasks 2.3/2.4/4.2; panic-freedom → Tasks 3.3/3.4; corpus → Task 3.5; CI (dynamic cache + cmin + smoke + scheduled) → Task 4.1; coverage check → Task 3.6; docs → Task 4.3. Input-size cap → `MAX_INPUT` (Task 3.2) + CI `-max_len`.
- **Known follow-ups (not blocking):** Property 3 is fully worked for FLAC; replicating it for MP3/MP4/Ogg/WAV is mechanical (reparse the synthesized front with that format's `read_tags`) and can be a fast follow. The `b64` reference-encode equivalence and the exact `vorbiscomment`/`tagmap`/`ogg::page` public function names must be confirmed against the source at implementation time (each such step says so explicitly).
- **Type consistency:** the assertion is `assert_backing_covers_audio` and the fixtures namespace is `fuzz_check::fixtures::{flac,mp3,m4a,ogg_opus,wav}` throughout; the fuzz lib exposes `arb_tags`, `arb_arts`, `MAX_INPUT`.
