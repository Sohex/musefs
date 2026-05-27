# Optimization Phase 6 — Bounded-Memory M4A/M4B Read Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Stop slurping the entire M4A/M4B file into memory on every resolve — read only the structural boxes (`ftyp`, `moov`, `mdat` header) by seeking, skipping the multi-hundred-MB `mdat` payload.

**Architecture:** Add a streaming `mp4::read_structure_from<R: Read + Seek>` to the format layer that header-walks the top-level boxes (reading only 8/16-byte headers), then reads `ftyp` + `moov` fully and the `mdat` header only — producing an `Mp4Scan` byte-identical to the existing buffer-based `read_structure`. The core reader opens the backing file and calls it instead of `std::fs::read`. Box parsing/validation stays in the format layer; IO stays in core.

**Tech Stack:** Rust, `std::io::{Read, Seek}`, `thiserror`.

---

## Context the implementer needs

- **The MP4 box model (already implemented in `musefs-format/src/mp4.rs`):** a file is a flat sequence of top-level boxes, each `[size:u32][type:[u8;4]][payload…]`. `size==1` means a 64-bit `largesize` follows the type (header is 16 bytes); `size==0` means the box extends to EOF. The supported shape is exactly one `ftyp`, one `moov`, one `mdat`, no `moof`/`mvex`, a single `soun` track. `moov` (the sample tables we re-tag) is small (KB–low-MB); `mdat` (the audio) is the huge part and may sit **after** `moov` or **before** it.
- **Why this matters:** `musefs-core/src/reader.rs` (the `Format::M4a` arm) currently does `std::fs::read(&track.backing_path)` then `mp4::read_structure(&bytes)`. For an audiobook that is a several-hundred-MB allocation on *every* layout resolve (every header-cache miss). We must read only `ftyp`+`moov`+`mdat`-header.
- **Byte-identity is the hard gate.** The served audio must stay byte-for-byte identical. `Mp4Scan` produced by the new path MUST equal the one from the buffer path; `synthesize_layout` is unchanged, so identical `Mp4Scan` ⇒ identical output. The independent-oracle test `musefs-format/tests/mp4_oracle.rs` (parses our synthesized output with the external `mp4` crate and compares samples) guards the whole pipeline.
- **Existing helpers you will reuse (all in `mp4.rs`):**
  - `fn be_u32(b: &[u8], pos: usize) -> Result<u32>`, `fn be_u64(b: &[u8], pos: usize) -> Result<u64>` (return `FormatError::Malformed` on short input).
  - `fn child_boxes(buf: &[u8]) -> Result<Vec<BoxRef>>`, `fn find_box(buf, kind)`, `fn find_path(buf, &[kind,…])`, `BoxRef::payload(&buf)`.
  - Test helpers in `mod tests`: `fn bx(kind: &[u8;4], payload: &[u8]) -> Vec<u8>` and `fn mk_mp4(moov_first: bool, mdat_payload: &[u8], stco_entries: &[u32]) -> Vec<u8>` (builds a minimal accepted MP4).
  - `Result<T>` in `mp4.rs` is `std::result::Result<T, FormatError>`.
- **`Mp4Scan` fields** (must be reproduced exactly): `ftyp: Vec<u8>`, `moov: Vec<u8>`, `mdat_header: Vec<u8>`, `mdat_payload_offset: u64`, `mdat_payload_len: u64`. The buffer path sets them as `buf[ftyp.start..end]`, `buf[moov.start..end]`, `buf[mdat.start..payload_start]`, `mdat.payload_start()`, `mdat.total_len - mdat.header_len`.
- **`CoreError`** (`musefs-core/src/error.rs`) has `Io(#[from] std::io::Error)` and `Format(#[from] musefs_format::FormatError)`.

## File structure

