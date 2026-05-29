# Phase 2 — Ogg Hardening

**Part of:** Test-audit remediation (`docs/superpowers/specs/test-audit-remediation/2026-05-29-remediation-tracking.md`)
**Source audit:** `docs/audits/2026-05-29-test-audit.md` (findings #1, #2, #3, #4, #7, #8, #14)
**Survivor data:** `docs/superpowers/specs/test-audit-remediation/2026-05-29-mutation-inventory.md`
**Date:** 2026-05-29

## Purpose

Close the Ogg unit-test gaps and drive the **44 real Ogg mutation survivors**
(`ogg/mod.rs` 25, `ogg/page.rs` 13 missed + 5 timeout, `ogg/b64.rs` 3,
`ogg_index.rs` 3 — excluding 3 test-support survivors) toward zero, and stand up
the independent Ogg audio oracle the format/core suites currently lack.

**All changes are additive: tests, two dev-dependencies, and one named constant.**
No production logic changes, so the byte-identity invariant is untouched.

## Guiding principle: verify, don't trust

The audit was executed by a weaker model. Findings are leads, not facts. Each was
re-verified against the live code and the verified mutation inventory before
landing in this spec. That verification already overturned three audit claims (see
"Adjusted findings" below).

## Verified findings

| # | Sev | Verified status |
|--:|:---:|-----------------|
| 1 | P1 | **Confirmed.** `ogg_index.rs::serve` has zero unit tests. Inventory pins 3 survivors at `:105`, `:113`, `:117`. |
| 8 | P2 | **Confirmed.** Same `serve()` — no boundary tests (header-only, payload-only, spanning, empty, past-end). Folds into #1. |
| 2 | P1 | **Confirmed.** `tests/common/mod.rs:80` `resolve_layout` marks `OggAudio`/`OggArtSlice` `unreachable!`. The renumber+CRC logic lives in core's `ogg_index`, so the independent oracle belongs there. |
| 3 | P1 | **Confirmed.** `ogg_index.rs:72` `consume != audio_length` error path is untested. |
| 4 | P2 | **Confirmed.** `build_index_renumbers_and_preserves_payload_length` only asserts `seq` on page[0]; no `FLAG_CONTINUED`, CRC, or `payload_len` checks. |

### Adjusted findings (audit was wrong/imprecise)

| # | Audit claim | Verified reality | Decision |
|--:|-------------|------------------|----------|
| 7 | CRC edge cases undertested (53.1% line) | `ogg/crc.rs` has **0 survivors** (23 caught, 5 unviable). The reference-comparison test fully pins the logic; the line gap is const-table inflation, as the audit itself admits. | **Dropped.** No CRC correctness work. Recorded in the inventory. |
| 14 | `FLAG_EOS` "not defined or handled" | The parser passes `header_type` through verbatim (renumber + CRC only) — there is nothing to *handle*. The gap is the absence of a test that a set EOS bit survives the round-trip. | **Reframed.** Add `pub const FLAG_EOS: u8 = 0x04;` and a preservation test. No EOS *handling* logic. |
| — | `ogg/mod.rs:455` (3 survivors) | Mutations of `vorbis_body_empty`, a `#[cfg(test)]` helper. | **Excluded** from the kill-set. |

## Verification method (how a test "kills" a survivor)

The inventory lists each survivor as exact `file:line: mutation`. For every
targeted survivor the implementer:

1. Writes the test that should detect the mutation; runs it — **green** (production
   is correct).
2. **Hand-applies the mutation** at that line (e.g. change `<` to `<=`); reruns the
   test — it must go **red**. Reverts; **green** again.

This is dependency-free proof the test kills that specific mutant and is the TDD
"watch it fail" step adapted to mutation testing. Final confirmation is the next
full mutants campaign showing Ogg survivor counts dropped.

### Equivalent mutants (do not chase)

Some survivors are unkillable *equivalent* mutants — the mutation produces
behavior identical to the original, so no test can distinguish them:

- `ogg_index.rs:105` (`if hs < he` → `<=`): when `hs == he` the body slices
  `&header[a..a]` (empty) and extends nothing — identical output.
- `ogg_index.rs:113` (`if ps < pe` → `<=`): when `ps == pe`, `n == 0`, reads and
  extends nothing — identical output.

`ogg_index.rs:117` (`+` → `-` on the backing read offset) **is** genuinely
killable (a payload-only read at non-zero `within` reads wrong bytes). When the
hand-apply step in the method above yields identical behavior, mark the survivor
**equivalent** in the inventory rather than forcing a contrived test.

## Implementation ordering

C1 → C2 → C3 → C4, C5, C6 (independent, parallelizable) → C7.

C1 builds the `serve()` test fixtures (temp file + `OggPageIndex` + `serve()`
calls) that C3's oracle plugs into. C2 extends C1's existing test. C4–C6 are
independent of each other and of C1–C3. C7 is the wrap-up pass.

## Components

### C1 — `serve()` unit tests (findings #1, #8)

`musefs-core/src/ogg_index.rs`, `#[cfg(test)] mod tests`.

Build a small `OggPageIndex` plus a backing temp file, then assert `serve()`
output byte-for-byte against an independently assembled expected buffer for:

- header-only read (within one page's header)
- payload-only read (within one page's payload, non-zero `within` — kills `:117`)
- read spanning header + payload of one page
- read crossing a page boundary
- whole-region read (`rstart=0`, `rend=total`) equals the concatenation of every
  page's `header` + its backing payload
- empty result (`rstart == rend`, and a read entirely past the last page)
- read whose `rend` exceeds the region end (served bytes clamp to available)

Document `:105`/`:113` as equivalents per the method above.

### C2 — `build_index` tests (findings #3, #4)

Same module.

- **#3:** call `build_index` with an `audio_length` that does not land on a page
  boundary (and a truncated region) → assert `Err` mapping to
  `FormatError::Malformed`.
- **#4:** extend the existing test to additionally assert, for a multi-page
  packet: page[1] carries `FLAG_CONTINUED`; every page's recomputed header CRC is
  valid (via the C3 oracle helper); each `payload_len` equals its lacing-table
  sum; and `region_offset`s are contiguous and sum to `audio_length`.

### C3 — Independent Ogg oracle, "Both" (finding #2)

Add the RustAudio **`ogg`** crate (`ogg = "0.9"`, matching `musefs-fuse`) and
the **`crc`** crate (`crc = "3"`, matching `musefs-format`) as `dev-dependencies`
of `musefs-core`. Oracle
lives **in-crate** in `ogg_index.rs`'s `#[cfg(test)] mod tests` (dev-deps are
available to `#[cfg(test)]`, and `serve`/`build_index` are crate-visible there —
no production visibility change).

A helper takes full `serve()` output (the entire renumbered audio region) and:

1. **Structural decode (third-party):** feed the bytes to `ogg::PacketReader`;
   assert it reads every packet without error (the crate validates page CRCs
   during packet reassembly).
2. **Independent CRC (via the `crc` crate):** for each page in the served stream,
   recompute the CRC-32/Ogg with the `crc` crate (not `musefs-format::ogg::crc`)
   and compare to the embedded CRC field; assert each page's
   `seq == original_seq + seq_delta` and that seqs are monotonic.

Exercised across **Vorbis, Opus, and OggFLAC** streams built from existing
`page_test_support` helpers (`build_header`, `lace_packet`) plus minimal
codec-magic bodies.

`resolve_layout`'s `OggAudio`/`OggArtSlice` arms stay `unreachable!` **by design**:
renumbering is core's responsibility and `musefs-format` cannot reach it without
violating the layering. This is noted in the inventory so it is not re-flagged.

### C4 — `page.rs` mutant-kills + EOS (finding #14 reframed)

`musefs-format/src/ogg/page.rs`, `#[cfg(test)] mod tests`. Add
`pub const FLAG_EOS: u8 = 0x04;` next to the existing flag constants.

Targeted tests (kill the listed survivors):

- `parse_page` header/segment bounds: `:33`, `:47` (`> → ==`/`>=`) — truncated
  header and truncated segment table.
- `lace_packet`: `:93` (timeout), `:122` (`+= → *=`).
- `read_packets`: `:181` (`== → !=`).
- `patch_page_header`: `:197` (`< → >`).
- `lace_chunks_to_segments`: `:256` (timeout), `:263`/`:266` (`|= → &=`), `:265`
  (`delete !`), `:294` (timeout), `:298` (`- → +`).
- `copy_payload`: `:310` (`< → <=`).
- `emit_segments`: `:337` (`< → <=`).
- **Boundary fixtures (required):** tests at the 255-lacing-value and 65 025-byte
  payload limits, asserting correct lacing encoding and multi-page spanning.
- **EOS preservation:** a page with `header_type | FLAG_EOS` run through
  `parse_page` → `patch_page_header` (renumber) asserts the EOS bit (and full
  `header_type`) is preserved and the CRC recomputed.

The 5 timeouts (`:93`, `:256`, `:294`) are loop/allocation blow-ups — already
non-silent (cargo-mutants flags them). Add bounded assertions where cheap;
otherwise accept timeout as detection.

### C5 — `mod.rs` mutant-kills (~25 survivors)

`musefs-format/src/ogg/mod.rs`, `#[cfg(test)] mod tests`. Needs Ogg fixtures with
comments + cover art across codecs.

- `detect_codec`: `:25` (`&& → ||`) — include a non-matching codec case.
- `oggflac_following_packets`: `:36`.
- `comment_body`: `:113`; `comment_packet_index`: `:121`, `:130` (5 survivors:
  `delete !`, `&& → ||`, `& → |`, `& → ^`, `== → !=`).
- `locate_audio`: `:196`.
- `synthesize_layout`: `:233` (`+= → *=`), `:235`.
- `picture_prefix`: `:254` (`% → +`).
- `build_packets_with_art`: `:304`, `:305`, `:306`.
- `oggflac_packets_with_art`: `:409`, `:410`, `:439`.

**Excludes** `:455` (`vorbis_body_empty`, test-support).

### C6 — `b64.rs` mutant-kills (3 survivors)

`musefs-format/src/ogg/b64.rs`, `#[cfg(test)] mod tests`. `b64_window` `:26`
(`take - 1` and `(g1+1) * 3` arithmetic): tests at output-window/group boundaries
— `out_offset` and `take` at exact multiples of 4, and a `take` of 1 — asserting
the computed `in_start`/`in_len`/`skip` against hand-derived values.

### C7 — Inventory + tracking updates

- Mark `ogg_index.rs:105`/`:113` (and any others found equivalent) as **equivalent
  mutant** in the inventory.
- Record #7 dropped (0 CRC survivors) and #14 reframed.
- Flip Phase 2 status to complete in the tracking doc once C1–C6 land.

## Fixtures and test support

Reuse `musefs-format::ogg::page_test_support` (`build_header`, `lace_packet`,
`lace_packet_pub`) and `parse_page`/`patch_page_header`. Codec-specific bodies
(Vorbis `\x01vorbis`/`\x03vorbis`, Opus `OpusHead`/`OpusTags`, OggFLAC `\x7fFLAC`)
are small byte literals built inline. No new fixture files unless a test proves
one is needed.

## Error handling

No new error paths. C2 asserts the existing `FormatError::Malformed` mapping on the
`build_index` consume-mismatch path; everything else verifies existing behavior.

## Out of scope

- Any production logic change beyond `pub const FLAG_EOS`.
- CRC work (#7 — dropped).
- The 3 `mod.rs:455` test-support survivors.
- Phases 3 (format non-Ogg) and 4 (core & db).

## Verification summary

| Item | Check |
|------|-------|
| C1 | `cargo test -p musefs-core ogg_index` — serve() byte-identity across all read shapes; `:117` hand-apply goes red |
| C2 | consume-mismatch returns `Err(Malformed)`; multi-page assertions pass |
| C3 | `ogg` crate decodes `serve()` output for Vorbis/Opus/OggFLAC; independent CRC + seq checks pass |
| C4 | `cargo test -p musefs-format ogg::page` green; EOS bit preserved; listed `:line` mutations hand-apply red |
| C5 | `cargo test -p musefs-format ogg` green; listed mutations hand-apply red |
| C6 | `b64_window` boundary tests; `:26` mutations hand-apply red |
| Whole | next full mutants campaign shows Ogg survivor counts dropped (excluding documented equivalents) |

The existing Ogg proptest (`cargo test -p musefs-format --features fuzzing`) must
also remain green after all changes.
