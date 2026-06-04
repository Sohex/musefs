# SP4 — Storage-aware serving residuals — design

*Date: 2026-06-01 · Part of the [2026-05-30 optimization pass](./README.md)*

## Goal

Eliminate the Ogg first-read whole-audio-region index scan. Currently, the first
read of any `OggAudio` segment calls `build_index`, which opens the backing file,
seeks to `audio_offset`, and reads the **entire** audio region sequentially to
build `Vec<IndexedPage>`. For a 10-minute Vorbis file at 320 kbps (~24 MB), this
is ~96 buffered reads before byte 0 is served — blocking the first read by hundreds
of milliseconds on HDD and potentially seconds on NFS-HDD.

Replace with a **stateless, per-request backwards-scan + algebraic CRC** strategy:
find the page boundary containing the request by reading a small (~65 KB) backwards
window, recompute the patched header using only the header bytes (no payload read),
serve the needed payload bytes via a single positioned `pread`. Each read is
O(request\_size + 65 KB backward-scan window). No persistent index state.

## Cardinal invariant (preserved by construction)

**Original audio bytes are never copied or modified, and served audio stays
byte-identical.** SP4 changes only *how the Ogg page renumbering is computed* (from
a pre-built in-memory index to an on-the-fly algebraic patch). The bytes served are
identical; the algebraic CRC update produces the same patched header as the
full-page computation. Existing proptests and the `#[ignore]`d FUSE e2e mount tests
are the hard gate.

## Key structural property

`patch_page_header` patches the sequence number (bytes 18–21) and CRC (bytes 22–25)
**in-place with no change in byte length**. This gives a 1:1 offset mapping between
the synthesized audio region and the backing file: virtual byte offset X within the
`OggAudio` segment corresponds to backing file byte `audio_offset + X`. No index is
needed to translate between coordinate spaces.

## Algebraic CRC update

### Why it works

The Ogg CRC (`crc32` in `ogg/crc.rs`) has initial value `0` and no final XOR,
making it **linear over GF(2)**:

```
crc32(A XOR B) = crc32(A) XOR crc32(B)    for equal-length messages
```

Proof of linearity: the loop `crc = (crc << 8) ^ TABLE[((crc >> 24) ^ b) as usize]`
is an affine map. With `init = 0` it is purely linear. `TABLE[0] = 0` (confirmed:
i=0 → all-zero shift-register for 8 bits → t[0]=0), so processing zero-valued bytes
from state 0 leaves state 0.

### Deriving the formula

When patching a page, the CRC is computed over the full page with bytes 22–25
zeroed. The old and new inputs differ only at bytes 18–21 (seq field); all other
bytes — including the entire payload — are identical and cancel in the XOR:

```
DELTA = old_input XOR new_input
      = [0×18,  (old_seq XOR new_seq),  0×(seg_count + 5 + payload_len)]
```

Therefore:

```
new_crc = old_crc XOR crc32(DELTA)
```

`crc32(DELTA)` is computed from header bytes alone:

1. Initialize `crc = 0`.
2. 18 leading zero bytes: `crc` stays 0 (`TABLE[0] = 0`).
3. 4 bytes `(old_seq XOR new_seq).to_le_bytes()`: `crc = f(delta_bytes)`.
4. Advance by `(5 + seg_count + payload_len)` zero-byte steps.

`payload_len = sum(seg_table)` comes from the segment table in the header.
**No payload I/O required.**

### Implementation

Add to `musefs-format/src/ogg/crc.rs`:

```rust
/// Advance the CRC register by `n` zero-byte steps (multiply by x^(8n) in
/// GF(2)[x] / poly). Used for algebraic CRC patching without payload reads.
pub fn crc_shift_zeros(mut crc: u32, n: usize) -> u32 {
    for _ in 0..n {
        crc = (crc << 8) ^ TABLE[(crc >> 24) as usize];
    }
    crc
}
```

The loop runs at most `5 + 255 + 65025 = 65285` iterations (max payload +
seg-table). At ~2 ns/iter that is ~130 µs worst-case CPU.

