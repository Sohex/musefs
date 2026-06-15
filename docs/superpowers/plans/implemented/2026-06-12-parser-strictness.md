# Parser Strictness Hardening Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make the FLAC, MP3, WAV, and Ogg parsers reject malformed container geometry with a controlled `FormatError` (at scan *and* synthesis time, per the crafted-DB threat model), and make Ogg page renumbering wrap mod 2³² instead of failing reads.

**Architecture:** Per-format inline hardening in `musefs-format` (one change in `musefs-core` for the Ogg read path). Each format gets a small validation helper shared between its scan and synthesis paths where they overlap. No cross-format abstraction. Four independent issues → four sequenced commits, each green under the pre-commit hook (which runs the full workspace test suite).

**Tech Stack:** Rust (workspace crates `musefs-format`, `musefs-core`); tests are inline `#[cfg(test)] mod tests` plus integration tests under `musefs-format/tests/`.

**Source spec:** `docs/superpowers/specs/2026-06-12-parser-strictness-design.md`

---

## Conventions for every task

- The pre-commit hook runs `cargo fmt`, `cargo clippy -D warnings`, and the **full workspace test suite**. A commit with any red test is rejected, so each task's implementation step and its test-churn step land in the **same commit** — the working tree must be fully green before `git commit`.
- Run `cargo fmt` before every commit (CI fmt gate; check exit status).
- `FormatError` variants used here: `Malformed`, `NotWav`, `NotMp3`, `NotFlac`, `TooLarge` (defined in `musefs-format/src/error.rs`).
- Errors are referenced by function + file; line numbers drift, so navigate by symbol name.

---

## File Structure

| File | Responsibility | Change |
| ---- | -------------- | ------ |
| `musefs-format/src/flac.rs` | FLAC parse/synthesis | Add STREAMINFO structural validation to `parse_blocks`, `read_metadata_bounded`, `synthesize_layout`; update 2 characterization tests + 4 synth-boundary tests; add new tests |
| `musefs-format/src/mp3.rs` | MP3 parse | Add shared `id3v2_header_len` helper; call from `locate_audio`, `locate_audio_bounded`, `id3v2_alloc_safe`; add tests |
| `musefs-format/src/wav.rs` | WAV parse | `riff_wave_start` returns `(start, form_end)`; enforce form bounds in `locate_audio`/`locate_audio_at_ceiling`; fix `wav_front` + `riff_wave_start` test; add tests |
| `musefs-core/src/ogg_index.rs` | Ogg read path | `serve_ogg_window` uses `wrapping_add` for seq; add wrap-boundary test |
| `docs/{FLAC,MP3,OGG,WAV}.md` | Format docs | Document the new strictness / wrap behavior |

---

## Task 1: FLAC STREAMINFO validation (#295)

**Files:**
- Modify: `musefs-format/src/flac.rs` (`parse_blocks`, `read_metadata_bounded`, `synthesize_layout`, tests module)

**Background — current behavior:** `parse_blocks` and `read_metadata_bounded` walk metadata blocks recording `STREAMINFO/APPLICATION/SEEKTABLE/CUESHEET` bodies, never enforcing that STREAMINFO is first, unique, and 34 bytes. `synthesize_layout(structural, …)` sorts blocks by type (`BLOCK_STREAMINFO == 0` sorts first) and emits them, trusting the rows. The DB-fed call site is `HeaderCache::build` in `musefs-core/src/reader.rs`.

- [ ] **Step 1: Add the validation helper and a length constant**

Insert near the top of `musefs-format/src/flac.rs` (after the `MAX_BLOCK_BODY` constant). `BLOCK_STREAMINFO` is already defined as `0`.

```rust
/// FLAC mandates a single STREAMINFO metadata block, first in the sequence, with
/// a fixed 34-byte body.
const STREAMINFO_BODY_LEN: usize = 34;

/// Enforce FLAC's STREAMINFO rule for the metadata block at position `index`:
/// the first block must be STREAMINFO with a 34-byte body, and STREAMINFO must
/// not appear anywhere else (so it appears exactly once). Any violation is
/// `FormatError::Malformed`.
fn check_streaminfo_position(index: usize, block_type: u8, body_len: usize) -> Result<()> {
    let is_streaminfo = block_type == BLOCK_STREAMINFO;
    if index == 0 {
        if !is_streaminfo || body_len != STREAMINFO_BODY_LEN {
            return Err(FormatError::Malformed);
        }
    } else if is_streaminfo {
        return Err(FormatError::Malformed);
    }
    Ok(())
}
```

- [ ] **Step 2: Write failing scan-time tests**

Add to the `tests` module in `musefs-format/src/flac.rs` (helpers `raw_block`, `flac_with`, `BLOCK_*`, `read_metadata_bounded`, `Extent` are already in scope). Also add the reusable `valid_streaminfo` helper used by later steps.

