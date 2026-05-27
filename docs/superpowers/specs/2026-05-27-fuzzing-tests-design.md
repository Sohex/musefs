# Fuzzing tests — design

Date: 2026-05-27
Status: Approved (pre-implementation)

## Problem

musefs is a daemon that ingests arbitrary user music files and serves a
re-tagged view by splicing freshly-generated metadata in front of the untouched
backing audio. Two failure modes matter:

1. **Robustness.** A malformed, truncated, or adversarial file must never panic,
   abort, hang, or OOM a parser. A panic in a parse path can take down the mount.
2. **The cardinal invariant.** Synthesis must never alter, drop, duplicate, or
   reorder the original audio bytes. This holds today by construction, but
   nothing exercises it across weird-but-parseable inputs.

The `musefs-format` parsers (`flac`, `mp3`, `mp4`, `ogg`, `wav`) and primitives
(`ogg::page`, `ogg::b64`, `vorbiscomment`, `tagmap`) are pure functions over raw
byte slices returning `Result<_, FormatError>` — textbook fuzz targets. There is
currently no fuzzing or property-based testing in the workspace.

## Goals

- Panic-freedom on every parse/probe path across all five formats and the shared
  primitives.
- The byte-identical invariant asserted across arbitrary parseable inputs, at two
  depths (structural layout and real spliced read).
- Tag round-trip: tags serialized into a synthesized metadata region reparse to
  the same logical tags.
- CI that catches broken targets per-PR and runs coverage-guided fuzzing on a
  schedule.
- All project documentation updated.

## Non-goals

- Fuzzing the FUSE layer, the CLI, or the SQLite schema directly. The value is in
  the byte-surgery layer; the FUSE/CLI crates are deliberately thin.
- Differential fuzzing against reference decoders.
- Replacing the existing `#[ignore]` end-to-end mount suite.

## Decisions (locked during brainstorming)

- **Goal:** both panic-freedom *and* the byte-identical / roundtrip correctness
  property.
- **Framework:** roll both ourselves — `cargo-fuzz` (libFuzzer, coverage-guided)
  for crash-finding on the raw-byte parsers, and `proptest` for the invariants —
  rather than adopting `bolero`. The duplication `bolero` removes is captured
  instead by factoring the assertions into shared check helpers; `cargo-fuzz` and
  `proptest` are the ecosystem standards with lower coupling and bus-factor.
- **CI:** scheduled coverage-guided fuzzing + a per-PR build-and-smoke check.
  proptest rides along in the existing `cargo test --workspace`.
- **Scope:** all five formats plus the shared primitives in this first pass.
- **Byte-identity verification depth:** layered — a fast pure structural property
  (A) baked into every fuzz target and proptest, plus a small set of core-layer
  end-to-end read proptests (B) for splice fidelity.

## Architecture

### Crate layout

- A standalone `fuzz/` crate at the repo root, **excluded from the workspace**
  (the `cargo-fuzz` default: the generated `fuzz/Cargo.toml` carries its own empty
  `[workspace]` table so it detaches from the parent). It depends on
  `musefs-format` with the `fuzzing` feature enabled, plus `libfuzzer-sys` and
  `arbitrary`.
- `fuzz/fuzz_targets/` holds one target per format and one per primitive
  (see below).
- `fuzz/corpus/<target>/` holds the committed seed corpus.

### Shared check helpers

The pure assertions are the valuable, shared code. They live in `musefs-format`
in a module gated:

```rust
#[cfg(any(test, feature = "fuzzing"))]
pub mod fuzz_check;
```

This single gate exposes the helpers to:

- the crate's own proptests, via `cfg(test)`;
- the external `fuzz/` crate, via `feature = "fuzzing"`;
- `musefs-core`'s end-to-end proptests, via a dev-dependency
  `musefs-format = { path = "../musefs-format", features = ["fuzzing"] }`.

No new support crate is introduced. A non-default `fuzzing` feature is added to
`musefs-format`'s `[features]`.

### Input generation

Generation stays per-tool and thin: `proptest` strategies for proptest cases,
`arbitrary` (`Arbitrary` derive / `Unstructured`) for fuzz targets. Only the
*check functions* are shared — generators are not worth unifying.

### Dependencies

- `musefs-format`: add `proptest` as a dev-dependency; add the non-default
  `fuzzing` feature.
- `musefs-core`: add `proptest` as a dev-dependency, plus the
  `musefs-format` dev-dependency with `features = ["fuzzing"]` for the
  end-to-end check helper.
- `fuzz/` crate: `libfuzzer-sys`, `arbitrary`, `musefs-format` (features =
  `["fuzzing"]`). Format-specific seed generation reuses the existing dev-dep
  encoders (`metaflac`, `mp4`, `hound`).

