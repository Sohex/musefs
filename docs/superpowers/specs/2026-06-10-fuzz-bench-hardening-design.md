# Fuzz/bench harness hardening — design

Date: 2026-06-10
Issues: #216, #212, #213, #197

## Summary

Four largely independent hardening changes to the fuzz and bench harnesses, all
within one subsystem, grouped into one spec. The only sequencing dependency: the
#216 regressions replay job must derive its target list dynamically (or land
after #213) so it does not reference the new `serve` target before it exists —
see Workstream A.

- **#216** — give fuzz-found crashes a durable regression home and a fast,
  deterministic replay that fails the build if a known reproducer panics again.
- **#212** — fuzz the bounded/streaming/ceiling probe variants (the bounds-heavy
  code production uses for large/windowed files), which currently get zero
  coverage, with a differential oracle against the full-buffer parse.
- **#213** — fuzz the read-time serve path (`read_segments_into` /
  `serve_ogg_window` / `OggArtSlice`), which no target reaches today, and add
  Ogg coverage to the read-fidelity proptest (which already covers FLAC, WAV,
  MP3, and M4A — only Ogg is missing).
- **#197** — three small polish items: a labelled OOB assert in the b64 harness,
  a bench join reorder so a reader-thread panic can't be masked, and a real
  binary-tag MP3 seed.

The cardinal invariant is unchanged and is what the new serve fuzzing guards:
**original audio bytes are never copied or modified.** For passthrough formats
(FLAC/WAV/MP3/M4A) the served audio region is byte-identical to the untouched
backing. Ogg is the documented exception: `serve_ogg_window` rewrites page
*headers* (sequence renumbering + CRC) while leaving every packet *payload*
untouched — so the universal, format-agnostic serve oracle is **splice
consistency** (any window equals the same slice of a single whole read), with
backing byte-identity asserted only for the non-Ogg passthrough regions.

## Background

The fuzz crate (`fuzz/`) lives outside the workspace and currently depends only
on `musefs-format`. Per-format targets (`fuzz/fuzz_targets/{flac,mp3,mp4,ogg,
wav,ogg_page,b64,vorbiscomment}.rs`) call the **full-buffer** entry points and
assert only `assert_backing_covers_audio` on the constructed layout. The serve
path that splices `(offset, size)` ranges over that layout lives in
`musefs-core/src/reader.rs` and needs a `Db` plus a backing file, so it is not
reachable from a format-only fuzz target.

CI fuzzing (`.github/workflows/fuzz.yml`) is a per-PR `smoke` job
(`-max_total_time=15` per target) plus a weekly `scheduled` matrix. There is no
`fuzz/regressions/` (or `fuzz/artifacts/`) directory of committed reproducers
and no deterministic `-runs=0` replay pass.

---

## Workstream A — #216: Regression durability

### Reproducer home

New directory `fuzz/regressions/<target>/` holding committed crash/oom
reproducer **bytes**, one file per reproducer. This is separate from
`fuzz/corpus/<target>/`, which `cargo fuzz cmin` minimizes — reproducers under
`regressions/` are never pruned.

### Deterministic replay

New `regressions` job (or step) in `fuzz.yml` that runs on every PR (not
time-boxed). It iterates the **regression directories that actually exist**
rather than a hardcoded target list — so it self-adjusts when the `serve`
target (Workstream C) is added and never references a target before it exists:

```bash
shopt -s nullglob
for dir in fuzz/regressions/*/; do
  target=$(basename "$dir")
  files=("$dir"*)
  [ ${#files[@]} -eq 0 ] && continue   # dir present but no reproducers yet
  cargo +nightly fuzz run "$target" "${files[@]}" -- -runs=0
done
```

`-runs=0` replays each explicitly-named input exactly once and exits non-zero if
any input panics — a fast, deterministic, named check that fails the build on a
known regression, independent of the time-boxed smoke window. (`cargo fuzz run
<target> <path...>` forwards the positional paths to libFuzzer as corpus inputs;
`-runs=0` means "run the provided inputs, then stop" — no mutation.)

Empty-directory handling is explicit: `nullglob` makes `"$dir"*` expand to
nothing (not a literal `*`) when a directory holds no reproducers, and the
`continue` guard skips it. A directory with no entries at all is also skipped.
We do **not** rely on a `.gitkeep` to keep the glob safe (`*` would not match a
dotfile anyway) — the directory simply may not exist until its first reproducer
lands, and the `*/` loop handles that.

