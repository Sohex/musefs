# SP0a per-format bench coverage — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Sweep the SP0a ingest and read benches over every supported format (FLAC, MP3, M4A moov-first, M4A moov-last, Ogg, WAV) with a per-format report column, and add the missing Ogg (Opus) corpus builder. No production code changes.

**Architecture:** Extend the shared bench-support module (`musefs-core/tests/common/`): add a `write_ogg` builder reusing `musefs-format`'s public `ogg::page_test_support` helpers, a `Format::Ogg` corpus variant, a centralized `ALL_FORMATS` + token mapping, a `format` column on `RunReport`, and a `prepare_format` per-format target helper. The `#[ignore]`d `bench_ingest` and the Criterion `read_throughput` bench then loop the format set; `bench_refresh` stays FLAC-only.

**Tech Stack:** Rust, `criterion` (read bench, `harness = false`), `#[ignore]`d std tests, `tempfile`, `musefs_db::Db`, `musefs_core::{scan_directory, revalidate, Musefs, MountConfig, Mode, VirtualTree, metrics}`, and `musefs_format::ogg::page_test_support` (already a `musefs-core` dev-dependency).

**Spec:** `docs/superpowers/specs/2026-05-30-optimization-pass/SP0a-per-format-coverage.md`

---

## Conventions for every task

- Work in the worktree `/home/cfutro/git/musefs/.claude/worktrees/optimization-pass` on branch `worktree-optimization-pass`.
- The pre-commit hook runs `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, and `cargo test --workspace`. **Before every commit run `cargo fmt -p musefs-core` then `cargo clippy --all-targets -- -D warnings`** (the `--all-targets` form lints benches and the shared `tests/common` module, which a plain `cargo clippy --tests` misses). A commit fails if any hook fails.
- The editor's LSP/rust-analyzer diagnostics in this worktree are known to lag (stale "unresolved import"/"unlinked-file" notes). Trust `cargo` output, not diagnostics.
- End every commit message with a HEREDOC trailer line: `Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>`. Stage only the files named in the task.
- The `common` module has a crate-level `#![allow(dead_code)]` (top of `tests/common/mod.rs`) that covers `corpus.rs`/`report.rs`, so symbols added before their first use do not trip the dead-code lint.

---

## File structure

- Modify `musefs-core/tests/common/mod.rs` — add `write_ogg`.
- Modify `musefs-core/tests/common/corpus.rs` — `Format::Ogg`, `format_from_token`/`format_token`, `ALL_FORMATS`, `bench_formats`, `prepare_format`, `bench_base_dir`, `generate_one` Ogg arm.
- Modify `musefs-core/tests/common/report.rs` — add the `format` column.
- Modify `musefs-core/tests/common_corpus_smoke.rs` — new unit tests; update the `RunReport` constructor.
- Rewrite `musefs-core/tests/bench_ingest.rs` — per-format sweep.
- Modify `musefs-core/benches/read_throughput.rs` — per-format groups + generic inode walk.
- Modify `musefs-core/tests/bench_refresh.rs` — `format: "flac"` + a clarifying comment (in Task 4).
- Modify `docs/superpowers/specs/2026-05-30-optimization-pass/README.md` — per-format run notes.

New tests live in `musefs-core/tests/common_corpus_smoke.rs` (which already has a `mod common;`, an `ENV_LOCK` mutex, and uses fully-qualified `common::corpus::…` calls — follow that style; do **not** add new `use common::corpus::{…}` lines, to avoid duplicate-import churn).

---

## Task 1: `write_ogg` corpus builder

**Files:**
- Modify: `musefs-core/tests/common/mod.rs` (append after `write_m4a_moov_last`, end of file)
- Test: `musefs-core/tests/common_corpus_smoke.rs` (append two tests + one `use`)

- [ ] **Step 1: Write the failing tests**

Add the import near the other `use common::…` lines at the top of `musefs-core/tests/common_corpus_smoke.rs`:

```rust
use common::write_ogg;
```

Append to the end of `musefs-core/tests/common_corpus_smoke.rs`:

```rust
#[test]
fn write_ogg_scans_as_one_track() {
    let dir = tempfile::tempdir().unwrap();
    write_ogg(&dir.path().join("a.ogg"), &[0x22u8; 256]);
    let db = Db::open_in_memory().unwrap();
    let stats = scan_directory(&db, dir.path()).unwrap();
    assert_eq!(stats.scanned, 1, "minimal Ogg Opus should probe & ingest");
    assert_eq!(stats.skipped, 0);
}

#[test]
fn write_ogg_is_deterministic() {
    let dir = tempfile::tempdir().unwrap();
    let a = dir.path().join("a.ogg");
    let b = dir.path().join("b.ogg");
    write_ogg(&a, &[0x33u8; 300]);
    write_ogg(&b, &[0x33u8; 300]);
    assert_eq!(
        std::fs::read(&a).unwrap(),
        std::fs::read(&b).unwrap(),
        "same audio bytes => identical Ogg file"
    );
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p musefs-core --test common_corpus_smoke write_ogg -- --nocapture`
Expected: FAIL — `cannot find function write_ogg in module common`.

