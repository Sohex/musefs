# SP4 — Storage-Aware Serving (Backwards-Scan + Algebraic CRC) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the eager whole-audio-region Ogg page index (`build_index` / `OggPageIndex`) with a stateless per-request backwards-scan that finds the page boundary from a ~65 KB window, patches headers algebraically (no payload I/O), and serves payload slices via exact positioned reads.

**Architecture:** `crc_shift_zeros` (crc.rs) enables algebraic CRC patching; `patch_page_header_algebraic` (page.rs) patches a page header from header bytes only; `find_page_start` + `serve_ogg_window` (ogg_index.rs) replace `build_index` + `serve`. The `ResolvedFile` struct drops the `ogg_index: OnceCell` field; the `OggAudio` arm in `read_segments` becomes a direct call to `serve_ogg_window`.

**Tech Stack:** Rust, musefs-format (ogg/crc.rs, ogg/page.rs), musefs-core (ogg_index.rs, reader.rs, facade.rs, tests/read_at.rs). No schema changes, no new dependencies.

---

## File map

| File | Change |
|------|--------|
| `musefs-format/src/ogg/crc.rs` | Add `pub fn crc_shift_zeros` |
| `musefs-format/src/ogg/page.rs` | Update import line 1; add `pub fn patch_page_header_algebraic` |
| `musefs-core/src/ogg_index.rs` | Full replacement: remove all old code; add `find_page_start`, `serve_ogg_window`, new tests |
| `musefs-core/src/reader.rs` | Remove imports (lines 9, 14); remove `ogg_index` field from `ResolvedFile`; remove constants + fn (lines 15–23); simplify `cache_bytes`; replace `OggAudio` arm; fix/delete tests |
| `musefs-core/src/facade.rs` | Remove `ogg_index: OnceCell::new()` from line 697 |
| `musefs-core/tests/read_at.rs` | Remove `ogg_index: once_cell::sync::OnceCell::new()` from line 120 |
| `BENCHMARKS.md` | Record before/after results |
| `docs/superpowers/specs/…/README.md` | Record SP4 results |

---

## Task 1 — `crc_shift_zeros` in crc.rs

**Files:** Modify `musefs-format/src/ogg/crc.rs`

- [ ] **Step 1.1 — Write failing tests**

  Add to the `mod tests` block in `musefs-format/src/ogg/crc.rs` (after the existing `matches_independent_reference` test):

  ```rust
  #[test]
  fn crc_shift_zeros_identity() {
      // Advancing 0 by any n stays 0 (TABLE[0] = 0 ⟹ each step: 0 ^ TABLE[0] = 0).
      assert_eq!(super::crc_shift_zeros(0, 0), 0);
      assert_eq!(super::crc_shift_zeros(0, 1), 0);
      assert_eq!(super::crc_shift_zeros(0, 65285), 0);
  }

  #[test]
  fn crc_shift_zeros_matches_appending_zeros() {
      // Semantic contract: crc_shift_zeros(crc32(data), n) == crc32(data ++ zeros×n).
      let data = b"hello world";
      let crc_start = crc32(data);
      for &n in &[0usize, 1, 10, 1000, 65285] {
          let mut extended = data.to_vec();
          extended.extend(std::iter::repeat(0u8).take(n));
          let expected = crc32(&extended);
          assert_eq!(
              super::crc_shift_zeros(crc_start, n),
              expected,
              "n = {n}"
          );
      }
  }
  ```

- [ ] **Step 1.2 — Run to verify they fail**

  ```bash
  cargo test -p musefs-format ogg::crc::tests::crc_shift_zeros 2>&1 | grep -E "FAILED|error"
  ```

  Expected: compile error — `super::crc_shift_zeros` not found.

- [ ] **Step 1.3 — Implement `crc_shift_zeros`**

  Add after the closing brace of `crc32` (before `#[cfg(test)]`) in `musefs-format/src/ogg/crc.rs`:

  ```rust
  /// Advance the CRC register by `n` zero-byte steps (equivalent to multiplying by
  /// x^(8n) in GF(2)[x] / poly). Since the Ogg CRC has init=0 and no final XOR it
  /// is linear: crc32(msg ++ zeros×n) == crc_shift_zeros(crc32(msg), n).
  pub fn crc_shift_zeros(mut crc: u32, n: usize) -> u32 {
      for _ in 0..n {
          crc = (crc << 8) ^ TABLE[(crc >> 24) as usize];
      }
      crc
  }
  ```

- [ ] **Step 1.4 — Run tests**

  ```bash
  cargo test -p musefs-format ogg::crc::tests 2>&1 | grep -E "test .* ok|FAILED"
  ```

  Expected: all three tests pass (`matches_independent_reference`, `crc_shift_zeros_identity`, `crc_shift_zeros_matches_appending_zeros`).

- [ ] **Step 1.5 — Commit**

  ```bash
  git add musefs-format/src/ogg/crc.rs
  git commit -m "$(cat <<'EOF'
  SP4: add crc_shift_zeros for algebraic CRC patching

  Advances the Ogg CRC register by n zero-byte steps without payload I/O.
  Enables patch_page_header_algebraic to compute the new page CRC from
  header bytes alone (no payload read needed).

  Co-Authored-By: Claude Sonnet 4.6 <noreply@anthropic.com>
  EOF
  )"
  ```

---

## Task 2 — `patch_page_header_algebraic` in page.rs

**Files:** Modify `musefs-format/src/ogg/page.rs`

- [ ] **Step 2.1 — Update the import line**

  Line 1 of `musefs-format/src/ogg/page.rs` currently reads:

  ```rust
  use super::crc::crc32;
  ```

  Change it to:

  ```rust
  use super::crc::{crc32, crc_shift_zeros};
  ```

- [ ] **Step 2.2 — Write the failing test**

  Add inside the `#[cfg(test)] mod tests` block at the bottom of `page.rs`, after any existing tests:

  ```rust
  #[test]
  fn patch_algebraic_matches_full_page() {
      // For each combination of payload size and seq values, the algebraic
      // patch must produce the same header bytes as the full-page oracle.
      for &payload_len in &[0usize, 1, 255, 3000, 65025] {
          for &old_seq in &[0u32, 1, 42, u32::MAX - 5] {
              for &new_seq in &[old_seq, old_seq.wrapping_add(1), old_seq.wrapping_add(10)] {
                  let payload = vec![0xA5u8; payload_len];
                  let (page_bytes, _) = lace_packet(0x1234, old_seq, false, 0, &payload);
                  // Full-page oracle (existing function).
                  let want = patch_page_header(&page_bytes, new_seq).unwrap();
                  // Header-only algebraic version.
                  let h = parse_page(&page_bytes, 0).unwrap();
                  let got =
                      patch_page_header_algebraic(&page_bytes[..h.header_len], new_seq)
                          .unwrap();
                  assert_eq!(
                      got, want,
                      "payload_len={payload_len} old_seq={old_seq} new_seq={new_seq}"
                  );
              }
          }
      }
  }
  ```