- `musefs-format/src/mp4.rs` — add `Mp4ScanError`, `BoxHeader`, `box_header`, `read_structure_from`; extract `validate_moov` (shared by `locate` and the streaming path); derive `PartialEq` on `Mp4Scan`; add unit tests.
- `musefs-core/src/reader.rs` — `Format::M4a` arm: open file + `read_structure_from` instead of `std::fs::read` + `read_structure`.

## Out of scope

- The scanner (`musefs-core/src/scan.rs`) also `std::fs::read`s whole files, but that is a one-time maintenance pass (not the per-resolve hot path the spec targets) and is generic across all formats. Leave it; a streaming scan is a separate follow-up.
- The buffer-based `mp4::read_structure(&[u8])` stays (used by scan and the oracle/unit tests).

---

## Task 1: Format-layer primitives (`box_header`, `Mp4ScanError`, `validate_moov`, `Mp4Scan: PartialEq`)

**Files:**
- Modify: `musefs-format/src/mp4.rs`

- [ ] **Step 1: Write the failing unit tests**

In `mp4.rs`'s `#[cfg(test)] mod tests` block, add:
```rust
    #[test]
    fn box_header_parses_8_byte_16_byte_and_size0() {
        // 8-byte header: size 16, type "moov".
        let mut h = 16u32.to_be_bytes().to_vec();
        h.extend_from_slice(b"moov");
        let bh = box_header(&h, 1000).unwrap();
        assert_eq!(&bh.kind, b"moov");
        assert_eq!(bh.header_len, 8);
        assert_eq!(bh.total_len, 16);

        // 64-bit largesize: size32==1, then u64 size = 40.
        let mut h = 1u32.to_be_bytes().to_vec();
        h.extend_from_slice(b"mdat");
        h.extend_from_slice(&40u64.to_be_bytes());
        let bh = box_header(&h, 1000).unwrap();
        assert_eq!(bh.header_len, 16);
        assert_eq!(bh.total_len, 40);

        // size32==0 means "extends to EOF" -> total_len == remaining.
        let mut h = 0u32.to_be_bytes().to_vec();
        h.extend_from_slice(b"mdat");
        let bh = box_header(&h, 500).unwrap();
        assert_eq!(bh.header_len, 8);
        assert_eq!(bh.total_len, 500);
    }

    #[test]
    fn box_header_rejects_impossible_sizes() {
        // total_len < header_len.
        let mut h = 4u32.to_be_bytes().to_vec();
        h.extend_from_slice(b"moov");
        assert_eq!(box_header(&h, 1000), Err(FormatError::Malformed));
        // total_len > remaining.
        let mut h = 2000u32.to_be_bytes().to_vec();
        h.extend_from_slice(b"moov");
        assert_eq!(box_header(&h, 100), Err(FormatError::Malformed));
    }
```

- [ ] **Step 2: Run to verify FAIL**

Run: `cargo test -p musefs-format box_header 2>&1 | head -20`
Expected: FAIL — `cannot find function box_header` / `cannot find type BoxHeader`.

- [ ] **Step 3: Add `BoxHeader` + `box_header` and `Mp4ScanError`**

Near the top of `mp4.rs` (after the `use` lines), add the imports needed for the streaming path:
```rust
use std::io::{self, Read, Seek, SeekFrom};
```
After the `BoxRef` impl block, add:
```rust
/// A parsed box header (the payload need not be in memory). Public so the core
/// reader can reason about box bounds while seeking.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BoxHeader {
    pub kind: [u8; 4],
    pub header_len: u64, // 8, or 16 for a 64-bit largesize
    pub total_len: u64,  // header + payload
}

/// Parse a box header from `hdr` (>= 8 bytes; >= 16 if it uses a 64-bit
/// largesize). `remaining` is the byte count from this box's start to EOF, used
/// to resolve a `size == 0` ("extends to end") box.
pub fn box_header(hdr: &[u8], remaining: u64) -> Result<BoxHeader> {
    let size32 = be_u32(hdr, 0)? as u64;
    let kind: [u8; 4] = hdr
        .get(4..8)
        .ok_or(FormatError::Malformed)?
        .try_into()
        .unwrap();
    let (header_len, total_len) = match size32 {
        1 => (16u64, be_u64(hdr, 8)?),
        0 => (8u64, remaining),
        n => (8u64, n),
    };
    if total_len < header_len || total_len > remaining {
        return Err(FormatError::Malformed);
    }
    Ok(BoxHeader {
        kind,
        header_len,
        total_len,
    })
}

/// Error from the seeking MP4 reader: an IO failure reading the file, or a
/// structural/format problem. Kept distinct so the core layer can map IO to
/// `CoreError::Io` (preserving errno) and format to `CoreError::Format`.
#[derive(Debug, thiserror::Error)]
pub enum Mp4ScanError {
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error(transparent)]
    Format(#[from] FormatError),
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p musefs-format box_header 2>&1 | tail -10`
Expected: PASS (2 tests).

