# Serve-Path Metrics Implementation Plan (Phase 5 — issues #71, #76)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Instrument the Ogg serve path's backing reads with the existing `preads`/`pread_bytes` counters (#71), and lock every serve-path counter call site with tests that drive real reads — run by CI under `--features metrics` (#76).

**Architecture:** A `read_counted` helper in `musefs-core/src/ogg_index.rs` wraps `read_exact_at` with attempt-based `metrics::on_pread` accounting at four of five backing-read sites (the fifth, a `read_at`, gets an inline count). New integration tests in `musefs-core/tests/metrics.rs` read whole synthesized files through `Musefs` and assert the targeted counter moved; new fixture builders live in `musefs-core/tests/common/mod.rs`. One new CI step runs the metrics-feature test suite. Spec: `docs/superpowers/specs/2026-06-03-serve-path-metrics-design.md`.

**Tech Stack:** Rust (stable), `cargo test --features metrics`, `base64` 0.22 (dev-dep only), GitHub Actions, `musefs-latencyfs` for the final bench refresh.

**Branch:** `phase5-serve-path-metrics` (already exists; spec committed there).

**Conventions that apply to every task:**
- Every test in `musefs-core/tests/metrics.rs` MUST take the `METRICS_LOCK` guard first (the counters are global statics; parallel tests corrupt each other) and call `metrics::reset()` before the measured section. Follow the existing tests in that file.
- All metrics assertions are *increments* (`> 0`), never exact totals — Ogg's backward scan makes exact pread counts fixture-dependent.
- Test commands need the feature flag: `cargo test -p musefs-core --features metrics`. Without it the counters compile to empty inline fns and every snapshot is zero.

---

### Task 1: Fixture builders in tests/common

**Files:**
- Modify: `musefs-core/Cargo.toml` (add `base64` to `[dev-dependencies]`)
- Modify: `musefs-core/tests/common/mod.rs` (three new helpers, appended after `write_ogg`)

- [ ] **Step 1: Add the base64 dev-dependency**

In `musefs-core/Cargo.toml`, `[dev-dependencies]` section, add (musefs-format already uses base64 0.22, so this adds no new crate to the tree):

```toml
base64 = "0.22"
```

- [ ] **Step 2: Add the three fixture helpers**

Append to `musefs-core/tests/common/mod.rs` (after `write_ogg`). The file is `#![allow(dead_code)]`, so helpers may land before their consuming tests. Mirror of the FLAC PICTURE body format parsed by `musefs-format/src/flac.rs::parse_picture_block` (all fields big-endian):