```rust
/// A structural STREAMINFO block with a valid 34-byte body, for synthesis tests.
fn valid_streaminfo() -> MetadataBlock {
    MetadataBlock { block_type: BLOCK_STREAMINFO, body: vec![0u8; 34] }
}

#[test]
fn locate_audio_rejects_missing_streaminfo() {
    // First (and only) block is VORBIS_COMMENT, no STREAMINFO at all.
    let file = flac_with(&[raw_block(BLOCK_VORBIS_COMMENT, &[], true, None)]);
    assert_eq!(locate_audio(&file), Err(FormatError::Malformed));
}

#[test]
fn locate_audio_rejects_streaminfo_wrong_body_len() {
    // STREAMINFO present and first, but body is 10 bytes, not 34.
    let file = flac_with(&[raw_block(BLOCK_STREAMINFO, &[0u8; 10], true, None)]);
    assert_eq!(locate_audio(&file), Err(FormatError::Malformed));
}

#[test]
fn locate_audio_rejects_duplicate_streaminfo() {
    let file = flac_with(&[
        raw_block(BLOCK_STREAMINFO, &[0u8; 34], false, None),
        raw_block(BLOCK_STREAMINFO, &[0u8; 34], true, None),
    ]);
    assert_eq!(locate_audio(&file), Err(FormatError::Malformed));
}

#[test]
fn read_metadata_bounded_rejects_duplicate_streaminfo() {
    let file = flac_with(&[
        raw_block(BLOCK_STREAMINFO, &[0u8; 34], false, None),
        raw_block(BLOCK_STREAMINFO, &[0u8; 34], true, None),
    ]);
    assert_eq!(read_metadata_bounded(&file), Err(FormatError::Malformed));
}

#[test]
fn synthesize_layout_rejects_structural_without_streaminfo() {
    // Hostile DB rows: a SEEKTABLE but no STREAMINFO.
    let structural = [MetadataBlock { block_type: BLOCK_SEEKTABLE, body: vec![0u8; 4] }];
    assert_eq!(
        synthesize_layout(&structural, 0, 0, &[], &[], &[]),
        Err(FormatError::Malformed)
    );
}
```

- [ ] **Step 3: Run the new tests to verify they fail**

Run: `cargo test -p musefs-format flac::tests::locate_audio_rejects_missing_streaminfo flac::tests::synthesize_layout_rejects_structural_without_streaminfo`
Expected: FAIL (current parser accepts these; current `synthesize_layout` returns `Ok`).

- [ ] **Step 4: Enforce the rule in `parse_blocks`**

In `parse_blocks`, add an index counter and call the helper before recording each block. The diff is: add `let mut index = 0usize;` before the `loop`, call the check after `body_end` is validated, and `index += 1;` at the end of each iteration. The block-recording `match` is unchanged.

```rust
    let mut pos = 4usize;
    let mut index = 0usize;
    let mut preserved = Vec::new();
    loop {
        if pos + 4 > data.len() {
            return Err(FormatError::Malformed);
        }
        let header = data[pos];
        let is_last = (header & 0x80) != 0;
        let block_type = header & 0x7F;
        let len = u24_be(data[pos + 1], data[pos + 2], data[pos + 3]);
        let body_start = pos + 4;
        let body_end = body_start + len;
        if body_end > data.len() {
            return Err(FormatError::Malformed);
        }
        check_streaminfo_position(index, block_type, len)?;
        match block_type {
            BLOCK_STREAMINFO | BLOCK_APPLICATION | BLOCK_SEEKTABLE | BLOCK_CUESHEET => {
                preserved.push(MetadataBlock {
                    block_type,
                    body: data[body_start..body_end].to_vec(),
                });
            }
            _ => {}
        }
        pos = body_end;
        index += 1;
        if is_last {
            break;
        }
    }
```

- [ ] **Step 5: Enforce the rule in `read_metadata_bounded`**