**Performance gate:** if `sequential_read` for Ogg/Opus/OggFLAC rises >10% vs the
SP3 baseline due to this loop, replace with O(log n) GF(2) polynomial exponentiation
(the standard `crc_combine`/`crc32_shift` technique: represent the linear map as a
32×32 GF(2) matrix and apply repeated squaring). The plan section handles this
contingency.

Add to `musefs-format/src/ogg/page.rs`:

```rust
/// Patch a page header using only the header bytes — no payload read needed.
/// `header` is the `27 + seg_count` bytes of the page header (fixed header +
/// segment table). Returns the patched header with `new_seq` and a correct CRC.
pub fn patch_page_header_algebraic(header: &[u8], new_seq: u32) -> Result<Vec<u8>> {
    if header.len() < 27 { return Err(FormatError::Malformed); }
    let seg_count = header[26] as usize;
    let header_len = 27 + seg_count;
    if header.len() < header_len { return Err(FormatError::Malformed); }
    let payload_len: usize = header[27..header_len].iter().map(|&b| b as usize).sum();
    let old_seq = u32::from_le_bytes(header[18..22].try_into().unwrap());
    let old_crc = u32::from_le_bytes(header[22..26].try_into().unwrap());
    let trailing = 5 + seg_count + payload_len;
    let mut delta_crc = crc32(                      // 18 leading zeros → crc stays 0
        &(old_seq ^ new_seq).to_le_bytes()          // 4 delta bytes
    );
    delta_crc = crc_shift_zeros(delta_crc, trailing); // trailing zeros
    let new_crc = old_crc ^ delta_crc;
    let mut out = header[..header_len].to_vec();
    out[18..22].copy_from_slice(&new_seq.to_le_bytes());
    out[22..26].copy_from_slice(&new_crc.to_le_bytes());
    Ok(out)
}
```

`patch_page_header` (full-page version) is **retained** as a test oracle — see
Testing section.

## Backwards-scan algorithm

### Finding the page start

```
fn find_page_start(backing: &File, audio_offset: u64, abs_target: u64) -> Result<u64>
```

- **Special case `abs_target == audio_offset`** (i.e., `rstart == 0`): return
  `audio_offset` immediately. The first audio page is known to start there
  (validated at scan time). No backward read.
- **General case:** read the window
  `[max(audio_offset, abs_target − 65307), abs_target)` in a single `pread`.
  Scan backwards for the rightmost `b"OggS"` with version byte 0, applying cheap
  header sanity checks:
  - `header_type & 0xF8 == 0` (only bits 0–2 are defined flags)
  - `num_segs` byte fits within the window at index `i + 26`
  - The segment table (`num_segs` bytes from `i + 27`) also fits within the window

  For each candidate that passes the cheap checks, **deterministically validate it
  by reading the full candidate page from the backing file and recomputing its CRC**
  (see "Entry-page CRC guard" below). Return `window_start + i` for the first
  (rightmost) candidate whose CRC matches; on a CRC mismatch, continue scanning
  backwards. If no candidate validates, return
  `Err(CoreError::Format(FormatError::Malformed))`.

If `abs_target` falls exactly on a page boundary, the backward scan finds the
**preceding** page's start (the current page's OggS is not in the half-open window).
The forward pass below reaches the correct page in one extra parse with no
additional I/O — the preceding page's header bytes are already in the backward-scan
window.

### Entry-page CRC guard (preserves the cardinal invariant unconditionally)

The old `build_index` scanned **forward from a known-good boundary** (`audio_offset`),
so it could never mis-locate a page. The backward scan starts from an arbitrary
offset and hunts for the `OggS` capture pattern, which introduces a new failure mode:
a compressed audio payload that coincidentally contains `b"OggS\x00"` plus a plausible
header could be mistaken for a page start, producing non-byte-identical output. The
`header_type`/`num_segs` checks make this astronomically unlikely (~10⁻⁸ per cold
seek), but "astronomically unlikely" is not "never," and the cardinal invariant is
non-negotiable.