- [ ] **Step 2.3 — Run to verify it fails**

  ```bash
  cargo test -p musefs-format ogg::page::tests::patch_algebraic 2>&1 | grep -E "FAILED|error"
  ```

  Expected: compile error — `patch_page_header_algebraic` not found.

- [ ] **Step 2.4 — Implement `patch_page_header_algebraic`**

  Add after the closing brace of `patch_page_header` in `musefs-format/src/ogg/page.rs`:

  ```rust
  /// Patch a page header algebraically — no payload read needed.
  ///
  /// `header` must be exactly `27 + seg_count` bytes (the fixed Ogg page header
  /// plus segment table; seg_count is read from byte 26). Returns the patched
  /// header bytes with `new_seq` written and the CRC updated via:
  ///
  ///   new_crc = old_crc XOR crc32(DELTA)
  ///
  /// where DELTA is the all-zero message of length page_len, except bytes 18–21
  /// hold `old_seq XOR new_seq`. The payload cancels out of the XOR because the
  /// Ogg CRC is linear (init=0, no xorout). `payload_len` is derived from the
  /// segment table (no payload I/O required).
  pub fn patch_page_header_algebraic(header: &[u8], new_seq: u32) -> Result<Vec<u8>> {
      if header.len() < 27 {
          return Err(FormatError::Malformed);
      }
      let seg_count = header[26] as usize;
      let header_len = 27 + seg_count;
      if header.len() < header_len {
          return Err(FormatError::Malformed);
      }
      let payload_len: usize = header[27..header_len].iter().map(|&b| b as usize).sum();
      let old_seq = u32::from_le_bytes(header[18..22].try_into().unwrap());
      let old_crc = u32::from_le_bytes(header[22..26].try_into().unwrap());
      // 18 leading zeros leave the CRC state at 0 (TABLE[0]=0), so we start
      // directly from the 4-byte seq delta, then shift by the trailing zero count.
      let delta_bytes = (old_seq ^ new_seq).to_le_bytes();
      let trailing = 5 + seg_count + payload_len; // bytes 22..page_end are zero in DELTA
      let delta_crc = crc_shift_zeros(crc32(&delta_bytes), trailing);
      let new_crc = old_crc ^ delta_crc;
      let mut out = header[..header_len].to_vec();
      out[18..22].copy_from_slice(&new_seq.to_le_bytes());
      out[22..26].copy_from_slice(&new_crc.to_le_bytes());
      Ok(out)
  }
  ```

- [ ] **Step 2.5 — Run tests**

  ```bash
  cargo test -p musefs-format ogg::page::tests 2>&1 | grep -E "test .* ok|FAILED"
  ```

  Expected: all page tests pass, including `patch_algebraic_matches_full_page`.

- [ ] **Step 2.6 — Run full musefs-format test suite**

  ```bash
  cargo test -p musefs-format 2>&1 | tail -5
  ```

  Expected: all tests pass.

- [ ] **Step 2.7 — Commit**

  ```bash
  git add musefs-format/src/ogg/page.rs
  git commit -m "$(cat <<'EOF'
  SP4: add patch_page_header_algebraic — header-only CRC patching

  Uses crc_shift_zeros to compute new_crc = old_crc XOR crc32(seq_delta
  contribution) from header bytes alone. Algebraically identical to the
  full-page patch_page_header; differential test confirms byte-identity
  across payload sizes 0..65025 and a range of seq values.

  Co-Authored-By: Claude Sonnet 4.6 <noreply@anthropic.com>
  EOF
  )"
  ```

---

## Task 3 — `find_page_start` + `serve_ogg_window` in ogg_index.rs

Add the new functions **alongside** the old ones so the build stays green while the new tests are written and verified. The old API is removed in Task 5.

**Files:** Modify `musefs-core/src/ogg_index.rs`