Same change in `read_metadata_bounded`: add `let mut index = 0usize;` before the loop, call `check_streaminfo_position(index, block_type, len)?;` immediately after the `body_end > prefix.len()` `NeedMore` guard, and `index += 1;` before the `if is_last` break. (The `NeedMore` returns stay ahead of the check, so validation only runs once the block's body is present.)

- [ ] **Step 6: Enforce the rule in `synthesize_layout`**

Add at the very top of `synthesize_layout`, before `let mut ordered`:

```rust
    let streaminfo: Vec<&MetadataBlock> = structural
        .iter()
        .filter(|b| b.block_type == BLOCK_STREAMINFO)
        .collect();
    if streaminfo.len() != 1 || streaminfo[0].body.len() != STREAMINFO_BODY_LEN {
        return Err(FormatError::Malformed);
    }
```

- [ ] **Step 7: Repurpose the two characterization tests (preserve their mutant coverage)**

`parse_blocks_accepts_header_flush_with_end` currently uses an empty STREAMINFO. Give it a valid 34-byte body so it still exercises "final header flushes at end" but obeys the new rule:

```rust
#[test]
fn parse_blocks_accepts_header_flush_with_end() {
    // Single last STREAMINFO with a valid 34-byte body, no audio: the final header
    // still flushes at the buffer end (pos+4 == data.len() at the loop guard), so
    // this keeps the :43 `> -> >=` mutant coverage while obeying STREAMINFO rules.
    let file = flac_with(&[raw_block(BLOCK_STREAMINFO, &[0u8; 34], true, None)]);
    let meta = parse_blocks(&file).unwrap();
    assert_eq!(meta.audio_offset, 42); // 4 marker + 4 header + 34 body
}
```

`bounded_is_last_flag_continues_past_nonlast_block` currently uses two STREAMINFO blocks (now a duplicate). Make block 1 a valid STREAMINFO and block 2 a SEEKTABLE so the "walk past a non-last block" coverage is preserved:

```rust
#[test]
fn bounded_is_last_flag_continues_past_nonlast_block() {
    // First NON-last STREAMINFO (valid 34-byte body), then a LAST SEEKTABLE.
    // audio_offset must span BOTH, proving we walked past the non-last block.
    let b1 = raw_block(BLOCK_STREAMINFO, &[0u8; 34], false, None); // 4+34 = 38
    let b2 = raw_block(BLOCK_SEEKTABLE, &[0xBB, 0xBB, 0xBB], true, None); // 4+3 = 7
    let file = flac_with(&[b1, b2]);
    let expected_offset = (4 + 38 + 7) as u64; // marker + block1 + block2
    match read_metadata_bounded(&file).unwrap() {
        Extent::Complete(meta) => {
            // kills flac `header & 0x80` `&` -> `|`: that mutant stops after block 1
            // (audio_offset == 4+38 == 42). Spanning both blocks (49) kills it.
            assert_eq!(meta.audio_offset, expected_offset);
            assert_eq!(meta.preserved.len(), 2);
        }
        other @ Extent::NeedMore { .. } => panic!("expected Complete, got {other:?}"),
    }
}
```

- [ ] **Step 8: Update the four synth-boundary tests to pass a valid STREAMINFO**

These pass `&[]` structural and expect `TooLarge`; the new check now returns `Malformed` first. Change the first argument from `&[]` to `&[valid_streaminfo()]` in each, leaving the rest identical:

- `synthesize_layout_picture_block_size_boundary_is_inclusive`: both `synthesize_layout(&[], …)` calls → `synthesize_layout(&[valid_streaminfo()], …)`.
- `synthesize_layout_vorbis_comment_block_size_boundary_is_inclusive`: both calls.
- `synthesize_layout_binary_tag_block_size_boundary_is_inclusive`: both calls.
- `synthesize_layout_checked_picture_len_rejects_overflow`: the single call.

Example for the binary-tag test:

```rust
    // len == 0x00FF_FFFF exactly must succeed.
    assert!(synthesize_layout(&[valid_streaminfo()], 0, 0, &[], &[mk(0x00FF_FFFF)], &[]).is_ok());
    // one byte over must still error, pinning the high side of the boundary.
    assert_eq!(
        synthesize_layout(&[valid_streaminfo()], 0, 0, &[], &[mk(0x0100_0000)], &[]),
        Err(FormatError::TooLarge)
    );
```

Also update the **integration** test `synthesize_errors_on_oversized_picture` in `musefs-format/tests/synthesize_art.rs` — it likewise passes `&[]` and expects `TooLarge`. The `valid_streaminfo()` unit-test helper is NOT visible from the `tests/` directory, so build the block inline with the idiom the other integration tests use (`MetadataBlock { block_type: 0, body: streaminfo_body() }`; `streaminfo_body()` is already imported from `common` and returns a valid 34-byte body). Add `MetadataBlock` to the file's import and pass the block:

```rust
// add to the existing import at the top of musefs-format/tests/synthesize_art.rs:
use musefs_format::flac::{locate_audio, synthesize_layout, MetadataBlock};

// in synthesize_errors_on_oversized_picture, replace the `&[]` first argument:
    let streaminfo = [MetadataBlock { block_type: 0, body: streaminfo_body() }];
    assert_eq!(
        synthesize_layout(&streaminfo, 0, 0, &[], &[], &[art]),
        Err(FormatError::TooLarge)
    );
```

- [ ] **Step 9: Run the full format suite to verify green**

Run: `cargo test -p musefs-format`
Expected: PASS. Note the split among integration tests:
- `roundtrip.rs` and `synthesize_tags.rs` build structural via `MetadataBlock { block_type: 0, body: streaminfo_body() }` (valid 34-byte STREAMINFO) and the `make_flac`/`locate_audio` round-trip, so they stay green untouched.
- `synthesize_art.rs::synthesize_errors_on_oversized_picture` is the one that passed `&[]`; it was fixed in Step 8.
- `proptest_flac.rs` feeds `synthesize_layout` only successful `locate_audio` output (a valid STREAMINFO-first fixture), so it stays green.

- [ ] **Step 10: Update FLAC docs**

In `docs/FLAC.md`, add a sentence to the metadata/synthesis section: the parser now rejects (at scan and synthesis) any FLAC whose metadata does not begin with exactly one 34-byte STREAMINFO block; a crafted store providing malformed structural rows fails synthesis with a controlled error rather than emitting decoder-rejected output.

- [ ] **Step 11: Commit**

```bash
cd /home/cfutro/git/musefs/.worktrees/parser-strictness
cargo fmt
git add musefs-format/src/flac.rs musefs-format/tests/synthesize_art.rs docs/FLAC.md
git commit -m "$(cat <<'EOF'
fix(flac): validate STREAMINFO structure at scan and synthesis (#295)

Require exactly one 34-byte STREAMINFO as the first metadata block in
parse_blocks and read_metadata_bounded; reject the same in
synthesize_layout so a crafted DB's structural rows cannot be emitted as
decoder-rejected FLAC. Repurpose the two laxness-characterizing tests to
keep their mutant coverage with valid STREAMINFO fixtures.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>
EOF
)"
```

Expected: pre-commit runs the full suite and passes; commit succeeds.

---

## Task 2: MP3 ID3v2 header validation (#296)

**Files:**
- Modify: `musefs-format/src/mp3.rs` (`locate_audio`, `locate_audio_bounded`, `id3v2_alloc_safe`, tests module)

**Background:** The locators mask the high bit of each synchsafe size byte and never check the ID3 major version, so a malformed header like `49 44 33 04 00 00 00 00 00 80` ("ID3", v4, flags 0, size bytes `00 00 00 80` with the high bit set) mask-decodes to `audio_offset = 10`. `id3v2_alloc_safe` already rejects that shape. The fix shares a header validator. **Decided:** the locator does NOT reject unsynchronization/extended-header flags (they don't shift the audio offset).

- [ ] **Step 1: Add the shared header validator**

Insert in `musefs-format/src/mp3.rs` after `synchsafe_decode`:

```rust
/// Validate a leading ID3v2 header and return the tag length (10-byte header +
/// declared body, *excluding* any footer), or `None` when there is no ID3v2
/// header at offset 0 or the buffer is shorter than a 10-byte header.
///
/// A present-but-malformed header — unsupported major version, or a synchsafe
/// size byte with its high bit set — is `FormatError::Malformed`. This is the
/// intersection of the checks `id3v2_alloc_safe` makes on the header, so the
/// audio locator and the allocation guard agree on which headers are well-formed
/// instead of the locator mask-decoding shapes the guard rejects. Footer and
/// frame-flag handling stay with the callers.
fn id3v2_header_len(data: &[u8]) -> Result<Option<usize>> {
    if data.len() < 10 || &data[0..3] != b"ID3" {
        return Ok(None);
    }
    if !matches!(data[3], 2..=4) {
        return Err(FormatError::Malformed);
    }
    if data[6] | data[7] | data[8] | data[9] >= 0x80 {
        return Err(FormatError::Malformed);
    }
    Ok(Some(10 + synchsafe_decode(&data[6..10]) as usize))
}
```

- [ ] **Step 2: Write failing tests**

Add to the `tests` module in `musefs-format/src/mp3.rs`:

```rust
#[test]
fn locate_audio_rejects_high_bit_size_byte() {
    // Malformed synchsafe size (last byte 0x80) that masks to body=0, with a valid
    // frame sync at offset 10. Must reject rather than serve audio from offset 10.
    let mut data = Vec::new();
    data.extend_from_slice(b"ID3");
    data.extend_from_slice(&[0x04, 0x00, 0x00]); // major, rev, flags
    data.extend_from_slice(&[0x00, 0x00, 0x00, 0x80]); // high bit set -> malformed
    data.extend_from_slice(&[0xFF, 0xFB, 0x90, 0x00]); // valid sync at offset 10
    assert_eq!(locate_audio(&data), Err(FormatError::Malformed));
}

#[test]
fn locate_audio_rejects_unsupported_major_version() {
    let mut data = Vec::new();
    data.extend_from_slice(b"ID3");
    data.extend_from_slice(&[0x05, 0x00, 0x00]); // major 5 (unsupported)
    data.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]);
    data.extend_from_slice(&[0xFF, 0xFB, 0x90, 0x00]);
    assert_eq!(locate_audio(&data), Err(FormatError::Malformed));
}

#[test]
fn locate_audio_bounded_rejects_high_bit_size_byte() {
    let mut data = Vec::new();
    data.extend_from_slice(b"ID3");
    data.extend_from_slice(&[0x04, 0x00, 0x00]);
    data.extend_from_slice(&[0x00, 0x00, 0x00, 0x80]);
    data.extend_from_slice(&[0xFF, 0xFB, 0x90, 0x00]);
    let file_len = data.len() as u64;
    assert_eq!(
        locate_audio_bounded(&data, file_len, None),
        Err(FormatError::Malformed)
    );
}
```

- [ ] **Step 3: Run the new tests to verify they fail**

Run: `cargo test -p musefs-format mp3::tests::locate_audio_rejects_high_bit_size_byte`
Expected: FAIL (current locator masks the high bit and returns `Ok`).

- [ ] **Step 4: Rewrite the ID3 skip in `locate_audio`**

Replace the leading-ID3 block in `locate_audio`:

```rust
    let mut audio_offset = 0usize;
    if let Some(base) = id3v2_header_len(data)? {
        let flags = data[5];
        let mut tag_len = base;
        if flags & 0x10 != 0 {
            tag_len += 10; // ID3v2.4 footer
        }
        if tag_len > len {
            return Err(FormatError::Malformed);
        }
        audio_offset = tag_len;
    }
```

(`id3v2_header_len` returns `None` for buffers shorter than 10 bytes or without the `ID3` magic, preserving the "no ID3 → audio at 0" path; `Some(base)` guarantees `data[5]` is in bounds.)

- [ ] **Step 5: Rewrite the ID3 skip in `locate_audio_bounded`**

Replace the leading-ID3 block. Keep the existing short-prefix `NeedMore` guard so a prefix too short to hold the 10-byte header is widened before validation:

```rust
    let mut audio_offset = 0usize;
    if prefix.len() < 10 && file_len >= 10 {
        // Not enough bytes even to read the ID3v2 header.
        return Ok(Extent::NeedMore { up_to: 10 });
    }
    if let Some(base) = id3v2_header_len(prefix)? {
        let flags = prefix[5];
        let mut tag_len = base;
        if flags & 0x10 != 0 {
            tag_len += 10; // ID3v2.4 footer
        }
        if tag_len as u64 > file_len {
            return Err(FormatError::Malformed);
        }
        audio_offset = tag_len;
    }
```

(The rest of `locate_audio_bounded` — the `audio_offset + 2 > file_len` fast-fail, the frame-sync `NeedMore`, and the trailer handling — is unchanged.)

- [ ] **Step 6: Route `id3v2_alloc_safe`'s header decode through the helper**

In `id3v2_alloc_safe`, replace the opening magic/version/high-bit/body-decode block with a call to the shared helper, keeping the flag check and everything after it unchanged:

```rust
    let Ok(Some(tag_end)) = id3v2_header_len(data) else {
        // Not an ID3v2 tag at offset 0, or a malformed header: skip parsing.
        return false;
    };
    let flags = data[5];
    // Extended header (0x40) and unsynchronisation (0x80) complicate frame
    // bounds; skip rather than risk mis-validating.
    if flags & 0xC0 != 0 {
        return false;
    }
    if tag_end > data.len() {
        return false;
    }
    let body = tag_end - 10;
```

(`tag_end == 10 + body`. The `tag_end > data.len()` guard is the one previously applied right after the body decode — keep it. The subsequent frame walk uses `body`, `tag_end`, `major = data[3]`, and `header_len` exactly as before; keep those lines.)

- [ ] **Step 7: Run the full format suite**

Run: `cargo test -p musefs-format`
Expected: PASS. The existing locate tests (`locate_audio_skips_id3v2_then_finds_sync`, `locate_audio_honors_footer_flag`, `locate_audio_no_id3_starts_at_zero`, `locate_audio_requires_frame_sync`, the bounded tests) use well-formed headers and stay green.

- [ ] **Step 8: Run the metrics-feature tests (CI gap guard)**

Run: `cargo test -p musefs-core --features metrics`
Expected: PASS. (These changes don't touch getattr/read stat counts, but this is the documented local CI gap, so confirm.)

- [ ] **Step 9: Update MP3 docs**

In `docs/MP3.md`, clarify the audio-boundary contract: the locator now validates the ID3v2 major version (2–4) and rejects high-bit synchsafe size bytes, so a malformed ID3-looking prefix is rejected with a controlled error; unsynchronization/extended-header tags still scan (their declared size already covers the audio offset).

- [ ] **Step 10: Commit**

```bash
cd /home/cfutro/git/musefs/.worktrees/parser-strictness
cargo fmt
git add musefs-format/src/mp3.rs docs/MP3.md
git commit -m "$(cat <<'EOF'
fix(mp3): validate ID3v2 header in the audio locator (#296)

Share an id3v2_header_len validator across locate_audio,
locate_audio_bounded, and id3v2_alloc_safe so the audio locator rejects
unsupported major versions and high-bit synchsafe size bytes instead of
mask-decoding a malformed ID3-looking prefix into an audio offset.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 3: WAV RIFF form-size enforcement (#299)

**Files:**
- Modify: `musefs-format/src/wav.rs` (`riff_wave_start`, `walk_chunks`, `locate_audio`, `locate_audio_at_ceiling`, tests module)

**Background:** `riff_wave_start` returns a bare `12` and ignores the RIFF size field; `locate_audio`/`locate_audio_at_ceiling` accept any in-bounds `data` chunk regardless of the declared form. We parse the RIFF size, compute `form_end = 8 + riff_size`, and reject geometry that lies outside it. `wav_file` (the main test helper) already writes a correct size; only `wav_front` writes `0`, and the `riff_wave_start` unit test asserts the old return type.

- [ ] **Step 1: Write failing tests**

Add to the `tests` module in `musefs-format/src/wav.rs` (the `wav` helper builds a correctly-sized RIFF; `wav_front` builds a front-only buffer — to be fixed in Step 5):

```rust
#[test]
fn locate_audio_rejects_form_end_before_data() {
    // Correctly framed chunks, but the RIFF size declares a form that ends before
    // the data payload. Build with `wav` (valid size) then overwrite bytes 4..8.
    let mut buf = wav(&[(b"fmt ", fmt_pcm()), (b"data", vec![0x11; 8])]);
    buf[4..8].copy_from_slice(&8u32.to_le_bytes()); // form_end = 16, before data
    assert_eq!(locate_audio(&buf), Err(FormatError::Malformed));
}

#[test]
fn locate_audio_rejects_form_end_past_file() {
    let mut buf = wav(&[(b"fmt ", fmt_pcm()), (b"data", vec![0x11; 8])]);
    let huge = u32::try_from(buf.len()).unwrap() + 100;
    buf[4..8].copy_from_slice(&huge.to_le_bytes()); // form_end > physical file
    assert_eq!(locate_audio(&buf), Err(FormatError::Malformed));
}

#[test]
fn locate_audio_accepts_valid_form_with_odd_chunk_and_trailing_metadata() {
    // Odd-size chunk before data (word-padded) + a LIST chunk trailing data, all
    // inside a correctly-sized RIFF form. Must still parse.
    let buf = wav(&[
        (b"fmt ", fmt_pcm()),
        (b"data", vec![0x22; 7]), // odd payload -> 1 pad byte
        (b"LIST", vec![0x33; 4]),
    ]);
    let b = locate_audio(&buf).unwrap();
    assert_eq!(b.audio_length, 7);
}
```

- [ ] **Step 2: Run the new tests to verify they fail**

Run: `cargo test -p musefs-format wav::tests::locate_audio_rejects_form_end_before_data`
Expected: FAIL (current `locate_audio` ignores the RIFF size).

- [ ] **Step 3: Change `riff_wave_start` to return `(start, form_end)`**

```rust
/// Validate the RIFF/WAVE container header and return `(first_chunk_offset,
/// form_end)`, where `form_end = 8 + riff_size` is the byte just past the
/// declared RIFF form. Rejects RF64/BW64 and any non-`WAVE` RIFF file.
fn riff_wave_start(buf: &[u8]) -> Result<(usize, u64)> {
    if buf.len() < 12 || &buf[0..4] != b"RIFF" || &buf[8..12] != b"WAVE" {
        return Err(FormatError::NotWav);
    }
    let riff_size = u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]);
    Ok((12, 8 + u64::from(riff_size)))
}
```

- [ ] **Step 4: Update `walk_chunks` to destructure the new return**

Only the binding changes (the walk is still bounded by `buf.len()`; per-chunk form-end checks live in the locators):

```rust
    let Ok((mut pos, _form_end)) = riff_wave_start(buf) else {
        return out;
    };
```

(`read_structure` calls `riff_wave_start(front)?;` as a bare statement that discards the value, so it compiles unchanged — verify in Step 7.)

- [ ] **Step 5: Enforce form bounds in `locate_audio` and `locate_audio_at_ceiling`**

`locate_audio` (full buffer present):

```rust
pub fn locate_audio(buf: &[u8]) -> Result<WavBounds> {
    let (_, form_end) = riff_wave_start(buf)?;
    if form_end > buf.len() as u64 {
        return Err(FormatError::Malformed);
    }
    let chunks = walk_chunks(buf);
    let has_fmt = chunks.iter().any(|(id, _, _)| id == b"fmt ");
    let data = chunks.iter().find(|(id, _, _)| id == b"data");
    match (has_fmt, data) {
        (true, Some(&(_, off, len))) => {
            let data_end = (off as u64).saturating_add(len);
            if data_end > buf.len() as u64 || data_end > form_end {
                return Err(FormatError::Malformed);
            }
            Ok(WavBounds { audio_offset: off as u64, audio_length: len })
        }
        _ => Err(FormatError::NotWav),
    }
}
```

`locate_audio_at_ceiling` (front-only prefix; `file_len` is the true size):

```rust
pub fn locate_audio_at_ceiling(prefix: &[u8], file_len: u64) -> Result<WavBounds> {
    let (_, form_end) = riff_wave_start(prefix)?;
    if form_end > file_len {
        return Err(FormatError::Malformed);
    }
    let chunks = walk_chunks(prefix);
    let has_fmt = chunks.iter().any(|(id, _, _)| id == b"fmt ");
    let data = chunks.iter().find(|(id, _, _)| id == b"data");
    match (has_fmt, data) {
        (true, Some(&(_, off, len))) => {
            let data_end = (off as u64).saturating_add(len);
            if data_end > file_len || data_end > form_end {
                return Err(FormatError::Malformed);
            }
            Ok(WavBounds { audio_offset: off as u64, audio_length: len })
        }
        _ => Err(FormatError::NotWav),
    }
}
```

(The `has_fmt`/`data` presence check stays first, so a missing `fmt ` still returns `NotWav` before any geometry check — preserving `locate_audio_at_ceiling_requires_fmt_chunk`.)

- [ ] **Step 6: Fix the `wav_front` fixture and the `riff_wave_start` unit test**

`wav_front` writes a zero RIFF size; give it a valid one. The front is 44 bytes (RIFF+size+WAVE + 24-byte `fmt ` chunk + 8-byte `data` header), so the form covers `WAVE`(4) + `fmt ` chunk(24) + `data` header(8) + payload(`data_len`) = `36 + data_len`:

```rust
    let mut v = b"RIFF".to_vec();
    let riff_size = 36u32 + u32::try_from(data_len).unwrap(); // form: WAVE + fmt + data hdr + payload
    v.extend_from_slice(&riff_size.to_le_bytes());
    v.extend_from_slice(b"WAVE");
    v.extend_from_slice(&body);
    v
```

`riff_wave_start_accepts_exactly_twelve_bytes` asserts the old `Ok(12)`; update to the new tuple (riff_size = 0 → form_end = 8):

```rust
    assert_eq!(riff_wave_start(&buf), Ok((12, 8)));
```

- [ ] **Step 7: Run the full format suite**

Run: `cargo test -p musefs-format`
Expected: PASS. Verify in particular: `locate_audio_at_ceiling_trusts_data_header_without_payload` and `_accepts_data_shorter_than_file` (now valid form size), `_rejects_data_running_past_file` (still `Malformed`, now also caught by `form_end > file_len`), `_requires_fmt_chunk` (still `NotWav`), `walk_chunks_advances_past_each_payload`, and `read_structure` callers compile.

- [ ] **Step 8: Update WAV docs**

In `docs/WAV.md`, document RIFF form-size enforcement (chunks must lie within `8 + riff_size`, which must not exceed the file) and the known limitation: streaming/concatenated WAVs that write `riff_size = 0` or `0xFFFFFFFF` are rejected (skipped from the virtual tree, never modified); allowing those sentinels is a deferred follow-up.

- [ ] **Step 9: Commit**

```bash
cd /home/cfutro/git/musefs/.worktrees/parser-strictness
cargo fmt
git add musefs-format/src/wav.rs docs/WAV.md
git commit -m "$(cat <<'EOF'
fix(wav): enforce the RIFF form size before accepting chunk bounds (#299)

riff_wave_start now parses the RIFF size and returns form_end = 8 +
riff_size; locate_audio and locate_audio_at_ceiling reject a form that
exceeds the file or ends before the data chunk. Streaming-sentinel sizes
(0 / 0xFFFFFFFF) are rejected as a documented known limitation.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 4: Ogg sequence-number wrap (#298)

**Files:**
- Modify: `musefs-core/src/ogg_index.rs` (`serve_ogg_window`, tests module)

**Background:** `serve_ogg_window` patches each backing page's sequence with `u32::try_from(i64::from(old_seq) + seq_delta)`, which errors when the result leaves the `u32` range — a read-path availability bug for files with high (or, with a negative delta, low) page sequence numbers. Ogg's `page_sequence_number` is a `u32` that wraps naturally; the reference helper `new_reference_region` already uses `wrapping_add`.

- [ ] **Step 1: Write the failing wrap-boundary test**

Add to the `tests` module in `musefs-core/src/ogg_index.rs`. `lace_packet_pub(serial, seq, bos, granule, payload)` builds a single Ogg page (used elsewhere in this module's tests); place the page at audio offset 16 as the other fixtures do.

```rust
#[test]
fn serve_ogg_window_wraps_seq_past_u32_max() {
    use musefs_format::ogg::{parse_page, patch_page_header};
    // A single audio page whose sequence number is u32::MAX. With seq_delta = +1
    // the patched sequence must wrap to 0, not fail the read.
    let payload = vec![0x5Au8; 300];
    let (page, _) = lace_packet_pub(0x1234, u32::MAX, false, 0, &payload);
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("wrap.ogg");
    let mut file = vec![0u8; 16];
    file.extend_from_slice(&page);
    std::fs::File::create(&path).unwrap().write_all(&file).unwrap();

    let ao = 16u64;
    let alen = page.len() as u64;
    let backing = std::fs::File::open(&path).unwrap();
    let mut out = Vec::new();
    serve_ogg_window(&backing, ao, alen, 1, 0, alen, &mut out, None).unwrap();

    // The served region must be the page with its sequence wrapped (u32::MAX + 1 == 0):
    // patched header followed by the original payload bytes.
    let h = parse_page(&page, 0).unwrap();
    let mut want = patch_page_header(&page[..h.total_len()], 0).unwrap();
    want.extend_from_slice(&page[h.header_len..h.total_len()]);
    assert_eq!(out, want);
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p musefs-core serve_ogg_window_wraps_seq_past_u32_max`
Expected: FAIL — the current `u32::try_from(...)` returns `Err(Malformed)` for `u32::MAX + 1`, so `serve_ogg_window` returns an error and `.unwrap()` panics.

- [ ] **Step 3: Switch to wrapping arithmetic**

In `serve_ogg_window`, replace the checked conversion:

```rust
            let old_seq = u32::from_le_bytes(hdr_buf[18..22].try_into().unwrap());
            let new_seq = old_seq.wrapping_add(seq_delta as u32);
            patch_page_header_algebraic(&hdr_buf[..header_len], new_seq)?
```

(`seq_delta as u32` is `seq_delta mod 2³²`, so `old_seq.wrapping_add(seq_delta as u32)` equals `(old_seq + seq_delta) mod 2³²` for both positive and negative deltas. CRC patching already runs over `new_seq`, so the emitted bytes stay consistent.)

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test -p musefs-core serve_ogg_window_wraps_seq_past_u32_max`
Expected: PASS.

- [ ] **Step 5: Run the Ogg core suite**

Run: `cargo test -p musefs-core ogg`
Expected: PASS, including `serve_ogg_window_whole_region_matches_reference` (ordinary `seq_delta = 2`, still matches `new_reference_region`'s `wrapping_add(2)`).

- [ ] **Step 6: Update Ogg docs**

In `docs/OGG.md`, add to the page-renumbering section: synthesized page sequence numbers wrap modulo 2³² (matching Ogg's `u32` sequence field), so files whose audio pages have very high sequence numbers serve correctly rather than failing the read.

- [ ] **Step 7: Commit**

```bash
cd /home/cfutro/git/musefs/.worktrees/parser-strictness
cargo fmt
git add musefs-core/src/ogg_index.rs docs/OGG.md
git commit -m "$(cat <<'EOF'
fix(ogg): wrap page sequence renumbering mod 2^32 (#298)

serve_ogg_window patched seq with a checked conversion that failed reads
when old_seq + seq_delta left the u32 range. Use wrapping_add so high
(or, with a negative delta, low) sequence numbers wrap like Ogg's native
u32 field instead of making the file unreadable.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 5: Final cross-cutting verification

**Files:** none (verification only)

- [ ] **Step 1: Full workspace test run**

Run: `cargo test`
Expected: PASS across all crates.

- [ ] **Step 2: Clippy and fmt gates**

Run: `cargo clippy --all-targets && cargo fmt --all --check`
Expected: no warnings; fmt clean (check exit status directly).

- [ ] **Step 3: Fuzz crate build check**

The fuzz crate is outside the workspace and uses format-layer APIs. No public signatures changed (`riff_wave_start` is private; `locate_audio`/`synthesize_layout` signatures are unchanged), but confirm:
Run: `cargo +nightly fuzz build flac ; cargo +nightly fuzz build mp3 ; cargo +nightly fuzz build wav ; cargo +nightly fuzz build ogg`
Expected: builds succeed. (If any target now exercises the stricter parser and a corpus seed is a malformed file the parser used to accept, that seed simply returns `Err` — not a build break.) The FLAC fuzz target chains `locate_audio` → `synthesize_layout(&scan.preserved, …)`; because strict `locate_audio` now succeeds only for valid STREAMINFO-first input, `scan.preserved` always satisfies the new synthesis check, so the chain gains no new panic/assert path.

- [ ] **Step 4: Confirm the four commits are present and the tree is clean**

Run: `git log --oneline -5 && git status`
Expected: the four `fix(...)` commits (#295, #296, #299, #298) on top of the design-doc commits; clean working tree.

---

## Self-Review notes

- **Spec coverage:** #295 → Task 1 (scan via `parse_blocks`/`read_metadata_bounded`, synth via `synthesize_layout`); #296 → Task 2 (shared `id3v2_header_len` across both locators + `id3v2_alloc_safe`); #298 → Task 4 (`wrapping_add`); #299 → Task 3 (`form_end` enforcement in both locators). Docs for all four formats covered in their tasks.
- **MP3 decision honored:** the locator validates only version + synchsafe size bytes; unsync/extended-header flag rejection stays in `id3v2_alloc_safe`, not pushed into the locator.
- **Ordering hazard handled:** the FLAC synth STREAMINFO check runs before the `TooLarge` guards (Task 1 Step 6), and Task 1 Step 8 updates exactly the four `&[]`-structural boundary tests it affects.
- **Type consistency:** `riff_wave_start` returns `(usize, u64)` everywhere (Task 3 Steps 3–5); `id3v2_header_len` returns `Result<Option<usize>>` and all three callers handle `Ok(None)`/`Err` (Task 2 Steps 4–6).
