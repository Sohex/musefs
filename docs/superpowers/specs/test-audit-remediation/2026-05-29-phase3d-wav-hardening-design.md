# Phase 3d — WAV Hardening (Design)

**Source audit:** `docs/audits/2026-05-29-test-audit.md` (the WAV dimension of
findings #5 and #16, plus the 28 `wav.rs` mutation survivors from the phase-1
inventory `2026-05-29-mutation-inventory.md`)
**Created:** 2026-05-29
**Status:** design — awaiting plan

## Goal

Drive the **28 `wav.rs` mutation survivors** toward zero, add the WAV dimension of
finding #5 (broaden the cross-format `proptest_read_fidelity` property), and close
finding #16 for WAV with a scoped zero-byte-art skip.

**Changes are additive tests plus one small scoped production fix, no new
dependencies.** The production change is the C2 zero-byte-art skip in
`wav::synthesize_layout` (a degenerate empty picture must not brick a track). **The
byte-identity invariant is untouched** — the skip only decides whether an empty
APIC frame is emitted into the embedded `id3 ` chunk, never the positioned `data`
reads.

This is the final per-format slice of Phase 3 (Format-layer coverage & mutants,
non-Ogg): **3a FLAC** (done), 3b MP3, 3c MP4, **3d WAV** (this doc). The WAV slice
of finding #5 lands here (its FLAC slice landed in 3a; mp3/mp4 in 3b/3c).

## Guiding principle: verify, don't trust

The 2026-05-29 audit was executed by a weaker model; its findings are **leads, not
facts.** Every survivor below was re-read against the actual `wav.rs` source. Two
clusters turned out to be **suspected-equivalent mutants** (`walk_chunks:49` and
both `synthesize_layout:186` variants — see the equivalent-mutant section); the
design's job is to *confirm* those by hand-apply and document them, exactly as 3a
confirmed its disjoint-bitfield `| → ^` equivalents. We never contrive a test for a
provably equivalent mutant.

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

While a mutation is applied, run **only the single targeted test**
(`cargo test -p musefs-format <test_name>`), never `cargo test --workspace` — a
live mutation can break unrelated tests and obscure the kill signal. Revert
(`git checkout -- musefs-format/src/wav.rs`) immediately after step 2.

## Test placement

`wav.rs` already has a small in-module `#[cfg(test)] mod tests` (the fuzz-discovered
OOM regression `wav_oom_crash_artifact_is_safe`). Survivor functions split by
visibility:

- `pub`: `locate_audio`, `read_structure`, `synthesize_layout`, `read_tags`,
  `read_pictures` — reachable from `musefs-format/tests/` integration files.
- private: `riff_wave_start`, `walk_chunks`, `chunk_slice`, `info_fourcc`,
  `build_info_payload`, `push_inline_chunk`, `info_to_key`, `find_id3_chunk`,
  `read_info_tags` — **not** reachable from integration tests (separate crate).

The byte-level kills (`info_fourcc`/`info_to_key` arms, the `% 2` word-align
checks, `walk_chunks` advance) need precise byte control on private helpers, which
is cleanest with direct calls. **All `wav.rs` mutant kills live in the existing
in-module `#[cfg(test)] mod tests` (expanded)** — `use super::*` reaches every
survivor function directly, `pub` and private alike, so there is no reason to split
kills across integration files. The existing integration tests (`wav_locate.rs`,
`wav_read_tags.rs`, `wav_synthesize.rs`, `proptest_wav.rs`) stay **unchanged**;
they provide end-to-end coverage but are not the vehicle for any mutant kill in 3d.
This keeps the kill→test mapping unambiguous (one module owns it).

The module's current import is narrow — `use super::{read_pictures, read_tags}`.
C1 **widens it to `use super::*`** so the expanded tests can call the private
helpers (`walk_chunks`, `build_info_payload`, `push_inline_chunk`, `info_fourcc`,
`info_to_key`, `riff_wave_start`) and `synthesize_layout` directly.