```rust
/// A FLAC PICTURE block body (type 3 = front cover, image/png) carrying `data`.
/// The identical bytes serve three fixtures: a native FLAC PICTURE block, the
/// base64 payload of an Opus/Vorbis `METADATA_BLOCK_PICTURE` comment, and an
/// OggFLAC native PICTURE packet body. `data` must be non-empty: FLAC synthesis
/// only emits an `ArtImage` segment for `data_len > 0`.
pub fn picture_block_body(data: &[u8]) -> Vec<u8> {
    let mut v = Vec::new();
    v.extend_from_slice(&3u32.to_be_bytes()); // picture type: front cover
    v.extend_from_slice(&(b"image/png".len() as u32).to_be_bytes());
    v.extend_from_slice(b"image/png");
    v.extend_from_slice(&0u32.to_be_bytes()); // empty description
    v.extend_from_slice(&1u32.to_be_bytes()); // width
    v.extend_from_slice(&1u32.to_be_bytes()); // height
    v.extend_from_slice(&0u32.to_be_bytes()); // depth
    v.extend_from_slice(&0u32.to_be_bytes()); // colors
    v.extend_from_slice(&(data.len() as u32).to_be_bytes());
    v.extend_from_slice(data);
    v
}

/// Write an Opus file whose `OpusTags` packet carries `comments` plus a base64
/// `METADATA_BLOCK_PICTURE` of `picture` (a PICTURE block body, e.g. from
/// `picture_block_body`), returning (audio_offset, audio_length). Same page
/// recipe as `write_ogg`.
pub fn write_opus_with_art(
    path: &Path,
    comments: &[&str],
    picture: &[u8],
    audio: &[u8],
) -> (i64, i64) {
    use base64::Engine as _;
    use musefs_format::ogg::page_test_support::{build_header_pub, lace_packet_pub};
    let head = b"OpusHead\x01\x02\x38\x01\x80\xbb\x00\x00\x00\x00\x00".to_vec();
    let mbp = format!(
        "METADATA_BLOCK_PICTURE={}",
        base64::engine::general_purpose::STANDARD.encode(picture)
    );
    let mut all: Vec<&str> = comments.to_vec();
    all.push(&mbp);
    let mut tags = b"OpusTags".to_vec();
    tags.extend_from_slice(&vorbis_comment_body("v", &all));
    let serial = 0x6d75_7366; // "musf"
    let (mut bytes, header_pages) = build_header_pub(serial, &[&head, &tags]);
    let header_len = bytes.len();
    let (page, _) = lace_packet_pub(serial, header_pages, false, 960, audio);
    bytes.extend_from_slice(&page);
    std::fs::write(path, &bytes).unwrap();
    (header_len as i64, (bytes.len() - header_len) as i64)
}

/// Write an OggFLAC file (`0x7F "FLAC"` 1.0 mapping) whose header packets carry
/// a VORBIS_COMMENT block with `comments` and a native PICTURE block with
/// `picture` (a PICTURE block body), returning (audio_offset, audio_length).
/// Packet 0 is `0x7F "FLAC" major minor count(u16 BE) "fLaC" STREAMINFO`; the
/// count is the number of metadata-block packets that follow.
pub fn write_oggflac_with_art(
    path: &Path,
    comments: &[&str],
    picture: &[u8],
    audio: &[u8],
) -> (i64, i64) {
    use musefs_format::ogg::page_test_support::{build_header_pub, lace_packet_pub};
    let mut pkt0 = vec![0x7F];
    pkt0.extend_from_slice(b"FLAC");
    pkt0.extend_from_slice(&[1, 0]); // mapping version 1.0
    pkt0.extend_from_slice(&2u16.to_be_bytes()); // two metadata packets follow
    pkt0.extend_from_slice(b"fLaC");
    pkt0.extend_from_slice(&flac_block(0, &streaminfo_body(), false));
    let vc_pkt = flac_block(4, &vorbis_comment_body("v", comments), false);
    let pic_pkt = flac_block(6, picture, true);
    let serial = 0x6f67_666c;
    let (mut bytes, header_pages) = build_header_pub(serial, &[&pkt0, &vc_pkt, &pic_pkt]);
    let header_len = bytes.len();
    let (page, _) = lace_packet_pub(serial, header_pages, false, 960, audio);
    bytes.extend_from_slice(&page);
    std::fs::write(path, &bytes).unwrap();
    (header_len as i64, (bytes.len() - header_len) as i64)
}
```

- [ ] **Step 3: Verify it compiles**

Run: `cargo test -p musefs-core --features metrics --no-run`
Expected: compiles cleanly (helpers are not yet called; `#![allow(dead_code)]` covers them).

- [ ] **Step 4: Commit**