### Convention (documented in CONTRIBUTING.md)

When a fuzz bug is fixed:

1. Drop the reproducer bytes into `fuzz/regressions/<target>/` (guards against
   the fuzzer losing/rediscovering the input).
2. Where the crash exposed a real logic/behavior defect, add a **focused
   behavioral test** for that logic in the owning crate's suite (gated by the
   pre-commit hook's full-workspace test run).

These play different roles and are not interchangeable: the byte replay proves
the exact input no longer panics; the behavioral test documents and locks in
the fix at the level of logic. We do **not** add a generic "load every file in
`regressions/` and assert no panic" Rust test — arbitrary-byte replay is the
CI `-runs=0` job's job; in-tree tests are about behavior.

---

## Workstream B — #212: Bounded/ceiling probers + differential oracle

Extend the **existing** per-format targets (no new targets, no added smoke
time, shared corpus). Each target, after its current full-buffer work, also
drives its bounded twin with `file_len = data.len() as u64` (the whole buffer is
present, so the prober has the full file available).

### Relationship to the existing `probe_equivalence` test

`musefs-core/tests/probe_equivalence.rs` already asserts bounded-scan-vs-full
equivalence **at the DB-scan level**, for every format, via a forced-widen
window. The #212 work is deliberately at a **lower level** — the prober
functions themselves, driven by adversarial bytes rather than valid fixtures —
so it is complementary, not redundant. The plan must not duplicate or contradict
`probe_equivalence`; the new oracle lives in the fuzz targets only.

### Per-format oracle

Each bounded result is compared against a named full-buffer twin. The oracle
strength differs by format because the bounded probers do not all share a
contract with the full parse. **Prerequisite:** every compared result type
(`FlacMeta`, `Mp3Bounds`, `WavBounds`, `Mp4Scan`, `OggHeader`) must derive
`PartialEq` (confirm and add where missing — `Mp4Scan` already does).

| Target | Bounded entry point | Compared against (full) | Oracle when whole buffer present |
| ------ | ------------------- | ----------------------- | -------------------------------- |
| `flac` | `flac::read_metadata_bounded(data) -> Extent<FlacMeta>` | `flac::read_metadata(data) -> FlacMeta` | **strict on `Complete`**: `Complete(m)` ⇒ `read_metadata(data)` is `Ok(m)`. `NeedMore` **can** fire at full buffer when a block declares a body past EOF — then assert `read_metadata(data)` is `Err` (genuinely unparseable). flac's bounded twin takes **no** `file_len`, so its `up_to` is a prefix-length request that may exceed the file; the `up_to <= file_len` invariant below does **not** apply to flac. |
| `mp3`  | `mp3::locate_audio_bounded(data, file_len, tail) -> Extent<Mp3Bounds>` | `mp3::locate_audio(data)` | **strict on `(audio_offset, audio_length)`**, with `tail = (len>=128).then(\|\| &data[len-128..])` (matches production). The reject guard `audio_offset+2 > file_len` is equivalent to the full path's `audio_offset+1 >= len`, and the ID3v1-trailer strip reads the same last-128 bytes — so they must agree. The plan adds a unit test proving this equivalence before the fuzz assert relies on it; if it cannot be proven, mp3 drops to weak-invariants-only. |
| `ogg`  | `ogg::read_metadata_bounded(data, file_len) -> Extent<OggHeader>` | `ogg::read_metadata(data) -> OggHeader` | **strict**: `Complete` only when `read_header(data)` already succeeds, so it must equal `read_metadata(data)`; `NeedMore` cannot fire (prefix == file). Note: compare against `read_metadata`, **not** the target's existing `locate_audio` (`OggScan` ≠ `OggHeader`). |
| `wav`  | `wav::locate_audio_bounded(data, file_len) -> Extent<WavBounds>` | `wav::locate_audio(data)` | **strict**: with full buffer the bounded fn is literally `Complete(locate_audio(data))`, so equality is guaranteed. |
| `wav`  | `wav::locate_audio_at_ceiling(data, file_len) -> WavBounds` | (none) | **weak only**: ceiling trusts a declared `data` length validated against `file_len`, so it can return `Ok` where `locate_audio` returns `Err`. Assert panic-freedom and `audio_offset + audio_length <= file_len`; no equality. |
| `mp4`  | `mp4::read_structure_from(&mut Cursor::new(data), file_len) -> Mp4Scan` | `mp4::read_structure(data) -> Mp4Scan` | **strict**: both read headers and skip the mdat payload; assert equality. Cursor supplies the `Read + Seek` the seeking variant needs. |