- [ ] **Step 5: Extract `validate_moov` and derive `PartialEq` on `Mp4Scan`**

Add `PartialEq` to the `Mp4Scan` derive (it currently derives `Debug, Clone`):
```rust
#[derive(Debug, Clone, PartialEq)]
pub struct Mp4Scan {
```
Add a shared validator (place it just above `locate`):
```rust
/// Validate the internal `moov` shape: no fragmentation (`mvex`), exactly one
/// track, and that track is audio (`soun`). `moov_payload` is the bytes inside
/// the `moov` box (after its header).
fn validate_moov(moov_payload: &[u8]) -> Result<()> {
    if find_box(moov_payload, b"mvex")?.is_some() {
        return Err(FormatError::NotMp4);
    }
    let traks: Vec<_> = child_boxes(moov_payload)?
        .into_iter()
        .filter(|b| &b.kind == b"trak")
        .collect();
    if traks.len() != 1 {
        return Err(FormatError::NotMp4);
    }
    let trak = traks[0].payload(moov_payload);
    let (hp, hl) = find_path(trak, &[b"mdia", b"hdlr"])?.ok_or(FormatError::NotMp4)?;
    if trak[hp..hp + hl].get(8..12) != Some(b"soun") {
        return Err(FormatError::NotMp4);
    }
    Ok(())
}
```
Then refactor `locate` so its `moov`-internal block calls the extracted fn. Replace, inside `locate`, the block:
```rust
    let moov_payload = moov.payload(buf);
    if find_box(moov_payload, b"mvex")?.is_some() {
        return Err(FormatError::NotMp4);
    }
    let traks: Vec<_> = child_boxes(moov_payload)?
        .into_iter()
        .filter(|b| &b.kind == b"trak")
        .collect();
    if traks.len() != 1 {
        return Err(FormatError::NotMp4);
    }
    let trak = traks[0].payload(moov_payload);
    let (hp, hl) = find_path(trak, &[b"mdia", b"hdlr"])?.ok_or(FormatError::NotMp4)?;
    if trak[hp..hp + hl].get(8..12) != Some(b"soun") {
        return Err(FormatError::NotMp4);
    }
    Ok((ftyp, moov, mdat))
```
with:
```rust
    validate_moov(moov.payload(buf))?;
    Ok((ftyp, moov, mdat))
```

- [ ] **Step 6: Verify the refactor is behavior-preserving**

Run: `cargo test -p musefs-format 2>&1 | tail -12`
Expected: all existing mp4 tests still pass (the `locate`/`read_structure` refactor is behavior-identical).
Run: `cargo clippy -p musefs-format --all-targets 2>&1 | tail -5` → no warnings.
Run: `cargo fmt -p musefs-format -- --check` → clean.

- [ ] **Step 7: Commit**

```bash
git add musefs-format/src/mp4.rs
git commit -m "refactor(format): mp4 box_header + Mp4ScanError + extract validate_moov"
```

---

## Task 2: Streaming `read_structure_from` (seek-based, payload never read)

