# SP0 — Measurement foundation — design

*Date: 2026-05-30 · Part of the [2026-05-30 optimization pass](./README.md)*

## Goal

Complete the measurement harness the 2026-05-26 Phase 0 only partially built, so
every later SP is validated against numbers rather than guessed. Deliver:

1. a **synthetic library generator** at controlled scale (tiered + custom +
   real-library);
2. a **bench suite** covering scan/ingest, refresh, and read — single-stream and
   concurrent;
3. a **deterministic latency-injection layer** (a bench-only passthrough FUSE
   filesystem) so HDD/NFS profiles are reproducible on one machine;
4. **comparable reporting** that makes SSD vs HDD vs NFS runs directly readable.

No production code paths change. The only `src/` edits are additive metrics
counters (behind the existing `metrics` feature) for paths not already counted.

## Reuse (existing foundation)

- `musefs-core/src/metrics.rs` — feature-gated atomic counters
  (`on_open`/`on_pread`/`on_art_chunk`, etc.). Extend, don't replace.
- `musefs-core/benches/read_throughput.rs` — the single existing Criterion bench
  (one 4 MiB FLAC, in-memory DB). Generalize it to consume the corpus generator.
- `musefs-*/tests/common/mod.rs` — `make_flac`, `streaminfo_body`,
  `vorbis_comment_body`, and the per-format builders. The generator builds on
  these rather than inventing new file synthesis.
- The `#[ignore]` + `/dev/fuse` gating pattern from `musefs-fuse` e2e tests.

## Non-goals

- Any optimization itself (that is SP1–SP4). SP0 only measures.
- Changing the SQLite schema or any production hot path.
- A faithful *bandwidth* model from injection — injection models **latency**
  (per-op RTT/seek); true throughput is measured on a real mount.
- Cross-platform support beyond Linux (the project is already Linux/FUSE-only).

## Components

### 1. Synthetic library generator

A bench/test-support module (not shipped in the library binary) that materializes
a backing directory of audio files plus, optionally, scans it into a DB.

Parameters (all env-overridable; see Interfaces):

- `albums`, `tracks_per_album` — tree shape.
- `bytes_per_track` — audio payload size. Payload is filler bytes (content is
  irrelevant: `BackingAudio` is served verbatim and probing reads only headers).
- `art_per_album` — embedded cover count; one shared cover per album so the
  content-addressed `art` table stays deduped/small. Embeddable into files via a
  size knob, or omitted for size-sensitive runs.
- `format_mix` — default FLAC-only for determinism; optional mix across
  FLAC/MP3/M4A/Ogg/WAV. The mix **must** be able to include an M4A with `moov`
  at end-of-file, the interesting case for SP1's bounded reads.
- `seed` — deterministic generation; a given (params, seed) reproduces byte-for-
  byte identical files.

Tiers (named presets; each is just a parameter bundle, still overridable):

| Tier | Shape | bytes/track | ~Footprint | Purpose |
|---|---|---|---|---|
| `ci` | ~200 tracks | ~4 KB | tens of MB | CI compute regression guard; runs in seconds |
| `large-compute` | 100k tracks (10k×10) | ~8 KB | ~1 GB | SP2/SP3 scale + injection latency runs |
| `bandwidth` | ~1k tracks | ~30 MB | ~30 GB | real-mount throughput; extrapolate per-file |
| `custom` | env-defined | env-defined | — | arbitrary manual composition |

Plus a **real-library mode**: instead of generating, point the harness at an
existing music directory and scan it in place. Scan is read-only with respect to
the audio files (it writes only to its own SQLite DB), and the DB path is a
separate, explicit knob — **the real library is never modified**.

### 2. Bench suite

- **Scan / ingest** — one-shot timing (Criterion's resampling does not fit a
  100k-file scan): wall time, op counts (opens/preads), **fsync count**, and peak
  RSS for a cold `scan_directory`, and for an incremental `revalidate`.
- **Refresh** — time `poll_refresh` / tree rebuild after touching 1 track vs N
  tracks. This is the metric SP2 attacks (full vs incremental rebuild).
- **Read** — generalize `read_throughput`: sequential and random-seek reads,
  single-threaded and **concurrent** (M streams + a metadata walker). The
  concurrent variant exercises SP3's `handles` / `size_cache` contention and the
  worker pool.

Each bench reports throughput where meaningful, the metrics op counts, and (for
scan/refresh) peak RSS.

### 3. Latency-injection layer — passthrough FUSE

A bench-only passthrough filesystem (test-support module in `musefs-fuse`, which
already owns the `fuser` dependency; exposed for benches). It mirrors a real
backing directory but sleeps a configurable amount per operation before
delegating to the underlying file via `std::fs`:

