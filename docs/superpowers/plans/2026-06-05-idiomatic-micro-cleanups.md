# Idiomatic Micro-Cleanups (#138) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Fix the six non-idiomatic patterns from issue #138 (manual 24-bit shifts, duplicated `sha256_hex`, `to_string_lossy().to_string()`, hand-interleaved WAV chunks, magic numbers, `mem::zeroed` statvfs) with zero behavior change.

**Architecture:** Pure refactoring — every touched path is pinned by existing byte-exact tests (format proptests, interop fixtures, mutant-kill tests). Each cleanup category is one self-contained commit on one branch; the in-diff mutation gate runs once at the end.

**Tech Stack:** Rust workspace (musefs-format, musefs-db, musefs-core, musefs-latencyfs), cargo-mutants in-diff gate.

**Spec:** `docs/superpowers/specs/2026-06-05-idiomatic-micro-cleanups-design.md`

**Conventions that matter here:**
- Run all commands from the repo root `/home/cfutro/git/musefs`.
- Commit messages use a HEREDOC and end with the `Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>` trailer.
- Stage files by name, never `git add -A`.
- `cargo fmt --all --check` must pass before push (CI fmt gate); check the exit status directly.

---

### Task 0: Branch setup

- [ ] **Step 0.1: Create the feature branch**

```bash
git checkout -b idiomatic-cleanups-138
```

Expected: `Switched to a new branch 'idiomatic-cleanups-138'`.

---

### Task 1: 24-bit big-endian assembly → `from_be_bytes` (flac.rs, mp3.rs)

**Files:**
- Modify: `musefs-format/src/flac.rs` (4 read sites, `push_block_header`, `raw_block` test helper, 5 test comment blocks, 1 test rename)
- Modify: `musefs-format/src/mp3.rs` (1 read site in `id3v2_alloc_safe`, 2 test comment lines)
- Modify: `.cargo/mutants.toml` (delete the now-moot `read_metadata_bounded` exclude)

- [ ] **Step 1.1: Confirm the pinning tests are green before changing anything**

```bash
cargo test -p musefs-format flac && cargo test -p musefs-format mp3
```

Expected: PASS (all tests).

- [ ] **Step 1.2: Add the `u24_be` helper in flac.rs**

Insert immediately before `pub(crate) fn read_u32_be` (currently `flac.rs:340`):

```rust
/// Assemble a 24-bit big-endian block length from its three raw bytes.
fn u24_be(b0: u8, b1: u8, b2: u8) -> usize {
    u32::from_be_bytes([0, b0, b1, b2]) as usize
}

```

- [ ] **Step 1.3: Replace the three identical `data`-based read sites**

In `parse_blocks` (~line 50), `read_vorbis_comments` (~321), and `read_pictures` (~411) — three identical occurrences, replace all:

```rust
// OLD (3 occurrences):
        let len = ((data[pos + 1] as usize) << 16)
            | ((data[pos + 2] as usize) << 8)
            | (data[pos + 3] as usize);
// NEW:
        let len = u24_be(data[pos + 1], data[pos + 2], data[pos + 3]);
```

- [ ] **Step 1.4: Replace the `prefix`-based read site in `read_metadata_bounded`**

At ~line 105:

```rust
// OLD:
        let len = ((prefix[pos + 1] as usize) << 16)
            | ((prefix[pos + 2] as usize) << 8)
            | (prefix[pos + 3] as usize);
// NEW:
        let len = u24_be(prefix[pos + 1], prefix[pos + 2], prefix[pos + 3]);
```

- [ ] **Step 1.5: Replace the write-side shifts in `push_block_header` (~152)**

```rust
// OLD:
    out.push(((body_len >> 16) & 0xFF) as u8);
    out.push(((body_len >> 8) & 0xFF) as u8);
    out.push((body_len & 0xFF) as u8);
// NEW:
    out.extend_from_slice(&(body_len as u32).to_be_bytes()[1..]);
```

(Truncation behavior is unchanged for values ≤ 24 bits, which the callers guarantee.)

- [ ] **Step 1.6: Replace the write-side shifts in the `raw_block` test helper (~498)**

```rust
// OLD:
        v.push((n >> 16) as u8);
        v.push((n >> 8) as u8);
        v.push(n as u8);
// NEW:
        v.extend_from_slice(&(n as u32).to_be_bytes()[1..]);
```