So the scan does not trust the cheap checks alone: for each candidate it reads the
full candidate page from the backing file and recomputes its CRC, accepting the
candidate only if the recomputed CRC matches the page's stored CRC. A coincidental
`OggS` in payload fails this check (its "CRC field" is just payload bytes), so the
scan rejects it and continues backward to the real page boundary. This makes the
backward scan **deterministically correct** — the byte-identical guarantee holds
unconditionally, not merely with high probability.

Cost: one extra page read (≤ 65 KB, typically a few KB for audio) per serve that does
not start at `rstart == 0`. For sequential play that page sits in the kernel page
cache (it is at the current read position), so the added cost is CPU-only
(`crc32` over one page). For cold random seeks it is one additional `pread`. The
`rstart == 0` fast path skips the scan and the guard entirely (the first audio page
starts at `audio_offset` by construction). Worst case is O(false positives in the
window) page reads, but for standard encoder output that count is zero.

The CRC recomputation reuses `crc32` via a new `musefs_format::ogg::verify_page_crc`
helper (CRC logic stays in the format layer where `crc32` lives).

### Forward pass (serve loop)

```
pub fn serve_ogg_window(
    backing: &File, audio_offset: u64, audio_length: u64,
    seq_delta: i64, rstart: u64, rend: u64, out: &mut Vec<u8>,
) -> Result<()>
```

```
audio_end = audio_offset + audio_length
abs_rstart = audio_offset + rstart

P = find_page_start(backing, audio_offset, abs_rstart)

while (P < audio_end) AND ((P - audio_offset) < rend):
    // Read max possible header in one pread to avoid two round-trips.
    // Clamp to available bytes (handles the last page of a small audio region).
    read_len = min(282, audio_end - P) as usize
    pread read_len bytes at P  →  header_buf
    if header_buf.len() < 27: return Err(Malformed)
    seg_count  = header_buf[26]
    header_len = 27 + seg_count
    if header_buf.len() < header_len: return Err(Malformed)
    payload_len = sum(header_buf[27..header_len])

    old_seq = LE32(header_buf[18..22])
    new_seq = (old_seq as i64 + seq_delta) as u32
    patched_hdr = patch_page_header_algebraic(&header_buf[..header_len], new_seq)

    page_rel = P - audio_offset          // page start relative to audio region
    hdr_end  = page_rel + header_len
    page_end = hdr_end + payload_len

    // Header overlap
    hs = rstart.max(page_rel);  he = rend.min(hdr_end)
    if hs < he: out.extend_from_slice(&patched_hdr[(hs-page_rel)..(he-page_rel)])

    // Payload overlap — exact bytes only, no full-page read
    ps = rstart.max(hdr_end);  pe = rend.min(page_end)
    if ps < pe:
        n = pe - ps
        out.resize(out.len() + n, 0)
        pread n bytes at (P + header_len + (ps - hdr_end)) into out's tail

    P += header_len + payload_len
```

### I/O profile

| Path | I/O per FUSE read |
|---|---|
| Backward-scan window | 65 KB pread (kernel page cache hit for sequential play); skipped when `rstart == 0` |
| Entry-page CRC guard | one full candidate page read (≤ 65 KB; cache hit for sequential play); skipped when `rstart == 0` |
| Header per page in window | 282 bytes pread (cache hit for sequential play) |
| Payload slice | Exactly the bytes served — same as warmed-index approach |

For sequential play the backward-scan window and per-page headers are in the kernel
page cache (they are the bytes immediately preceding the current readahead window).
Effective extra I/O overhead for sequential play: zero additional NFS RPCs.

For cold random seeks: ~1 NFS RPC for the backward-scan window + 1 RPC per page
header in the serve window. Compared to the current approach (O(whole file) scan
before any bytes are served), this is a dramatic improvement.

## Code changes

### `musefs-format/src/ogg/crc.rs`

- **Add** `pub fn crc_shift_zeros(crc: u32, n: usize) -> u32`

### `musefs-format/src/ogg/page.rs`

