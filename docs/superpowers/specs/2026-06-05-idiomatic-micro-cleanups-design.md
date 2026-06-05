# Idiomatic micro-cleanups across format/db/core/latencyfs (issue #138)

**Date:** 2026-06-05
**Issue:** #138
**Status:** Approved

## Goal

Fix six small non-idiomatic patterns flagged by the v1 review triage. None are
behavior-changing: every touched path is already pinned by byte-exact tests
(format proptests, interop fixtures, mutant-kill tests), and the served-byte
output must be identical before and after.

**Scope:** src **and** test code (the bare-`282` item is test-only, so test
files are explicitly in scope). One branch, one PR, one commit per category
(six commits) for reviewability and bisectability.

## The six cleanups

### 1. 24-bit big-endian assembly → `from_be_bytes`

Manual 24-bit big-endian shift assembly, both directions:

- **Read side** — `(b1 << 16) | (b2 << 8) | b3`:
  - `musefs-format/src/flac.rs` — 4 sites (lines ~50, ~105, ~321, ~411): add a
    private `fn u24_be(b0: u8, b1: u8, b2: u8) -> usize` implemented as
    `u32::from_be_bytes([0, b0, b1, b2]) as usize` and use it at all four
    sites.
  - `musefs-format/src/mp3.rs` — 1 site (~479, the ID3v2.2 three-byte frame
    size): inline `u32::from_be_bytes` with a leading `0` pad byte, same shape
    as `u24_be` (a shared helper across crates is not warranted for one extra
    site).
- **Write side** — per-byte `push((n >> 16) as u8)` shifts:
  - `flac.rs::push_block_header` (~152) and the `raw_block` test helper
    (~498): replace the three pushes with
    `extend_from_slice(&(n as u32).to_be_bytes()[1..])`.

**Mutation-gate residue to clean up in the same commit:**

- `.cargo/mutants.toml` carries an exclude
  `'musefs-format/src/flac\.rs:\d+:\d+: replace \| with \^ in read_metadata_bounded'`
  for the `|`-assembly equivalence. With `from_be_bytes` there is no `|` to
  mutate; delete the entry. Note the exclude covers `read_metadata_bounded`
  only — the other three read sites never had one, so removing the `|`
  everywhere only *removes* mutants and cannot fail the gate.
- The four read sites sit in four different functions, each with mutant-kill
  test comments that name the retired shift/`|` mutants: `flac.rs:547-548`,
  `:617`, `:705`, `:805-811`, `:821`. Reword all of them to describe the
  invariant (e.g. truncated-length handling), not the retired mutants.

### 2. Deduplicate `sha256_hex`, drop the hand-rolled hex loop

`sha256_hex` is duplicated identically in `musefs-db/src/art.rs:6` and
`musefs-db/src/bulk.rs:6`, each hand-encoding hex with a per-byte `write!`
loop. Keep a single `pub(crate)` copy in `art.rs` with body
`format!("{:x}", Sha256::digest(data))` (sha2's digest output implements
`LowerHex`); `bulk.rs` imports it. The known-digest test in `bulk.rs`
(`sha256_hex_matches_known_digest`) stays and keeps pinning the output.

### 3. `to_string_lossy().to_string()` → `.into_owned()`

Mechanical swap at all 28 sites — src: `musefs-core/src/scan.rs`,
`facade.rs`, `reader.rs`; tests: `proptest_read_fidelity.rs`,
`interop_emit.rs`, `external_contract.rs`, `read_at.rs`, `tests/reader.rs`.
`.into_owned()` avoids the unconditional reallocation when the lossy
conversion borrows.

### 4. `wav.rs` chunk assembly helpers

`build_info_payload` and the `id3 `/`data` chunk heads in `synthesize_layout`
hand-interleave fourcc/LE-length/payload/pad `extend_from_slice` calls.
Extract:

- `fn chunk_header(id: &[u8; 4], len: u32) -> [u8; 8]` — used by the `id3 `
  and `data` heads, whose payloads are segments (not bytes) and so cannot be
  fully inlined.
- `fn append_chunk(out: &mut Vec<u8>, id: &[u8; 4], payload: &[u8])` — header
  + payload + odd-length pad; shared by `push_inline_chunk` (which wraps it in
  a `Segment::Inline`) and `build_info_payload`'s subchunk loop.

The RIFF header stays as-is: it is the 12-byte file head
(`RIFF` + size + `WAVE`), not a padded chunk. Byte output is pinned by the
existing WAV unit tests, `proptest_wav`, and the interop fixtures.

### 5. Name the magic numbers

- `musefs-format/src/mp3.rs`: `const SYNCHSAFE_MAX: usize = 0x0FFF_FFFF;`
  replaces the two inline guards (~138, ~386). Tests may keep the literal
  where the test is documenting the boundary value itself. The const stays
  mutation-killable: the existing boundary tests pin both guards
  (cargo-mutants mutates const initializers — see the project memory — and
  these are observable).
- `musefs-core/src/ogg_index.rs`: `MAX_OGG_HEADER_BYTES` (already defined at
  line 27) replaces the 7 bare `282` allocation sites in the test module.

### 6. `statvfs` via `MaybeUninit`

`musefs-latencyfs/src/lib.rs:404` initializes `libc::statvfs` with
`mem::zeroed()`. Switch to `MaybeUninit::<libc::statvfs>::uninit()`, pass
`.as_mut_ptr()` to `libc::statvfs`, and `assume_init()` only after the
`== 0` return check. (`musefs-latencyfs` is excluded from the in-diff
mutation gate, so this carries no gate cost.)

## Error handling

No new fallible paths. The `as u32` length casts in `wav.rs` keep their
existing guarantees (`synthesize_layout` already rejects payloads
`> u32::MAX` up front).

## Validation

- `cargo fmt --all --check`
- `cargo clippy --all-targets`
- `cargo test --workspace`
- `cargo test -p musefs-format --features fuzzing`
- In-diff mutation gate (CI parity): `-j2`, output under `/tmp`, default
  `TMPDIR`, sanity-check `mutants.diff` is non-empty first.
- Format-layer signatures do not change, so the out-of-workspace fuzz targets
  are unaffected.

## Risk

Near-zero. All rewrites are local, behavior-preserving, and covered by
byte-exact tests. The one subtlety is keeping rewritten arithmetic
mutation-killable; the existing boundary/equivalence tests already cover the
new shapes, and the stale flac exclude is removed rather than left to mask a
live mutant.
