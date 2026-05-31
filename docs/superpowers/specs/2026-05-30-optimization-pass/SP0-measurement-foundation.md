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
  (`on_open`/`on_stat`/`on_pread`/`on_art_chunk`), a `Snapshot` +
  `snapshot()`/`reset()` API, **and in-process per-syscall latency injection**
  already wired to `MUSEFS_FAULT_OPEN_US` / `MUSEFS_FAULT_STAT_US` /
  `MUSEFS_FAULT_PREAD_US`. Extend, don't replace. SP0's reporting reuses
  `snapshot()`; the only counter gap is fsync (see Component 4).
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
- `bytes_per_track` — the **audio payload** size (authoritative definition).
  Payload is filler bytes (content is irrelevant: `BackingAudio` is served
  verbatim and probing reads only headers). Total file size = payload +
  synthesized format front + any embedded art.
- `art_bytes_per_track` — embedded cover size (0 = no embedded art). One shared
  cover per album, so the content-addressed `art` table stays deduped/small. Per
  tier, the table below states whether art is embedded.
- `format_mix` — default FLAC-only for determinism; optional mix across
  FLAC/MP3/M4A/Ogg/WAV. **New work:** the mix must be able to include an M4A with
  `moov` **at end-of-file** — the interesting case for SP1's bounded reads. The
  existing `tests/common` builder (`minimal_m4a`) is moov-**first** only, so a
  moov-at-end builder must be authored. (Verified: the MP4 reader's `locate`
  finds boxes by scanning top-level atoms regardless of order, so a moov-at-end
  fixture parses and scans correctly — no reader change needed.)
- `seed` — deterministic generation; a given (params, seed) reproduces byte-for-
  byte identical files.

Tiers (named presets; each is just a parameter bundle, still overridable):

| Tier | Shape | payload/track | art | ~Footprint | Purpose |
|---|---|---|---|---|---|
| `ci` | ~200 tracks | ~4 KB | none | tens of MB | CI compute regression guard; runs in seconds |
| `large-compute` | 100k tracks (10k×10) | ~8 KB | one ~30 KB cover/album (deduped) | ~1 GB | SP2/SP3 scale + injection latency runs |
| `bandwidth` | ~1k tracks | ~30 MB | one realistic cover/album | ~30 GB | real-mount throughput; extrapolate per-file |
| `custom` | env-defined | env-defined | env-defined | — | arbitrary manual composition |

(`ci` omits embedded art to keep files tiny and parsing trivial; art-bearing
synthesis paths are still exercised because the shared per-album cover lands in
the DB and on the synthesized read path at the larger tiers.)

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
  single-threaded and **concurrent**. The concurrent variant runs `M` reader
  threads streaming distinct files plus one walker thread that loops
  `lookup`+`getattr` over the tree, all sharing one `Arc<Musefs>` in-process
  (`Musefs` methods take `&self`, so no real mount is needed — per the README
  rule that compute-bound work is fine on tempfs). `M` defaults to `2×ncpu`,
  env-overridable. This exercises SP3's `handles` / `size_cache` mutex
  contention and the worker pool.

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
- **Full op surface required** (not just the delayed ones): SQLite in WAL mode
  under the mount needs `create`/`open`/`write`/`read`/`fsync`/`flush`/`release`,
  `setattr`+`ftruncate` (WAL extend/truncate), `unlink`/`rename` (journal/wal/shm
  cleanup and checkpoint), plus `opendir`/`readdir`/`lookup`/`getattr`/`statfs`
  and possibly `getxattr`. A missing op surfaces as an obscure `EIO` mid-scan —
  the `ssd` smoke test (Acceptance) is the guard, and building this op surface is
  the **first** plan task, not the last.
- Because the corpus **and** the SQLite DB live under this mount, both
  backing-file I/O (`fs::read`, `File::open`, `read_exact_at`) **and** SQLite's
  own fsyncs are delayed uniformly — no in-process I/O seam, no SQLite VFS shim.
- **Relationship to the existing `MUSEFS_FAULT_*_US` knobs:** those stay as the
  *tempfs-mode* complement — they inject open/stat/pread latency in-process with
  no mount, useful for read-path microbenchmarks. The passthrough FS is the
  *whole-system* mechanism that additionally covers `write`/`fsync` and SQLite's
  durability path (which the in-process counters cannot reach). The two are used
  in different modes, not simultaneously; SP0 keeps both and documents which to
  use when.