## Fuzz targets

One comprehensive target per format, plus primitive targets, in
`fuzz/fuzz_targets/`:

- **Per format** — `flac`, `mp3`, `mp4`, `ogg`, `wav`. Each takes arbitrary
  `&[u8]`, runs the parse/probe path (panic-freedom). If parsing yields a scan,
  it then calls `synthesize_layout(scan, arb_tags, arb_arts)` with
  arbitrary-but-valid tag/art inputs and asserts:
  - the structural byte-identity property (Property A), and
  - the tag round-trip property (reparse the synthesized metadata region).
- **Primitives:**
  - `ogg_page` — `parse_page` + lacing on arbitrary bytes.
  - `b64` — `b64_window` / `encode_b64_slice` / `b64_len` window-equivalence.
  - `vorbiscomment` — parse an arbitrary comment block.
  - `tagmap` — the canonical-vocabulary mapping functions.

## Fuzzer runtime constraints

Media parsers (chunk-based MP4, frame-based MP3) are prone to pathological slow
paths on zero-length frames and large declared allocations; letting libFuzzer
generate multi-megabyte inputs causes false-positive timeouts and slow execution
without improving structural coverage. Inputs are therefore capped two ways:

- **Per-harness size guard (authoritative).** Each `fuzz_target!` early-returns
  on oversized input (`if data.len() > MAX_INPUT { return; }`), with `MAX_INPUT`
  in the 64–128 KiB range. This travels with the code and is enforced regardless
  of how the target is invoked. 64–128 KiB is ample to reach full structural
  coverage of these parsers.
- **CI invocation flags.** The fuzz CI jobs additionally pass libFuzzer runtime
  flags — `-max_len`, `-rss_limit_mb`, `-timeout` — on the command line (after
  `--`), so generation, memory, and hang limits are explicit in CI.

Note: `cargo-fuzz` has no `[target.<name>]` table in `fuzz/Cargo.toml` for
libFuzzer flags — targets are auto-discovered (or declared as `[[bin]]`) and
runtime flags are passed on the command line. The size cap is enforced in the
harness for that reason.

## Properties

1. **Panic-freedom.** No panic / abort / OOM / hang on any input, on every
   target. libFuzzer enforces OOM via its rss limit and hangs via `-timeout`.
2. **Structural byte-identity (Property A).** For any parseable file and any
   tags/arts, the resulting `RegionLayout` satisfies:
   - the `BackingAudio` / `OggAudio` segments cover exactly
     `[audio_offset, audio_offset + audio_length)`, contiguously, with no gap,
     overlap, or duplication;
   - `total_len() == header_len() + audio_length`;
   - for Ogg, the served audio byte length equals the backing audio length
     (renumbering patches page sequence numbers and CRCs in place).
   This is pure (operates on the parsed scan + the layout) and runs in both the
   fuzz targets and proptest.
3. **Tag round-trip (normalization fixed-point).** ID3v2 and MP4 metadata are
   historically messy — musefs normalizes on parse (e.g. version coercion,
   duplicate/deprecated frame handling), so the raw bytes of a fuzzer-generated
   "technically parseable" tag are not preserved verbatim, and asserting raw
   preservation would produce false failures. The property is therefore stated as
   idempotence of normalization: let `N0` be the normalized internal tag set from
   the first parse; serialize `N0` into a metadata region, reparse and normalize
   to `N1`; assert `N0 == N1`. The comparison is over the normalized internal
   representation, never the original raw tag bytes. Primarily a proptest with
   structured inputs; also asserted in the per-format fuzz targets where a parsed
   file already provides a comment/tag block.
4. **End-to-end read fidelity (Property B).** A `musefs-core` proptest builds a
   temporary SQLite DB and backing file, runs `reader::read_at` over the full
   virtual file, and asserts that the audio sub-range of the output bytes equals
   the original file's audio bytes and that the output length equals
   `layout.total_len()`. This exercises the real splice, including Ogg CRC
   recomputation and page renumbering. Bounded proptest only — too heavy per case
   to be a libFuzzer target.

## Seed corpus

A small, regenerable seed corpus under `fuzz/corpus/<target>/`. Generation is an
explicit binary in the fuzz crate — `[[bin]] name = "generate_seeds"`, run via
`cargo run --bin generate_seeds` — so it stays compiled and refactored alongside
the fuzz targets rather than bit-rotting as a loose script. It writes minimal
valid files for each format using the existing dev-dep encoders (`metaflac`,
`mp4`, `hound`) plus hand-built minimal Ogg/MP3 inputs. Seeds are kept tiny
(a few KB each) and committed so coverage-guided runs start warm and CI is
reproducible. Regeneration is documented.

