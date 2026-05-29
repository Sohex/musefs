# Phase 3a — FLAC Hardening (Design)

**Source audit:** `docs/audits/2026-05-29-test-audit.md` (findings #5, #16, plus the
`flac.rs` mutation survivors from the phase-1 inventory)
**Created:** 2026-05-29
**Status:** design — awaiting plan

## Goal

Close the FLAC unit-test gaps and drive the **45 `flac.rs` mutation survivors**
toward zero, broaden the cross-format read-fidelity property test along the
dimensions finding #5 calls out (partial reads, header/audio boundary spanning,
art-segment serving), and add the zero-byte embedded-art boundary test (finding
#16).

**All changes are additive: tests only. No production logic changes and no new
dependencies**, so the byte-identity invariant is untouched. (Phase 2 needed one
named constant; 3a needs none.)

This is the first slice of Phase 3 (Format-layer coverage & mutants, non-Ogg),
which is decomposed into per-format sub-phases: **3a FLAC** (this doc), 3b MP3,
3c MP4, 3d WAV. Findings #5 and #16 are cross-cutting but small, so they ride with
3a; the non-FLAC dimension of #5 lands incrementally in 3b/3c/3d when each format's
fixtures get built.

## Guiding principle: verify, don't trust

The 2026-05-29 audit was executed by a weaker model; its findings are **leads, not
facts.** Every survivor below was re-read against the actual `flac.rs` source, and
several turned out to be **provably equivalent mutants** (see the equivalent-mutant
section). We document those rather than contrive tests for them.

## The hand-apply verification method (use for every kill)

cargo-mutants is not available locally. To prove a new test kills a specific
survivor, for each targeted `file:line: mutation`:

1. Run the new test → it passes (production code is correct).
2. Apply the exact mutation at that line, rerun **just that test** → it must **fail**.
3. Revert the mutation (`git checkout -- <file>`), rerun → passes again.

If step 2 still passes, the test does not kill the mutant. Either strengthen the
test, or — if the mutation provably produces identical behavior — record it as an
**equivalent mutant** instead of forcing a contrived test. Never leave a mutation
applied.

## Test placement

`flac.rs` has no test module today. Survivor functions split by visibility:

- `pub`: `synthesize_layout`, `read_vorbis_comments`, `read_pictures` — reachable
  from `musefs-format/tests/` integration files.
- `pub(crate)` / private: `parse_blocks`, `push_block_header`, `read_u32_be`,
  `parse_picture_block` — **not** reachable from integration tests (separate crate).

The bit-level kills need precise byte control, which is cleanest with direct calls.
So the bulk of 3a lives in a new `#[cfg(test)] mod tests` inside `flac.rs` (mirrors
Phase 2's in-module tests in `page.rs`/`mod.rs`/`b64.rs`), with the `pub`-function
work strengthening the existing `flac_pictures.rs` / `read_comments.rs` integration
tests where they already exercise the path.

**Approach considered and rejected:** integration-tests-only. The private helpers
(`parse_blocks`, `read_u32_be`, `parse_picture_block`) are only reachable
indirectly through `pub` entry points, which makes the byte-precise fixtures for
the bit-op kills awkward and indirect. In-module unit tests are the right tool.

## Verified findings

The 45 survivors cluster by function. Kill strategy and killability were verified
by reading the source.

| Function | Survivor lines | Nature | Strategy |
|----------|----------------|--------|----------|
| `read_u32_be` | :219 (`>`→`==`/`>=`), :224 (`+`→`*` on the index) | bounds + byte-index | in-module: 4 distinct bytes pin the BE assembly; short slice pins the bound |
| `parse_blocks` | :37 (`<`→`==`/`<=`), :43 (`+`→`-`, `>`→`==`/`>=`), :49 (`<<16`→`>>16`) | header/length decode | crafted fixtures incl. malformed-path (see "shift kills") |
| `push_block_header` | :101 (`>>16`→`<<16`) | 24-bit length emit | in-module: `body_len = 0x123456`, assert emitted bytes |
| `synthesize_layout` | :155 (`>`→`>=`) | 24-bit TooLarge guard | exact-boundary test at `body_len == 0x00FF_FFFF` |
| `read_vorbis_comments` | :188 (`<`→`==`/`<=`, `\|\|`→`&&`), :193 (`+`→`-`, `>`→`==`/`>=`), :199 (`<<16`→`>>16`), :200 (`\|`→`&`, `<<8`→`>>8`), :204 (`>`→`==`/`>=`) | block-walk + length decode | crafted fixtures + malformed-path |
| `parse_picture_block` | :237 (`>`→`==`/`>=`), :245 (`>`→`==`/`>=`), :261 (`>`→`<`) | mime/desc/data bounds | in-module: picture bodies truncated at each boundary |
| `read_pictures` | :277 (`<`→`==`/`<=`, `\|\|`→`&&`), :283 (`+`→`-`, `>`→`==`/`>=`), :289 (`<<16`→`>>16`), :290 (`\|`→`&`, `<<8`→`>>8`), :294 (`>`→`==`/`>=`) | block-walk + length decode | crafted fixtures + malformed-path |

### "Shift kills" technique (`<<16`/`<<8` → `>>`)

The 24-bit length decode reads bytes that are normally zero in small fixtures, so
the existing happy-path tests can't observe a flipped shift. Killing them does
**not** require 64 KB bodies: set the high (or mid) length byte nonzero and assert
the **malformed vs parsed** divergence. E.g. a STREAMINFO header with length bytes
`[0x01, 0x00, 0x00]` (= 0x010000) over a short body: the original computes
`len = 65536` → `body_end > data.len()` → `Err(Malformed)`; the `>>16` mutant
computes `len = 0` → body fits → `Ok`. The Err/Ok split kills the mutant with a
tiny fixture.

## Equivalent mutants (do not chase — document)

The 24-bit length decode `(b1 << 16) | (b2 << 8) | b3` shifts each byte into a
**disjoint** bit range (23–16, 15–8, 7–0). For disjoint operands, `|` ≡ `^` ≡ `+`,
so the following `| → ^` mutations produce byte-identical results and are
**equivalent**:

- `parse_blocks`: `:50`, `:51`
- `read_vorbis_comments`: `:200` (the `\| → ^` variant), `:201`
- `read_pictures`: `:290` (the `\| → ^` variant), `:291`

Likewise `push_block_header:99` — `(if is_last {0x80} else {0}) | (block_type &
0x7F)` ORs bit 7 with a value masked to bits 6–0 (disjoint), so its `| → ^` is
**equivalent**.

The sibling `| → &` mutations on the same lines (`:200`, `:290`) are **not**
equivalent (AND of disjoint ranges = 0) and **are** killed. Each equivalence is
re-confirmed by hand-apply (the test stays green under `^`) before being recorded.

## Components

### C1 — byte-decode helper kills (in-module `#[cfg(test)] mod tests`)

`read_u32_be` (:219, :224), `parse_blocks` (:37, :43, :49 + record :50/:51 equiv),
`push_block_header` (:101 killed, :99 equiv). Fixtures: distinct-byte BE values,
truncated headers, and the shift-kill malformed fixtures.

### C2 — VORBIS_COMMENT + picture parsing kills

`read_vorbis_comments` (:188, :193, :199, :200 `&`, :204; record :200 `^`/:201
equiv), `parse_picture_block` (:237, :245, :261), `read_pictures` (:277, :283,
:289, :290 `&`, :294; record :290 `^`/:291 equiv). In-module for the private
`parse_picture_block`; strengthen `flac_pictures.rs` / `read_comments.rs` for the
`pub` walkers where they already build real fixtures.

### C3 — `synthesize_layout:155` boundary

`> → >=` on the 24-bit `TooLarge` guard. The existing
`synthesize_errors_on_oversized_picture` test uses `data_len` well over the limit,
so `>` and `>=` both fire — it can't distinguish them. Add a test at
`body_len == 0x00FF_FFFF` exactly (`data_len = 0x00FF_FFFF - framing.len()`):
original returns `Ok` (boundary is inclusive-valid), `>=` mutant returns
`Err(TooLarge)`.

### C4 — Finding #5: broaden `proptest_read_fidelity` (FLAC)

`musefs-core/tests/proptest_read_fidelity.rs` today reads only `[0, total_len)`
with no art. Broaden, on FLAC:

- **Partial reads:** random `(offset, size)` windows; assert the served bytes equal
  the corresponding slice of an independently-assembled whole.
- **Header/audio boundary spanning:** windows straddling `header_len` (the
  Inline→BackingAudio seam).
- **Art-segment serving:** build a track with embedded art (an `ArtImage` segment)
  and assert partial reads across the art window serve the blob bytes correctly.

Non-FLAC formats are deferred to 3b/3c/3d (their fixtures don't exist in
`musefs-core/tests/common` yet).

### C5 — Finding #16: zero-byte embedded art

Extend `musefs-format/tests/synthesize_art.rs`: synthesize a picture with
`data_len == 0` and assert the boundary behavior — a valid zero-length PICTURE
block (the `ArtImage` segment has `len: 0`, the layout round-trips, and
`metaflac` reads a picture with empty data). Pins the `data_len == 0` edge that
the survivor bounds (`parse_picture_block:261`, the `data_end > body.len()` guard)
depend on.

### C6 — inventory + tracking docs

Annotate the `flac.rs` rows in `2026-05-29-mutation-inventory.md` (`killed (phase
3a)` / `equivalent`), and mark Phase 3a complete in `2026-05-29-remediation-tracking.md`
(recording the equivalent set and that findings #5/#16 are addressed for FLAC, with
non-FLAC #5 coverage tracked into 3b/3c/3d).

## Implementation ordering

C1 → C2 → C3 (the in-`flac.rs` kills, building the test module incrementally) →
C4, C5 (independent of each other and of C1–C3) → C6 (wrap-up).

## Error handling

No new error paths. The kills assert the existing `FormatError::{NotFlac,
Malformed, TooLarge}` mappings on crafted inputs; nothing in production changes.

## Acceptance

| Component | Check |
|-----------|-------|
| C1 | `cargo test -p musefs-format --features fuzzing flac` green; :219/:224/:37/:43/:49/:101 hand-apply red; :50/:51/:99 stay green under `^` (equivalent) |
| C2 | listed `read_vorbis_comments`/`parse_picture_block`/`read_pictures` mutations hand-apply red; `| → ^` rows recorded equivalent |
| C3 | boundary test at `0x00FF_FFFF` passes; `> → >=` hand-apply red |
| C4 | `cargo test -p musefs-core proptest_read_fidelity` green; partial-read / boundary / art windows covered |
| C5 | zero-byte art test passes; round-trips via `metaflac` |
| Whole | `cargo test --workspace` + `--features fuzzing` + `clippy -D warnings` + `fmt --check` green; next full mutants campaign shows `flac.rs` survivors dropped (excluding documented equivalents) |
