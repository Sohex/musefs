# Binary Tags Phase 5 — Test-Surface Expansion Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Close the remaining §Testing gaps for binary-tag handling: WAV round-trip, FLAC `APPLICATION`/`CUESHEET` round-trip, the byte-identical-audio invariant with binary frames present, query-split tree correctness, legacy-FLAC backfill, V1→V2 data preservation, MP3 fuzz coverage, and the mutagen interop proof.

**Architecture:** This phase is mostly **tests** that characterize and lock the binary-tag machinery already shipped in Phases 1–4. It carries **one small production fix** (Task 5): Phase 4 shipped the FLAC legacy-track *serve-path fallback* but not the spec-mandated *`revalidate` backfill* (spec §1, §Testing), so `revalidate` must learn to re-scan unchanged FLAC tracks that still lack `structural_blocks`. The other two non-test changes are mechanical (an MP3 fuzz target gaining `arb_binary_tags`, and the interop emitter gaining binary fixtures + a `binary_manifest.json`). For every characterization test, a failure on first run is a real regression to file, not a TODO — the one genuine fail-first is Task 5, whose test is red until its production fix lands.

**Tech Stack:** Rust workspace (musefs-db / musefs-format / musefs-core), rusqlite (SQLite), proptest (`fuzzing` feature gate in `musefs-format`), `id3` + `metaflac` + `hound` test oracles, `cargo-fuzz` (nightly, out-of-workspace `fuzz/` crate), Python + mutagen (interop, `tests/interop`).

---

## Context the engineer needs before starting

**Phases 1–4 are merged.** The full binary-tag pipeline exists and is partly tested. Do **not** re-create production code or duplicate existing tests. The shipped surface you build on:

- `Segment::BinaryTag { payload_id: i64, len: u64 }` (`musefs-format/src/layout.rs`).
- `BinaryTagInput { key, payload_id, len }` and `EmbeddedBinaryTag { key, payload }` (`musefs-format/src/input.rs`).
- DB: `Db::set_binary_tags(track_id, &[BinaryTag])`, `Db::get_binary_tags(track_id) -> Vec<BinaryTag rows with .key/.rowid/.byte_len>`, `Db::read_binary_tag_chunk(rowid, offset, len)`, `Db::set_structural_blocks`, `Db::get_structural_blocks` (`musefs-db/src/tags.rs`, `structural.rs`). `BinaryTag { key, payload, ordinal }` and `StructuralBlock { kind, ordinal, body }` (`musefs-db/src/models.rs`).
- Migration V2: `value_blob` column on `tags` + `structural_blocks` table; `user_version → 2` (`musefs-db/src/schema.rs`).
- Per-format parse/synthesis: `mp3::read_binary_tags(&[u8]) -> (Vec<EmbeddedBinaryTag>, Vec<(String,String)>)` (opaque, promoted) and `build_id3v2_segments`; `wav::read_binary_tags(&[u8])` (extracts the `id3 ` chunk, then delegates to `mp3::read_binary_tags`) and `wav::synthesize_layout`; `mp4::read_binary_tags`; `flac::synthesize_layout(structural, audio_offset, audio_length, tags, binary_tags, arts)`.

**Existing tests you must NOT duplicate:**

- `musefs-format/tests/proptest_mp3.rs::binary_tags_round_trip_survives_byte_identically` — MP3 PRIV/GEOB/SYLT opaque + POPM/UFID promotion + dual-UFID owner-uniqueness. **MP3 round-trip + UFID coexistence are DONE.**
- `musefs-format/tests/proptest_mp4.rs::mp4_binary_freeform_round_trips_byte_identically` — MP4 `----` round-trip + byte-identical audio. **DONE.**
- `musefs-db/src/tags.rs` tests `text_queries_exclude_binary_rows` (DB-layer query split) and `binary_tags_round_trip_and_are_independent_of_text`. **DB-layer query split is DONE.**
- `musefs-core/src/reader.rs::binary_tag_serve_tests` — resolve emits `BinaryTag`, `read_at` serves it.
- `musefs-core/tests/flac_binary_tags.rs` — scan split (`scan_splits_flac_into_structural_store_and_binary_tags`), serve (`rescanned_flac_serves_valid_file_with_binary_tags`), legacy fallback (`legacy_flac_without_structural_rows_serves_via_front_read_fallback`).
- `musefs-core/tests/metrics.rs::rescanned_flac_resolve_does_no_front_read` — fresh-scan front-read elimination.
- `musefs-db/src/schema.rs::migration_v2_tests::v2_adds_value_blob_and_structural_blocks_and_is_idempotent` — column/table added + NULL default + idempotent.
- `fuzz/fuzz_targets/mp4.rs` already calls `arb_binary_tags`; `fuzz/corpus/mp4/seed_binary` exists.

**The gaps this phase fills (one task each):**

1. WAV binary-tag round-trip proptest (the format-layer round-trip exists for MP3/MP4 but **not WAV**, whose `read_binary_tags` is untested).
2. FLAC `APPLICATION`/`CUESHEET` payload round-trip (proptest, payload-fidelity only — **not** byte-identical block order).
3. Byte-identical-audio invariant **with binary frames present**, at the core read path (MP3).
4. Query-split correctness at the **tree** level (binary frame doesn't change the rendered path / doesn't leak into `tags_to_fields`).
5. Legacy-FLAC **backfill after `revalidate`**: structural rows + `value_blob` get populated and the front read stops firing.
6. V1→V2 migration **data preservation** (existing rows survive; `value_blob` defaults NULL).
7. MP3 fuzz target gains `arb_binary_tags` + an MP3 `seed_binary`.
8. Interop (mutagen): MP3 `POPM`/`UFID`/`PRIV`/`GEOB` + MP4 `----` survive the mount, asserted by an independent reader.

**Conventions:**
- `musefs-format` proptests are behind `#![cfg(feature = "fuzzing")]`; run with `--features fuzzing`.
- `musefs-core` metrics tests are behind `#![cfg(feature = "metrics")]`; run with `--features metrics`.
- The `fuzz/` crate is **out of the workspace** — `cargo test`/`clippy` never builds it. Verify fuzz changes with `cargo +nightly fuzz build <target>`.
- **Pre-push:** `cargo fmt --all --check` (CI fmt gate) and `cargo clippy --all-targets`.