**Files:**
- Modify: `musefs-format/src/mp4.rs`

- [ ] **Step 1: Write the failing tests**

In `mp4.rs`'s `mod tests`, add:
```rust
    #[test]
    fn read_structure_from_matches_buffer_path() {
        // Both moov-first and moov-last (moov-last is the audiobook spike case).
        for moov_first in [true, false] {
            let buf = mk_mp4(moov_first, &vec![0xABu8; 4096], &[0]);
            let from_buf = read_structure(&buf).unwrap();
            let mut cur = std::io::Cursor::new(buf.clone());
            let from_stream = read_structure_from(&mut cur, buf.len() as u64).unwrap();
            assert_eq!(from_stream, from_buf);
        }
    }

    #[test]
    fn read_structure_from_never_reads_mdat_payload() {
        // moov LAST: reaching it requires skipping the mdat payload.
        let buf = mk_mp4(false, &vec![0xCDu8; 100_000], &[0]);
        let scan = read_structure(&buf).unwrap();
        let pay_start = scan.mdat_payload_offset;
        let pay_end = pay_start + scan.mdat_payload_len;

        // A reader that records every byte range it is asked to read.
        struct Tracking {
            inner: std::io::Cursor<Vec<u8>>,
            touched: Vec<(u64, u64)>,
        }
        impl std::io::Read for Tracking {
            fn read(&mut self, b: &mut [u8]) -> std::io::Result<usize> {
                let off = self.inner.position();
                let n = std::io::Read::read(&mut self.inner, b)?;
                self.touched.push((off, off + n as u64));
                Ok(n)
            }
        }
        impl std::io::Seek for Tracking {
            fn seek(&mut self, p: std::io::SeekFrom) -> std::io::Result<u64> {
                self.inner.seek(p)
            }
        }

        let mut tr = Tracking {
            inner: std::io::Cursor::new(buf.clone()),
            touched: Vec::new(),
        };
        let from_stream = read_structure_from(&mut tr, buf.len() as u64).unwrap();
        assert_eq!(from_stream, scan);
        for (s, e) in &tr.touched {
            assert!(
                *e <= pay_start || *s >= pay_end,
                "read [{s},{e}) overlaps mdat payload [{pay_start},{pay_end})"
            );
        }
    }
```

- [ ] **Step 2: Run to verify FAIL**

Run: `cargo test -p musefs-format read_structure_from 2>&1 | head -20`
Expected: FAIL — `cannot find function read_structure_from`.

- [ ] **Step 3: Implement `read_structure_from`**

