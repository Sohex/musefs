# Scan Progress Indicator Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Give `musefs scan` and `musefs scan --revalidate` live progress (discovery spinner → determinate bar on a TTY, throttled `N/M` lines otherwise) plus an elapsed-time summary, honouring `--quiet`.

**Architecture:** `musefs-core` stays UI-agnostic — it emits `ScanProgress` events through an optional `ProgressSink` callback held in `ScanOptions` (default `None`, so all existing callers are untouched). `musefs-cli` owns rendering via `indicatif`, building the sink from `--quiet` and `stderr().is_terminal()`. The walk emits `Discovered`, the callers emit `Walked` once totals are known, and the single writer thread in `run_pipeline` emits `Ingested` per committed file.

**Tech Stack:** Rust workspace (db→format→core→cli→binary), `indicatif` (new `musefs-cli` dep), `std::io::IsTerminal`, existing `cargo-mutants` anchor tooling.

**Reference spec:** `docs/superpowers/specs/2026-06-14-scan-progress-indicator-design.md`

---

## File Structure

- `musefs-core/src/scan.rs` — **modify.** Add `ScanProgress<'a>` enum + `ProgressSink` type, `ScanOptions.progress` field, walk/pipeline wiring, and core tests.
- `musefs-core/src/lib.rs` — **modify.** Re-export `ScanProgress`, `ProgressSink`.
- `.cargo/mutants.toml` — **modify.** Re-anchor `scan.rs` `file:line:col` entries shifted by the edits.
- `musefs-cli/Cargo.toml` — **modify.** Add `indicatif`.
- `musefs-cli/src/progress.rs` — **create.** `next_milestone` pure fn (+ unit tests) and the `ScanReporter`/`Renderer` indicatif renderer.
- `musefs-cli/src/lib.rs` — **modify.** `mod progress;`, wire `run_scan` (build reporter, pass sink, elapsed summaries, finish).
- `musefs-cli/tests/scan.rs` — **modify.** Smoke integration tests for the progress-enabled paths.
- `README.md` — **modify.** Note progress behaviour + `--quiet`.

> **Note on commit granularity:** The spec floated splitting the core work into 1a/1b. This plan keeps all `scan.rs` changes in **one** commit (Task 1) so the `.cargo/mutants.toml` re-anchor — which requires a `cargo mutants --list` run (minutes) — happens **once** instead of twice. The reviewer flagged the split as optional; consolidating is the lower-churn choice given the anchor gate.

---

## Task 1: Core progress events + wiring (`musefs-core`)

**Files:**
- Modify: `musefs-core/src/scan.rs`
- Modify: `musefs-core/src/lib.rs`
- Modify: `.cargo/mutants.toml`

This task compiles only once all of steps 2–7 land, so the new tests are written first (they fail to compile = "fail"), then the implementation, then one green commit.

- [ ] **Step 1: Write the failing core tests**

Add to the `scan_unit_tests` module in `musefs-core/src/scan.rs` (the module that already defines `flac_block` / `streaminfo` / `write_flac` helpers). These use the same minimal-FLAC bytes pattern as the existing `jobs1_and_jobs_n_produce_equivalent_state` test.