- [ ] **Step 3.1 — Write failing tests for `find_page_start`**

  Add the following inside `mod tests` in `musefs-core/src/ogg_index.rs`, after the existing tests:

  ```rust
  // ── helpers for the new serve_ogg_window API ──────────────────────────────

  /// Synthetic fixture: 16-byte prefix, then two packets (300 B at seq 5,
  /// 70 000 B at seq 6 spanning 2 pages). Returns (TempDir, path,
  /// audio_offset=16, audio_length).
  fn new_serve_fixture() -> (tempfile::TempDir, std::path::PathBuf, u64, u64) {
      let (mut audio, _) = lace_packet_pub(0xABCD, 5, false, 100, &vec![1u8; 300]);
      let (b2, _) = lace_packet_pub(0xABCD, 6, false, 200, &vec![2u8; 70_000]);
      audio.extend_from_slice(&b2);
      let dir = tempfile::tempdir().unwrap();
      let path = dir.path().join("audio.ogg");
      let mut file = vec![0u8; 16];
      file.extend_from_slice(&audio);
      std::fs::File::create(&path).unwrap().write_all(&file).unwrap();
      let audio_length = audio.len() as u64;
      (dir, path, 16, audio_length)
  }

  /// Build the reference served bytes for seq_delta=2 by applying the full-page
  /// oracle (patch_page_header) to every page and concatenating header+payload.
  fn new_reference_region(path: &std::path::Path, ao: u64, alen: u64) -> Vec<u8> {
      use musefs_format::ogg::{parse_page, patch_page_header};
      let backing = std::fs::File::open(path).unwrap();
      let mut full = vec![0u8; alen as usize];
      backing.read_exact_at(&mut full, ao).unwrap();
      let mut out = Vec::new();
      let mut pos = 0usize;
      while pos < full.len() {
          let h = parse_page(&full, pos).unwrap();
          let new_seq = h.seq.wrapping_add(2);
          let patched = patch_page_header(&full[pos..pos + h.total_len()], new_seq).unwrap();
          out.extend_from_slice(&patched);
          out.extend_from_slice(&full[pos + h.header_len..pos + h.total_len()]);
          pos += h.total_len();
      }
      out
  }

  fn new_serve_range(path: &std::path::Path, ao: u64, alen: u64, a: u64, b: u64) -> Vec<u8> {
      let backing = std::fs::File::open(path).unwrap();
      let mut out = Vec::new();
      serve_ogg_window(&backing, ao, alen, 2, a, b, &mut out).unwrap();
      out
  }

  // ── find_page_start tests ─────────────────────────────────────────────────

  #[test]
  fn find_page_start_at_audio_offset_returns_immediately() {
      let (_d, path, ao, _alen) = new_serve_fixture();
      let backing = std::fs::File::open(&path).unwrap();
      // abs_target == audio_offset → special-case, no backward read.
      assert_eq!(find_page_start(&backing, ao, ao).unwrap(), ao);
  }

  #[test]
  fn find_page_start_mid_page_returns_page_start() {
      let (_d, path, ao, _alen) = new_serve_fixture();
      let backing = std::fs::File::open(&path).unwrap();
      // Parse the first page header to know its length.
      let mut hdr = vec![0u8; 282];
      backing.read_exact_at(&mut hdr, ao).unwrap();
      let h = musefs_format::ogg::parse_page(&hdr, 0).unwrap();
      // Target 10 bytes into the payload of page 0.
      let target = ao + h.header_len as u64 + 10;
      let found = find_page_start(&backing, ao, target).unwrap();
      assert_eq!(found, ao, "mid-payload target should resolve to page 0's start");
  }

  #[test]
  fn find_page_start_at_page_boundary_returns_preceding_page() {
      let (_d, path, ao, _alen) = new_serve_fixture();
      let backing = std::fs::File::open(&path).unwrap();
      let mut hdr = vec![0u8; 282];
      backing.read_exact_at(&mut hdr, ao).unwrap();
      let h = musefs_format::ogg::parse_page(&hdr, 0).unwrap();
      // Target exactly at the boundary between page 0 and page 1.
      // The half-open scan window [start, abs_target) does not include abs_target,
      // so the scan returns page 0's start. The forward pass in serve_ogg_window
      // will skip page 0 (no overlap) and serve from page 1 correctly.
      let page1_abs = ao + h.total_len() as u64;
      let found = find_page_start(&backing, ao, page1_abs).unwrap();
      assert_eq!(found, ao);
  }

  // ── serve_ogg_window tests ────────────────────────────────────────────────

  #[test]
  fn serve_ogg_window_whole_region_matches_reference() {
      let (_d, path, ao, alen) = new_serve_fixture();
      let want = new_reference_region(&path, ao, alen);
      assert_eq!(new_serve_range(&path, ao, alen, 0, alen), want);
  }
  ```

- [ ] **Step 3.2 — Run to verify they fail**

  ```bash
  cargo test -p musefs-core find_page_start serve_ogg_window_whole 2>&1 | grep -E "FAILED|error"
  ```

  Expected: compile error — `find_page_start` and `serve_ogg_window` not found.

- [ ] **Step 3.3 — Add constants and `find_page_start` to `ogg_index.rs`**

  Add the following at the top of `musefs-core/src/ogg_index.rs` (before the existing `use` statements, or after them — keep as a clearly delimited new section):

  ```rust
  use std::os::unix::fs::FileExt;
  use musefs_format::ogg::patch_page_header_algebraic;
  ```

  Then add the constants and `find_page_start` function directly before the `#[cfg(test)]` line:

  ```rust
  /// Maximum Ogg page size in bytes: 27 fixed header + 255 seg-table + 255×255 payload.
  const MAX_OGG_PAGE_BYTES: u64 = 65_307;
  /// Maximum Ogg page header size: 27 fixed + 255 seg-table.
  const MAX_OGG_HEADER_BYTES: usize = 282;

  /// Find the absolute file offset of the Ogg page whose region contains or
  /// immediately precedes `abs_target` within `[audio_offset, audio_offset + ?)`.
  ///
  /// Special case: `abs_target == audio_offset` returns `audio_offset` immediately
  /// (the first audio page always starts there, validated at scan time).
  ///
  /// General case: reads the window `[max(audio_offset, abs_target−65307), abs_target)`
  /// in one `pread` and scans backwards for the rightmost valid OggS page start.
  /// Validity checks: version byte 0, `header_type & 0xF8 == 0`, segment table fits
  /// within the window. A false positive that evades these checks would produce a
  /// malformed-CRC page (detectable by the client's Ogg decoder).
  fn find_page_start(
      backing: &std::fs::File,
      audio_offset: u64,
      abs_target: u64,
  ) -> Result<u64> {
      if abs_target == audio_offset {
          return Ok(audio_offset);
      }
      let scan_start = abs_target
          .saturating_sub(MAX_OGG_PAGE_BYTES)
          .max(audio_offset);
      let window_len = (abs_target - scan_start) as usize;
      let mut window = vec![0u8; window_len];
      backing.read_exact_at(&mut window, scan_start)?;

      // Scan backwards for the rightmost valid OggS capture.
      let mut i = window_len.saturating_sub(4);
      loop {
          if window[i..].starts_with(b"OggS") {
              let ok = window.get(i + 4) == Some(&0)           // version == 0
                  && window.get(i + 5).map_or(false, |&ht| ht & 0xF8 == 0) // header_type
                  && i + 26 < window_len                         // num_segs byte fits
                  && {
                      let ns = window[i + 26] as usize;
                      i + 27 + ns <= window_len                  // seg table fits
                  };
              if ok {
                  return Ok(scan_start + i as u64);
              }
          }
          if i == 0 {
              break;
          }
          i -= 1;
      }
      Err(musefs_format::FormatError::Malformed.into())
  }
  ```

