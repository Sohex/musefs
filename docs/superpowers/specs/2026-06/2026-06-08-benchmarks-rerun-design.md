# Spec: Re-run & rewrite BENCHMARKS.md (apples-to-apples, this box)

*2026-06-08*

## Goal

Every number in `BENCHMARKS.md` currently comes from different machines and
conditions, accreted across many sessions (the header machine — AMD EPYC 6-core
/ 17 GiB SSD — is not the machine the repo now lives on, and several sections
were measured "under load"). Re-measure every pass on **this box** under a
single, documented methodology so the numbers are mutually comparable and
self-consistent, then rewrite the file in a layered structure that reads clearly.

This is a measurement + documentation task only. **No changes to the optimization
code under test, nor to existing harness logic.** The benchmark harnesses
(`bench_ingest`, `bench_refresh`, `read_throughput`) and the served-byte invariant
are untouched. Two mechanical exceptions, neither of which alters what is measured:
(a) where a "before" commit predates a bench that its "after" commit *added*, the
after-commit's harness file is overlaid onto the before checkout to measure the old
code (see Harness availability below); (b) the #112 passthrough `dd` recipe, today
only prose, is captured as a committed runnable script.

## Decisions (settled during brainstorming)

1. **Re-measure before/after pairs**, not current-state-only. The file's value is
   per-pass *attribution* ("this change bought X"), so the before/after story is
   preserved.
2. **After = the pass's own squash-merge commit; before = its parent
   (`<after>^`).** Each pass is isolated against its immediate predecessor — NOT
   against current `main`. Rationale: overlapping code paths (e.g. `read_segments`
   alloc elimination was done in both SP3 and phase6-pr3) would double-count and
   misattribute if every section used current `main` as "after". Adjacent
   before/after commits run the *same* bench code (the after-commit's harness,
   overlaid onto the before checkout where the before commit predates it — see
   Harness availability), so each isolated pair is self-consistent.
3. **Plus one cumulative summary** `16caba4` → current `main`, clearly labelled as
   cumulative (non-isolating), to show the total journey.
4. **Storage class per section is preserved**, mapped onto this box's equivalents:
   durable → `/data`, RAM-backed → `/dev/shm`.
5. **Layered file structure**: at-a-glance summary → shared methodology → trimmed
   per-pass detail sections.

## This box (new machine header)

- 8-core, **32 GB RAM**, `rustc 1.96`, release builds.
- **Durable storage → `/data`**: a 2-device btrfs spanning the rotational disks
  `/dev/sda3` + `/dev/sdb3` (`compress=zstd:1`, `noatime`), 3.0 TB free, profile
  `Data: single` / `Metadata,System: RAID1` (so metadata fsyncs mirror to both
  spindles). Distinct from the `md` RAID that backs `/home`. No SSD on this box —
  durable benches are rotational, so SP1's fsync-bound "before" absolutes will be
  *larger* than the header SSD's (and the RAID1 metadata mirroring adds to that);
  expected, and still a valid before/after on identical hardware. `zstd:1` is
  irrelevant to the fsync-count tests and the bandwidth-tier FLAC corpus is largely
  incompressible, so it does not materially distort the I/O numbers; noted in the
  header for honesty.
- **RAM-backed → `/dev/shm`** (16 GiB tmpfs) for the compute-isolated / tempfs
  sections. (`/tmp` on this box is *also* tmpfs, so the harnesses' default
  `tempfile::tempdir()` is already RAM-backed — but we pin `TMPDIR=/dev/shm`
  explicitly so the RAM rows are deterministic and not silently `/tmp`-sized.)
- One-time setup (requires sudo): `sudo mkdir -p /data/musefs-bench && sudo chown
  "$USER" /data/musefs-bench`.

## Methodology (written once in the rewritten file)

- **Before/after definition** as in decision 2 above. Both sides built fresh on
  this box from a clean checkout of the respective commit.
- **Run convention:**
  - Ignored-test benches (`bench_ingest`, `bench_refresh`): **3 runs, report
    median**; show the spread where runs are noisy.
  - Criterion (`read_throughput`): its own sampling; report Criterion's median and
    change-estimate (p-value) as the file already does.
  - Box quiesced before each run set.
