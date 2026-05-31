# SP0a extension — per-format bench coverage

*Part of the 2026-05-30 optimization pass. Extends the shipped SP0a harness
(`SP0-measurement-foundation.md`, plan `../../plans/2026-05-30-optimization-sp0a-corpus-and-benches.md`).*

## Problem

The SP0a corpus generator and benches are **FLAC-only**: every tier preset uses
`format_mix: vec![Format::Flac]`, and although the generator already supports
MP3 / M4A (moov-first & moov-last) / WAV writers, the benches never exercise
them. Ogg has no corpus builder at all. So scan/ingest and read numbers reflect
only FLAC — yet probing, tag extraction, audio-offset finding, and metadata
synthesis differ materially per format (MP3 regenerates ID3v2 wholesale; M4A
rebuilds `moov` and patches `stco`/`co64`; Ogg renumbers pages and recomputes
CRCs). SP1–SP4 need per-format baselines to avoid optimizing one format while
regressing another.

## Goal

Run the ingest and read benches against **every supported format**, reporting
one row / Criterion line per format, so per-format cost is directly comparable.
Add the missing Ogg corpus builder. No production code changes.

## Cardinal invariant

Unchanged from the umbrella doc: original audio bytes are never copied or
modified, served audio stays byte-identical. This is a test/bench-only change
(everything under `tests/` and `benches/` plus this doc); all existing crate
tests and the `#[ignore]`d FUSE e2e tests stay green.

## Format set

The sweep covers all supported formats plus the one layout variant that matters
for SP1:

| Token (`MUSEFS_BENCH_FORMAT_MIX`) | `Format` variant | Builder |
|---|---|---|
| `flac` | `Flac` | existing `flac_bytes` (corpus) |
| `mp3` | `Mp3` | existing `write_mp3` |
| `m4a` | `M4aMoovFirst` | existing `write_m4a` |
| `m4a-last` | `M4aMoovLast` | existing `write_m4a_moov_last` (SP1 bounded-read hard case) |
| `ogg` | `Ogg` | **new** `write_ogg` |
| `wav` | `Wav` | existing `write_wav` |

Centralized as a single `corpus::ALL_FORMATS: &[Format]` constant so benches and
any future format stay in sync.

## Components

### 1. `write_ogg` corpus builder (`musefs-core/tests/common/mod.rs`)

```
pub fn write_ogg(path: &Path, audio: &[u8]) -> (i64, i64)
```

Builds a minimal valid **Ogg Opus** file by reusing the already-wired
`musefs_format::ogg::page_test_support` helpers (`musefs-core`'s
`[dev-dependencies]` already enable `musefs-format`'s `fuzzing` feature plus the
`ogg`/`crc` crates; `tests/interop_emit.rs` already uses `fuzz_check::fixtures`):

- `build_header_pub(serial, &[OpusHead, OpusTags])` → the two header pages.
- `lace_packet_pub(serial, 2, false, 960, audio)` → one audio page whose packet
  body is the corpus's `filler` bytes. The synthesizer treats the packet body as
  opaque (it renumbers pages / recomputes CRC, never decodes), so arbitrary
  filler bytes are a valid payload.

Returns `(audio_offset, audio_length)` from the header length / page span. The
return is informational only — `generate_one` discards it and `scan_directory`
re-probes the file (consistent with the other `write_*` helpers).

Add `Format::Ogg` to the corpus `Format` enum, the `"ogg"` token to
`from_env`'s `MUSEFS_BENCH_FORMAT_MIX` parser, and the `Format::Ogg => write_ogg`
arm in `generate_one`. (`scan.rs` already probes `.ogg`.)

### 2. `format` column in `RunReport` (`musefs-core/tests/common/report.rs`)

Add a `pub format: String` field. Extend the `report_fmt!` macro's column layout
and the `header()` label row with a `format` column (placed after `label`). Both
header and row expand the one macro, so they cannot drift.

### 3. Format-set selection (`musefs-core/tests/common/corpus.rs`)

```
pub const ALL_FORMATS: &[Format] = &[Flac, Mp3, M4aMoovFirst, M4aMoovLast, Ogg, Wav];

pub fn bench_formats() -> Vec<Format>
//   = parse(MUSEFS_BENCH_FORMAT_MIX) when set   (acts as a sweep filter, e.g. "ogg,wav")
//   = ALL_FORMATS.to_vec()           when unset (full coverage by default)
```