**Commit after every task.** Each task compiles green and its test passes (or, if it fails, you've found a real bug — stop and report).

---

## File structure

| File | Responsibility | Change |
|------|----------------|--------|
| `musefs-format/tests/proptest_wav.rs` | WAV synthesis proptests | **Add** WAV binary-tag round-trip test + a `wav_with_id3` source builder |
| `musefs-core/tests/flac_binary_tags.rs` | FLAC binary integration tests | **Add** an arbitrary-payload `APPLICATION`/`CUESHEET` re-scan round-trip + a revalidate-backfill test |
| `musefs-core/tests/proptest_read_fidelity.rs` | Core read-path fidelity proptests | **Add** an MP3 byte-identical-audio test with binary frames present |
| `musefs-core/tests/binary_tag_tree.rs` | New: tree-level query-split test | **Create** |
| `musefs-db/src/structural.rs` | Structural-block store | **Add** `track_ids_with_structural_blocks` query (Task 5 production fix) |
| `musefs-core/src/scan.rs` | Scan / revalidate maintenance pass | **Modify** `revalidate_with` to re-scan unchanged FLAC tracks lacking structural blocks (Task 5 production fix) |
| `musefs-core/tests/metrics.rs` | Syscall accounting | **Add** revalidate-backfill front-read-elimination assertion (paired with task 5) |
| `musefs-db/src/schema.rs` | Migrations | **Add** a V1→V2 data-preservation test |
| `fuzz/src/lib.rs` | Fuzz entropy helpers | (unchanged — `arb_binary_tags` already exists) |
| `fuzz/fuzz_targets/mp3.rs` | MP3 fuzz target | **Modify** to drive `arb_binary_tags` |
| `fuzz/src/bin/generate_seeds.rs` | Corpus seed generator | **Add** an MP3 `seed_binary` |
| `musefs-core/tests/interop_emit.rs` | Interop fixture emitter | **Add** binary MP3 + MP4 fixtures + `binary_manifest.json` |
| `tests/interop/test_mutagen_roundtrip.py` | mutagen interop assertions | **Add** `test_binary_frames_survive` |

---

## Task 1: WAV binary-tag round-trip proptest

The format-layer round-trip is proven for MP3 and MP4 but **not WAV**, even though `wav::read_binary_tags` ships. WAV binary frames are ID3 frames inside an `id3 ` RIFF chunk; classification reuses `mp3::read_binary_tags`. This test builds a source WAV carrying ID3 binary frames, parses them, round-trips through the DB, synthesizes a fresh WAV, and re-parses — asserting opaque payloads survive byte-identically and `POPM`/`UFID` promote.

**Files:**
- Modify: `musefs-format/tests/proptest_wav.rs`

- [ ] **Step 1: Add the source-WAV builder and the proptest**

Append to `musefs-format/tests/proptest_wav.rs`. Note the imports at the top must grow to include `BinaryTagInput`, `Segment`, and the `wav::WavScan` type; the existing file already imports `wav`, `ArtInput`, `TagInput`. Replace the top `use` line accordingly and append the new helper + test:

```rust
// (top of file) — extend the existing import line:
use musefs_format::{wav, ArtInput, BinaryTagInput, RegionLayout, Segment, TagInput};

/// A 16-bit mono PCM `fmt ` body (matches musefs-format/tests/wav_synthesize.rs).
fn fmt_pcm_16bit_mono() -> Vec<u8> {
    let mut f = Vec::new();
    f.extend_from_slice(&1u16.to_le_bytes()); // audio_format = PCM
    f.extend_from_slice(&1u16.to_le_bytes()); // channels = 1
    f.extend_from_slice(&44_100u32.to_le_bytes());
    f.extend_from_slice(&88_200u32.to_le_bytes());
    f.extend_from_slice(&2u16.to_le_bytes());
    f.extend_from_slice(&16u16.to_le_bytes());
    f
}

/// Wrap raw ID3v2 tag bytes + PCM audio into a minimal RIFF/WAVE file carrying an
/// `id3 ` chunk, so `wav::read_binary_tags` has a real chunk to extract.
fn wav_with_id3(id3: &[u8], audio: &[u8]) -> Vec<u8> {
    fn chunk(id: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut c = id.to_vec();
        c.extend_from_slice(&(body.len() as u32).to_le_bytes());
        c.extend_from_slice(body);
        if body.len() % 2 == 1 {
            c.push(0); // RIFF word-alignment pad
        }
        c
    }
    let mut body = Vec::new();
    body.extend_from_slice(b"WAVE");
    body.extend(chunk(b"fmt ", &fmt_pcm_16bit_mono()));
    body.extend(chunk(b"id3 ", id3));
    body.extend(chunk(b"data", audio));
    let mut out = Vec::new();
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(body.len() as u32).to_le_bytes());
    out.extend_from_slice(&body);
    out
}

/// Flatten a layout: inline verbatim, BinaryTag from `map`, BackingAudio from `audio`.
fn materialize_wav(layout: &RegionLayout, audio: &[u8], map: &std::collections::HashMap<i64, Vec<u8>>) -> Vec<u8> {
    let mut out = Vec::new();
    for seg in layout.segments() {
        match seg {
            Segment::Inline(b) => out.extend_from_slice(b),
            Segment::BinaryTag { payload_id, .. } => out.extend_from_slice(map.get(payload_id).unwrap()),
            Segment::BackingAudio { offset, len } => {
                let s = *offset as usize;
                out.extend_from_slice(&audio[s..s + *len as usize]);
            }
            Segment::ArtImage { .. } => panic!("no art in this fixture"),
            other => panic!("unexpected segment in WAV layout: {other:?}"),
        }
    }
    out
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    #[test]
    fn wav_binary_tags_round_trip_survives_byte_identically(
        priv_payload in proptest::collection::vec(any::<u8>(), 1..100),
        geob_payload in proptest::collection::vec(any::<u8>(), 1..100),
        popm_rating in proptest::option::of(0u8..=255),
        playcount in 0u64..10_000,
        has_mb_ufid in proptest::bool::ANY,
        samples in proptest::collection::vec(any::<i16>(), 1..64),
    ) {
        use id3::frame::{Content, Popularimeter, UniqueFileIdentifier, Unknown};
        use id3::{Encoder, Frame, Tag, Version};
        use std::collections::HashMap;

        let audio: Vec<u8> = samples.iter().flat_map(|s| s.to_le_bytes()).collect();

        // Build an ID3v2.4 tag with opaque + promotable frames.
        let mut tag = Tag::new();
        tag.add_frame(Frame::with_content(
            "PRIV",
            Content::Unknown(Unknown { data: priv_payload.clone(), version: Version::Id3v24 }),
        ));
        let geob_body: Vec<u8> = std::iter::once(0x00).chain(geob_payload.iter().copied()).collect();
        tag.add_frame(Frame::with_content(
            "GEOB",
            Content::Unknown(Unknown { data: geob_body, version: Version::Id3v24 }),
        ));
        if let Some(rating) = popm_rating {
            tag.add_frame(Popularimeter { user: "user@example".into(), rating, counter: playcount });
        }
        if has_mb_ufid {
            tag.add_frame(UniqueFileIdentifier {
                owner_identifier: "http://musicbrainz.org".into(),
                identifier: b"test-mbid-value".to_vec(),
            });
        }
        tag.add_frame(UniqueFileIdentifier {
            owner_identifier: "http://other.example".into(),
            identifier: b"other-id-data".to_vec(),
        });
        let mut id3_bytes = Vec::new();
        Encoder::new().version(Version::Id3v24).encode(&tag, &mut id3_bytes).unwrap();

        // Source WAV → first parse.
        let source = wav_with_id3(&id3_bytes, &audio);
        let (opaque, promoted) = wav::read_binary_tags(&source);
        prop_assert!(opaque.iter().any(|e| e.key == "PRIV"), "PRIV must be opaque");
        prop_assert!(opaque.iter().any(|e| e.key == "GEOB"), "GEOB must be opaque");
        prop_assert!(opaque.iter().any(|e| e.key == "UFID"), "non-MB UFID must be opaque");

        // DB round-trip (synthetic rowids via the in-memory DB).
        let db = musefs_db::Db::open_in_memory().unwrap();
        let tid = db.upsert_track(&musefs_db::NewTrack {
            backing_path: "/a.wav".into(),
            format: musefs_db::Format::Wav,
            audio_offset: 0, audio_length: 0, backing_size: 0, backing_mtime: 0,
        }).unwrap();
        let rows: Vec<musefs_db::BinaryTag> = opaque.iter().enumerate().map(|(i, e)| {
            musefs_db::BinaryTag { key: e.key.clone(), payload: e.payload.clone(), ordinal: i as i64 }
        }).collect();
        db.set_binary_tags(tid, &rows).unwrap();
        let stored = db.get_binary_tags(tid).unwrap();
        let inputs: Vec<BinaryTagInput> = stored.iter().map(|r| {
            BinaryTagInput { key: r.key.clone(), payload_id: r.rowid, len: r.byte_len as u64 }
        }).collect();
        let mut map: HashMap<i64, Vec<u8>> = HashMap::new();
        for r in &stored {
            map.insert(r.rowid, db.read_binary_tag_chunk(r.rowid, 0, r.byte_len as usize).unwrap());
        }

        // Promoted text tags drive POPM/UFID regeneration.
        let text: Vec<TagInput> = promoted.iter().map(|(k, v)| TagInput::new(k, v)).collect();

        // Synthesize a fresh WAV and re-parse.
        let scan = wav::WavScan { fmt: fmt_pcm_16bit_mono(), fact: None };
        let layout = wav::synthesize_layout(&scan, 0, audio.len() as u64, &text, &inputs, &[]).unwrap();
        let served = materialize_wav(&layout, &audio, &map);
        let (opaque2, promoted2) = wav::read_binary_tags(&served);

        // Opaque payloads survive byte-identically.
        prop_assert_eq!(opaque.len(), opaque2.len(), "opaque count mismatch");
        for orig in &opaque {
            prop_assert!(
                opaque2.iter().any(|o| o.key == orig.key && o.payload == orig.payload),
                "opaque frame {:?} lost in round-trip", orig.key
            );
        }
        // Promoted values survive (semantic, not byte-identical).
        if let Some(rating) = popm_rating {
            prop_assert!(promoted2.iter().any(|(k, v)| k == "rating" && v == &rating.to_string()), "rating lost");
        }
        if has_mb_ufid {
            prop_assert!(promoted2.iter().any(|(k, _)| k == "musicbrainz_trackid"), "mbid lost");
        }
    }
}
```

- [ ] **Step 2: Run the test**

Run: `cargo test -p musefs-format --features fuzzing wav_binary_tags_round_trip`
Expected: PASS (128 cases). This characterizes shipped WAV behavior; a failure is a real WAV binary-tag bug — stop and report it.

- [ ] **Step 3: Fmt + commit**

```bash
cargo fmt --all
git add musefs-format/tests/proptest_wav.rs
git commit -m "test(wav): binary-tag round-trip proptest (Binary Tags Phase 5)"
```

---

## Task 2: FLAC APPLICATION/CUESHEET payload round-trip (re-scan)

Spec §Testing: "a FLAC round-trip test locks the migrated `APPLICATION`/`CUESHEET` behavior and the structural store" and "for FLAC, assert payload/round-trip fidelity **only** — not byte-identical block ordering." The existing `rescanned_flac_serves_valid_file_with_binary_tags` checks one fixed `APPLICATION` via metaflac (and avoids `CUESHEET`, whose dummy body metaflac can't parse). This task adds an arbitrary-payload proptest that round-trips **both** `APPLICATION` and `CUESHEET` through the full scan → synthesize → **re-scan** loop, comparing `get_binary_tags` payloads — sidestepping metaflac's CUESHEET strictness while proving payload fidelity end to end.

**Files:**
- Modify: `musefs-core/tests/flac_binary_tags.rs`

- [ ] **Step 1: Add the round-trip proptest**

`musefs-core` already uses proptest as a dev-dependency (see `tests/proptest_read_fidelity.rs`). Append to `musefs-core/tests/flac_binary_tags.rs`:

```rust
use proptest::prelude::*;

/// Serve a scanned track's whole file via the synthesis read path.
fn serve_whole(db: &musefs_db::Db, id: i64) -> Vec<u8> {
    use musefs_core::{read_at, HeaderCache, Mode};
    let resolved = HeaderCache::new(Mode::Synthesis).resolve(db, id).unwrap();
    read_at(&resolved, db, 0, resolved.total_len).unwrap()
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn flac_application_cuesheet_payloads_round_trip_via_rescan(
        app_payload in proptest::collection::vec(any::<u8>(), 1..200),
        cue_payload in proptest::collection::vec(any::<u8>(), 1..200),
        audio in proptest::collection::vec(any::<u8>(), 1..512),
    ) {
        // Block types: STREAMINFO=0, APPLICATION=2, SEEKTABLE=3, VORBIS_COMMENT=4, CUESHEET=5.
        let blocks = vec![
            (0u8, streaminfo_body()),
            (2u8, app_payload.clone()),
            (3u8, vec![0xEE; 36]),
            (4u8, vorbis_comment_body("v", &["ARTIST=Alice", "TITLE=Song"])),
            (5u8, cue_payload.clone()),
        ];
        let bytes = make_flac(&blocks, &audio);

        // Scan #1.
        let dir1 = tempfile::tempdir().unwrap();
        std::fs::write(dir1.path().join("a.flac"), &bytes).unwrap();
        let db1 = musefs_db::Db::open_in_memory().unwrap();
        scan_directory(&db1, dir1.path()).unwrap();
        let id1 = db1.list_tracks().unwrap()[0].id;

        // Synthesize the served FLAC, then scan #2 over the synthesized output.
        let served = serve_whole(&db1, id1);
        let dir2 = tempfile::tempdir().unwrap();
        std::fs::write(dir2.path().join("b.flac"), &served).unwrap();
        let db2 = musefs_db::Db::open_in_memory().unwrap();
        scan_directory(&db2, dir2.path()).unwrap();
        let id2 = db2.list_tracks().unwrap()[0].id;

        // Binary payloads survive byte-identically across the round trip.
        let bin = db2.get_binary_tags(id2).unwrap();
        let app = bin.iter().find(|b| b.key == "APPLICATION").expect("APPLICATION survives");
        prop_assert_eq!(&app.payload, &app_payload, "APPLICATION payload changed");
        let cue = bin.iter().find(|b| b.key == "CUESHEET").expect("CUESHEET survives");
        prop_assert_eq!(&cue.payload, &cue_payload, "CUESHEET payload changed");

        // Structural store repopulated (canonical reorder is fine; only presence matters).
        let structural = db2.get_structural_blocks(id2).unwrap();
        let kinds: Vec<&str> = structural.iter().map(|b| b.kind.as_str()).collect();
        prop_assert!(kinds.contains(&"STREAMINFO"), "STREAMINFO missing after round-trip");
        prop_assert!(kinds.contains(&"SEEKTABLE"), "SEEKTABLE missing after round-trip");
    }
}
```

- [ ] **Step 2: Run the test**

Run: `cargo test -p musefs-core --test flac_binary_tags flac_application_cuesheet_payloads_round_trip_via_rescan`
Expected: PASS (64 cases).

- [ ] **Step 3: Fmt + commit**

```bash
cargo fmt --all
git add musefs-core/tests/flac_binary_tags.rs
git commit -m "test(flac): APPLICATION/CUESHEET payload round-trip via re-scan (Binary Tags Phase 5)"
```

---

## Task 3: Byte-identical-audio invariant with binary frames (core)

Spec §Testing: "Byte-identical invariant proptest extended so fixtures carry binary frames, confirming audio bytes remain untouched with binary tags present." The existing `read_at_preserves_backing_audio_mp3` builds with text tags only. This task adds an MP3 variant that sets opaque binary tags + promoted text tags in the DB before resolving, then asserts the served audio (everything after the synthesized header) is byte-for-byte the original — proving `Segment::BinaryTag` emission never disturbs the `BackingAudio` run.

**Files:**
- Modify: `musefs-core/tests/proptest_read_fidelity.rs`

- [ ] **Step 1: Add the test inside the existing MP3 `proptest! { ... }` block**

Add this test next to `read_at_preserves_backing_audio_mp3` (same `proptest!` block, ~`musefs-core/tests/proptest_read_fidelity.rs:433`). It reuses the file's existing `build_mp3` helper, then layers binary tags on:

```rust
    #[test]
    fn read_at_preserves_backing_audio_mp3_with_binary_frames(
        audio in proptest::collection::vec(any::<u8>(), 1..512),
        priv_payload in proptest::collection::vec(any::<u8>(), 1..120),
        rating in 0u8..=255,
    ) {
        let (_dir, db, id, original) = build_mp3(&audio, "Bin Title");
        // Opaque PRIV (frame body = owner\0 + data) + a promoted rating text tag.
        db.set_binary_tags(
            id,
            &[musefs_db::BinaryTag {
                key: "PRIV".into(),
                payload: {
                    let mut p = b"musefs\0".to_vec();
                    p.extend_from_slice(&priv_payload);
                    p
                },
                ordinal: 0,
            }],
        )
        .unwrap();
        db.replace_tags(
            id,
            &[Tag::new("title", "Bin Title", 0), Tag::new("rating", &rating.to_string(), 0)],
        )
        .unwrap();

        let resolved = HeaderCache::new(Mode::Synthesis).resolve(&db, id).unwrap();
        // The layout must actually carry a streamed binary tag (else the test is vacuous).
        prop_assert!(
            resolved.layout.segments.iter().any(|s| matches!(s, Segment::BinaryTag { .. })),
            "resolve did not emit a BinaryTag segment"
        );
        let whole = read_at(&resolved, &db, 0, resolved.total_len).unwrap();
        prop_assert_eq!(whole.len() as u64, resolved.total_len);
        // Audio is the suffix after the synthesized header; binary frames live in the header.
        let served_audio = &whole[resolved.layout.header_len() as usize..];
        prop_assert_eq!(served_audio, &original[..]);
    }
```

If `Segment` is not already imported at the top of the file, add it to the existing `use musefs_format::...` line (the art-window tests already reference `Segment`, so it is in scope — confirm before adding).

- [ ] **Step 2: Run the test**

Run: `cargo test -p musefs-core --test proptest_read_fidelity read_at_preserves_backing_audio_mp3_with_binary_frames`
Expected: PASS (64 cases per the block's `ProptestConfig`).

- [ ] **Step 3: Fmt + commit**

```bash
cargo fmt --all
git add musefs-core/tests/proptest_read_fidelity.rs
git commit -m "test(core): byte-identical audio invariant with binary frames present (Binary Tags Phase 5)"
```

---

## Task 4: Query-split correctness at the tree level

Spec §Testing: "a track with a binary frame (e.g. `PRIV`) renders the **same** tree path as one without — binary rows must not leak into `tags_to_fields`." The DB-layer filter is unit-tested (`tags.rs::text_queries_exclude_binary_rows`), but the **tree path** has no test. A regression in the `value_blob IS NULL` filter on `tags_grouped` would feed empty-string `PRIV`/`GEOB` values into the template renderer; this test catches that at the rendered-path level.

**Files:**
- Create: `musefs-core/tests/binary_tag_tree.rs`

- [ ] **Step 1: Create the test file**

```rust
mod common;
use common::{make_flac, streaminfo_body, vorbis_comment_body};
use musefs_core::{scan_directory, MountConfig, Musefs, VirtualTree};
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

/// A FLAC with the given vorbis comments and an APPLICATION block (→ a binary row).
/// Block types: STREAMINFO=0, APPLICATION=2, VORBIS_COMMENT=4.
fn flac_with_binary(comments: &[&str]) -> Vec<u8> {
    make_flac(
        &[
            (0u8, streaminfo_body()),
            (2u8, b"testPRIVATE-ANALYSIS".to_vec()),
            (4u8, vorbis_comment_body("v", comments)),
        ],
        &vec![0xCD; 4096],
    )
}

/// Same comments, no binary block.
fn flac_without_binary(comments: &[&str]) -> Vec<u8> {
    make_flac(
        &[
            (0u8, streaminfo_body()),
            (4u8, vorbis_comment_body("v", comments)),
        ],
        &vec![0xCD; 4096],
    )
}

/// Collect every rendered file path under ROOT as "dir/file".
fn rendered_paths(fs: &Musefs) -> Vec<String> {
    let mut out = Vec::new();
    for (dirname, dinode, _) in fs.readdir(VirtualTree::ROOT).unwrap() {
        if dirname == "." || dirname == ".." {
            continue;
        }
        for (fname, _finode, _) in fs.readdir(dinode).unwrap() {
            if fname == "." || fname == ".." {
                continue;
            }
            out.push(format!("{dirname}/{fname}"));
        }
    }
    out.sort();
    out
}

#[test]
fn binary_row_does_not_alter_rendered_tree_path() {
    let comments = ["ARTIST=Alice", "TITLE=Song"];

    // Track WITH a binary (APPLICATION) row.
    let dir_a = tempfile::tempdir().unwrap();
    std::fs::write(dir_a.path().join("a.flac"), flac_with_binary(&comments)).unwrap();
    let db_a = musefs_db::Db::open_in_memory().unwrap();
    scan_directory(&db_a, dir_a.path()).unwrap();
    let fs_a = Musefs::open(db_a, config()).unwrap();

    // Track WITHOUT a binary row.
    let dir_b = tempfile::tempdir().unwrap();
    std::fs::write(dir_b.path().join("b.flac"), flac_without_binary(&comments)).unwrap();
    let db_b = musefs_db::Db::open_in_memory().unwrap();
    scan_directory(&db_b, dir_b.path()).unwrap();
    let fs_b = Musefs::open(db_b, config()).unwrap();

    // Identical rendered path — the binary row never leaked into tags_to_fields.
    assert_eq!(rendered_paths(&fs_a), vec!["Alice/Song.flac".to_string()]);
    assert_eq!(rendered_paths(&fs_a), rendered_paths(&fs_b));
}
```

The store-presence of the binary row is already covered by `flac_binary_tags.rs::scan_splits_into_structural_store_and_binary_tags`; this test's sole job is the rendered-path equivalence.

- [ ] **Step 2: Run the test**

Run: `cargo test -p musefs-core --test binary_tag_tree binary_row_does_not_alter_rendered_tree_path`
Expected: PASS. The shared `mod common` is the same `musefs-core/tests/common` used by `flac_binary_tags.rs`/`metrics.rs`, so `make_flac`/`streaminfo_body`/`vorbis_comment_body` resolve. If `MountConfig`/`Musefs`/`VirtualTree` field or method names drift, mirror `flac_binary_tags.rs::config`/`read_whole` (the canonical reference in this crate).

- [ ] **Step 3: Fmt + commit**

```bash
cargo fmt --all
git add musefs-core/tests/binary_tag_tree.rs
git commit -m "test(core): binary rows do not leak into rendered tree paths (Binary Tags Phase 5)"
```

---

## Task 5: Legacy-FLAC backfill after `revalidate` (production fix + tests)

Spec §1 (lines 126-131) and §Testing (lines 430-431) mandate: "the next `scan`/`scan --revalidate` backfills `structural_blocks` (and migrates `APPLICATION`/`CUESHEET` into `tags.value_blob`) for any FLAC track lacking them … after a `scan --revalidate`, `structural_blocks`/`value_blob` are backfilled and the front read no longer fires."

**Phase 4 shipped the serve-path *fallback* (`flac_binary_tags.rs::legacy_flac_without_structural_rows_serves_via_front_read_fallback`) but NOT the `revalidate` *backfill*.** Verified: `revalidate_with` (`musefs-core/src/scan.rs:788-805`) skips any file whose `(size, mtime)` matches the stored row, with no branch for FLAC tracks lacking structural blocks — so a legacy FLAC track with an unchanged backing file is never re-scanned and never backfilled. This task closes that gap (a genuine fail-first), then locks it with two tests.

**Approach for the fix:** in the skip pass, load the set of track ids that already have structural blocks (one bulk query). Force a re-scan for any unchanged FLAC track NOT in that set; `run_pipeline`'s existing ingest path then populates `structural_blocks` + binary tags exactly as a fresh scan does.

**Approach for the tests:** rather than hand-build a legacy row (fragile: the manual `backing_path` must exactly match `revalidate`'s canonicalized key, or a duplicate track is created), do a real `scan_directory` then strip the V2 rows (`set_structural_blocks(id, &[])` + `set_binary_tags(id, &[])`, both of which delete-all on an empty slice). This yields a genuine V1-shaped state with the exact `backing_path` scan uses.

**Files:**
- Modify: `musefs-db/src/structural.rs` (new query)
- Modify: `musefs-core/src/scan.rs` (`revalidate_with`)
- Modify: `musefs-core/tests/flac_binary_tags.rs`
- Modify: `musefs-core/tests/metrics.rs`

- [ ] **Step 1: Write the failing backfill test**

Append to `musefs-core/tests/flac_binary_tags.rs`. `revalidate` and `scan_directory` are public re-exports (`musefs-core/src/scan.rs:834`); `scan_directory` is already imported at the top of this file.

```rust
use musefs_core::revalidate;

#[test]
fn revalidate_backfills_structural_and_binary_rows_for_legacy_flac() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.flac"), serve_fixture()).unwrap();
    let db = musefs_db::Db::open_in_memory().unwrap();

    // Real scan, then strip the V2 rows to simulate a V1-scanned (legacy) track.
    scan_directory(&db, dir.path()).unwrap();
    let id = db.list_tracks().unwrap()[0].id;
    db.set_structural_blocks(id, &[]).unwrap();
    db.set_binary_tags(id, &[]).unwrap();
    assert!(db.get_structural_blocks(id).unwrap().is_empty());
    assert!(db.get_binary_tags(id).unwrap().is_empty());

    // Maintenance pass must backfill both stores even though the file is unchanged.
    revalidate(&db, dir.path()).unwrap();

    let kinds: Vec<String> = db
        .get_structural_blocks(id)
        .unwrap()
        .into_iter()
        .map(|b| b.kind)
        .collect();
    assert!(kinds.iter().any(|k| k == "STREAMINFO"), "STREAMINFO backfilled");
    assert!(kinds.iter().any(|k| k == "SEEKTABLE"), "SEEKTABLE backfilled");
    assert!(
        db.get_binary_tags(id).unwrap().iter().any(|b| b.key == "APPLICATION"),
        "APPLICATION backfilled"
    );
}
```

- [ ] **Step 2: Run it — verify it FAILS for the right reason**

Run: `cargo test -p musefs-core --test flac_binary_tags revalidate_backfills_structural_and_binary_rows_for_legacy_flac`
Expected: FAIL — `revalidate` skips the unchanged file, so `get_structural_blocks` is still empty and the `STREAMINFO backfilled` assertion fires. (This confirms the Phase 4 gap before fixing it.)

- [ ] **Step 3: Add the `track_ids_with_structural_blocks` query**

In `musefs-db/src/structural.rs`, insert into `impl Db` after `get_structural_blocks` (after line `42`):

```rust
    /// Track ids that have at least one structural block row. Used by `revalidate`
    /// to detect legacy FLAC tracks (scanned under V1) that still need a backfill.
    pub fn track_ids_with_structural_blocks(&self) -> Result<std::collections::HashSet<i64>> {
        let mut stmt = self
            .conn
            .prepare("SELECT DISTINCT track_id FROM structural_blocks")?;
        let rows = stmt.query_map([], |r| r.get::<_, i64>(0))?;
        Ok(rows.collect::<rusqlite::Result<std::collections::HashSet<i64>>>()?)
    }
```

- [ ] **Step 4: Teach `revalidate_with` to re-scan legacy FLAC tracks**

In `musefs-core/src/scan.rs`, replace the `existing` map build (`scan.rs:779-783`) so it also carries the track id + format, and load the structural-block set just after it:

```rust
    let existing: HashMap<String, (i64, i64, i64, Format)> = db
        .list_tracks()?
        .into_iter()
        .map(|t| (t.backing_path, (t.backing_size, t.backing_mtime, t.id, t.format)))
        .collect();
    // Legacy backfill (spec §1): FLAC tracks scanned under V1 have no structural
    // blocks. Re-scan them even when the backing file is unchanged so the V2
    // structural store + binary tags get populated by the ingest path.
    let have_structural = db.track_ids_with_structural_blocks()?;
```

Then replace the skip check inside the loop (`scan.rs:798-803`) with:

```rust
        if let Some((size, mtime, id, format)) = existing.get(&key) {
            let needs_backfill = *format == Format::Flac && !have_structural.contains(id);
            if *size == meta.len() as i64 && *mtime == mtime_secs(&meta) && !needs_backfill {
                unchanged += 1;
                continue;
            }
        }
        changed.push(path);
```

`Format` is already in scope in `scan.rs` (the ingest path matches on `track.format`); if the compiler disagrees, add it to the existing `use musefs_db::{...}` line. Binding the tuple by reference (`(size, mtime, id, format)` not `&(...)`) avoids requiring `Format: Copy`.

- [ ] **Step 5: Run the backfill test — now PASS**

Run: `cargo test -p musefs-core --test flac_binary_tags revalidate_backfills_structural_and_binary_rows_for_legacy_flac`
Expected: PASS. Also run the DB crate to confirm the new query compiles and existing tests are green: `cargo test -p musefs-db`.

- [ ] **Step 6: Front-read-elimination-after-backfill test in `metrics.rs`**

Append to `musefs-core/tests/metrics.rs`. Same scan-then-strip setup, then assert resolve opens only the handle fd (no front re-read) after backfill. Reuses the file's `METRICS_LOCK` and `config`.

```rust
#[test]
fn revalidated_legacy_flac_resolve_does_no_front_read() {
    use musefs_core::{revalidate, scan_directory};
    let _guard = METRICS_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);

    let dir = tempfile::tempdir().unwrap();
    let flac = make_flac(
        &[
            (0, streaminfo_body()),
            (2, b"testAPPDATA".to_vec()),
            (4, vorbis_comment_body("v", &["ARTIST=Alice", "TITLE=Song"])),
        ],
        &vec![0xCD_u8; 64 * 1024],
    );
    std::fs::write(dir.path().join("a.flac"), &flac).unwrap();
    let db = musefs_db::Db::open_in_memory().unwrap();

    // Scan, then strip to a legacy (V1) state, then backfill via revalidate.
    scan_directory(&db, dir.path()).unwrap();
    let id = db.list_tracks().unwrap()[0].id;
    db.set_structural_blocks(id, &[]).unwrap();
    db.set_binary_tags(id, &[]).unwrap();
    revalidate(&db, dir.path()).unwrap();

    let fs = Musefs::open(db, config()).unwrap();
    let artist = fs.lookup(VirtualTree::ROOT, "Alice").unwrap();
    let (_, inode, _) = fs.readdir(artist).unwrap().into_iter().next().unwrap();

    metrics::reset();
    let fh = fs.open_handle(inode).unwrap();
    let s = metrics::snapshot();
    fs.release_handle(fh);
    assert_eq!(
        s.opens, 1,
        "after revalidate-backfill, FLAC resolve must not re-read the backing front"
    );
}
```

- [ ] **Step 7: Run the metrics test**

Run: `cargo test -p musefs-core --features metrics --test metrics revalidated_legacy_flac_resolve_does_no_front_read`
Expected: PASS (`opens == 1`).

- [ ] **Step 8: Fmt + commit (production fix + tests together)**

```bash
cargo fmt --all
git add musefs-db/src/structural.rs musefs-core/src/scan.rs \
    musefs-core/tests/flac_binary_tags.rs musefs-core/tests/metrics.rs
git commit -m "feat(scan): revalidate backfills legacy-FLAC structural+binary rows (Binary Tags Phase 5)"
```

---

## Task 6: V1→V2 migration data preservation

Spec §Testing: "V1→V2 upgrade is idempotent and preserves existing rows; `value_blob` defaults NULL on existing tags." The existing `v2_adds_value_blob_and_structural_blocks_and_is_idempotent` runs the full `migrate` (straight to V2) and inserts a *new* row. This task applies **only V1**, inserts a row under the V1 schema, then upgrades to V2 and asserts the pre-existing row survived with `value_blob` NULL.

**Files:**
- Modify: `musefs-db/src/schema.rs` (the `migration_v2_tests` module)

- [ ] **Step 1: Add the preservation test**

Add inside `mod migration_v2_tests` (`musefs-db/src/schema.rs:117`), after the existing test. It applies `MIGRATIONS[0]` (V1) manually, stamps `user_version = 1`, writes a row, then calls `super::migrate` to reach V2. `MIGRATIONS` is module-private; reference it as `super::MIGRATIONS`.

```rust
    #[test]
    fn v1_rows_survive_v2_migration_with_null_value_blob() {
        let mut conn = Connection::open_in_memory().unwrap();
        // Apply ONLY V1, then stamp the version so migrate() resumes at V2.
        conn.execute_batch(super::MIGRATIONS[0]).unwrap();
        conn.pragma_update(None, "user_version", 1i64).unwrap();

        // Insert under the V1 schema (no value_blob column exists yet).
        conn.execute(
            "INSERT INTO tracks (backing_path, format, audio_offset, audio_length, \
             backing_size, backing_mtime, updated_at) \
             VALUES ('/legacy.flac','flac',10,20,30,0,0)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO tags (track_id, key, value, ordinal) VALUES (1,'artist','Legacy',0)",
            [],
        )
        .unwrap();

        // Upgrade V1 -> V2.
        super::migrate(&mut conn).unwrap();
        let uv: i64 = conn
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap();
        assert_eq!(uv, 2);

        // The pre-existing row survived unchanged, with value_blob defaulted NULL.
        let (value, blob_is_null): (String, bool) = conn
            .query_row(
                "SELECT value, value_blob IS NULL FROM tags WHERE track_id=1 AND key='artist'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(value, "Legacy");
        assert!(blob_is_null, "existing tag rows must default value_blob to NULL");

        // The track row survived too.
        let offset: i64 = conn
            .query_row("SELECT audio_offset FROM tracks WHERE id=1", [], |r| r.get(0))
            .unwrap();
        assert_eq!(offset, 10);
    }
```

If `MIGRATIONS` is declared `const` without `pub(crate)`, it is still reachable from a child `#[cfg(test)] mod` via `super::MIGRATIONS` (same crate). If the compiler reports it private to a narrower scope, check its declaration in `musefs-db/src/schema.rs` and reference it by its actual path.

- [ ] **Step 2: Run the test**

Run: `cargo test -p musefs-db v1_rows_survive_v2_migration_with_null_value_blob`
Expected: PASS.

- [ ] **Step 3: Fmt + commit**

```bash
cargo fmt --all
git add musefs-db/src/schema.rs
git commit -m "test(db): V1->V2 migration preserves existing rows with NULL value_blob (Binary Tags Phase 5)"
```

---

## Task 7: MP3 fuzz target drives binary tags

Spec §Testing: "Fuzz targets `mp3`/`mp4` seeded with binary frames." `fuzz/fuzz_targets/mp4.rs` already calls `arb_binary_tags` and has a `seed_binary`; `mp3.rs` does not. `arb_binary_tags` already exists in `fuzz/src/lib.rs` (it builds `----`-keyed inputs, but synthesis only consumes `len` for size bounding, so the key namespace is irrelevant to the MP3 size-guard path). This task wires it into the MP3 target and adds an MP3 seed with enough entropy to make `arb_binary_tags` produce non-empty input.

**Files:**
- Modify: `fuzz/fuzz_targets/mp3.rs`
- Modify: `fuzz/src/bin/generate_seeds.rs`

- [ ] **Step 1: Drive `arb_binary_tags` in the MP3 target**

Replace the body of `fuzz/fuzz_targets/mp3.rs` with (import line gains `arb_binary_tags`; the synthesis call's 4th argument changes from `&[]` to `&binary`):

```rust
#![no_main]
use arbitrary::Unstructured;
use libfuzzer_sys::fuzz_target;
use musefs_format::{fuzz_check::assert_backing_covers_audio, mp3};
use musefs_fuzz::{arb_arts, arb_binary_tags, arb_tags, MAX_INPUT};

fuzz_target!(|data: &[u8]| {
    if data.len() > MAX_INPUT {
        return;
    }
    let _ = mp3::read_tags(data);
    let _ = mp3::read_pictures(data);
    let _ = mp3::read_binary_tags(data);
    let bounds = match mp3::locate_audio(data) {
        Ok(b) => b,
        Err(_) => return,
    };
    let mut u = Unstructured::new(data);
    let tags = arb_tags(&mut u).unwrap_or_default();
    let binary = arb_binary_tags(&mut u).unwrap_or_default();
    let arts = arb_arts(&mut u).unwrap_or_default();
    if let Ok(layout) =
        mp3::synthesize_layout(bounds.audio_offset, bounds.audio_length, &tags, &binary, &arts)
    {
        assert_backing_covers_audio(bounds.audio_offset, bounds.audio_length, &layout);
    }
});
```

- [ ] **Step 2: Add an MP3 `seed_binary`**

`musefs_format::fuzz_check::fixtures` exposes exactly one MP3 builder — `fixtures::mp3()` (no parameters; `musefs-format/src/fuzz_check.rs:202`) — so there is no "longer valid MP3" option, and appending trailing bytes would corrupt `locate_audio` (the exact trap the mp4 `seed_binary` comment documents). The seed must stay a parseable MP3, so reuse the same valid fixture under a `seed_binary` label: it gives the corpus a labeled entry the coverage-guided fuzzer mutates toward the binary-frame synthesis path. The real coverage win is the target wiring in Step 1; the labeled seed is a low-cost nudge, not the mechanism.

In `fuzz/src/bin/generate_seeds.rs`, add the second MP3 seed immediately after the existing `write("mp3", "seed0", &fixtures::mp3());`:

```rust
    write("mp3", "seed0", &fixtures::mp3());
    // A second, identically-valid MP3 seed labeled for the binary-tag synthesis
    // path. fixtures::mp3() is the only MP3 builder (no parameterized/longer
    // variant exists), and a corrupt seed would make locate_audio reject the
    // file and skip synthesize_layout entirely — so reuse the valid fixture.
    // The fuzzer reaches non-empty arb_binary_tags via mutation from here.
    write("mp3", "seed_binary", &fixtures::mp3());
```

- [ ] **Step 3: Verify the fuzz target builds and regenerate seeds**

The `fuzz/` crate is out-of-workspace, so `cargo test` never compiles it. Build the target explicitly and regenerate the corpus:

Run: `cargo +nightly fuzz build mp3`
Expected: builds clean (no signature mismatch on `synthesize_layout`).

Run: `cargo run --manifest-path fuzz/Cargo.toml --bin generate_seeds`
Expected: prints "seeds written under fuzz/corpus/"; `fuzz/corpus/mp3/seed_binary` now exists.

Optionally smoke-run briefly: `cargo +nightly fuzz run mp3 -- -runs=10000` (expect no crash).

- [ ] **Step 4: Commit**

```bash
git add fuzz/fuzz_targets/mp3.rs fuzz/src/bin/generate_seeds.rs fuzz/corpus/mp3/seed_binary
git commit -m "test(fuzz): MP3 target drives arb_binary_tags + binary seed (Binary Tags Phase 5)"
```

---

## Task 8: Interop (mutagen) — binary frames survive the mount

Spec §Testing: "fixtures gain `POPM`/`UFID`/`PRIV`/`GEOB` (ID3) and a `----` atom (MP4), and the independent mutagen reader asserts they survive the mount (semantic fields as readable tags, opaque frames byte-for-byte)." This is the real-world proof. The emitter writes the DB out-of-band (exactly how a media manager would), synthesizes via the read path, and writes the served files + a `binary_manifest.json`; the Python test re-opens them with mutagen and asserts.

All payloads are ASCII so the Python side can compare without hex decoding. Opaque frame bodies are constructed to be valid ID3 frames mutagen can parse:
- `PRIV` body = `owner` + `\0` + `data` → mutagen exposes `.owner`, `.data`.
- `GEOB` body = `\x00` (latin-1) + `mime\0` + `filename\0` + `description\0` + `data` → mutagen exposes `.data`.
- `POPM`/`UFID` are regenerated from promoted text tags (`rating`/`playcount`/`musicbrainz_trackid`).
- MP4 `----:com.apple.iTunes:<name>` → mutagen exposes a list of freeform byte values.

**Files:**
- Modify: `musefs-core/tests/interop_emit.rs`
- Modify: `tests/interop/test_mutagen_roundtrip.py`

- [ ] **Step 1: Add a binary emitter + `binary_manifest.json` to `interop_emit.rs`**

Add this helper and extend `emit_interop_fixtures` to emit `out_bin.mp3`, `out_bin.m4a`, and `binary_manifest.json`. Insert the helper near `emit` (it shares its structure but also writes text + binary tags), and append the two fixture blocks at the end of `emit_interop_fixtures` before the `manifest.json` write (the binary manifest is a separate file, so ordering vs the existing manifest does not matter).

```rust
use musefs_db::BinaryTag;

/// Like `emit`, but also writes promoted text tags and opaque binary tags to the
/// DB before synthesis — mirroring how a media manager populates the store.
#[allow(clippy::too_many_arguments)]
fn emit_binary(
    src: &Path,
    dst: &Path,
    bytes: &[u8],
    format: Format,
    audio_offset: i64,
    audio_length: i64,
    text: &[Tag],
    binary: &[BinaryTag],
) {
    std::fs::write(src, bytes).unwrap();
    let db = Db::open_in_memory().unwrap();
    let id = db
        .upsert_track(&NewTrack {
            backing_path: src.to_string_lossy().to_string(),
            format,
            audio_offset,
            audio_length,
            backing_size: std::fs::metadata(src).unwrap().len() as i64,
            backing_mtime: real_mtime(src),
        })
        .unwrap();
    db.replace_tags(id, text).unwrap();
    db.set_binary_tags(id, binary).unwrap();
    let resolved = HeaderCache::new(Mode::Synthesis).resolve(&db, id).unwrap();
    let out = read_at(&resolved, &db, 0, resolved.total_len).unwrap();
    std::fs::write(dst, &out).unwrap();
}
```

Then, inside `emit_interop_fixtures`, after the existing WAV block and before the `manifest.json` serialization, add:

```rust
    // ── Binary-frame fixtures (spec §Testing: POPM/UFID/PRIV/GEOB + MP4 ----) ──
    // Known ASCII payloads so the Python side compares without hex.
    let priv_owner = "musefs";
    let priv_data = "PRIV-ANALYSIS-001";
    let geob_data = "GEOB-OBJECT-XYZ";
    let mb_trackid = "11111111-2222-3333-4444-555555555555";
    let rating = "200";
    let playcount = "42";
    let freeform_name = "MUSEFSTEST";
    let freeform_data = "FREEFORM-DATA-001";

    // MP3: PRIV + GEOB opaque; POPM/UFID via promoted text tags.
    {
        let bytes = fixtures::mp3();
        let b = musefs_format::mp3::locate_audio(&bytes).unwrap();
        let mut priv_body = priv_owner.as_bytes().to_vec();
        priv_body.push(0);
        priv_body.extend_from_slice(priv_data.as_bytes());
        let mut geob_body = vec![0x00u8]; // latin-1 text encoding
        geob_body.extend_from_slice(b"application/octet-stream\0");
        geob_body.push(0); // empty filename
        geob_body.push(0); // empty description
        geob_body.extend_from_slice(geob_data.as_bytes());
        emit_binary(
            &dir.join("src_bin.mp3"),
            &dir.join("out_bin.mp3"),
            &bytes,
            Format::Mp3,
            b.audio_offset as i64,
            b.audio_length as i64,
            &[
                Tag::new("title", "Bin Title", 0),
                Tag::new("artist", "Bin Artist", 0),
                Tag::new("rating", rating, 0),
                Tag::new("playcount", playcount, 0),
                Tag::new("musicbrainz_trackid", mb_trackid, 0),
            ],
            &[
                BinaryTag { key: "PRIV".into(), payload: priv_body, ordinal: 0 },
                BinaryTag { key: "GEOB".into(), payload: geob_body, ordinal: 0 },
            ],
        );
    }

    // MP4: one `----` freeform atom.
    {
        let bytes = richer_m4a(&[7u8; 64]);
        let scan = musefs_format::mp4::read_structure(&bytes).unwrap();
        emit_binary(
            &dir.join("src_bin.m4a"),
            &dir.join("out_bin.m4a"),
            &bytes,
            Format::M4a,
            scan.mdat_payload_offset as i64,
            scan.mdat_payload_len as i64,
            &[Tag::new("title", "Bin Title", 0), Tag::new("artist", "Bin Artist", 0)],
            &[BinaryTag {
                key: format!("----:com.apple.iTunes:{freeform_name}"),
                payload: freeform_data.as_bytes().to_vec(),
                ordinal: 0,
            }],
        );
    }

    // Emit the binary manifest the Python test consumes.
    let binary_manifest = format!(
        "{{\"mp3\":{{\"file\":\"out_bin.mp3\",\"priv_owner\":{priv_owner:?},\"priv_data\":{priv_data:?},\
         \"geob_data\":{geob_data:?},\"rating\":{rating},\"playcount\":{playcount},\
         \"mb_trackid\":{mb_trackid:?}}},\
         \"mp4\":{{\"file\":\"out_bin.m4a\",\"freeform_key\":\"----:com.apple.iTunes:{freeform_name}\",\
         \"freeform_data\":{freeform_data:?}}}}}",
    );
    std::fs::write(dir.join("binary_manifest.json"), binary_manifest).unwrap();
```

Note: `rating`/`playcount` are emitted unquoted (numbers) in the JSON; `priv_owner`/`priv_data`/`geob_data`/`mb_trackid`/`freeform_data` use `{:?}` which produces a quoted, escaped JSON string for these ASCII values.

- [ ] **Step 2: Emit the fixtures**

Run:
```bash
MUSEFS_INTEROP_DIR=/tmp/musefs-interop cargo test -p musefs-core --test interop_emit -- --ignored emit_interop_fixtures
```
Expected: PASS; `/tmp/musefs-interop/` now contains `out_bin.mp3`, `out_bin.m4a`, `binary_manifest.json` alongside the existing fixtures.

- [ ] **Step 3: Add the mutagen assertion to the Python suite**

Append to `tests/interop/test_mutagen_roundtrip.py`:

```python
def test_binary_frames_survive():
    """Binary tag frames written to the DB survive the mount and are readable
    by mutagen: POPM/UFID as semantic fields, PRIV/GEOB and MP4 ---- byte-for-byte."""
    base = os.environ["MUSEFS_INTEROP_DIR"]
    with open(os.path.join(base, "binary_manifest.json")) as fh:
        bm = json.load(fh)

    # ── MP3 (ID3) ──
    mp3 = bm["mp3"]
    id3 = mutagen.id3.ID3(os.path.join(base, mp3["file"]))

    priv = [f for f in id3.getall("PRIV") if f.owner == mp3["priv_owner"]]
    assert priv, "PRIV frame missing"
    assert priv[0].data == mp3["priv_data"].encode("ascii"), "PRIV data changed"

    geob = id3.getall("GEOB")
    assert geob, "GEOB frame missing"
    assert any(g.data == mp3["geob_data"].encode("ascii") for g in geob), "GEOB data changed"

    popm = id3.getall("POPM")
    assert popm, "POPM frame missing"
    assert popm[0].rating == mp3["rating"], f"rating {popm[0].rating} != {mp3['rating']}"
    assert popm[0].count == mp3["playcount"], f"playcount {popm[0].count} != {mp3['playcount']}"

    ufid = [f for f in id3.getall("UFID") if f.owner == "http://musicbrainz.org"]
    assert ufid, "MusicBrainz UFID missing"
    assert ufid[0].data == mp3["mb_trackid"].encode("ascii"), "musicbrainz_trackid changed"

    # ── MP4 (----) ──
    mp4 = bm["mp4"]
    f = mutagen.mp4.MP4(os.path.join(base, mp4["file"]))
    vals = f.tags.get(mp4["freeform_key"]) if f.tags else None
    assert vals, f"freeform atom {mp4['freeform_key']} missing"
    assert bytes(vals[0]) == mp4["freeform_data"].encode("ascii"), "---- payload changed"
```

- [ ] **Step 4: Run the Python interop suite**

Run:
```bash
MUSEFS_INTEROP_DIR=/tmp/musefs-interop python -m pytest tests/interop -q
```
Expected: all tests pass, including `test_binary_frames_survive`. (Requires the `tests/interop/requirements.txt` deps — `mutagen` — installed in the active Python env.)

If `popm[0].rating` is an `int` (mutagen returns POPM rating as an int 0–255), compare against `int(mp3["rating"])`; adjust the assertion to coerce both sides to `int`. Likewise `popm[0].count` is an int — compare to `int(mp3["playcount"])`. (The emitter writes them as JSON numbers, so `mp3["rating"]` is already an int after `json.load`; keep the comparison int-vs-int and drop any `.encode`.)

- [ ] **Step 5: Fmt + commit**

```bash
cargo fmt --all
git add musefs-core/tests/interop_emit.rs tests/interop/test_mutagen_roundtrip.py
git commit -m "test(interop): binary frames (POPM/UFID/PRIV/GEOB, MP4 ----) survive the mount (Binary Tags Phase 5)"
```

---

## Final verification

- [ ] **Workspace tests (incl. feature-gated proptests via unification):**

Run: `cargo test --workspace`
Expected: green. (Per CLAUDE.md, `--workspace` runs the `fuzzing`-gated format proptests via feature unification; FUSE e2e stays `#[ignore]`d.)

- [ ] **Format proptests explicitly:**

Run: `cargo test -p musefs-format --features fuzzing`
Expected: green, including `wav_binary_tags_round_trip_survives_byte_identically`.

- [ ] **Metrics tests:**

Run: `cargo test -p musefs-core --features metrics --test metrics`
Expected: green, including `revalidated_legacy_flac_resolve_does_no_front_read`.

- [ ] **Fuzz target compiles (out-of-workspace):**

Run: `cargo +nightly fuzz build mp3`
Expected: clean build.

- [ ] **Interop (manual, two-step):**

Run:
```bash
MUSEFS_INTEROP_DIR=/tmp/musefs-interop cargo test -p musefs-core --test interop_emit -- --ignored emit_interop_fixtures
MUSEFS_INTEROP_DIR=/tmp/musefs-interop python -m pytest tests/interop -q
```
Expected: green.

- [ ] **Lint + format gates:**

Run: `cargo clippy --all-targets` and `cargo fmt --all --check`
Expected: no warnings, no diff.

---

## Self-review notes (spec §Testing coverage map)

| Spec §Testing bullet | Covered by |
|----------------------|-----------|
| Round-trip proptest per format | MP3/MP4 pre-existing; **WAV → Task 1**; **FLAC payload-fidelity → Task 2** |
| Promoted + opaque UFID coexistence | Pre-existing (`proptest_mp3.rs`); re-exercised for WAV in Task 1 |
| Query-split correctness (tree path) | DB-layer pre-existing; **tree level → Task 4** |
| Byte-identical invariant with binary frames | **Task 3** (core MP3); also asserted in Task 1 (WAV materialize) and pre-existing MP4 proptest |
| Fuzz mp3/mp4 with binary frames | MP4 pre-existing; **MP3 → Task 7** |
| Interop (Property 5) POPM/UFID/PRIV/GEOB + MP4 ---- | **Task 8** |
| Migration V1→V2 idempotent + preserves rows | idempotency pre-existing; **data preservation → Task 6** |
| Legacy-FLAC migration/backfill (fallback + revalidate) | fallback pre-existing; **revalidate backfill (production fix in `scan.rs`/`structural.rs`) + front-read elimination → Task 5** |