- [ ] **Step 3: Implement `write_ogg`**

Append to `musefs-core/tests/common/mod.rs`:

```rust
/// Write a minimal valid Ogg **Opus** file (two header pages + one audio page
/// whose packet body is `audio`) to `path`, returning (audio_offset,
/// audio_length) of the audio-page span. Mirrors the recipe in
/// `musefs-core/src/scan.rs`'s `ogg_probe_tests`: the `OpusTags` body must be a
/// parseable VorbisComment (here empty) because the scanner runs `read_tags`.
/// The synthesizer treats the audio packet body as opaque (renumbers pages,
/// recomputes CRCs, never decodes), so arbitrary `audio` bytes are valid. The
/// return is informational — `scan_directory` re-probes the file.
pub fn write_ogg(path: &Path, audio: &[u8]) -> (i64, i64) {
    use musefs_format::ogg::page_test_support::{
        build_header_pub, lace_packet_pub, vorbis_body_empty,
    };
    let head = b"OpusHead\x01\x02\x38\x01\x80\xbb\x00\x00\x00\x00\x00".to_vec();
    let mut tags = b"OpusTags".to_vec();
    tags.extend_from_slice(&vorbis_body_empty());
    let serial = 0x6d75_7366; // "musf"
    // build_header returns (bytes, header_page_count); the audio page continues
    // the sequence at that count.
    let (mut bytes, header_pages) = build_header_pub(serial, &[&head, &tags]);
    let header_len = bytes.len();
    let (page, _) = lace_packet_pub(serial, header_pages, false, 960, audio);
    bytes.extend_from_slice(&page);
    std::fs::write(path, &bytes).unwrap();
    (header_len as i64, (bytes.len() - header_len) as i64)
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p musefs-core --test common_corpus_smoke write_ogg -- --nocapture`
Expected: PASS (both `write_ogg_scans_as_one_track` and `write_ogg_is_deterministic`).

- [ ] **Step 5: Verify fmt + clippy**

Run: `cargo fmt -p musefs-core && cargo clippy --all-targets -- -D warnings`
Expected: clean (no warnings).

- [ ] **Step 6: Commit**

```bash
git add musefs-core/tests/common/mod.rs musefs-core/tests/common_corpus_smoke.rs
git commit -m "$(cat <<'EOF'
test(core): add write_ogg minimal Opus corpus builder

Reuses musefs-format's public ogg::page_test_support helpers (already a
musefs-core dev-dep); OpusTags carries an empty VorbisComment body so the
scanner's read_tags probe succeeds.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 2: `Format::Ogg` + token mapping refactor + `generate_one` arm

**Files:**
- Modify: `musefs-core/tests/common/corpus.rs`
- Test: `musefs-core/tests/common_corpus_smoke.rs` (append one test)

- [ ] **Step 1: Write the failing test**

Append to `musefs-core/tests/common_corpus_smoke.rs`:

```rust
#[test]
fn generate_with_ogg_in_mix_scans_all() {
    let p = CorpusParams {
        albums: 1,
        tracks_per_album: 4,
        bytes_per_track: 256,
        art_bytes_per_track: 0,
        format_mix: vec![Format::Flac, Format::Mp3, Format::Ogg, Format::Wav],
        seed: 9,
    };
    let dir = tempfile::tempdir().unwrap();
    common::corpus::generate(dir.path(), &p);
    let db = Db::open_in_memory().unwrap();
    let stats = scan_directory(&db, dir.path()).unwrap();
    assert_eq!(stats.scanned, 4, "all four formats (incl. Ogg) ingest");
}
```

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo test -p musefs-core --test common_corpus_smoke generate_with_ogg -- --nocapture`
Expected: FAIL — `no variant named Ogg found for enum Format` (compile error).

- [ ] **Step 3: Add the `Ogg` variant**

In `musefs-core/tests/common/corpus.rs`, change the `Format` enum (currently ends `M4aMoovLast, Wav,`):

```rust
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    Flac,
    Mp3,
    M4aMoovFirst,
    M4aMoovLast,
    Ogg,
    Wav,
}
```

- [ ] **Step 4: Add the token mapping helpers**

In `musefs-core/tests/common/corpus.rs`, add these two free functions immediately after the `Format` enum (before `pub struct CorpusParams`):