## CI integration

- **Per-PR (existing `ci.yml`).** proptest cases are ordinary `#[test]`s, so they
  run under the existing `cargo test --workspace` with no toolchain change —
  only the new dev-dependencies. proptest case counts are left at sensible
  bounded defaults.
- **New `fuzz.yml`.**
  - *PR / push job:* `dtolnay/rust-toolchain@nightly` + `cargo-fuzz`,
    `cargo fuzz build` for all targets, then a few-second smoke run per target
    (`-runs=` or `-max_total_time=`) to catch broken targets quickly.
  - *Scheduled job:* a weekly cron mirroring `audit.yml`. Each target runs
    time-boxed (a few minutes each, with `-max_len`/`-rss_limit_mb`/`-timeout`
    passed on the command line) against the corpus. On any crash, the reproducing
    input is uploaded as an artifact and the job fails.
  - *Corpus accumulation across runs.* `actions/cache` is immutable per key — a
    static key restores but never re-saves an updated corpus. The corpus is
    therefore cached under a **dynamic key** (`fuzz-corpus-${{ github.run_id }}`)
    with `restore-keys: fuzz-corpus-` to restore the most recent prior corpus.
    `cargo fuzz cmin <target>` runs before save to minimize the corpus and stop
    the cache ballooning over weeks of scheduled runs.

## Findings and triage

A finding is a panic, abort, OOM, timeout/hang, or a failed structural /
round-trip assertion. libFuzzer writes the reproducing input to `crash-*` (the
scheduled CI job uploads it). proptest pins failing seeds in a committed
`proptest-regressions/` directory so regressions stay reproducible. Triage
commands (`cargo fuzz run <target> <crashfile>`, `cargo fuzz tmin`) are
documented in the `fuzz/` run instructions.

## Harness self-verification

Before a green target is trusted, it must be confirmed capable of failing.
During implementation, each property is validated by planting a known-bad case
(e.g. a layout with a corrupted audio offset or a dropped backing segment) and
confirming the structural assertion trips; likewise a planted byte alteration
must trip the end-to-end read property. This confirmation is part of the
implementation for each target/property, not a separate optional step.

Additionally, `cargo fuzz coverage <target>` (llvm-cov HTML report) is run
locally during development to confirm the fuzzer penetrates past early
magic-byte checks into the real parsing logic — a target that never gets past a
format-marker check is effectively dead despite running clean.

## Documentation updates

All project documentation is updated as part of this work:

- **`CLAUDE.md`** — add fuzz / proptest commands to the "Commands" section; add a
  line to the "Adding a format" checklist (add a fuzz target + a corpus seed for
  the new format); note the `fuzzing` feature on `musefs-format`.
- **`README.md`** — extend the "Development" section with how to run proptest
  (`cargo test`) and the fuzz targets (`cargo +nightly fuzz run <target>`),
  including the nightly + `cargo-fuzz` prerequisite.
- **`CHANGELOG.md`** — add an entry (under an "Unreleased" section) recording the
  fuzzing and property-test infrastructure.
- **`docs/ROADMAP.md`** — record the fuzzing/property-test hardening under the
  delivered work.
- **`fuzz/`** — minimal run/triage instructions (kept in `CLAUDE.md` and the
  README rather than introducing unrelated new top-level markdown; a short
  `fuzz/README.md` is acceptable as it is conventional for `cargo-fuzz` crates).

## Testing strategy summary

| Layer | Mechanism | What it checks | Runs |
|-------|-----------|----------------|------|
| `musefs-format` parsers | cargo-fuzz target per format | panic-freedom + Property A + tag round-trip | scheduled CI + per-PR smoke |
| `musefs-format` primitives | cargo-fuzz targets | panic-freedom + primitive invariants | scheduled CI + per-PR smoke |
| `musefs-format` | proptest | Property A + tag round-trip (structured inputs) | `cargo test` (per PR) |
| `musefs-core` | proptest | Property B (end-to-end read fidelity) | `cargo test` (per PR) |

## Risks and mitigations

- **Nightly toolchain drift** — `cargo-fuzz` needs nightly; pin via
  `dtolnay/rust-toolchain@nightly` and keep the fuzz job tolerant of nightly
  breakage (it is scheduled/PR-smoke, not a release gate).
- **Corpus bloat in git** — keep seeds minimal; rely on the Actions cache (not
  git) for accumulated corpus.
- **Slow proptest cases (Property B)** — bound case counts and keep fixtures
  minimal so per-PR `cargo test` stays fast.
- **False sense of safety from green targets** — mitigated by the harness
  self-verification requirement above.