`bench_formats` reads the env var directly (not via `CorpusParams.format_mix`,
whose tier default of FLAC is indistinguishable from an explicit `flac`). Reuses
the same token→`Format` mapping as `from_env`.

### 4. `bench_ingest` per-format sweep (`musefs-core/tests/bench_ingest.rs`)

**Generated mode** (no `MUSEFS_BENCH_LIBRARY`): resolve a base dir once
(`MUSEFS_BENCH_DIR` or a tempdir, exactly as `prepare` does today) and loop over
`bench_formats()`. For each format, clone `params`, set `format_mix = vec![fmt]`,
generate that single-format corpus into a **per-format subdir** of the base dir
(e.g. `<base>/<format>/`) with its own cold DB (delete `musefs-bench.db` +
`-wal`/`-shm` first, mirroring `prepare`), time `scan` then `revalidate`, and emit
a `scan` + `revalidate` row tagged with the format. `assert!(scanned > 0)` per
format. Existing metrics/RSS/wall semantics per row.

This is the per-format generalization of SP0a's `prepare`; factor the shared
base-dir + cold-DB logic into a helper (e.g. `prepare_format(params, fmt)`)
rather than duplicating it.

**Real-library mode** (`MUSEFS_BENCH_LIBRARY` set): a real library is already
mixed-format and cannot be regenerated per format, so the sweep collapses to a
**single** scan + revalidate of the real directory, emitting one row pair tagged
`format = "mixed"`. No per-format generation occurs.

### 5. `read_throughput` per-format groups (`musefs-core/benches/read_throughput.rs`)

`fixture(format, bytes_per_track, tracks)` takes a `Format`. The sequential read
bench emits one `bench_function` per format in `bench_formats()` (Criterion
reports a per-format throughput line). The concurrent read+walk variant stays a
single FLAC-only group — its purpose is mutex-contention scaling (SP3), not
per-format cost. Throughput annotations as in SP0a.

### 6. `bench_refresh` stays FLAC-only (`musefs-core/tests/bench_refresh.rs`)

`poll_refresh` times a DB-driven virtual-tree rebuild; the backing audio format
is irrelevant to it. Per-format rows would be pure noise. Unchanged except a
one-line comment stating why it does not sweep formats.

### 7. Tests (TDD, `musefs-core/tests/common_corpus_smoke.rs`)

- `write_ogg` output scans as exactly 1 track (`scanned == 1`).
- `generate` with `format_mix == ALL_FORMATS` scans all tracks and is
  deterministic (byte-identical first file across two runs, per the existing
  determinism-test pattern).
- `bench_formats()` returns `ALL_FORMATS` when `MUSEFS_BENCH_FORMAT_MIX` is unset
  and the parsed subset when set (holds `ENV_LOCK`).

The benches remain `#[ignore]`d; their per-format output is verified by running
them manually (as in SP0a), not asserted in the default suite.

## Non-goals (explicit)

- **Ogg cover art.** The `write_ogg` builder embeds no `METADATA_BLOCK_PICTURE`
  art. The Ogg art path (`OggArtSlice` synthesis) is exercised elsewhere; the
  per-format scan/read baseline does not need it. Deferred.
- **FLAC-in-Ogg (OggFLAC).** The builder emits **Opus** only (`OpusHead`/
  `OpusTags`), not a FLAC codec inside the Ogg container. One Opus stream is
  enough to measure the Ogg container's page-renumber + CRC synthesis cost;
  adding an OggFLAC variant is deferred unless a later SP needs codec-specific
  numbers.
- No new Cargo features and no production-code changes; reuses the existing
  `fuzzing` dev-dependency wiring.

## Acceptance criteria

1. `cargo test --workspace` and `cargo clippy --all-targets -- -D warnings` stay
   green; the default suite is unchanged except the new `common_corpus_smoke`
   cases.
2. `cargo test -p musefs-core --features metrics --test bench_ingest -- --ignored --nocapture`
   prints a table with a `scan`+`revalidate` row for each of the six formats,
   each with `scanned > 0`.
3. `cargo bench -p musefs-core --bench read_throughput` reports a sequential
   throughput line per format without panicking.
4. `MUSEFS_BENCH_FORMAT_MIX=ogg,wav` restricts both sweeps to those two formats.
5. The corpus generator produces a valid, scannable Ogg file, and generation is
   deterministic for a fixed `(params, seed)`.
