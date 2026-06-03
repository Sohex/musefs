# Phase 2 — Plugin Correctness Batch Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Fix five plugin-correctness bugs (#83–87) so the beets and Picard plugins write the SQLite store contract faithfully, consistently, and efficiently.

**Architecture:** Most fixes are plugin-local edits to each plugin's own `_core.py`/entry point. Two changes are shared: a multi-target `run_scan` and a relocated `SCAN_TIMEOUT_SECONDS` in `musefs_common` (re-vendored into Picard). One small Rust change makes `musefs scan` accept multiple paths so batching is real. A new shared `contract.py` plus parallel tests in both plugin suites guard the mappers against future divergence.

**Tech Stack:** Python 3.13 (beets/Picard plugins + `python-musefs` shared lib), pytest, ruff; Rust (clap CLI in `musefs-cli`), cargo.

**Spec:** `docs/superpowers/specs/2026-06-03-phase2-plugin-correctness-design.md`

---

## File Structure

**Shared library (`contrib/python-musefs/`):**
- Modify `src/musefs_common/constants.py` — add `SCAN_TIMEOUT_SECONDS`.
- Modify `src/musefs_common/__init__.py` — export `SCAN_TIMEOUT_SECONDS`.
- Modify `src/musefs_common/scan.py` — `run_scan` accepts one path or a list.
- Create `src/musefs_common/contract.py` — canonical tag-row contract + `normalize_rows`.
- Modify `tests/test_scan.py`, `tests/test_constants.py`, `tests/test_public_api.py`.

**Rust CLI (`musefs-cli/`):**
- Modify `src/lib.rs` — `Scan` takes `targets: Vec<PathBuf>`; `run_scan` loops; tests.