Panic-freedom is asserted for every format unconditionally. The
`Extent::NeedMore { up_to }` ⇒ `up_to <= file_len` invariant is asserted only for
the probers that are **given** a `file_len` — mp3, ogg, wav (and for those, with
a full buffer the `NeedMore` arm cannot fire at all, since `prefix.len() ==
file_len`). It does **not** apply to flac (no `file_len` parameter; see its row)
or mp4 (returns a plain `Result`, no `Extent`).

---

## Workstream C — #213: Serve-path fuzzing + Ogg proptest

### New `serve` fuzz target

The fuzz crate gains a dependency on `musefs-core` and `musefs-db`. New target
`fuzz/fuzz_targets/serve.rs`, with a `[[bin]]` entry in `fuzz/Cargo.toml`.

**Placement: scheduled-only, not in the per-PR smoke loop.** A `serve` iteration
opens an in-memory SQLite `Db`, upserts a track + tags/art, and writes a temp
backing file before it can serve a single byte — intrinsically far lower
exec/s than a format-only target. In the 15s smoke window it would execute too
few cases to be meaningful while still adding build + run time to every PR. So:
`serve` is **built** by `cargo +nightly fuzz build` (smoke job) to catch
breakage, added to the **`scheduled` matrix**, but **excluded from the smoke
run loop**. (The smoke loop's target list is explicit, so omitting `serve` is a
one-line choice.)

**Input grammar.** A fully random backing almost never parses, so `resolve`
would fail and the serve path would rarely be reached. Instead the target starts
from a valid fixture and lets the corpus/mutator explore nearby malformed
variants — the same strategy the format targets use. The input bytes decode as:

1. A leading selector byte picks the backing format (bias toward Ogg so
   `serve_ogg_window`/`OggArtSlice` are exercised; cover FLAC/WAV/MP3/M4A too).
2. The target builds the chosen fixture's bytes (optionally mutated by remaining
   entropy), writes them to a temp backing file, and builds an in-memory `Db`
   with the track plus fuzzer-chosen tags/art (reusing `arb_tags`/`arb_arts`/
   `arb_binary_tags`, bounded as today) so the layout contains `ArtImage`/
   `BinaryTag`/`OggArtSlice` segments.
3. The remaining entropy yields a sequence of `(offset, size)` windows.

**Serving.** Open the backing file **once** per input and drive
`read_at_with_file` / `read_at_with_file_into` (`reader.rs:460,448`) for every
window — **not** `read_at`, which reopens the file on each call (`reader.rs:325`)
and would be N opens per input. Assert:
   - no panic on any range (including boundary/zero/oversized ranges);
   - the whole read has length `resolved.total_len`;
   - **splice consistency (universal oracle)** — each random window equals the
     same byte slice of the single whole read. This holds for every format,
     including Ogg, and is what catches a `serve_ogg_window` splice/page-patch
     defect or an `OggArtSlice` base64-windowing bug.

   Backing byte-identity (served audio region == backing bytes) is **not**
   asserted for OggAudio segments — `serve_ogg_window` legitimately rewrites
   page headers (seq + CRC), so only the packet payloads match. It already holds
   and is already proptested for the passthrough formats, so the serve fuzz
   target relies on splice consistency as its cross-format invariant rather than
   re-deriving per-format payload identity.

**Seeding.** `generate_seeds.rs` gains a `serve` entry writing one seed per
covered format: each seed is `[selector byte] ++ [a few window-spec bytes]` so a
freshly-checked-out corpus immediately reaches `resolve` + a real read for each
format, rather than starting from nothing.

### Ogg read-fidelity proptest