- [ ] **Step 3.4 — Add `serve_ogg_window`**

  Add immediately after `find_page_start` (before `#[cfg(test)]`):

  ```rust
  /// Serve bytes `[rstart, rend)` (relative to the audio region start) into `out`.
  ///
  /// Locates the containing page via a backwards scan, then walks pages forward,
  /// patching each header algebraically (`patch_page_header_algebraic`) and serving
  /// payload slices via exact positioned reads — no full-page I/O and no in-memory
  /// page index.
  ///
  /// Integrity guard (debug builds): asserts that the page walk does not overrun
  /// `audio_offset + audio_length`, which would indicate corrupt or misaligned data.
  pub fn serve_ogg_window(
      backing: &std::fs::File,
      audio_offset: u64,
      audio_length: u64,
      seq_delta: i64,
      rstart: u64,
      rend: u64,
      out: &mut Vec<u8>,
  ) -> Result<()> {
      if rstart >= rend {
          return Ok(());
      }
      let audio_end = audio_offset + audio_length;
      let abs_rstart = audio_offset + rstart;
      let mut pos = find_page_start(backing, audio_offset, abs_rstart)?;

      while pos < audio_end {
          let page_rel = pos - audio_offset;
          if page_rel >= rend {
              break;
          }
          // One pread for the full header (27 + up to 255 seg-table bytes).
          // Clamped to the declared audio region end.
          let read_len = MAX_OGG_HEADER_BYTES.min((audio_end - pos) as usize);
          let mut hdr_buf = vec![0u8; read_len];
          backing.read_exact_at(&mut hdr_buf, pos)?;
          if hdr_buf.len() < 27 {
              return Err(musefs_format::FormatError::Malformed.into());
          }
          let seg_count = hdr_buf[26] as usize;
          let header_len = 27 + seg_count;
          if hdr_buf.len() < header_len {
              return Err(musefs_format::FormatError::Malformed.into());
          }
          let payload_len: usize =
              hdr_buf[27..header_len].iter().map(|&b| b as usize).sum();

          let old_seq = u32::from_le_bytes(hdr_buf[18..22].try_into().unwrap());
          let new_seq = (old_seq as i64 + seq_delta) as u32;
          let patched_hdr =
              patch_page_header_algebraic(&hdr_buf[..header_len], new_seq)
                  .map_err(CoreError::from)?;

          let hdr_end = page_rel + header_len as u64;
          let page_end = hdr_end + payload_len as u64;

          // Header overlap.
          let hs = rstart.max(page_rel);
          let he = rend.min(hdr_end);
          if hs < he {
              let a = (hs - page_rel) as usize;
              let b = (he - page_rel) as usize;
              out.extend_from_slice(&patched_hdr[a..b]);
          }

          // Payload overlap — exactly the bytes requested, no full-page read.
          let ps = rstart.max(hdr_end);
          let pe = rend.min(page_end);
          if ps < pe {
              let within = ps - hdr_end;
              let n = (pe - ps) as usize;
              let start = out.len();
              out.resize(start + n, 0);
              backing.read_exact_at(&mut out[start..], pos + header_len as u64 + within)?;
          }

          pos += (header_len + payload_len) as u64;
      }

      debug_assert!(
          pos <= audio_end,
          "serve_ogg_window: page walk overran audio_end by {} bytes \
           (audio_offset={audio_offset} audio_length={audio_length})",
          pos.saturating_sub(audio_end),
      );
      Ok(())
  }
  ```

- [ ] **Step 3.5 — Run the new tests**

  ```bash
  cargo test -p musefs-core find_page_start serve_ogg_window_whole 2>&1 | grep -E "test .* ok|FAILED"
  ```

  Expected: all four new tests pass.

- [ ] **Step 3.6 — Add remaining serve coverage tests**

  Add inside `mod tests` after `serve_ogg_window_whole_region_matches_reference`:

  ```rust
  #[test]
  fn serve_ogg_window_header_only_read() {
      let (_d, path, ao, alen) = new_serve_fixture();
      let want = new_reference_region(&path, ao, alen);
      // Parse the first page to get header_len.
      let backing = std::fs::File::open(&path).unwrap();
      let mut hdr = vec![0u8; 282];
      backing.read_exact_at(&mut hdr, ao).unwrap();
      let h = musefs_format::ogg::parse_page(&hdr, 0).unwrap();
      let hlen = h.header_len as u64;
      // First 10 bytes of header.
      assert_eq!(new_serve_range(&path, ao, alen, 0, 10), want[..10]);
      // Exactly the whole header of page 0.
      assert_eq!(new_serve_range(&path, ao, alen, 0, hlen), want[..hlen as usize]);
  }

  #[test]
  fn serve_ogg_window_payload_mid_start() {
      // Serve starting 10 bytes into page 0's payload.
      let (_d, path, ao, alen) = new_serve_fixture();
      let want = new_reference_region(&path, ao, alen);
      let backing = std::fs::File::open(&path).unwrap();
      let mut hdr = vec![0u8; 282];
      backing.read_exact_at(&mut hdr, ao).unwrap();
      let h = musefs_format::ogg::parse_page(&hdr, 0).unwrap();
      let hlen = h.header_len as u64;
      let start = hlen + 10;
      let end = hlen + 60;
      assert_eq!(
          new_serve_range(&path, ao, alen, start, end),
          want[start as usize..end as usize]
      );
  }

  #[test]
  fn serve_ogg_window_spanning_header_and_payload() {
      let (_d, path, ao, alen) = new_serve_fixture();
      let want = new_reference_region(&path, ao, alen);
      let backing = std::fs::File::open(&path).unwrap();
      let mut hdr = vec![0u8; 282];
      backing.read_exact_at(&mut hdr, ao).unwrap();
      let h = musefs_format::ogg::parse_page(&hdr, 0).unwrap();
      let hlen = h.header_len as u64;
      let r = (hlen - 5)..(hlen + 20);
      assert_eq!(
          new_serve_range(&path, ao, alen, r.start, r.end),
          want[r.start as usize..r.end as usize]
      );
  }

  #[test]
  fn serve_ogg_window_crossing_page_boundary() {
      let (_d, path, ao, alen) = new_serve_fixture();
      let want = new_reference_region(&path, ao, alen);
      let backing = std::fs::File::open(&path).unwrap();
      let mut hdr = vec![0u8; 282];
      backing.read_exact_at(&mut hdr, ao).unwrap();
      let h = musefs_format::ogg::parse_page(&hdr, 0).unwrap();
      let p0_end = h.total_len() as u64;
      let r = (p0_end - 30)..(p0_end + 40);
      assert_eq!(
          new_serve_range(&path, ao, alen, r.start, r.end),
          want[r.start as usize..r.end as usize]
      );
  }

  #[test]
  fn serve_ogg_window_empty_and_past_end() {
      let (_d, path, ao, alen) = new_serve_fixture();
      let want = new_reference_region(&path, ao, alen);
      // Empty range.
      assert!(new_serve_range(&path, ao, alen, 100, 100).is_empty());
      // Entirely past end.
      assert!(new_serve_range(&path, ao, alen, alen, alen + 50).is_empty());
      // rend clamped to region end.
      assert_eq!(
          new_serve_range(&path, ao, alen, alen - 25, alen + 1000),
          want[(alen - 25) as usize..]
      );
  }

  #[test]
  #[cfg(debug_assertions)]
  #[should_panic(expected = "overran audio_end")]
  fn serve_ogg_window_panics_on_misaligned_audio_length() {
      // audio_length that is not on a page boundary triggers the integrity guard.
      let (bytes, _) = lace_packet_pub(0xABCD, 0, false, 0, &vec![7u8; 300]);
      let dir = tempfile::tempdir().unwrap();
      let path = dir.path().join("a.ogg");
      std::fs::File::create(&path).unwrap().write_all(&bytes).unwrap();
      let audio_length = bytes.len() as u64 - 5;
      let backing = std::fs::File::open(&path).unwrap();
      let mut out = Vec::new();
      serve_ogg_window(&backing, 0, audio_length, 0, 0, audio_length, &mut out).unwrap();
  }
  ```