Add after `read_structure` in `mp4.rs`:
```rust
/// Read the structural boxes (`ftyp`, `moov`, and the `mdat` header) by seeking,
/// **never** reading the `mdat` payload — for audiobooks that payload is hundreds
/// of MB and is served from the backing file at read time. Produces an `Mp4Scan`
/// byte-identical to `read_structure` on the same file, so synthesis is unchanged.
///
/// The header walk reads only 8 bytes per top-level box (16 for a 64-bit
/// largesize), so it skips over the `mdat` payload to reach a trailing `moov`.
pub fn read_structure_from<R: Read + Seek>(
    r: &mut R,
    file_len: u64,
) -> std::result::Result<Mp4Scan, Mp4ScanError> {
    fn region<R: Read + Seek>(r: &mut R, off: u64, len: usize) -> io::Result<Vec<u8>> {
        r.seek(SeekFrom::Start(off))?;
        let mut buf = vec![0u8; len];
        r.read_exact(&mut buf)?;
        Ok(buf)
    }

    // (start_offset, header) for each box we care about.
    let mut ftyp: Option<(u64, BoxHeader)> = None;
    let mut moov: Option<(u64, BoxHeader)> = None;
    let mut mdat: Option<(u64, BoxHeader)> = None;
    let mut dup = false;

    let mut pos = 0u64;
    while pos + 8 <= file_len {
        // Read exactly the header — 8 bytes, plus 8 more only for a largesize box.
        // This guarantees we never touch a box's payload (notably mdat's).
        let first8 = region(r, pos, 8)?;
        let size32 = u32::from_be_bytes(first8[0..4].try_into().unwrap());
        let hdr = if size32 == 1 {
            let mut h = first8;
            h.extend_from_slice(&region(r, pos + 8, 8)?);
            h
        } else {
            first8
        };
        let bh = box_header(&hdr, file_len - pos)?;
        let total = bh.total_len;
        match &bh.kind {
            b"moof" => return Err(FormatError::NotMp4.into()),
            b"ftyp" => {
                if ftyp.replace((pos, bh)).is_some() {
                    dup = true;
                }
            }
            b"moov" => {
                if moov.replace((pos, bh)).is_some() {
                    dup = true;
                }
            }
            b"mdat" => {
                if mdat.replace((pos, bh)).is_some() {
                    dup = true;
                }
            }
            _ => {}
        }
        pos += total;
    }
    if dup {
        return Err(FormatError::NotMp4.into());
    }

    let (ftyp_s, ftyp_h) = ftyp.ok_or(FormatError::NotMp4)?;
    let (moov_s, moov_h) = moov.ok_or(FormatError::NotMp4)?;
    let (mdat_s, mdat_h) = mdat.ok_or(FormatError::NotMp4)?;

    let ftyp_bytes = region(r, ftyp_s, ftyp_h.total_len as usize)?;
    let moov_bytes = region(r, moov_s, moov_h.total_len as usize)?;
    let mdat_header = region(r, mdat_s, mdat_h.header_len as usize)?;

    validate_moov(&moov_bytes[moov_h.header_len as usize..])?;

    Ok(Mp4Scan {
        ftyp: ftyp_bytes,
        moov: moov_bytes,
        mdat_header,
        mdat_payload_offset: mdat_s + mdat_h.header_len,
        mdat_payload_len: mdat_h.total_len - mdat_h.header_len,
    })
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p musefs-format read_structure_from 2>&1 | tail -10`
Expected: PASS (2 tests — equivalence + payload-not-read).

- [ ] **Step 5: Full format gates**

Run: `cargo test -p musefs-format 2>&1 | tail -12` → all pass (incl. the oracle test).
Run: `cargo clippy -p musefs-format --all-targets 2>&1 | tail -5` → no warnings.
Run: `cargo fmt -p musefs-format -- --check` → clean.

- [ ] **Step 6: Commit**

```bash
git add musefs-format/src/mp4.rs
git commit -m "perf(format): seek-based mp4 read_structure_from (never reads mdat payload)"
```

---

## Task 3: Wire the core reader to the streaming path

**Files:**
- Modify: `musefs-core/src/reader.rs` (the `Format::M4a` arm, around line 270)