```rust
/// Map a `MUSEFS_BENCH_FORMAT_MIX` token to a `Format`. Single source of truth
/// for both `from_env` and `bench_formats`.
pub fn format_from_token(token: &str) -> Option<Format> {
    match token.trim() {
        "flac" => Some(Format::Flac),
        "mp3" => Some(Format::Mp3),
        "m4a" => Some(Format::M4aMoovFirst),
        "m4a-last" => Some(Format::M4aMoovLast),
        "ogg" => Some(Format::Ogg),
        "wav" => Some(Format::Wav),
        _ => None,
    }
}

/// The canonical token for a `Format` (inverse of `format_from_token`). Used for
/// report labels, per-format corpus subdir names, and `.ext` choices.
pub fn format_token(f: Format) -> &'static str {
    match f {
        Format::Flac => "flac",
        Format::Mp3 => "mp3",
        Format::M4aMoovFirst => "m4a",
        Format::M4aMoovLast => "m4a-last",
        Format::Ogg => "ogg",
        Format::Wav => "wav",
    }
}
```

- [ ] **Step 5: Route `from_env`'s parser through `format_from_token`**

In `from_env`, replace the inline match block:

```rust
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
            // An all-unrecognized value keeps the tier default rather than
            // erroring or yielding an empty mix.
            if !parsed.is_empty() {
                p.format_mix = parsed;
            }
        }
```

with:

```rust
        if let Ok(mix) = std::env::var("MUSEFS_BENCH_FORMAT_MIX") {
            let parsed: Vec<Format> = mix.split(',').filter_map(format_from_token).collect();
            // An all-unrecognized value keeps the tier default rather than
            // erroring or yielding an empty mix.
            if !parsed.is_empty() {
                p.format_mix = parsed;
            }
        }
```

- [ ] **Step 6: Add the `generate_one` Ogg arm**

In `generate_one`, add this arm before the `Format::Wav` arm:

```rust
        Format::Ogg => {
            let path = adir.join(format!("track-{idx:06}.ogg"));
            super::write_ogg(&path, audio);
            path
        }
```

- [ ] **Step 7: Run the test to verify it passes**

Run: `cargo test -p musefs-core --test common_corpus_smoke generate_with_ogg -- --nocapture`
Expected: PASS.

- [ ] **Step 8: Verify fmt + clippy + full smoke suite**

Run: `cargo fmt -p musefs-core && cargo clippy --all-targets -- -D warnings && cargo test -p musefs-core --test common_corpus_smoke`
Expected: clean; all smoke tests pass.

- [ ] **Step 9: Commit**

```bash
git add musefs-core/tests/common/corpus.rs musefs-core/tests/common_corpus_smoke.rs
git commit -m "$(cat <<'EOF'
test(core): wire Format::Ogg into the corpus generator

Add the Ogg variant, centralize the token<->Format mapping in
format_from_token/format_token (now shared by from_env), and emit .ogg tracks
from generate_one via write_ogg.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 3: `ALL_FORMATS` + `bench_formats`

**Files:**
- Modify: `musefs-core/tests/common/corpus.rs`
- Test: `musefs-core/tests/common_corpus_smoke.rs` (append three tests)

- [ ] **Step 1: Write the failing tests**

Append to `musefs-core/tests/common_corpus_smoke.rs`:

```rust
#[test]
fn bench_formats_defaults_to_all_when_unset() {
    let _g = ENV_LOCK.lock().unwrap();
    std::env::remove_var("MUSEFS_BENCH_FORMAT_MIX");
    assert_eq!(
        common::corpus::bench_formats(),
        common::corpus::ALL_FORMATS.to_vec()
    );
}

#[test]
fn bench_formats_filters_and_never_empty() {
    let _g = ENV_LOCK.lock().unwrap();
    std::env::set_var("MUSEFS_BENCH_FORMAT_MIX", "ogg,wav");
    let filtered = common::corpus::bench_formats();
    std::env::set_var("MUSEFS_BENCH_FORMAT_MIX", "garbagetoken");
    let fallback = common::corpus::bench_formats();
    std::env::remove_var("MUSEFS_BENCH_FORMAT_MIX");
    assert_eq!(filtered, vec![Format::Ogg, Format::Wav]);
    assert_eq!(
        fallback,
        common::corpus::ALL_FORMATS.to_vec(),
        "all-unrecognized must fall back to full coverage, never empty"
    );
}

#[test]
fn all_formats_round_trip_through_tokens() {
    for &f in common::corpus::ALL_FORMATS {
        assert_eq!(
            common::corpus::format_from_token(common::corpus::format_token(f)),
            Some(f),
            "ALL_FORMATS member {f:?} must round-trip through its token"
        );
    }
}
```

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test -p musefs-core --test common_corpus_smoke bench_formats -- --nocapture`
Expected: FAIL — `cannot find value ALL_FORMATS` / `cannot find function bench_formats` in `common::corpus`.

- [ ] **Step 3: Implement `ALL_FORMATS` and `bench_formats`**

In `musefs-core/tests/common/corpus.rs`, add after the `format_token` function:

