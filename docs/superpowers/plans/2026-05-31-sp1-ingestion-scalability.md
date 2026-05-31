# SP1 — Ingestion scalability — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make `scan` / `revalidate` fast and bounded-memory over large libraries — bounded probing reads (no whole-file slurp), parallel probing with a single serial DB writer, per-batch transactions, and bulk-write pragmas — without changing what is written to the DB or the served bytes.

**Architecture:** Two stages. **Stage A** replaces the whole-file slurp with a per-format *bounded* metadata probe (a `prefix` window + `file_len`, widened on a `NeedMore { up_to }` signal; M4A uses the existing seek reader), gated green by an **equivalence property** (bounded probe ≡ full-file probe). **Stage B** layers a parallel-probe / single-writer pipeline on top: probe workers (no DB access) feed a bounded byte-budgeted channel to one writer thread that batches transactions on a scan-scoped bulk connection.

**Tech Stack:** Rust workspace (`musefs-format`, `musefs-db`, `musefs-core`, `musefs-cli`), `rusqlite` (SQLite, WAL), `thiserror`, `std::thread` + `std::sync::mpsc::sync_channel`, `proptest`, the SP0 bench harness (`musefs-core/tests/bench_ingest.rs`, `musefs-latencyfs`).

**Spec:** `docs/superpowers/specs/2026-05-30-optimization-pass/SP1-ingestion-scalability.md`

---

## File map

**Stage A**
- `musefs-core/src/metrics.rs` — add scan-path counters (`scan_opens`, `scan_preads`, `scan_bytes_read`) + hooks `on_scan_open` / `on_scan_read`.
- `musefs-format/src/probe.rs` *(new)* — the shared `Extent<T>` outcome type (`Complete(T)` / `NeedMore { up_to }`).
- `musefs-format/src/flac.rs` — add `read_metadata_bounded(prefix) -> Result<Extent<FlacMeta>>`.
- `musefs-format/src/mp3.rs` — add `locate_audio_bounded(prefix, file_len, tail) -> Result<Extent<Mp3Bounds>>`.
- `musefs-format/src/ogg/mod.rs` — add `read_metadata_bounded(prefix, file_len) -> Result<Extent<OggHeader>>`.
- `musefs-format/src/wav.rs` — add `locate_audio_bounded(prefix, file_len) -> Result<Extent<WavBounds>>`.
- `musefs-format/src/lib.rs` — `pub mod probe;` + re-exports.
- `musefs-core/src/scan.rs` — new bounded `probe_file(path) -> io::Result<ProbeOutcome>`, the windowed read loop, `read_window`; rewire `scan_directory` / `revalidate`; keep `probe_full` (test-only) for equivalence.
- `musefs-core/tests/probe_equivalence.rs` *(new)* — the headline equivalence property.

**Stage B**
- `musefs-db/src/bulk.rs` *(new)* — `Db::apply_bulk_pragmas` / `apply_bulk_pragmas_self`, `BulkWriter` (one transaction wrapping the four writes).
- `musefs-db/src/lib.rs` — `mod bulk;` wiring.
- `musefs-core/src/scan.rs` — `ScanStats`/`RevalidateStats` gain `failed`; `ScanOptions`; `scan_directory_with` / `revalidate_with` pipeline (probe pool + `ByteBudget` backpressure + writer batching); `revalidate` pre-dispatch skip pass.
- `musefs-core/src/byte_budget.rs` *(new)* — `ByteBudget` (Mutex<u64> + Condvar) backpressure.
- `musefs-cli/src/lib.rs` — `--jobs` flag wired through `run_scan`.
- `musefs-core/tests/common/report.rs` — add `bytes_read` column.
- `musefs-core/tests/bench_ingest.rs` — report `scan_bytes_read`; add a `jobs` dimension; fix the stale "opens/preads are serve-only" comment.

**Constants** (all in `musefs-core/src/scan.rs`): `WINDOW = 1 << 20` (1 MiB), `BATCH_FILES = 256`, `BATCH_BYTES = 64 << 20` (64 MiB), `MAX_WIDEN_RETRIES = 8`.

---

# Stage A — Bounded reads (single-threaded, gated by equivalence)

## Task A1: Scan-path metrics counters

**Files:**
- Modify: `musefs-core/src/metrics.rs` (both the `#[cfg(feature = "metrics")]` and `#[cfg(not(...))]` modules)

- [ ] **Step 1: Write the failing test**

Add to the `#[cfg(all(test, feature = "metrics"))] mod tests` block in `musefs-core/src/metrics.rs`:

```rust
    #[test]
    fn scan_counters_accumulate_and_reset() {
        reset();
        on_scan_open();
        on_scan_read(4096);
        on_scan_read(128);
        let s = snapshot();
        assert_eq!(s.scan_opens, 1);
        assert_eq!(s.scan_preads, 2);
        assert_eq!(s.scan_bytes_read, 4096 + 128);
        reset();
        assert_eq!(snapshot(), Snapshot::default());
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p musefs-core --features metrics --lib metrics::tests::scan_counters -- --nocapture`
Expected: FAIL — `on_scan_open` / `on_scan_read` / `Snapshot.scan_opens` not found.

- [ ] **Step 3: Implement the counters**

In the `#[cfg(feature = "metrics")] mod imp`, add three statics beside the existing ones:

```rust
    static SCAN_OPENS: AtomicU64 = AtomicU64::new(0);
    static SCAN_PREADS: AtomicU64 = AtomicU64::new(0);
    static SCAN_BYTES_READ: AtomicU64 = AtomicU64::new(0);
```

Add two hooks beside `on_art_chunk`:

```rust
    /// One backing-file open on the *scan* path (distinct from serve-path `on_open`).
    pub fn on_scan_open() {
        SCAN_OPENS.fetch_add(1, Ordering::Relaxed);
    }

    /// One positioned scan-path read of `bytes` bytes (prefix, widen, or tail read).
    pub fn on_scan_read(bytes: u64) {
        SCAN_PREADS.fetch_add(1, Ordering::Relaxed);
        SCAN_BYTES_READ.fetch_add(bytes, Ordering::Relaxed);
    }
```

Add the three fields to `Snapshot` (after `art_chunks`):

```rust
        pub scan_opens: u64,
        pub scan_preads: u64,
        pub scan_bytes_read: u64,
```

Populate them in `snapshot()`:

```rust
            scan_opens: SCAN_OPENS.load(Ordering::Relaxed),
            scan_preads: SCAN_PREADS.load(Ordering::Relaxed),
            scan_bytes_read: SCAN_BYTES_READ.load(Ordering::Relaxed),
```

Zero them in `reset()`:

```rust
        SCAN_OPENS.store(0, Ordering::Relaxed);
        SCAN_PREADS.store(0, Ordering::Relaxed);
        SCAN_BYTES_READ.store(0, Ordering::Relaxed);
```

In the `#[cfg(not(feature = "metrics"))] mod imp`, mirror the surface: add the same three `Snapshot` fields, and the two no-op hooks:

```rust
    #[inline(always)]
    pub fn on_scan_open() {}
    #[inline(always)]
    pub fn on_scan_read(_bytes: u64) {}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p musefs-core --features metrics --lib metrics:: -- --nocapture`
Expected: PASS (both `counters_accumulate_and_reset` and `scan_counters_accumulate_and_reset`).
Run: `cargo build -p musefs-core` (no-metrics build still compiles).
Expected: builds clean.

- [ ] **Step 5: Commit**

```bash
git add musefs-core/src/metrics.rs
git commit -m "feat(metrics): scan-path open/read/byte counters"
```

---

## Task A2: `Extent` outcome type + FLAC bounded metadata

**Files:**
- Create: `musefs-format/src/probe.rs`
- Modify: `musefs-format/src/lib.rs`
- Modify: `musefs-format/src/flac.rs`

- [ ] **Step 1: Write the failing test**

Add to the `#[cfg(test)] mod tests` in `musefs-format/src/flac.rs`:

```rust
    use crate::probe::Extent;

    /// Build a minimal FLAC: marker + a single last STREAMINFO (type 0, 34-byte
    /// body) + `audio` bytes. Returns (full_bytes, audio_offset).
    fn flac_with_streaminfo(audio: &[u8]) -> (Vec<u8>, u64) {
        let mut v = b"fLaC".to_vec();
        push_block_header(&mut v, BLOCK_STREAMINFO, 34, true);
        v.extend(std::iter::repeat(0u8).take(34));
        let audio_offset = v.len() as u64;
        v.extend_from_slice(audio);
        (v, audio_offset)
    }

    #[test]
    fn read_metadata_bounded_complete_when_prefix_covers_blocks() {
        let (full, audio_offset) = flac_with_streaminfo(b"AUDIOAUDIO");
        // Prefix that includes all metadata but not all audio.
        let prefix = &full[..audio_offset as usize + 2];
        match read_metadata_bounded(prefix).unwrap() {
            Extent::Complete(meta) => assert_eq!(meta.audio_offset, audio_offset),
            other => panic!("expected Complete, got {other:?}"),
        }
    }

    #[test]
    fn read_metadata_bounded_needmore_when_block_body_truncated() {
        let (full, audio_offset) = flac_with_streaminfo(b"AUDIO");
        // Cut inside the STREAMINFO body (header is 4 bytes after the marker).
        let prefix = &full[..8];
        match read_metadata_bounded(prefix).unwrap() {
            Extent::NeedMore { up_to } => assert_eq!(up_to, audio_offset),
            other => panic!("expected NeedMore, got {other:?}"),
        }
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p musefs-format --lib flac::tests::read_metadata_bounded -- --nocapture`
Expected: FAIL — `crate::probe` and `read_metadata_bounded` do not exist.

- [ ] **Step 3: Create the `Extent` type and the FLAC bounded walk**

Create `musefs-format/src/probe.rs`:

```rust
//! Shared outcome type for *bounded* metadata probing: a format parser is given
//! only a `prefix` of the file (plus the true `file_len`) and either completes,
//! or reports the exact byte offset it must reach to continue.

/// Result of a bounded metadata probe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Extent<T> {
    /// The metadata region is fully present in the prefix; here is the parse.
    Complete(T),
    /// The prefix is too short. Read at least up to `up_to` bytes (capped at the
    /// file length) and retry. `up_to` is strictly greater than the current
    /// prefix length unless the parser cannot bound its need, in which case the
    /// caller falls back to reading the whole file.
    NeedMore { up_to: u64 },
}
```