- [ ] **Step 1.7: Replace the v2.2 frame-size site in mp3.rs `id3v2_alloc_safe` (~479)**

```rust
// OLD:
        let size = if major == 2 {
            ((data[pos + 3] as usize) << 16)
                | ((data[pos + 4] as usize) << 8)
                | (data[pos + 5] as usize)
        } else if major == 3 {
// NEW:
        let size = if major == 2 {
            u32::from_be_bytes([0, data[pos + 3], data[pos + 4], data[pos + 5]]) as usize
        } else if major == 3 {
```

- [ ] **Step 1.8: Reword the five stale flac test comments (the shift/`|` mutants they name no longer exist)**

(a) `parse_blocks_decodes_24bit_length_high_byte` (~545):

```rust
// OLD:
        // STREAMINFO header claims length 0x010000 (high byte set) over an empty body.
        // Original: len = 65536 -> body_end > data.len() -> Malformed.
        // :49 `<<16 -> >>16`: (0x01 >> 16) = 0 -> len = 0 -> body fits -> Ok.
        // (:50/:51 `| -> ^` are equivalent here: the shifted bytes are disjoint.)
// NEW:
        // STREAMINFO header claims length 0x010000 (high byte set) over an empty body.
        // Pins the high byte of the 24-bit length decode: len = 65536 -> body_end >
        // data.len() -> Malformed; a decode that drops the high byte gets len 0 -> Ok.
```

(b) `read_vorbis_comments_decodes_24bit_length` (~615) — the test has three stale comment chunks; replace the whole body's comments, keeping both assertions:

```rust
// OLD:
        // :199 `<<16 -> >>16` AND :200 `| -> &`: high length byte set over a short
        // body. Original len = 0x10000 -> Malformed. `>>16` -> len 0 -> Ok; `&` ->
        // (0x10000 & 0) -> len 0 -> Ok. Either mutant returns Ok instead of Malformed.
        let hi = flac_with(&[raw_block(BLOCK_STREAMINFO, &[], true, Some(0x01_0000))]);
        assert_eq!(read_vorbis_comments(&hi), Err(FormatError::Malformed));
        // :200 `<<8 -> >>8`: mid length byte set, high byte 0. Original len = 0x100
        // -> Malformed; `>>8` -> len 0 -> Ok.
        let mid = flac_with(&[raw_block(BLOCK_STREAMINFO, &[], true, Some(0x00_0100))]);
        assert_eq!(read_vorbis_comments(&mid), Err(FormatError::Malformed));
        // (:200 `| -> ^` and :201 `| -> ^` are equivalent: disjoint shifted bytes.)
// NEW:
        // High length byte set over a short body: len = 0x10000 -> Malformed. Pins
        // the high byte of the 24-bit length decode (dropping it gets len 0 -> Ok).
        let hi = flac_with(&[raw_block(BLOCK_STREAMINFO, &[], true, Some(0x01_0000))]);
        assert_eq!(read_vorbis_comments(&hi), Err(FormatError::Malformed));
        // Mid length byte set, high byte 0: len = 0x100 -> Malformed. Pins the mid
        // byte (dropping it gets len 0 -> Ok).
        let mid = flac_with(&[raw_block(BLOCK_STREAMINFO, &[], true, Some(0x00_0100))]);
        assert_eq!(read_vorbis_comments(&mid), Err(FormatError::Malformed));
```

(The final parenthetical line about the equivalent `| -> ^` mutants is deleted outright — there is no `|` left to mutate.)

(c) `read_pictures` test (~705 and ~708), two single-line rewording edits:

```rust
// OLD:
        // :289 `<<16 -> >>16` and :290 `| -> &`: high length byte over short body.
// NEW:
        // High length byte over short body: pins the 24-bit decode's high byte.
```

```rust
// OLD:
        // :290 `<<8 -> >>8`: mid length byte, high byte 0.
// NEW:
        // Mid length byte (high byte 0): pins the 24-bit decode's mid byte.
```

(Leave the `:283 `+ -> -`` / `> -> ==` / `> -> >=` comment lines alone — those bounds-check mutants still exist.)

(d) `bounded_decodes_24bit_length_exactly` (~805):

