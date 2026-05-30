# SP0a — Corpus generator & compute bench suite — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build a synthetic music-library generator plus scan/refresh/read benchmarks with comparable reporting, so optimization sub-projects SP1–SP4 are validated against numbers. No production code paths change except an additive metrics field.

**Architecture:** A bench/test-support corpus generator lives in `musefs-core/tests/common/` (already shared by both integration tests and benches via `#[path]`). It writes deterministic backing files using the existing byte builders. Scan/refresh timings are `#[ignore]`d integration tests that print a comparable table (Criterion's resampling does not fit a 100k-file scan); the read path stays in the Criterion bench, generalized to consume the generator and add a concurrent variant. This is the first plan of SP0; the passthrough latency-injection FUSE is a separate plan (SP0b).

**Tech Stack:** Rust, `criterion` 0.8 (read bench, `harness = false`), `#[ignore]`d std tests with `--nocapture` (scan/refresh timing), `tempfile`, `musefs_db::Db`, `musefs_core::{scan_directory, revalidate, Musefs, MountConfig, Mode, VirtualTree}`. Linux `/proc/self/status` for RSS.

**Scope note (read before starting):** This plan delivers the `ci`, `large-compute`, and `bandwidth` tiers, real-mount runs (point `MUSEFS_BENCH_DIR` at any filesystem), and the `custom` / real-library modes — everything that does **not** need `/dev/fuse`. Deterministic latency injection (passthrough FUSE) and the fsync counter are SP0b. Reporting therefore shows `fsyncs: n/a` for these runs; SP0b fills that column.

**Spec:** `docs/superpowers/specs/2026-05-30-optimization-pass/SP0-measurement-foundation.md`

---

## File structure

- Create `musefs-core/tests/common/corpus.rs` — tiers, params, env parsing, deterministic generation.
- Create `musefs-core/tests/common/report.rs` — `RunReport`, peak-RSS, table printer.
- Modify `musefs-core/tests/common/mod.rs` — declare the two new submodules; add the moov-at-end M4A builder.
- Create `musefs-core/tests/bench_ingest.rs` — `#[ignore]`d timed `scan_directory` / `revalidate` reports.
- Create `musefs-core/tests/bench_refresh.rs` — `#[ignore]`d timed `poll_refresh` reports (1 vs N changed tracks).
- Modify `musefs-core/benches/read_throughput.rs` — consume the generator; add a concurrent read+walk group.

Module resolution note: Rust resolves `mod corpus;` / `mod report;` relative to the **real** path of `mod.rs` (`musefs-core/tests/common/`), so the submodule files work identically whether `mod.rs` is compiled as a test (`tests/common/mod.rs`) or `#[path]`-included by a bench. Each bench/test binary is its own crate, so `#[path]`-including `common` in several of them is fine (the existing `read_throughput` bench already does this).

---

## Task 1: moov-at-end M4A builder

The generator's format mix must include an M4A whose `moov` follows `mdat` (the SP1 bounded-read hard case). Only a moov-**first** builder (`minimal_m4a`) exists today. The MP4 reader's `locate` finds boxes by scanning top-level atoms, so order does not matter to parsing — verified against `musefs-format/src/mp4.rs`.

**Files:**
- Modify: `musefs-core/tests/common/mod.rs` (append after `minimal_m4a`, end of file ~line 179)
- Test: `musefs-core/tests/common_corpus_smoke.rs` (new — exercises `tests/common` helpers through the public scan path)

- [ ] **Step 1: Write the failing test**

Create `musefs-core/tests/common_corpus_smoke.rs`:

```rust
mod common;

use common::write_m4a_moov_last;
use musefs_db::Db;
use musefs_core::scan_directory;

#[test]
fn moov_last_m4a_scans_as_one_track() {
    let dir = tempfile::tempdir().unwrap();
    let (_off, _len) = write_m4a_moov_last(&dir.path().join("a.m4a"), &[0x11u8; 256]);
    let db = Db::open_in_memory().unwrap();
    let stats = scan_directory(&db, dir.path()).unwrap();
    assert_eq!(stats.scanned, 1, "moov-at-end M4A should probe & ingest");
    assert_eq!(stats.skipped, 0);
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p musefs-core --test common_corpus_smoke moov_last -- --nocapture`
Expected: FAIL — `cannot find function write_m4a_moov_last in module common`.

- [ ] **Step 3: Implement the builder**

Append to `musefs-core/tests/common/mod.rs`. It reuses the existing private `bx` / `m4a_data_atom` helpers and mirrors `minimal_m4a`, but emits `[ftyp, mdat, moov]` and patches the `stco` chunk offset to the (now earlier) `mdat` payload start:

```rust
/// Build a minimal valid M4A with `moov` AFTER `mdat` (the SP1 bounded-read hard
/// case). Same box contents as `minimal_m4a`; only top-level order differs. The
/// MP4 reader locates boxes by scanning, so order does not affect parsing.
pub fn minimal_m4a_moov_last(mdat_payload: &[u8]) -> Vec<u8> {
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
    let trak = bx(
        b"trak",
        &bx(b"mdia", &[bx(b"hdlr", &soun_hdlr), minf].concat()),
    );
    let moov = bx(b"moov", &[bx(b"mvhd", &[0u8; 8]), trak, udta].concat());
    let ftyp = bx(b"ftyp", b"M4A isom");
    let mdat = bx(b"mdat", mdat_payload);

    // Order: ftyp, mdat, moov. The mdat payload starts right after ftyp + mdat header.
    let mut out = [ftyp, mdat, moov].concat();
    let mdat_payload_offset = {
        // ftyp len + mdat header (8) is where the payload begins.
        let ftyp_len = 8 + b"M4A isom".len();
        (ftyp_len + 8) as u32
    };
    let stco = out
        .windows(4)
        .position(|w| w == b"stco")
        .expect("stco present");
    let entry = stco + 4 + 4 + 4;
    out[entry..entry + 4].copy_from_slice(&mdat_payload_offset.to_be_bytes());
    out
}

/// Write a moov-at-end M4A to `path`, returning (audio_offset, audio_length) of
/// the verbatim `mdat` payload.
pub fn write_m4a_moov_last(path: &std::path::Path, audio: &[u8]) -> (i64, i64) {
    let bytes = minimal_m4a_moov_last(audio);
    let ftyp_len = 8 + b"M4A isom".len();
    let audio_offset = (ftyp_len + 8) as i64;
    std::fs::write(path, &bytes).unwrap();
    (audio_offset, audio.len() as i64)
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p musefs-core --test common_corpus_smoke moov_last -- --nocapture`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add musefs-core/tests/common/mod.rs musefs-core/tests/common_corpus_smoke.rs
git commit -m "test(core): add moov-at-end M4A builder for the corpus generator"
```

---

## Task 2: Corpus params, tiers, and env parsing

**Files:**
- Create: `musefs-core/tests/common/corpus.rs`
- Modify: `musefs-core/tests/common/mod.rs` (add `pub mod corpus;` near the top, after the `use` line)
- Test: `musefs-core/tests/common_corpus_smoke.rs` (add cases)

- [ ] **Step 1: Write the failing test**

Append to `musefs-core/tests/common_corpus_smoke.rs`:

```rust
use common::corpus::{CorpusParams, Tier};

#[test]
fn tier_presets_have_expected_shape() {
    let ci = CorpusParams::for_tier(Tier::Ci);
    assert_eq!(ci.track_count(), 200);
    assert_eq!(ci.art_bytes_per_track, 0, "ci omits embedded art");

    let lc = CorpusParams::for_tier(Tier::LargeCompute);
    assert_eq!(lc.track_count(), 100_000);
    assert!(lc.art_bytes_per_track > 0, "large-compute embeds a cover");

    let bw = CorpusParams::for_tier(Tier::Bandwidth);
    assert!(bw.bytes_per_track >= 1_000_000, "bandwidth uses realistic payloads");
}

#[test]
fn env_overrides_apply_over_tier() {
    std::env::set_var("MUSEFS_BENCH_TIER", "ci");
    std::env::set_var("MUSEFS_BENCH_ALBUMS", "3");
    std::env::set_var("MUSEFS_BENCH_TRACKS_PER_ALBUM", "4");
    let p = CorpusParams::from_env();
    std::env::remove_var("MUSEFS_BENCH_ALBUMS");
    std::env::remove_var("MUSEFS_BENCH_TRACKS_PER_ALBUM");
    std::env::remove_var("MUSEFS_BENCH_TIER");
    assert_eq!(p.albums, 3);
    assert_eq!(p.tracks_per_album, 4);
    assert_eq!(p.track_count(), 12);
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p musefs-core --test common_corpus_smoke tier_presets -- --nocapture`
Expected: FAIL — `unresolved import common::corpus`.

- [ ] **Step 3: Implement params + tiers + env parsing**

Add to `musefs-core/tests/common/mod.rs` (after `use std::path::Path;`):

```rust
pub mod corpus;
```

Create `musefs-core/tests/common/corpus.rs`:

```rust
//! Deterministic synthetic-library generator for the SP0 bench harness.
//! Shared by `#[ignore]`d timing tests and the read Criterion bench.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tier {
    Ci,
    LargeCompute,
    Bandwidth,
    Custom,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    Flac,
    Mp3,
    M4aMoovFirst,
    M4aMoovLast,
    Wav,
}

#[derive(Clone, Debug)]
pub struct CorpusParams {
    pub albums: usize,
    pub tracks_per_album: usize,
    /// Audio payload bytes per track (file size = payload + format front + art).
    pub bytes_per_track: usize,
    /// Embedded cover bytes per track (0 = no embedded art). One shared cover
    /// per album, so the content-addressed `art` table dedups across the album.
    pub art_bytes_per_track: usize,
    /// Round-robin formats. Default `[Flac]`.
    pub format_mix: Vec<Format>,
    pub seed: u64,
}

impl CorpusParams {
    pub fn track_count(&self) -> usize {
        self.albums * self.tracks_per_album
    }

    pub fn for_tier(t: Tier) -> Self {
        match t {
            // ~200 tracks, tiny, no art — runs in seconds.
            Tier::Ci => CorpusParams {
                albums: 20,
                tracks_per_album: 10,
                bytes_per_track: 4 * 1024,
                art_bytes_per_track: 0,
                format_mix: vec![Format::Flac],
                seed: 1,
            },
            // 100k tracks, ~8 KB payload + one shared ~30 KB cover/album (deduped).
            Tier::LargeCompute => CorpusParams {
                albums: 10_000,
                tracks_per_album: 10,
                bytes_per_track: 8 * 1024,
                art_bytes_per_track: 30 * 1024,
                format_mix: vec![Format::Flac],
                seed: 1,
            },
            // ~1k tracks, realistic payload — real-mount throughput.
            Tier::Bandwidth => CorpusParams {
                albums: 100,
                tracks_per_album: 10,
                bytes_per_track: 30 * 1024 * 1024,
                art_bytes_per_track: 200 * 1024,
                format_mix: vec![Format::Flac],
                seed: 1,
            },
            // Defaults equal to ci; every dimension is env-overridable.
            Tier::Custom => CorpusParams::for_tier(Tier::Ci),
        }
    }

    /// Read `MUSEFS_BENCH_TIER` (default `ci`) then apply any `MUSEFS_BENCH_*`
    /// overrides. `MUSEFS_BENCH_FORMAT_MIX` is a comma list of
    /// flac|mp3|m4a|m4a-last|wav.
    pub fn from_env() -> Self {
        let tier = match std::env::var("MUSEFS_BENCH_TIER").as_deref() {
            Ok("large-compute") => Tier::LargeCompute,
            Ok("bandwidth") => Tier::Bandwidth,
            Ok("custom") => Tier::Custom,
            _ => Tier::Ci,
        };
        let mut p = CorpusParams::for_tier(tier);
        if let Some(v) = env_usize("MUSEFS_BENCH_ALBUMS") {
            p.albums = v;
        }
        if let Some(v) = env_usize("MUSEFS_BENCH_TRACKS_PER_ALBUM") {
            p.tracks_per_album = v;
        }
        if let Some(v) = env_usize("MUSEFS_BENCH_BYTES_PER_TRACK") {
            p.bytes_per_track = v;
        }
        if let Some(v) = env_usize("MUSEFS_BENCH_ART_PER_ALBUM") {
            p.art_bytes_per_track = v;
        }
        if let Some(v) = env_usize("MUSEFS_BENCH_SEED") {
            p.seed = v as u64;
        }
        if let Ok(mix) = std::env::var("MUSEFS_BENCH_FORMAT_MIX") {
            let parsed: Vec<Format> = mix
                .split(',')
                .filter_map(|s| match s.trim() {
                    "flac" => Some(Format::Flac),
                    "mp3" => Some(Format::Mp3),
                    "m4a" => Some(Format::M4aMoovFirst),
                    "m4a-last" => Some(Format::M4aMoovLast),
                    "wav" => Some(Format::Wav),
                    _ => None,
                })
                .collect();
            if !parsed.is_empty() {
                p.format_mix = parsed;
            }
        }
        p
    }
}

fn env_usize(key: &str) -> Option<usize> {
    std::env::var(key).ok().and_then(|s| s.parse().ok())
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p musefs-core --test common_corpus_smoke tier_presets -- --nocapture`
Run: `cargo test -p musefs-core --test common_corpus_smoke env_overrides -- --nocapture`
Expected: PASS both.

- [ ] **Step 5: Commit**

```bash
git add musefs-core/tests/common/mod.rs musefs-core/tests/common/corpus.rs musefs-core/tests/common_corpus_smoke.rs
git commit -m "test(core): corpus params, tier presets, and env overrides"
```

---

## Task 3: Deterministic generation (FLAC, with optional embedded art)

**Files:**
- Modify: `musefs-core/tests/common/corpus.rs` (add `generate` + helpers)
- Test: `musefs-core/tests/common_corpus_smoke.rs` (add cases)

- [ ] **Step 1: Write the failing test**

Append to `musefs-core/tests/common_corpus_smoke.rs`:

```rust
use common::corpus::{generate, Format};
use musefs_db::Db as Db2; // alias to avoid clashing with the Task-1 `Db` import

#[test]
fn generate_is_deterministic_and_scans_all_tracks() {
    let p = CorpusParams {
        albums: 2,
        tracks_per_album: 3,
        bytes_per_track: 512,
        art_bytes_per_track: 64,
        format_mix: vec![Format::Flac],
        seed: 7,
    };
    let a = tempfile::tempdir().unwrap();
    let b = tempfile::tempdir().unwrap();
    let files_a = generate(a.path(), &p);
    let files_b = generate(b.path(), &p);
    assert_eq!(files_a.len(), 6);
    // Determinism: same relative names and identical bytes for the first file.
    let first_a = std::fs::read(&files_a[0]).unwrap();
    let first_b = std::fs::read(&files_b[0]).unwrap();
    assert_eq!(first_a, first_b, "same (params, seed) => identical bytes");

    let db = Db2::open_in_memory().unwrap();
    let stats = musefs_core::scan_directory(&db, a.path()).unwrap();
    assert_eq!(stats.scanned, 6);
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p musefs-core --test common_corpus_smoke generate_is_deterministic -- --nocapture`
Expected: FAIL — `cannot find function generate`.

- [ ] **Step 3: Implement generation**

Append to `musefs-core/tests/common/corpus.rs`:

```rust
use std::path::{Path, PathBuf};

/// Deterministic filler audio: a seedable byte ramp (content is irrelevant —
/// `BackingAudio` is served verbatim and probing reads only headers).
fn filler(seed: u64, idx: usize, len: usize) -> Vec<u8> {
    let base = seed.wrapping_add(idx as u64).wrapping_mul(2654435761);
    (0..len).map(|i| (base.wrapping_add(i as u64) & 0xFF) as u8).collect()
}

/// One shared cover per album so the content-addressed `art` table dedups.
fn cover(seed: u64, album: usize, len: usize) -> Vec<u8> {
    filler(seed ^ 0xC0FFEE, album.wrapping_mul(101).wrapping_add(1), len)
}

/// A FLAC with STREAMINFO + comments + (optionally) a PICTURE block + audio.
/// Mirrors `tests/common/scan.rs`'s `flac_with_picture` layout.
fn flac_bytes(comments: &[String], art: Option<&[u8]>, audio: &[u8]) -> Vec<u8> {
    use super::{flac_block, streaminfo_body, vorbis_comment_body};
    let refs: Vec<&str> = comments.iter().map(|s| s.as_str()).collect();
    let mut out = Vec::new();
    out.extend_from_slice(b"fLaC");
    out.extend_from_slice(&flac_block(0, &streaminfo_body(), false));
    let last_meta = art.is_none();
    out.extend_from_slice(&flac_block(4, &vorbis_comment_body("musefs-bench", &refs), last_meta));
    if let Some(img) = art {
        let mut body = Vec::new();
        body.extend_from_slice(&3u32.to_be_bytes()); // picture type: front cover
        body.extend_from_slice(&(b"image/png".len() as u32).to_be_bytes());
        body.extend_from_slice(b"image/png");
        body.extend_from_slice(&0u32.to_be_bytes()); // description len
        body.extend_from_slice(&0u32.to_be_bytes()); // width
        body.extend_from_slice(&0u32.to_be_bytes()); // height
        body.extend_from_slice(&0u32.to_be_bytes()); // depth
        body.extend_from_slice(&0u32.to_be_bytes()); // colors
        body.extend_from_slice(&(img.len() as u32).to_be_bytes());
        body.extend_from_slice(img);
        out.extend_from_slice(&flac_block(6, &body, true));
    }
    out.extend_from_slice(audio);
    out
}

/// Generate the corpus into `dir` (created if missing). Layout is
/// `dir/album-{a}/track-{t}.{ext}`. Returns created file paths in stable order.
pub fn generate(dir: &Path, p: &CorpusParams) -> Vec<PathBuf> {
    std::fs::create_dir_all(dir).unwrap();
    let mut paths = Vec::with_capacity(p.track_count());
    let mut idx = 0usize;
    for album in 0..p.albums {
        let adir = dir.join(format!("album-{album:05}"));
        std::fs::create_dir_all(&adir).unwrap();
        let art_blob = (p.art_bytes_per_track > 0)
            .then(|| cover(p.seed, album, p.art_bytes_per_track));
        for track in 0..p.tracks_per_album {
            let fmt = p.format_mix[idx % p.format_mix.len()];
            let audio = filler(p.seed, idx, p.bytes_per_track);
            let comments = vec![
                format!("ARTIST=Artist {album:05}"),
                format!("ALBUM=Album {album:05}"),
                format!("TITLE=Track {track:03}"),
            ];
            let path = generate_one(&adir, idx, fmt, &comments, art_blob.as_deref(), &audio);
            paths.push(path);
            idx += 1;
        }
    }
    paths
}

fn generate_one(
    adir: &Path,
    idx: usize,
    fmt: Format,
    comments: &[String],
    art: Option<&[u8]>,
    audio: &[u8],
) -> PathBuf {
    match fmt {
        Format::Flac => {
            let path = adir.join(format!("track-{idx:06}.flac"));
            std::fs::write(&path, flac_bytes(comments, art, audio)).unwrap();
            path
        }
        Format::Mp3 => {
            let path = adir.join(format!("track-{idx:06}.mp3"));
            super::write_mp3(&path, audio);
            path
        }
        Format::M4aMoovFirst => {
            let path = adir.join(format!("track-{idx:06}.m4a"));
            super::write_m4a(&path, audio);
            path
        }
        Format::M4aMoovLast => {
            let path = adir.join(format!("track-{idx:06}.m4a"));
            super::write_m4a_moov_last(&path, audio);
            path
        }
        Format::Wav => {
            let path = adir.join(format!("track-{idx:06}.wav"));
            super::write_wav(&path, audio);
            path
        }
    }
}
```

Note: non-FLAC formats here carry tags via the DB at scan time the same way the existing builders do; embedded art is only generated for FLAC (the only builder with a picture block). This is sufficient — the default mix is FLAC-only and art exercises the FLAC synthesis path.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p musefs-core --test common_corpus_smoke generate_is_deterministic -- --nocapture`
Expected: PASS (6 files, identical first-file bytes, all 6 scanned).

- [ ] **Step 5: Commit**

```bash
git add musefs-core/tests/common/corpus.rs musefs-core/tests/common_corpus_smoke.rs
git commit -m "test(core): deterministic corpus generation with optional FLAC art"
```

---

## Task 4: Target resolution (generate vs real-library) + DB path

**Files:**
- Modify: `musefs-core/tests/common/corpus.rs` (add `Target` + `prepare`)
- Test: `musefs-core/tests/common_corpus_smoke.rs` (add case)

- [ ] **Step 1: Write the failing test**

Append to `musefs-core/tests/common_corpus_smoke.rs`:

```rust
use common::corpus::prepare;

#[test]
fn prepare_generates_when_no_library_set() {
    std::env::remove_var("MUSEFS_BENCH_LIBRARY");
    let scratch = tempfile::tempdir().unwrap();
    std::env::set_var("MUSEFS_BENCH_DIR", scratch.path());
    let p = CorpusParams {
        albums: 1,
        tracks_per_album: 2,
        bytes_per_track: 128,
        art_bytes_per_track: 0,
        format_mix: vec![Format::Flac],
        seed: 3,
    };
    let t = prepare(&p);
    std::env::remove_var("MUSEFS_BENCH_DIR");
    assert!(t.corpus_dir.exists());
    assert!(!t.is_real_library);
    // DB path is separate from the corpus dir.
    assert_ne!(t.db_path, t.corpus_dir);
    let db = Db2::open(&t.db_path).unwrap();
    let stats = musefs_core::scan_directory(&db, &t.corpus_dir).unwrap();
    assert_eq!(stats.scanned, 2);
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p musefs-core --test common_corpus_smoke prepare_generates -- --nocapture`
Expected: FAIL — `cannot find function prepare`.

- [ ] **Step 3: Implement target resolution**

Append to `musefs-core/tests/common/corpus.rs`:

```rust
/// Where the corpus and DB live for a run, and whether it was generated.
pub struct Target {
    pub corpus_dir: PathBuf,
    pub db_path: PathBuf,
    pub is_real_library: bool,
    /// Held to keep a tempdir alive for the run when one was created.
    _scratch: Option<tempfile::TempDir>,
}

/// Resolve the run target:
/// - `MUSEFS_BENCH_LIBRARY` set -> scan that real directory in place (never
///   written to); DB goes to `MUSEFS_BENCH_DB` or a fresh tempfile.
/// - else generate the corpus under `MUSEFS_BENCH_DIR` (or a tempdir) and put
///   the DB alongside under a separate `musefs-bench.db` name.
pub fn prepare(p: &CorpusParams) -> Target {
    if let Ok(lib) = std::env::var("MUSEFS_BENCH_LIBRARY") {
        let scratch = tempfile::tempdir().unwrap();
        let db_path = std::env::var("MUSEFS_BENCH_DB")
            .map(PathBuf::from)
            .unwrap_or_else(|_| scratch.path().join("musefs-bench.db"));
        return Target {
            corpus_dir: PathBuf::from(lib),
            db_path,
            is_real_library: true,
            _scratch: Some(scratch),
        };
    }
    let (corpus_dir, scratch) = match std::env::var("MUSEFS_BENCH_DIR") {
        Ok(d) => (PathBuf::from(d), None),
        Err(_) => {
            let s = tempfile::tempdir().unwrap();
            (s.path().to_path_buf(), Some(s))
        }
    };
    generate(&corpus_dir, p);
    let db_path = std::env::var("MUSEFS_BENCH_DB")
        .map(PathBuf::from)
        .unwrap_or_else(|_| corpus_dir.join("musefs-bench.db"));
    Target {
        corpus_dir,
        db_path,
        is_real_library: false,
        _scratch: scratch,
    }
}
```

Add `tempfile` is already a dev-dependency, so it is in scope for test/bench code.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p musefs-core --test common_corpus_smoke prepare_generates -- --nocapture`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add musefs-core/tests/common/corpus.rs musefs-core/tests/common_corpus_smoke.rs
git commit -m "test(core): target resolution for generated vs real-library runs"
```

---

## Task 5: Reporting (peak RSS + comparable table)

**Files:**
- Create: `musefs-core/tests/common/report.rs`
- Modify: `musefs-core/tests/common/mod.rs` (add `pub mod report;`)
- Test: `musefs-core/tests/common_corpus_smoke.rs` (add case)

- [ ] **Step 1: Write the failing test**

Append to `musefs-core/tests/common_corpus_smoke.rs`:

```rust
use common::report::{peak_rss_kib, RunReport};

#[test]
fn report_renders_a_row() {
    let r = RunReport {
        label: "scan".into(),
        tier: "ci".into(),
        storage: "tempfs".into(),
        wall_ms: 1234,
        opens: 200,
        preads: 200,
        fsyncs: None,
        peak_rss_kib: Some(50_000),
    };
    let line = r.row();
    assert!(line.contains("scan"));
    assert!(line.contains("ci"));
    assert!(line.contains("n/a"), "fsyncs None renders as n/a");
    // RSS is readable and positive on Linux.
    assert!(peak_rss_kib().unwrap_or(1) > 0);
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p musefs-core --test common_corpus_smoke report_renders -- --nocapture`
Expected: FAIL — `unresolved import common::report`.

- [ ] **Step 3: Implement reporting**

Add to `musefs-core/tests/common/mod.rs`:

```rust
pub mod report;
```

Create `musefs-core/tests/common/report.rs`:

```rust
//! Comparable run reporting for the SP0 bench harness.

/// One measured run. `fsyncs`/`peak_rss_kib` are `None` when not applicable
/// (e.g. fsyncs need the SP0b passthrough FS; RSS is meaningful only in-process).
pub struct RunReport {
    pub label: String,
    pub tier: String,
    pub storage: String,
    pub wall_ms: u128,
    pub opens: u64,
    pub preads: u64,
    pub fsyncs: Option<u64>,
    pub peak_rss_kib: Option<u64>,
}

impl RunReport {
    pub fn header() -> String {
        format!(
            "{:<10} {:<14} {:<10} {:>10} {:>10} {:>10} {:>10} {:>12}",
            "label", "tier", "storage", "wall_ms", "opens", "preads", "fsyncs", "rss_kib"
        )
    }

    pub fn row(&self) -> String {
        let opt = |v: Option<u64>| v.map(|x| x.to_string()).unwrap_or_else(|| "n/a".into());
        format!(
            "{:<10} {:<14} {:<10} {:>10} {:>10} {:>10} {:>10} {:>12}",
            self.label,
            self.tier,
            self.storage,
            self.wall_ms,
            self.opens,
            self.preads,
            opt(self.fsyncs),
            opt(self.peak_rss_kib),
        )
    }
}

/// Peak resident set size (high-water mark) in KiB, from `/proc/self/status`
/// `VmHWM`. Linux only; `None` elsewhere or if unreadable.
pub fn peak_rss_kib() -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("VmHWM:") {
            return rest.trim().trim_end_matches(" kB").trim().parse().ok();
        }
    }
    None
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p musefs-core --test common_corpus_smoke report_renders -- --nocapture`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add musefs-core/tests/common/mod.rs musefs-core/tests/common/report.rs musefs-core/tests/common_corpus_smoke.rs
git commit -m "test(core): comparable run reporting (peak RSS + table row)"
```

---

## Task 6: Scan/ingest timing bench (`#[ignore]`d)

**Files:**
- Create: `musefs-core/tests/bench_ingest.rs`

- [ ] **Step 1: Write the test (an `#[ignore]`d timing harness)**

Create `musefs-core/tests/bench_ingest.rs`:

```rust
mod common;

use std::time::Instant;

use common::corpus::{prepare, CorpusParams};
use common::report::{peak_rss_kib, RunReport};
use musefs_core::{metrics, revalidate, scan_directory};
use musefs_db::Db;

fn storage_label(t: &common::corpus::Target) -> String {
    if t.is_real_library {
        "real-lib".into()
    } else if std::env::var("MUSEFS_BENCH_DIR").is_ok() {
        "env-dir".into()
    } else {
        "tempfs".into()
    }
}

#[test]
#[ignore = "SP0 timing harness; run with --ignored --nocapture"]
fn bench_cold_scan_and_revalidate() {
    let params = CorpusParams::from_env();
    let tier = std::env::var("MUSEFS_BENCH_TIER").unwrap_or_else(|_| "ci".into());
    let target = prepare(&params);
    let storage = storage_label(&target);

    let db = Db::open(&target.db_path).unwrap();

    metrics::reset();
    let t0 = Instant::now();
    let stats = scan_directory(&db, &target.corpus_dir).unwrap();
    let scan_ms = t0.elapsed().as_millis();
    let s = metrics::snapshot();

    // Second pass: revalidate should skip unchanged files (cheap).
    metrics::reset();
    let t1 = Instant::now();
    let _ = revalidate(&db, &target.corpus_dir).unwrap();
    let reval_ms = t1.elapsed().as_millis();

    println!("\n{}", RunReport::header());
    println!(
        "{}",
        RunReport {
            label: "scan".into(),
            tier: tier.clone(),
            storage: storage.clone(),
            wall_ms: scan_ms,
            opens: s.opens,
            preads: s.preads,
            fsyncs: None,
            peak_rss_kib: peak_rss_kib(),
        }
        .row()
    );
    println!(
        "{}",
        RunReport {
            label: "revalidate".into(),
            tier,
            storage,
            wall_ms: reval_ms,
            opens: 0,
            preads: 0,
            fsyncs: None,
            peak_rss_kib: peak_rss_kib(),
        }
        .row()
    );
    println!("scanned={} skipped={}\n", stats.scanned, stats.skipped);
    assert!(stats.scanned > 0);
}
```

Note: the `metrics` counters instrument the **serve** path
(`read_at`/`open_handle`/`read_segments`), not the scan path — so `opens`/`preads`
read ≈0 during a pure scan even with `--features metrics`. The SP1-relevant
signals in this bench are **`wall_ms`** and **`peak_rss_kib`**; the latter
captures SP1's whole-file-`fs::read` memory spike (the headline SP1 problem).
Reporting the ≈0 serve counters is intentional and harmless. (Per-file scan I/O
counting is added in SP1 when `scan.rs` is being modified, not here.)

- [ ] **Step 2: Run it (ci tier) to verify it runs and reports**

Run: `cargo test -p musefs-core --features metrics --test bench_ingest -- --ignored --nocapture`
Expected: PASS, prints a two-row table; `scanned` equals the ci track count (200) on a fresh tempfs run. `wall_ms` and `rss_kib` are populated; `opens`/`preads` are ≈0 (see note above).

- [ ] **Step 3: Verify it does nothing under a normal test run**

Run: `cargo test -p musefs-core --test bench_ingest`
Expected: `0 passed; ... 1 ignored` — it stays out of the default suite.

- [ ] **Step 4: Commit**

```bash
git add musefs-core/tests/bench_ingest.rs
git commit -m "test(core): SP0 cold-scan + revalidate timing harness"
```

---

## Task 7: Refresh timing bench (`#[ignore]`d, 1 vs N changed)

This records the baseline SP2 attacks: a full tree rebuild after touching 1 vs N tracks. Mutate tags through a second `Db` connection on the same file (bumps `data_version` whole-DB + `content_version` via triggers), then time `poll_refresh`. Disable debounce with `poll_interval = ZERO`.

**Files:**
- Create: `musefs-core/tests/bench_refresh.rs`

- [ ] **Step 1: Write the `#[ignore]`d harness**

Create `musefs-core/tests/bench_refresh.rs`:

```rust
mod common;

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use common::corpus::{prepare, CorpusParams};
use common::report::RunReport;
use musefs_core::{scan_directory, Mode, Musefs, MountConfig};
use musefs_db::Db;

fn config() -> MountConfig {
    MountConfig {
        template: "$artist/$album/$title".to_string(),
        fallbacks: BTreeMap::new(),
        default_fallback: "Unknown".to_string(),
        mode: Mode::Synthesis,
        poll_interval: Duration::ZERO, // no debounce: each poll actually polls
    }
}

/// Re-tag `count` tracks via a separate connection, then time the refresh.
fn time_refresh(db_path: &std::path::Path, fs: &Musefs, count: usize) -> u128 {
    let writer = Db::open(db_path).unwrap();
    let tracks = writer.list_tracks().unwrap();
    for t in tracks.iter().take(count) {
        // Append a tag so content_version + data_version bump.
        writer
            .replace_tags(
                t.id,
                &[musefs_db::Tag::new("COMMENT", "bench-touch", 0)],
            )
            .unwrap();
    }
    let t0 = Instant::now();
    fs.poll_refresh();
    t0.elapsed().as_millis()
}

#[test]
#[ignore = "SP0 timing harness; run with --ignored --nocapture"]
fn bench_refresh_one_vs_many() {
    let params = CorpusParams::from_env();
    let tier = std::env::var("MUSEFS_BENCH_TIER").unwrap_or_else(|_| "ci".into());
    let target = prepare(&params);

    let db = Db::open(&target.db_path).unwrap();
    scan_directory(&db, &target.corpus_dir).unwrap();
    let fs = Musefs::open(db, config()).unwrap();

    let one_ms = time_refresh(&target.db_path, &fs, 1);
    let many = (params.track_count() / 2).max(1);
    let many_ms = time_refresh(&target.db_path, &fs, many);

    println!("\n{}", RunReport::header());
    for (label, ms) in [("refresh-1", one_ms), ("refresh-N", many_ms)] {
        println!(
            "{}",
            RunReport {
                label: label.into(),
                tier: tier.clone(),
                storage: "tempfs".into(),
                wall_ms: ms,
                opens: 0,
                preads: 0,
                fsyncs: None,
                peak_rss_kib: None,
            }
            .row()
        );
    }
    println!("touched_many={many}\n");
}
```

Note: this assumes `Db::list_tracks`, `Db::replace_tags`, and `musefs_db::Tag::new` are public (confirmed in `musefs-db/src/tracks.rs` / `tags.rs` / `models.rs`). If `musefs_db::Tag` is not re-exported at the crate root, use `musefs_db::models::Tag`.

- [ ] **Step 2: Run it (ci tier)**

Run: `cargo test -p musefs-core --test bench_refresh -- --ignored --nocapture`
Expected: PASS, prints `refresh-1` and `refresh-N` rows. On ci both are small and similar (the current rebuild is full — that is the SP2 baseline).

- [ ] **Step 3: Verify ignored in normal runs**

Run: `cargo test -p musefs-core --test bench_refresh`
Expected: `0 passed; ... 1 ignored`.

- [ ] **Step 4: Commit**

```bash
git add musefs-core/tests/bench_refresh.rs
git commit -m "test(core): SP0 refresh timing harness (1 vs N changed)"
```

---

## Task 8: Generalize the read bench + add a concurrent variant

**Files:**
- Modify: `musefs-core/benches/read_throughput.rs`

- [ ] **Step 1: Replace the bench body to consume the generator and add concurrency**

Rewrite `musefs-core/benches/read_throughput.rs` as:

```rust
use std::collections::BTreeMap;
use std::sync::Arc;
use std::thread;

use criterion::{criterion_group, criterion_main, Criterion, Throughput};
use musefs_core::{scan_directory, Mode, Musefs, MountConfig, VirtualTree};

#[path = "../tests/common/mod.rs"]
mod common;
use common::corpus::{generate, CorpusParams, Format};

fn config() -> MountConfig {
    MountConfig {
        template: "$artist/$album/$title".to_string(),
        fallbacks: BTreeMap::new(),
        default_fallback: "Unknown".to_string(),
        mode: Mode::Synthesis,
        poll_interval: std::time::Duration::ZERO,
    }
}

/// A small generated corpus with a few MB of audio per track, scanned into an
/// in-memory DB and mounted. Returns the fs plus all file inodes.
fn fixture(bytes_per_track: usize, tracks: usize) -> (Arc<Musefs>, Vec<u64>) {
    let p = CorpusParams {
        albums: 1,
        tracks_per_album: tracks,
        bytes_per_track,
        art_bytes_per_track: 0,
        format_mix: vec![Format::Flac],
        seed: 42,
    };
    let dir = tempfile::tempdir().unwrap();
    generate(dir.path(), &p);
    let db = musefs_db::Db::open_in_memory().unwrap();
    scan_directory(&db, dir.path()).unwrap();
    let fs = Arc::new(Musefs::open(db, config()).unwrap());

    // Walk the single album dir to collect file inodes.
    let album = fs.lookup(VirtualTree::ROOT, "Artist 00000").unwrap();
    let sub = fs.readdir(album).unwrap()[0].1; // Album 00000 dir
    let inodes: Vec<u64> = fs.readdir(sub).unwrap().into_iter().map(|(_, ino, _)| ino).collect();
    // Keep the tempdir alive for the duration of the bench by leaking it.
    std::mem::forget(dir);
    (fs, inodes)
}

fn bench_sequential_read(c: &mut Criterion) {
    let (fs, inodes) = fixture(4 * 1024 * 1024, 1);
    let inode = inodes[0];
    let size = fs.getattr(inode).unwrap().size;
    let mut group = c.benchmark_group("sequential_read");
    group.throughput(Throughput::Bytes(size));
    let chunk = 128 * 1024u64;
    group.bench_function("flac_128k_chunks", |b| {
        b.iter(|| {
            let mut off = 0u64;
            while off < size {
                let got = std::hint::black_box(fs.read(inode, 0, off, chunk).unwrap());
                if got.is_empty() {
                    break;
                }
                off += got.len() as u64;
            }
        });
    });
    group.finish();
}

fn bench_concurrent_read_and_walk(c: &mut Criterion) {
    // M reader threads streaming distinct files + one metadata walker, sharing
    // one Arc<Musefs>. Exercises handles/size_cache mutex contention (SP3).
    let m = std::env::var("MUSEFS_BENCH_READERS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| num_streams());
    let (fs, inodes) = fixture(1024 * 1024, m.max(2));

    let mut group = c.benchmark_group("concurrent_read_walk");
    group.bench_function(format!("m{m}_plus_walker"), |b| {
        b.iter(|| {
            let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
            // Walker thread: loop lookups/getattrs over the inodes.
            let walker = {
                let fs = Arc::clone(&fs);
                let inodes = inodes.clone();
                let stop = Arc::clone(&stop);
                thread::spawn(move || {
                    while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                        for &ino in &inodes {
                            let _ = std::hint::black_box(fs.getattr(ino));
                        }
                    }
                })
            };
            let readers: Vec<_> = (0..m)
                .map(|i| {
                    let fs = Arc::clone(&fs);
                    let ino = inodes[i % inodes.len()];
                    thread::spawn(move || {
                        // open_handle + per-handle reads exercise the `handles`
                        // mutex (SP3); the walker's getattr exercises `size_cache`.
                        let fh = fs.open_handle(ino).unwrap();
                        let size = fs.getattr(ino).unwrap().size;
                        let mut off = 0u64;
                        while off < size {
                            let got = fs.read(ino, fh, off, 128 * 1024).unwrap();
                            if got.is_empty() {
                                break;
                            }
                            off += got.len() as u64;
                        }
                        fs.release_handle(fh);
                    })
                })
                .collect();
            for r in readers {
                r.join().unwrap();
            }
            stop.store(true, std::sync::atomic::Ordering::Relaxed);
            walker.join().unwrap();
        });
    });
    group.finish();
}

fn num_streams() -> usize {
    std::thread::available_parallelism().map(|n| n.get() * 2).unwrap_or(4)
}

criterion_group!(benches, bench_sequential_read, bench_concurrent_read_and_walk);
criterion_main!(benches);
```

- [ ] **Step 2: Run the bench (quick smoke)**

Run: `cargo bench -p musefs-core --bench read_throughput -- --warm-up-time 1 --measurement-time 2`
Expected: both `sequential_read/flac_128k_chunks` and `concurrent_read_walk/mN_plus_walker` report timings without panicking.

- [ ] **Step 3: Confirm the workspace still builds/tests clean**

Run: `cargo test -p musefs-core`
Expected: all existing tests PASS; the new `#[ignore]`d benches stay ignored.

- [ ] **Step 4: Commit**

```bash
git add musefs-core/benches/read_throughput.rs
git commit -m "bench(core): drive read bench from the corpus generator; add concurrent read+walk"
```

---

## Task 9: Document the harness in the spec directory README

**Files:**
- Modify: `docs/superpowers/specs/2026-05-30-optimization-pass/README.md` (status table + a short "How to run SP0a" section)

- [ ] **Step 1: Update the SP0 status row and add run instructions**

In the Status table, change the SP0 row to `Harness (SP0a) implemented; SP0b pending`. Append a section:

```markdown
## Running the SP0a harness

```bash
# Read throughput + concurrent read/walk (Criterion):
cargo bench -p musefs-core --bench read_throughput

# Cold scan + revalidate timing (prints a table):
cargo test -p musefs-core --features metrics --test bench_ingest -- --ignored --nocapture

# Refresh timing, 1 vs N changed tracks:
cargo test -p musefs-core --test bench_refresh -- --ignored --nocapture

# Scale / storage knobs (any of the timing/bench commands above):
MUSEFS_BENCH_TIER=large-compute \
MUSEFS_BENCH_DIR=/mnt/ssd/musefs-bench \
  cargo test -p musefs-core --features metrics --test bench_ingest -- --ignored --nocapture

# Run against a real library (never written to; DB goes to MUSEFS_BENCH_DB or a tempfile):
MUSEFS_BENCH_LIBRARY=/srv/music \
MUSEFS_BENCH_DB=/tmp/musefs-bench.db \
  cargo test -p musefs-core --features metrics --test bench_ingest -- --ignored --nocapture
```
```

- [ ] **Step 2: Commit**

```bash
git add docs/superpowers/specs/2026-05-30-optimization-pass/README.md
git commit -m "docs: record SP0a harness and how to run it"
```

---

## Self-review notes (for the executor)

- **Spec coverage:** corpus generator with tiers + custom + real-library (Tasks 2–4), scan/refresh/read benches incl. concurrent (Tasks 6–8), comparable reporting (Task 5), moov-at-end fixture (Task 1). Deferred to SP0b per the scope note: passthrough-FUSE latency injection, the fsync counter/column, and the `ssd`-smoke + latency-effect + fsync-direction acceptance criteria (all FUSE-dependent).
- **Determinism gate:** Task 3 asserts identical bytes for a fixed (params, seed).
- **No-regression gate:** Tasks 6–8 keep all `#[ignore]`d so the default `cargo test` is unchanged; Task 8 Step 3 confirms the existing suite stays green.
- **Reproducibility threshold (spec acceptance):** Criterion already reports medians + variance for the read benches; defining the explicit `ci` regression percentage is a one-line policy choice the user makes when wiring CI — flagged here, not blocking.
- **Type consistency:** `CorpusParams`, `Tier`, `Format`, `Target`, `RunReport`, `generate`, `prepare`, `peak_rss_kib` are used with identical signatures across tasks.
```