- [ ] **Step 3.7 — Add oracle roundtrip tests for new API**

  Add inside `mod tests` after the `build_codec_file` helper (keep `assert_clean_bitstream`, `materialize_header_and_audio_params`, and `build_codec_file` unchanged):

  ```rust
  fn oracle_roundtrip_new(file: &[u8]) {
      use musefs_format::ogg::{locate_audio, read_header, synthesize_layout};
      let scan = locate_audio(file).unwrap();
      let header = read_header(file).unwrap();
      let layout =
          synthesize_layout(&header, scan.audio_offset, scan.audio_length, &[], &[]).unwrap();
      let (hdr_bytes, ao, alen, delta) = materialize_header_and_audio_params(&layout);

      let dir = tempfile::tempdir().unwrap();
      let path = dir.path().join("f.ogg");
      std::fs::File::create(&path).unwrap().write_all(file).unwrap();

      let backing = std::fs::File::open(&path).unwrap();
      let mut audio = Vec::new();
      serve_ogg_window(&backing, ao, alen, delta, 0, alen, &mut audio).unwrap();

      let mut full = hdr_bytes;
      full.extend_from_slice(&audio);
      assert_clean_bitstream(&full);
  }

  #[test]
  fn oracle_new_opus_stream_is_clean() {
      let head = b"OpusHead\x01\x02\x38\x01\x80\xbb\x00\x00\x00\x00\x00".as_slice();
      let tags = b"OpusTags\x06\x00\x00\x00musefs\x00\x00\x00\x00".as_slice();
      let audio0 = vec![0xA1u8; 4000];
      let audio1 = vec![0xA2u8; 80_000];
      let file = build_codec_file(0x1234, &[head, tags], &[&audio0, &audio1]);
      oracle_roundtrip_new(&file);
  }

  #[test]
  fn oracle_new_vorbis_stream_is_clean() {
      let id = b"\x01vorbis\x00\x00\x00\x00\x02\x44\xac\x00\x00\x00\x00\x00\x00\x00\xee\x02\x00\x00\x00\x00\x00\x01".as_slice();
      let comment = b"\x03vorbis\x06\x00\x00\x00musefs\x00\x00\x00\x00\x01".as_slice();
      let setup = b"\x05vorbis-setup-stub".as_slice();
      let audio0 = vec![0xB1u8; 5000];
      let file = build_codec_file(0x2222, &[id, comment, setup], &[&audio0]);
      oracle_roundtrip_new(&file);
  }

  #[test]
  fn oracle_new_oggflac_stream_is_clean() {
      let mut p0 = Vec::new();
      p0.extend_from_slice(b"\x7FFLAC");
      p0.extend_from_slice(&[1, 0]);
      p0.extend_from_slice(&1u16.to_be_bytes());
      p0.extend_from_slice(b"fLaC");
      p0.push(0);
      p0.extend_from_slice(&[0, 0, 34]);
      p0.extend_from_slice(&[0u8; 34]);
      let mut comment = Vec::new();
      comment.push(0x84);
      let vc = b"\x06\x00\x00\x00musefs\x00\x00\x00\x00";
      comment.extend_from_slice(&[0, 0, vc.len() as u8]);
      comment.extend_from_slice(vc);
      let audio0 = vec![0xC1u8; 6000];
      let file = build_codec_file(0x3333, &[&p0, &comment], &[&audio0]);
      oracle_roundtrip_new(&file);
  }
  ```

- [ ] **Step 3.8 — Run all new ogg_index tests**

  ```bash
  cargo test -p musefs-core -- ogg_index 2>&1 | grep -E "test .* ok|FAILED"
  ```

  Expected: all new tests pass; all old tests also still pass.

- [ ] **Step 3.9 — Commit**

  ```bash
  git add musefs-core/src/ogg_index.rs
  git commit -m "$(cat <<'EOF'
  SP4: add find_page_start + serve_ogg_window alongside old index API

  find_page_start: backwards-scan ~65 KB window to locate the Ogg page
  containing the request, with OggS capture + header_type/seg_table sanity
  checks. serve_ogg_window: stateless per-request serve — algebraic header
  patch via patch_page_header_algebraic + exact payload pread, no index.
  Oracle tests (Opus, Vorbis, OggFLAC) confirm clean bitstream output.
  Old API (OggPageIndex/build_index/serve) retained temporarily.

  Co-Authored-By: Claude Sonnet 4.6 <noreply@anthropic.com>
  EOF
  )"
  ```

---

## Task 4 — Wire up reader.rs, facade.rs, tests/read_at.rs

All three files must change simultaneously because removing the `ogg_index` field from `ResolvedFile` breaks any struct literal that names it.

**Files:** `musefs-core/src/reader.rs`, `musefs-core/src/facade.rs`, `musefs-core/tests/read_at.rs`

- [ ] **Step 4.1 — Update imports in reader.rs**

  Line 9 currently reads:
  ```rust
  use once_cell::sync::OnceCell;
  ```
  **Delete that line.**

  Line 14 currently reads:
  ```rust
  use crate::ogg_index::{build_index, serve, OggPageIndex};
  ```
  **Replace with:**
  ```rust
  use crate::ogg_index::serve_ogg_window;
  ```

- [ ] **Step 4.2 — Remove dead constants and function**

  Lines 16–23 in reader.rs:
  ```rust
  const OGG_MIN_PAGE_BYTES: u64 = 27;
  const OGG_INDEX_BYTES_PER_PAGE: u64 = 128;

  fn estimated_ogg_index_bytes(audio_length: u64) -> u64 {
      let estimated_pages = audio_length
          .saturating_div(OGG_MIN_PAGE_BYTES)
          .saturating_add(1);
      estimated_pages.saturating_mul(OGG_INDEX_BYTES_PER_PAGE)
  }
  ```
  **Delete all six lines.**