- Delayed ops: `lookup`/`getattr` (stat RTT), `open` (open RTT), `read` (seek +
  RTT, optionally per-op rather than per-byte), `write`/`fsync` (commit
  durability cost).
- Because the corpus **and** the SQLite DB live under this mount, both
  backing-file I/O (`fs::read`, `File::open`, `read_exact_at`) **and** SQLite's
  own fsyncs are delayed uniformly — no in-process I/O seam, no SQLite VFS shim.
- Profiles via env knobs (illustrative defaults, tunable): `ssd` (≈0), `hdd`
  (multi-ms per open/read), `nfs-ssd` (sub-ms RTT per op), `nfs-hdd` (RTT +
  seek). A profile is a bundle of per-op latencies.
- Gating: requires `/dev/fuse`; `#[ignore]`d like the existing e2e tests. Never
  part of the default `cargo test` / `cargo bench`.

This is the heaviest piece to build but keeps the production code untouched and
exercises real syscalls end to end.

### 4. Metrics & reporting

- Extend `metrics.rs` only where a path is uncounted — notably an **fsync
  counter** for the scan/ingest path (so SP1's batching win is measurable).
- Peak RSS from `/proc/self/status` `VmHWM` (Linux; no new crate).
- Emit a compact, comparable table per run: `tier · storage-class · wall ·
  opens · preads · fsyncs · peak-RSS`, so an SSD run and an NFS-HDD run of the
  same tier sit side by side.

### 5. CI gating

- Default CI: only the `ci` tier, on tempfs, as a compute regression guard.
- `large-compute`, `bandwidth`, real-mount, and all latency-injection runs are
  `#[ignore]`d / env-gated, opt-in.

## Interfaces (env vars — names indicative, finalized in the plan)

- `MUSEFS_BENCH_TIER` = `ci` | `large-compute` | `bandwidth` | `custom`
- `MUSEFS_BENCH_ALBUMS`, `MUSEFS_BENCH_TRACKS_PER_ALBUM`,
  `MUSEFS_BENCH_BYTES_PER_TRACK`, `MUSEFS_BENCH_ART_PER_ALBUM`,
  `MUSEFS_BENCH_FORMAT_MIX`, `MUSEFS_BENCH_SEED` — override any tier dimension.
- `MUSEFS_BENCH_DIR` — target directory for corpus + DB (default: tempdir). Point
  at a real SSD/HDD/NFS mount for ground-truth runs.
- `MUSEFS_BENCH_LIBRARY` — path to an existing real library (real-library mode);
  mutually exclusive with generation.
- `MUSEFS_BENCH_DB` — explicit DB path (kept separate from a real library).
- `MUSEFS_BENCH_LATENCY_PROFILE` = `ssd` | `hdd` | `nfs-ssd` | `nfs-hdd` |
  per-op overrides — selects the passthrough-FUSE profile.

## Testing & acceptance criteria

- `cargo bench` runs the `ci`-tier compute benches on tempfs with no extra setup.
- The generator produces byte-deterministic corpora for a given (params, seed) at
  every tier; real-library mode scans a provided directory without writing to it.
- An `#[ignore]` test demonstrates the passthrough-FUSE layer measurably raises
  scan and read time under a non-zero latency profile (and ~0 under `ssd`).
- Reporting emits the comparable table described above.
- All existing crate tests and the `#[ignore]` FUSE e2e tests stay green; with
  the `metrics` feature off, behavior is unchanged.

## Risks & open questions (resolve during planning)

- **Passthrough FUSE completeness** — needs enough of the `Filesystem` surface
  (lookup/getattr/open/read/release/opendir/readdir/fsync/flush) for SQLite WAL +
  scan + reads to work. Risk of a missing op surfacing as an obscure I/O error;
  mitigate with a smoke test that scans + reads through the passthrough at `ssd`
  (≈0 latency) before trusting timed runs.
- **Criterion vs one-shot** — read benches fit Criterion; scan/refresh at scale
  do not. Plan splits them: Criterion group for read, a separate timed binary or
  `#[ignore]` test for scan/refresh.
- **fsync counting** — the cleanest hook for fsync counts is the passthrough FS
  (counts kernel fsync ops); the in-process `metrics` counter is a complement for
  tempfs runs where there is no FUSE layer. Plan decides whether both are needed.

## New dependencies

None required at runtime. Bench/test-only: `criterion` and `tempfile` are already
dev-deps; peak RSS reads `/proc` directly (no crate). The passthrough FUSE reuses
the existing `fuser` dependency in `musefs-fuse`.