```rust
    #[test]
    fn scan_options_debug_includes_progress_sink() {
        let opts = ScanOptions {
            progress: Some(ProgressSink::new(|_| {})),
            ..Default::default()
        };
        assert!(format!("{opts:?}").contains("ProgressSink"));
    }

    #[test]
    fn scan_emits_discovered_walked_ingested_events() {
        use std::sync::Mutex;
        let dir = tempfile::tempdir().unwrap();
        for i in 0..5 {
            let mut bytes = b"fLaC".to_vec();
            bytes.push(0x80);
            bytes.extend_from_slice(&[0, 0, 34]);
            bytes.extend(std::iter::repeat_n(0u8, 34));
            bytes.extend_from_slice(format!("AUDIO-{i}").as_bytes());
            std::fs::write(dir.path().join(format!("t{i}.flac")), &bytes).unwrap();
        }

        let events = Arc::new(Mutex::new(Vec::<String>::new()));
        let recorder = Arc::clone(&events);
        let sink = ProgressSink::new(move |ev| {
            let line = match ev {
                ScanProgress::Discovered { found } => format!("disc:{found}"),
                ScanProgress::Walked { total } => format!("walk:{total}"),
                ScanProgress::Ingested { done, total, .. } => format!("ing:{done}/{total}"),
            };
            recorder.lock().unwrap().push(line);
        });

        let db = Db::open_in_memory().unwrap();
        let opts = ScanOptions {
            jobs: 1,
            progress: Some(sink),
            ..Default::default()
        };
        let stats = scan_directory_with(&db, dir.path(), &opts).unwrap();
        assert_eq!(stats.scanned, 5);

        let ev = events.lock().unwrap();
        // Discovery climbs to the full count.
        assert!(ev.iter().any(|e| e == "disc:5"), "events: {ev:?}");
        // Walk reports the total to ingest.
        assert!(ev.contains(&"walk:5".to_string()), "events: {ev:?}");
        // Ingest reports each committed file, done strictly 1..=total.
        let ing: Vec<&String> = ev.iter().filter(|e| e.starts_with("ing:")).collect();
        assert_eq!(
            ing,
            vec!["ing:1/5", "ing:2/5", "ing:3/5", "ing:4/5", "ing:5/5"],
        );
    }
```