- [ ] **Step 4.3 — Remove `ogg_index` field from `ResolvedFile`**

  In the `ResolvedFile` struct definition, delete:
  ```rust
      /// Lazily built on the first read that touches an `OggAudio` segment; guarded
      /// so concurrent first reads build it once. Empty for non-Ogg files.
      pub ogg_index: OnceCell<Arc<OggPageIndex>>,
  ```

  Also remove `Arc` from imports if it is now unused — check with `cargo check`.
  (`Arc` is still used elsewhere in reader.rs, so it stays.)

- [ ] **Step 4.4 — Simplify `cache_bytes` computation**

  In `impl HeaderCache`, the `build` method computes `cache_bytes` as:
  ```rust
  let cache_bytes = layout
      .segments()
      .iter()
      .map(|s| match s {
          Segment::Inline(b) => b.len() as u64,
          _ => 0,
      })
      .sum::<u64>()
      + match track.format {
          Format::Opus | Format::Vorbis | Format::OggFlac => {
              estimated_ogg_index_bytes(track.audio_length as u64)
          }
          _ => 0,
      };
  ```

  **Replace the entire block** with:
  ```rust
  let cache_bytes = layout
      .segments()
      .iter()
      .map(|s| match s {
          Segment::Inline(b) => b.len() as u64,
          _ => 0,
      })
      .sum::<u64>();
  ```

- [ ] **Step 4.5 — Remove `ogg_index: OnceCell::new()` from `ResolvedFile` construction in `build`**

  In `Ok(Arc::new(ResolvedFile { ... }))` inside the `build` method, delete the line:
  ```rust
          ogg_index: OnceCell::new(),
  ```

- [ ] **Step 4.6 — Replace the `OggAudio` arm in `read_segments`**

  Find the `Segment::OggAudio` arm (currently lines ~440–453):
  ```rust
  Segment::OggAudio {
      offset: ao,
      seq_delta,
      len,
  } => {
      let index = resolved
          .ogg_index
          .get_or_try_init(|| {
              build_index(&resolved.backing_path, *ao, *len, *seq_delta).map(Arc::new)
          })?
          .clone();
      let f = file.expect("ogg-audio segment requires an open backing file");
      serve(&index, f, *ao, within, within + n as u64, &mut out)?;
  }
  ```

  **Replace with:**
  ```rust
  Segment::OggAudio {
      offset: ao,
      seq_delta,
      len,
  } => {
      let f = file.expect("ogg-audio segment requires an open backing file");
      serve_ogg_window(f, *ao, *len, *seq_delta, within, within + n as u64, &mut out)?;
  }
  ```

- [ ] **Step 4.7 — Fix `ResolvedFile` struct literals in reader.rs tests**

  There are multiple `ResolvedFile { ... ogg_index: OnceCell::new(), ... }` literals inside `#[cfg(test)]` blocks. Remove `ogg_index: OnceCell::new(),` from every one. They appear in: `ogg_serve_tests`, `ogg_art_serve_tests`, and `cache_bound_tests` modules.

  Locate all occurrences:
  ```bash
  grep -n "ogg_index" musefs-core/src/reader.rs
  ```
  Delete each `ogg_index: OnceCell::new(),` line found.

- [ ] **Step 4.8 — Delete `ogg_index_estimate_accounts_page_dense_files`**

  Find and delete the entire test function (lines ~892–903):
  ```rust
  #[test]
  fn ogg_index_estimate_accounts_page_dense_files() {
      assert_eq!(estimated_ogg_index_bytes(0), OGG_INDEX_BYTES_PER_PAGE);
      assert_eq!(
          estimated_ogg_index_bytes(OGG_MIN_PAGE_BYTES),
          OGG_INDEX_BYTES_PER_PAGE * 2
      );
      assert!(
          estimated_ogg_index_bytes(8 * 1024) > OGG_INDEX_BYTES_PER_PAGE * 100,
          "8 KiB of tiny Ogg pages must cost far more than one average page"
      );
  }
  ```

- [ ] **Step 4.9 — Rewrite `build_cache_bytes_includes_ogg_index_estimate`**

  Find the test at ~line 731. It currently asserts `cache_bytes == inline_sum + estimated_ogg_index_bytes(audio_length)`.

  Replace the entire test body with:
  ```rust
  #[test]
  fn build_cache_bytes_counts_inline_segments_for_ogg() {
      use musefs_db::{Format, NewTrack};
      let dir = tempfile::tempdir().unwrap();
      let path = dir.path().join("a.opus");
      let (audio_offset, audio_length) = build_opus_file(&path);
      let db = musefs_db::Db::open_in_memory().unwrap();
      let meta = std::fs::metadata(&path).unwrap();
      let id = db
          .upsert_track(&NewTrack {
              backing_path: path.to_string_lossy().to_string(),
              format: Format::Opus,
              audio_offset: audio_offset as i64,
              audio_length: audio_length as i64,
              backing_size: meta.len() as i64,
              backing_mtime: mtime_secs(&meta),
          })
          .unwrap();
      let cache = HeaderCache::new(Mode::Synthesis);
      let resolved = cache.resolve(&db, id).unwrap();
      let inline_sum: u64 = resolved
          .layout
          .segments()
          .iter()
          .map(|s| match s {
              Segment::Inline(b) => b.len() as u64,
              _ => 0,
          })
          .sum();
      // SP4: no per-file index estimate; cache_bytes == inline segment bytes only.
      assert_eq!(resolved.cache_bytes, inline_sum);
      assert!(inline_sum > 0, "Opus header should have non-empty inline segments");
  }
  ```

  (Also rename the function from `build_cache_bytes_includes_ogg_index_estimate` to `build_cache_bytes_counts_inline_segments_for_ogg`.)

- [ ] **Step 4.10 — Fix `ResolvedFile` literal in facade.rs**

  In `musefs-core/src/facade.rs` at line ~697:
  ```rust
  ogg_index: OnceCell::new(),
  ```
  **Delete that line.** Also check and remove the `OnceCell` import from facade.rs if it is now unused:
  ```bash
  grep -n "OnceCell" musefs-core/src/facade.rs
  ```
  Delete any unused `use once_cell::sync::OnceCell;` line found.

- [ ] **Step 4.11 — Fix `ResolvedFile` literal in tests/read_at.rs**

  In `musefs-core/tests/read_at.rs` at line ~120:
  ```rust
  ogg_index: once_cell::sync::OnceCell::new(),
  ```
  **Delete that line.**

- [ ] **Step 4.12 — Compile check**

  ```bash
  cargo check -p musefs-core 2>&1 | grep -E "^error"
  ```

  Expected: no errors.

