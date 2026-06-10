# Fuzz/bench harness hardening — design

Date: 2026-06-10
Issues: #216, #212, #213, #197

## Summary

Four independent hardening changes to the fuzz and bench harnesses, all within
one subsystem and with no cross-dependencies, grouped into one spec:

- **#216** — give fuzz-found crashes a durable regression home and a fast,
  deterministic replay that fails the build if a known reproducer panics again.
- **#212** — fuzz the bounded/streaming/ceiling probe variants (the bounds-heavy
  code production uses for large/windowed files), which currently get zero
  coverage, with a differential oracle against the full-buffer parse.
- **#213** — fuzz the read-time serve path (`read_segments_into` /
  `serve_ogg_window` / `OggArtSlice`), which no target reaches today, and add
  Ogg coverage to the read-fidelity proptest (currently FLAC/WAV only).
- **#197** — three small polish items: a labelled OOB assert in the b64 harness,
  a bench join reorder so a reader-thread panic can't be masked, and a real
  binary-tag MP3 seed.

The cardinal invariant is unchanged and is in fact what the new serve fuzzing
asserts: **original audio bytes are never copied or modified** — served audio
regions must be byte-identical to the untouched backing file.

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
`regressions/` are never pruned. Each target dir gets a `.gitkeep` so the
replay glob does not error on an empty directory before the first reproducer
lands.

### Deterministic replay

New `regressions` job (or step) in `fuzz.yml` that runs on every PR (not
time-boxed):

```
for t in flac mp3 mp4 ogg wav ogg_page b64 vorbiscomment serve; do
  cargo +nightly fuzz run "$t" fuzz/regressions/"$t"/* -- -runs=0
done
```

`-runs=0` replays each named input exactly once and exits non-zero if any input
panics — a fast, deterministic, named check that fails the build on a known
regression, independent of the time-boxed smoke window. The job must tolerate
an empty `regressions/<target>/` (only the `.gitkeep` present): guard the glob
so a target with no reproducers is a no-op rather than a "no such file" error.

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
drives its bounded twin and asserts a differential oracle:

| Target | Bounded entry point | Notes |
| ------ | ------------------- | ----- |
| `flac` | `flac::read_metadata_bounded(data)` | |
| `mp3`  | `mp3::locate_audio_bounded(prefix, file_len, tail)` | exercise the `tail: Option<&[u8;128]>` arm |
| `ogg`  | `ogg::read_metadata_bounded(data, file_len)` | |
| `wav`  | `wav::locate_audio_bounded(prefix, file_len)` and `wav::locate_audio_at_ceiling(prefix, file_len)` | |
| `mp4`  | `mp4::read_structure_from(&mut Cursor::new(data), file_len)` | seeking variant; reads headers, skips mdat |

`file_len` is `data.len() as u64` (the whole buffer is present), so the bounded
prober has the full file available.

**Differential oracle** — when the whole buffer is present:

- If the bounded prober returns `Extent::Complete(x)`, assert `x` equals the
  result of the corresponding full-buffer parse. A mismatch is a bug, not just
  a panic.
- If it returns `Extent::NeedMore { up_to }`, assert `up_to <= file_len` (it
  must never ask to widen past the file it was told the length of).
- `Err` on the bounded path when the full-buffer parse succeeded (or vice
  versa) is a divergence worth asserting where the two are contractually
  expected to agree; where they are *not* expected to agree (e.g. ceiling trusts
  a declared length the full parse validates against present bytes), only the
  panic-freedom and `up_to`/length-bound invariants are asserted.

The exact equality comparisons (which fields of `Mp3Bounds`/`FlacMeta`/etc.) are
nailed in the implementation plan against each type's definition.

---

## Workstream C — #213: Serve-path fuzzing + Ogg proptest

### New `serve` fuzz target

The fuzz crate gains a dependency on `musefs-core` (and `musefs-db`, plus
whatever test-fixture helper is needed to build a `Db` and a backing file). New
target `fuzz/fuzz_targets/serve.rs`, added to both the smoke loop and the
scheduled matrix in `fuzz.yml`, and a `[[bin]]` entry in `fuzz/Cargo.toml`.

Behavior:

1. From the adversarial input, build an in-memory `Db` and a temp backing file
   (covering at least the Ogg path so `serve_ogg_window`/`OggArtSlice` are
   reached; a small per-format selector driven by fuzzer entropy is acceptable).
2. Resolve a layout (`ResolvedFile`) for the track.
3. Draw arbitrary `(offset, size)` windows from fuzzer entropy and call
   `read_at`. Assert:
   - no panic on any range (including boundary/zero/oversized ranges);
   - **audio-byte identity** — bytes served from a `BackingAudio`/`OggAudio`
     region equal the corresponding backing bytes (the cardinal invariant);
   - concatenating sequential windows reconstructs the same bytes as a single
     full-length read (splice consistency).

Building a `Db` + temp file per input is heavier than a format-only target; the
`MAX_INPUT` cap and bounded art/tag sizes keep each iteration cheap, mirroring
the existing targets' cost discipline.

### Ogg read-fidelity proptest

`musefs-core/tests/proptest_read_fidelity.rs` currently builds only FLAC and
WAV backings. Add an Ogg backing builder (mirroring `build`/`build_wav`, using
the existing `common::write_*`/fixture helpers) and wire it into the existing
random-window `read_at` property so the Ogg serve path gets proptest range
coverage in the normal (pre-commit-gated) `cargo test` run, complementing the
adversarial fuzz target.

---

## Workstream D — #197: Three polish items

1. **b64 harness OOB assert** — `fuzz/fuzz_targets/b64.rs`: before slicing
   `&img[win.in_start..win.in_start + win.in_len]`, add
   `assert!(win.in_start + win.in_len <= img.len() as u64, "b64_window returned OOB range")`
   so a `b64_window` bug surfaces as a labelled failure rather than a generic
   slice-index panic. Cosmetic (the fuzzer already catches the bug), but makes
   the cause unambiguous.

2. **Bench join reorder** — `musefs-core/benches/read_throughput.rs:151-156`:
   after collecting `reader_results`, check them for panics (`for r in
   reader_results { r.unwrap() }`) **before** `walker.join().unwrap()`, so a
   panicking walker join can no longer hide an original reader-thread panic.
   Bench-only diagnostics; a cheap reorder.

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
- **CI** (`fuzz.yml`): new `regressions` `-runs=0` replay job; `serve` target
  added to the smoke loop and scheduled matrix; new `[[bin]]` in
  `fuzz/Cargo.toml`.
- **Docs**: `CONTRIBUTING.md` fuzzing section gains the regressions convention
  (reproducer bytes + behavioral test) and notes on the new bounded/serve
  coverage.

## Out of scope

- No change to the cardinal audio-byte-identity invariant or any synthesis path
  — these are test/harness-only changes (plus the small `fixtures` builder).
- No new format support, no scan/serve behavior changes.
- No rework of the existing smoke/scheduled fuzz cadence beyond adding the new
  target and the regressions replay.