- **Add** `pub fn patch_page_header_algebraic(header: &[u8], new_seq: u32) -> Result<Vec<u8>>`
- **Add** `pub fn verify_page_crc(page: &[u8]) -> Result<bool>` — recompute a full
  page's CRC and compare to its stored CRC (used by the entry-page guard).
- **Keep** `patch_page_header` (used as test oracle; not on the serve hot-path after
  this SP)
- Re-export both new functions from `musefs-format/src/ogg/mod.rs` (the `page` module
  is private).

### `musefs-core/src/ogg_index.rs` (net reduction)

Remove:
- `pub struct OggPageIndex { pub pages: Vec<IndexedPage> }`
- `pub struct IndexedPage { … }`
- `pub fn build_index(…) -> Result<OggPageIndex>`
- `pub fn serve(…) -> Result<()>`

Add:
- `pub fn serve_ogg_window(…) -> Result<()>` — the backwards-scan serve function
- `fn find_page_start(…) -> Result<u64>` — the backward scan helper (CRC-validates
  each candidate page via `page_crc_ok`)
- `fn page_crc_ok(backing, page_start) -> Result<bool>` — read the full candidate
  page and check its CRC via `verify_page_crc`; returns `Ok(false)` (not an error) on
  CRC mismatch or EOF so the scan continues to the next candidate.

### `musefs-core/src/reader.rs`

Remove:
- `ogg_index: OnceCell<Arc<OggPageIndex>>` from `ResolvedFile`
- Constants: `OGG_MIN_PAGE_BYTES`, `OGG_INDEX_BYTES_PER_PAGE`
- Function: `estimated_ogg_index_bytes`
- Imports (line 14): `use crate::ogg_index::{build_index, serve, OggPageIndex}` →
  replace with `use crate::ogg_index::serve_ogg_window`
- Import (line 9): `use once_cell::sync::OnceCell` (no longer needed)

Simplify:
- `cache_bytes` computation: remove the entire `+ match track.format { Opus |
  Vorbis | OggFlac => estimated_ogg_index_bytes(track.audio_length as u64), _ => 0
  }` block that follows the `.sum::<u64>()`. All formats now use the same formula:
  sum of `Inline` segment byte lengths.
- `OggAudio` arm in `read_segments`: replace the `get_or_try_init` /
  `build_index` / `serve` block with a single call to `serve_ogg_window`.

## Testing and validation

### New unit tests

**`musefs-format/src/ogg/crc.rs`**
- `crc_shift_zeros_identity` — advancing `crc = 0` by any n stays 0.
- `crc_shift_zeros_matches_naive` — verify `crc_shift_zeros` against direct
  zero-byte loop (independent implementation of the same step).

**`musefs-format/src/ogg/page.rs`**
- `patch_algebraic_matches_full_page` — for a range of synthetic pages (varied
  payload sizes 0, 1, 255×255, and random; varied seq values), assert
  `patch_page_header_algebraic(header, new_seq) == patch_page_header(full_page,
  new_seq)`. This is the primary correctness gate for the algebraic shortcut.
- `verify_page_crc_accepts_valid_rejects_tampered` — a laced page verifies `true`;
  flipping one payload byte (or the stored CRC) verifies `false`.

**`musefs-core/src/ogg_index.rs`**
- `serve_ogg_window_renumbers_and_preserves_payload` — synthetic two-page file;
  assert seq renumbering and byte-identical payload. Mirrors the existing
  `read_at_renumbers_audio_and_preserves_payload` test at the lower level.
- `find_page_start_mid_page` — assert correct page-start location for a target
  offset mid-payload.
- `find_page_start_at_boundary` — assert the preceding page's start is returned
  when target is exactly on a page boundary, and the forward pass reaches the
  correct page.
- `find_page_start_skips_false_oggs_in_payload` — a real page whose payload embeds a
  coincidental `OggS` + plausible header (but invalid CRC); assert the backward scan
  rejects the fake via the CRC guard and returns the real page start. This is the
  correctness gate for the entry-page guard.

### Updated existing tests