- [ ] **Step 4.13 — Run musefs-core test suite**

  ```bash
  cargo test -p musefs-core 2>&1 | tail -10
  ```

  Expected: all tests pass. If any test references `estimated_ogg_index_bytes`, `OGG_MIN_PAGE_BYTES`, `OGG_INDEX_BYTES_PER_PAGE`, or `ogg_index:`, they were missed — re-run Step 4.7 grep.

- [ ] **Step 4.14 — Commit**

  ```bash
  git add musefs-core/src/reader.rs musefs-core/src/facade.rs musefs-core/tests/read_at.rs
  git commit -m "$(cat <<'EOF'
  SP4: wire serve_ogg_window into reader; drop OggPageIndex field + estimates

  - reader.rs: replace OggAudio arm (get_or_try_init/build_index/serve →
    serve_ogg_window); drop ogg_index field from ResolvedFile; remove
    OnceCell import and the OGG_MIN_PAGE_BYTES/OGG_INDEX_BYTES_PER_PAGE
    constants + estimated_ogg_index_bytes; simplify cache_bytes to
    inline-segment sum only
  - Rewrite build_cache_bytes_counts_inline_segments_for_ogg (was
    build_cache_bytes_includes_ogg_index_estimate); delete
    ogg_index_estimate_accounts_page_dense_files
  - facade.rs, tests/read_at.rs: remove ogg_index struct literal fields

  Co-Authored-By: Claude Sonnet 4.6 <noreply@anthropic.com>
  EOF
  )"
  ```

---

## Task 5 — Remove old API from ogg_index.rs

The old `OggPageIndex`, `IndexedPage`, `build_index`, and `serve` are no longer imported anywhere. Remove them and collapse the old test suite.

**Files:** `musefs-core/src/ogg_index.rs`

- [ ] **Step 5.1 — Delete old production code**

  Remove:
  - The module-level doc comment (lines 1–6, referencing the old eager-index approach)
  - `use std::io::{BufReader, Read, Seek, SeekFrom};`
  - `use std::path::Path;`
  - `use musefs_format::ogg::parse_page;` (top-level; stays in test module if needed)
  - `pub struct IndexedPage { ... }` (with doc comment)
  - `pub struct OggPageIndex { ... }` (with doc comment)
  - `pub fn build_index(...) -> Result<OggPageIndex> { ... }` (with doc comment)
  - `use std::os::unix::fs::FileExt;` — the duplicate one before `pub fn serve`
  - `pub fn serve(...) -> Result<()> { ... }` (with doc comment)

  The file's production section should now contain only:
  - Two `use` statements (`std::os::unix::fs::FileExt`, `musefs_format::ogg::patch_page_header_algebraic`)
  - `use crate::error::{CoreError, Result};`
  - `const MAX_OGG_PAGE_BYTES` and `const MAX_OGG_HEADER_BYTES`
  - `fn find_page_start`
  - `pub fn serve_ogg_window`

- [ ] **Step 5.2 — Remove old tests**

  Inside `mod tests`, delete:
  - `build_index_errors_when_audio_length_is_not_on_a_page_boundary`
  - `build_index_renumbers_and_preserves_payload_length`
  - `serve_fixture()` (the old helper returning `OggPageIndex`)
  - `reference_region()` (the old helper using `OggPageIndex`)
  - `serve_range()` (the old helper calling `serve(idx, ...)`)
  - `serve_whole_region_matches_reference` (old)
  - `serve_header_only_read` (old)
  - `serve_payload_only_read_starting_mid_payload` (old)
  - `serve_spanning_header_and_payload` (old)
  - `serve_crossing_page_boundary` (old)
  - `serve_empty_and_past_end_reads` (old)
  - `CRC_32_OGG` constant and `assert_clean_bitstream` — **keep** these (used by oracle tests)
  - `materialize_header_and_audio_params` — **keep**
  - `build_codec_file` — **keep**
  - `oracle_roundtrip()` (old, using `build_index`) — **delete**
  - `oracle_opus_stream_is_clean_after_synth_and_serve` (old) — **delete**
  - `oracle_vorbis_stream_is_clean_after_synth_and_serve` (old) — **delete**
  - `oracle_oggflac_stream_is_clean_after_synth_and_serve` (old) — **delete**

  The new oracle tests (`oracle_new_*`, `oracle_roundtrip_new`) added in Task 3 stay.

- [ ] **Step 5.3 — Update the module-level doc comment**

  Replace the deleted doc comment at the top with:

  ```rust
  //! Per-request Ogg audio serving via backwards-scan and algebraic CRC patching.
  //! Replaces the eager whole-region `build_index` with a stateless strategy:
  //! `find_page_start` locates the containing page via a ~65 KB backwards read;
  //! `serve_ogg_window` patches headers algebraically and serves payload slices
  //! via exact positioned reads — no in-memory index, no first-read scan cost.
  ```

- [ ] **Step 5.4 — Compile check**

  ```bash
  cargo check -p musefs-core 2>&1 | grep "^error"
  ```

  Expected: no errors.

- [ ] **Step 5.5 — Run full test suite**

  ```bash
  cargo test -p musefs-core 2>&1 | tail -10
  cargo test -p musefs-format 2>&1 | tail -5
  ```

  Expected: all tests pass across both crates.

- [ ] **Step 5.6 — Commit**

  ```bash
  git add musefs-core/src/ogg_index.rs
  git commit -m "$(cat <<'EOF'
  SP4: remove OggPageIndex/build_index/serve; ogg_index.rs net reduction

  Old eager-scan code deleted: OggPageIndex, IndexedPage, build_index,
  serve, and all dependent tests. New module: find_page_start (backwards
  scan) + serve_ogg_window (algebraic CRC patch + exact payload pread).
  Oracle tests (Opus, Vorbis, OggFLAC) confirm byte-identical bitstream.

  Co-Authored-By: Claude Sonnet 4.6 <noreply@anthropic.com>
  EOF
  )"
  ```

---

## Task 6 — Full validation suite

- [ ] **Step 6.1 — Proptest byte-identical gate**

  ```bash
  cargo test -p musefs-core --test proptest_read_fidelity -- --nocapture 2>&1 | tail -5
  ```

  Expected: all cases pass (`0 failures`).

- [ ] **Step 6.2 — Format-layer fuzz**

  ```bash
  cargo test -p musefs-format --features fuzzing 2>&1 | tail -5
  ```

  Expected: all tests pass.