**Out of scope (noted, not fixed):** the integration files duplicate
`fmt_pcm_16bit_mono()` and `build_wav()` helpers (copy-pasted across
`wav_locate.rs`, `wav_read_tags.rs`, `wav_synthesize.rs`). 3d leaves these files
unchanged and does **not** consolidate the helpers — that de-duplication is
unrelated test-tidy tech debt for a separate pass, called out here only to prevent
future confusion.

The only 3d tests *outside* `wav.rs` are the cross-cutting C3 (`proptest_read_fidelity.rs`
in `musefs-core`) and C2's assertion (`wav_synthesize.rs`, which is the natural home
for the zero-byte-art synthesis behaviour and does get the one new C2 test —
distinct from the kill module).

**Approach considered and rejected:** integration-tests-only. The private helpers
(`build_info_payload`, `push_inline_chunk`, `walk_chunks`, `info_fourcc`) are only
reachable indirectly through `pub` entry points, which makes the byte-precise
fixtures for the word-align and arm-deletion kills awkward and indirect. In-module
unit tests are the right tool.

## Verified findings

The 28 survivors cluster by function. Kill strategy and killability were verified by
reading the source. **Re-confirm each line by code pattern (not the line number)
before hand-applying** — intervening edits drift the numbers; the pattern named in
the table is the source of truth.