```rust
// OLD:
                // kills flac L105 `<< 16` -> `>> 16`: (0x01 >> 16) == 0 -> len loses
                //   its high byte -> body_end shifts -> wrong audio_offset.
                // kills flac L106 `<< 8` -> `>> 8`: (0x02 >> 8) == 0 -> mid byte lost.
                // kills flac L106 `|` -> `&`: (0x010000) & (0x000200) == 0 -> length
                //   collapses (disjoint high/mid bits) -> wrong audio_offset.
                // kills flac L107 final `|` -> assembles low byte; an exact audio_offset
                //   pins the full 24-bit assembly.
// NEW:
                // The exact audio_offset pins all three bytes of the 24-bit length
                // decode: losing the high, mid, or low byte shifts body_end and
                // yields a wrong audio_offset.
```

(e) `bounded_length_or_vs_and_high_byte` (~818) — rename (its name describes the retired `|`→`&` mutant) and reword:

```rust
// OLD:
    fn bounded_length_or_vs_and_high_byte() {
        // Dedicated `|` -> `&` kill on flac L106: declare length 0x010100 (high byte
        // 0x01, mid byte 0x01, low 0x00). Correct len = 65792. With `(b1<<16) &
        // (b2<<8)` the disjoint bits AND to 0, then `| low` -> very different length.
        // Use NeedMore: body is absent, so the correct parse asks for the full body.
