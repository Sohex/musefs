# Phase 2 — Plugin correctness batch (#83–87)

*Spec date: 2026-06-03. Roadmap phase: "Phase 2 — Plugin correctness batch".*

## Goal

Make the beets and Picard plugins write the SQLite store contract faithfully and
consistently. Five correctness bugs are fixed as one batch: a beets reconciliation
hook that can mask bugs and hang, Picard truncating multi-value tags, Picard
mangling comma-bearing field-map values, beets duplicating the `genre` tag, and
both plugins spawning one `musefs scan` process per file.

The cardinal project invariant is untouched: no plugin rewrites audio. All writes
go to the SQLite store; a live mount surfaces them via `data_version` polling.

## Scope

**In scope:** issues #83, #84, #85, #86, #87. This requires:

- Python changes in `contrib/beets/beetsplug/` and `contrib/picard/musefs/`.
- One small change to the shared `python-musefs` library (`musefs_common`): a
  multi-target `run_scan` and a shared `SCAN_TIMEOUT_SECONDS` constant.
- One small Rust change: the `musefs scan` CLI accepts multiple positional paths.
- One new cross-plugin contract test in `python-musefs`.
- Re-vendor `python-musefs` into Picard (drift-guard test enforces freshness).

**Out of scope:** #98 (schema duplicated as hand-maintained `schema.sql`) and all
other open Rust-track issues (#67–94, #71/#76). These are independent and tracked
separately in the roadmap.

## Context: where the code lives (post-#99)

PR #99 extracted the host-agnostic surface into `musefs_common`; each plugin keeps
its own `_core.py` for host-specific tag mapping (beets `Item` vs Picard
`Metadata`). The relevant symbols:

- `musefs_common/scan.py::run_scan(binary, db_path, target, *, timeout=None)` —
  single-target shell-out to `musefs scan`; raises `ScanError`
  (`kind ∈ {not_found, timeout, failed}`).
- `musefs_common/constants.py` — shared constants mirrored against the Rust schema.
- `beets/beetsplug/_core.py` — `DIRECT_FIELDS`, `_values`, `map_fields`,
  `build_records`.
- `beets/beetsplug/musefs.py` — `MusefsPlugin`, including `_reconcile_pending`
  (`cli_exit` listener) and `_run_scan`.
- `picard/musefs/_core.py` — `DIRECT_FIELDS`, `_first_value`, `map_fields`,
  `parse_field_map`, `front_cover`, `resolve_config`, `SCAN_TIMEOUT_SECONDS`.
- `picard/musefs/__init__.py` — the GUI entry point; loops `run_scan` per file.

## Changes by issue

### #83 — beets reconciliation hook: narrow the catch, add a scan timeout

`beets/beetsplug/musefs.py`.

**Finding (issue premise is stale).** The issue states the reconciliation `except`
"catches only `ui.UserError`". The current `_reconcile_pending` (musefs.py:103–124)
already catches a blanket `except Exception` and degrades to `self._log.warning`.
That was broadened in PR #97. The current state is the inverse problem: it swallows
*every* exception — including programming bugs (a `TypeError`, a bad attribute) —
into a one-line warning, so real defects hide indefinitely.

**Note on the scan timeout:** the missing timeout is **beets-only**. Picard already
passes `timeout=SCAN_TIMEOUT_SECONDS` (`picard/musefs/__init__.py:126`); only beets'
`_run_scan` passes `timeout=None`. The shared-constant move below deduplicates the
constant so beets can import it — it does not imply Picard was missing a timeout.

**Fix:**

1. **Narrow** the blanket `except Exception` to the expected environmental/expected
   failures only: `(ui.UserError, sqlite3.Error, OSError, subprocess.SubprocessError)`.
   These are "the environment went wrong, not the user's fault mid-import" cases — a
   locked DB, a vanished file, a wedged scan. They degrade to `self._log.warning`
   and never abort beets. Any *other* exception propagates, surfacing real bugs
   loudly at `cli_exit` (the import has already committed by then, so propagation
   does not corrupt the user's library; it just produces a visible traceback).
2. **Add the missing scan timeout.** `_run_scan` currently calls
   `run_scan(binary, db_path, target, timeout=None)`. Pass
   `timeout=SCAN_TIMEOUT_SECONDS` (the shared constant — see cross-cutting). A
   wedged `musefs scan` then raises `ScanError(kind="timeout")`, which `_run_scan`
   already translates to `ui.UserError`; in the reconcile path that is caught by
   (1) and becomes a warning, and in the explicit `beet musefs` command path it
   aborts that command as before (correct — the user asked for it directly).

This is a breadth fix, not a visibility fix: `self._log.warning` is already visible
at beets' default log level. The point is to stop masking bugs while still never
aborting an import for an environmental failure.

### #84 — Picard multi-value tags

`picard/musefs/_core.py`.

`_first_value(metadata, field)` returns only the first non-empty value from
`metadata.getall(...)`, so multiple artists/genres/composers collapse to one. beets
already expands these into multiple `tags` rows (`_values`), which `musefs-core`
synthesizes into proper multi-value frames.

Picard's multi-value source differs from beets': Picard exposes multiple values
under a *single* metadata key via `getall(field)` (there is no `genres`/`composers`
twin key), whereas beets carries list fields. So the fix is to expand `getall`
results, not to add twin keys.

`map_fields` iterates `DIRECT_FIELDS` uniformly, and that table carries
`tracknumber`/`discnumber`/`date` inline. A blind `_first_value`→`_values` swap would
wrongly multi-expand `date` (it is neither in `_NUMERIC_KEYS` nor genuinely
multi-valued). The guard must therefore be an explicit **multi-value allowlist**, not
the numeric denylist.

**Fix:**

1. Add a `_values(metadata, field)` helper mirroring `_first_value` but returning the
   full list of non-empty, stripped values (via `getall`, with the same `.get`
   fallback).
2. Define a multi-value-eligible key set — `{"artist", "albumartist", "genre",
   "composer"}`. In `map_fields`, expand those keys via `_values` (one tag row per
   value); every other key (`title`, `tracknumber`, `discnumber`, `date`) stays
   scalar via `_first_value`. `_NUMERIC_KEYS` still applies its zero-skip to the
   scalar numerics. The front cover is unaffected.

### #85 — Picard field-map parsing splits on commas

`picard/musefs/_core.py`.

`parse_field_map` does `str(text).replace("\n", ",").split(",")`, so a value
containing a comma (`comment=This is a great, upbeat song`) is split and mangled.

**Fix:** parse **newline-separated** entries only — one `key=value` per line. Commas
become literal within values. The Picard options page field-map widget is a
multi-line text area, so newline-per-entry is the natural input model. Update the
`parse_field_map` docstring and the options-page help text to say "one `key=value`
per line". Blank lines and lines without `=` are still skipped.

### #86 — beets list/scalar twins both map to one store key

`beets/beetsplug/_core.py`.

`DIRECT_FIELDS` (lines 15–24) maps **two** scalar/list twin pairs to a single store
key: `genre`+`genres` → `genre`, and `composer`+`composers` → `composer`. The issue
names only `genre`/`genres`, but `composer`/`composers` is the identical bug on the
same `map_fields`/`_values` code path — an item carrying both halves of a pair emits
duplicate/overlapping rows. The fix covers **both** pairs (the issue is broadened to
its true extent; recorded here rather than split to a new issue, since it is the same
code and the same test).

**Fix:** for each twin pair, emit the store key once. Prefer the list form (`genres`,
`composers`) when non-empty; otherwise fall back to the scalar (`genre`, `composer`).
Dedupe the resulting values while preserving order. Concretely, remove the ambiguous
double-mappings from `DIRECT_FIELDS` and merge each pair's two sources before
expanding through `_values`. No field outside these two pairs is affected.

### #87 — both plugins spawn one `musefs scan` per file

Rust CLI + `musefs_common/scan.py` + both plugins.

**Finding (issue premise is stale).** The issue says "the beets plugin passes all
targets to a single scan invocation." It does not: `_run_scan` (musefs.py:144–153)
loops `run_scan(...)` per target, and Picard's `__init__.py` loops per file. The
Rust `scan` CLI only accepts a single `backing_dir` positional
(`musefs-cli/src/lib.rs`). So both plugins spawn one process (clap init, DB open,
version check, teardown) per file. Batching is a shared concern.

**Fix:**

1. **Rust CLI** (`musefs-cli/src/lib.rs`): change the `Scan` subcommand's
   `backing_dir: PathBuf` to `targets: Vec<PathBuf>` with clap `num_args = 1..`.
   The command opens/migrates the DB once and scans each target in turn (each
   target may be a file or a directory, preserving current single-target and
   recursive-directory semantics). The single-path form `musefs scan <dir>` keeps
   working unchanged. Update the dispatch arm and the doc comment. `--revalidate`
   and `--jobs` keep their meaning and apply across the whole target list
   (revalidate runs per target; `--jobs` is unchanged). **Failure semantics:**
   scan targets in order and fail fast — the first failing target aborts the batch
   with a non-zero exit naming that target. Targets already scanned before the
   failure stay committed; this is safe because ingest is an idempotent upsert (a
   re-run re-processes them), and it matches today's per-file behavior where earlier
   files were already committed before a later one raised.
2. **Shared `run_scan`** (`musefs_common/scan.py`): accept multiple targets and
   pass them all to one `subprocess.run` invocation, building argv as
   `[binary, "scan", *targets, "--db", db_path]` — **all positional targets before
   the `--db` flag** (do not interleave). Keep a single-target call ergonomic
   (accept either one path or a list). The single `timeout` covers the whole batch.
   `ScanError` carries the full `targets` list as its context; the plugins'
   message formatters (`_scan_user_error` in beets, `_scan_error` text in Picard)
   render "N file(s)" rather than a single path.
3. **Both plugins**: collect the full selection and call `run_scan` once. beets'
   `_run_scan` passes its `targets` list in one call; Picard's `__init__.py`
   collects all selected files' paths and calls once.

## Cross-cutting

### Shared `SCAN_TIMEOUT_SECONDS`

Move `SCAN_TIMEOUT_SECONDS` out of `picard/musefs/_core.py` into `musefs_common`
(alongside the other shared constants) so both plugins use one value. beets imports
it for #83; Picard imports it from the shared lib instead of defining its own.
Re-vendor `python-musefs` into Picard afterward (the drift-guard test enforces
freshness).

### Cross-plugin contract test

Add a test in `python-musefs` (or a shared test fixture both plugin suites can use)
asserting that beets and Picard produce **identical normalized tag rows** for an
equivalent input — multiple artists, multiple genres, multiple composers, and a
comma-bearing comment. This guards #84/#85/#86 against future divergence: the two
`map_fields` implementations must agree on multi-value expansion and value
preservation. Because the source objects differ (beets `Item` vs Picard `Metadata`),
the test builds an equivalent input for each host and compares the emitted
`(key, value)` rows.

**Normalization oracle (so pass/fail is unambiguous):** compare the emitted rows as a
**multiset of `(key, value)` pairs**, except for `artist` and `albumartist`, whose
positional order is meaningful (primary artist first) and is compared **as an ordered
list**. `genre` and `composer` are compared order-insensitively (set semantics). This
mirrors how `musefs-core` consumes the rows. The test must build inputs that exercise
a key whose order matters (≥2 artists) and keys that do not (≥2 genres, ≥2 composers).

## Testing

- **#83:** unit test that `_reconcile_pending` swallows a simulated
  `sqlite3.OperationalError`/`OSError` as a warning and does not raise; unit test
  that an unexpected type *outside* the caught tuple (e.g. raise `ValueError`)
  *does* propagate; unit test that beets' `_run_scan` passes
  `timeout=SCAN_TIMEOUT_SECONDS`.
- **#84:** `test_map_fields` asserts a multi-value Picard field (≥2 `artist`,
  ≥2 `genre`) emits multiple rows, **and** that `date` with a comma-like or
  multi-token value stays a single scalar row (the allowlist guard).
- **#85:** `parse_field_map` test with a comma-bearing value preserved intact; a
  multi-line map parsed into multiple entries.
- **#86:** beets `test_map_fields` with both `genre`+`genres` **and** both
  `composer`+`composers` set emits one deduped, order-preserving set of rows for
  each key.
- **#87:** Rust CLI test that `scan a b c` parses multiple paths and that the
  single-path form still parses; a core test that a multi-target scan ingests all
  targets under one DB open; a test of fail-fast on a bad target. Python tests that
  each plugin calls `run_scan` once for a multi-file selection and that the built
  argv places all targets before `--db`.
- **Contract:** the new cross-plugin equivalence test.
- **Full gate:** re-vendor Picard; run all three pytest suites + `ruff check` +
  `ruff format --check`; `cargo test`, `cargo clippy --all-targets`, `cargo fmt
  --all --check`.

## Risks

- **Multi-path scan regression.** The Rust change must preserve single-path usage
  and recursive-directory ingestion. Covered by keeping the single-target test and
  adding a multi-target test.
- **Over-broadening vs over-narrowing #83.** The named exception tuple must cover
  the real environmental failures (locked DB, missing file, wedged/timed-out scan)
  without catching `TypeError`/`AttributeError`-class bugs. `subprocess` timeout
  surfaces as `ScanError` → `ui.UserError`, which is in the caught set.
- **Contract test brittleness.** It must compare *normalized* rows (order-insensitive
  where the store treats values as a set, order-sensitive where order is meaningful),
  matching how `musefs-core` consumes the rows.