- [ ] **Step 6.3 — Record SP3 baseline then run sequential_read bench**

  The SP3 Ogg baseline from BENCHMARKS.md: `ogg 965→948 µs`. Capture SP4 numbers:

  ```bash
  cargo bench -p musefs-core --bench read_throughput -- sequential_read 2>&1 | grep -E "ogg|opus|oggflac|Vorbis"
  ```

  Check: no Ogg/Opus/OggFLAC format rises >10% above the SP3 baseline. Record medians.

  **If any format rises >10%:** the `crc_shift_zeros` loop is causing regression in the warm path. Implement the O(log n) GF(2) polynomial exponentiation replacement for `crc_shift_zeros`:

  <details>
  <summary>O(log n) fallback — implement only if regression gate is breached</summary>

  In `musefs-format/src/ogg/crc.rs`, replace `crc_shift_zeros` with:

  ```rust
  pub fn crc_shift_zeros(crc: u32, n: usize) -> u32 {
      if n == 0 || crc == 0 {
          return crc;
      }
      // Compute x^(8n) mod poly using repeated squaring, then apply as a linear
      // map to the CRC register. Each CRC state bit i maps to a 32-bit row of
      // the GF(2) transition matrix; we square the matrix log2(n) times.
      //
      // Represent polynomials in the same MSB-first convention as the CRC step:
      // "multiply by x" = one zero-byte step of the CRC register from state `poly_val`.
      fn poly_step(p: u32) -> u32 {
          (p << 8) ^ TABLE[(p >> 24) as usize]
      }
      // Build the 32-row transition matrix for one zero-byte step.
      // Row i = poly_step applied to the basis vector (1 << (31-i)).
      let mut mat: [u32; 32] = [0u32; 32];
      for i in 0..32u32 {
          mat[i as usize] = poly_step(1u32 << (31 - i));
      }
      // Matrix–matrix multiply in GF(2): result[i][j] = OR of mat_a[i] & mat_b col j.
      fn mat_mul(a: &[u32; 32], b: &[u32; 32]) -> [u32; 32] {
          let mut r = [0u32; 32];
          for i in 0..32usize {
              for j in 0..32usize {
                  if (a[i] >> (31 - j)) & 1 == 1 {
                      r[i] ^= b[j];
                  }
              }
          }
          r
      }
      // Raise mat to the power 8n via repeated squaring.
      let mut power = mat;
      let mut result = {
          // Identity matrix.
          let mut id = [0u32; 32];
          for i in 0..32usize { id[i] = 1u32 << (31 - i); }
          id
      };
      let mut exp = 8 * n;
      while exp > 0 {
          if exp & 1 == 1 {
              result = mat_mul(&result, &power);
          }
          power = mat_mul(&power, &power);
          exp >>= 1;
      }
      // Apply result matrix to crc (matrix-vector multiply).
      let mut out = 0u32;
      for i in 0..32usize {
          let bit = (crc >> (31 - i)) & 1;
          if bit == 1 {
              out ^= result[i];
          }
      }
      out
  }
  ```

  Re-run the bench to confirm the regression is resolved.
  </details>

- [ ] **Step 6.4 — concurrent_read_walk bench**

  ```bash
  cargo bench -p musefs-core --bench read_throughput -- concurrent_read_walk 2>&1 | grep "m16_plus_walker"
  ```

  Record the median. Expect parity or improvement vs SP3 (`8.35 ms`).

- [ ] **Step 6.5 — In-diff mutation gate**

  ```bash
  cargo mutants -p musefs-core --file src/ogg_index.rs \
    -p musefs-format --file src/ogg/crc.rs \
    -p musefs-format --file src/ogg/page.rs \
    -j$(nproc) 2>&1 | tail -20
  ```

  Record caught/missed/unviable counts.

- [ ] **Step 6.6 — FUSE e2e (requires /dev/fuse)**

  ```bash
  cargo test -p musefs-fuse -- --ignored --nocapture 2>&1 | tail -10
  ```

  Expected: `all_supported_formats_decode_to_same_pcm_sha_as_source` and `end_to_end_read_through_mount` both pass.

- [ ] **Step 6.7 — Record results in BENCHMARKS.md**

  Add a new section "SP4 — Storage-aware serving residuals" to `BENCHMARKS.md` with:
  - `sequential_read` medians before/after for Ogg/Opus/OggFLAC
  - `concurrent_read_walk/m16_plus_walker` before/after
  - Mutation gate counts

  ```bash
  git add BENCHMARKS.md
  git commit -m "$(cat <<'EOF'
  SP4 benchmarks: backwards-scan + algebraic CRC results

  Co-Authored-By: Claude Sonnet 4.6 <noreply@anthropic.com>
  EOF
  )"
  ```

- [ ] **Step 6.8 — Update tracking README**

  Add SP4 entry to the Results log in
  `docs/superpowers/specs/2026-05-30-optimization-pass/README.md`:

  ```markdown
  - **SP4 — Storage-aware serving residuals** (2026-06-XX, box · tempfs · Criterion `ci`
    tier): three changes — `OggPageIndex`/`build_index`/`serve` replaced by stateless
    `find_page_start` (backwards ~65 KB window scan) + `serve_ogg_window` (algebraic
    CRC via `crc_shift_zeros`; no payload I/O); `ResolvedFile.ogg_index` OnceCell
    removed; `cache_bytes` simplified to inline-segment sum. `sequential_read` medians,
    before → after: **[fill from bench]**. `concurrent_read_walk/m16_plus_walker`:
    **[fill]**. Byte-identical gate: proptest + FUSE e2e green.
    Mutation gate: **N caught / M unviable / 0 missed**.
  ```

  ```bash
  git add docs/superpowers/specs/2026-05-30-optimization-pass/README.md
  git commit -m "$(cat <<'EOF'
  SP4 tracking: update README results log

  Co-Authored-By: Claude Sonnet 4.6 <noreply@anthropic.com>
  EOF
  )"
  ```

- [ ] **Step 6.9 — Latency-injected run (VPS — storage-aware validation)**

  On the VPS (needs `/dev/fuse` and `musefs-latencyfs`):

  ```bash
  MUSEFS_BENCH_LATENCY_PROFILE=nfs-hdd MUSEFS_BENCH_TIER=large-compute \
    cargo bench -p musefs-core --bench read_throughput -- sequential_read 2>&1 \
    | grep -E "ogg|opus|oggflac"
  ```

  Compare first-iteration latency for Ogg formats before vs after (the cold-first-read O(whole file) scan was the primary NFS-HDD pain point). Record in BENCHMARKS.md.
