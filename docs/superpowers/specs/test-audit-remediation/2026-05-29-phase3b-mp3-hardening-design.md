# Phase 3b — MP3 Hardening (Design)

**Source audit:** `docs/audits/2026-05-29-test-audit.md` (the `mp3.rs` mutation
survivors from the phase-1 inventory)
**Created:** 2026-05-29
**Status:** design — awaiting plan

## Goal

Drive the **70 `mp3.rs` mutation survivors** toward zero with additive tests,
killing the killable ones and documenting the genuine equivalents.

**All changes are additive tests; no new dependencies.** No production logic change
is expected (these are coverage gaps). The one contingency: if a survivor in
`id3v2_alloc_safe` (the allocation-safety validator) turns out to mark a real
off-by-one in a bound, it gets a small scoped fix — flagged, not assumed (mirrors
3a's #16 handling). The byte-identity invariant is untouched either way: nothing
here touches the positioned audio reads.

This is the second slice of Phase 3 (Format-layer coverage & mutants, non-Ogg):
3a FLAC (done/in-flight), **3b MP3** (this doc), 3c MP4, 3d WAV. 3b carries no
cross-cutting findings (finding #5/#16 were resolved for FLAC in 3a; the non-FLAC
read-fidelity dimension of #5 is tracked separately and is **out of scope here**).

## Guiding principle: verify, don't trust

The 2026-05-29 audit was executed by a weaker model; its findings are **leads, not
facts.** Every survivor below was re-read against the actual `mp3.rs` source. Two
consequences already surfaced:

1. **Line numbers in the inventory have drifted.** They were captured at the CI sha
   `81d6d845d`; current `main` shifted them by ~10 lines (e.g. the inventory lists
   `id3v2_alloc_safe` at `:267`, but it is at `:257`). **Locate every target by its
   code construct, never by the raw line number.** Re-confirm before each kill.
2. **`| → ^` is not uniformly equivalent** (see the equivalence section): it depends
   on whether the operands are disjoint bitfields or whole bytes.

## The hand-apply verification method (use for every kill)

cargo-mutants is not available locally. For each targeted `function: construct: mutation`:

1. Run the new test → it passes (production code is correct).
2. Locate the construct by pattern, apply the exact mutation, rerun **just that
   test** → it must **fail** (a failed assertion *or* a panic both count).
3. Revert (`git checkout -- <file>`), rerun → passes again.

If step 2 still passes, strengthen the test, or — if the mutation provably yields
identical behavior — record it as an **equivalent mutant** instead of contriving a
test. Never leave a mutation applied.

## Test placement

`mp3.rs` **already has** a `#[cfg(test)] mod tests` (with `id3v2_guard_*`,
`read_tags_*`, `synthesize_round_trips_*`). 3b **extends that module** — it can call
the private survivor functions (`synchsafe_decode`, `syncsafe`, `is_id3_text_frame_id`,
`push_frame_header`, `id3v2_alloc_safe`) and the `pub(crate) build_id3v2_segments`
directly via `use super::*`, with byte-precise ID3v2 fixtures. The existing
integration files (`mp3_locate.rs`, `mp3_pictures.rs`, `mp3_read_tags.rs`,
`mp3_synthesize.rs`, `proptest_mp3.rs`) stay unchanged; all 3b kills live in the
in-module test module so the kill→test mapping is unambiguous.

## Verified findings

The 70 survivors cluster by function (line numbers approximate — locate by construct):

| Function | Constructs with survivors | Kill approach |
|----------|---------------------------|---------------|
| `synchsafe_decode` | 4-group shift-OR decode (`<<21/14/7`, the joining `\|`) | unit test: crafted high-bit bytes pin each `<<`; `\|→&` killable; the disjoint-shift `\|→^`/`\|→+` are **equivalent** |
| `syncsafe` (encode) | `>>21`, `>>14` group extraction | unit test: value with high bits set distinguishes `>>→<<`; decode∘encode round-trip pins both |
| `locate_audio` | ID3v2-skip (`&&` marker guard, `+=` footer, `tag_len` bound), frame-sync (`+ 1`, `\|\|` chain) | crafted buffers: short input, footer flag, sync-byte boundary |
| `push_frame_header` | `data_len > 0x0FFF_FFFF` TooLarge guard | boundary test at exactly `0x0FFF_FFFF` (kills `>→==`/`>=`) |
| `is_id3_text_frame_id` | whole-fn, `key != "TXXX"`, `is_upper \|\| is_digit` | `"TPE1"`→true, `"TXXX"`→false, digit case kills the `\|\|→&&` |
| `build_id3v2_segments` | `is_id3_text_frame_id` match guard; total-tag `> 0x0FFF_FFFF` guard | assert a `TPE1` tag emits a `TPE1` frame (not `TXXX`); total-tag boundary test |
| `id3v2_alloc_safe` | **49 survivors** — see its own section below | per-branch crafted ID3v2 headers |

### `id3v2_alloc_safe` — the dominant validator (49 survivors)

A `bool` guard that rejects ID3v2 tags whose declared sizes could OOM the `id3`
crate. Survivors span every branch; each needs a crafted fixture asserting the
`true`/`false` boundary:

- **header gate:** `data.len() < 10`, `data[0..3] == b"ID3"`, `matches!(major, 2..=4)`,
  extended/unsync flag reject (`flags & 0xC0 != 0`).
- **synchsafe body high-bit check:** `data[6] | data[7] | data[8] | data[9] >= 0x80`
  — **whole-byte OR**, so `|→&` and `|→^` are **killable** (two bytes sharing bit 7
  diverge; see equivalence section). The v2.4 per-frame `data[pos+4] | … | data[pos+7]
  >= 0x80` check is the same shape — also killable.
- **body length + `tag_end`:** `checked_add`, `tag_end > data.len()`.
- **per-version frame-size decode:** v2.2 24-bit `(d3<<16)|(d4<<8)|d5` (disjoint →
  `|→^` equivalent, `<<→>>` killable); v2.3 plain 32-bit `u32::from_be_bytes`; v2.4
  synchsafe.
- **`CHAP`/`CTOC` reject** (v2.3/2.4 only).
- **frame-flag rejects:** `data[pos+8] != 0 || data[pos+9] != 0`.
- **bounds + walk:** `data_start > tag_end || size > tag_end - data_start`, pos
  arithmetic (`pos = data_start + size`), loop guard `pos + header_len <= scan_end`,
  termination `pos >= tag_end`.

Killing these needs ~one fixture per branch (valid tag accepted; each malformed
variant rejected at exactly the guarded boundary). The existing `id3v2_guard_*`
tests are the starting point — strengthen them with boundary cases rather than
duplicate.

## Equivalent mutants (confirm green under the mutation, then record)

**Per-site `| →` analysis — the key verify-don't-trust result for MP3:**

- **Disjoint-bitfield ORs are equivalent under `| → ^` (and `| → +`) ONLY** — *not*
  under `| → &`. Operands occupy non-overlapping bit ranges, so `^`/`+` give an
  identical result, but `&` of disjoint ranges is `0`, which changes the value and
  **is killable**. Sites:
  - `synchsafe_decode`'s four 7-bit groups (the three joining `|`): `|→^`/`|→+`
    equivalent, `|→&` killable.
  - `id3v2_alloc_safe`'s v2.2 24-bit frame-size decode `(d3<<16)|(d4<<8)|d5` (the
    `:325`/`:326` sites): `|→^` equivalent, but **`|→&` is killable** —
    `(d3<<16) & (d4<<8) = 0` zeroes the decoded size and flips a frame-bounds
    accept/reject. Do **not** record these `|→&` as equivalent.
- **Whole-byte OR-chains are NOT equivalent** — `| → ^` and `| → &` are **killable**:
  - `id3v2_alloc_safe`'s synchsafe high-bit checks
    `data[6]|data[7]|data[8]|data[9] >= 0x80` and the v2.4
    `data[pos+4]|…|data[pos+7] >= 0x80`. With two bytes set to `0x80`: OR = `0x80`
    (reject), XOR = `0x00` (accept), AND = `0x00` (accept) — all three diverge, so a
    two-high-byte fixture kills both `^` and `&`.

The disjoint-shift `<< → >>` mutations are **killable** everywhere (a value with a
nonzero high group makes the shift direction observable). Each equivalence is
re-confirmed by hand-apply (test stays green under `^`/`+`) before being recorded.

## Components

### C1 — synchsafe codec (`synchsafe_decode`, `syncsafe`)

In-module unit tests: crafted high-bit byte arrays pin each `<<`/`>>`; a
`synchsafe_decode(syncsafe(n)) == n` round-trip for `n < 2^28` pins the group
boundaries; `|→&` killed by a value where AND-of-disjoint = 0. Record the
disjoint-shift `|→^`/`|→+` as equivalent.

### C2 — `locate_audio`

Crafted MP3 buffers: a `<10`-byte input (marker guard), a valid ID3v2 skip, the
v2.4 footer-flag path, an oversized `tag_len` (Malformed), and frame-sync boundary
bytes (`0xFF`, `& 0xE0`). Kills `&&`/`||`/`+=`/`+` mutations.

### C3 — frame helpers

`push_frame_header`: boundary test at `data_len == 0x0FFF_FFFF` (Ok) vs `+1`
(TooLarge). `is_id3_text_frame_id`: `"TPE1"`→true, `"TXXX"`→false, `"TPE1"` (with
digit) kills the `||→&&`, a non-`T`/non-4-char key→false. `build_id3v2_segments`:
assert a `TPE1` tag produces a `TPE1` frame (kills the match-guard); total-tag
boundary at `0x0FFF_FFFF`.

**Relation to `proptest_mp3.rs`:** the existing `mp3_synthesis_preserves_audio`
proptest drives random tag keys (`[A-Z]{1,12}`, **no digits**) through
`synthesize_layout` and asserts only the audio-coverage invariant — it neither
exercises a digit-bearing frame id (so it cannot reach the `||→&&` case) nor
asserts frame-id *classification* (TPE1-vs-TXXX). C3's targeted in-module kills are
therefore complementary, not redundant; the proptest does **not** need broadening
for 3b (it guards a different, byte-identity invariant). Leave it unchanged.

### C4 — `id3v2_alloc_safe` (the validator)

A suite of crafted ID3v2 headers, one assertion per branch boundary (see its
section above). This is the bulk of 3b. Strengthen the existing `id3v2_guard_*`
tests; add per-branch fixtures for the version gate, flag rejects, the **whole-byte
high-bit checks** (two-`0x80`-byte fixtures kill the `|→^`/`|→&`), per-version size
decode (v2.2 disjoint-shift, v2.3 32-bit, v2.4 synchsafe), `CHAP`/`CTOC` reject, and
the `data_start`/`size`/`pos`/`tag_end` bounds.

### C5 — inventory + tracking docs

Annotate the `mp3.rs` rows in
`docs/superpowers/specs/test-audit-remediation/2026-05-29-mutation-inventory.md`
(`missed → **killed** (phase 3b)` / `missed → **equivalent**`, matching the Phase 2
convention), and mark Phase 3b complete in
`docs/superpowers/specs/test-audit-remediation/2026-05-29-remediation-tracking.md`
(Status line + Phase 3 section). Record the equivalent set (the disjoint-shift
`|→^`/`|→+` in `synchsafe_decode` and the v2.2 24-bit decode) and any production fix
the contingency forced.

## Test budget (for chunking the plan)

Rough new-test counts so the plan can split work into bite-sized tasks:

- C1 synchsafe codec: ~4–6 tests.
- C2 `locate_audio`: ~5–6 tests.
- C3 frame helpers (`push_frame_header`, `is_id3_text_frame_id`, `build_id3v2_segments`): ~5–6 tests.
- C4 `id3v2_alloc_safe`: ~20–30 tests (≈ one per branch boundary; several branches
  share a base fixture mutated minimally). **The plan should split C4 into multiple
  tasks** grouped by region — header gate, high-bit checks, per-version size decode,
  `CHAP`/`CTOC`, bounds/walk — rather than one monolithic task.

Total ≈ 35–50 new tests.

## Implementation ordering

C1 → C2 → C3 → C4 (the large validator suite, itself split into per-region tasks)
→ C5. C1–C4 are independent; do C4 last because it is the biggest and reuses
fixture idioms from C1–C3.

## Error handling

No new error paths. Tests assert the existing `FormatError::{Malformed, NotMp3,
TooLarge}` mappings and the `bool` returns of `id3v2_alloc_safe`/`is_id3_text_frame_id`
on crafted inputs. If a bound survivor reveals a real off-by-one, the scoped fix
stays within `mp3.rs` framing/validation (never the audio reads).

## Acceptance

| Component | Check |
|-----------|-------|
| C1 | `cargo test -p musefs-format --features fuzzing mp3` green; `<<`/`>>`/`|→&` hand-apply red; disjoint `|→^`/`+` stay green (equivalent) |
| C2 | `locate_audio` `&&`/`||`/`+=`/`+` mutations hand-apply red |
| C3 | `push_frame_header` + total-tag boundary tests red under `>→==`/`>=`; `is_id3_text_frame_id` + match-guard kills red |
| C4 | per-branch `id3v2_alloc_safe` fixtures; whole-byte high-bit `|→^`/`|→&` hand-apply red; v2.2 disjoint-decode `|→^` recorded equivalent **but its `|→&` hand-apply red** (AND-of-disjoint = 0); all bound mutations red |
| Whole | `cargo test --workspace` + `--features fuzzing` + `clippy -D warnings` + `fmt --check` green; next full mutants campaign shows `mp3.rs` survivors dropped (excluding documented equivalents) |
