# Serve-path metrics: Ogg instrumentation + counter test coverage

**Date:** 2026-06-03
**Issues:** #71 (Ogg serve path records no pread/byte metrics), #76 (serve-path
metric counters covered only by direct-call tests)
**Status:** approved

## Problem

Two gaps in the optional `metrics` feature (`musefs-core/src/metrics.rs`):

1. **Ogg is blind (#71).** `serve_ogg_window` and the lazy page-index scan
   (`find_page_start`) in `musefs-core/src/ogg_index.rs` perform positioned
   backing reads at four sites without calling `metrics::on_pread`, so Ogg
   reads report zero `preads`/`pread_bytes` in `metrics::snapshot()`. The
   latency read benchmark (`bench_read_under_latency` in
   `musefs-core/tests/bench_ingest.rs`) consequently reports 0 round-trips for
   Ogg — and, because per-read latency injection (`MUSEFS_FAULT_PREAD_US`)
   lives inside `on_pread`, Ogg reads are also exempt from injected latency,
   so the bench cannot measure them at all.

2. **Counters are unverified at their call sites (#76).** `on_art_chunk` and
   `on_binary_tag_chunk` are exercised only by unit tests that call the
   increment functions directly. No test serves the corresponding segment
   (`Segment::ArtImage`, `Segment::BinaryTag` in `reader.rs`) and asserts the
   counter moved. This already bit once: the `Segment::BinaryTag` serve arm
   shipped without its `on_binary_tag_chunk()` call and the suite stayed
   green. Worse, no CI step runs with `--features metrics`, so the entire
   existing metrics test surface (`musefs-core/tests/metrics.rs`) is compiled
   out of `cargo test --workspace` and invisible to CI.

## Goals

- Ogg serve-path backing I/O is counted (and latency-injectable) like every
  other serve arm.
- Every serve-path counter has a test that drives it through a real
  `Musefs::read` of the corresponding segment.
- CI compiles and runs the metrics-feature tests on every PR.
- The misleading zero-round-trip Ogg rows in `BENCHMARKS.md` are refreshed.

## Non-goals

- No new `Snapshot` fields, no schema/CLI/public-API changes.
- `metrics` stays a non-default feature; zero-cost-when-off is preserved.
- The in-diff mutation gate is not extended to the metrics feature
  (cargo-mutants does not generate delete-this-call-statement mutants, so the
  integration tests — not mutation testing — are the protection for dropped
  increments).

## Design

### 1. Ogg instrumentation (#71)

Add a private helper to `musefs-core/src/ogg_index.rs`:

```rust
/// Positioned read that records serve-path pread metrics (count + bytes).
/// Counts on the attempt, like `on_open` — a failed read is still a round-trip.
fn read_counted(f: &File, buf: &mut [u8], offset: u64) -> io::Result<()> {
    crate::metrics::on_pread(buf.len() as u64);
    f.read_exact_at(buf, offset)
}
```

All four backing-read sites switch to it:

- the `find_page_start` backward-scan window read,
- `find_page_start`'s CRC-check page read (its `is_err()` use is unchanged —
  the attempt is counted regardless of outcome),
- the per-page header read in `serve_ogg_window`,
- the payload read in `serve_ogg_window`.

**Counting semantics (decided):** `preads`/`pread_bytes` mean *actual backing
I/O performed*. For Ogg this includes index-scan and header reads whose bytes
are patched or discarded before serving, so `pread_bytes` ≠ bytes-served on
the Ogg path. This is a deliberate asymmetry with `BackingAudio` (where the
two coincide): the metric's purpose is counting backing round-trips and bytes
read — what latency benchmarking needs — not output accounting. The module
doc in `metrics.rs` (which currently documents the Ogg blind spot as a known
limitation) is rewritten to state these semantics.

Because the helper counts before reading, `MUSEFS_FAULT_PREAD_US` injection
now applies to every Ogg backing read, making the latency bench's Ogg numbers
real rather than structurally zero.

With the feature off, `on_pread` is an empty `#[inline(always)]` fn, so the
helper compiles to a bare `read_exact_at` — no behavior or cost change.

### 2. Serve-site counter tests (#76)

Extend `musefs-core/tests/metrics.rs` (existing file; each test takes the
existing `METRICS_LOCK` and calls `metrics::reset()`), driving reads through a
real `Musefs` built over a scanned tempdir:

- **FLAC + PICTURE block** → reading the file's art region (`Segment::ArtImage`)
  asserts `art_chunks` incremented.
- **FLAC + APPLICATION block** → reading the binary-tag region
  (`Segment::BinaryTag`) asserts `binary_tag_chunks` incremented (the exact
  regression class that shipped once).
- **Ogg audio** (via `common::write_ogg`) → a read covering the audio region
  asserts `preads > 0` and `pread_bytes > 0`, locking in section 1's fix.
- **Ogg + cover art** → a read covering the art region (`Segment::OggArtSlice`)
  asserts `art_chunks` incremented.

Fixture gaps are filled in `musefs-core/tests/common/mod.rs` in the existing
helper style (a PICTURE-block body builder, an APPLICATION-block fixture, an
Ogg-with-art writer — whichever of these `common` does not already provide).

These tests assert *increments* (counter strictly greater than its
pre-read snapshot for the targeted counter), not exact totals, so they stay
robust to unrelated reads in the serve path.

### 3. CI + benchmark refresh

- `ci.yml`, `check` job: add one step after "DB mutants-feature tests":

  ```yaml
  - name: Core metrics-feature tests
    run: cargo test -p musefs-core --features metrics
  ```

  This mirrors the adjacent fuzzing-feature and mutants-feature steps and puts
  the whole metrics suite (existing + new) on every PR.

- Re-run `bench_read_under_latency` locally after the fix and update the Ogg
  rows in `BENCHMARKS.md` (and the optimization-pass tracking README if it
  mirrors those rows), noting that pre-fix Ogg counters were blind — the old
  zeros meant "uninstrumented", not "free".

## Error handling

No new error paths. `read_counted` propagates the `io::Result` exactly as the
bare `read_exact_at` calls do today; the one tolerated-failure site
(`find_page_start`'s CRC-check read) keeps its tolerate-and-continue handling.

## Testing / validation

- New serve-site tests above, run via `cargo test -p musefs-core --features
  metrics` (now also in CI).
- Existing metrics tests must stay green (the Ogg fix adds counts only to Ogg
  paths; FLAC/MP3/M4A/WAV expectations are unaffected).
- `cargo test --workspace`, clippy, fmt as usual; byte-identical-audio
  property tests are untouched by design (no serve-path byte changes).
- Bench refresh recorded in `BENCHMARKS.md` per the deliverable above.