In `musefs-format/src/lib.rs`, register the module beside the existing `pub mod` lines and re-export the type:

```rust
pub mod probe;
pub use probe::Extent;
```

In `musefs-format/src/flac.rs`, add (next to `read_metadata`):

```rust
use crate::probe::Extent;

/// Bounded twin of [`read_metadata`]: walk the metadata blocks present in
/// `prefix` (which may be a front-only window of the file). If a block's declared
/// body runs past the prefix, return `NeedMore { up_to }` with the exact end of
/// that block — the caller widens the window and retries. Otherwise `Complete`.
pub fn read_metadata_bounded(prefix: &[u8]) -> Result<Extent<FlacMeta>> {
    if prefix.len() < 4 || &prefix[0..4] != FLAC_MARKER {
        return Err(FormatError::NotFlac);
    }
    let mut pos = 4usize;
    let mut preserved = Vec::new();
    loop {
        if pos + 4 > prefix.len() {
            // Need at least the 4-byte block header.
            return Ok(Extent::NeedMore {
                up_to: (pos + 4) as u64,
            });
        }
        let header = prefix[pos];
        let is_last = (header & 0x80) != 0;
        let block_type = header & 0x7F;
        let len = ((prefix[pos + 1] as usize) << 16)
            | ((prefix[pos + 2] as usize) << 8)
            | (prefix[pos + 3] as usize);
        let body_start = pos + 4;
        let body_end = body_start + len;
        if body_end > prefix.len() {
            return Ok(Extent::NeedMore {
                up_to: body_end as u64,
            });
        }
        match block_type {
            BLOCK_STREAMINFO | BLOCK_APPLICATION | BLOCK_SEEKTABLE | BLOCK_CUESHEET => {
                preserved.push(MetadataBlock {
                    block_type,
                    body: prefix[body_start..body_end].to_vec(),
                });
            }
            _ => {}
        }
        pos = body_end;
        if is_last {
            break;
        }
    }
    Ok(Extent::Complete(FlacMeta {
        audio_offset: pos as u64,
        preserved,
    }))
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p musefs-format --lib flac::tests::read_metadata_bounded -- --nocapture`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add musefs-format/src/probe.rs musefs-format/src/lib.rs musefs-format/src/flac.rs
git commit -m "feat(format): Extent type + flac::read_metadata_bounded"
```

---

## Task A3: MP3 bounded metadata

**Files:**
- Modify: `musefs-format/src/mp3.rs`

- [ ] **Step 1: Write the failing test**

Add to `#[cfg(test)] mod tests` in `musefs-format/src/mp3.rs`:

```rust
    use crate::probe::Extent;

    /// ID3v2 header declaring `body` bytes of tag, then a frame-sync byte pair,
    /// then `audio`. Returns (full, audio_offset).
    fn mp3_with_id3v2(body_len: usize, audio: &[u8]) -> (Vec<u8>, u64) {
        let mut v = b"ID3\x04\x00\x00".to_vec(); // version 2.4, no flags
        v.extend_from_slice(&syncsafe(body_len as u32));
        v.extend(std::iter::repeat(0u8).take(body_len)); // tag body
        let audio_offset = v.len() as u64;
        v.extend_from_slice(&[0xFF, 0xFB]); // MPEG frame sync
        v.extend_from_slice(audio);
        (v, audio_offset)
    }

    #[test]
    fn locate_audio_bounded_complete_with_no_id3v1() {
        let (full, audio_offset) = mp3_with_id3v2(8, b"frames");
        let prefix = &full[..audio_offset as usize + 2]; // covers tag + sync
        let file_len = full.len() as u64;
        match locate_audio_bounded(prefix, file_len, None).unwrap() {
            Extent::Complete(b) => {
                assert_eq!(b.audio_offset, audio_offset);
                assert_eq!(b.audio_length, file_len - audio_offset);
            }
            other => panic!("expected Complete, got {other:?}"),
        }
    }

    #[test]
    fn locate_audio_bounded_needmore_when_tag_exceeds_prefix() {
        let (full, _audio_offset) = mp3_with_id3v2(4096, b"frames");
        let prefix = &full[..32]; // only the 10-byte header is present
        let file_len = full.len() as u64;
        match locate_audio_bounded(prefix, file_len, None).unwrap() {
            Extent::NeedMore { up_to } => assert_eq!(up_to, 10 + 4096 + 2),
            other => panic!("expected NeedMore, got {other:?}"),
        }
    }

    #[test]
    fn locate_audio_bounded_strips_id3v1_tail() {
        let (mut full, audio_offset) = mp3_with_id3v2(8, b"frames");
        let body_end = full.len();
        full.extend_from_slice(b"TAG"); // ID3v1 marker
        full.extend(std::iter::repeat(0u8).take(125)); // 128-byte tag total
        let file_len = full.len() as u64;
        let tail: [u8; 128] = full[full.len() - 128..].try_into().unwrap();
        let prefix = &full[..audio_offset as usize + 2];
        match locate_audio_bounded(prefix, file_len, Some(&tail)).unwrap() {
            Extent::Complete(b) => {
                assert_eq!(b.audio_offset, audio_offset);
                assert_eq!(b.audio_length, body_end as u64 - audio_offset);
            }
            other => panic!("expected Complete, got {other:?}"),
        }
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p musefs-format --lib mp3::tests::locate_audio_bounded -- --nocapture`
Expected: FAIL — `locate_audio_bounded` not found.

- [ ] **Step 3: Implement `locate_audio_bounded`**

Add to `musefs-format/src/mp3.rs` (next to `locate_audio`):

```rust
use crate::probe::Extent;

/// Bounded twin of [`locate_audio`]. `prefix` is a front window; `file_len` is the
/// true size; `tail` is the file's last 128 bytes (or `None` if the file is
/// shorter than 128 bytes). The audio start is the end of any leading ID3v2 tag
/// (declared in its 10-byte header); if that end is past the prefix, returns
/// `NeedMore`. The audio end is `file_len` minus a 128-byte ID3v1 trailer when the
/// `tail` begins with `TAG`.
pub fn locate_audio_bounded(
    prefix: &[u8],
    file_len: u64,
    tail: Option<&[u8; 128]>,
) -> Result<Extent<Mp3Bounds>> {
    let mut audio_offset = 0usize;
    if prefix.len() >= 10 && &prefix[0..3] == b"ID3" {
        let flags = prefix[5];
        let body = synchsafe_decode(&prefix[6..10]) as usize;
        let mut tag_len = 10 + body;
        if flags & 0x10 != 0 {
            tag_len += 10; // ID3v2.4 footer
        }
        if tag_len as u64 > file_len {
            return Err(FormatError::Malformed);
        }
        audio_offset = tag_len;
    } else if prefix.len() < 10 && (file_len as usize) >= 10 {
        // Not enough bytes even to read the ID3v2 header.
        return Ok(Extent::NeedMore { up_to: 10 });
    }

    // Need the frame-sync pair at the audio offset to be inside the prefix.
    if audio_offset + 2 > prefix.len() {
        return Ok(Extent::NeedMore {
            up_to: (audio_offset + 2) as u64,
        });
    }

    if prefix[audio_offset] != 0xFF || (prefix[audio_offset + 1] & 0xE0) != 0xE0 {
        return Err(FormatError::NotMp3);
    }

    let mut audio_end = file_len;
    if let Some(tail) = tail {
        if file_len >= audio_offset as u64 + 128 && &tail[0..3] == b"TAG" {
            audio_end -= 128;
        }
    }

    Ok(Extent::Complete(Mp3Bounds {
        audio_offset: audio_offset as u64,
        audio_length: audio_end - audio_offset as u64,
    }))
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p musefs-format --lib mp3::tests::locate_audio_bounded -- --nocapture`
Expected: PASS (all three).

- [ ] **Step 5: Commit**

```bash
git add musefs-format/src/mp3.rs
git commit -m "feat(format): mp3::locate_audio_bounded (prefix + ID3v1 tail)"
```

---

## Task A4: OGG bounded metadata

**Files:**
- Modify: `musefs-format/src/ogg/mod.rs`

OGG header packets are all front-anchored (so is all OGG embedded art). `read_header` already stops once the header packets are reassembled, but it returns `Malformed` on a truncated front rather than a "need more" signal — and the packet-level page walk does not cheaply expose an exact byte high-water mark. So the bounded variant geometrically grows the window (still capped at `file_len`, so the worst case equals reading the whole file and the equivalence property holds).

- [ ] **Step 1: Write the failing test**

Add a new test module at the bottom of `musefs-format/src/ogg/mod.rs`:

```rust
#[cfg(test)]
mod bounded_tests {
    use super::*;
    use crate::ogg::page_test_support::{build_header_pub, lace_packet_pub, vorbis_body_empty};
    use crate::probe::Extent;

    /// A minimal Opus stream: OpusHead + OpusTags header packets, then a trailing
    /// audio page. Returns (full, audio_offset). Mirrors the proven fixture in
    /// `musefs-core/src/scan.rs::ogg_probe_tests::probe_detects_opus_and_seeds_tags`.
    /// Note `build_header_pub(serial, &[&[u8]])` laces *all* header packets across
    /// pages (BOS set once) and returns `(Vec<u8>, u32)`; `lace_packet_pub` takes
    /// `(serial, seq_start, bos, granule, packet)` and returns `(Vec<u8>, u32)`.
    fn opus_stream() -> (Vec<u8>, u64) {
        let head = b"OpusHead\x01\x02\x38\x01\x80\xbb\x00\x00\x00\x00\x00".to_vec();
        let mut tags = b"OpusTags".to_vec();
        tags.extend_from_slice(&vorbis_body_empty());
        let serial = 0x1234;
        let (mut v, _) = build_header_pub(serial, &[&head, &tags]);
        let audio_offset = v.len() as u64;
        let (audio, _) = lace_packet_pub(serial, 2, false, 960, &[0u8; 100]);
        v.extend_from_slice(&audio);
        (v, audio_offset)
    }

    #[test]
    fn read_metadata_bounded_complete_when_prefix_covers_header() {
        let (full, audio_offset) = opus_stream();
        let file_len = full.len() as u64;
        let prefix = &full[..audio_offset as usize]; // exactly the header region
        match read_metadata_bounded(prefix, file_len).unwrap() {
            Extent::Complete(h) => assert_eq!(h.audio_offset, audio_offset),
            other => panic!("expected Complete, got {other:?}"),
        }
    }

    #[test]
    fn read_metadata_bounded_needmore_when_header_truncated() {
        let (full, _audio_offset) = opus_stream();
        let file_len = full.len() as u64;
        let prefix = &full[..20]; // mid first page
        match read_metadata_bounded(prefix, file_len).unwrap() {
            Extent::NeedMore { up_to } => assert!(up_to > 20 && up_to <= file_len),
            other => panic!("expected NeedMore, got {other:?}"),
        }
    }
}
```