```bash
git add musefs-core/Cargo.toml musefs-core/tests/common/mod.rs Cargo.lock
git commit -m "test: fixture builders for PICTURE-bearing FLAC/Opus/OggFLAC files

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 2: Ogg pread instrumentation (#71)

**Files:**
- Modify: `musefs-core/tests/metrics.rs` (shared `read_all_and_snapshot` helper + failing test)
- Modify: `musefs-core/src/ogg_index.rs` (the `read_counted` helper + 5 read sites)
- Modify: `musefs-core/src/metrics.rs` (module doc rewrite only)
- Modify: `.cargo/mutants.toml` (re-anchor three line:col exclusions shifted by the edit)

- [ ] **Step 1: Write the shared read helper and the failing test**

`musefs-core/tests/metrics.rs` currently imports three helpers; replace that
line with:
`use common::{make_flac, streaminfo_body, vorbis_comment_body, write_ogg};`
(`write_ogg` is NOT in the current import. Import each later helper in the
task that first uses it — an early unused import fails the feature clippy
gate in Task 6.) Then add (after the `config()` fn):

```rust
/// Scan `dir`, mount, read the single track end-to-end in 16 KiB chunks under
/// template `$artist/$title`, and return the metrics snapshot for those reads.
/// Caller must hold `METRICS_LOCK`.
fn read_all_and_snapshot(dir: &std::path::Path, artist_dir: &str) -> metrics::Snapshot {
    let db = musefs_db::Db::open_in_memory().unwrap();
    scan_directory(&db, dir).unwrap();
    let fs = Musefs::open(db, config()).unwrap();
    let parent = fs.lookup(VirtualTree::ROOT, artist_dir).unwrap();
    let (_, inode, _) = fs.readdir(parent).unwrap().into_iter().next().unwrap();
    let size = fs.getattr(inode).unwrap().size;
    metrics::reset();
    let mut off = 0u64;
    while off < size {
        let got = fs.read(inode, 0, off, 16 * 1024).unwrap();
        if got.is_empty() {
            break;
        }
        off += got.len() as u64;
    }
    metrics::snapshot()
}
```

And the test (note: `write_ogg` writes no tags, so `$artist`/`$title` both fall back to `Unknown`):

```rust
#[test]
fn ogg_serve_counts_backing_preads() {
    let _guard = METRICS_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let dir = tempfile::tempdir().unwrap();
    write_ogg(&dir.path().join("a.ogg"), &vec![0xAB_u8; 8 * 1024]);

    let s = read_all_and_snapshot(dir.path(), "Unknown");
    assert!(s.preads > 0, "Ogg serve must count backing preads, got 0");
    assert!(
        s.pread_bytes > 0,
        "Ogg serve must count backing bytes read, got 0"
    );
}
```

- [ ] **Step 2: Run the test — verify it fails**

Run: `cargo test -p musefs-core --features metrics --test metrics ogg_serve_counts_backing_preads`
Expected: FAIL with `Ogg serve must count backing preads, got 0` (issue #71 reproduced).

- [ ] **Step 3: Add `read_counted` and instrument the five read sites**

In `musefs-core/src/ogg_index.rs`, insert after the `MAX_OGG_HEADER_BYTES` const:

```rust
/// Positioned read that records serve-path pread metrics (count + bytes).
/// Counts on the attempt, like `on_open` — a failed read is still a round-trip.
fn read_counted(f: &std::fs::File, buf: &mut [u8], offset: u64) -> std::io::Result<()> {
    crate::metrics::on_pread(buf.len() as u64);
    f.read_exact_at(buf, offset)
}
```

Then switch the five backing-read sites (these are ALL the positioned reads in this file's non-test code; verify with `grep -n "read_exact_at\|read_at" musefs-core/src/ogg_index.rs`):

1. `find_page_start`, the backward-scan window read:
```rust
// before:
    backing.read_exact_at(&mut window, scan_start)?;
// after:
    read_counted(backing, &mut window, scan_start)?;
```

2. `page_crc_ok`, the header probe — this is a `read_at` (tolerated short read at EOF, returns a byte count), so it cannot use the `read_exact_at`-shaped helper; count inline, attempt-based, before the read:
```rust
// before:
    let mut head = vec![0u8; MAX_OGG_HEADER_BYTES];
    let n = backing.read_at(&mut head, page_start)?;
// after:
    let mut head = vec![0u8; MAX_OGG_HEADER_BYTES];
    crate::metrics::on_pread(head.len() as u64);
    let n = backing.read_at(&mut head, page_start)?;