- Profiles via env knobs (illustrative defaults, tunable): `ssd` (≈0), `hdd`
  (multi-ms per open/read), `nfs-ssd` (sub-ms RTT per op), `nfs-hdd` (RTT +
  seek). A profile is a bundle of per-op latencies.
- Gating: requires `/dev/fuse`; `#[ignore]`d like the existing e2e tests. Never
  part of the default `cargo test` / `cargo bench`.

This is the heaviest piece to build but keeps the production code untouched and
exercises real syscalls end to end.

### 4. Metrics & reporting

- Reuse `metrics::snapshot()` for opens/stats/preads/pread_bytes/art_chunks.
- **fsync counts** come from the passthrough FS (it counts kernel `fsync` ops
  directly); for tempfs in-process runs add a complementary `on_fsync` counter to
  `metrics.rs` only if a meaningful in-process hook exists. **WAL caveat:** under
  WAL, SQLite fsyncs the `-wal` file and at checkpoints, not once per row/commit,
  so the fsync count is a **relative** signal (it must drop when SP1 batches
  transactions) — *not* an absolute per-transaction number. SP1 must interpret it
  that way.
- Peak RSS from `/proc/self/status` `VmHWM` (Linux; no new crate). **Scope:** RSS
  is the bench process and is meaningful only for the **in-process tempfs**
  scan/refresh benches (where SP1's whole-file-read spike shows). Under the
  passthrough mount the FS runs in a separate process, so its memory is out of
  scope and RSS is not reported for those runs.
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
  per-op overrides — selects the **passthrough-FUSE** profile (whole-system,
  incl. write/fsync). Distinct from the pre-existing in-process
  `MUSEFS_FAULT_OPEN_US` / `MUSEFS_FAULT_STAT_US` / `MUSEFS_FAULT_PREAD_US` knobs,
  which inject read-path latency with no mount. The two are not combined; the
  harness documents which mode a run uses.

## Testing & acceptance criteria

- `cargo bench` runs the `ci`-tier compute benches on tempfs with no extra setup.
- The generator produces byte-deterministic corpora for a given (params, seed) at
  every tier; real-library mode scans a provided directory without writing to it.
- **Passthrough smoke test (gating, runs before any timed run):** an `#[ignore]`
  test scans *and* reads a small corpus through the passthrough FS at the `ssd`
  (≈0) profile — including a SQLite WAL checkpoint — and passes, proving the op
  surface is complete.
- An `#[ignore]` test then demonstrates the passthrough layer measurably raises
  scan and read time under a non-zero latency profile (and ~0 under `ssd`).
- **Reproducibility / regression gate:** the `ci` compute bench reports a stable
  median across repeated runs on tempfs with documented expected variance, and a
  regression threshold (e.g. median > X%) is defined so SP1–SP4 have a concrete
  pass/fail gate — this is what makes the harness *useful*, not merely present.
- **fsync signal:** a known scan workload through the passthrough FS yields an
  fsync count that moves in the expected direction (drops) when writes are
  batched — validating it as the relative SP1 signal described in Component 4.
- Reporting emits the comparable table described above.
- All existing crate tests and the `#[ignore]` FUSE e2e tests stay green; with
  the `metrics` feature off, behavior is unchanged.

## Risks & open questions (resolve during planning)

- **Passthrough FUSE completeness (top risk)** — must implement the full op
  surface in Component 3 (incl. SQLite WAL's write/create/truncate/rename), not
  just the delayed ops. A missing op surfaces as an obscure `EIO` mid-scan;
  guarded by the gating `ssd` smoke test, which is the first plan task.
- **Criterion vs one-shot** — read benches fit Criterion; scan/refresh at scale
  do not. Plan splits them: Criterion group for read, a separate timed binary or
  `#[ignore]` test for scan/refresh.
- **fsync semantics** — under WAL the count is a relative signal, not a
  per-transaction absolute (see Component 4). Confirm during planning whether an
  in-process `on_fsync` hook is worth adding for tempfs runs or whether the
  passthrough count alone suffices for SP1.

## New dependencies

None required at runtime. Bench/test-only: `criterion` and `tempfile` are already
dev-deps; peak RSS reads `/proc` directly (no crate). The passthrough FUSE reuses
the existing `fuser` dependency in `musefs-fuse`.