| Function / lines | Mutations | Nature | Strategy |
|---|---|---|---|
| `riff_wave_start:24` | `<`→`==`, `<`→`<=` | container-length guard `buf.len() < 12` | `<=`: a valid 12-byte `RIFF????WAVE` → orig `Ok(12)`, mutant `12 <= 12` → `NotWav`. `==`: an 11-byte buffer starting `RIFF` → orig short-circuits (`11 < 12` true → `NotWav`); mutant `11 == 12` false → falls through to `&buf[8..12]` on an 11-byte slice → **panic** (panic-vs-`Err`) |
| `walk_chunks:47` | `+`→`-` (in `8u64 + size + (size & 1)`) | header-to-header advance | multi-chunk fixture (`fmt `,`data`): a wrong advance loses the `data` chunk, so `walk_chunks`/`locate_audio` returns a different chunk set; assert the parsed chunks |
| `walk_chunks:49` | guard `next <= buf.len()` → `true` | over-buffer break | **suspected equivalent** — when `next > buf.len()` the mutant sets `pos = next` then the `while pos + 8 <= buf.len()` test is immediately false, so the output `Vec` is identical to the original's `break` (the chunk header was already pushed before the advance). Confirm by hand-apply; document |
| `locate_audio:67` | `==`→`!=` (`id == b"fmt "`) | `fmt ` presence detection | a **data-only** WAV (a `data` chunk, no `fmt `): orig `has_fmt = false` → `NotWav`; mutant `any(id != "fmt ")` is true (the `data` chunk) → `has_fmt = true` → returns `Ok`/`Malformed`. Either differs from `NotWav` |
| `locate_audio:71` | `>`→`<` (`off+len > buf.len`) | oversize-`data` guard | a valid WAV with **trailing slack** after the `data` payload (`off+len < buf.len`): orig `>` false → `Ok`; mutant `<` true → `Malformed`. (Exact-fit fixtures can't distinguish — they sit at equality) |
| `info_fourcc:119-124` | delete arms artist→`IART`, album→`IPRD`, date→`ICRD`, genre→`IGNR`, comment→`ICMT`, tracknumber→`ITRK` | tag-key → INFO FourCC | per key: `build_info_payload(&[TagInput{key,..}])` → assert the expected FourCC bytes appear in the payload. (title→`INAM` is already covered, not a survivor) |
| `build_info_payload:155` | `%`→`/`, `%`→`+`, `==`→`!=` (in `v.len() % 2 == 1`) | INFO value word-align | controlled `v.len()` (value bytes + NUL): a len-2 value (`"a"` → `[a,0]`) splits `%`vs`/` (orig no pad, `/`: `2/2==1` pads) and `==`vs`!=` (`!=` pads); an odd value (`"ab"` → len 3) splits `%`vs`+` (`+`: `3+2≠1` never pads). Assert exact payload incl. the pad byte |
| `push_inline_chunk:168` | `%`→`/`, `%`→`+` (in `payload.len() % 2 == 1`) | inline-chunk word-align | call directly with a len-2 payload (`%`vs`/`) and an odd-length payload (`%`vs`+`); assert the trailing pad byte present/absent in the emitted `Segment::Inline` |
| `synthesize_layout:186` | `>`→`==`, `>`→`>=` (in `audio_length > u32::MAX`) | RF64 size guard | **suspected equivalent (both)** — whenever `audio_length > u32::MAX`, `body_len ≥ audio_length` so `riff_size = body_len + 4 > u32::MAX` and the `:227` guard returns `TooLarge` anyway; for `audio_length ≤ u32::MAX` the `:186` guard doesn't fire. Both mutations yield the identical observable result. Confirm by hand-apply; document |
| `synthesize_layout:207` | `%`→`/`, `%`→`+` (in `tag_len % 2 == 1`) | embedded `id3 ` word-align | craft tags whose ID3v2 `tag_len` is **odd**; assert the following `data` FourCC lands at an **even** byte offset in the assembled output (orig pads → even) vs odd (mutant omits pad). `tag_len` parity is controllable via value length; compute it from `build_id3v2_segments` in the test |
| `synthesize_layout:227` | `>`→`==` killable; `>`→`>=` **equivalent** (in `riff_size > u32::MAX`) | RIFF size overflow | `==`: `BackingAudio` is virtual (no real 4 GB allocation), so `audio_length == u32::MAX` passes `:186` but makes `riff_size > u32::MAX`; orig `TooLarge`, mutant `==` proceeds → `Ok` with a truncated size. `>=`: see equivalents — every synthesized RIFF size is even, so the only distinguishing point (`riff_size == u32::MAX`, odd) is unreachable |
| `info_to_key:245-249` | delete arms `IPRD`→album, `ICRD`→date, `ICMT`→comment, `ITRK`→tracknumber | INFO FourCC → tag-key (inverse) | `read_tags` on a WAV carrying a `LIST/INFO` with each FourCC → assert the `(key, value)` pair returns. (`INAM`/`IART`/`IGNR` already covered) |
| `read_tags:300` | `&&`→`\|\|` (in `slice.len() >= 4 && &slice[0..4] == b"INFO"`) | INFO body validation | a `LIST` chunk whose payload is **<4 bytes**: orig short-circuits (`len >= 4` false → filter false → empty `from_info`); mutant `\|\|` evaluates `&slice[0..4]` on the short slice → **panic** (panic-vs-empty) |

### Excluded: `synthesize_layout:220` (the fourth `% 2 == 1` word-align)

`synthesize_layout:220` (`if audio_length % 2 == 1` — the `data`-chunk pad) is the
same `% 2 == 1` pattern as `:155`/`:168`/`:207` but is **not in the survivor
inventory**: the existing `pads_odd_data_payload_to_word_boundary` test
(`wav_synthesize.rs:138`) already exercises an odd-length `data` payload, so its
`%`→`/`/`%`→`+` mutants are caught. It is therefore out of 3d scope. (The three
*surviving* `% 2` sites differ only in that no existing test feeds them an
odd-length INFO value / inline payload / id3 `tag_len`.) Re-verify this assumption
against the next mutants campaign; if `:220` ever surfaces as a survivor it folds
into C1 with the same word-align strategy.

### Kill mechanism: panic-vs-`Err`/empty for the short-input guards

Some bound mutations kill by **panic divergence**, not a clean value comparison —
the test asserts the *clean* result and the mutation turns it into an out-of-bounds
panic. Note this in the test so the mechanism is readable:

- `riff_wave_start:24`, `< → ==` variant: an 11-byte `RIFF…` buffer. Orig
  short-circuits (`11 < 12` true → `Err(NotWav)`); the `==` mutant evaluates
  `11 == 12` → false → continues past the length check → `&buf[8..12]` panics. The
  test asserts `Err(NotWav)` on the 11-byte input; the kill is panic ≠ `Err`.
- `read_tags:300`, `&& → ||` variant: a `LIST` payload of <4 bytes. Orig
  short-circuits (`len >= 4` false → no INFO, empty result); the `||` mutant forces
  `&slice[0..4]` on the short slice → panic. The test asserts an empty/`id3`-only
  result; the kill is panic ≠ clean return.

## Equivalent mutants (confirm by hand-apply, then document — do not chase)

**Four** mutations are **suspected equivalent** (the third and fourth in
`synthesize_layout` were pinned during planning, not at spec time). Each is
confirmed by hand-apply (the targeted test stays green under the mutation) before
being recorded; if a hand-apply unexpectedly shows the mutant *is* killable, it
moves into C1 with a real test.

- **`walk_chunks:49`** — guard `next <= buf.len()` → `true`. The guard only changes
  behaviour when `checked_add` is `Some(next)` **and** `next > buf.len()`. There the
  original `break`s; the mutant sets `pos = next` (> `buf.len()`), and the loop
  condition `pos + 8 <= buf.len()` is then immediately false, so the loop exits with
  the **same** output `Vec` (the current chunk header was pushed *before* the advance
  in both). Overflow (`checked_add` → `None`) takes the `_ => break` arm in both.
  Observably identical → equivalent.
- **`synthesize_layout:186`** — `audio_length > u32::MAX`, both `> → ==` and
  `> → >=`. The `:227` guard (`riff_size > u32::MAX`) shadows `:186` entirely:
  `body_len` sums all segment lengths including the `BackingAudio` length
  (`audio_length`, the full `u64`), so `body_len ≥ audio_length`. Whenever `:186`
  would fire (`audio_length > u32::MAX`), `riff_size = body_len + 4 > u32::MAX` and
  `:227` returns the identical `Err(FormatError::TooLarge)`. For
  `audio_length ≤ u32::MAX`, `:186` never fires. So both mutations produce the same
  observable result as the original → equivalent. (The existing
  `rejects_audio_over_32bit` test cannot distinguish `:186` from `:227` for this
  reason — it passes `u32::MAX + 1`, which `:186` catches first.) **Optional
  follow-up, not done here:** the `:186` guard is strictly redundant given `:227`;
  removing it is a behaviour-neutral simplification deferred to avoid scope creep.
- **`synthesize_layout:227`**, `> → >=` only. `synthesize_layout` word-aligns every
  emitted chunk (RIFF header, `fmt `/`fact`, `LIST`, the `id3 ` chunk via `:207`,
  the `data` header, and the `data` payload via `:220`), so `body_len` is always
  even and `riff_size = body_len + 4` is always **even**. The only input that
  distinguishes `>` from `>=` is `riff_size == u32::MAX` (odd), which is therefore
  **unreachable** → the mutant is observably identical to the original. Its sibling
  `> → ==` is **not** equivalent and **is** killed (`riff_size > u32::MAX` is
  reachable — `==` then wrongly proceeds, the original rejects).

## Components

### C1 — in-module mutant kills (`wav.rs` `#[cfg(test)] mod tests`)

Expand the existing test module to cover the killable rows in the verified-findings
table:

- **Container/structure:** `riff_wave_start:24` (`<=` clean, `==` panic-vs-`Err`),
  `walk_chunks:47` (advance), `locate_audio:67` (data-only → `fmt ` detection),
  `locate_audio:71` (trailing-slack → oversize guard).
- **INFO mapping:** `info_fourcc:119-124` (6 arm-deletions), `info_to_key:245-249`
  (4 arm-deletions, exercised through `read_tags`), `read_tags:300`
  (<4-byte `LIST` → panic-vs-empty).
- **Word-align:** `build_info_payload:155` (`/`,`+`,`!=`), `push_inline_chunk:168`
  (`/`,`+`), `synthesize_layout:207` (id3 pad → even-`data`-offset assertion). The
  fourth `% 2` site, `synthesize_layout:220`, is **excluded** — already caught (see
  the exclusion note above).
- **Overflow:** `synthesize_layout:227` `==` only — `audio_length == u32::MAX`
  (virtual `BackingAudio`, no allocation) passes `:186` yet drives
  `riff_size > u32::MAX`; orig `TooLarge`, mutant `==` proceeds → `Ok`. (The `>=`
  sibling is equivalent — see equivalents.)

Record the suspected equivalents (`walk_chunks:49`, `synthesize_layout:186` ×2,
`synthesize_layout:227` `>=`) with their hand-apply justification (a comment in the
module and the inventory annotation).

### C2 — finding #16: zero-byte embedded art (WAV-local skip)

**Verified behaviour (not an assumption).** WAV's embedded art is emitted by
`crate::mp3::build_id3v2_segments`, called from `wav::synthesize_layout`. A
zero-byte picture becomes an APIC frame whose image bytes are a
`Segment::ArtImage { len: 0 }`; `RegionLayout::validate` rejects empty segments
(`EmptySegment`), so `synthesize_layout` returns `Err(InvalidLayout)` and the
**whole WAV track becomes unreadable** at serve time — the same gap 3a fixed for
FLAC. Ingestion (`scan.rs`) only filters art *above* `MAX_ART_BYTES`, so a source
file with an empty embedded picture is ingested and then bricks the track.

**Resolution (chosen): WAV-local filter.** In `wav::synthesize_layout`, filter
arts with `data_len == 0` **before** calling `build_id3v2_segments` (mirrors
`flac.rs:131/153`). This keeps the fix self-contained to 3d and matches the 3a
plan's "apply the same skip in each format's sub-phase." `mp3::build_id3v2_segments`
itself is left untouched — mp3-direct synthesis gets its own skip in 3b.
**Byte-identity is unaffected** — the filter only decides whether an empty APIC is
emitted, never the positioned `data` reads.

**Red-green ordering (this is a production behaviour change, not a mutant kill, so
it follows TDD, not the hand-apply method).** First add the `wav_synthesize.rs`
test asserting the desired behaviour — a zero-byte art → no `ArtImage` segment, the
layout is valid and round-trips, served audio byte-identical — and **run it against
unmodified `wav.rs` to watch it fail** (today it returns `Err(InvalidLayout)`, so
the assertion is red). Then apply the `data_len == 0` filter and rerun to green. Add
the mixed case (empty + real art) confirming the surviving APIC is preserved. (No
WAV art mutation survivor exists — this is robustness coverage, not a kill.)

**Cross-cutting status:** this resolves #16 for WAV. mp3/mp4 remain for 3b/3c (or a
single ingestion-level filter in `scan.rs`, the alternative the 3a doc recorded).

