# Phase 3c — MP4 Hardening (Design)

**Source audit:** `docs/audits/2026-05-29-test-audit.md` (the `mp4.rs` mutation
survivors from the phase-1 inventory)
**Created:** 2026-05-29
**Status:** design — awaiting plan

## Goal

Drive the **44 `mp4.rs` mutation survivors** (40 missed + 4 timeout) toward zero
with additive tests, killing the killable ones, documenting genuine equivalents,
and recording the 4 infinite-loop survivors as **timeout-detected**.

**All changes are additive tests; no new dependencies.** No production logic change
is expected (these are coverage gaps). The one contingency: if a survivor in a
bounds check (`patch_chunk_offsets`, `box_header`, the `u32::MAX` size guards)
turns out to mark a real off-by-one, it gets a small scoped fix — flagged, not
assumed (mirrors 3a's #16 handling and 3b's `id3v2_alloc_safe` contingency). The
byte-identity invariant is untouched either way: nothing here touches the
positioned `mdat`-payload reads.

This is the third slice of Phase 3 (Format-layer coverage & mutants, non-Ogg):
3a FLAC (merged, #47), 3b MP3 (in flight), **3c MP4** (this doc), 3d WAV. 3c
carries no cross-cutting findings — finding #5/#16 were resolved for FLAC in 3a;
the non-FLAC read-fidelity dimension of #5 is tracked separately and is **out of
scope here** (per the 3b decision).

## Guiding principle: verify, don't trust

The 2026-05-29 audit was executed by a weaker model; its findings are **leads, not
facts.** Every survivor below was re-read against the actual `mp4.rs` source on the
3c base (`main` @ `895b910`, with 3a merged). Two consequences surfaced:

1. **Line numbers in the inventory are approximate** (captured at CI sha
   `81d6d845d`). They happen to line up closely with current `main` for `mp4.rs`,
   but **locate every target by its code construct, never by the raw line
   number.** Re-confirm before each kill.
2. **MP4 has no manual shift-OR bit-decode sites.** `be_u32`/`be_u64` use
   `u32::from_be_bytes`/`u64::from_be_bytes`, so — unlike MP3 (3b) and FLAC (3a) —
   there is **no `|→^` disjoint-bitfield equivalence question.** The only `|`
   mutants are the bool `|=` dup-accumulators in `read_structure_from`, which are
   **killable** (see the equivalence section).

## The hand-apply verification method (use for every kill)

cargo-mutants is not available locally. For each targeted `function: construct: mutation`:

1. Run the new test → it passes (production code is correct).
2. Locate the construct by pattern, apply the exact mutation, rerun **just that
   test** → it must **fail** (a failed assertion *or* a panic both count).
3. Revert (`git checkout -- musefs-format/src/mp4.rs`), rerun → passes again.

If step 2 still passes, strengthen the test, or — if the mutation provably yields
identical behavior — record it as an **equivalent mutant** instead of contriving a
test. Never leave a mutation applied.

**Timeout survivors are the exception** (see the timeout section): they cannot be
hand-applied locally because the mutation makes a walk loop non-terminating (the
test would hang rather than fail). They are confirmed by reasoning + a covering
test and recorded as caught-by-timeout.

## Test placement

`mp4.rs` **already has** a `#[cfg(test)] mod tests` (31 tests, already
`use super::*`, with local fixture helpers `bx`, `mk_mp4`, `mk_mp4_co64`,
`soun_trak`, `mp4_with_ilst`, `data_atom`, `inline_head`, `first_stco`,
`first_co64`, plus direct use of the production `boxed`/`text_atom`/etc.). 3c
**extends that module** — it can call every private survivor fn (`box_header`,
`read_box`, `child_boxes`, `read_structure_from`, `read_freeform`, `read_tags`,
`read_pictures`, `build_udta`, `patch_chunk_offsets`, `synthesize_layout`)
directly with byte-precise fixtures. The existing integration files under
`musefs-format/tests/` stay unchanged; all 3c kills live in the in-module test
module so the kill→test mapping is unambiguous.

## Verified findings

The 44 survivors cluster by function (line numbers approximate — locate by
construct):

| Function | Constructs with survivors | Kill approach |
|----------|---------------------------|---------------|
| `BoxRef::end` | whole-fn (`→0`/`→1`), `start + total_len` (`+→*`) | **timeout** — stalls the `child_boxes` walk; covering walk test + record |
| `box_header` | `total_len < header_len` size bound (`<→<=`) | empty-payload box (`total_len == header_len`) must parse Ok |
| `read_box` | size-0 `(buf.len() - pos)` (`-→+`, `-→/`) | size-0 box at `pos > 0`: `total_len == buf.len() - pos` |
| `read_structure_from` | `remaining` arg `file_len - pos` (`-→+`); `moof` arm delete; dup `\|=` ×3 (`\|=→&=`); `pos += total` (`+=→*=`) | over-large box rejected; `moof` file rejected; duplicate ftyp/moov/mdat rejected; `pos +=` is **timeout** |
| `read_freeform` | `np.len() < 4 \|\| dp.len() < 8` (`<→==`/`<=`, `\|\|→&&`); mean `p.len() >= 4` (`>=→<`) | exact-length name/data fixtures; one-side-true `\|\|` fixture |
| `read_tags` | `dp.len() < 8` (`<→==`/`<=`); `trkn`/`disk` branch (`&&→\|\|`, `==→!=`, `>=→<`) | exact 8-byte data; `trkn`/`disk` with value len exactly 4 and <4 |
| `read_pictures` | `dp.len() < 8` (`<→==`/`<=`); `data`-type arm `14` delete | exact 8-byte data; **PNG** cover (type 14) recognized as `image/png` |
| `build_udta` | png `a.mime == "image/png"` (`==→!=`); covr/data size `+→-/*`; `udta_size > u32::MAX` (`>→>=`) | png→type 14 / jpeg→type 13; exact emitted box sizes; `u32::MAX` boundary via `art_len` |
| `patch_chunk_offsets` | bounds `pos + entry > start + len` (`+→-/*`); stco `v < 0 \|\| v > u32::MAX` (`<→==`/`<=`, `\|\|→&&`, `>→==`/`>=`); co64 `v < 0` (`<→==`/`<=`) | delta driving `v` to exactly `0` / `u32::MAX` and one past each |
| `synthesize_layout` | `new_moov_size > u32::MAX` (`>→==`/`>=`) | `u32::MAX` boundary via `art_len` |

### The `u32::MAX` size guards are cheaply testable

`build_udta:566` (`udta_size > u32::MAX`) and `synthesize_layout:638`
(`new_moov_size > u32::MAX`) look untestable (a 4 GiB tag region), but **are not**:
`build_udta` *reserves* the art region from `art.data_len` (a `u64`) without ever
holding the image bytes (see the existing `build_udta_with_art_reserves_size_without_image`
test). So an `ArtInput` with a large `data_len` drives `udta_size`/`new_moov_size`
to exactly `u32::MAX` (must be Ok) and one byte past (must be `TooLarge`) without
allocating anything. That boundary pair kills both `>→==` and `>→>=`.

## Equivalent mutants

**MP4 has essentially none.** Because every multi-byte field is decoded with
`from_be_bytes` (no hand-rolled `(a<<16)|(b<<8)|c`), the disjoint-bitfield `|→^`
equivalence that dominated 3a/3b **does not arise**. The only `|` mutants in the
inventory are the three bool dup-accumulators in `read_structure_from`:

```rust
dup |= ftyp.replace((pos, bh)).is_some(),   // and the moov / mdat lines
```

`|=→&=` here is **killable**, not equivalent: `dup` starts `false`, and `&=` can
never set it `true`, so a file with a duplicate box (where the correct `|=`
accumulates `true` and rejects with `NotMp4`) is wrongly accepted under `&=`. One
duplicate-ftyp / duplicate-moov / duplicate-mdat fixture kills each line. **No
mutant is recorded as equivalent** unless a kill attempt proves otherwise during
implementation (recorded then, with the hand-apply evidence).

## Timeout survivors → timeout-detected

Four survivors are **infinite-loop mutations**: they make a box-walk stop
advancing `pos`, so the loop never terminates and cargo-mutants times out rather
than classifying caught/missed. Per the Phase 2 convention they are recorded as
**timeout-detected** (cargo-mutants' own per-mutant timeout kills a non-terminating
mutant in CI). No production change; verified by reasoning + a confirmed covering
test, **not** by running the hang locally.

- `BoxRef::end` `→0`, `→1`, `+→*`: `child_boxes` advances with `pos = b.end()`. A
  zero/constant/`start*total_len` end pins `pos` at `0` (or fails to advance), so
  any walk over a ≥2-box buffer hangs. **Covering tests already exist**
  (`walks_top_level_boxes`, `find_box_and_nested_path`, the `locate`/`read_tags`
  paths). Confirm one walks ≥2 boxes; record.
- `read_structure_from:285` `pos += total` → `pos *= total`: `pos` starts `0`, so
  `0 *= total` stays `0` forever. **Covered** by `read_structure_from_matches_buffer_path`
  / `_never_reads_mdat_payload` / `_handles_largesize_mdat`. Confirm one walks past
  the first box; record.

The plan's verification step for these is "confirm a covering test exercises the
multi-box walk path" — never an apply-and-rerun (which would hang the suite).

## Components

### C1 — box primitives (`box_header`, `read_box`, `BoxRef::end`)

In-module unit tests: `box_header` with an empty-payload box (`size == header_len`,
e.g. an 8-byte box) must return Ok (kills `<→<=`); a size-0 box parsed at a nonzero
`pos` asserts `total_len == buf.len() - pos` (kills `read_box`'s `-→+`/`-→/`).
Confirm an existing multi-box walk test covers the `BoxRef::end` timeouts; record
the three as timeout-detected.

### C2 — `read_structure_from` structural walk

Cursor-fed fixtures (reuse the `read_structure_from_*` idiom): a file whose second
top-level box declares a size larger than the bytes remaining must error (kills the
`remaining` `-→+`); a file carrying a `moof` top-level box must be rejected via the
seeking path (kills the `moof`-arm delete); files with a duplicated `ftyp` / `moov`
/ `mdat` must each be rejected (kills the three `|=→&=`). Confirm a covering walk
test for the `pos += total` timeout; record.

### C3 — metadata read (`read_freeform`, `read_tags`, `read_pictures`)

Byte-precise atom fixtures: name/data payloads at exactly the guarded lengths
(`np.len() == 4` vs `3`; `dp.len() == 8` vs `7`) pin the `<` boundaries; a fixture
where one side of `np.len() < 4 || dp.len() < 8` is true and the other false kills
`||→&&`; a `mean` payload of exactly 4 bytes pins `>=→<`. For `read_tags`: a
`trkn`/`disk` atom with value length exactly 4 (parsed) vs <4 (skipped) kills the
`&&→||`/`>=→<`/`==→!=` on those branches; an 8-byte `data` boundary pins the length
guard. For `read_pictures`: a **PNG** cover atom (`data` type `14`) must yield
`image/png` (kills the arm-`14` delete), a JPEG (`13`) yields `image/jpeg`, and an
8-byte `data` boundary pins the length guard.

### C4 — synthesis (`build_udta`, `patch_chunk_offsets`, `synthesize_layout`)

`build_udta`: a PNG `ArtInput` emits a `covr/data` atom with type code `14`, a JPEG
with `13` (kills `==→!=`); assert the exact emitted `covr_size`/`data_size` (kills
the `+→-/*` size arithmetic); an `art_len` driving `udta_size` to exactly
`u32::MAX` is Ok, one past is `TooLarge` (kills `>→>=`/`>→==`). `patch_chunk_offsets`:
craft a `stbl` with `stco`/`co64` entries and a `delta` that drives a patched offset
to exactly `0` and exactly `u32::MAX` (Ok) versus `-1` and `u32::MAX + 1`
(`Malformed`/`TooLarge`) — kills the `v < 0 || v > u32::MAX` mutants and the co64
`v < 0`; an entry count one past the table bound kills `pos + entry > start + len`.
`synthesize_layout`: the `art_len` `u32::MAX` boundary kills `new_moov_size`'s
`>→>=`/`>→==`. Reuse `mk_mp4`/`mk_mp4_co64`/`first_stco`/`first_co64`.

### C5 — inventory + tracking docs

Annotate the `mp4.rs` rows in
`docs/superpowers/specs/test-audit-remediation/2026-05-29-mutation-inventory.md`
(`missed → **killed** (phase 3c)` / `timeout → **timeout-detected**`, matching the
Phase 2 convention), and mark Phase 3c complete in
`docs/superpowers/specs/test-audit-remediation/2026-05-29-remediation-tracking.md`
(Status line + Phase 3 section). Record the empty equivalent set (note the
`|=→&=` are killable, not equivalent) and any production fix the contingency
forced.

## Test budget (for chunking the plan)

Rough new/strengthened-test counts so the plan can split into bite-sized tasks:

- C1 box primitives: ~3–4 tests (+ confirm/record 3 timeouts).
- C2 `read_structure_from`: ~4–5 tests (over-large box, `moof`, dup ×3; + confirm/record 1 timeout).
- C3 metadata read: ~10–12 tests (the bulk of the `<`/`||`/`&&`/arm survivors).
- C4 synthesis: ~10–12 tests (png type, size arithmetic, the two `u32::MAX` guards, stco/co64 bounds).

Total ≈ 30–40 new/strengthened tests, plus 4 timeout records.

## Implementation ordering

C1 → C2 → C3 → C4 → C5. C1–C4 are independent; do C4 last because it is the
largest and reuses fixture idioms from C1–C3.

## Error handling

No new error paths. Tests assert the existing `FormatError::{Malformed, NotMp4,
TooLarge, InvalidLayout}` mappings and the leniency contracts of `read_tags` /
`read_pictures` (skip-and-continue, never error) on crafted inputs. If a bound
survivor reveals a real off-by-one, the scoped fix stays within `mp4.rs` framing /
validation (never the positioned `mdat` reads).

## Acceptance

| Component | Check |
|-----------|-------|
| C1 | `box_header` empty-payload boundary red under `<→<=`; `read_box` size-0 red under `-→+`/`-→/`; 3 `BoxRef::end` timeouts have a confirmed multi-box covering test and are recorded |
| C2 | over-large-box, `moof`-reject, and dup-ftyp/moov/mdat tests red under the `remaining` `-→+`, the `moof`-arm delete, and the three `\|=→&=`; `pos += total` timeout covered + recorded |
| C3 | `read_freeform`/`read_tags`/`read_pictures` length-guard, `\|\|→&&`/`&&→\|\|`, trkn/disk, and PNG-arm mutations hand-apply red |
| C4 | `build_udta` png-type + size-arithmetic + `u32::MAX`-guard red; `patch_chunk_offsets` overflow/underflow/bounds red; `synthesize_layout` `u32::MAX`-guard red |
| Whole | `cargo test --workspace` + `--features fuzzing` + `clippy -D warnings` + `fmt --check` green; inventory/tracking docs updated; next full mutants campaign shows `mp4.rs` survivors dropped (excluding the documented timeouts) |