// NEW:
    fn bounded_length_decodes_high_and_mid_bytes() {
        // Declare length 0x010100 (high byte 0x01, mid byte 0x01, low 0x00); correct
        // len = 65792. A decode that collapses the high or mid byte asks for a very
        // different body.
        // Use NeedMore: body is absent, so the correct parse asks for the full body.
```

- [ ] **Step 1.9: Reword the two stale mp3 test comment lines (~1208-1214)**

```rust
// OLD:
        // Declare a size that the *correct* decode puts out of bounds (reject), so a
        // wrong shift/OR that shrinks the size would wrongly accept.
// NEW:
        // Declare a size that the *correct* decode puts out of bounds (reject), so a
        // decode that drops a size byte would wrongly accept.
```

```rust
// OLD:
        assert!(!id3v2_alloc_safe(&id3v2(0x02, 0x00, 6, &f_mid))); // kills <<8 and |->&
                                                                   // size bytes [0x01,0x00,0x00] = 65536 -> reject; `<<16 -> >>16` shrinks to 0.
// NEW:
        assert!(!id3v2_alloc_safe(&id3v2(0x02, 0x00, 6, &f_mid))); // pins the mid byte
                                                                   // size bytes [0x01,0x00,0x00] = 65536 -> reject; pins the high byte.
```

- [ ] **Step 1.10: Delete the now-moot exclude from `.cargo/mutants.toml`**

Remove this entire entry (comment + regex line) from `exclude_re`:

```toml
    # flac::read_metadata_bounded 24-bit block-length assembly: the three length
    # bytes occupy disjoint bit ranges after shifting, so `|`, `^`, and `+` are
    # bit-for-bit identical there (the `|`->`&` variant collapses to 0 and IS
    # caught). Same equivalence the sibling parse_blocks/read_vorbis_comments tests
    # already note.
    'musefs-format/src/flac\.rs:\d+:\d+: replace \| with \^ in read_metadata_bounded',
```

This exclude covered `read_metadata_bounded` only; the other three read sites never had one. Removing the `|` operators everywhere only *removes* mutants — it cannot introduce a gate failure by itself.

- [ ] **Step 1.11: Verify no manual 24-bit assembly remains and tests still pass**

```bash
grep -n '<< 16' musefs-format/src/flac.rs musefs-format/src/mp3.rs
cargo test -p musefs-format flac && cargo test -p musefs-format mp3
```

Expected: the only `<< 16` hits (if any) are unrelated to length assembly (there should be none in flac.rs; mp3.rs keeps none). All tests PASS.

- [ ] **Step 1.12: Commit**

```bash
git add musefs-format/src/flac.rs musefs-format/src/mp3.rs .cargo/mutants.toml
git commit -m "$(cat <<'EOF'
Decode 24-bit lengths with from_be_bytes instead of manual shifts (#138)

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 2: Deduplicate `sha256_hex`, use `LowerHex` (musefs-db)

**Files:**
- Modify: `musefs-db/src/art.rs:6-14` (single shared impl)
- Modify: `musefs-db/src/bulk.rs:6-14` (delete dup, import)

- [ ] **Step 2.1: Confirm the known-digest test is green**

```bash
cargo test -p musefs-db sha256_hex_matches_known_digest
```

Expected: PASS (1 test).

- [ ] **Step 2.2: Replace the art.rs impl and make it `pub(crate)`**

```rust
// OLD (art.rs:6-14):
fn sha256_hex(data: &[u8]) -> String {
    let digest = Sha256::digest(data);
    let mut s = String::with_capacity(64);
    for b in digest {
        use std::fmt::Write;
        let _ = write!(s, "{b:02x}");
    }
    s
}
// NEW:
pub(crate) fn sha256_hex(data: &[u8]) -> String {
    format!("{:x}", Sha256::digest(data))
}
```

- [ ] **Step 2.3: Delete the bulk.rs duplicate and import the shared fn**

Delete the identical `fn sha256_hex` block at `bulk.rs:6-14`. Then fix the imports at the top of bulk.rs:

```rust
// OLD:
use crate::models::{BinaryTag, NewArt, NewTrack, StructuralBlock, Tag, TrackArt};
use crate::{Db, ReadWrite, Result};
use rusqlite::{params, Transaction};
use sha2::{Digest, Sha256};
// NEW:
use crate::art::sha256_hex;
use crate::models::{BinaryTag, NewArt, NewTrack, StructuralBlock, Tag, TrackArt};
use crate::{Db, ReadWrite, Result};
use rusqlite::{params, Transaction};
```

(`sha2` is no longer referenced in bulk.rs; the `sha256_hex_matches_known_digest` test calls `super::sha256_hex`, which now resolves through the `use` — it stays where it is and keeps pinning the output.)

- [ ] **Step 2.4: Run the crate tests**

```bash
cargo test -p musefs-db
```

Expected: PASS (including `sha256_hex_matches_known_digest` — `LowerHex` output is identical to the old per-byte loop).

- [ ] **Step 2.5: Commit**

```bash
git add musefs-db/src/art.rs musefs-db/src/bulk.rs
git commit -m "$(cat <<'EOF'
Deduplicate sha256_hex and format via LowerHex (#138)

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 3: `to_string_lossy().to_string()` → `.into_owned()` (28 sites)

**Files:**
- Modify: `musefs-core/src/scan.rs`, `musefs-core/src/facade.rs`, `musefs-core/src/reader.rs`
- Modify: `musefs-core/tests/proptest_read_fidelity.rs`, `tests/interop_emit.rs`, `tests/external_contract.rs`, `tests/read_at.rs`, `tests/reader.rs`

- [ ] **Step 3.1: Apply the mechanical swap**

```bash
grep -rl 'to_string_lossy()\.to_string()' --include='*.rs' musefs-core \
  | xargs sed -i 's/to_string_lossy()\.to_string()/to_string_lossy().into_owned()/g'
```

- [ ] **Step 3.2: Verify zero sites remain anywhere in the repo**

```bash
grep -rn 'to_string_lossy()\.to_string()' --include='*.rs' . | grep -v target
```

Expected: no output (exit code 1).

- [ ] **Step 3.3: Build and run the affected crate's tests**

```bash
cargo test -p musefs-core
```

Expected: PASS. (`.into_owned()` on `Cow<str>` produces the identical `String`; only the borrow case skips a realloc.)

- [ ] **Step 3.4: Commit**

```bash
git add musefs-core/src/scan.rs musefs-core/src/facade.rs musefs-core/src/reader.rs \
  musefs-core/tests/proptest_read_fidelity.rs musefs-core/tests/interop_emit.rs \
  musefs-core/tests/external_contract.rs musefs-core/tests/read_at.rs musefs-core/tests/reader.rs
git commit -m "$(cat <<'EOF'
Replace to_string_lossy().to_string() with .into_owned() (#138)

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 4: WAV chunk-assembly helpers (wav.rs)

**Files:**
- Modify: `musefs-format/src/wav.rs` (`push_inline_chunk` ~173, `build_info_payload` ~140, `synthesize_layout` ~185)

- [ ] **Step 4.1: Confirm the WAV byte-exactness tests are green**

```bash
cargo test -p musefs-format wav && cargo test -p musefs-format --features fuzzing proptest_wav
```

Expected: PASS.

- [ ] **Step 4.2: Add the two helpers, replacing the body of `push_inline_chunk`**

Replace the current `push_inline_chunk` (wav.rs ~173-183):

```rust
// OLD:
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
// NEW:
/// 8-byte RIFF chunk header: fourcc + LE u32 size.
fn chunk_header(id: &[u8; 4], len: u32) -> [u8; 8] {
    let mut h = [0u8; 8];
    h[..4].copy_from_slice(id);
    h[4..].copy_from_slice(&len.to_le_bytes());
    h
}

/// Append a chunk (`fourcc + LE size + payload + word-align pad`) to `out`.
fn append_chunk(out: &mut Vec<u8>, id: &[u8; 4], payload: &[u8]) {
    out.extend_from_slice(&chunk_header(id, payload.len() as u32));
    out.extend_from_slice(payload);
    if payload.len() % 2 == 1 {
        out.push(0x00);
    }
}

/// Push a fully-inline chunk (`fourcc + LE size + payload + word-align pad`).
fn push_inline_chunk(segments: &mut Vec<Segment>, id: &[u8; 4], payload: &[u8]) {
    let mut chunk = Vec::with_capacity(8 + payload.len() + 1);
    append_chunk(&mut chunk, id, payload);
    segments.push(Segment::Inline(chunk));
}
```

- [ ] **Step 4.3: Use `append_chunk` in `build_info_payload`'s subchunk loop (~158)**

```rust
// OLD:
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
// NEW:
    for (cc, value) in entries {
        let mut v = value.as_bytes().to_vec();
        v.push(0x00); // INFO values are NUL-terminated
        append_chunk(&mut payload, cc, &v);
    }
```

- [ ] **Step 4.4: Use `chunk_header` for the `id3 ` and `data` heads in `synthesize_layout`**

```rust
// OLD:
    let mut id3_head = Vec::with_capacity(8);
    id3_head.extend_from_slice(b"id3 ");
    id3_head.extend_from_slice(&(tag_len as u32).to_le_bytes());
    segments.push(Segment::Inline(id3_head));
// NEW:
    segments.push(Segment::Inline(chunk_header(b"id3 ", tag_len as u32).to_vec()));
```

```rust
// OLD:
    let mut data_head = Vec::with_capacity(8);
    data_head.extend_from_slice(b"data");
    data_head.extend_from_slice(&(audio_length as u32).to_le_bytes());
    segments.push(Segment::Inline(data_head));
// NEW:
    segments.push(Segment::Inline(chunk_header(b"data", audio_length as u32).to_vec()));
```

(The RIFF file head stays hand-built: `RIFF` + size + `WAVE` is the 12-byte file header, not a padded chunk.)

- [ ] **Step 4.5: Verify byte output is unchanged**

```bash
cargo test -p musefs-format wav && cargo test -p musefs-format --features fuzzing proptest_wav
```

Expected: PASS — these tests pin the synthesized bytes exactly.

- [ ] **Step 4.6: Commit**

```bash
git add musefs-format/src/wav.rs
git commit -m "$(cat <<'EOF'
Extract WAV chunk assembly helpers (#138)

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 5: Name the magic numbers (mp3.rs, ogg_index.rs)

**Files:**
- Modify: `musefs-format/src/mp3.rs` (new const + 2 guard sites, ~138 and ~386)
- Modify: `musefs-core/src/ogg_index.rs` (7 test allocation sites)

- [ ] **Step 5.1: Add `SYNCHSAFE_MAX` in mp3.rs**

Insert immediately before `fn push_frame_header` (~135):

```rust
/// Inclusive maximum of a 28-bit ID3v2.4 syncsafe size field.
const SYNCHSAFE_MAX: usize = 0x0FFF_FFFF;

```

- [ ] **Step 5.2: Replace the two src guards**

In `push_frame_header` (~138):

```rust
// OLD:
    if data_len > 0x0FFF_FFFF {
// NEW:
    if data_len > SYNCHSAFE_MAX {
```

In `build_id3v2_segments`' tag-size guard (~386):

```rust
// OLD:
    if frames_len > 0x0FFF_FFFF {
// NEW:
    if frames_len > SYNCHSAFE_MAX {
```

Leave the test-module literals (`mp3.rs` tests at ~898, ~911, ~972-1046) as-is: those tests document the boundary value itself. The const stays mutation-killable — the boundary tests pin both guards (cargo-mutants mutates const initializers).

- [ ] **Step 5.3: Replace the 7 bare `282`s in ogg_index.rs tests**

The tests `use super::*`, so `MAX_OGG_HEADER_BYTES` (defined at `ogg_index.rs:27`) is already in scope:

```bash
sed -i 's/vec!\[0u8; 282\]/vec![0u8; MAX_OGG_HEADER_BYTES]/g' musefs-core/src/ogg_index.rs
grep -cn 'vec!\[0u8; MAX_OGG_HEADER_BYTES\]' musefs-core/src/ogg_index.rs
```

Expected: count of 7; `grep -n '\b282\b' musefs-core/src/ogg_index.rs` afterwards matches only the const definition (line 27) and the doc comment (line 21).

- [ ] **Step 5.4: Run the affected tests**

```bash
cargo test -p musefs-format mp3 && cargo test -p musefs-core ogg_index
```

Expected: PASS.

- [ ] **Step 5.5: Commit**

```bash
git add musefs-format/src/mp3.rs musefs-core/src/ogg_index.rs
git commit -m "$(cat <<'EOF'
Name the synchsafe-max and Ogg header-size magic numbers (#138)

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 6: `statvfs` via `MaybeUninit` (musefs-latencyfs)

**Files:**
- Modify: `musefs-latencyfs/src/lib.rs:404-406` (inside `statfs`)

- [ ] **Step 6.1: Replace `mem::zeroed` with `MaybeUninit`**

```rust
// OLD:
                let mut s: libc::statvfs = unsafe { std::mem::zeroed() };
                // SAFETY: cstr is a valid NUL-terminated path; s is a valid out-param.
                if unsafe { libc::statvfs(cstr.as_ptr(), &raw mut s) } == 0 {
// NEW:
                let mut s = std::mem::MaybeUninit::<libc::statvfs>::uninit();
                // SAFETY: cstr is a valid NUL-terminated path; s is a valid out-param.
                if unsafe { libc::statvfs(cstr.as_ptr(), s.as_mut_ptr()) } == 0 {
                    // SAFETY: statvfs returned 0, so it fully initialized `s`.
                    let s = unsafe { s.assume_init() };
```

The `reply.statfs(s.f_blocks as u64, ...)` body below is unchanged — the shadowed `s` keeps every field access compiling as-is.

- [ ] **Step 6.2: Build and test the crate**

```bash
cargo test -p musefs-latencyfs
```

Expected: compiles and passes (the mounted e2e tests are `#[ignore]`d and don't run here; `musefs-latencyfs` is also excluded from the in-diff mutation gate, so this change carries no gate cost).

- [ ] **Step 6.3: Commit**

```bash
git add musefs-latencyfs/src/lib.rs
git commit -m "$(cat <<'EOF'
Initialize statvfs out-param via MaybeUninit instead of mem::zeroed (#138)

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 7: Full validation gate

- [ ] **Step 7.1: Format and lint** (clippy must be `--all-targets` — benches/ and tests/ hold API consumers a plain build misses)

```bash
cargo fmt --all --check && echo FMT-OK
cargo clippy --all-targets && echo CLIPPY-OK
```

Expected: `FMT-OK` and `CLIPPY-OK` with no warnings. If fmt fails, run `cargo fmt --all`, re-check, and amend nothing — fold the formatting into a new commit only if any file changed.

- [ ] **Step 7.2: Full test suite, including the feature-gated format proptests**

```bash
cargo test --workspace
cargo test -p musefs-format --features fuzzing
```

Expected: all PASS.

- [ ] **Step 7.3: In-diff mutation gate (CI parity)**

Always `-j2`, output on `/tmp`, do NOT set `TMPDIR`. Sanity-check the diff is non-empty first — an empty diff mutates nothing and exits 0, a silent false pass:

```bash
git diff "$(git merge-base main HEAD)...HEAD" -- '*.rs' > mutants.diff
grep -q '^@@ ' mutants.diff && echo DIFF-OK
cargo mutants --in-diff mutants.diff -j2 --exclude 'musefs-latencyfs/**' --output /tmp/mutants-out/in-diff
```

Expected: `DIFF-OK`, then gate passes with no MISSED mutants. The rewritten arithmetic stays killable: `u24_be`'s bytes are pinned by the renamed/reworded flac length tests, the wav helpers by the byte-exact wav tests, `SYNCHSAFE_MAX` by the mp3 boundary tests, and `sha256_hex` by the known-digest test. If a MISSED mutant appears, stop and fix it (add a pinning test, or a documented `.cargo/mutants.toml` exclude only for a proven-equivalent mutant) before pushing — do not push with a red gate.

- [ ] **Step 7.4: Finish the branch**

Use the superpowers:finishing-a-development-branch skill to merge/PR. The PR closes #138; draft the title/body from the full diff against main (six commits, one per cleanup category).