**beets plugin (`contrib/beets/`):**
- Modify `beetsplug/_core.py` — collapse `genre`/`genres` and `composer`/`composers` twins (#86).
- Modify `beetsplug/musefs.py` — narrow `_reconcile_pending` catch + pass timeout, batch scan (#83/#87).
- Modify `tests/test_map_fields.py`; create `tests/test_reconcile.py`, `tests/test_contract.py`.

**Picard plugin (`contrib/picard/`):**
- Modify `musefs/_core.py` — multi-value allowlist (#84), newline-only field map (#85), drop local `SCAN_TIMEOUT_SECONDS`.
- Modify `musefs/__init__.py` — import shared `SCAN_TIMEOUT_SECONDS`, batch scan (#87).
- Re-vendor `musefs/_common/` (the drift-guard test enforces freshness).
- Modify `tests/test_map_fields.py`; create `tests/test_parse_field_map.py`, `tests/test_batch_scan.py`, `tests/test_contract.py`.

**Refinement vs spec (recorded for traceability):** the spec's contract-test note describes `artist`/`albumartist` as ordered multi-value. beets has *no* multi-artist field (`DIRECT_FIELDS` lacks `artists`/`albumartists`), so beets emits a single `artist` row. The cross-plugin contract therefore covers the genuinely-shared multi-value fields — `genre` and `composer` — with `artist`/`albumartist` single-valued in the contract input. Picard's multi-artist expansion (its native model via `getall`) is real and is tested in Picard's own unit tests, not the cross-plugin contract.

---

## Test commands (reference)

```bash
# python-musefs (self-contained, pythonpath=src):
cd contrib/python-musefs && python -m pytest && ruff check . && ruff format --check .

# Re-vendor after any musefs_common change, from the repo root:
python contrib/python-musefs/vendor_to_picard.py

# beets (install local lib first):
cd contrib/beets && pip install -e ../python-musefs && pip install -e ".[test]" && python -m pytest tests

# Picard (vendored; no install):
cd contrib/picard && python -m pytest tests

# Rust:
cargo test -p musefs-cli && cargo clippy --all-targets && cargo fmt --all --check
```

---

## Task 1: Relocate `SCAN_TIMEOUT_SECONDS` to the shared library

**Files:**
- Modify: `contrib/python-musefs/src/musefs_common/constants.py`
- Modify: `contrib/python-musefs/src/musefs_common/__init__.py`
- Modify: `contrib/python-musefs/tests/test_constants.py`
- Modify: `contrib/python-musefs/tests/test_public_api.py`

- [ ] **Step 1: Write the failing test** — append to `contrib/python-musefs/tests/test_constants.py`:

```python
def test_scan_timeout_seconds_present():
    from musefs_common import SCAN_TIMEOUT_SECONDS
    from musefs_common.constants import SCAN_TIMEOUT_SECONDS as direct

    assert SCAN_TIMEOUT_SECONDS == direct == 120
```

- [ ] **Step 2: Run it to verify it fails**

Run: `cd contrib/python-musefs && python -m pytest tests/test_constants.py::test_scan_timeout_seconds_present -v`
Expected: FAIL with `ImportError: cannot import name 'SCAN_TIMEOUT_SECONDS'`.

- [ ] **Step 3: Add the constant.** In `constants.py`, append:

```python
# Wall-clock cap (seconds) for a single `musefs scan` shell-out; a wedged scan
# (stuck disk, DB lock) raises ScanError(kind="timeout") rather than hanging.
SCAN_TIMEOUT_SECONDS = 120
```

- [ ] **Step 4: Export it.** In `__init__.py`, change the constants import and `__all__`:

```python
from .constants import EXPECTED_USER_VERSION, MAX_ART_BYTES, SCAN_TIMEOUT_SECONDS
```

Add `"SCAN_TIMEOUT_SECONDS",` to the `__all__` list (next to `"MAX_ART_BYTES",`).

- [ ] **Step 5: Assert the public-API test still passes.** `tests/test_public_api.py` likely checks `__all__`. If it asserts an exact membership set, add `"SCAN_TIMEOUT_SECONDS"` to that expected set. Run:

Run: `cd contrib/python-musefs && python -m pytest tests/test_public_api.py -v`
Expected: PASS (update the expected set first if it fails on the new name).

- [ ] **Step 6: Run the new test to verify it passes**

Run: `cd contrib/python-musefs && python -m pytest tests/test_constants.py -v`
Expected: PASS.

- [ ] **Step 7: Re-vendor into Picard and verify the drift-guard**

Run: `python contrib/python-musefs/vendor_to_picard.py && cd contrib/picard && python -m pytest tests/test_vendor_sync.py -v`
Expected: PASS (vendored copy now matches).

- [ ] **Step 8: Commit**

```bash
git add contrib/python-musefs/src/musefs_common/constants.py \
        contrib/python-musefs/src/musefs_common/__init__.py \
        contrib/python-musefs/tests/test_constants.py \
        contrib/python-musefs/tests/test_public_api.py \
        contrib/picard/musefs/_common/
git commit -m "Add shared SCAN_TIMEOUT_SECONDS constant to musefs_common (#83)"
```

---

## Task 2: Multi-target `run_scan` in the shared library

**Files:**
- Modify: `contrib/python-musefs/src/musefs_common/scan.py`
- Modify: `contrib/python-musefs/tests/test_scan.py`

The current `run_scan(binary, db_path, target, *, timeout=None)` builds argv `[binary, "scan", target, "--db", db_path]`. It must accept either one path or a list of paths and place all targets **before** `--db`.

- [ ] **Step 1: Write the failing tests** — append to `contrib/python-musefs/tests/test_scan.py`:

```python
def test_run_scan_multiple_targets_one_invocation(monkeypatch):
    import subprocess

    import musefs_common.scan as scan

    calls = []

    class FakeResult:
        returncode = 0
        stderr = b""

    def fake_run(argv, **kwargs):
        calls.append(argv)
        return FakeResult()

    monkeypatch.setattr(subprocess, "run", fake_run)
    scan.run_scan("musefs", "/db.sqlite", ["/a.flac", "/b.flac"])

    assert len(calls) == 1
    argv = calls[0]
    assert argv == ["musefs", "scan", "/a.flac", "/b.flac", "--db", "/db.sqlite"]
    # All targets precede the --db flag.
    assert argv.index("/b.flac") < argv.index("--db")


def test_run_scan_single_path_still_works(monkeypatch):
    import subprocess

    import musefs_common.scan as scan

    seen = {}

    class FakeResult:
        returncode = 0
        stderr = b""

    monkeypatch.setattr(subprocess, "run", lambda argv, **kw: seen.update(argv=argv) or FakeResult())
    scan.run_scan("musefs", "/db.sqlite", "/only.flac")
    assert seen["argv"] == ["musefs", "scan", "/only.flac", "--db", "/db.sqlite"]


def test_run_scan_failed_batch_error_names_count(monkeypatch):
    import subprocess

    import musefs_common.scan as scan
    from musefs_common import ScanError

    class FakeResult:
        returncode = 2
        stderr = b"boom"

    monkeypatch.setattr(subprocess, "run", lambda argv, **kw: FakeResult())
    try:
        scan.run_scan("musefs", "/db.sqlite", ["/a.flac", "/b.flac"])
    except ScanError as exc:
        assert exc.kind == "failed"
        assert "2" in str(exc.target)  # "2 target(s)"
    else:
        raise AssertionError("expected ScanError")
```

- [ ] **Step 2: Run them to verify they fail**

Run: `cd contrib/python-musefs && python -m pytest tests/test_scan.py -k "multiple_targets or single_path_still or failed_batch" -v`
Expected: FAIL (single-target argv built; passing a list breaks the current single-string argv).

- [ ] **Step 3: Rewrite `run_scan`.** Replace the body of `run_scan` in `scan.py` with:

```python
def run_scan(binary, db_path, target, *, timeout=None):
    """Run ``<binary> scan <target...> --db <db_path>``. ``target`` is a single
    path or an iterable of paths; all targets precede the ``--db`` flag and are
    scanned under one process (one DB open). Creates the DB if absent and fills
    the structural columns a plugin can't compute. Raises ``ScanError`` (with
    ``kind`` in ``"not_found" | "timeout" | "failed"``) on failure; the caller
    formats its own user-facing message from the exception attributes."""
    if isinstance(target, (str, os.PathLike)):
        targets = [target]
    else:
        targets = list(target)
    display = str(targets[0]) if len(targets) == 1 else f"{len(targets)} target(s)"
    argv = [binary, "scan", *(str(t) for t in targets), "--db", str(db_path)]
    try:
        result = subprocess.run(argv, capture_output=True, timeout=timeout)
    except FileNotFoundError as exc:
        raise ScanError("not_found", binary=binary, target=display) from exc
    except subprocess.TimeoutExpired as exc:
        raise ScanError("timeout", binary=binary, target=display, timeout=timeout) from exc
    if result.returncode != 0:
        raise ScanError(
            "failed",
            binary=binary,
            target=display,
            returncode=result.returncode,
            stderr=result.stderr.decode(errors="replace").strip(),
        )
```

Add `import os` at the top of `scan.py` (next to `import subprocess`) if not already present.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cd contrib/python-musefs && python -m pytest tests/test_scan.py -v`
Expected: PASS (including the pre-existing single-target tests).

- [ ] **Step 5: Re-vendor and verify the drift-guard**

Run: `python contrib/python-musefs/vendor_to_picard.py && cd contrib/picard && python -m pytest tests/test_vendor_sync.py -v`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add contrib/python-musefs/src/musefs_common/scan.py \
        contrib/python-musefs/tests/test_scan.py \
        contrib/picard/musefs/_common/
git commit -m "run_scan accepts multiple targets in one invocation (#87)"
```

---

## Task 3: Multi-path `musefs scan` CLI (Rust)

**Files:**
- Modify: `musefs-cli/src/lib.rs` (the `Scan` subcommand at lines 42–56, `run_scan` at 95–118, the dispatch arm at 185–190, and the test module at 217–231)

- [ ] **Step 1: Write the failing test** — add to the `tests` module in `lib.rs` (after `scan_command_parses_jobs_flag`):

```rust
    #[test]
    fn scan_command_parses_multiple_paths() {
        use clap::Parser;
        let cli =
            Cli::try_parse_from(["musefs", "scan", "/a", "/b", "/c", "--db", "/tmp/x.db"]).unwrap();
        match cli.command {
            Command::Scan { targets, .. } => {
                assert_eq!(targets, vec![PathBuf::from("/a"), PathBuf::from("/b"), PathBuf::from("/c")]);
            }
            Command::Mount { .. } => panic!("expected Scan"),
        }
    }
```

Also update the existing `scan_command_parses_jobs_flag` to match the new field name — change its match arm from `Command::Scan { jobs, .. }` (unchanged) but note the single-path invocation `["musefs", "scan", "/m", ...]` must still parse; add an assertion:

```rust
        match cli.command {
            Command::Scan { jobs, targets, .. } => {
                assert_eq!(jobs, 3);
                assert_eq!(targets, vec![PathBuf::from("/m")]);
            }
            Command::Mount { .. } => panic!("expected Scan"),
        }
```

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo test -p musefs-cli scan_command 2>&1 | tail -20`
Expected: FAIL to compile — `Scan` has no field `targets` (it has `backing_dir`).

- [ ] **Step 3: Change the `Scan` subcommand field.** In `lib.rs`, replace:

```rust
        /// Directory of backing audio files to scan recursively.
        backing_dir: PathBuf,
```

with:

```rust
        /// One or more files or directories to scan (directories recurse).
        #[arg(required = true, num_args = 1..)]
        targets: Vec<PathBuf>,
```

- [ ] **Step 4: Rewrite `run_scan` to loop over targets.** Replace the `run_scan` function body with:

```rust
/// Open (creating/migrating) the DB at `db_path` once, then scan each target in
/// `targets` (a file or a directory; directories recurse). With `revalidate`,
/// run the maintenance pass (skip-unchanged, prune, GC) instead of a full
/// ingest. Fails fast: the first failing target aborts the batch; targets
/// already scanned stay committed (ingest is an idempotent upsert).
pub fn run_scan(db_path: &Path, targets: &[PathBuf], revalidate: bool, jobs: usize) -> Result<()> {
    let db =
        Db::open(db_path).with_context(|| format!("opening database at {}", db_path.display()))?;
    let opts = musefs_core::ScanOptions { jobs };
    for target in targets {
        if revalidate {
            let stats = musefs_core::revalidate_with(&db, target, &opts)
                .with_context(|| format!("revalidating {}", target.display()))?;
            println!(
                "revalidated {}: {} updated, {} unchanged, {} pruned, {} failed",
                target.display(),
                stats.updated,
                stats.unchanged,
                stats.pruned,
                stats.failed
            );
        } else {
            let stats = musefs_core::scan_directory_with(&db, target, &opts)
                .with_context(|| format!("scanning {}", target.display()))?;
            println!(
                "scanned {}: {} file(s), skipped {}, failed {}",
                target.display(),
                stats.scanned,
                stats.skipped,
                stats.failed
            );
        }
    }
    Ok(())
}
```

- [ ] **Step 5: Update the dispatch arm.** In `run`, replace:

```rust
        Command::Scan {
            backing_dir,
            db,
            revalidate,
            jobs,
        } => run_scan(&db, &backing_dir, revalidate, jobs),
```

with:

```rust
        Command::Scan {
            targets,
            db,
            revalidate,
            jobs,
        } => run_scan(&db, &targets, revalidate, jobs),
```

- [ ] **Step 6: Run the tests to verify they pass**

Run: `cargo test -p musefs-cli 2>&1 | tail -20`
Expected: PASS.

- [ ] **Step 7: Lint and format**

Run: `cargo clippy -p musefs-cli --all-targets 2>&1 | tail -5 && cargo fmt --all --check`
Expected: no warnings; fmt clean (exit 0).

- [ ] **Step 8: Commit**

```bash
git add musefs-cli/src/lib.rs
git commit -m "musefs scan accepts multiple target paths (#87)"
```

---

## Task 4: beets — narrow the reconcile catch + pass the scan timeout (#83, batches via Task 2)

**Files:**
- Modify: `contrib/beets/beetsplug/musefs.py` (imports at lines 1–18, `_reconcile_pending` at 103–124, `_run_scan` at 144–153)
- Create: `contrib/beets/tests/test_reconcile.py`

- [ ] **Step 1: Write the failing tests** — create `contrib/beets/tests/test_reconcile.py`:

```python
import sqlite3
from types import SimpleNamespace

import pytest

pytest.importorskip("beets")

from beetsplug import musefs as musefs_mod  # noqa: E402
from beetsplug.musefs import MusefsPlugin  # noqa: E402


class FakeLog:
    def __init__(self):
        self.warnings = []

    def warning(self, msg, *args):
        self.warnings.append((msg, args))


def _plugin(monkeypatch, *, sync_raises=None):
    """A MusefsPlugin with __init__ bypassed and its collaborators stubbed."""
    plugin = MusefsPlugin.__new__(MusefsPlugin)
    plugin._log = FakeLog()
    plugin._pending = [SimpleNamespace(path=b"/music/a.flac")]
    plugin._db_path = lambda: "/db.sqlite"
    plugin._autoscan = lambda: False
    plugin._prune_missing = lambda db: None

    def _sync(db, items):
        if sync_raises is not None:
            raise sync_raises

    plugin._sync = _sync
    return plugin


def test_reconcile_swallows_db_error_as_warning(monkeypatch):
    plugin = _plugin(monkeypatch, sync_raises=sqlite3.OperationalError("database is locked"))
    plugin._reconcile_pending()  # must NOT raise
    assert len(plugin._log.warnings) == 1


def test_reconcile_swallows_os_error_as_warning(monkeypatch):
    plugin = _plugin(monkeypatch, sync_raises=OSError("disk gone"))
    plugin._reconcile_pending()
    assert len(plugin._log.warnings) == 1


def test_reconcile_propagates_unexpected_error(monkeypatch):
    plugin = _plugin(monkeypatch, sync_raises=ValueError("a real bug"))
    with pytest.raises(ValueError):
        plugin._reconcile_pending()


def test_run_scan_passes_shared_timeout(monkeypatch):
    captured = {}

    def fake_run_scan(binary, db_path, targets, *, timeout=None):
        captured["targets"] = targets
        captured["timeout"] = timeout

    monkeypatch.setattr(musefs_mod, "run_scan", fake_run_scan)
    plugin = MusefsPlugin.__new__(MusefsPlugin)
    plugin._bin = lambda: "musefs"
    plugin._run_scan("/db.sqlite", ["/a.flac", "/b.flac"])

    assert captured["targets"] == ["/a.flac", "/b.flac"]  # one call, full list
    assert captured["timeout"] == musefs_mod.SCAN_TIMEOUT_SECONDS == 120
```

- [ ] **Step 2: Run them to verify they fail**

Run: `cd contrib/beets && python -m pytest tests/test_reconcile.py -v`
Expected: FAIL — `test_run_scan_passes_shared_timeout` fails (`run_scan` called per-target with `timeout=None`, and `musefs_mod.SCAN_TIMEOUT_SECONDS` does not exist); the propagate test fails (blanket `except Exception` swallows `ValueError`).

- [ ] **Step 3: Add the import.** In `musefs.py`, extend the `from musefs_common import (...)` block (lines 7–18) to include the constant, and add the stdlib imports. After `import os` (line 3) add:

```python
import sqlite3
import subprocess
```

In the `from musefs_common import (` block, add `SCAN_TIMEOUT_SECONDS,` (keep alphabetical-ish ordering; place it before `ScanError`):

```python
from musefs_common import (
    SCAN_TIMEOUT_SECONDS,
    ScanError,
    SchemaMismatch,
    SyncStats,
    check_schema_version,
    connect,
    prune_missing,
    realpath_key,
    run_scan,
    sync_files,
    track_id_for_path,
)
```

- [ ] **Step 4: Narrow the reconcile catch.** In `_reconcile_pending`, replace:

```python
        except Exception as exc:
            # A passive cli_exit hook must never abort the beets operation, so any
            # failure (ui.UserError, a sqlite3 error, etc.) degrades to a warning.
            self._log.warning("musefs: {}", exc)
```

with:

```python
        except (ui.UserError, sqlite3.Error, OSError, subprocess.SubprocessError) as exc:
            # A passive cli_exit hook must never abort the beets operation for an
            # environmental failure (locked DB, vanished file, wedged scan); those
            # degrade to a warning. An unexpected exception still propagates so a
            # real bug surfaces instead of hiding behind a one-line warning.
            self._log.warning("musefs: {}", exc)
```

- [ ] **Step 5: Batch the scan and pass the timeout.** Replace the body of `_run_scan`:

```python
    def _run_scan(self, db_path, targets):
        """Run `musefs scan <target...> --db <db>` once for the whole batch.
        Creates the DB if missing and fills the structural columns the plugin
        can't compute itself. Raises ui.UserError on failure."""
        binary = self._bin()
        try:
            run_scan(binary, db_path, targets, timeout=SCAN_TIMEOUT_SECONDS)
        except ScanError as exc:
            raise self._scan_user_error(exc)
```

- [ ] **Step 6: Run the tests to verify they pass**

Run: `cd contrib/beets && python -m pytest tests/test_reconcile.py -v`
Expected: PASS.

- [ ] **Step 7: Run the full beets suite (no regressions)**

Run: `cd contrib/beets && python -m pytest tests`
Expected: PASS.

- [ ] **Step 8: Commit**

```bash
git add contrib/beets/beetsplug/musefs.py contrib/beets/tests/test_reconcile.py
git commit -m "beets: narrow reconcile catch, pass scan timeout, batch scan (#83, #87)"
```

---

## Task 5: beets — collapse genre/composer list/scalar twins (#86)

**Files:**
- Modify: `contrib/beets/beetsplug/_core.py` (`DIRECT_FIELDS` at 15–24, `map_fields` at 57–82)
- Modify: `contrib/beets/tests/test_map_fields.py`

- [ ] **Step 1: Write the failing test** — append to `contrib/beets/tests/test_map_fields.py`:

```python
def test_genre_and_genres_deduped_prefer_list():
    # Both the scalar and list set: prefer the list, dedupe, preserve order.
    pairs = map_fields(item(genre="Rock", genres=["Rock", "Indie"]))
    genres = [v for k, v in pairs if k == "genre"]
    assert genres == ["Rock", "Indie"]  # not ["Rock", "Rock", "Indie"]


def test_composer_and_composers_deduped_prefer_list():
    pairs = map_fields(item(composer="Bach", composers=["Bach", "Mozart"]))
    composers = [v for k, v in pairs if k == "composer"]
    assert composers == ["Bach", "Mozart"]


def test_genre_scalar_only_when_no_list():
    pairs = map_fields(item(genre="Jazz"))
    assert [v for k, v in pairs if k == "genre"] == ["Jazz"]


def test_genres_list_only_when_no_scalar():
    pairs = map_fields(item(genres=["Folk", "Pop"]))
    assert [v for k, v in pairs if k == "genre"] == ["Folk", "Pop"]
```

- [ ] **Step 2: Run them to verify they fail**

Run: `cd contrib/beets && python -m pytest tests/test_map_fields.py -k "deduped or scalar_only or list_only" -v`
Expected: FAIL — `test_genre_and_genres_deduped_prefer_list` yields `["Rock", "Rock", "Indie"]` (both sources emitted).

- [ ] **Step 3: Remove the twin entries from `DIRECT_FIELDS`.** Replace the `DIRECT_FIELDS` constant with:

```python
DIRECT_FIELDS = {
    "title": "title",
    "artist": "artist",
    "albumartist": "albumartist",
    "album": "album",
}

# (list_field, scalar_field, store_key): beets carries some tags as both a list
# (genres/composers, beets 2.x) and a joined scalar (genre/composer). Emitting
# both duplicates rows, so prefer the list when present, else the scalar.
TWIN_FIELDS = (
    ("genres", "genre", "genre"),
    ("composers", "composer", "composer"),
)
```

- [ ] **Step 4: Handle the twins in `map_fields`.** In `map_fields`, after the `for beets_field, key in fields.items():` loop (which now only iterates the four direct fields) and before the `track = ...` block, insert:

```python
    for list_field, scalar_field, key in TWIN_FIELDS:
        values = _values(getattr(item, list_field, None)) or _values(
            getattr(item, scalar_field, None)
        )
        seen = set()
        for text in values:
            if text not in seen:
                seen.add(text)
                pairs.append((key, text))
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cd contrib/beets && python -m pytest tests/test_map_fields.py -v`
Expected: PASS (including the pre-existing `test_empty_and_zero_omitted` — twins resolve to empty).

- [ ] **Step 6: Run the real-Item regression test (no regression on the existing multivalue path)**

Run: `cd contrib/beets && python -m pytest tests/test_plugin.py::test_map_fields_handles_real_beets_multivalue -v`
Expected: PASS (`genres`/`composers`-only Item still expands correctly).

- [ ] **Step 7: Commit**

```bash
git add contrib/beets/beetsplug/_core.py contrib/beets/tests/test_map_fields.py
git commit -m "beets: collapse genre/genres and composer/composers twins (#86)"
```

---

## Task 6: Picard — multi-value tag expansion via an allowlist (#84)

**Files:**
- Modify: `contrib/picard/musefs/_core.py` (`_first_value` at 48–62, `map_fields` at 65–84)
- Modify: `contrib/picard/tests/test_map_fields.py`

- [ ] **Step 1: Write/Update the failing tests** in `contrib/picard/tests/test_map_fields.py`. Replace the existing `test_first_value_of_multivalued_field` with:

```python
def test_multivalued_field_expands(fake_metadata):
    # Picard multi-valued: getall returns a list; multi-value-eligible keys
    # (artist/albumartist/genre/composer) emit one row per value.
    pairs = map_fields(fake_metadata(artist=["First", "Second"]))
    artists = [v for k, v in pairs if k == "artist"]
    assert artists == ["First", "Second"]


def test_genre_multivalue_expands(fake_metadata):
    pairs = map_fields(fake_metadata(genre=["Rock", "Pop"]))
    assert [v for k, v in pairs if k == "genre"] == ["Rock", "Pop"]


def test_date_not_multivalue_expanded(fake_metadata):
    # date is NOT in the multi-value allowlist: stays a single scalar row even
    # if Picard happens to expose multiple values.
    pairs = map_fields(fake_metadata(date=["2020", "2021"]))
    assert [v for k, v in pairs if k == "date"] == ["2020"]
```

- [ ] **Step 2: Run them to verify they fail**

Run: `cd contrib/picard && python -m pytest tests/test_map_fields.py -k "multivalue or expands" -v`
Expected: FAIL — `test_multivalued_field_expands` gets `["First"]` (current `_first_value` collapse).

- [ ] **Step 3: Add the `_values` helper and the allowlist.** In `_core.py`, after `_first_value` (line 62), insert:

```python
def _values(metadata, field_name):
    """All non-empty, stripped string values of a Picard metadata field
    (``getall`` when available, else a plain ``.get``)."""
    getall = getattr(metadata, "getall", None)
    if getall is not None:
        values = getall(field_name)
    else:
        v = metadata.get(field_name) if hasattr(metadata, "get") else None
        values = v if isinstance(v, (list, tuple)) else ([] if v is None else [v])
    return [text for v in values if (text := str(v).strip())]


# Keys whose Picard values may legitimately be multi-valued (one store row each).
# Everything else (title, tracknumber, discnumber, date) stays a single scalar.
_MULTI_VALUE_KEYS = {"artist", "albumartist", "genre", "composer"}
```

- [ ] **Step 4: Update `map_fields` to branch on the allowlist.** Replace everything from `pairs = []` through `return pairs` (the loop and the return) with:

```python
    pairs = []
    for pic_field, key in fields.items():
        if key in _MULTI_VALUE_KEYS:
            for text in _values(metadata, pic_field):
                pairs.append((key, text))
            continue
        text = _first_value(metadata, pic_field)
        if not text:
            continue
        if key in _NUMERIC_KEYS and _to_int(text) == 0:
            continue
        pairs.append((key, text))
    return pairs
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cd contrib/picard && python -m pytest tests/test_map_fields.py -v`
Expected: PASS (the zero-tracknumber and date scalar tests still hold).

- [ ] **Step 6: Commit**

```bash
git add contrib/picard/musefs/_core.py contrib/picard/tests/test_map_fields.py
git commit -m "picard: expand multi-value tags via an allowlist (#84)"
```

---

## Task 7: Picard — newline-only field-map parsing (#85)

**Files:**
- Modify: `contrib/picard/musefs/_core.py` (`parse_field_map` at 107–121)
- Create: `contrib/picard/tests/test_parse_field_map.py`

- [ ] **Step 1: Write the failing tests** — create `contrib/picard/tests/test_parse_field_map.py`:

```python
from musefs._core import parse_field_map


def test_value_with_comma_preserved():
    result = parse_field_map("comment=This is a great, upbeat song")
    assert result == {"comment": "This is a great, upbeat song"}


def test_multiple_lines_parsed():
    result = parse_field_map("comment=hello\ngrouping=My Set")
    assert result == {"comment": "hello", "grouping": "My Set"}


def test_blank_and_invalid_lines_skipped():
    result = parse_field_map("\ncomment=hi\nnot a mapping\n  \nkey=value\n")
    assert result == {"comment": "hi", "key": "value"}


def test_empty_text_returns_empty():
    assert parse_field_map("") == {}
    assert parse_field_map(None) == {}
```

- [ ] **Step 2: Run them to verify they fail**

Run: `cd contrib/picard && python -m pytest tests/test_parse_field_map.py -v`
Expected: FAIL — `test_value_with_comma_preserved` yields `{"comment": "This is a great"}` (comma split).

- [ ] **Step 3: Rewrite `parse_field_map` to split on newlines only.** Replace the function with:

```python
def parse_field_map(text):
    """Parse a ``key=value`` field map (from the options page) into a dict.
    One entry per line; blank lines and lines without ``=`` are ignored. A
    value may contain commas — they are kept literally."""
    result = {}
    if not text:
        return result
    for line in str(text).splitlines():
        line = line.strip()
        if not line or "=" not in line:
            continue
        k, v = line.split("=", 1)
        k, v = k.strip(), v.strip()
        if k and v:
            result[k] = v
    return result
```

- [ ] **Step 4: Update the options-page help text.** Find the field-map widget description in `contrib/picard/musefs/__init__.py` (the `MusefsOptionsPage` / `OPT_FIELDS` label or tooltip referencing "comma"). Change any "separated by commas or newlines" wording to "one `key=value` per line". If no such user-facing string mentions commas, skip this step.

Run: `grep -rn "comma\|key=value\|separated" contrib/picard/musefs/__init__.py`
Expected: locate any field-map help string; update it to "one `key=value` per line".

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cd contrib/picard && python -m pytest tests/test_parse_field_map.py -v`
Expected: PASS.

- [ ] **Step 6: Run the full Picard suite (catch any test that assumed comma-splitting)**

Run: `cd contrib/picard && python -m pytest tests`
Expected: PASS (update any pre-existing field-map test that relied on comma separation to use newlines).

- [ ] **Step 7: Commit**

```bash
git add contrib/picard/musefs/_core.py contrib/picard/tests/test_parse_field_map.py contrib/picard/musefs/__init__.py
git commit -m "picard: parse field map newline-only so commas survive in values (#85)"
```

---

## Task 8: Picard — use shared timeout + batch the autoscan (#87, #83 dedup)

**Files:**
- Modify: `contrib/picard/musefs/_core.py` (remove `SCAN_TIMEOUT_SECONDS` at line 15)
- Modify: `contrib/picard/musefs/__init__.py` (imports at 17–33, `_do_sync` at 117–149)
- Create: `contrib/picard/tests/test_batch_scan.py`

- [ ] **Step 1: Write the failing test** — create `contrib/picard/tests/test_batch_scan.py`:

```python
from types import SimpleNamespace

import musefs as plugin_mod  # the plugin package; its namespace is __init__.py's globals


def test_autoscan_batches_into_one_run_scan(monkeypatch, db_path):
    calls = []

    def fake_run_scan(binary, db, targets, *, timeout=None):
        calls.append((targets, timeout))

    # Stub the scan + write collaborators so the test isolates batching.
    monkeypatch.setattr(plugin_mod, "run_scan", fake_run_scan)
    monkeypatch.setattr(plugin_mod, "check_schema_version", lambda conn: None)
    monkeypatch.setattr(plugin_mod, "sync_files", lambda conn, records: SimpleNamespace())
    monkeypatch.setattr(plugin_mod, "map_fields", lambda md, fields: [])
    monkeypatch.setattr(plugin_mod, "front_cover", lambda md: None)

    opts = SimpleNamespace(db=db_path, bin="musefs", autoscan=True, fields={})
    files = {
        "/music/a.flac": SimpleNamespace(filename="/music/a.flac", metadata=object()),
        "/music/b.flac": SimpleNamespace(filename="/music/b.flac", metadata=object()),
    }
    plugin_mod._do_sync(opts, files)

    assert len(calls) == 1
    targets, timeout = calls[0]
    assert sorted(targets) == ["/music/a.flac", "/music/b.flac"]
    assert timeout == plugin_mod.SCAN_TIMEOUT_SECONDS == 120
```

- [ ] **Step 2: Run it to verify it fails**

Run: `cd contrib/picard && python -m pytest tests/test_batch_scan.py -v`
Expected: FAIL — `run_scan` is called once per file (len(calls) == 2).

- [ ] **Step 3: Remove the local constant.** In `_core.py`, delete the line:

```python
SCAN_TIMEOUT_SECONDS = 120
```

- [ ] **Step 4: Import the shared constant.** In `__init__.py`, move `SCAN_TIMEOUT_SECONDS` from the `from musefs._core import (...)` block into the `from musefs._common import (...)` block:

```python
from musefs._common import (
    Record,
    SCAN_TIMEOUT_SECONDS,
    ScanError,
    SchemaMismatch,
    check_schema_version,
    connect,
    realpath_key,
    run_scan,
    sync_files,
)
from musefs._core import (
    MusefsError,
    front_cover,
    map_fields,
    resolve_config,
)
```

- [ ] **Step 5: Batch the autoscan in `_do_sync`.** Replace the autoscan block:

```python
        if opts.autoscan:
            for f in files.values():
                try:
                    run_scan(opts.bin, opts.db, f.filename, timeout=SCAN_TIMEOUT_SECONDS)
                except ScanError as exc:
                    raise _scan_error(exc)
```

with:

```python
        if opts.autoscan:
            try:
                run_scan(
                    opts.bin,
                    opts.db,
                    [f.filename for f in files.values()],
                    timeout=SCAN_TIMEOUT_SECONDS,
                )
            except ScanError as exc:
                raise _scan_error(exc)
```

- [ ] **Step 6: Re-vendor (constant moved) and run the test**

Run: `python contrib/python-musefs/vendor_to_picard.py && cd contrib/picard && python -m pytest tests/test_batch_scan.py tests/test_vendor_sync.py -v`
Expected: PASS.

- [ ] **Step 7: Run the full Picard suite**

Run: `cd contrib/picard && python -m pytest tests`
Expected: PASS.

- [ ] **Step 8: Commit**

```bash
git add contrib/picard/musefs/_core.py contrib/picard/musefs/__init__.py \
        contrib/picard/tests/test_batch_scan.py contrib/picard/musefs/_common/
git commit -m "picard: batch autoscan into one run_scan, use shared timeout (#87)"
```

---

## Task 9: Cross-plugin tag-row contract

**Files:**
- Create: `contrib/python-musefs/src/musefs_common/contract.py`
- Create: `contrib/python-musefs/tests/test_contract.py`
- Create: `contrib/beets/tests/test_contract.py`
- Create: `contrib/picard/tests/test_contract.py`

- [ ] **Step 1: Create the shared contract module** — `contrib/python-musefs/src/musefs_common/contract.py`:

```python
"""Canonical tag-row contract both plugins must satisfy.

Each plugin's test builds an equivalent host object (a beets ``Item`` from the
list fields, a Picard ``Metadata`` from ``getall``) carrying ``CONTRACT_VALUES``
and asserts its ``map_fields`` output, normalized, equals
``normalize_rows(CONTRACT_EXPECTED)``. This guards #84/#86 against future
divergence between the two mappers.

Scope: the genuinely-shared multi-value fields (``genre``, ``composer``). beets
has no multi-artist field, so ``artist``/``albumartist`` are single-valued here;
Picard's multi-artist expansion is tested in its own unit tests.
"""

from collections import defaultdict

CONTRACT_VALUES = {
    "title": "Song",
    "artist": "Alice",
    "albumartist": "Alice",
    "album": "Greatest Hits",
    "genre": ["Rock", "Pop"],
    "composer": ["Carol", "Dave"],
}

CONTRACT_EXPECTED = [
    ("title", "Song"),
    ("artist", "Alice"),
    ("albumartist", "Alice"),
    ("album", "Greatest Hits"),
    ("genre", "Rock"),
    ("genre", "Pop"),
    ("composer", "Carol"),
    ("composer", "Dave"),
]


def normalize_rows(rows):
    """Group ``(key, value)`` rows by key into a comparison-stable dict. All
    contract keys use set semantics (the store treats multi-values as a set), so
    each key's values are returned sorted."""
    grouped = defaultdict(list)
    for key, value in rows:
        grouped[key].append(value)
    return {key: sorted(values) for key, values in grouped.items()}
```

- [ ] **Step 2: Self-test the contract module** — `contrib/python-musefs/tests/test_contract.py`:

```python
from musefs_common.contract import CONTRACT_EXPECTED, normalize_rows


def test_normalize_groups_and_sorts():
    norm = normalize_rows(CONTRACT_EXPECTED)
    assert norm["genre"] == ["Pop", "Rock"]
    assert norm["composer"] == ["Carol", "Dave"]
    assert norm["title"] == ["Song"]


def test_normalize_is_order_insensitive():
    shuffled = list(reversed(CONTRACT_EXPECTED))
    assert normalize_rows(shuffled) == normalize_rows(CONTRACT_EXPECTED)
```

- [ ] **Step 3: Run it, re-vendor, verify drift-guard**

Run: `cd contrib/python-musefs && python -m pytest tests/test_contract.py -v`
Expected: PASS.

Run: `python contrib/python-musefs/vendor_to_picard.py && cd contrib/picard && python -m pytest tests/test_vendor_sync.py -v`
Expected: PASS.

- [ ] **Step 4: Write the beets conformance test** — `contrib/beets/tests/test_contract.py`:

```python
from types import SimpleNamespace

from musefs_common.contract import CONTRACT_EXPECTED, CONTRACT_VALUES, normalize_rows

from beetsplug._core import map_fields


def _beets_item():
    # beets carries multi-value tags as the list fields genres/composers.
    return SimpleNamespace(
        title=CONTRACT_VALUES["title"],
        artist=CONTRACT_VALUES["artist"],
        albumartist=CONTRACT_VALUES["albumartist"],
        album=CONTRACT_VALUES["album"],
        genres=list(CONTRACT_VALUES["genre"]),
        composers=list(CONTRACT_VALUES["composer"]),
        genre="",
        composer="",
        track=0,
        disc=0,
        year=0,
        month=0,
        day=0,
    )


def test_beets_satisfies_contract():
    assert normalize_rows(map_fields(_beets_item())) == normalize_rows(CONTRACT_EXPECTED)
```

- [ ] **Step 5: Run it**

Run: `cd contrib/beets && python -m pytest tests/test_contract.py -v`
Expected: PASS.

- [ ] **Step 6: Write the Picard conformance test** — `contrib/picard/tests/test_contract.py`:

```python
from musefs._common.contract import CONTRACT_EXPECTED, CONTRACT_VALUES, normalize_rows

from musefs._core import map_fields


def test_picard_satisfies_contract(fake_metadata):
    # FakeMetadata wraps scalars to single-element lists; getall returns them.
    md = fake_metadata(**CONTRACT_VALUES)
    assert normalize_rows(map_fields(md)) == normalize_rows(CONTRACT_EXPECTED)
```

- [ ] **Step 7: Run it**

Run: `cd contrib/picard && python -m pytest tests/test_contract.py -v`
Expected: PASS.

- [ ] **Step 8: Commit**

```bash
git add contrib/python-musefs/src/musefs_common/contract.py \
        contrib/python-musefs/tests/test_contract.py \
        contrib/beets/tests/test_contract.py \
        contrib/picard/tests/test_contract.py \
        contrib/picard/musefs/_common/
git commit -m "Add cross-plugin tag-row contract test (#84, #86)"
```

---

## Task 10: Full verification gate

**Files:** none (verification only)

- [ ] **Step 1: Re-vendor once more (idempotent) and confirm no drift**

Run: `python contrib/python-musefs/vendor_to_picard.py && git status --porcelain contrib/picard/musefs/_common/`
Expected: empty output (already vendored in prior tasks; nothing to change).

- [ ] **Step 2: python-musefs — tests + lint**

Run: `cd contrib/python-musefs && python -m pytest && ruff check . && ruff format --check .`
Expected: all PASS / clean.

- [ ] **Step 3: beets — install local lib + tests**

Run: `cd contrib/beets && pip install -e ../python-musefs && pip install -e ".[test]" && python -m pytest tests`
Expected: PASS.

- [ ] **Step 4: Picard — tests**

Run: `cd contrib/picard && python -m pytest tests`
Expected: PASS.

- [ ] **Step 5: contrib lint/format across the plugins**

Run: `cd contrib && ruff check . && ruff format --check .`
Expected: clean (matches the project's `python-musefs` ruff pre-commit hook).

- [ ] **Step 6: Rust — build, test, clippy, fmt**

Run: `cargo build && cargo test && cargo clippy --all-targets && cargo fmt --all --check`
Expected: all PASS; clippy no warnings; fmt clean.

- [ ] **Step 7: Final commit if anything was adjusted**

```bash
git add -A
git commit -m "Phase 2 plugin-correctness batch: final verification (#83-87)"
```

(If nothing changed in this task, skip the commit.)
