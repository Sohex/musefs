# Progress indicator for `musefs scan` / `musefs scan --revalidate`

Issue: [#406](https://github.com/Sohex/musefs/issues/406)
Date: 2026-06-14
Status: design approved, pending spec review

## Problem

`musefs scan` walks the backing tree and ingests supported audio into the
SQLite store. On a multi-hundred-album library this runs for a long time with
no feedback — the operator cannot tell whether it is progressing, stalled, or
how far along it is. The same applies to `musefs scan --revalidate`.

## Goals

- A live progress indicator on an interactive terminal: a discovery spinner
  during the walk, then a determinate bar (position / total, percent, ETA,
  current file) during ingest.
- Plain line-based progress on a non-TTY (logged / piped runs).
- A final summary line that includes elapsed time.
- Honour the existing `--quiet` flag: suppress progress *and* the summary.
- Cover both the `scan` and `revalidate` code paths.

## Non-goals

- An `added / updated / unchanged` breakdown for the full-scan path. That would
  require plumbing per-file upsert disposition through `ingest_bulk` and the DB
  layer; out of scope. The scan summary keeps `scanned / skipped / failed`;
  revalidate already reports `updated / unchanged / pruned / failed`.
- A `log`↔progress-bar bridge (e.g. `indicatif-log-bridge`). Core `log::warn`
  lines share stderr with the bar and may briefly smear it; indicatif redraws
  on the next tick. Acceptable for v1.

## Layering

`musefs-core` stays UI-agnostic. It emits progress *events* through an optional
callback held in `ScanOptions`. All rendering — indicatif on a TTY, plain lines
otherwise — lives in `musefs-cli`. Core never prints progress.

## Core API (`musefs-core/src/scan.rs`)

```rust
/// A progress event emitted during a scan or revalidate. Borrows the current
/// path to avoid a per-file allocation in the writer hot path.
#[derive(Debug, Clone, Copy)]
pub enum ScanProgress<'a> {
    /// A supported-audio file was found during the directory walk;
    /// `found` is the running count.
    Discovered { found: u64 },
    /// The walk (and, for revalidate, the skip-unchanged pass) finished;
    /// `total` files will be ingested and tracked by the determinate bar.
    Walked { total: u64 },
    /// A file was committed. `done` runs 1..=total; `path` is its absolute path.
    Ingested { done: u64, total: u64, path: &'a str },
}

/// UI-agnostic progress callback. Invoked only from the caller's thread (the
/// walk and the single writer thread), never from probe workers — `ScanOptions`
/// is passed into `run_pipeline` by `&` and never moves into a worker. The
/// `Send + Sync` bound is therefore not required by today's code; it is kept
/// deliberately as future-proofing (so emit could move into workers without an
/// API break) and is free here because `indicatif::ProgressBar` is already
/// `Send + Sync`. The `for<'a> Fn` HRTB is needed only because `Ingested`
/// borrows the path; the allocation it saves is negligible next to the existing
/// per-file `to_string_lossy().into_owned()` + DB write, so a maintainer should
/// not contort the API to preserve the borrow.
#[derive(Clone)]
pub struct ProgressSink(Arc<dyn for<'a> Fn(ScanProgress<'a>) + Send + Sync>);