(If `build_header_pub` / `lace_packet_pub` / `vorbis_body_empty` are not yet `pub` in `page_test_support`, they already are — they are used by `scan.rs`'s `ogg_probe_tests`.)

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p musefs-format --lib ogg::bounded_tests -- --nocapture`
Expected: FAIL — `read_metadata_bounded` not found.

- [ ] **Step 3: Implement `read_metadata_bounded`**

Add to `musefs-format/src/ogg/mod.rs` (next to `read_metadata`):

```rust
use crate::probe::Extent;

/// Bounded twin of [`read_metadata`]. OGG header packets (and all OGG embedded
/// art) are front-anchored, so a prefix covering the header region is sufficient.
/// `read_header` does not expose an exact byte need, so on a short/truncated
/// prefix this geometrically grows the window (doubling, capped at `file_len`):
/// header regions are tiny, so the first 1 MiB window almost always completes,
/// and the cap guarantees the worst case equals reading the whole file.
pub fn read_metadata_bounded(prefix: &[u8], file_len: u64) -> Result<Extent<OggHeader>> {
    match read_header(prefix) {
        Ok(header) => Ok(Extent::Complete(header)),
        Err(_) if (prefix.len() as u64) < file_len => {
            let grown = ((prefix.len() as u64).saturating_mul(2)).max(64 * 1024);
            Ok(Extent::NeedMore {
                up_to: grown.min(file_len),
            })
        }
        Err(e) => Err(e),
    }
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p musefs-format --lib ogg::bounded_tests -- --nocapture`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add musefs-format/src/ogg/mod.rs
git commit -m "feat(format): ogg::read_metadata_bounded (geometric widen, file_len cap)"
```

---

## Task A5: WAV bounded metadata

**Files:**
- Modify: `musefs-format/src/wav.rs`

WAV metadata chunks (`LIST`/`INFO`, `id3 `) may sit *after* the `data` payload, and the slice-based walk cannot skip a `data` payload it doesn't hold. To keep the equivalence property trivially true, the bounded variant completes only when the prefix already covers the whole file; otherwise it asks for the whole file (`NeedMore { up_to: file_len }`). This is "no worse than today" (a seek-based RIFF walk is out of scope per the spec).

- [ ] **Step 1: Write the failing test**

Add to `#[cfg(test)] mod tests` in `musefs-format/src/wav.rs`:

```rust
    use crate::probe::Extent;

    /// RIFF/WAVE with a `fmt ` (16-byte) chunk and a `data` chunk of `audio`.
    fn wav_file(audio: &[u8]) -> Vec<u8> {
        let mut body = Vec::new();
        body.extend_from_slice(b"fmt ");
        body.extend_from_slice(&16u32.to_le_bytes());
        body.extend(std::iter::repeat(0u8).take(16));
        body.extend_from_slice(b"data");
        body.extend_from_slice(&(audio.len() as u32).to_le_bytes());
        body.extend_from_slice(audio);
        let mut v = b"RIFF".to_vec();
        v.extend_from_slice(&((4 + body.len()) as u32).to_le_bytes());
        v.extend_from_slice(b"WAVE");
        v.extend_from_slice(&body);
        v
    }

    #[test]
    fn locate_audio_bounded_complete_when_prefix_is_whole_file() {
        let full = wav_file(b"AUDIOAUDIO");
        let file_len = full.len() as u64;
        match locate_audio_bounded(&full, file_len).unwrap() {
            Extent::Complete(b) => assert_eq!(b.audio_length, 10),
            other => panic!("expected Complete, got {other:?}"),
        }
    }

    #[test]
    fn locate_audio_bounded_needmore_when_prefix_short() {
        let full = wav_file(b"AUDIOAUDIO");
        let file_len = full.len() as u64;
        let prefix = &full[..full.len() - 4];
        match locate_audio_bounded(prefix, file_len).unwrap() {
            Extent::NeedMore { up_to } => assert_eq!(up_to, file_len),
            other => panic!("expected NeedMore, got {other:?}"),
        }
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p musefs-format --lib wav::tests::locate_audio_bounded -- --nocapture`
Expected: FAIL — `locate_audio_bounded` not found.

- [ ] **Step 3: Implement `locate_audio_bounded`**

Add to `musefs-format/src/wav.rs` (next to `locate_audio`):

```rust
use crate::probe::Extent;

/// Bounded twin of [`locate_audio`]. WAV metadata chunks can trail the `data`
/// payload, which the slice walk cannot skip past, so completion requires the
/// whole file in `prefix`; otherwise request it. Equivalence is trivially
/// preserved (the completing parse is exactly `locate_audio` on the full file).
pub fn locate_audio_bounded(prefix: &[u8], file_len: u64) -> Result<Extent<WavBounds>> {
    if (prefix.len() as u64) < file_len {
        return Ok(Extent::NeedMore { up_to: file_len });
    }
    Ok(Extent::Complete(locate_audio(prefix)?))
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p musefs-format --lib wav::tests::locate_audio_bounded -- --nocapture`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add musefs-format/src/wav.rs
git commit -m "feat(format): wav::locate_audio_bounded (whole-file gate; seek-walk deferred)"
```

---

## Task A6: Bounded read loop in scan.rs

This is the integration task: a windowed `probe_file` that reads a bounded prefix, dispatches per format, widens on `NeedMore`, opens M4A through the seek reader, and reads the MP3 tail — counting every read via the new metrics hooks. `scan_directory` and `revalidate` switch to it. The original slice-based `probe` is renamed `probe_full` and kept (used by the equivalence test in Task A7).

**Files:**
- Modify: `musefs-core/src/scan.rs`

- [ ] **Step 1: Write the failing test**

Add to the bottom of `musefs-core/src/scan.rs` (new test module):

```rust
#[cfg(test)]
mod bounded_probe_tests {
    use super::*;
    use musefs_db::Db;

    /// Minimal FLAC: marker + a single last STREAMINFO (34-byte body) + audio.
    /// FLAC has no frame-sync check at the audio offset, so any payload works.
    fn flac_fixture() -> Vec<u8> {
        let mut bytes = b"fLaC".to_vec();
        bytes.push(0x80); // last-block flag set, type 0 (STREAMINFO)
        bytes.extend_from_slice(&[0, 0, 34]); // 24-bit length = 34
        bytes.extend(std::iter::repeat(0u8).take(34));
        bytes.extend_from_slice(b"AUDIOPAYLOAD");
        bytes
    }

    #[test]
    fn scan_directory_bounded_matches_full_for_flac() {
        // A FLAC fixture written to a temp dir, scanned with the (default) bounded
        // path, yields a track with the same audio bounds as a full-file probe.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.flac");
        let bytes = flac_fixture();
        std::fs::write(&path, &bytes).unwrap();

        let full = probe_full(&path, &bytes).expect("full probe");

        let db = Db::open_in_memory().unwrap();
        let stats = scan_directory(&db, dir.path()).unwrap();
        assert_eq!(stats.scanned, 1);
        let track = db.get_track_by_path(&std::fs::canonicalize(&path).unwrap().to_string_lossy()).unwrap().unwrap();
        assert_eq!(track.audio_offset as u64, full.audio_offset);
        assert_eq!(track.audio_length as u64, full.audio_length);
    }
}
```

(The other `bounded_probe_tests` cases below — B2, B3, B4 — reuse this `flac_fixture()` shape inline; each builds its own bytes so tasks read independently.)

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p musefs-core --lib scan::bounded_probe_tests -- --nocapture`
Expected: FAIL — `probe_full` not found (and `scan_directory` still slurps, so the symbol is missing).

- [ ] **Step 3: Implement the bounded read loop**

In `musefs-core/src/scan.rs`:

1. Add imports/constants near the top:

```rust
use std::io::{Read, Seek, SeekFrom};
use musefs_format::Extent;
use crate::metrics;

/// Initial bounded-read window. Covers typical metadata + cover art; a larger
/// metadata region triggers a `NeedMore` widen.
const WINDOW: usize = 1 << 20; // 1 MiB
/// Cap on widen iterations before falling back to a whole-file read.
const MAX_WIDEN_RETRIES: usize = 8;
```

2. Rename the existing `fn probe(path: &Path, bytes: &[u8]) -> Option<Probed>` to:

```rust
/// Full-buffer probe (legacy path). Retained as the reference implementation the
/// bounded path is checked against (see `tests/probe_equivalence.rs`).
pub(crate) fn probe_full(path: &Path, bytes: &[u8]) -> Option<Probed> {
```

(Keep its body unchanged.) **Also update the existing in-module test call sites** that call the old `probe(&path, …)` — `cargo build -p musefs-core --tests` will name them; at the time of writing they are in `scan.rs::ogg_probe_tests` (the `probe(&path, &bytes)` calls in the opus/oga/wav probe tests and the `probe(&path, b"not a real audio file")` negative test). Rewrite each `probe(` → `probe_full(`. (The two production call sites at the old lines 208/250 are replaced wholesale in steps 4–5 below, so don't rename those — they become `probe_file`.)

3. Add a positioned reader helper and the bounded probe:

```rust
/// Read `[0, len)` of `path` into a buffer, counting the read. `len` is clamped
/// to the file size by the OS (short read at EOF is fine).
fn read_window(file: &std::fs::File, len: usize) -> std::io::Result<Vec<u8>> {
    use std::os::unix::fs::FileExt;
    let mut buf = vec![0u8; len];
    let n = file.read_at(&mut buf, 0)?;
    buf.truncate(n);
    metrics::on_scan_read(n as u64);
    Ok(buf)
}

/// Read the file's last 128 bytes (for the MP3 ID3v1 trailer check), or `None`
/// if the file is shorter than 128 bytes.
fn read_tail_128(file: &std::fs::File, file_len: u64) -> std::io::Result<Option<[u8; 128]>> {
    if file_len < 128 {
        return Ok(None);
    }
    use std::os::unix::fs::FileExt;
    let mut buf = [0u8; 128];
    file.read_exact_at(&mut buf, file_len - 128)?;
    metrics::on_scan_read(128);
    Ok(Some(buf))
}

/// Bounded probe of one backing file: open once, read a bounded window, dispatch
/// per format, widening on `NeedMore`. Never reads the audio payload (M4A uses
/// the seek reader; front-anchored formats read only the metadata extent).
/// Returns `Ok(None)` for an unsupported/unparseable file (to be skipped).
fn probe_file(path: &Path, file_len: u64) -> std::io::Result<Option<Probed>> {
    let file = std::fs::File::open(path)?;
    metrics::on_scan_open();

    // M4A: seek reader, never touches mdat.
    if has_ext(path, "m4a") || has_ext(path, "m4b") {
        let mut f = &file;
        let scan = match mp4::read_structure_from(&mut f, file_len) {
            Ok(s) => s,
            Err(_) => return Ok(None),
        };
        return Ok(Some(Probed {
            format: Format::M4a,
            audio_offset: scan.mdat_payload_offset,
            audio_length: scan.mdat_payload_len,
            tags: mp4::read_tags(&scan.moov),
            pictures: mp4::read_pictures(&scan.moov),
        }));
    }

    // Front-anchored formats: read a window, widen on NeedMore.
    let tail = read_tail_128(&file, file_len)?;
    let mut want = (WINDOW as u64).min(file_len) as usize;
    let mut prefix = read_window(&file, want)?;
    for _ in 0..MAX_WIDEN_RETRIES {
        match probe_prefix(path, &prefix, file_len, tail.as_ref()) {
            Probe::Done(p) => return Ok(Some(p)),
            Probe::Skip => return Ok(None),
            Probe::NeedMore(up_to) => {
                // Already at EOF? The prefix is the whole file; widening can't help.
                if want as u64 >= file_len {
                    break;
                }
                // Grow to at least `up_to` (capped at the file), always making
                // progress (`+1`), then retry.
                want = (up_to.min(file_len) as usize).max(want + 1).min(file_len as usize);
                prefix = read_window(&file, want)?;
            }
        }
    }
    // Fallback: read the whole file once and use the full-buffer probe.
    if (prefix.len() as u64) < file_len {
        prefix = read_window(&file, file_len as usize)?;
    }
    Ok(probe_full(path, &prefix))
}

/// Outcome of a single bounded dispatch attempt against the current `prefix`.
enum Probe {
    Done(Probed),
    NeedMore(u64),
    Skip,
}

/// Dispatch the front-anchored formats against `prefix` + `file_len`.
fn probe_prefix(path: &Path, prefix: &[u8], file_len: u64, tail: Option<&[u8; 128]>) -> Probe {
    if has_ext(path, "flac") {
        match flac::read_metadata_bounded(prefix) {
            Ok(Extent::Complete(meta)) => Probe::Done(Probed {
                format: Format::Flac,
                audio_offset: meta.audio_offset,
                audio_length: file_len - meta.audio_offset,
                tags: flac::read_vorbis_comments(prefix).unwrap_or_default(),
                pictures: flac::read_pictures(prefix).unwrap_or_default(),
            }),
            Ok(Extent::NeedMore { up_to }) => Probe::NeedMore(up_to),
            Err(_) => Probe::Skip,
        }
    } else if has_ext(path, "mp3") {
        match mp3::locate_audio_bounded(prefix, file_len, tail) {
            Ok(Extent::Complete(b)) => Probe::Done(Probed {
                format: Format::Mp3,
                audio_offset: b.audio_offset,
                audio_length: b.audio_length,
                tags: mp3::read_tags(prefix),
                pictures: mp3::read_pictures(prefix),
            }),
            Ok(Extent::NeedMore { up_to }) => Probe::NeedMore(up_to),
            Err(_) => Probe::Skip,
        }
    } else if has_ext(path, "ogg") || has_ext(path, "oga") || has_ext(path, "opus") {
        match ogg::read_metadata_bounded(prefix, file_len) {
            Ok(Extent::Complete(header)) => {
                let format = match header.codec {
                    ogg::Codec::Opus => Format::Opus,
                    ogg::Codec::Vorbis => Format::Vorbis,
                    ogg::Codec::OggFlac => Format::OggFlac,
                };
                Probe::Done(Probed {
                    format,
                    audio_offset: header.audio_offset,
                    audio_length: file_len - header.audio_offset,
                    tags: ogg::read_tags(prefix).unwrap_or_default(),
                    pictures: ogg::read_pictures(prefix).unwrap_or_default(),
                })
            }
            Ok(Extent::NeedMore { up_to }) => Probe::NeedMore(up_to),
            Err(_) => Probe::Skip,
        }
    } else if has_ext(path, "wav") {
        match wav::locate_audio_bounded(prefix, file_len) {
            Ok(Extent::Complete(b)) => Probe::Done(Probed {
                format: Format::Wav,
                audio_offset: b.audio_offset,
                audio_length: b.audio_length,
                tags: wav::read_tags(prefix),
                pictures: wav::read_pictures(prefix),
            }),
            Ok(Extent::NeedMore { up_to }) => Probe::NeedMore(up_to),
            Err(_) => Probe::Skip,
        }
    } else {
        Probe::Skip
    }
}
```

4. Rewrite `scan_directory`'s per-file loop body to use `probe_file` (replacing the `std::fs::read` + `probe` + `metadata` ordering):

```rust
    for path in files {
        let meta = std::fs::metadata(&path)?;
        let Some(probed) = probe_file(&path, meta.len())? else {
            stats.skipped += 1;
            continue;
        };
        let abs = std::fs::canonicalize(&path)?;
        ingest(db, &abs.to_string_lossy(), &meta, probed)?;
        stats.scanned += 1;
    }
```

5. Rewrite `revalidate`'s changed-file branch likewise:

```rust
        let Some(probed) = probe_file(&path, meta.len())? else {
            continue;
        };
        ingest(db, &abs_str, &meta, probed)?;
        stats.updated += 1;
```

(Delete the `let bytes = std::fs::read(&path)?;` lines in both functions.)

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p musefs-core --lib scan:: -- --nocapture`
Expected: PASS (the new `bounded_probe_tests` plus the existing `ogg_probe_tests`).
Run: `cargo test -p musefs-core` and `cargo test -p musefs-format`
Expected: PASS — no regressions.

- [ ] **Step 5: Commit**

```bash
git add musefs-core/src/scan.rs
git commit -m "feat(scan): bounded windowed probe_file (widen on NeedMore, M4A seek, tail read)"
```

---

## Task A7: Equivalence property (headline gate)

The true SP1 guard: a bounded scan (run with a deliberately tiny window so the `NeedMore`/widen path fires on every file) must produce **identical** DB state to a scan done with the legacy full-file probe (`probe_full`) as the oracle — for every format. Comparing against the legacy oracle (not just two bounded runs) is what proves the bounded path cannot drift the served output.

**Files:**
- Create: `musefs-core/tests/probe_equivalence.rs`
- Modify: `musefs-core/src/scan.rs` (env-overridable window + a `#[doc(hidden)]` full-probe oracle scan)
- Modify: `musefs-core/tests/common/corpus.rs` (`CorpusParams::single`)

- [ ] **Step 1: Write the failing test**

Create `musefs-core/tests/probe_equivalence.rs`:

```rust
//! Headline SP1 correctness guard: a tiny-window bounded scan is equivalent (in
//! its parsed DB rows) to a legacy full-file-probe scan, for every format.

mod common;

use common::corpus::{bench_formats, format_token, generate, CorpusParams};
use musefs_db::Db;

/// Normalize a DB to comparable rows: tracks by path, tags by (key,value,ordinal),
/// art by (sha256, ordinal). Excludes raw `art.id` (insertion-order rowid).
fn normalized(db: &Db) -> Vec<(String, i64, i64, Vec<(String, String, i64)>, Vec<(String, i64)>)> {
    let mut out = Vec::new();
    for t in db.list_tracks().unwrap() {
        let tags: Vec<_> = db
            .get_tags(t.id)
            .unwrap()
            .into_iter()
            .map(|tg| (tg.key, tg.value, tg.ordinal))
            .collect();
        let art: Vec<_> = db
            .get_track_art(t.id)
            .unwrap()
            .into_iter()
            .map(|a| (db.get_art(a.art_id).unwrap().unwrap().sha256, a.ordinal))
            .collect();
        out.push((t.backing_path, t.audio_offset, t.audio_length, tags, art));
    }
    out.sort();
    out
}

#[test]
fn bounded_probe_equivalent_to_full_for_every_format() {
    for fmt in bench_formats() {
        let dir = tempfile::tempdir().unwrap();
        let params = CorpusParams::single(fmt, /*albums*/ 2, /*tracks*/ 3);
        generate(dir.path(), &params);

        // Oracle: legacy whole-file probe.
        let oracle_db = Db::open_in_memory().unwrap();
        musefs_core::scan_directory_full_oracle(&oracle_db, dir.path()).unwrap();
        let oracle = normalized(&oracle_db);

        // Bounded scan with a 64-byte window → widen path fires on every file.
        std::env::set_var("MUSEFS_SCAN_WINDOW", "64");
        let bounded_db = Db::open_in_memory().unwrap();
        musefs_core::scan_directory(&bounded_db, dir.path()).unwrap();
        std::env::remove_var("MUSEFS_SCAN_WINDOW");
        let bounded = normalized(&bounded_db);

        assert_eq!(
            oracle, bounded,
            "format {}: bounded scan diverged from full-probe oracle",
            format_token(fmt)
        );
        assert!(!oracle.is_empty(), "format {}: scanned nothing", format_token(fmt));
    }
}
```

(This integration file has a single test in its own test binary, so the `set_var`/`remove_var` around the bounded scan cannot race another test. Edition 2021 — `set_var` is safe.)

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p musefs-core --test probe_equivalence -- --nocapture`
Expected: FAIL — `scan_directory_full_oracle` and `CorpusParams::single` do not exist, and `MUSEFS_SCAN_WINDOW` is not honored.

- [ ] **Step 3: Add the window override, the oracle scan, and the corpus helper**

In `musefs-core/src/scan.rs`:

Make the window env-overridable:

```rust
/// Effective initial window: `MUSEFS_SCAN_WINDOW` (bytes) if set, else `WINDOW`.
fn scan_window() -> usize {
    std::env::var("MUSEFS_SCAN_WINDOW")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .filter(|&n| n > 0)
        .unwrap_or(WINDOW)
}
```

and in `probe_file` change the initial `want` to `let mut want = (scan_window() as u64).min(file_len) as usize;`.

Add the full-probe oracle scan (a thin retention of the pre-SP1 slurp path, used only as a test oracle):

```rust
/// Test/oracle only: scan using the legacy whole-file probe (`probe_full`). The
/// equivalence property compares this against the bounded `scan_directory`.
#[doc(hidden)]
pub fn scan_directory_full_oracle(db: &Db, root: &Path) -> Result<ScanStats> {
    let mut files = Vec::new();
    if root.is_file() {
        if is_supported_audio(root) {
            files.push(root.to_path_buf());
        }
    } else {
        collect_audio(root, &mut files)?;
    }
    let mut stats = ScanStats { scanned: 0, skipped: 0, failed: 0 };
    for path in files {
        let bytes = std::fs::read(&path)?;
        let Some(probed) = probe_full(&path, &bytes) else {
            stats.skipped += 1;
            continue;
        };
        let meta = std::fs::metadata(&path)?;
        let abs = std::fs::canonicalize(&path)?;
        ingest(db, &abs.to_string_lossy(), &meta, probed)?;
        stats.scanned += 1;
    }
    Ok(stats)
}
```

Re-export it from `musefs-core/src/lib.rs` beside the other scan re-exports:

```rust
pub use scan::scan_directory_full_oracle;
```

In `musefs-core/tests/common/corpus.rs`, add the constructor (fields verified against the struct: `albums`, `tracks_per_album`, `bytes_per_track`, `art_bytes_per_track`, `format_mix`, `seed`):

```rust
impl CorpusParams {
    /// A small single-format corpus used by the equivalence property. `art_bytes`
    /// is honored only by the FLAC generator (the MP3/M4A/Ogg/WAV corpus files
    /// carry no embedded art or in-file tags — those ride via the DB at scan
    /// time). The tiny `MUSEFS_SCAN_WINDOW=64` still forces the widen path on
    /// every format, via the format's own trigger: FLAC the metadata-block walk,
    /// MP3 the ID3v2 tag size, OGG the geometric grow, WAV the whole-file gate.
    pub fn single(fmt: Format, albums: usize, tracks_per_album: usize) -> Self {
        Self {
            albums,
            tracks_per_album,
            bytes_per_track: 4 * 1024,
            art_bytes_per_track: 8 * 1024,
            format_mix: vec![fmt],
            seed: 1,
        }
    }
}
```

The spec's headline property names "every format fixture **and** proptest-generated files." This task covers the six format fixtures (via the corpus generator) directly; the **generative** side is already covered by the format-layer proptests (`cargo test -p musefs-format --features fuzzing`) and `musefs-core/tests/proptest_read_fidelity` — both run in the final verification. No new proptest oracle is added here; the corpus-based oracle is the SP1-specific bounded≡full guard.

Note: this task depends on Task B2 having added the `failed` field to `ScanStats` — but A7 runs in Stage A, before B2. So in Stage A, `ScanStats` still has only `scanned`/`skipped`; write `ScanStats { scanned: 0, skipped: 0 }` here and add `failed: 0` when Task B2 lands (Task B2's diff already updates every `ScanStats` constructor). If executing strictly in order, omit `failed` in this task.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p musefs-core --test probe_equivalence -- --nocapture`
Expected: PASS for all six formats (flac, mp3, m4a-moov-first, m4a-moov-last, ogg, wav).

- [ ] **Step 5: Commit**

```bash
git add musefs-core/src/scan.rs musefs-core/src/lib.rs musefs-core/tests/probe_equivalence.rs musefs-core/tests/common/corpus.rs
git commit -m "test(scan): bounded≡full-oracle probe equivalence (all formats, tiny-window widen)"
```

**Stage A gate:** `cargo test --workspace` green, `cargo clippy --all-targets -- -D warnings` clean. Do not start Stage B until the equivalence property passes.

---

# Stage B — Pipeline (parallel probe, single writer, batching, bulk pragmas)

## Task B1: `BulkWriter` + bulk pragmas (musefs-db)

A batch-writer that wraps many files' writes in **one** transaction, plus the scan-scoped bulk pragmas. The four write operations are duplicated onto a held `Transaction` (the existing `upsert_track`/`replace_tags`/etc. each open their own transaction and cannot be nested). The "scan-scoped connection distinct from any mount connection" the spec calls for is realized by applying the bulk pragmas to the scanner's own short-lived `Db` (CLI `run_scan` opens it, scans, drops it; the mount opens a different connection) — no separate `open_bulk` is needed.

**Files:**
- Create: `musefs-db/src/bulk.rs`
- Modify: `musefs-db/src/lib.rs`

- [ ] **Step 1: Write the failing test**

Create `musefs-db/src/bulk.rs` with only this test for now (implementation in Step 3). The test reads `db.conn` directly — `conn` is a private field of `Db` (defined in the crate root `lib.rs`), and `bulk` / `bulk::tests` are descendant modules, so the private field is in scope:

```rust
#[cfg(test)]
mod tests {
    use crate::models::{Format, NewArt, NewTrack, Tag, TrackArt};
    use crate::Db;

    #[test]
    fn bulk_writer_persists_a_batch_in_one_commit() {
        let db = Db::open_in_memory().unwrap();
        {
            let mut bw = db.bulk_writer().unwrap();
            for i in 0..3 {
                let id = bw
                    .upsert_track(&NewTrack {
                        backing_path: format!("/m/{i}.flac"),
                        format: Format::Flac,
                        audio_offset: 100,
                        audio_length: 200,
                        backing_size: 300,
                        backing_mtime: 1,
                    })
                    .unwrap();
                bw.replace_tags(id, &[Tag::new("title", &format!("t{i}"), 0)])
                    .unwrap();
                let art_id = bw
                    .upsert_art(&NewArt {
                        mime: "image/png".into(),
                        width: None,
                        height: None,
                        data: vec![1, 2, 3, 4],
                    })
                    .unwrap();
                bw.set_track_art(
                    id,
                    &[TrackArt { art_id, picture_type: 3, description: String::new(), ordinal: 0 }],
                )
                .unwrap();
            }
            bw.commit().unwrap();
        }
        assert_eq!(db.list_tracks().unwrap().len(), 3);
        // Dedup: identical art blob stored once.
        let count: i64 = db
            .conn
            .query_row("SELECT COUNT(*) FROM art", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1);
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p musefs-db --lib bulk::tests -- --nocapture`
Expected: FAIL — `bulk_writer` / `BulkWriter` not found.

- [ ] **Step 3: Implement the bulk pragmas + writer**

Add to `musefs-db/src/bulk.rs` (above the test module):

```rust
use crate::models::{NewArt, NewTrack, Tag, TrackArt};
use crate::{Db, Result};
use rusqlite::{params, Transaction};
use sha2::{Digest, Sha256};

fn sha256_hex(data: &[u8]) -> String {
    let digest = Sha256::digest(data);
    let mut s = String::with_capacity(64);
    for b in digest {
        use std::fmt::Write;
        let _ = write!(s, "{b:02x}");
    }
    s
}

impl Db {
    /// Apply the bulk-write pragmas to an open connection. WAL is left untouched
    /// (retained from `open`), so concurrent mount readers keep working. Safe on
    /// in-memory DBs. Intended for a scan-scoped `Db` the caller drops at scan end.
    pub fn apply_bulk_pragmas(conn: &rusqlite::Connection) -> Result<()> {
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "cache_size", -65536)?; // 64 MiB
        conn.pragma_update(None, "temp_store", "MEMORY")?;
        Ok(())
    }

    /// Apply bulk pragmas to this DB's own connection.
    pub fn apply_bulk_pragmas_self(&self) -> Result<()> {
        Self::apply_bulk_pragmas(&self.conn)
    }

    /// Begin a batch transaction. All writes go through the returned handle and
    /// land atomically on `commit()`.
    pub fn bulk_writer(&self) -> Result<BulkWriter<'_>> {
        Ok(BulkWriter {
            tx: self.conn.unchecked_transaction()?,
        })
    }
}

/// A batch of track writes held in one transaction. Mirrors `Db::upsert_track` /
/// `replace_tags` / `upsert_art` / `set_track_art`, but executes on a single
/// caller-held transaction so a whole batch commits with one fsync.
pub struct BulkWriter<'c> {
    tx: Transaction<'c>,
}

impl BulkWriter<'_> {
    pub fn upsert_track(&mut self, t: &NewTrack) -> Result<i64> {
        self.tx.execute(
            "INSERT INTO tracks
                (backing_path, format, audio_offset, audio_length, backing_size, backing_mtime, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, CAST(strftime('%s','now') AS INTEGER))
             ON CONFLICT(backing_path) DO UPDATE SET
                format=excluded.format, audio_offset=excluded.audio_offset,
                audio_length=excluded.audio_length, backing_size=excluded.backing_size,
                backing_mtime=excluded.backing_mtime,
                updated_at=CAST(strftime('%s','now') AS INTEGER)",
            params![t.backing_path, t.format.as_str(), t.audio_offset, t.audio_length, t.backing_size, t.backing_mtime],
        )?;
        Ok(self.tx.query_row(
            "SELECT id FROM tracks WHERE backing_path = ?1",
            params![t.backing_path],
            |r| r.get(0),
        )?)
    }

    pub fn replace_tags(&mut self, track_id: i64, tags: &[Tag]) -> Result<()> {
        self.tx.execute("DELETE FROM tags WHERE track_id = ?1", params![track_id])?;
        let mut stmt = self
            .tx
            .prepare_cached("INSERT INTO tags (track_id, key, value, ordinal) VALUES (?1, ?2, ?3, ?4)")?;
        for t in tags {
            stmt.execute(params![track_id, t.key, t.value, t.ordinal])?;
        }
        Ok(())
    }

    pub fn upsert_art(&mut self, a: &NewArt) -> Result<i64> {
        let sha = sha256_hex(&a.data);
        self.tx.execute(
            "INSERT INTO art (sha256, mime, width, height, byte_len, data)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6) ON CONFLICT(sha256) DO NOTHING",
            params![sha, a.mime, a.width, a.height, a.data.len() as i64, a.data],
        )?;
        Ok(self
            .tx
            .query_row("SELECT id FROM art WHERE sha256 = ?1", params![sha], |r| r.get(0))?)
    }

    pub fn set_track_art(&mut self, track_id: i64, items: &[TrackArt]) -> Result<()> {
        self.tx.execute("DELETE FROM track_art WHERE track_id = ?1", params![track_id])?;
        let mut stmt = self.tx.prepare_cached(
            "INSERT INTO track_art (track_id, art_id, picture_type, description, ordinal)
             VALUES (?1, ?2, ?3, ?4, ?5)",
        )?;
        for it in items {
            stmt.execute(params![track_id, it.art_id, it.picture_type, it.description, it.ordinal])?;
        }
        Ok(())
    }

    pub fn commit(self) -> Result<()> {
        self.tx.commit()?;
        Ok(())
    }
}
```

In `musefs-db/src/lib.rs`, register the module beside the others:

```rust
mod bulk;
pub use bulk::BulkWriter;
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p musefs-db --lib bulk::tests -- --nocapture`
Expected: PASS.
Run: `cargo test -p musefs-db`
Expected: PASS — no regressions.

- [ ] **Step 5: Commit**

```bash
git add musefs-db/src/bulk.rs musefs-db/src/lib.rs
git commit -m "feat(db): scan-scoped bulk connection + BulkWriter batch transaction"
```

---

## Task B2: Resilience — `failed` counter, non-fatal per-file errors

**Files:**
- Modify: `musefs-core/src/scan.rs`

- [ ] **Step 1: Write the failing test**

Add to `bounded_probe_tests` in `musefs-core/src/scan.rs`:

```rust
    #[test]
    fn scan_counts_unreadable_file_as_failed_and_continues() {
        let dir = tempfile::tempdir().unwrap();
        // One good FLAC + one zero-byte ".flac" that cannot parse.
        let good = dir.path().join("good.flac");
        let mut bytes = b"fLaC".to_vec();
        bytes.push(0x80);
        bytes.extend_from_slice(&[0, 0, 34]);
        bytes.extend(std::iter::repeat(0u8).take(34));
        bytes.extend_from_slice(b"AUDIO");
        std::fs::write(&good, &bytes).unwrap();
        std::fs::write(dir.path().join("bad.flac"), b"").unwrap();

        let db = Db::open_in_memory().unwrap();
        let stats = scan_directory(&db, dir.path()).unwrap();
        assert_eq!(stats.scanned, 1);
        assert_eq!(stats.skipped + stats.failed, 1);
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p musefs-core --lib scan::bounded_probe_tests::scan_counts_unreadable -- --nocapture`
Expected: FAIL — `ScanStats` has no `failed` field.

- [ ] **Step 3: Add the `failed` field and make per-file IO non-fatal**

In `musefs-core/src/scan.rs`:

Add `pub failed: u64,` to `ScanStats` and `RevalidateStats` (and a `failed: 0,` initializer where each is constructed).

Wrap the per-file work in `scan_directory` so a per-file IO error is counted, not propagated:

```rust
    for path in files {
        let meta = match std::fs::metadata(&path) {
            Ok(m) => m,
            Err(_) => { stats.failed += 1; continue; }
        };
        match probe_file(&path, meta.len()) {
            Ok(Some(probed)) => {
                let abs = std::fs::canonicalize(&path)?;
                ingest(db, &abs.to_string_lossy(), &meta, probed)?;
                stats.scanned += 1;
            }
            Ok(None) => stats.skipped += 1,
            Err(_) => stats.failed += 1,
        }
    }
```

Apply the same `Err(_) => { failed += 1; continue }` treatment to `revalidate`'s `metadata` / `probe_file` calls. (Keep `db.*` write errors propagating via `?` — those are fatal.)

Update the CLI print line in `musefs-cli/src/lib.rs::run_scan` to include `failed`:

```rust
        println!(
            "scanned {} file(s), skipped {}, failed {}",
            stats.scanned, stats.skipped, stats.failed
        );
```

and the revalidate print to add `, {} failed` / `stats.failed`.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p musefs-core --lib scan:: -- --nocapture` and `cargo build -p musefs-cli`
Expected: PASS / builds.

- [ ] **Step 5: Commit**

```bash
git add musefs-core/src/scan.rs musefs-cli/src/lib.rs
git commit -m "feat(scan): non-fatal per-file errors with a failed counter"
```

---

## Task B3: Byte-budget backpressure + parallel pipeline

**Files:**
- Create: `musefs-core/src/byte_budget.rs`
- Modify: `musefs-core/src/lib.rs` (`mod byte_budget;`)
- Modify: `musefs-core/src/scan.rs`

- [ ] **Step 1: Write the failing test (byte budget)**

Create `musefs-core/src/byte_budget.rs`:

```rust
//! A byte-accounted semaphore: producers reserve N bytes before holding a value,
//! blocking until the in-flight total would stay within the cap; the consumer
//! releases bytes after persisting. Bounds peak in-flight art memory.

use std::sync::{Condvar, Mutex};

pub struct ByteBudget {
    cap: u64,
    state: Mutex<u64>,
    cv: Condvar,
}

impl ByteBudget {
    pub fn new(cap: u64) -> Self {
        Self { cap, state: Mutex::new(0), cv: Condvar::new() }
    }

    /// Reserve `n` bytes, blocking until they fit (a single item larger than the
    /// cap is admitted alone once in-flight is zero, to guarantee progress).
    pub fn acquire(&self, n: u64) {
        let mut in_flight = self.state.lock().unwrap();
        while *in_flight != 0 && *in_flight + n > self.cap {
            in_flight = self.cv.wait(in_flight).unwrap();
        }
        *in_flight += n;
    }

    /// Release `n` previously reserved bytes.
    pub fn release(&self, n: u64) {
        let mut in_flight = self.state.lock().unwrap();
        *in_flight = in_flight.saturating_sub(n);
        self.cv.notify_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn oversized_item_admitted_when_idle() {
        let b = ByteBudget::new(10);
        b.acquire(1000); // larger than cap, but in-flight was 0 → admitted
        b.release(1000);
    }

    #[test]
    fn blocks_until_release() {
        let b = Arc::new(ByteBudget::new(10));
        b.acquire(10);
        let b2 = Arc::clone(&b);
        let h = std::thread::spawn(move || b2.acquire(5)); // must block
        std::thread::sleep(std::time::Duration::from_millis(50));
        b.release(10); // unblocks the spawned acquire
        h.join().unwrap();
        b.release(5);
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p musefs-core --lib byte_budget -- --nocapture`
Expected: FAIL — module not registered.

- [ ] **Step 3: Register the module and add the parallel pipeline**

In `musefs-core/src/lib.rs` add `mod byte_budget;`.

In `musefs-core/src/scan.rs` add the options struct and the pipelined scan. Keep `scan_directory` as a back-compat wrapper:

```rust
use crate::byte_budget::ByteBudget;
use std::sync::mpsc::sync_channel;

const BATCH_FILES: usize = 256;
const BATCH_BYTES: u64 = 64 << 20; // 64 MiB

/// Knobs for a scan. `jobs == 0` means "use available parallelism".
#[derive(Debug, Clone)]
pub struct ScanOptions {
    pub jobs: usize,
}

impl Default for ScanOptions {
    fn default() -> Self {
        Self { jobs: 0 }
    }
}

fn effective_jobs(jobs: usize) -> usize {
    if jobs != 0 {
        return jobs;
    }
    std::thread::available_parallelism().map_or(1, |n| n.get())
}

/// One probed file ready to write, plus its art-byte weight for backpressure.
struct Unit {
    abs_path: String,
    meta_len: u64,
    meta_mtime: i64,
    probed: Probed,
    weight: u64,
}

fn art_weight(p: &Probed) -> u64 {
    p.pictures.iter().map(|pic| pic.data.len() as u64).sum()
}

/// Public entry: parallel-probe / single-writer scan of `root`.
pub fn scan_directory_with(db: &Db, root: &Path, opts: &ScanOptions) -> Result<ScanStats> {
    let mut files = Vec::new();
    if root.is_file() {
        if is_supported_audio(root) {
            files.push(root.to_path_buf());
        }
    } else {
        collect_audio(root, &mut files)?;
    }
    db.apply_bulk_pragmas_self()?; // scan-scoped tuning on the caller's connection
    let stats = run_pipeline(db, files, opts)?;
    Ok(stats)
}

/// Back-compat shim used by the CLI and existing tests.
pub fn scan_directory(db: &Db, root: &Path) -> Result<ScanStats> {
    scan_directory_with(db, root, &ScanOptions::default())
}
```

(`apply_bulk_pragmas_self` was added to `Db` in Task B1.) Now the pipeline itself, in `scan.rs`:

```rust
/// Probe `files` across `jobs` workers (no DB access) and write the results from a
/// single writer (this thread) in batched transactions. Per-file errors are
/// counted, not fatal.
fn run_pipeline(db: &Db, files: Vec<PathBuf>, opts: &ScanOptions) -> Result<ScanStats> {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Arc;

    let jobs = effective_jobs(opts.jobs);
    let budget = Arc::new(ByteBudget::new(BATCH_BYTES));
    let skipped = Arc::new(AtomicU64::new(0));
    let failed = Arc::new(AtomicU64::new(0));

    // Work queue: a shared iterator behind a mutex (cheap; probing dominates).
    let work = Arc::new(std::sync::Mutex::new(files.into_iter()));
    let (tx, rx) = sync_channel::<Unit>(jobs * 2);

    let mut workers = Vec::with_capacity(jobs);
    for _ in 0..jobs {
        let work = Arc::clone(&work);
        let tx = tx.clone();
        let budget = Arc::clone(&budget);
        let skipped = Arc::clone(&skipped);
        let failed = Arc::clone(&failed);
        workers.push(std::thread::spawn(move || loop {
            let next = { work.lock().unwrap().next() };
            let Some(path) = next else { break };
            let meta = match std::fs::metadata(&path) {
                Ok(m) => m,
                Err(_) => { failed.fetch_add(1, Ordering::Relaxed); continue; }
            };
            match probe_file(&path, meta.len()) {
                Ok(Some(probed)) => {
                    let abs = match std::fs::canonicalize(&path) {
                        Ok(a) => a,
                        Err(_) => { failed.fetch_add(1, Ordering::Relaxed); continue; }
                    };
                    let weight = art_weight(&probed);
                    budget.acquire(weight); // backpressure on in-flight art bytes
                    let unit = Unit {
                        abs_path: abs.to_string_lossy().into_owned(),
                        meta_len: meta.len(),
                        meta_mtime: mtime_secs(&meta),
                        probed,
                        weight,
                    };
                    if tx.send(unit).is_err() {
                        budget.release(weight);
                        break;
                    }
                }
                Ok(None) => { skipped.fetch_add(1, Ordering::Relaxed); }
                Err(_) => { failed.fetch_add(1, Ordering::Relaxed); }
            }
        }));
    }
    drop(tx); // close the channel once all clones (workers) finish

    // Writer: this thread. Batch by file count and accumulated art bytes.
    let mut scanned = 0u64;
    let mut batch: Vec<Unit> = Vec::new();
    let mut batch_bytes = 0u64;
    let mut flush = |batch: &mut Vec<Unit>, batch_bytes: &mut u64, scanned: &mut u64| -> Result<()> {
        if batch.is_empty() { return Ok(()); }
        let mut bw = db.bulk_writer()?;
        for u in batch.iter() {
            ingest_bulk(&mut bw, &u.abs_path, u.meta_len, u.meta_mtime, &u.probed)?;
            *scanned += 1;
        }
        bw.commit()?;
        for u in batch.drain(..) {
            budget.release(u.weight);
        }
        *batch_bytes = 0;
        Ok(())
    };

    for unit in rx {
        batch_bytes += unit.weight;
        batch.push(unit);
        if batch.len() >= BATCH_FILES || batch_bytes >= BATCH_BYTES {
            flush(&mut batch, &mut batch_bytes, &mut scanned)?;
        }
    }
    flush(&mut batch, &mut batch_bytes, &mut scanned)?;
    for w in workers { let _ = w.join(); }

    Ok(ScanStats {
        scanned,
        skipped: skipped.load(Ordering::Relaxed),
        failed: failed.load(Ordering::Relaxed),
    })
}
```

**Resilience invariant (keep workers panic-free).** The spec counts per-file errors and makes only writer/DB errors fatal. That guarantee holds **only because `probe_file` returns `io::Result` and never `unwrap()`s on per-file I/O** — a worker that panicked would silently drop its file (neither `failed`-counted nor ingested) and just reduce throughput. So: the worker body must propagate every per-file error as `Err`/`Ok(None)` (as written above), never `unwrap`/`expect` on `metadata`/`canonicalize`/read/probe. (The format parsers are already panic-hardened by the fuzz suite; this invariant keeps the orchestration layer matching it. A `catch_unwind` wrapper is deliberately *not* added — it would mask a real parser regression that the fuzzers should catch instead.)

Add a `BulkWriter` flavour of `ingest` next to the existing `ingest` in `scan.rs` (the existing `ingest` stays for any single-`Db` callers; both share the tag/art mapping logic):

```rust
/// Like `ingest`, but writes through a batch `BulkWriter`.
fn ingest_bulk(
    bw: &mut musefs_db::BulkWriter<'_>,
    abs_path: &str,
    meta_len: u64,
    meta_mtime: i64,
    probed: &Probed,
) -> Result<()> {
    let track_id = bw.upsert_track(&NewTrack {
        backing_path: abs_path.to_string(),
        format: probed.format,
        audio_offset: probed.audio_offset as i64,
        audio_length: probed.audio_length as i64,
        backing_size: meta_len as i64,
        backing_mtime: meta_mtime,
    })?;

    let mut tags = Vec::new();
    let mut ordinals: HashMap<String, i64> = HashMap::new();
    for (key, value) in &probed.tags {
        let ord = ordinals.entry(key.clone()).or_insert(0);
        tags.push(Tag::new(key, value, *ord));
        *ord += 1;
    }
    bw.replace_tags(track_id, &tags)?;

    let mut track_arts = Vec::new();
    let accepted = probed.pictures.iter().filter(|p| p.data.len() <= MAX_ART_BYTES);
    for (ordinal, pic) in accepted.enumerate() {
        let art_id = bw.upsert_art(&NewArt {
            mime: pic.mime.clone(),
            width: (pic.width != 0).then_some(pic.width as i64),
            height: (pic.height != 0).then_some(pic.height as i64),
            data: pic.data.clone(),
        })?;
        let picture_type = if pic.picture_type <= 20 { pic.picture_type as i64 } else { 0 };
        track_arts.push(TrackArt {
            art_id,
            picture_type,
            description: pic.description.clone(),
            ordinal: ordinal as i64,
        });
    }
    bw.set_track_art(track_id, &track_arts)?;
    Ok(())
}
```

Re-export `ScanOptions` from `musefs-core/src/lib.rs` (beside the existing `scan_directory`/`revalidate` re-exports):

```rust
pub use scan::{revalidate, scan_directory, scan_directory_with, RevalidateStats, ScanOptions, ScanStats};
```

- [ ] **Step 4: Write the determinism test, then run all**

Add to `bounded_probe_tests` in `scan.rs`:

```rust
    #[test]
    fn jobs1_and_jobsN_produce_equivalent_state() {
        let dir = tempfile::tempdir().unwrap();
        // A handful of distinct FLACs.
        for i in 0..12 {
            let mut bytes = b"fLaC".to_vec();
            bytes.push(0x80);
            bytes.extend_from_slice(&[0, 0, 34]);
            bytes.extend(std::iter::repeat(0u8).take(34));
            bytes.extend_from_slice(format!("AUDIO-{i}").as_bytes());
            std::fs::write(dir.path().join(format!("t{i}.flac")), &bytes).unwrap();
        }
        let norm = |jobs: usize| {
            let db = Db::open_in_memory().unwrap();
            scan_directory_with(&db, dir.path(), &ScanOptions { jobs }).unwrap();
            let mut rows: Vec<(String, i64, i64)> = db
                .list_tracks()
                .unwrap()
                .into_iter()
                .map(|t| (t.backing_path, t.audio_offset, t.audio_length))
                .collect();
            rows.sort();
            rows
        };
        assert_eq!(norm(1), norm(4));
        assert_eq!(norm(1).len(), 12);
    }
```

Run: `cargo test -p musefs-core --lib -- --nocapture`
Run: `cargo test -p musefs-core --test probe_equivalence -- --nocapture`
Expected: PASS (determinism holds; equivalence still green through the pipeline since `scan_directory` now routes through `run_pipeline`).

- [ ] **Step 5: Commit**

```bash
git add musefs-core/src/byte_budget.rs musefs-core/src/lib.rs musefs-core/src/scan.rs musefs-db/src/bulk.rs
git commit -m "feat(scan): parallel-probe/single-writer pipeline with byte-budget backpressure + batching"
```

---

## Task B4: `revalidate` pre-dispatch skip pass + pipeline

The unchanged-file skip is a DB read and must run on the writer/main thread before workers (which are DB-free) are dispatched.

**Files:**
- Modify: `musefs-core/src/scan.rs`

- [ ] **Step 1: Write the failing test**

Add to `bounded_probe_tests`:

```rust
    #[test]
    fn revalidate_skips_unchanged_and_reprobes_changed() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("x.flac");
        let mk = |audio: &[u8]| {
            let mut b = b"fLaC".to_vec();
            b.push(0x80);
            b.extend_from_slice(&[0, 0, 34]);
            b.extend(std::iter::repeat(0u8).take(34));
            b.extend_from_slice(audio);
            b
        };
        std::fs::write(&p, mk(b"AUDIO")).unwrap();
        let db = Db::open_in_memory().unwrap();
        scan_directory(&db, dir.path()).unwrap();

        // Unchanged → all unchanged.
        let s1 = revalidate_with(&db, dir.path(), &ScanOptions::default()).unwrap();
        assert_eq!(s1.unchanged, 1);
        assert_eq!(s1.updated, 0);
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p musefs-core --lib scan::bounded_probe_tests::revalidate_skips -- --nocapture`
Expected: FAIL — `revalidate_with` not found.

- [ ] **Step 3: Implement `revalidate_with`**

In `scan.rs`, add the pre-dispatch filter and pipeline the changed set:

```rust
pub fn revalidate_with(db: &Db, root: &Path, opts: &ScanOptions) -> Result<RevalidateStats> {
    let mut files = Vec::new();
    collect_audio(root, &mut files)?;
    db.apply_bulk_pragmas_self()?;

    // Main-thread pre-dispatch skip pass: load existing (path -> size,mtime) once,
    // stat each candidate, keep only changed files. Workers stay DB-free.
    let existing: HashMap<String, (i64, i64)> = db
        .list_tracks()?
        .into_iter()
        .map(|t| (t.backing_path, (t.backing_size, t.backing_mtime)))
        .collect();

    let mut unchanged = 0u64;
    let mut changed: Vec<PathBuf> = Vec::new();
    for path in files {
        let Ok(meta) = std::fs::metadata(&path) else { continue };
        let Ok(abs) = std::fs::canonicalize(&path) else { continue };
        let key = abs.to_string_lossy().to_string();
        if let Some(&(size, mtime)) = existing.get(&key) {
            if size == meta.len() as i64 && mtime == mtime_secs(&meta) {
                unchanged += 1;
                continue;
            }
        }
        changed.push(path);
    }

    let scan = run_pipeline(db, changed, opts)?;

    // Prune + GC on the writer connection (single-threaded), unchanged from before.
    let canon_root = std::fs::canonicalize(root)?;
    let mut pruned = 0u64;
    for track in db.list_tracks()? {
        if !Path::new(&track.backing_path).starts_with(&canon_root) {
            continue;
        }
        if let Err(e) = std::fs::metadata(&track.backing_path) {
            if e.kind() == std::io::ErrorKind::NotFound {
                db.delete_track(track.id)?;
                pruned += 1;
            }
        }
    }
    db.gc_orphan_art()?;

    Ok(RevalidateStats {
        updated: scan.scanned,
        unchanged,
        pruned,
        failed: scan.failed,
    })
}

pub fn revalidate(db: &Db, root: &Path) -> Result<RevalidateStats> {
    revalidate_with(db, root, &ScanOptions::default())
}
```

Delete the old `revalidate` body (replaced above). Update the `musefs-core/src/lib.rs` re-export to include `revalidate_with`.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p musefs-core -- --nocapture`
Expected: PASS (including existing revalidate tests, which call `revalidate`).

- [ ] **Step 5: Commit**

```bash
git add musefs-core/src/scan.rs musefs-core/src/lib.rs
git commit -m "feat(scan): pipelined revalidate with main-thread pre-dispatch skip pass"
```

---

## Task B5: CLI `--jobs` flag

**Files:**
- Modify: `musefs-cli/src/lib.rs`

- [ ] **Step 1: Write the failing test**

Add to the `#[cfg(test)] mod tests` in `musefs-cli/src/lib.rs` (or create one):

```rust
    #[test]
    fn scan_command_parses_jobs_flag() {
        use clap::Parser;
        let cli = Cli::try_parse_from(["musefs", "scan", "/m", "--db", "/tmp/x.db", "--jobs", "3"])
            .unwrap();
        match cli.command {
            Command::Scan { jobs, .. } => assert_eq!(jobs, 3),
            _ => panic!("expected Scan"),
        }
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p musefs-cli --lib scan_command_parses_jobs -- --nocapture`
Expected: FAIL — `Scan` has no `jobs` field.

- [ ] **Step 3: Add the flag and thread it through**

In the `Command::Scan { .. }` variant, add:

```rust
        /// Probe worker threads (0 = available parallelism). 1 = sequential.
        #[arg(long, default_value_t = 0)]
        jobs: usize,
```

Change `run_scan`'s signature and body to take and use `jobs`:

```rust
pub fn run_scan(db_path: &Path, backing_dir: &Path, revalidate: bool, jobs: usize) -> Result<()> {
    let db =
        Db::open(db_path).with_context(|| format!("opening database at {}", db_path.display()))?;
    let opts = musefs_core::ScanOptions { jobs };
    if revalidate {
        let stats = musefs_core::revalidate_with(&db, backing_dir, &opts)
            .with_context(|| format!("revalidating {}", backing_dir.display()))?;
        println!(
            "revalidated: {} updated, {} unchanged, {} pruned, {} failed",
            stats.updated, stats.unchanged, stats.pruned, stats.failed
        );
    } else {
        let stats = musefs_core::scan_directory_with(&db, backing_dir, &opts)
            .with_context(|| format!("scanning {}", backing_dir.display()))?;
        println!(
            "scanned {} file(s), skipped {}, failed {}",
            stats.scanned, stats.skipped, stats.failed
        );
    }
    Ok(())
}
```

Update the dispatch in `run` where `Command::Scan { backing_dir, db, revalidate }` is destructured to also bind `jobs` and pass it: `run_scan(&db, &backing_dir, revalidate, jobs)`.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p musefs-cli -- --nocapture` and `cargo build`
Expected: PASS / builds.

- [ ] **Step 5: Commit**

```bash
git add musefs-cli/src/lib.rs
git commit -m "feat(cli): scan --jobs flag (0=auto, 1=sequential)"
```

---

## Task B6: Bench/report — `bytes_read` column + `jobs` dimension

**Files:**
- Modify: `musefs-core/tests/common/report.rs`
- Modify: `musefs-core/tests/bench_ingest.rs`

- [ ] **Step 1: Add the `bytes_read` column to the report**

In `musefs-core/tests/common/report.rs`:

- Add a column to the macro format string (one more `{:>14}`):

```rust
        format!("{:<10} {:<10} {:<14} {:<10} {:>10} {:>10} {:>10} {:>10} {:>14} {:>12}", $($arg),*)
```

- Add `pub bytes_read: u64,` to `RunReport`.
- Add `"bytes_read"` to `header()` (before `"rss_kib"`).
- Add `self.bytes_read,` to `row()` (before the `opt(self.peak_rss_kib)` arg).

- [ ] **Step 2: Report `scan_bytes_read` and add a `jobs` dimension in bench_ingest**

In `musefs-core/tests/bench_ingest.rs`:

- Update the import at the top of the file (it currently reads `use musefs_core::{metrics, revalidate, scan_directory};`) to add the `_with` variants and `ScanOptions`:

```rust
use musefs_core::{metrics, revalidate_with, scan_directory_with, ScanOptions};
```

(`scan_directory` / `revalidate` are no longer called directly once the `_with` forms below replace them; drop them from the `use` to avoid an unused-import warning.)
- In `run_one`, set `bytes_read: snap.scan_bytes_read,` on both `RunReport`s (scan and revalidate), where `snap` is the corresponding `metrics::snapshot()`.
- In `bench_scan_under_latency`, set `bytes_read: s.scan_bytes_read,` **and** convert its `scan_directory(&db, &mount.path())` call to `scan_directory_with(&db, &mount.path(), &ScanOptions { jobs: std::env::var("MUSEFS_BENCH_JOBS").ok().and_then(|s| s.parse().ok()).unwrap_or(0) })` (it has no `run_one` `opts` in scope).
- Read `MUSEFS_BENCH_JOBS` (default 0) and thread it into a `ScanOptions`, replacing the bare `scan_directory(&db, …)` / `revalidate(&db, …)` calls with `scan_directory_with(&db, …, &opts)` / `revalidate_with(&db, …, &opts)`. Add near the top of `run_one`:

```rust
    let jobs = std::env::var("MUSEFS_BENCH_JOBS").ok().and_then(|s| s.parse().ok()).unwrap_or(0);
    let opts = musefs_core::ScanOptions { jobs };
```

and use `scan_directory_with(&db, &target.corpus_dir, &opts)` / `revalidate_with(&db, &target.corpus_dir, &opts)`.

- Replace the stale doc comment on `run_one` ("The `opens`/`preads` metrics instrument the *serve* path … print ~0") with:

```rust
/// Scan + revalidate one resolved target, printing a `scan` and a `revalidate`
/// row tagged with `format`/`storage`. The `bytes_read` column reports
/// `scan_bytes_read` (the SP1 bounded-read signal: front-anchored prefix + widen
/// + MP3 tail reads). M4A's seek-reader bytes are not counted here (they live in
/// musefs-format); M4A's win shows in `wall_ms` + `peak_rss_kib`. `opens`/`preads`
/// remain serve-path counters and stay ~0 on the scan path.
```

- [ ] **Step 3: Run the benches to verify they print the new column**

Run: `cargo test -p musefs-core --features metrics --test bench_ingest -- --ignored --nocapture bench_cold_scan_and_revalidate`
Expected: a table including a populated `bytes_read` column; `scan` rows show `bytes_read` far below the corpus's per-file size for FLAC/MP3/OGG.

- [ ] **Step 4: Run the full suite**

Run: `cargo test --workspace` and `cargo clippy --all-targets -- -D warnings`
Expected: PASS / clean.

- [ ] **Step 5: Commit**

```bash
git add musefs-core/tests/common/report.rs musefs-core/tests/bench_ingest.rs
git commit -m "test(bench): report scan_bytes_read + MUSEFS_BENCH_JOBS dimension"
```

---

# Final verification (before opening the PR)

- [ ] `cargo test --workspace` green (includes `probe_equivalence`).
- [ ] `cargo test -p musefs-format --features fuzzing` green (format proptests under feature unification).
- [ ] `cargo clippy --all-targets -- -D warnings` clean; `cargo fmt --check` clean.
- [ ] `cargo test -p musefs-fuse -- --ignored` green on `/dev/fuse` (byte-identical e2e holds — SP1 doesn't touch the serve path, but confirm).
- [ ] Bench evidence captured for the results log:
  - `cargo test -p musefs-core --features metrics --test bench_ingest -- --ignored --nocapture` on the `ci` and `large-compute` tiers (compute + `bytes_read` + `peak_rss_kib`), comparing `MUSEFS_BENCH_JOBS=1` vs unset.
  - `bandwidth` tier on a real mount (30 MiB payloads): `MUSEFS_BENCH_TIER=bandwidth MUSEFS_BENCH_DIR=<real-disk> …`.
  - fsync drop via latency FS: `MUSEFS_BENCH_LATENCY_PROFILE=hdd cargo test -p musefs-core --features metrics --test bench_ingest -- --ignored --nocapture bench_scan_under_latency` (batched vs `MUSEFS_BENCH_JOBS=1` per-file).
  - Record tier · storage · wall · bytes_read · fsyncs · peak_rss in `docs/superpowers/specs/2026-05-30-optimization-pass/README.md` results log.
- [ ] Update the SP1 status row in that README to **Implemented**, and link this plan.
- [ ] Finish the branch via `superpowers:finishing-a-development-branch`.

---

## Self-review notes (addressed)

- **Spec coverage:** bounded reads (A2–A6) · M4A seek (A6) · `NeedMore`/widen (A6, A7) · equivalence property (A7) · parallel default-on + `--jobs` (B3, B5) · single writer + batching (B1, B3) · byte-budget backpressure (B3) · bulk pragmas scan-scoped (B1) · resilience `failed` (B2) · revalidate pre-dispatch skip (B4) · scan-path metrics + bench (A1, B6) · all four spec components mapped.
- **M4A bytes-read caveat** is documented (B6) — the in-process scan counter covers front-anchored reads; M4A's win is wall/RSS. Not a correctness gap.
- **Determinism** is asserted on normalized state (B3), never on raw `art.id`, per the spec.
- **Naming consistency:** `read_metadata_bounded` (flac, ogg), `locate_audio_bounded` (mp3, wav), `Extent`, `ScanOptions`, `scan_directory_with` / `revalidate_with`, `BulkWriter`, `ByteBudget`, `ingest_bulk`, `probe_file` / `probe_full` / `probe_prefix` — used identically across tasks.
