# Skip oversized M4A `ilst` payloads before materializing them

Issue: [#297](https://github.com/Sohex/musefs/issues/297)

## Problem

The M4A scan path correctly avoids reading `mdat`, but it still loads the full
`moov` box (up to the 256 MiB `MAX_MP4_METADATA_BYTES` cap) and then clones
selected `ilst` payloads *before* the scanner's per-payload caps are applied:

- `musefs-format/src/mp4.rs` `read_pictures` copies every JPEG/PNG `covr`
  `data` payload via `dp[8..].to_vec()`.
- `musefs-format/src/mp4.rs` `read_binary_tags` copies every binary freeform
  `----` `data` payload via `dp[8..].to_vec()`.
- Only later does `ingest` (`musefs-core/src/scan.rs`) drop binary tags larger
  than `MAX_BINARY_TAG_BYTES` and art larger than `MAX_ART_BYTES`.

A crafted `.m4a` can place a very large `covr` or binary freeform `data` atom
in `moov`, forcing an allocation/copy of a payload that scanner policy will
immediately discard. Total exposure is bounded by the 256 MiB `moov` cap, not
the much lower art/binary caps (~16 MiB). This is attacker-controlled scan-time
memory pressure from untrusted metadata. It does **not** mutate or copy original
audio bytes — the cardinal invariant is unaffected.

## Scope

- **MP4 only.** The other formats' `read_pictures` / `read_binary_tags` read a
  small bounded prefix, not a 256 MiB `moov`, so they keep their current
  signatures and rely on the `ingest` backstop. They are out of scope.
- **Art and binary tags only.** Text materialization (`read_tags`) is deferred
  to #267 and is untouched here.

## Approach

Thread an explicit byte budget into the two MP4 extraction functions and skip
any payload that exceeds it *before* the `to_vec` copy. The caller
(`musefs-core`) continues to own the policy constants; the format layer stays
policy-free. Explicit budgets also make the skip-before-copy behavior directly
unit-testable with tiny buffers.

## Changes

### `musefs-format/src/mp4.rs`

- `read_pictures(buf: &[u8], max_art_bytes: usize) -> Vec<EmbeddedPicture>`

  After the existing `if dp.len() < 8 { continue; }` guard (which makes the
  subtraction safe), add:

  ```rust
  if dp.len() - 8 > max_art_bytes {
      continue;
  }
  ```

  `dp[8..]` is the art payload after the 8-byte `[type][locale]` header, so the
  budget applies to the same bytes `ingest` measures. Strict `>` means a payload
  *exactly* at the budget is still accepted, matching `ingest`'s current
  `<= MAX_ART_BYTES`.

- `read_binary_tags(buf: &[u8], max_binary_tag_bytes: usize) -> Vec<EmbeddedBinaryTag>`

  Place the same guard immediately after the `if dp.len() < 8 { continue; }`
  check, so an oversized `----` payload is skipped before the `name`/`mean`
  parsing and the `to_vec`.

`read_tags` is unchanged.

### `musefs-core/src/scan.rs`

Pass the existing module constants `MAX_ART_BYTES` (`pub(crate)`) and
`MAX_BINARY_TAG_BYTES` (private) at both probe sites — both are already in scope,
no visibility change is needed:

- `probe_full` — the buffer-path `.m4a`/`.m4b` arm calling
  `mp4::read_pictures(bytes)` / `mp4::read_binary_tags(bytes)`
- `probe_file` — the seek path, calling the same on `&scan.moov`

`ingest`'s `<= MAX_ART_BYTES` / `<= MAX_BINARY_TAG_BYTES` filters are left
**unchanged**. They remain the universal backstop for the formats that still
produce unbounded payloads (MP3/FLAC/OGG/WAV). For MP4 they become no-ops, so
the observable end state is identical — the only difference is that MP4 no
longer allocates throwaway copies.

### Test / fuzz callers (unbounded budget)

Callers that intend to extract everything pass `usize::MAX`:

- inline `#[cfg(test)]` callers in `musefs-format/src/mp4.rs` — **update all of
  them**. As of writing there are ~14: ten `read_pictures` calls and four
  `read_binary_tags` calls (including the `read_pictures(&[])` / `(garbage)`
  edge-case tests, which become `read_pictures(&[], usize::MAX)` etc.). Do not
  rely on these line numbers; grep `read_pictures\|read_binary_tags` within
  `mp4.rs` and update every call site so the suite stays green.
- `musefs-format/tests/proptest_mp4.rs` (`read_binary_tags`)
- `musefs-format/src/fuzz_check.rs` (`read_pictures`)
- `fuzz/fuzz_targets/mp4.rs` (`read_pictures` only — no `read_binary_tags` call
  there) — out-of-workspace; not built by the workspace, so update manually and
  verify with `cargo +nightly fuzz build mp4`

## Tests

### Format layer — `musefs-format/src/mp4.rs`

Unit tests using small budgets so no multi-megabyte buffers are needed:

1. A `covr` JPEG/PNG payload one byte over a small budget is skipped
   (`read_pictures` returns empty).
2. A binary freeform `----` payload one byte over a small budget is skipped
   (`read_binary_tags` returns empty).
3. Boundary: a payload *exactly* at the budget is still accepted — one case for
   art, one for binary.

### Core layer — `musefs-core/src/scan.rs`

The format-layer tests only exercise the budget *parameter*. They do not prove
the scan call sites pass the real caps — wiring `usize::MAX` at a probe site
would compile, pass every format-layer test, and silently reintroduce the bug
(the `ingest` backstop would still drop the payload, masking the regression).
So add a core-level test that proves the skip happens *before* `ingest*:

- Build an MP4 buffer with a `covr` (and a `----`) payload larger than the cap,
  probe it via `probe_file` (the seek path that loads the up-to-256 MiB `moov`
  via `read_structure_from` — the actual exposure in #297), and assert the
  oversized payload is **absent from `Probed.pictures` / `Probed.binary_tags`**
  — i.e. it was skipped at extraction, not merely filtered later by `ingest`.
  The test buffers need a single `soun` track to pass `validate_moov`; model the
  `covr` builder on the existing `mp4_with_binary_freeform` test helper.

The existing `ingest` oversize-filter tests in `scan.rs` are unaffected: they
construct `Probed` directly, so they continue to exercise the backstop filter
for the formats that still produce unbounded payloads.

## Docs

`docs/M4A.md` does not document the per-payload art/binary byte caps (it
describes which `covr`/`----` atoms are ingested and a separate resolve-time
structural cap), so **no doc change is required**. Its "every `data` child of a
`covr` atom is ingested" wording was already subject to the `ingest` cap and is
left as-is.

## Verification

- Full workspace test suite (`cargo test`) — also enforced by the pre-commit hook.
- `cargo clippy --all-targets`.
- `cargo +nightly fuzz build mp4` for the out-of-workspace fuzz target.

## Non-goals

- Reducing the 256 MiB `moov` cap itself (`read_structure_from` already enforces
  it).
- Text/`read_tags` payload caps (#267).
- Streaming or zero-copy extraction returning borrowed slices (a far larger
  refactor of `EmbeddedPicture` / `EmbeddedBinaryTag`, shared by all formats).
- Applying budgets to non-MP4 formats.