- **Storage class preserved per section**, set per harness:
  - `bench_ingest`: honors `MUSEFS_BENCH_DIR` (via `bench_base_dir` /
    `corpus.rs`) → set it to `/data/musefs-bench` for the durable tiers,
    `/dev/shm/...` for the RAM tiers.
  - `bench_refresh` and `read_throughput`: do **not** read `MUSEFS_BENCH_DIR` —
    they call `tempfile::tempdir()` unconditionally, so storage is controlled by
    `TMPDIR`. Export `TMPDIR=/dev/shm` for their (RAM-only) rows.

## Rerun matrix

All "after" SHAs are now pinned (resolved against the tree). Before = `<after>^`.
"H?" flags rows where the before commit predates the after's bench, requiring the
harness overlay (see Harness availability).

| Pass | Before | After | Harness | H? | Storage |
|------|--------|-------|---------|----|---------|
| SP1 ingest (small-files §1, bandwidth §2, fsync-count §3, large-compute §4) | `ccbbfaa^` | `ccbbfaa` | `bench_ingest` | — | durable `/data` (§1–§3) + `/dev/shm` (§4) + latencyfs (§3) |
| SP2 refresh | `ed5f380^` | `ed5f380` | `bench_refresh` | **yes** | `/dev/shm` |
| SP3 read residuals | `e8d56bd^` | `e8d56bd` | `read_throughput` | — | `/dev/shm` |
| SP4 ogg serve | `a62453b^` | `a62453b` | `read_throughput` (+ latencyfs) | **yes** | `/dev/shm` |
| Phase 6 PR1 (#69) | `e7ae912^` | `e7ae912` | `bench_refresh` | — | `/dev/shm` |
| Issue #114 root fan-out | `0881b31^` | `0881b31` | `bench_refresh` | — | `/dev/shm` |
| Phase 6 PR2 (#67/#68) | `2d4faf3^` | `2d4faf3` | `bench_ingest` | — | `/dev/shm` |
| Phase 6 PR3 (#70) | `32be8f0^` | `32be8f0` | `read_throughput` | — | `/dev/shm` |
| HeaderCache (#136) | `2e6674e^` | `2e6674e` | `read_throughput` | — | `/dev/shm` |
| Issue #112 passthrough | `0881b31` (pre) | `faec017` | `dd` on real mount, **sudo** | n/a | RAM-cached backing |
| **Cumulative** | `16caba4` (nominal) | current `main` | derived: composed per-pass deltas + current-`main` absolutes (no same-harness run — API drift; see Harness availability) | n/a | n/a |

Resolved anchors: `16caba4` (baseline), SP1 `ccbbfaa`, SP2 `ed5f380`, SP3
`e8d56bd`, SP4 `a62453b`, #69 `e7ae912`, PR2 `2d4faf3`, #114 `0881b31`, PR3
`32be8f0`, #136 `2e6674e`, #112 `faec017`. Note `0881b31` is *both* #114's after
and #112's before — the rewrite must not double-list #114.

## Harness availability (the overlay rule)

Several passes *added* the bench they report. Verified against the tree: the
`bench_refresh_one_across_library_sizes` sweep (SP2's tables) does not exist at
SP2's before commit, and `cold_first_read`/`seek_read` (SP4's headline ~8×/~15×
tables) do not exist at SP4's before commit. So "run each commit's own harness"
fails for the before side of SP2 and SP4 (the **H? = yes** rows).

Rule for those rows: **check out the before commit, overlay the after-commit's
harness file (`git checkout <after> -- <bench-file>`, or cherry-pick just the
missing test fns), run, then restore.** This is sound because the harness only
*measures* — it does not touch the optimization code under test, so the before
column reflects the old code measured by the new yardstick. This is the same
technique the current file already uses for #69 (BENCHMARKS.md "Before (main —
apply 4-point sweep edit first)").

**The cumulative row cannot use this technique.** A same-harness `16caba4`↔`main`
measurement is infeasible: the current-`main` harnesses require `MountConfig`'s
`case_insensitive` field and `scan_directory_with`/`ScanOptions`/`revalidate_with`
(none exist at `16caba4`), and the `16caba4`-era harness omits the now-required
`case_insensitive` field (so it won't compile on `main` either). API drift over
the whole journey makes any single harness incompatible with one end. The
cumulative summary is therefore **derived, not freshly measured**: per subsystem,
**compose the per-pass isolated deltas already collected** (Tasks 1–9) and anchor
them to current-`main` absolute numbers measured once with the `main` harness.
Labelled explicitly as composed / non-isolating.

## Known wrinkles

- **SP2 is a single pre/post pair** (`ed5f380^` → `ed5f380`), *not* a deferred
  Stage-A/Stage-B question: Stage A was an intra-PR pre-state, and only the
  Stage-B squash commit landed on `main`. The current file's two tables (Stage A,
  Stage B) collapse to one before/after table in the rewrite.
- **Old-commit build drift:** build each before/after pair from **its own pinned
  `Cargo.lock`** (not a shared lock), so registry resolution matches what the
  commit shipped. Edition is pinned by each commit's manifest (the edition-2024
  bump landed after #136; earlier commits are edition 2021 — the lockfile handles
  this). Drift is low-risk (all May–June 2026, `rustc 1.96`); build failures are
  caught at run time and reported, not papered over.
- **`bandwidth` tier (~30 GiB)** lands on durable `/data` only (won't fit RAM);
  it is long-running, so a single run is acceptable unless cheap to repeat.
- **SP1 §3 fsync-count** runs *through* the latencyfs mount (which counts fsyncs
  at the FUSE layer) but its backing store sits on rotational `/data` with RAID1
  metadata — so the §3 wall-ms absolutes are not directly comparable to the old
  SSD header's; the 403→0 *fsync-count* delta is the portable signal, the ms is
  box-relative.

## Gates removed entirely

The per-pass "Gates" subsections (byte-identical proptests, in-diff mutation) are
**correctness** results, not performance, and are **dropped from `BENCHMARKS.md`
entirely** — not re-run, not kept as notes. Correctness lives in the test suite,
CI, and the per-PR history; this file is about performance only.

## Rewritten file structure (layered)

1. **Results at a glance** — the cumulative `16caba4` → `main` summary (one line
   per subsystem: ingest / refresh / serve, as the before→after delta), plus one
   headline number per pass. **Headline-selection rule** (so the table is
   reproducible): the headline is the pass's single largest *statistically-
   significant* delta on its deployment-representative tier (for SP1, the durable
   tier, not tempfs); for passes whose changes are within noise (e.g. #136), state
   "within noise" rather than cherry-picking a sign or magnitude.
2. **Methodology** — the block above, written once (machine, before/after
   definition, run convention, storage mapping per harness, overlay rule).
3. **Per-pass detail sections** — same passes as today, each trimmed to: a
   one-line "what changed", the before/after table with this-box numbers, and the
   reproduce command. No gate notes; repeated methodology boilerplate removed.
   **SP2's Stage-A/B narrative and SP4's crc linear↔matrix↔memo evolution table
   are kept** as "why" context (they explain the implementation journey, not just
   the numbers) — trimming applies only to the repeated methodology boilerplate,
   not to genuine design rationale.

## #112 passthrough harness

The dd-based recipe currently lives only as prose in `BENCHMARKS.md`. Per the
project convention that plans commit test harnesses in-tree as runnable scripts,
capture it as a committed script (e.g. `benches/passthrough_dd.sh`) that takes the
mount path, does the fresh-mount-per-binary 3-runs-inside loop, and prints the
GB/s. The reproduce command in the rewritten §112 section points at the script.

## Resolved presentation decisions

- **SP2/SP4 detail depth:** *keep* the richer "why" context — SP2's Stage A/B
  narrative and SP4's crc linear↔matrix↔memo evolution table stay. They explain
  the implementation journey; only repeated methodology boilerplate is trimmed.
- **Cumulative row form:** per-subsystem **deltas**, one line each for ingest /
  refresh / serve, composed from the per-pass isolated deltas and anchored to
  current-`main` absolutes (a same-harness `16caba4`→`main` run is infeasible —
  see Harness availability). Labelled composed / non-isolating.

## Out of scope

- No changes to the optimization code under test, nor to existing harness logic
  (the overlay rule and the #112 script are measurement scaffolding, not changes
  to what is measured).
- No new benchmarks of behavior not already benched.
- Old numbers are replaced, not archived in-file (git history preserves them).