```rust
/// Every supported format, plus the M4A moov-last layout variant (the SP1
/// bounded-read hard case). The per-format benches sweep this set.
pub const ALL_FORMATS: &[Format] = &[
    Format::Flac,
    Format::Mp3,
    Format::M4aMoovFirst,
    Format::M4aMoovLast,
    Format::Ogg,
    Format::Wav,
];

/// The formats to sweep: `MUSEFS_BENCH_FORMAT_MIX` (comma list) acts as a filter
/// when set; an unset or all-unrecognized value yields `ALL_FORMATS` (full
/// coverage). Never returns an empty vec.
pub fn bench_formats() -> Vec<Format> {
    match std::env::var("MUSEFS_BENCH_FORMAT_MIX") {
        Ok(mix) => {
            let parsed: Vec<Format> = mix.split(',').filter_map(format_from_token).collect();
            if parsed.is_empty() {
                ALL_FORMATS.to_vec()
            } else {
                parsed
            }
        }
        Err(_) => ALL_FORMATS.to_vec(),
    }
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p musefs-core --test common_corpus_smoke bench_formats -- --nocapture && cargo test -p musefs-core --test common_corpus_smoke all_formats_round_trip -- --nocapture`
Expected: PASS (all three).

- [ ] **Step 5: Verify fmt + clippy**

Run: `cargo fmt -p musefs-core && cargo clippy --all-targets -- -D warnings`
Expected: clean.

- [ ] **Step 6: Commit**