`musefs-core/tests/proptest_read_fidelity.rs` already covers FLAC (`build`), WAV
(`build_wav`), MP3 (`build_mp3`), and M4A (`build_m4a`) — **only Ogg is
missing**. Add a `build_ogg` backing builder mirroring those (registering
`Format::Opus` and using the existing `common::write_ogg` helper at
`common/mod.rs:223`) and a `build_ogg_with_art` variant mirroring `build_with_art`
(write_ogg backing + DB-sourced art via `upsert_art`/`set_track_art`, which Opus
synthesis emits as an `OggArtSlice` segment). Add an Ogg `proptest!` block with
the **splice-consistency** properties
— partial-windows-match-whole and windows-spanning-header-seam — over both
builders. These assert each random window equals the slice of a single whole
read; they deliberately do **not** assert served-audio == original, because the
Ogg serve path renumbers page headers (only payloads are preserved). This gives
the Ogg serve path (`serve_ogg_window` + `OggArtSlice`) random-window coverage in
the normal (pre-commit-gated) `cargo test` run, complementing the fuzz target.

---

## Workstream D — #197: Three polish items

1. **b64 harness OOB assert** — `fuzz/fuzz_targets/b64.rs`: before slicing
   `&img[win.in_start..win.in_start + win.in_len]`, add
   `assert!(win.in_start + win.in_len <= img.len() as u64, "b64_window returned OOB range")`
   so a `b64_window` bug surfaces as a labelled failure rather than a generic
   slice-index panic. Cosmetic (the fuzzer already catches the bug), but makes
   the cause unambiguous.

2. **Bench join reorder** — `musefs-core/benches/read_throughput.rs:151-157`:
   the live order is `collect reader_results` (152) → `stop.store(true)` (153) →
   `walker.join().unwrap()` (154) → `for r in reader_results { r.unwrap() }`
   (155-157). Because the walker join is unwrapped *before* the reader-result
   loop, a panicking walker join skips the reader unwraps and hides an original
   reader-thread panic — the defect is present, not already fixed. Move the
   `for r in reader_results { r.unwrap() }` loop **before** `walker.join()
   .unwrap()`. This preserves the existing comment's invariant ("stop is set
   before we re-raise"): `stop.store(true)` at 153 still precedes both unwraps,
   so re-raising a reader panic first cannot leave the walker spinning (it sees
   `stop` and exits). Bench-only diagnostics; a cheap reorder. Update the
   adjacent comment (148-150) to describe the new order.

3. **Real binary-tag MP3 seed** — add a `fixtures` builder in
   `musefs-format/src/fuzz_check.rs` that produces a well-formed MP3 carrying a
   binary ID3 frame (e.g. a GEOB/PRIV frame), and use it for the `mp3`
   `seed_binary` in `fuzz/src/bin/generate_seeds.rs` instead of the byte-identical
   `fixtures::mp3()`. The longer, binary-frame-carrying seed gives the binary-tag
   synthesis path immediate coverage rather than requiring the fuzzer to mutate
   its way there. Update the `seed_binary` comment to match. The new builder gets
   a small unit test asserting `mp3::locate_audio` accepts it (so the seed stays
   well-formed).

---

## Testing & gates

- **Behavioral additions** ride existing `cargo test`: the Ogg proptest builder
  and the new `fixtures` MP3 builder's unit test (both pre-commit gated).
- **`cargo +nightly fuzz build`** must stay green. The fuzz crate now depends on
  `musefs-core`, widening the surface where out-of-workspace breakage can occur,
  so this is checked explicitly (the fuzz crate is not built by the normal
  workspace build/clippy).
- **CI** (`fuzz.yml`): new `regressions` `-runs=0` replay job iterating
  `fuzz/regressions/*/` dynamically; `serve` target added to the scheduled matrix
  and built (not smoke-run) by the smoke job; new `[[bin]]` in `fuzz/Cargo.toml`.
- **Docs**: `CONTRIBUTING.md` fuzzing section gains the regressions convention
  (reproducer bytes + behavioral test) and notes on the new bounded/serve
  coverage.

## Out of scope

- No change to the cardinal audio-byte-identity invariant or any synthesis path
  — these are test/harness-only changes (plus the small `fixtures` builder).
- No new format support, no scan/serve behavior changes.
- No rework of the existing smoke/scheduled fuzz cadence beyond adding the new
  target and the regressions replay.