```

3. `page_crc_ok`, the full-page CRC read (the `.is_err()` tolerate-and-continue handling is unchanged):
```rust
// before:
    if backing.read_exact_at(&mut page, page_start).is_err() {
// after:
    if read_counted(backing, &mut page, page_start).is_err() {
```

4. `serve_ogg_window`, the per-page header read:
```rust
// before:
        backing.read_exact_at(&mut hdr_buf, pos)?;
// after:
        read_counted(backing, &mut hdr_buf, pos)?;
```

5. `serve_ogg_window`, the payload read:
```rust
// before:
            backing.read_exact_at(&mut out[start..], pos + header_len as u64 + within)?;
// after:
            read_counted(backing, &mut out[start..], pos + header_len as u64 + within)?;
```

- [ ] **Step 4: Rewrite the metrics module doc**

Replace the module doc comment at the top of `musefs-core/src/metrics.rs` (the current lines 1–13, which document the Ogg blind spot as a known limitation — replace ONLY the `//!` block; the blank line and `pub use imp::*;` after it stay) with:

```rust
//! Optional syscall/query counters and per-syscall latency injection for
//! benchmarking. Zero-cost when the `metrics` feature is off: every hook
//! compiles to an empty inline fn, so call sites stay unconditional and clean.
//!
//! Counting scope: `on_open`/`on_stat` count every backing-file open and
//! metadata syscall on any read path; `on_open` fires on the open *attempt*
//! (a failed open is still a syscall). `on_pread` counts positioned backing
//! reads on the serve path, attempt-based: one pread plus the attempted
//! buffer length, recorded before the read (a failed or short read is still
//! a round-trip, and the `MUSEFS_FAULT_PREAD_US` injection applies to it).
//! For `BackingAudio` segments, bytes attempted equal bytes served; on the
//! Ogg path (page-index scans, CRC probes, header and payload reads) bytes
//! attempted may exceed bytes served, because scan and header bytes are
//! patched or discarded — the counter reports backing I/O performed, not
//! output produced. Art-blob and binary-tag chunks are DB reads, tracked by
//! call count (`on_art_chunk`/`on_binary_tag_chunk`), not byte-counted.
//! `on_scan_open`/`on_scan_read` count backing-file opens and positioned
//! reads on the *scan* path (distinct from the serve path); `on_scan_read`
//! also accumulates bytes read, analogous to `on_pread`.
```

- [ ] **Step 5: Run the test — verify it passes, with no collateral damage**

Run: `cargo test -p musefs-core --features metrics --test metrics`
Expected: ALL tests in the file PASS (the new one and the pre-existing FLAC expectations, which the Ogg-only change must not disturb).

Run: `cargo test -p musefs-core` (default features — exercises the `ogg_index` unit tests with the empty-fn metrics shim)
Expected: PASS.

- [ ] **Step 6: Re-anchor the mutants.toml line:col exclusions**

`.cargo/mutants.toml` contains FIVE `ogg_index.rs` exclusions; two are
line-agnostic (`\d+:\d+` — the `-= with /=` and `+= with *=` entries) and
need NO change. Only the three pinned by literal line:col need re-anchoring,
because Step 3 added lines above them:

```
'musefs-core/src/ogg_index\.rs:194:41: replace \+ with \* in serve_ogg_window',
'musefs-core/src/ogg_index\.rs:204:15: replace < with <= in serve_ogg_window',
'musefs-core/src/ogg_index\.rs:213:15: replace < with <= in serve_ogg_window',
```

First run `cargo fmt --all` (anchors are fmt-fragile; fmt BEFORE recomputing). Then find the new line numbers:

```bash
grep -n "header_len + payload_len) as u64\|if hs < he\|if ps < pe" musefs-core/src/ogg_index.rs
```

- `194:41` → the `let total_len = (header_len + payload_len) as u64;` line (column stays 41 if the line content/indent is unchanged)
- `204:15` → the `if hs < he {` line (column stays 15)
- `213:15` → the `if ps < pe {` line (column stays 15)

Update the three regexes with the new line numbers. Sanity-check columns only if the lines themselves were rewrapped by fmt (count: column = 1-based byte offset of the operator/comparison).

- [ ] **Step 7: Commit**

```bash
git add musefs-core/src/ogg_index.rs musefs-core/src/metrics.rs musefs-core/tests/metrics.rs .cargo/mutants.toml
git commit -m "feat: count Ogg serve-path backing reads in pread metrics (#71)

All five positioned reads (index scan, CRC probes, header, payload)
count attempt-based preads/bytes, so latency injection and the read
bench now cover Ogg. mutants.toml ogg_index anchors re-pinned.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 3: FLAC serve-site counter tests (#76 — ArtImage, BinaryTag)

**Files:**
- Modify: `musefs-core/tests/metrics.rs` (two tests; uses Task 2's `read_all_and_snapshot`)

- [ ] **Step 1: Write the two tests**

Add `picture_block_body` to the `common` import in `musefs-core/tests/metrics.rs`, then:

```rust
#[test]
fn flac_art_serve_increments_art_chunks() {
    let _guard = METRICS_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let dir = tempfile::tempdir().unwrap();
    let flac = make_flac(
        &[
            (0, streaminfo_body()),
            (6, picture_block_body(&[0x89_u8; 256])), // PICTURE -> Segment::ArtImage
            (4, vorbis_comment_body("v", &["ARTIST=Alice", "TITLE=Song"])),
        ],
        &vec![0xCD_u8; 16 * 1024],
    );
    std::fs::write(dir.path().join("a.flac"), &flac).unwrap();

    let s = read_all_and_snapshot(dir.path(), "Alice");
    assert!(
        s.art_chunks > 0,
        "serving Segment::ArtImage must increment art_chunks"
    );
}

#[test]
fn flac_binary_tag_serve_increments_binary_tag_chunks() {
    let _guard = METRICS_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let dir = tempfile::tempdir().unwrap();
    let flac = make_flac(
        &[
            (0, streaminfo_body()),
            (2, b"testAPPDATA".to_vec()), // APPLICATION -> Segment::BinaryTag
            (4, vorbis_comment_body("v", &["ARTIST=Alice", "TITLE=Song"])),
        ],
        &vec![0xCD_u8; 16 * 1024],
    );
    std::fs::write(dir.path().join("a.flac"), &flac).unwrap();

    let s = read_all_and_snapshot(dir.path(), "Alice");
    assert!(
        s.binary_tag_chunks > 0,
        "serving Segment::BinaryTag must increment binary_tag_chunks"
    );
}
```

- [ ] **Step 2: Run them — verify they pass**

Run: `cargo test -p musefs-core --features metrics --test metrics flac_art_serve flac_binary_tag_serve`
Expected: both PASS (the increments exist in `reader.rs` today; these tests are the regression lock #76 asks for).

- [ ] **Step 3: Verify each test bites (manual mutation check)**

These tests guard against a *dropped* increment — the exact bug that shipped once. Prove they would catch it:

1. In `musefs-core/src/reader.rs`, comment out `crate::metrics::on_art_chunk();` in the `Segment::ArtImage` arm (the one directly after `db.read_art_chunk(*art_id, within, n)?;` in the `ArtImage` match arm — NOT the two in the `OggArtSlice` arm).
2. Run: `cargo test -p musefs-core --features metrics --test metrics flac_art_serve`
   Expected: FAIL (`art_chunks` stayed 0).
3. Restore the line. Repeat for `crate::metrics::on_binary_tag_chunk();` in the `Segment::BinaryTag` arm against `flac_binary_tag_serve`.
4. Restore everything; run Step 2's command again.
   Expected: both PASS. `git diff musefs-core/src/reader.rs` must be empty.

- [ ] **Step 4: Commit**

```bash
git add musefs-core/tests/metrics.rs
git commit -m "test: lock ArtImage/BinaryTag serve arms to their counters (#76)

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 4: Ogg art serve-site counter tests (#76 — both OggArtSlice arms)

**Files:**
- Modify: `musefs-core/tests/metrics.rs` (two tests)

- [ ] **Step 1: Write the two tests**

Add `write_opus_with_art` and `write_oggflac_with_art` to the `common` import in `musefs-core/tests/metrics.rs`, then:

```rust
#[test]
fn opus_base64_art_serve_increments_art_chunks() {
    let _guard = METRICS_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let dir = tempfile::tempdir().unwrap();
    write_opus_with_art(
        &dir.path().join("a.opus"),
        &["ARTIST=Alice", "TITLE=Song"],
        &picture_block_body(&[0x89_u8; 256]),
        &vec![0xAB_u8; 8 * 1024],
    );

    let s = read_all_and_snapshot(dir.path(), "Alice");
    assert!(
        s.art_chunks > 0,
        "serving OggArtSlice (base64 METADATA_BLOCK_PICTURE) must increment art_chunks"
    );
}

#[test]
fn oggflac_raw_art_serve_increments_art_chunks() {
    let _guard = METRICS_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let dir = tempfile::tempdir().unwrap();
    write_oggflac_with_art(
        &dir.path().join("a.ogg"),
        &["ARTIST=Alice", "TITLE=Song"],
        &picture_block_body(&[0x89_u8; 256]),
        &vec![0xAB_u8; 8 * 1024],
    );

    let s = read_all_and_snapshot(dir.path(), "Alice");
    assert!(
        s.art_chunks > 0,
        "serving OggArtSlice (raw OggFLAC PICTURE) must increment art_chunks"
    );
}
```

- [ ] **Step 2: Run them — verify they pass**

Run: `cargo test -p musefs-core --features metrics --test metrics opus_base64_art oggflac_raw_art`
Expected: both PASS.

If the OggFLAC test fails at the `scan_directory`/`lookup` stage instead of the assertion, the fixture didn't survive probing — debug the fixture (most likely suspect: packet-0 layout vs `musefs-format/src/ogg/mod.rs::oggflac_following_packets`, which reads the u16 BE count at bytes 7..9) before touching any production code. Use the superpowers:systematic-debugging skill.

- [ ] **Step 3: Verify each test bites (manual mutation check)**

The `Segment::OggArtSlice` arm in `musefs-core/src/reader.rs` has TWO `crate::metrics::on_art_chunk();` calls — one in the `if *base64` branch, one in the `else` (raw) branch:

1. Comment out the one in the `if *base64` branch. Run the Opus test → expected FAIL; run the OggFLAC test → expected PASS (proves the tests discriminate between the branches). Restore.
2. Comment out the one in the `else` branch. Run the OggFLAC test → expected FAIL. Restore.
3. Run Step 2's command again → both PASS. `git diff musefs-core/src/reader.rs` must be empty.

- [ ] **Step 4: Run the full metrics suite and commit**

Run: `cargo test -p musefs-core --features metrics`
Expected: PASS.

```bash
git add musefs-core/tests/metrics.rs
git commit -m "test: lock both OggArtSlice serve arms to art_chunks (#76)

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 5: CI runs the metrics-feature tests

**Files:**
- Modify: `.github/workflows/ci.yml` (`check` job, one step)

- [ ] **Step 1: Add the step**

In `.github/workflows/ci.yml`, `check` job, directly after the "DB mutants-feature tests" step (mirroring the adjacent feature-step pattern):

```yaml
      - name: Core metrics-feature tests
        run: cargo test -p musefs-core --features metrics
```

- [ ] **Step 2: Verify the exact command locally**

Run: `cargo test -p musefs-core --features metrics`
Expected: PASS (this is precisely what CI will run; no `#[ignore]`d tests are included, so no `/dev/fuse` needed in CI).

- [ ] **Step 3: Commit**

```bash
git add .github/workflows/ci.yml
git commit -m "ci: run musefs-core metrics-feature tests in the check job (#76)

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 6: Full local validation

**Files:** none (verification only)

- [ ] **Step 1: Format and lint**

```bash
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo clippy -p musefs-core --features metrics --all-targets -- -D warnings
```
Expected: all clean. Check each command's exit status directly (CI has a hard fmt gate).

- [ ] **Step 2: Test everything**

```bash
cargo test --workspace
cargo test -p musefs-core --features metrics
```
Expected: PASS.

- [ ] **Step 3: Run the in-diff mutation gate locally**

This PR changes `ogg_index.rs` production code; mirror CI's gate (TMPDIR under /home, parallel jobs):

```bash
mkdir -p ~/tmp/mutants
git diff "$(git merge-base main HEAD)...HEAD" -- '*.rs' > /tmp/mutants.diff
wc -l /tmp/mutants.diff && grep -c "ogg_index" /tmp/mutants.diff
TMPDIR=~/tmp/mutants cargo mutants --in-diff /tmp/mutants.diff -j"$(nproc)" --exclude 'musefs-latencyfs/**'
```

Sanity-check the diff first: it must be non-empty and mention `ogg_index.rs`, or the gate is a silent false pass. Expected: no MISSED mutants in the changed lines (note: `read_counted`'s `on_pread` call is invisible to the gate — it builds without the feature — but its `read_exact_at` body is covered by every Ogg byte-fidelity test).

- [ ] **Step 4: FUSE e2e suite (real mounts, local-only)**

```bash
cargo test -p musefs-fuse -- --ignored
```
Expected: PASS (needs `/dev/fuse` + libfuse; this machine has both).

---

### Task 7: Bench refresh — BENCHMARKS.md Ogg rows

**Files:**
- Modify: `BENCHMARKS.md` (the "Latency-injected reads (`bench_read_under_latency`, nfs-hdd, SP4)" subsection)

- [ ] **Step 1: Run the latency read bench**

The bench is `#[ignore]`d and local-only (needs `/dev/fuse` and a `musefs-latencyfs` mount). Use the `nfs-hdd` profile to match the existing rows:

```bash
MUSEFS_BENCH_LATENCY_PROFILE=nfs-hdd cargo test -p musefs-core --features metrics \
    --test bench_ingest -- --ignored --nocapture bench_read_under_latency
```

Expected output: a two-row report (`read_whole_cold`, `read_seek_cold`) where `preads` and `bytes_read` are now **non-zero** (they were structurally 0 before Task 2). Record the rows.

- [ ] **Step 2: Update BENCHMARKS.md**

Replace the existing caveat paragraph under "### Latency-injected reads (`bench_read_under_latency`, nfs-hdd, SP4)" — the one explaining that the pread columns read 0 because the Ogg serve path never incremented the counters — with the refreshed numbers and a note of this shape (substitute the measured values):

```markdown
### Latency-injected reads (`bench_read_under_latency`, nfs-hdd, SP4 / Phase 5)

`read_whole_cold` <X> ms (<N> preads, <B> bytes), `read_seek_cold` <Y> ms
(<M> preads, <C> bytes). Earlier recordings showed 0 in the pread columns
because the Ogg serve path was uninstrumented (#71) — the zeros meant
"uncounted", not "free". Since Phase 5 every Ogg backing read (index scan,
CRC probe, header, payload) counts attempt-based preads/bytes, and
`MUSEFS_FAULT_PREAD_US`/latency injection applies to them, so `wall_ms` and
the round-trip columns are all meaningful for Ogg.
```

Keep any surrounding analysis that still holds (e.g. the open+resolve-latency observation) only if the fresh numbers still support it; otherwise rewrite to match the data.

- [ ] **Step 3: Check the tracking README for mirrored rows**

```bash
grep -n "under_latency\|read_whole_cold" docs/superpowers/specs/2026-05-30-optimization-pass/README.md
```
The known pread caveat in that README (≈line 124) is about the *scan-path* sweep columns — leave it. Update only if the grep shows the serve-latency rows mirrored there; otherwise no change.

- [ ] **Step 4: Commit**

```bash
git add BENCHMARKS.md
git commit -m "docs: refresh latency-read bench rows now that Ogg preads count (#71)

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 8: Roadmap update

**Files:**
- Modify: `docs/ROADMAP.md` (the "Phase 5 — Metrics" section)

- [ ] **Step 1: Mark the phase done**

Replace the open Phase 5 section:

```markdown
**Phase 5 — Metrics**
- #71 — Ogg serve path records no pread/byte metrics (instrumentation blind today).
- #76 — metric counters covered only by direct-call tests (locks in #71's fix).
```

with (matching the done-phase style above it):

```markdown
**Phase 5 — Metrics** — *done*
- ~~#71 — Ogg serve path records no pread/byte metrics~~ — done: every Ogg
  backing read (index scan, CRC probe, header, payload) counts attempt-based
  `preads`/`pread_bytes`, so latency injection and `bench_read_under_latency`
  now cover Ogg.
- ~~#76 — metric counters covered only by direct-call tests~~ — done: serve-site
  tests drive real reads through every counter's segment arm (ArtImage,
  BinaryTag, both OggArtSlice branches, Ogg preads), and CI runs
  `cargo test -p musefs-core --features metrics` on every PR.
```

- [ ] **Step 2: Commit**

```bash
git add docs/ROADMAP.md
git commit -m "docs: mark Phase 5 metrics issues done in roadmap (#71, #76)

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Completion

After Task 8, all work is on `phase5-serve-path-metrics`. Use the superpowers:finishing-a-development-branch skill: the PR closes #71 and #76 (`main` is protected; PR + ci-ok/coverage-ok required).