```bash
git add musefs-core/tests/common/corpus.rs musefs-core/tests/common_corpus_smoke.rs
git commit -m "$(cat <<'EOF'
test(core): add ALL_FORMATS + bench_formats sweep selector

bench_formats treats MUSEFS_BENCH_FORMAT_MIX as a filter and defaults to full
coverage; a round-trip test keeps ALL_FORMATS and the token parser in sync.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 4: `format` column on `RunReport` (+ update all constructors)

Adding a required field breaks every `RunReport { … }` literal, so this task adds the column **and** updates all four existing constructors in the same commit to keep the workspace compiling. `read_throughput.rs` does **not** construct `RunReport` (it uses Criterion), so it is untouched here.

**Files:**
- Modify: `musefs-core/tests/common/report.rs`
- Modify: `musefs-core/tests/common_corpus_smoke.rs` (the `report_renders_a_row` test)
- Modify: `musefs-core/tests/bench_ingest.rs` (two literals)
- Modify: `musefs-core/tests/bench_refresh.rs` (one literal + comment)

- [ ] **Step 1: Update the smoke test (failing first)**

In `musefs-core/tests/common_corpus_smoke.rs`, replace the `report_renders_a_row` test body's `RunReport { … }` construction and add a `format` assertion. Find:

```rust
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
```

Replace with:

```rust
    let r = RunReport {
        label: "scan".into(),
        format: "flac".into(),
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
    assert!(line.contains("flac"), "format column is rendered");
    assert!(line.contains("ci"));
    assert!(line.contains("n/a"), "fsyncs None renders as n/a");
```

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo test -p musefs-core --test common_corpus_smoke report_renders -- --nocapture`
Expected: FAIL — `missing field format in initializer of RunReport` (compile error).

- [ ] **Step 3: Add the `format` column to `RunReport`**

In `musefs-core/tests/common/report.rs`:

Change the macro's format string (add one `{:<10}` column for `format`, after `label`):

```rust
macro_rules! report_fmt {
    ($($arg:expr),* $(,)?) => {
        format!("{:<10} {:<10} {:<14} {:<10} {:>10} {:>10} {:>10} {:>10} {:>12}", $($arg),*)
    };
}
```

Add the field (after `label`):

```rust
pub struct RunReport {
    pub label: String,
    pub format: String,
    pub tier: String,
    pub storage: String,
    pub wall_ms: u128,
    pub opens: u64,
    pub preads: u64,
    pub fsyncs: Option<u64>,
    pub peak_rss_kib: Option<u64>,
}
```

Update `header()` (add `"format"` after `"label"`):

```rust
    pub fn header() -> String {
        report_fmt!(
            "label", "format", "tier", "storage", "wall_ms", "opens", "preads", "fsyncs",
            "rss_kib"
        )
    }
```

Update `row()` (add `self.format` after `self.label`):

```rust
    pub fn row(&self) -> String {
        let opt = |v: Option<u64>| v.map_or_else(|| "n/a".into(), |x| x.to_string());
        report_fmt!(
            self.label,
            self.format,
            self.tier,
            self.storage,
            self.wall_ms,
            self.opens,
            self.preads,
            opt(self.fsyncs),
            opt(self.peak_rss_kib),
        )
    }
```

- [ ] **Step 4: Update the `bench_ingest` constructors**

In `musefs-core/tests/bench_ingest.rs`, add `format: "flac".into(),` after the `label` line in **both** `RunReport { … }` literals (the `"scan"` row and the `"revalidate"` row). The corpus is FLAC-only here until Task 6 rewrites this file, so `"flac"` is accurate. Example for the scan row:

```rust
        RunReport {
            label: "scan".into(),
            format: "flac".into(),
            tier: tier.clone(),
            storage: storage.clone(),
            wall_ms: scan_ms,
            opens: s.opens,
            preads: s.preads,
            fsyncs: None,
            peak_rss_kib: peak_rss_kib(),
        }
```

Apply the same `format: "flac".into(),` insertion to the `"revalidate"` row.

- [ ] **Step 5: Update the `bench_refresh` constructor + add a clarifying comment**

In `musefs-core/tests/bench_refresh.rs`, the rows are built in a loop. Add `format: "flac".into(),` after the `label` line in that `RunReport { … }` literal. Above the `println!("\n{}", RunReport::header());` line, add:

```rust
    // bench_refresh stays FLAC-only: poll_refresh times a DB-driven virtual-tree
    // rebuild, independent of the backing audio format, so per-format rows would
    // be pure noise. The format column is fixed to "flac" for table consistency.
```

The constructor becomes:

```rust
            RunReport {
                label: label.into(),
                format: "flac".into(),
                tier: tier.clone(),
                storage: "tempfs".into(),
                wall_ms: ms,
                opens: 0,
                preads: 0,
                fsyncs: None,
                peak_rss_kib: None,
            }
```

- [ ] **Step 6: Run the smoke test + full workspace**

Run: `cargo test -p musefs-core --test common_corpus_smoke report_renders -- --nocapture`
Expected: PASS (row contains "flac").
Run: `cargo test --workspace`
Expected: green (the two `#[ignore]`d benches stay ignored; everything compiles).

- [ ] **Step 7: Verify fmt + clippy**

Run: `cargo fmt -p musefs-core && cargo clippy --all-targets -- -D warnings`
Expected: clean.

- [ ] **Step 8: Commit**

```bash
git add musefs-core/tests/common/report.rs musefs-core/tests/common_corpus_smoke.rs musefs-core/tests/bench_ingest.rs musefs-core/tests/bench_refresh.rs
git commit -m "$(cat <<'EOF'
test(core): add a format column to RunReport

New `format` field rendered via the shared report_fmt! macro (so header/row
can't drift). Existing constructors set "flac"; bench_refresh notes why it
stays FLAC-only.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 5: `prepare_format` + `bench_base_dir` helpers

**Files:**
- Modify: `musefs-core/tests/common/corpus.rs`
- Test: `musefs-core/tests/common_corpus_smoke.rs` (append one test)

- [ ] **Step 1: Write the failing test**

Append to `musefs-core/tests/common_corpus_smoke.rs`:

```rust
#[test]
fn prepare_format_generates_scannable_single_format_corpus() {
    let base = tempfile::tempdir().unwrap();
    let p = CorpusParams {
        albums: 1,
        tracks_per_album: 2,
        bytes_per_track: 256,
        // format_mix is overridden by prepare_format; set something different
        // to prove the override.
        art_bytes_per_track: 0,
        format_mix: vec![Format::Flac],
        seed: 5,
    };
    let t = common::corpus::prepare_format(&p, base.path(), Format::Ogg);
    assert!(t.corpus_dir.ends_with("ogg"), "per-format subdir named by token");
    assert_ne!(t.db_path, t.corpus_dir);
    let db = Db::open(&t.db_path).unwrap();
    let stats = scan_directory(&db, &t.corpus_dir).unwrap();
    assert_eq!(stats.scanned, 2, "two Ogg tracks generated and scanned");
}
```

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo test -p musefs-core --test common_corpus_smoke prepare_format -- --nocapture`
Expected: FAIL — `cannot find function prepare_format in module common::corpus`.

- [ ] **Step 3: Implement the helpers**

In `musefs-core/tests/common/corpus.rs`, add after the `prepare` function:

```rust
/// Resolve the base directory shared by a per-format sweep: `MUSEFS_BENCH_DIR`
/// when set (caller-managed, second element `None`), else a fresh tempdir the
/// caller must hold for the run's duration.
pub fn bench_base_dir() -> (PathBuf, Option<tempfile::TempDir>) {
    if let Ok(d) = std::env::var("MUSEFS_BENCH_DIR") {
        (PathBuf::from(d), None)
    } else {
        let s = tempfile::tempdir().unwrap();
        (s.path().to_path_buf(), Some(s))
    }
}

/// Generate a single-format corpus for `fmt` under `<base>/<token>/` with its own
/// cold DB (`musefs-bench.db` + sidecars deleted first), returning its `Target`.
/// Per-format generalization of `prepare`'s generated branch. `MUSEFS_BENCH_DB`
/// is intentionally **ignored** here: each format needs its own DB, so a single
/// shared path would clobber across formats. `p.format_mix` is overridden to the
/// single `fmt`. The base tempdir's lifetime is the caller's responsibility, so
/// `_scratch` is `None`.
pub fn prepare_format(p: &CorpusParams, base: &Path, fmt: Format) -> Target {
    let corpus_dir = base.join(format_token(fmt));
    let mut fp = p.clone();
    fp.format_mix = vec![fmt];
    generate(&corpus_dir, &fp);
    let db_path = corpus_dir.join("musefs-bench.db");
    for suffix in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{suffix}", db_path.display()));
    }
    Target {
        corpus_dir,
        db_path,
        is_real_library: false,
        _scratch: None,
    }
}
```

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test -p musefs-core --test common_corpus_smoke prepare_format -- --nocapture`
Expected: PASS.

- [ ] **Step 5: Verify fmt + clippy**

Run: `cargo fmt -p musefs-core && cargo clippy --all-targets -- -D warnings`
Expected: clean.

- [ ] **Step 6: Commit**

```bash
git add musefs-core/tests/common/corpus.rs musefs-core/tests/common_corpus_smoke.rs
git commit -m "$(cat <<'EOF'
test(core): add prepare_format + bench_base_dir for the per-format sweep

prepare_format generates a single-format corpus under <base>/<token>/ with its
own cold DB (MUSEFS_BENCH_DB ignored in sweep mode); bench_base_dir resolves the
shared base dir once.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 6: `bench_ingest` per-format sweep

Rewrite the harness to loop `bench_formats()` in generated mode and collapse to a single `"mixed"` scan in real-library mode.

**Files:**
- Rewrite: `musefs-core/tests/bench_ingest.rs`

- [ ] **Step 1: Replace the file**

Replace the entire contents of `musefs-core/tests/bench_ingest.rs` with:

```rust
mod common;

use std::time::Instant;

use common::corpus::{bench_base_dir, bench_formats, format_token, prepare, prepare_format, CorpusParams, Target};
use common::report::{peak_rss_kib, RunReport};
use musefs_core::{metrics, revalidate, scan_directory};
use musefs_db::Db;

/// Scan + revalidate one resolved target, printing a `scan` and a `revalidate`
/// row tagged with `format`/`storage`.
///
/// The `opens`/`preads` metrics instrument the *serve* path (reader.rs /
/// open_handle), not the scan path, so both rows print ~0 even under
/// `--features metrics`. The SP1-relevant signals are `wall_ms` and
/// `peak_rss_kib`. `peak_rss_kib()` reads VmHWM — a process-lifetime high-water
/// mark — so every row reflects the same peak; the meaningful figure is the
/// largest corpus's scan row.
fn run_one(target: &Target, tier: &str, format: &str, storage: &str) {
    let db = Db::open(&target.db_path).unwrap();

    metrics::reset();
    let t0 = Instant::now();
    let stats = scan_directory(&db, &target.corpus_dir).unwrap();
    let scan_ms = t0.elapsed().as_millis();
    let s = metrics::snapshot();

    metrics::reset();
    let t1 = Instant::now();
    let _ = revalidate(&db, &target.corpus_dir).unwrap();
    let reval_ms = t1.elapsed().as_millis();
    let r = metrics::snapshot();

    for (label, ms, snap) in [("scan", scan_ms, &s), ("revalidate", reval_ms, &r)] {
        println!(
            "{}",
            RunReport {
                label: label.into(),
                format: format.into(),
                tier: tier.into(),
                storage: storage.into(),
                wall_ms: ms,
                opens: snap.opens,
                preads: snap.preads,
                fsyncs: None,
                peak_rss_kib: peak_rss_kib(),
            }
            .row()
        );
    }
    assert!(stats.scanned > 0, "format {format}: scanned 0 tracks");
}

#[test]
#[ignore = "SP0 timing harness; run with --ignored --nocapture"]
fn bench_cold_scan_and_revalidate() {
    let params = CorpusParams::from_env();
    let tier = std::env::var("MUSEFS_BENCH_TIER").unwrap_or_else(|_| "ci".into());

    println!("\n{}", RunReport::header());

    // Real library: already mixed-format and never written to — a single scan
    // tagged "mixed" rather than a per-format sweep.
    if std::env::var("MUSEFS_BENCH_LIBRARY").is_ok() {
        let target = prepare(&params);
        run_one(&target, &tier, "mixed", "real-lib");
        return;
    }

    // Generated mode: one single-format corpus + cold DB per format under a
    // shared base dir (held for the loop's duration).
    let (base, _scratch) = bench_base_dir();
    let storage = if std::env::var("MUSEFS_BENCH_DIR").is_ok() {
        "env-dir"
    } else {
        "tempfs"
    };
    for fmt in bench_formats() {
        let target = prepare_format(&params, &base, fmt);
        run_one(&target, &tier, format_token(fmt), storage);
    }
}
```

- [ ] **Step 2: Run the sweep (ci tier, with metrics) to verify it reports per-format rows**

Run: `cargo test -p musefs-core --features metrics --test bench_ingest -- --ignored --nocapture`
Expected: PASS. Prints a header then a `scan` + `revalidate` row for each of the six formats (`flac`, `mp3`, `m4a`, `m4a-last`, `ogg`, `wav`), each with `scanned > 0` (the assert holds). `wall_ms`/`rss_kib` populated; `opens`/`preads` ~0.

- [ ] **Step 3: Confirm it stays ignored in a normal run**

Run: `cargo test -p musefs-core --test bench_ingest`
Expected: `0 passed; … 1 ignored`.

- [ ] **Step 4: Confirm the filter works**

Run: `MUSEFS_BENCH_FORMAT_MIX=ogg,wav cargo test -p musefs-core --features metrics --test bench_ingest -- --ignored --nocapture`
Expected: only `ogg` and `wav` row-pairs are printed.

- [ ] **Step 5: Verify fmt + clippy + full workspace**

Run: `cargo fmt -p musefs-core && cargo clippy --all-targets -- -D warnings && cargo test --workspace`
Expected: clean; green.

- [ ] **Step 6: Commit**

```bash
git add musefs-core/tests/bench_ingest.rs
git commit -m "$(cat <<'EOF'
test(core): sweep bench_ingest over every format

Generated mode loops bench_formats(), generating a single-format corpus + cold
DB per format under one base dir and emitting per-format scan/revalidate rows;
real-library mode collapses to a single "mixed" scan.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 7: `read_throughput` per-format groups + generic inode walk

**Files:**
- Modify: `musefs-core/benches/read_throughput.rs`

- [ ] **Step 1: Update the corpus import**

In `musefs-core/benches/read_throughput.rs`, change:

```rust
use common::corpus::{generate, CorpusParams, Format};
```

to:

```rust
use common::corpus::{bench_formats, format_token, generate, CorpusParams, Format};
```

- [ ] **Step 2: Add a generic inode walk and make `fixture` format-aware**

Replace the entire current `fixture` function (the one with the doc comment "A small generated corpus … Returns the fs plus all file inodes." and the hardcoded `fs.lookup(VirtualTree::ROOT, "Artist 00000")` walk) with:

```rust
/// Recursively collect every non-directory inode reachable from `dir`. Used
/// instead of a name-based lookup because non-FLAC corpus builders embed no tags,
/// so their tracks render under the `default_fallback` ("Unknown/…") path.
fn collect_file_inodes(fs: &Musefs, dir: u64, out: &mut Vec<u64>) {
    for (_, ino, is_dir) in fs.readdir(dir).unwrap() {
        if is_dir {
            collect_file_inodes(fs, ino, out);
        } else {
            out.push(ino);
        }
    }
}

/// A small single-format generated corpus, scanned into an in-memory DB and
/// mounted. Returns the fs plus all file inodes (discovered by a format-agnostic
/// tree walk).
fn fixture(format: Format, bytes_per_track: usize, tracks: usize) -> (Arc<Musefs>, Vec<u64>) {
    let p = CorpusParams {
        albums: 1,
        tracks_per_album: tracks,
        bytes_per_track,
        art_bytes_per_track: 0,
        format_mix: vec![format],
        seed: 42,
    };
    let dir = tempfile::tempdir().unwrap();
    generate(dir.path(), &p);
    let db = musefs_db::Db::open_in_memory().unwrap();
    scan_directory(&db, dir.path()).unwrap();
    let fs = Arc::new(Musefs::open(db, config()).unwrap());

    let mut inodes = Vec::new();
    collect_file_inodes(&fs, VirtualTree::ROOT, &mut inodes);
    assert!(!inodes.is_empty(), "fixture: no file inodes for {format:?}");
    // Keep the tempdir alive for the duration of the bench by leaking it.
    std::mem::forget(dir);
    (fs, inodes)
}
```

- [ ] **Step 3: Make `bench_sequential_read` loop the formats**

Replace the entire current `bench_sequential_read` function with:

```rust
fn bench_sequential_read(c: &mut Criterion) {
    let mut group = c.benchmark_group("sequential_read");
    let chunk = 128 * 1024u64;
    for fmt in bench_formats() {
        let (fs, inodes) = fixture(fmt, 4 * 1024 * 1024, 1);
        let inode = inodes[0];
        let size = fs.getattr(inode).unwrap().size;
        group.throughput(Throughput::Bytes(size));
        // fh=0 takes the no-handle path: each read resolves the inode via the
        // HeaderCache rather than reusing a registered fd.
        group.bench_function(format_token(fmt), |b| {
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
    }
    group.finish();
}
```

- [ ] **Step 4: Point the concurrent bench's fixture at FLAC**

In `bench_concurrent_read_and_walk`, change the fixture call:

```rust
    let (fs, inodes) = fixture(1024 * 1024, m.max(2));
```

to:

```rust
    let (fs, inodes) = fixture(Format::Flac, 1024 * 1024, m.max(2));
```

- [ ] **Step 5: Smoke-run the bench (both groups, all formats)**

Run: `cargo bench -p musefs-core --bench read_throughput -- --warm-up-time 1 --measurement-time 1`
Expected: `sequential_read/{flac,mp3,m4a,m4a-last,ogg,wav}` each report a throughput line (no panic; the `!inodes.is_empty()` assert holds for every format including the non-FLAC ones that render under `Unknown/…`), and `concurrent_read_walk/mN_plus_walker` reports a line.

- [ ] **Step 6: Confirm the workspace still builds/tests clean**

Run: `cargo fmt -p musefs-core && cargo clippy --all-targets -- -D warnings && cargo test -p musefs-core`
Expected: clean; all existing tests pass; benches stay ignored in the default test run.

- [ ] **Step 7: Commit**

```bash
git add musefs-core/benches/read_throughput.rs
git commit -m "$(cat <<'EOF'
bench(core): sweep sequential read over every format

fixture takes a Format and discovers inodes via a generic tree walk (non-FLAC
corpora render under Unknown/...); sequential_read emits one Criterion function
per format. Concurrent read+walk stays FLAC-only (SP3 contention focus).

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 8: Document the per-format sweep in the tracking README

**Files:**
- Modify: `docs/superpowers/specs/2026-05-30-optimization-pass/README.md`

- [ ] **Step 1: Update the "Running the SP0a harness" notes**

In `docs/superpowers/specs/2026-05-30-optimization-pass/README.md`, find the "Notes:" list under the "## Running the SP0a harness" section and insert these two bullets at the top of that list (before the existing "A reused `MUSEFS_BENCH_DIR`…" bullet):

```markdown
- **Per-format sweep:** `bench_ingest` and the `read_throughput` sequential bench
  run against every supported format (FLAC, MP3, M4A moov-first, M4A moov-last,
  Ogg, WAV) by default, one report row / Criterion line per format (see the
  `format` column). `bench_refresh` stays FLAC-only (it times a format-independent
  DB-driven tree rebuild).
- `MUSEFS_BENCH_FORMAT_MIX` (comma list of `flac,mp3,m4a,m4a-last,ogg,wav`)
  restricts the sweep to those formats; unset = all. In a `bench_ingest` sweep,
  `MUSEFS_BENCH_DB` is ignored (each format gets its own DB under a per-format
  subdir); a real `MUSEFS_BENCH_LIBRARY` run does a single `mixed` scan instead of
  sweeping.
```

- [ ] **Step 2: Update the SP0a status-row note**

In the Status table, change the SP0a row's Notes cell (currently ending `… See "Running the SP0a harness" below`) to append `; per-format sweep added (SP0a-per-format-coverage.md)`.

- [ ] **Step 3: Verify the commit hook (markdown only, but the hook runs the suite)**

Run: `cargo test --workspace`
Expected: green (no code changed; sanity check before commit).

- [ ] **Step 4: Commit**

```bash
git add docs/superpowers/specs/2026-05-30-optimization-pass/README.md
git commit -m "$(cat <<'EOF'
docs: record the per-format bench sweep

Note the default per-format coverage, the format column, MUSEFS_BENCH_FORMAT_MIX
as a filter, the per-format DB behavior, and the real-library "mixed" collapse.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Self-review notes (for the executor)

- **Spec coverage:** `write_ogg` + `Format::Ogg` wiring (Tasks 1–2); `format` column (Task 4); `ALL_FORMATS`/`bench_formats` with filter + never-empty semantics (Task 3); `bench_ingest` per-format sweep with real-library `"mixed"` collapse and per-format DBs (Tasks 5–6); per-format `read_throughput` with a format-agnostic inode walk + `≥1 inode` assert (Task 7); `bench_refresh` stays FLAC-only with a comment (Task 4); determinism + scan + token-consistency tests (Tasks 1–3, 5); docs (Task 8).
- **Non-goals preserved:** no Ogg art, Opus-only (no OggFLAC), no production-code changes, no new Cargo features, no tag embedding in non-FLAC builders.
- **Always-green ordering:** Task 4 adds the required `format` field and updates *all* existing `RunReport` literals in the same commit, so `cargo test --workspace` stays green at every step; Task 6 then rewrites `bench_ingest` (its temporary `"flac"` literals from Task 4 are replaced by the looped `format_token(fmt)`).
- **Type consistency:** `format_from_token(&str) -> Option<Format>`, `format_token(Format) -> &'static str`, `ALL_FORMATS: &[Format]`, `bench_formats() -> Vec<Format>`, `bench_base_dir() -> (PathBuf, Option<TempDir>)`, `prepare_format(&CorpusParams, &Path, Format) -> Target`, and `RunReport.format: String` are used identically across tasks. `Snapshot` fields `opens`/`preads` and `Musefs::{readdir -> Vec<(String,u64,bool)>, getattr().size, read(inode,fh,off,len)}` match the existing code.