**`musefs-core/src/reader.rs`** — four changes:
1. Remove `ogg_index: OnceCell::new()` from all `ResolvedFile` struct literals in
   `ogg_serve_tests`, `ogg_art_serve_tests`, and `cache_bound_tests`.
2. **Delete** `ogg_index_estimate_accounts_page_dense_files` (line 894) entirely —
   it tests `estimated_ogg_index_bytes` and the two constants, all of which are
   removed.
3. **Rewrite** `build_cache_bytes_includes_ogg_index_estimate` (line 731) to assert
   `cache_bytes == inline_byte_sum` for an Ogg file — the Ogg index estimate term
   is gone, so the correct assertion is the same as for non-Ogg formats. Rename to
   `build_cache_bytes_counts_inline_segments_for_ogg` to reflect the new meaning.
4. Replace the `OggAudio` arm construction (all uses of `OnceCell::new()`) with the
   struct-literal form after `ogg_index` field removal.

**`musefs-core/src/facade.rs`** (line 697): Remove `ogg_index: OnceCell::new()` from
the `ResolvedFile` literal constructed there.

**`musefs-core/tests/read_at.rs`** (line 120): Remove
`ogg_index: once_cell::sync::OnceCell::new()` from the `ResolvedFile` literal in the
integration test.

### Integrity guard in `serve_ogg_window`

`build_index` validated that `consumed == audio_length` at the end of the scan
(`ogg_index.rs:72`), catching truncated or misaligned audio regions. This guard is
silently dropped when `build_index` is removed. Re-introduce it in `serve_ogg_window`
as a cheap end-of-region assertion: after the serve loop exits with
`(P - audio_offset) >= rend`, assert (in debug builds via `debug_assert!`, hard error
in release) that `P - audio_offset <= audio_length`. A mismatch means the page walk
overran the declared audio region, which indicates a corrupt file or stale DB bounds.

### Validation — latency-injected run (VPS)

Per SP convention, storage-aware SPs are validated under injected latency as well as
tempfs (README §Conventions). The primary win of SP4 — eliminating the first-read
O(whole-file) scan on HDD/NFS — is not observable in the tempfs `ci` bench. On the
VPS, run:

```bash
MUSEFS_BENCH_LATENCY_PROFILE=nfs-hdd MUSEFS_BENCH_TIER=large-compute \
  cargo bench -p musefs-core --bench read_throughput
```

Record the `sequential_read` median for Ogg/Opus/OggFLAC before and after (first
iteration of each Criterion sample exercises the cold path). The improvement should
be measurable; record in BENCHMARKS.md and the tracking README.

### Unchanged gates (must stay green)

- `proptest_read_fidelity` — byte-identical round-trip for all formats.
- `cargo test -p musefs-format --features fuzzing` — format-layer fuzz.
- `#[ignore]`d FUSE e2e: `all_supported_formats_decode_to_same_pcm_sha_as_source`
  and `end_to_end_read_through_mount`.
- `sequential_read` Criterion bench (CI tier, tempfs): no format may rise >10% vs
  SP3 baseline. Ogg/Opus/OggFLAC expected to hold or improve (cold-start scan
  eliminated). If the `crc_shift_zeros` loop causes >10% regression in the warm
  path, replace with GF(2) polynomial exponentiation (O(log n)).
- `concurrent_read_walk/m16_plus_walker`: removing `OnceCell` removes the last
  first-read serialization point for Ogg — monitor for improvement.
- In-diff mutation gate: run `cargo mutants` on changed files; record caught/missed.

## Out of scope

- **Sparse checkpoint index in DB**: persisting page boundaries across remounts
  would further reduce cold-seek cost, but requires schema changes and is deferred.
  It is the natural SP5 if cold seeks on remounted libraries prove to be a
  bottleneck in practice.
- **Art-chunk zero-copy** and **FUSE reply-buffer zero-copy**: explicitly deferred
  from SP3, still deferred.
- **O(log n) CRC shift precomputation**: implement only if the regression gate
  forces it; do not pre-optimize.