- [ ] **Step 2: Run the tests to confirm they fail (don't compile)**

Run: `cargo test -p musefs-core --lib scan_emits_discovered_walked_ingested_events 2>&1 | tail -20`
Expected: compile error — `cannot find type ScanProgress` / no field `progress` on `ScanOptions`.

- [ ] **Step 3: Add the `ScanProgress` enum and `ProgressSink` type**

In `musefs-core/src/scan.rs`, add two module-level imports next to the existing `use` block at the top (`use std::sync::mpsc::sync_channel;` is already there):

```rust
use std::fmt;
use std::sync::Arc;
```

Then **remove** the now-redundant function-local `use std::sync::Arc;` line inside `run_pipeline` (leaving its `use std::sync::atomic::{AtomicU64, Ordering};`). A nested redundant import trips `unused_imports` under `-D warnings`.

Add the types just above the `ScanStats` struct:

```rust
/// A progress event emitted during a scan or revalidate. Borrows the current
/// path to avoid a per-file allocation in the writer; the saved allocation is
/// negligible next to the existing per-file `to_string_lossy` + DB write, so do
/// not contort the API to preserve the borrow.
#[derive(Debug, Clone, Copy)]
pub enum ScanProgress<'a> {
    /// A supported-audio file was found during the walk; `found` is the running
    /// count of collected files.
    Discovered { found: u64 },
    /// The walk (and, for revalidate, the skip-unchanged pass) finished;
    /// `total` files will be ingested and tracked by the determinate bar.
    Walked { total: u64 },
    /// A file was committed. `done` runs 1..=total; `path` is its absolute path.
    Ingested { done: u64, total: u64, path: &'a str },
}

/// UI-agnostic progress callback for [`ScanOptions`]. Invoked only from the
/// caller's thread (the walk and the single writer), never from probe workers.
/// The `Send + Sync` bound is not required by today's code; it is deliberate
/// future-proofing and free here (`indicatif::ProgressBar` is `Send + Sync`).
#[derive(Clone)]
pub struct ProgressSink(Arc<dyn for<'a> Fn(ScanProgress<'a>) + Send + Sync>);

impl ProgressSink {
    pub fn new(f: impl for<'a> Fn(ScanProgress<'a>) + Send + Sync + 'static) -> Self {
        ProgressSink(Arc::new(f))
    }

    fn emit(&self, ev: ScanProgress<'_>) {
        (self.0)(ev);
    }
}

impl fmt::Debug for ProgressSink {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ProgressSink")
    }
}
```

- [ ] **Step 4: Add the `progress` field to `ScanOptions`**

In the `ScanOptions` struct, add the field after `follow_symlinks`:

```rust
    /// Optional progress callback. `None` (the default) disables reporting.
    pub progress: Option<ProgressSink>,
```

In `impl Default for ScanOptions`, add `progress: None,` after `follow_symlinks: false,`.

- [ ] **Step 5: Thread discovery progress through the walk**

Replace `collect_audio` with a thin wrapper that delegates to a new `collect_audio_with` carrying the sink (keeps the existing 3-arg signature so the ~9 other call sites and tests stay unchanged):

```rust
fn collect_audio(
    root: &Path,
    out: &mut Vec<PathBuf>,
    follow_symlinks: bool,
) -> std::io::Result<SkipTally> {
    collect_audio_with(root, out, follow_symlinks, None)
}

fn collect_audio_with(
    root: &Path,
    out: &mut Vec<PathBuf>,
    follow_symlinks: bool,
    progress: Option<&ProgressSink>,
) -> std::io::Result<SkipTally> {
    let mut visited = HashSet::new();
    let mut files_visited = HashSet::new();
    let mut tally = SkipTally::default();
    if follow_symlinks {
        if let Ok(meta) = std::fs::metadata(root) {
            visited.insert(dir_key(&meta));
        }
    }
    collect_audio_inner(
        root,
        out,
        follow_symlinks,
        &mut visited,
        &mut files_visited,
        &mut tally,
        progress,
    )?;
    Ok(tally)
}
```

Add a trailing `progress: Option<&ProgressSink>` parameter to `collect_audio_inner` and `descend`, and forward it at every recursive call and `push_file` call inside them (each existing call gets `, progress` appended). `descend`'s two `collect_audio_inner(...)` calls and `collect_audio_inner`'s `descend(...)` / `push_file(...)` calls all gain the argument.

Update `push_file` to accept the sink and emit `Discovered` after each successful push:

```rust
fn push_file(
    path: &Path,
    out: &mut Vec<PathBuf>,
    follow_symlinks: bool,
    files_visited: &mut HashSet<(u64, u64)>,
    known_meta: Option<&std::fs::Metadata>,
    progress: Option<&ProgressSink>,
) {
    if !follow_symlinks {
        out.push(path.to_path_buf());
        if let Some(p) = progress {
            p.emit(ScanProgress::Discovered { found: out.len() as u64 });
        }
        return;
    }
    let key = match known_meta {
        Some(m) => Some(dir_key(m)),
        None => std::fs::metadata(path).ok().map(|m| dir_key(&m)),
    };
    match key {
        Some(k) if !files_visited.insert(k) => {
            log::debug!("skipping duplicate backing target {}", path.display());
        }
        _ => {
            out.push(path.to_path_buf());
            if let Some(p) = progress {
                p.emit(ScanProgress::Discovered { found: out.len() as u64 });
            }
        }
    }
}
```

- [ ] **Step 6: Emit `Walked` in the callers and `Ingested` in the pipeline**

In `scan_directory_with`: pass the sink into the walk and emit `Walked` before `run_pipeline`. Change the directory branch from `collect_audio(root, &mut files, opts.follow_symlinks)?` to `collect_audio_with(root, &mut files, opts.follow_symlinks, opts.progress.as_ref())?`, then after the `if root.is_file() { … } else { … }` block and before `db.apply_bulk_pragmas_self()?`:

```rust
    if let Some(p) = &opts.progress {
        p.emit(ScanProgress::Walked { total: files.len() as u64 });
    }
```

In `revalidate_with`: change `collect_audio(root, &mut files, opts.follow_symlinks)?` to `collect_audio_with(root, &mut files, opts.follow_symlinks, opts.progress.as_ref())?`, and immediately before `let scan = run_pipeline(db, changed, opts)?;` add:

```rust
    if let Some(p) = &opts.progress {
        p.emit(ScanProgress::Walked { total: changed.len() as u64 });
    }
```

In `run_pipeline`: capture the total and the sink before `files` is moved into the work queue. Right after `let jobs = effective_jobs(opts.jobs);`:

```rust
    let total = files.len() as u64;
    let progress = opts.progress.as_ref();
```

Then inside the `flush` closure's per-unit `for` loop, immediately after `*scanned += 1;`, add the emit (`abs_path` is still in scope — `ingest_bulk` borrows it, does not move it):

```rust
            if let Some(p) = progress {
                p.emit(ScanProgress::Ingested {
                    done: *scanned,
                    total,
                    path: &abs_path,
                });
            }
```

The closure already captures by reference; `progress` (`Option<&ProgressSink>`, `Copy`) and `total` (`u64`, `Copy`) are captured automatically.

- [ ] **Step 7: Re-export the new types**

In `musefs-core/src/lib.rs`, extend the `pub use scan::{ … }` block to include `ProgressSink` and `ScanProgress`:

```rust
pub use scan::{
    ProgressSink, RevalidateStats, ScanOptions, ScanProgress, ScanStats, revalidate,
    revalidate_with, scan_directory, scan_directory_with,
};
```

- [ ] **Step 8: Run the core tests to confirm they pass**

Run: `cargo test -p musefs-core --lib scan 2>&1 | tail -20`
Expected: the two new tests pass and no existing `scan` test regresses. (rtk may summarise the output as `cargo test: N passed`; trust the exit code.)

- [ ] **Step 9: Re-anchor `.cargo/mutants.toml`**

The edits shift the `scan.rs` `file:line:col` anchors (run_pipeline `+=`/`>=`/`||`/`*`, revalidate `+=`). Re-anchor in this commit:

```bash
cargo mutants --no-config --list --json > /tmp/scan-mutants.json
python3 scripts/check_mutant_anchors.py --fix --mutants-json /tmp/scan-mutants.json
python3 scripts/check_mutant_anchors.py --mutants-json /tmp/scan-mutants.json
```

The final command must print `OK: … entries validated`. `--fix` only re-anchors covering-set clusters; for any remaining failure it reports, open `.cargo/mutants.toml`, find the entry by its `# guard:` tag (e.g. `op="+=" fn="run_pipeline" rows=2`), and replace the `file:line:col` with the new coordinates from `/tmp/scan-mutants.json` (match the same op/function). Re-run until `OK`.

> Do not add new exclusions here. The new `*scanned`/`done`/`found`/`total` counters are killed by the Step 1 event test; only the pre-existing anchored entries move.

- [ ] **Step 10: Format and lint**

Run: `cargo fmt -p musefs-core && cargo clippy -p musefs-core --all-targets 2>&1 | tail -20`
Expected: no diff from fmt, no clippy warnings.

- [ ] **Step 11: Commit**

```bash
git add musefs-core/src/scan.rs musefs-core/src/lib.rs .cargo/mutants.toml
git commit -m "feat(core): emit scan/revalidate progress events via ScanOptions sink (#406)"
```
(The pre-commit hook runs the full workspace suite + the mutant-anchor guard; both must pass.)

---

## Task 2: CLI rendering (`musefs-cli`)

**Files:**
- Modify: `musefs-cli/Cargo.toml`
- Create: `musefs-cli/src/progress.rs`
- Modify: `musefs-cli/src/lib.rs`
- Test: `musefs-cli/src/progress.rs` (unit), `musefs-cli/tests/scan.rs` (integration)

- [ ] **Step 1: Add the `indicatif` dependency**

In `musefs-cli/Cargo.toml`, under `[dependencies]`, add:

```toml
indicatif = { version = "0.17", default-features = false }
```

Run `cargo build -p musefs-cli` to confirm it resolves. If the workspace MSRV rejects this `indicatif`, pin the latest version that builds and note it.

- [ ] **Step 2: Write `next_milestone` and its failing unit tests**

Create `musefs-cli/src/progress.rs` with the pure decision function and tests (implementation filled in the same step so the file compiles; the tests are the verification gate):

```rust
//! Progress rendering for `musefs scan` / `--revalidate`. The decision logic
//! (`next_milestone`) is split out as a pure function so the mutation gate can
//! reach it; the indicatif rendering itself is unobservable side-effect I/O.

const STEP: u64 = 5; // milestone granularity, percent

/// The milestone percent (a multiple of `STEP`, or 100) that `done` newly
/// reaches relative to `prev_done`, else `None`. `total == 0` yields `None`
/// (no divide-by-zero); reaching `total` always yields `Some(100)` so the
/// final line prints even when the last step is smaller than `STEP`.
pub(crate) fn next_milestone(prev_done: u64, done: u64, total: u64) -> Option<u64> {
    if total == 0 || done <= prev_done {
        return None;
    }
    if done >= total {
        return Some(100);
    }
    let bucket = done * 100 / total / STEP;
    let prev_bucket = prev_done * 100 / total / STEP;
    if bucket > prev_bucket {
        Some(bucket * STEP)
    } else {
        None
    }
}

#[cfg(test)]
mod milestone_tests {
    use super::next_milestone;

    #[test]
    fn zero_total_is_none() {
        assert_eq!(next_milestone(0, 0, 0), None);
    }

    #[test]
    fn no_advance_is_none() {
        assert_eq!(next_milestone(3, 3, 100), None);
    }

    #[test]
    fn single_file_is_hundred() {
        assert_eq!(next_milestone(0, 1, 1), Some(100));
    }

    #[test]
    fn final_step_below_granularity_still_fires() {
        // 30 files: 29 -> 30 is < 5% but completion must always report.
        assert_eq!(next_milestone(29, 30, 30), Some(100));
    }

    #[test]
    fn crossing_first_five_percent() {
        assert_eq!(next_milestone(0, 1, 20), Some(5));
    }

    #[test]
    fn within_a_bucket_is_none() {
        assert_eq!(next_milestone(5, 6, 100), None);
    }

    #[test]
    fn crossing_into_ten_percent() {
        assert_eq!(next_milestone(9, 10, 100), Some(10));
    }
}
```

Add `mod progress;` to `musefs-cli/src/lib.rs` (near the top, with the other module declarations).

- [ ] **Step 3: Run the unit tests to confirm they pass**

Run: `cargo test -p musefs-cli --lib milestone 2>&1 | tail -20`
Expected: all 7 `milestone_tests` pass.

- [ ] **Step 4: Implement the `ScanReporter` renderer**

Append to `musefs-cli/src/progress.rs`:

```rust
use std::io::IsTerminal;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use indicatif::{ProgressBar, ProgressStyle};
use musefs_core::{ProgressSink, ScanProgress};

enum Mode {
    /// `--quiet`: no progress at all.
    Quiet,
    /// stderr is a TTY: animated spinner that becomes a determinate bar.
    Tty(ProgressBar),
    /// stderr is not a TTY: throttled `N/M` lines.
    Plain,
}

struct Renderer {
    mode: Mode,
    /// Last `done` for which a `Plain` line was printed; reset per target.
    prev_done: AtomicU64,
}

impl Renderer {
    fn handle(&self, ev: ScanProgress<'_>) {
        match (&self.mode, ev) {
            (Mode::Tty(bar), ScanProgress::Discovered { found }) => {
                bar.set_message(format!("discovering files… {found} found"));
            }
            (Mode::Tty(bar), ScanProgress::Walked { total }) => {
                bar.set_style(
                    ProgressStyle::with_template(
                        "{spinner} [{elapsed_precise}] [{bar:30}] {pos}/{len} ({percent}%) {wide_msg}",
                    )
                    .expect("static template")
                    .progress_chars("##-"),
                );
                bar.set_length(total);
                bar.set_position(0);
            }
            (Mode::Tty(bar), ScanProgress::Ingested { done, path, .. }) => {
                bar.set_position(done);
                bar.set_message(basename(path));
            }
            (Mode::Plain, ScanProgress::Ingested { done, total, .. }) => {
                let prev = self.prev_done.load(Ordering::Relaxed);
                if let Some(pct) = next_milestone(prev, done, total) {
                    eprintln!("ingested {done}/{total} ({pct}%)");
                    self.prev_done.store(done, Ordering::Relaxed);
                }
            }
            // Quiet ignores everything; Plain ignores Discovered/Walked
            // (per-target reset happens in `start_target`).
            _ => {}
        }
    }
}

fn basename(path: &str) -> String {
    Path::new(path)
        .file_name()
        .map_or_else(|| path.to_string(), |n| n.to_string_lossy().into_owned())
}

/// Owns the progress renderer for a scan run. Build once, hand `sink()` to
/// `ScanOptions`, then call `finish()` after the last target.
pub(crate) struct ScanReporter {
    inner: Arc<Renderer>,
}

impl ScanReporter {
    pub(crate) fn new(quiet: bool) -> Self {
        let mode = if quiet {
            Mode::Quiet
        } else if std::io::stderr().is_terminal() {
            let bar = ProgressBar::new_spinner();
            bar.enable_steady_tick(Duration::from_millis(120));
            bar.set_message("discovering files…");
            Mode::Tty(bar)
        } else {
            Mode::Plain
        };
        ScanReporter {
            inner: Arc::new(Renderer { mode, prev_done: AtomicU64::new(0) }),
        }
    }

    /// `None` under `--quiet`, so `ScanOptions.progress` stays unset.
    pub(crate) fn sink(&self) -> Option<ProgressSink> {
        if matches!(self.inner.mode, Mode::Quiet) {
            return None;
        }
        let inner = Arc::clone(&self.inner);
        Some(ProgressSink::new(move |ev| inner.handle(ev)))
    }

    /// Reset for the next target in a multi-target run: rewind the TTY bar back
    /// to the discovery spinner (target N-1 left it as a finished determinate
    /// bar) and clear the Plain milestone watermark. Call at the top of each
    /// target iteration, including the first (a no-op there).
    pub(crate) fn start_target(&self) {
        self.inner.prev_done.store(0, Ordering::Relaxed);
        if let Mode::Tty(bar) = &self.inner.mode {
            bar.set_style(ProgressStyle::default_spinner());
            bar.set_position(0);
            bar.set_message("discovering files…");
        }
    }

    pub(crate) fn finish(&self) {
        if let Mode::Tty(bar) = &self.inner.mode {
            bar.finish_and_clear();
        }
    }
}
```

- [ ] **Step 5: Wire `run_scan`**

In `musefs-cli/src/lib.rs`, add imports near the existing `use` lines:

```rust
use std::time::Instant;

use indicatif::HumanDuration;

use crate::progress::ScanReporter;
```

Rewrite `run_scan` to build the reporter, thread the sink, time each target, append elapsed to both summaries, and finish:

```rust
pub fn run_scan(
    db_path: &Path,
    targets: &[PathBuf],
    revalidate: bool,
    jobs: usize,
    follow_symlinks: bool,
    quiet: bool,
) -> Result<()> {
    let db =
        Db::open(db_path).with_context(|| format!("opening database at {}", db_path.display()))?;
    let reporter = ScanReporter::new(quiet);
    let opts = musefs_core::ScanOptions {
        jobs,
        follow_symlinks,
        progress: reporter.sink(),
        ..Default::default()
    };
    for target in targets {
        reporter.start_target();
        let start = Instant::now();
        if revalidate {
            let stats = musefs_core::revalidate_with(&db, target, &opts)
                .with_context(|| format!("revalidating {}", target.display()))?;
            if !quiet {
                println!(
                    "revalidated {}: {} updated, {} unchanged, {} pruned, {} failed in {}",
                    target.display(),
                    stats.updated,
                    stats.unchanged,
                    stats.pruned,
                    stats.failed,
                    HumanDuration(start.elapsed()),
                );
            }
        } else {
            let stats = musefs_core::scan_directory_with(&db, target, &opts)
                .with_context(|| format!("scanning {}", target.display()))?;
            if !quiet {
                println!(
                    "scanned {}: {} file(s), skipped {}, failed {} in {}",
                    target.display(),
                    stats.scanned,
                    stats.skipped,
                    stats.failed,
                    HumanDuration(start.elapsed()),
                );
            }
        }
    }
    reporter.finish();
    Ok(())
}
```

- [ ] **Step 6: Add integration smoke tests**

`musefs-cli/tests/scan.rs` runs `run_scan` in-process under `cargo test`, where stderr is captured (non-TTY → the `Plain` printer path). In-process stderr text cannot be captured without extra deps, so these assert that progress wiring does not break ingest across a ≥20-file library (the printed-line *decision* is already covered by `next_milestone` units; TTY rendering is manual — see Task 4). Append:

```rust
fn write_n_flacs(dir: &std::path::Path, n: usize) {
    for i in 0..n {
        // make_flac takes `&[&str]`; bind the owned String first so the slice
        // element is `&str` (a `&String` element does not coerce).
        let title = format!("TITLE=T{i}");
        std::fs::write(
            dir.join(format!("t{i:02}.flac")),
            make_flac(&[title.as_str()], &[0xAB; 32]),
        )
        .unwrap();
    }
}

#[test]
fn scan_with_progress_ingests_all_files() {
    let backing = tempfile::tempdir().unwrap();
    write_n_flacs(backing.path(), 20);
    let db_dir = tempfile::tempdir().unwrap();
    let db_path = db_dir.path().join("musefs.db");

    // quiet = false exercises the Plain milestone printer.
    run_scan(&db_path, &[backing.path().to_path_buf()], false, 0, false, false).unwrap();

    let db = musefs_db::Db::open(&db_path).unwrap();
    assert_eq!(db.list_tracks().unwrap().len(), 20);
}

#[test]
fn quiet_scan_still_ingests_all_files() {
    let backing = tempfile::tempdir().unwrap();
    write_n_flacs(backing.path(), 20);
    let db_dir = tempfile::tempdir().unwrap();
    let db_path = db_dir.path().join("musefs.db");

    // quiet = true: sink is None; ingest must be identical.
    run_scan(&db_path, &[backing.path().to_path_buf()], false, 0, false, true).unwrap();

    let db = musefs_db::Db::open(&db_path).unwrap();
    assert_eq!(db.list_tracks().unwrap().len(), 20);
}

#[test]
fn revalidate_with_progress_reports_unchanged() {
    let backing = tempfile::tempdir().unwrap();
    write_n_flacs(backing.path(), 20);
    let db_dir = tempfile::tempdir().unwrap();
    let db_path = db_dir.path().join("musefs.db");

    run_scan(&db_path, &[backing.path().to_path_buf()], false, 0, false, false).unwrap();
    // Second pass as revalidate: all unchanged, no panic, rows preserved.
    run_scan(&db_path, &[backing.path().to_path_buf()], true, 0, false, false).unwrap();

    let db = musefs_db::Db::open(&db_path).unwrap();
    assert_eq!(db.list_tracks().unwrap().len(), 20);
}
```

- [ ] **Step 7: Run CLI tests, lint, format**

Run: `cargo test -p musefs-cli 2>&1 | tail -20`
Expected: all pass (existing + 7 milestone + 3 integration).

Run: `cargo clippy -p musefs-cli --all-targets 2>&1 | tail -20 && cargo fmt -p musefs-cli`
Expected: no clippy warnings, no fmt diff.

- [ ] **Step 8: Commit**

```bash
git add musefs-cli/Cargo.toml Cargo.lock musefs-cli/src/progress.rs musefs-cli/src/lib.rs musefs-cli/tests/scan.rs
git commit -m "feat(cli): render scan/revalidate progress with indicatif + elapsed summary (#406)"
```

---

## Task 3: Documentation (`README.md`)

**Files:**
- Modify: `README.md`

- [ ] **Step 1: Document the progress behaviour**

In the `scan` usage section of `README.md`, add a short paragraph near the `--quiet` flag:

> `scan` and `scan --revalidate` show a live progress indicator: on an interactive
> terminal, a discovery spinner followed by a determinate bar (position, percent,
> ETA, current file); on a non-interactive stderr (piped or logged), throttled
> `ingested N/M (P%)` lines. `--quiet` (`-q`) suppresses the progress indicator
> and the per-target summary. Each summary line ends with the elapsed time.

(Match the exact heading/anchor style already used in `README.md` for the scan flags.)

- [ ] **Step 2: Commit**

```bash
git add README.md
git commit -m "docs: describe scan progress indicator and --quiet (#406)"
```

(Docs-only commit: the pre-commit cargo gate is skipped.)

---

## Task 4: Whole-feature verification

**Files:** none (verification only).

- [ ] **Step 1: Full workspace suite + metrics feature**

Run: `cargo test --workspace 2>&1 | tail -15`
Expected: all green.

Run: `cargo test -p musefs-core --features metrics 2>&1 | tail -15`
Expected: green. (Scan changes do not touch the getattr/read counters, but this is the CI `check` job's feature set — confirm no surprise.)

- [ ] **Step 2: Mutation gate on the diff**

Per the local in-diff gate procedure (mutants run `--in-place`, serial, default `/tmp` TMPDIR), run the in-diff mutation gate against this branch's diff. Sanity-check that the diff actually contains the new `scan.rs`/`progress.rs` lines (a silent empty diff is a false pass).

- `next_milestone` mutants must be **killed** by `milestone_tests`; the core counter mutants by `scan_emits_discovered_walked_ingested_events`.
- The indicatif `Renderer::handle` rendering arms are unobservable side-effect I/O (same class as the existing `log_mp4_oversize_drops` exclusion). For any survivors there, add a documented `exclude_re` entry to `.cargo/mutants.toml` with a `# guard:` tag explaining the unobservability — do **not** add tests that assert on terminal output. Re-run the anchor guard (`python3 scripts/check_mutant_anchors.py`) after editing the toml, and fold the change into the relevant commit (amend Task 1's commit if pre-PR, else a follow-up `test:` commit).

- [ ] **Step 3: Manual TTY verification (no automated coverage)**

The indicatif TTY branch is unreachable under `cargo test`. Verify by hand on a real terminal against a real library subdirectory (e.g. one album under `/data/media/music`; mount is not needed for scan):

```bash
cargo run -p musefs -- scan --db /tmp/progress-check.db /data/media/music/<one-album>
```

Confirm:
- the discovery spinner animates; on `Walked` it switches to a determinate bar; the bar shows percent / ETA / current file basename; `log::warn` lines (if any) do not permanently corrupt the bar; the bar clears cleanly at the end; the summary prints with `in …` elapsed.
- **Multi-target** (`scan … <album-a> <album-b>`): the bar rewinds to a discovery spinner at the start of the *second* target (not stuck at target 1's finished determinate bar), then re-lengths for target 2's ingest.
- **Empty library** (`scan …` on a directory with no supported audio): the spinner clears with no hung bar, and the summary still prints `0 file(s)`.
- Repeat once with `--revalidate` (re-run against the same DB) and once with `--quiet` (no spinner, no summary).
- `… | cat` (non-TTY): prints `ingested N/M (P%)` lines instead of a bar.

- [ ] **Step 4: Finish the development branch**

Once all tasks are green and the manual check passes, use the `superpowers:finishing-a-development-branch` skill to choose how to integrate (PR to `main`, etc.). Reference issue #406 in the PR.

---

## Self-Review Notes

- **Spec coverage:** discovery spinner (Task 1 push_file `Discovered` + Task 2 Tty arm), determinate bar (Task 2 Walked/Ingested arms), non-TTY lines (Task 2 Plain + `next_milestone`), `--quiet` (Task 2 `Mode::Quiet` + `sink()` → None), elapsed summary (Task 2 run_scan), revalidate coverage (Task 1 Walked in `revalidate_with`, shared `run_pipeline` Ingested), `total == 0` guard (`next_milestone` + Tty `set_length(0)` finishes), single-file target (`Walked { total: 1 }` via the `is_file` branch path). All present.
- **Type consistency:** `ProgressSink::new` / `emit`, `ScanProgress::{Discovered,Walked,Ingested}` field names, and `next_milestone(prev_done, done, total) -> Option<u64>` are used identically across core wiring, the renderer, and tests.
- **Casts:** `len() as u64` (usize→u64 widening, clippy-clean, matches existing `data.len() as u64` in scan.rs); `next_milestone` stays in `u64` end-to-end to avoid a narrowing cast.
- **No placeholders:** every code step contains complete code; the only "fill-in" is matching README's existing anchor style (Task 3) and adding mutation exclusions *iff* survivors appear (Task 4), which cannot be enumerated before running the gate.