> **Testing note:** the behavior is guarded end-to-end by `musefs-format/tests/mp4_oracle.rs` (byte-identical samples through our patched offsets) and, at the mount level, by the `#[ignore]` e2e read-through. There is no new core unit test to write here — the change swaps the IO source while `Mp4Scan` (and thus synthesis output) is provably identical (Task 2's equivalence test). The gate is the full test suite + the oracle + (where `/dev/fuse` is present) the e2e.

- [ ] **Step 1: Replace the `Format::M4a` arm body**

In `musefs-core/src/reader.rs`, replace:
```rust
                    Format::M4a => {
                        // The `moov` box may sit at EOF, so the whole file is read and
                        // parsed; the resulting layout's leading inline `head` ends in a
                        // deliberately truncated `mdat` header whose payload is the
                        // backing-audio tail. The generic segment server consumes the
                        // layout as-is — it never re-parses `head` as a complete MP4.
                        let bytes = std::fs::read(&track.backing_path)?;
                        let scan = mp4::read_structure(&bytes)?;
                        mp4::synthesize_layout(&scan, &inputs, &art_inputs)?
                    }
```
with:
```rust
                    Format::M4a => {
                        // Read only the structural boxes (ftyp/moov/mdat header) by
                        // seeking — never the (potentially hundreds-of-MB) mdat payload,
                        // which is served from the backing file at read time. The `moov`
                        // box may sit at EOF; the streaming reader skips the mdat payload
                        // to reach it. The resulting layout's leading inline `head` ends
                        // in a deliberately truncated `mdat` header whose payload is the
                        // backing-audio tail.
                        let mut f = std::fs::File::open(&track.backing_path)?;
                        let len = f.metadata()?.len();
                        let scan = mp4::read_structure_from(&mut f, len).map_err(|e| match e {
                            mp4::Mp4ScanError::Io(io) => CoreError::Io(io),
                            mp4::Mp4ScanError::Format(fe) => CoreError::Format(fe),
                        })?;
                        mp4::synthesize_layout(&scan, &inputs, &art_inputs)?
                    }
```
(`CoreError` is already in scope in `reader.rs`; `mp4` is already imported. If `cargo build` reports `CoreError` not in scope, add `use crate::error::CoreError;` to the file's imports.)

- [ ] **Step 2: Build + lint**

Run: `cargo build -p musefs-core 2>&1 | tail -5` → PASS.
Run: `cargo clippy -p musefs-core --all-targets 2>&1 | tail -5` → no warnings.
Run: `cargo fmt -p musefs-core -- --check` → clean.

- [ ] **Step 3: Full verification (byte-identity is the hard gate)**

Run: `cargo test --workspace 2>&1 | tail -25` → all non-ignored pass (includes `mp4_oracle`).
Run: `cargo test -p musefs-format --test mp4_oracle 2>&1 | tail -10` → PASS (samples byte-identical through patched offsets).
Run: `cargo test -p musefs-fuse -- --ignored --list 2>&1 | tail -10` → links. If `/dev/fuse` is available, run `cargo test -p musefs-fuse -- --ignored 2>&1 | tail -20` and confirm the read-through tests pass; otherwise note it must run on a FUSE-capable host before merge.

- [ ] **Step 4: Commit**

```bash
git add musefs-core/src/reader.rs
git commit -m "perf(core): stream M4A moov from disk on resolve (drop whole-file read)"
```

---

## Final verification (whole phase)

- [ ] `cargo build --workspace 2>&1 | tail -15` → PASS, no warnings.
- [ ] `cargo test --workspace 2>&1 | tail -25` → all non-ignored pass.
- [ ] `cargo clippy --all-targets 2>&1 | tail -15` → no warnings.
- [ ] `cargo fmt --all -- --check` → clean.
- [ ] `cargo test -p musefs-format --test mp4_oracle` → PASS (byte-identity).
- [ ] e2e on a FUSE host: `cargo test -p musefs-fuse -- --ignored` → read-through tests pass.

## Self-review (completed during planning)

- **Spec coverage:** "read only the `moov` region, locating it without slurping `mdat`" → Task 2 `read_structure_from` (header-walk skips mdat payload; the `never_reads_mdat_payload` test proves it). "Served audio stays byte-identical" → Task 2 equivalence test + Task 3's reliance on the unchanged `synthesize_layout` + the mp4 oracle test. "Removes the multi-hundred-MB spike per resolve" → Task 3 swaps `std::fs::read` for the streaming reader.
- **Type consistency:** `BoxHeader { kind, header_len, total_len }`, `box_header(hdr, remaining) -> Result<BoxHeader>`, `Mp4ScanError::{Io, Format}`, `read_structure_from<R: Read+Seek>(r, file_len) -> Result<Mp4Scan, Mp4ScanError>`, and the `Mp4Scan` fields are used identically across tasks and match the existing buffer path.
- **No placeholders:** every code step is complete; the one no-new-test step (Task 3) is justified (behavior is provably identical and oracle/e2e-guarded).