### C3 — finding #5: broaden `proptest_read_fidelity` (WAV)

`musefs-core/tests/proptest_read_fidelity.rs` currently has **four** properties on
FLAC only: the original baseline `read_at_preserves_backing_audio` (the whole-file
`[0, total_len)` read, finding #5's starting point) plus the three 3a-added
windowed ones (`read_at_partial_windows_match_whole`,
`read_at_windows_spanning_header_seam`, `read_at_art_window_serves_blob`). **Add WAV
variants of all four**, each asserting served bytes equal the corresponding slice of
the independently-assembled `whole` (`read_at(&resolved, &db, 0, total_len)`).

The whole-file `preserves_backing_audio` variant is technically subsumed by
`partial_windows` (which generates `offset=0, size=total_len`), but it is the
canonical statement of the byte-identity invariant for WAV and is cheap, so it is
included for parity with the FLAC set and as the clearest regression guard rather
than relying on a subsuming property.

**Fixture sub-task (non-trivial — its own task in the plan).**
`musefs-core/tests/common/mod.rs` has only `make_flac`/`write_flac`; there is no WAV
writer. Add `write_wav(path, tags, audio) -> (audio_offset, audio_length)` that
emits a minimal valid `RIFF`/`WAVE` (`fmt ` + `LIST/INFO` and/or `id3 ` + `data`)
and upserts the track + tags, following the existing `write_flac` shape. For the
art-window property, reuse the 3a `build_with_art` pattern (`upsert_art` +
`set_track_art`) against a WAV backing file. Keep `ProptestConfig::with_cases ≤ 64`
(each case does DB + file I/O), matching the existing properties.

### C4 — inventory + tracking docs

Annotate the `wav.rs` rows in `2026-05-29-mutation-inventory.md` (`killed (phase
3d)` / `equivalent`), and update `2026-05-29-remediation-tracking.md`: mark Phase 3d
(recording the suspected-equivalent set once confirmed, and that the WAV dimensions
of findings #5 and #16 are addressed). If 3d completes the last non-Ogg format,
update the Phase 3 status accordingly.

## Implementation ordering

C1 → C2 → C3 → C4. C2 and C3 are independent of C1 and of each other.

**Re-verify line numbers before each component.** Survivor references are pinned to
exact `wav.rs` line numbers from the phase-1 inventory; any intervening edit shifts
them. At the start of each component, locate the target by its **code pattern**
(named in the verified-findings table — e.g. "the `% 2 == 1` word-align in
`build_info_payload`", "the `riff_size > u32::MAX` guard") and confirm the current
line before applying the hand-apply mutation. The pattern is the source of truth;
the line number is a convenience that may drift.

## Error handling

No new error paths. The kills assert the existing `FormatError::{NotWav, Malformed,
TooLarge, InvalidLayout}` mappings on crafted inputs; the only production change is
C2's zero-byte-art filter, which *removes* a spurious `InvalidLayout` for a
degenerate input (the track becomes readable instead of bricked).

## Acceptance

| Component | Check |
|-----------|-------|
| C1 | `cargo test -p musefs-format --features fuzzing wav` green; the killable rows (`riff_wave_start:24`, `walk_chunks:47`, `locate_audio:67/:71`, `info_fourcc` 6 arms, `info_to_key` 4 arms, `read_tags:300`, `build_info_payload:155`, `push_inline_chunk:168`, `synthesize_layout:207`, `synthesize_layout:227` `==`) hand-apply red |
| C1 (equiv) | `walk_chunks:49`, `synthesize_layout:186` (×2), and `synthesize_layout:227` `>=` stay green under their mutation; recorded equivalent **both** in the inventory annotation **and** as a justification comment in the `wav.rs` test module (so a future reader sees why no test targets them) |
| C2 | C2 test written **first and shown red** against unmodified `wav.rs`, then green after the filter; `wav::synthesize_layout` skips zero-byte art; the `wav_synthesize.rs` test asserts no `ArtImage` segment, valid round-trip, byte-identical audio, and the mixed empty+real case preserves the real APIC; existing non-empty-art tests still pass |
| C3 | `cargo test -p musefs-core proptest_read_fidelity` green; **all four** WAV-variant properties pass with the new `write_wav` fixture |
| Whole | `cargo test --workspace` + `--features fuzzing` + `clippy --all-targets -D warnings` + `fmt --check` green; next full mutants campaign shows `wav.rs` survivors dropped (excluding the documented equivalents) |