impl ProgressSink {
    pub fn new(f: impl for<'a> Fn(ScanProgress<'a>) + Send + Sync + 'static) -> Self;
    fn emit(&self, ev: ScanProgress<'_>); // pub(crate)
}

impl fmt::Debug for ProgressSink { /* writes "ProgressSink" so ScanOptions: Debug holds */ }
```

`ScanOptions` gains:

```rust
pub progress: Option<ProgressSink>, // Default: None
```

`Default::default()` sets it to `None`, so every existing caller (and the many
tests using `..Default::default()`) is unaffected.

`ScanProgress` and `ProgressSink` are re-exported from `musefs-core/src/lib.rs`.

## Core wiring

### Discovery (shared by scan and revalidate)

The private walk helpers `collect_audio_inner`, `descend`, and `push_file` gain
a `progress: Option<&ProgressSink>` parameter. `push_file` emits
`Discovered { found: out.len() }` immediately after it pushes a file (the only
choke point where the collected count grows).

To avoid touching the ~9 existing `collect_audio(root, out, follow_symlinks)`
call sites (tests, the full-oracle path, the revalidate skip pass), the current
signature is preserved as a thin wrapper that delegates with `None`:

```rust
fn collect_audio(root, out, follow_symlinks) -> io::Result<SkipTally> {
    collect_audio_with(root, out, follow_symlinks, None)
}
fn collect_audio_with(root, out, follow_symlinks, progress: Option<&ProgressSink>)
    -> io::Result<SkipTally> { /* existing body, threads progress inward */ }
```

`scan_directory_with` and `revalidate_with` call `collect_audio_with(.., opts.progress.as_ref())`.

### `Walked`

- `scan_directory_with`: emit `Walked { total: files.len() }` after the walk,
  before `run_pipeline`. (For a single-file target, `total == 1`; for an empty
  library, `total == 0`.)
- `revalidate_with`: emit `Walked { total: changed.len() }` after the
  skip-unchanged pass, before `run_pipeline`. The bar therefore tracks only the
  files actually re-ingested. The (potentially slow) skip pass emits nothing;
  the CLI spinner keeps animating via steady-tick.

### `Ingested` (shared)

`run_pipeline` reads `opts.progress`. It captures `total = files.len()` before
moving `files` into the work queue. Inside `flush`'s per-unit drain, right after
the existing `*scanned += 1`, it emits
`Ingested { done: *scanned, total, path: &abs_path }` — `abs_path` is in scope
there (it is borrowed, not moved, by `ingest_bulk(&mut bw, &abs_path, ..)`). The
emit stays at the per-unit point (not after `commit()`), so events arrive
per-file (`done` strictly increasing `1..=total`) even though commits are
batched. Per-file failures and races do not advance `done`.

## CLI rendering (`musefs-cli`)

Add the `indicatif` dependency (with `default-features = false` plus only the
features needed for `ProgressBar` + `HumanDuration`, to avoid pulling unused
transitive crates). The TTY predicate is **`std::io::stderr().is_terminal()`**
specifically (not stdout): progress renders on stderr, so a `musefs scan | tee`
that leaves a TTY on stderr still animates while stdout stays a clean,
pipe-friendly summary. A helper constructs the `ProgressSink` from `quiet` and
that predicate:

- **`quiet`** → no sink (`None`). Progress and summary both suppressed, as today.
- **TTY** → indicatif on stderr:
  - A spinner with `enable_steady_tick`, message `discovering files… {N} found`,
    updated on `Discovered`.
  - On the first `Walked { total }`: finish the spinner and start a determinate
    bar, **also with `enable_steady_tick`** (so it redraws over interleaved
    `env_logger` warn lines without waiting for the next position update);
    `set_length(total)`; template shows position/length, percent, ETA, and the
    current file's basename as the message. `total == 0` finishes immediately.
  - `Ingested { done, path }` → `set_position(done)` and `set_message(basename)`.
  - At the end: `finish_and_clear()`.
- **non-TTY** → a throttled plain-line printer to stderr driven by a pure
  decision function (see below): on each `Ingested` it prints
  `ingested {done}/{total} ({pct}%)` when a new 5%-of-total milestone is crossed,
  and **always** prints the final `done == total` line. Counts only, no
  animation. This is the "logged runs" path. Discovery on a non-TTY stays quiet
  (no per-file count line) to avoid log spam; milestone lines begin once
  `Walked` is known. On `Walked { total: 0 }` the printer emits nothing (the
  arithmetic is guarded against `total == 0`); the summary still prints
  `0 file(s)`.

The milestone decision is isolated in a pure, unit-testable function so the
mutation gate can reach the densest new arithmetic (the integration test cannot):

```rust
/// Returns the milestone percent (a multiple of `STEP`, or 100) that `done`
/// newly reaches versus `prev_done`, else `None`. `total == 0` → `None`.
fn next_milestone(prev_done: u64, done: u64, total: u64) -> Option<u8>;
```

Progress always renders on **stderr**, leaving stdout clean for the summary
line — `env_logger` (the binary's default `warn`→stderr sink) and the bar
intentionally share stderr; coexistence is cosmetic-only (steady-tick redraw)
and called out as a manual-verification item, not asserted in tests. `run_scan`
wraps each target in an `Instant`, builds the sink once, passes it via
`ScanOptions { progress, .. }`, and appends ` in {HumanDuration}`
(`indicatif::HumanDuration`) to both the scan and revalidate summary lines.

## Edge cases

- Empty library / no supported audio: `Walked { total: 0 }`; spinner finishes,
  no bar; summary still prints `0 file(s)`.
- Single-file target: `scan_directory_with` pushes one file without walking;
  `Walked { total: 1 }`, no discovery ticks.
- Failures / races: tracked in the summary as today; they do not advance the
  bar, which `finish_and_clear`s regardless of final position.

## Tests

### Core (`musefs-core/src/scan.rs` tests, or `tests/`)

Inject a recording sink backed by `Arc<Mutex<Vec<…>>>` and assert against a
fixture library:

- `Walked.total` equals the number of supported files (scan) / changed files
  (revalidate).
- `Ingested.done` is strictly increasing `1..=total`; the final event has
  `done == total`.
- `Discovered.found` increases over the walk and ends at the discovered count.
- A `quiet`-equivalent run (sink `None`) is exercised implicitly by the existing
  suite (no panics, unchanged stats).
- `format!("{opts:?}")` succeeds and contains `ProgressSink`, locking the manual
  `Debug` impl (existing consumers rely on `ScanOptions: Debug`).

These assertions kill the value mutants on the new counters.

### CLI — `next_milestone` unit tests (`musefs-cli`)

Direct boundary tests on the pure function (this is where the densest new
arithmetic lives; the integration test alone leaks `>=`→`>` and `*100/total`
mutants):

- `total == 0` → `None` (no divide-by-zero).
- crossing exactly one 5% boundary returns `Some(pct)`; staying within a bucket
  returns `None`.
- `done == total` always returns `Some(100)` (the guaranteed final line), even
  when the last step is smaller than 5%.
- `total == 1` (`done` 0→1) returns `Some(100)`.

### CLI integration (`musefs-cli/tests/scan.rs`, `assert_cmd` — runs non-TTY)

`assert_cmd` pipes streams, so `is_terminal()` is false: these exercise only the
non-TTY line printer. The fixture must hold **≥ 20 supported files** so at least
one intermediate milestone is guaranteed in addition to the final line.

- A non-quiet scan emits the final `ingested {total}/{total} (100%)` line (and at
  least one intermediate milestone) on stderr, and the summary with ` in …`
  elapsed on stdout.
- `--quiet` emits neither progress nor summary.
- A revalidate run emits the same milestone lines and an elapsed summary.

### Manual TTY verification (no automated coverage)

The entire indicatif TTY branch (spinner → determinate bar → `finish_and_clear`)
is unreachable under `assert_cmd`. Verify by hand on a real terminal against a
library subdirectory (e.g. one album from `/data/media/music`, or the live
~4427-track DB per the live-mount harness): confirm the discovery spinner
animates, the spinner→bar transition fires on `Walked`, the bar shows
percent/ETA/current basename, warn lines do not permanently corrupt the bar, and
the bar clears cleanly at the end. Repeat once for `--revalidate`. This step is
intentionally manual-only.

## Housekeeping

- Editing `scan.rs` shifts the `line:col` anchors in `.cargo/mutants.toml`.
  Re-anchor in the **same commit**: run `check_mutant_anchors.py --fix`, then
  fix any remaining entries by hand, so the pre-commit anchor guard passes.
- README: note that scan/revalidate show a progress bar on a TTY, degrade to
  line-based progress on a non-TTY, and that `--quiet` suppresses both progress
  and the summary.
- `indicatif` is a new `musefs-cli` runtime dependency (well-established;
  stderr-targeted), pinned with `default-features = false` + only the needed
  features. No new dependency in `musefs-core`. Confirm the workspace MSRV
  covers `std::io::IsTerminal` (stable 1.70) and the chosen `indicatif` version.

## Commit plan (each commit green — pre-commit runs the full suite)

Commit 1 is split to shrink the blast radius for the anchor + mutation gates:

1a. Core types: `ScanProgress` / `ProgressSink` (manual `Debug`) /
    `ScanOptions.progress` + re-exports + the `Debug`/`Clone` test. Re-anchor
    `.cargo/mutants.toml` here (this commit is where most `scan.rs` line shifts
    land); **gate: run the pre-commit anchor guard locally before committing** —
    `--fix` only re-anchors covering-set clusters, so op/fn-position anchors are
    fixed by hand and verified, or the commit is rejected.
1b. Core wiring: walk emit (`collect_audio_with` + `push_file`), `Walked` in
    `scan_directory_with` / `revalidate_with`, `Ingested` in `run_pipeline`, and
    the recording-sink tests.
2. CLI: `indicatif` dep, `next_milestone` + its unit tests, the renderer,
   `run_scan` wiring (sink + elapsed), CLI integration tests.
3. Docs: README progress/`--quiet` note.
