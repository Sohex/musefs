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

**Changes are additive tests plus one small scoped production fix, no new
dependencies.** The production change is the C5 zero-byte-art skip in
`flac.rs::synthesize_layout` (a degenerate empty picture must not brick a track).
**The byte-identity invariant is untouched** — no change in 3a touches the
positioned audio reads, only whether an empty metadata-block is emitted. (Phase 2
needed one named constant; 3a needs the zero-byte-art skip.)

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
**All `flac.rs` mutant kills live in one new `#[cfg(test)] mod tests` inside
`flac.rs`** (mirrors Phase 2's in-module tests in `page.rs`/`mod.rs`/`b64.rs`). A
`#[cfg(test)] mod tests` with `use super::*` can call every survivor function
directly — `pub`, `pub(crate)`, and private alike — so there is no reason to split
kills across integration files. The existing integration tests (`flac_pictures.rs`,
`read_comments.rs`, `layout.rs`, `locate.rs`, `proptest_flac.rs`) stay **unchanged**;
they provide end-to-end coverage but are not the vehicle for any mutant kill in 3a.
This keeps the kill→test mapping unambiguous (one module owns it) and avoids
redundant or gap-prone work across files.

The only 3a tests *outside* `flac.rs` are the cross-cutting C4 (`proptest_read_fidelity.rs`,
in `musefs-core`) and C5 (`synthesize_art.rs`, in `musefs-format/tests`).

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
| `parse_picture_block` | :237 (`>`→`==` kill; `>=` **equiv**), :245 (`>`→`==` kill; `>=` **equiv**), :261 (`>`→`<`) | mime/desc/data bounds | in-module: over-length field → panic kills `==`; trailing-byte body kills `:261`; `>=` equivalent (see below) |
| `read_pictures` | :277 (`<`→`==`/`<=`, `\|\|`→`&&`), :283 (`+`→`-`, `>`→`==`/`>=`), :289 (`<<16`→`>>16`), :290 (`\|`→`&`, `<<8`→`>>8`), :294 (`>`→`==`/`>=`) | block-walk + length decode | crafted fixtures + malformed-path |

### Kill mechanism: panic-vs-`Err` for the short-input guards

Some bound mutations kill by **panic-vs-`Err` divergence**, not a clean value
comparison — the test must assert the *clean* `Err` and the mutation turns it into
an out-of-bounds panic. Note this in the test so the mechanism is readable:

- `parse_blocks:37` / `read_vorbis_comments:188` / `read_pictures:277`,
  `< → ==` variant: with a **3-byte** input the original short-circuits
  (`len < 4` true → `Err(NotFlac)`); the `==` mutant evaluates `3 == 4` → false →
  falls through to `&data[0..4]` → **panic**. The test asserts `Err(NotFlac)` on a
  3-byte input; the kill is panic ≠ Err.
- The `< → <=` variant of the same lines kills cleanly by value: a **4-byte**
  `fLaC`-only input gives original `Err(Malformed)` (loop reaches `pos+4 > len`)
  vs. mutant `Err(NotFlac)` (`4 <= 4` short-circuits).
- The `|| → &&` variant (`:188`, `:277`) likewise kills via panic-vs-`Err` on a
  3-byte input (`&&` forces evaluation of `data[0..4]`).

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
equivalent (AND of disjoint ranges = 0) and **are** killed.

**Inclusive-bound `> → >=` in `parse_picture_block` (`:237`, `:245`) are also
equivalent** (refinement found during planning). The only input that distinguishes
`>` from `>=` is `*_end == body.len()`; there the original proceeds and immediately
fails at the *next* `read_u32_be` (zero bytes remain) → `Err(Malformed)`, identical
to the mutant's direct `Err(Malformed)`. Their `> → ==` siblings remain killable
(an `*_end > len` input makes `==` fall through to an out-of-bounds slice → panic).

Each equivalence is re-confirmed by hand-apply (the test stays green under the
mutation) before being recorded.

## Components

### C1 — byte-decode helper kills (in-module `#[cfg(test)] mod tests`)

`read_u32_be` (:219, :224), `parse_blocks` (:37, :43, :49 + record :50/:51 equiv),
`push_block_header` (:101 killed, :99 equiv). Fixtures: distinct-byte BE values,
truncated headers, and the shift-kill malformed fixtures.

### C2 — VORBIS_COMMENT + picture parsing kills

`read_vorbis_comments` (:188, :193, :199, :200 `&`, :204; record :200 `^`/:201
equiv), `parse_picture_block` (:237, :245, :261), `read_pictures` (:277, :283,
:289, :290 `&`, :294; record :290 `^`/:291 equiv). **All in-module** — the test
module calls `read_vorbis_comments`/`read_pictures` (`pub`) and `parse_picture_block`
(`pub(crate)`) directly via `use super::*`, with crafted FLAC byte arrays. The
existing `flac_pictures.rs` / `read_comments.rs` integration tests are not touched.

### C3 — `synthesize_layout:155` boundary

`> → >=` on the 24-bit `TooLarge` guard. The existing
`synthesize_errors_on_oversized_picture` test uses `data_len` well over the limit,
so `>` and `>=` both fire — it can't distinguish them. Add a test at
`body_len == 0x00FF_FFFF` exactly (`data_len = 0x00FF_FFFF - framing.len()`):
original returns `Ok` (boundary is inclusive-valid), `>=` mutant returns
`Err(TooLarge)`.

### C4 — Finding #5: broaden `proptest_read_fidelity` (FLAC)

`musefs-core/tests/proptest_read_fidelity.rs` today has one property
(`read_at_preserves_backing_audio`) that reads only `[0, total_len)` with no art.
Broaden on FLAC with these named properties, each asserting served bytes equal the
corresponding slice of an independently-assembled `whole` (the existing
`read_at(&resolved, &db, 0, total_len)` output is the reference):

- **`read_at_partial_windows_match_whole`** — strategy: `offset in 0..=total_len`,
  `size in 0..=(total_len - offset)` (or clamp `offset+size` to `total_len`); assert
  `read_at(.., offset, size) == whole[offset..offset+size]` and the returned length
  equals `size`. Covers arbitrary partial reads, empty reads, and tail reads.
- **`read_at_windows_spanning_header_seam`** — strategy: pick windows that straddle
  `resolved.layout.header_len()` (e.g. `start in 0..header_len`, `end in
  header_len..total_len`); assert equality with `whole[start..end]`. Pins the
  Inline→BackingAudio seam.
- **`read_at_art_window_serves_blob`** — uses the new art fixture (below); strategy:
  windows overlapping the `ArtImage` segment's byte range; assert the served bytes
  match the inserted art blob at the right offsets and that audio after the art is
  still byte-identical.

**Art-fixture sub-task (non-trivial — its own task in the plan).** `write_flac`
produces tags only; there is no art helper. Add `build_with_art(...)` in this test
file that, after `upsert_track` + `replace_tags`:

1. `let art_id = db.upsert_art(&NewArt { mime: "image/jpeg".into(), width: Some(8),
   height: Some(8), data: <blob> })?;`
2. `db.set_track_art(track_id, &[TrackArt { art_id, picture_type: 3, description:
   "front".into(), ordinal: 0 }])?;`

`HeaderCache::resolve` then emits an `ArtImage` segment via
`mapping::track_art_to_inputs`, and `read_at` serves the blob through
`db.read_art_chunk`. **Reference the existing `musefs-core/tests/reader.rs` and
`tests/read_at.rs`**, which already insert and link art this way — follow their
pattern rather than inventing one. Keep `ProptestConfig::with_cases` modest (≤64,
matching the existing property) since each case does DB + file I/O.

Non-FLAC formats are deferred to 3b/3c/3d (their fixtures don't exist in
`musefs-core/tests/common` yet); 3a does not add `write_mp3/mp4/wav`.

### C5 — Finding #16: zero-byte embedded art (skip at synthesis)

**Verified behavior (not an assumption).** Today `synthesize_layout` builds
`Segment::ArtImage { len: 0 }` for zero-byte art, which `RegionLayout::validate`
(`layout.rs:102`, `EmptySegment`) rejects → `synthesize_layout` returns
`Err(FormatError::InvalidLayout)`. Ingestion (`scan.rs:162`) only filters art
*above* `MAX_ART_BYTES`, so a source file with an empty PICTURE block is ingested
and then makes the **whole track unreadable** at serve time. That is a real
robustness gap.

**Resolution (chosen): skip zero-byte art at FLAC synthesis.** `synthesize_layout`
filters arts with `data_len == 0` **before** computing `num_blocks`/`last_index`
(so the FLAC `is_last` flag lands on the true final block) and `continue`s past them
in the emit loop. The track then serves normally without the empty picture. This is
a small, scoped production change in `musefs-format/src/flac.rs`; **byte-identity is
unaffected** — it only decides whether an empty PICTURE metadata block is emitted,
never the positioned audio reads.

`musefs-format/tests/synthesize_art.rs` asserts: zero-byte art → no `ArtImage`
segment, the layout is valid and round-trips, and `metaflac` reads **zero** pictures;
plus a mixed case (empty + real art) confirming the surviving PICTURE block keeps its
last-block flag.

**Cross-cutting follow-up (noted, not done in 3a):** the same degenerate-art skip
applies to mp3/mp4/ogg/wav synthesis — handle it in each format's sub-phase, or
filter empty art once at ingestion (`scan.rs`, add `&& !p.data.is_empty()`). 3a
fixes FLAC and records the follow-up.

### C6 — inventory + tracking docs

Annotate the `flac.rs` rows in `2026-05-29-mutation-inventory.md` (`killed (phase
3a)` / `equivalent`), and mark Phase 3a complete in `2026-05-29-remediation-tracking.md`
(recording the equivalent set and that findings #5/#16 are addressed for FLAC, with
non-FLAC #5 coverage tracked into 3b/3c/3d).

## Implementation ordering

C1 → C2 → C3 (the in-`flac.rs` kills, building the test module incrementally) →
C4, C5 (independent of each other and of C1–C3) → C6 (wrap-up).

**Re-verify line numbers before each component.** All survivor references are
pinned to exact `flac.rs` line numbers from the phase-1 inventory; any intervening
edit shifts them. At the start of each component, locate the target by its **code
pattern** (named in the verified-findings table — e.g. "the `<< 16` in the 24-bit
length decode", "the `data_end > body.len()` guard in `parse_picture_block`") and
confirm the current line before applying the hand-apply mutation. The pattern is
the source of truth; the line number is a convenience that may drift.

## Error handling

No new error paths. The kills assert the existing `FormatError::{NotFlac,
Malformed, TooLarge}` mappings on crafted inputs; nothing in production changes.

## Acceptance

| Component | Check |
|-----------|-------|
| C1 | `cargo test -p musefs-format --features fuzzing flac` green; :219/:224/:37/:43/:49/:101 hand-apply red; :50/:51/:99 stay green under `^` (equivalent) |
| C2 | listed `read_vorbis_comments`/`parse_picture_block`/`read_pictures` mutations hand-apply red; `| → ^` rows recorded equivalent |
| C3 | boundary test at `0x00FF_FFFF` passes; `> → >=` hand-apply red |
| C4 | `cargo test -p musefs-core proptest_read_fidelity` green; the three named properties (`read_at_partial_windows_match_whole`, `read_at_windows_spanning_header_seam`, `read_at_art_window_serves_blob`) pass with the art fixture |
| C5 | `synthesize_layout` skips zero-byte art; the synthesize_art tests assert no `ArtImage` segment, valid round-trip, `metaflac` reads 0 pictures, and the mixed empty+real case keeps the last-block flag; existing non-empty-art tests still pass |
| Whole | `cargo test --workspace` + `--features fuzzing` + `clippy -D warnings` + `fmt --check` green; next full mutants campaign shows `flac.rs` survivors dropped (excluding documented equivalents) |
