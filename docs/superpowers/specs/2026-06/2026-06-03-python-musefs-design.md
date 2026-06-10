# python-musefs: a shared store-contract library for the beets and Picard plugins

**Status:** design
**Date:** 2026-06-03

## Problem

The beets plugin (`contrib/beets/beetsplug/_core.py`) and the Picard plugin
(`contrib/picard/musefs/_core.py`) each carry their own copy of the musefs
SQLite-store contract: ~150 near-identical lines covering schema-version
checking, the `tags`/`art`/`track_art` writes, art content-addressing, the
`realpath_key` path normalization, the `run_scan` shell-out to the `musefs`
binary, and the per-file sync write-loop. The two copies must change in
lockstep — the V1→V2 schema bump already forced parallel edits to both — and
their tests overlap heavily.

This is fragile (silent drift between the two mirrors of the Rust schema) and
blocks a third integration: any future host (another tagger, an MPD bridge, a
standalone sync tool) would mean a third hand-maintained copy.

## Goal

Extract the shared store-contract, scan, and write-loop logic into a single
publishable Python library, `python-musefs`, that becomes the **one** place the
SQLite contract is encoded. The two plugins become thin adapters over it. The
library is structured to be the base a third integration builds on.

This is a refactor with **no observable behavior change** to either plugin: same
sync/scan/prune semantics, same conditional-art-replacement and binary-tag
preservation rules, same `SyncStats` counts, and **identical user-facing error
strings**. The shared library raises its own exceptions (`ScanError`,
`SchemaMismatch`); each adapter catches them and re-raises its current
host-native error type with its existing message text verbatim — beets'
`_run_scan` raises `ui.UserError` with beets-specific wording today
(`musefs.py:144-152`), and that exact wording is restored in the adapter's
`ScanError` handler, not inherited from the library. The Rust crates and the
SQLite schema are untouched.

## Non-goals

- No mapping-toolkit abstraction. Field mapping stays per-host because the
  semantics genuinely differ (beets expands multi-valued `genres`/`composers`
  into one tag each; Picard takes the first non-empty value). Forcing both
  behind one `map_fields` would be a leaky abstraction.
- No behavior change to scanning, pruning, or art handling.
- No change to the Rust workspace, the CLI, or `schema.rs`.

## The library boundary

`python-musefs` owns everything that is mechanically identical across hosts:

1. **The store contract** — schema constants, version check, every write to
   `tags`/`art`/`track_art`, art content-addressing, `realpath_key`.
2. **The scan shell-out** — `run_scan`, unified with an optional timeout.
3. **The write-loop** — `sync_one` / `sync_files` over a host-agnostic `Record`.

Each host keeps only what is genuinely host-shaped: field mapping, art
acquisition (a file path for beets vs. embedded image bytes for Picard), the
options/config layer, and event/UI wiring.

## Package layout

A third contrib package, sibling to the two plugins:

```
contrib/python-musefs/
  pyproject.toml          # dist name: "python-musefs"; import package: musefs_common
  README.md
  src/musefs_common/
    __init__.py           # public API re-exports + __version__
    constants.py          # EXPECTED_USER_VERSION, MAX_ART_BYTES
    errors.py             # SchemaMismatch, ScanError
    paths.py              # realpath_key, _to_int
    store.py              # connect, check_schema_version, track_id_for_path,
                          #   prune_missing, replace_tags, upsert_art,
                          #   replace_track_art, sniff_mime, _EXT_MIME
    scan.py               # run_scan(binary, db, target, *, timeout=None)
    sync.py               # SyncStats, Record, sync_one, sync_files
  tests/                  # the canonical store/scan/sync suite
```

**Import package name: `musefs_common`** (distribution name `python-musefs`).
Not the bare `musefs`, which would collide with Picard's folder plugin (itself a
top-level `musefs` package) and with any future Python binding of the FUSE
binary.

**All intra-library imports are relative** (`from .errors import ScanError`,
`from .constants import MAX_ART_BYTES`). This is a hard requirement: it lets the
package be vendored under a different parent package name (see Picard delivery)
without rewriting any imports.

## Public API

```python
# constants
EXPECTED_USER_VERSION: int          # = 2 (mirrors schema.rs MIGRATIONS length)
MAX_ART_BYTES: int                  # 16 MiB - 64 KiB

# errors
class SchemaMismatch(Exception)     # DB user_version != EXPECTED_USER_VERSION
class ScanError(Exception)          # `musefs scan` missing / timed out / failed

# paths
def realpath_key(path) -> str       # canonical key matching scan's backing_path

# store
def connect(db_path)                # busy_timeout=5000, foreign_keys=ON
def check_schema_version(conn)      # raises SchemaMismatch
def track_id_for_path(conn, key) -> int | None
def prune_missing(conn, track_ids=None) -> int
def replace_tags(conn, track_id, pairs)        # scoped to value_blob IS NULL (#82)
def upsert_art(conn, data, mime) -> int        # sha256 content-addressed
def replace_track_art(conn, track_id, art_id)  # front cover, picture_type 3
def sniff_mime(data, path) -> str

# scan
def run_scan(binary, db_path, target, *, timeout=None)   # raises ScanError

# sync
@dataclass
class Record:
    key: str                        # realpath key
    pairs: list[tuple[str, str]]    # (musefs_key, value)
    art: tuple[bytes, str] | None   # (data, mime) or None

@dataclass
class SyncStats:
    synced: int = 0
    skipped: int = 0                # path had no matching track row
    art_linked: int = 0
    skipped_art: int = 0            # art oversized
    def summary(self) -> str

def sync_one(conn, record, stats, *, dry_run=False)
def sync_files(conn, records, *, dry_run=False, stats=None) -> SyncStats
                                    # stats: reuse a caller-seeded SyncStats
                                    # (beets pre-counts unreadable art); else fresh
```

### `sync_one` / `sync_files` semantics

`sync_one` is Picard's current `sync_one` generalized to take a `Record`. A
`Record` carries **already-resolved** art bytes (`art: (data, mime) | None`), so
all art *acquisition* — and the part of `skipped_art` that depends on
acquisition — lives in the adapter, not the library (see the per-host split
below). `sync_one`:

- Looks up `track_id` by `record.key`; if absent, `stats.skipped += 1`, return.
- Tags are **always fully replaced** (scoped to `value_blob IS NULL`, so
  scanner-written binary tags survive — #82).
- Art is **conditionally replaced**: only when `record.art` is present and
  `len(data) <= MAX_ART_BYTES`. An over-cap image bumps `skipped_art` and leaves
  any scan-seeded `track_art` untouched; no art means no change to art.
- Under `dry_run`, no writes occur but `synced` / `art_linked` / over-cap
  `skipped_art` are counted as if they had.

`sync_files(conn, records, *, dry_run)` constructs a fresh `SyncStats`, calls
`sync_one` for each record, returns the stats. The caller owns the transaction
(commit on success, rollback for dry runs) — matching both adapters today.

**`skipped_art` is split for beets, and that is intentional.** Today beets'
`_prepare_art` (`_core.py:273-281`) counts `skipped_art` for *both* an
unreadable art file *and* an over-cap one. After extraction, the **adapter**
reads the art file (preserving its per-run path→bytes cache that dedups one
album cover across its tracks) and counts the *unreadable* case into its own
`SyncStats` before calling `sync_files`; the **library** counts the *over-cap*
case inside `sync_one`. The sum is identical to today's count — but the plan
must keep the adapter and library writing into the *same* `SyncStats` instance
(adapter builds it, passes records, library increments it) so the total is
correct. beets' previous dry-run "would link" sentinel (`_WOULD_LINK`)
disappears: `sync_files` handles over-cap dry-run counting; the adapter handles
unreadable dry-run counting.

Because of this, `sync_files` must accept an optional pre-seeded `stats`
argument (default: fresh) so beets can pass the `SyncStats` it has already
incremented: `sync_files(conn, records, *, dry_run=False, stats=None)`.

### `run_scan`

```python
def run_scan(binary, db_path, target, *, timeout=None):
    # subprocess.run([binary, "scan", target, "--db", db_path], capture_output=True, timeout=timeout)
    # FileNotFoundError    -> ScanError("musefs binary '<binary>' not found; ...")
    # TimeoutExpired       -> ScanError("`<binary> scan` for <target> timed out after <timeout>s; ...")
    # returncode != 0      -> ScanError("`<binary> scan` failed for <target> (exit <rc>): <stderr>")
```

Unifies Picard's `run_scan` (had a fixed 120s timeout) and beets' inline
`_run_scan` (no timeout). The shared function is timeout-parameterized; each
host passes the value matching its current behavior (see below), so neither
plugin's behavior changes.

## Adapter responsibilities after extraction

### beets (`contrib/beets/`)

- `beetsplug/_core.py` shrinks to beets-specific mapping: `DIRECT_FIELDS`,
  `map_fields`, `_values`, `_format_date`, and album-cover-file reading (the
  per-run art cache that dedups a shared album cover across its tracks). It
  produces `Record`s.
- `beetsplug/musefs.py` (the `BeetsPlugin`) imports from `musefs_common`:
  `connect`, `check_schema_version`, `track_id_for_path`, `prune_missing`,
  `realpath_key`, `run_scan`, `sync_files`, `SchemaMismatch`, `ScanError`,
  `Record`, `SyncStats`. Its `_track_ids_for_items` helper (`musefs.py:155-163`)
  — which drives the *scoped* `prune_missing(track_ids=…)` for a query subset —
  stays in the adapter; it is beets-item-shaped (iterates `item.path`) and just
  calls the library's `realpath_key` + `track_id_for_path`.
- Calls `run_scan(binary, db, target, timeout=None)` per target — preserves the
  current CLI no-timeout behavior.
- Translates `ScanError` and `SchemaMismatch` to `ui.UserError` (same messages
  as today).
- `pyproject.toml` gains `python-musefs` to `dependencies` (alongside
  `beets>=1.6`). beets installs plugins via pip, so this is a clean dependency.

### Picard (`contrib/picard/`)

- `musefs/_core.py` shrinks to Picard-specific logic: `DIRECT_FIELDS`,
  `map_fields`, `_first_value`, `front_cover`, `_NUMERIC_KEYS`, and the options
  layer (`Opts`, `resolve_config`, `parse_field_map`). `MusefsError` stays here
  for adapter-level conditions ("no DB configured", "DB not found").
- `musefs/__init__.py` (the adapter) imports the store/scan/sync API from the
  **vendored** copy: `from musefs._common import ...` (see below).
- Calls `run_scan(binary, db, target, timeout=120)` — preserves the current
  120s worker-thread guard.
- Translates `ScanError` / `SchemaMismatch` to `MusefsError` for the GUI.

## Picard delivery: vendoring

Picard loads folder plugins by copying the `musefs/` folder into its plugins
directory and does **not** pip-install plugin dependencies. The shipped folder
must stay self-contained. Therefore the library is **vendored** into the Picard
plugin rather than imported from site-packages.

- A script `contrib/python-musefs/vendor_to_picard.py` copies
  `src/musefs_common/*.py` into `contrib/picard/musefs/_common/` (imported as
  `musefs._common`, a private subpackage of the folder plugin). Because the
  library uses only relative imports internally, it works unchanged under the
  `_common` name.
- The copies are **committed real files** (not symlinks — symlinks break the
  "copy the folder" install and Windows).
- **Vendored-file format (exact):** each vendored file is a fixed
  **3-line header** followed by the canonical file's bytes copied **verbatim**:

  ```
  # GENERATED from python-musefs/src/musefs_common/<name>.py — do not edit.
  # Run contrib/python-musefs/vendor_to_picard.py after changing the library.
  #
  <verbatim canonical bytes>
  ```

  The body is appended byte-for-byte with no reformatting, no line-ending
  translation, and no added/stripped trailing newline. The repo is LF-only
  (enforce via `.gitattributes` on the vendored dir if not already global).
- **Ruff must not touch the vendored dir.** Add `contrib/picard/musefs/_common/`
  to ruff's `extend-exclude` so `ruff format --check` (run in the Picard CI job,
  see `ci.yml`) cannot reformat the vendored copy and create false drift; the
  canonical source is the only formatted copy.
- A pytest in Picard's suite (`tests/test_vendor_sync.py`) reads each canonical
  file and its vendored counterpart, drops exactly the **first 3 lines** of the
  vendored file, and asserts `vendored_body == canonical_bytes` byte-for-byte.
  Editing the library without re-vendoring, or hand-editing the vendored copy,
  fails CI. The test enumerates the canonical `*.py` set and asserts the vendored
  set matches it exactly (so an added/removed library module can't silently slip
  the guard).

beets needs no vendoring — it pulls `python-musefs` as a normal pip dependency.

**Validation required before committing to this layout (plan step 0).** The
`from musefs._common import …` model assumes Picard, having added its plugins
directory to `sys.path` and imported the folder as the top-level package
`musefs`, can import a committed subpackage `musefs._common` while
`musefs/__init__.py` (the plugin module) is itself mid-import. This is standard
Python package behavior and is expected to work, but Picard's plugin loader is
the unknown — it has historically special-cased single-file vs. folder plugins.
A small spike against a real Picard install (the same `pytest-qt` harness the
Picard suite already uses, per its README) must confirm the import resolves
before the rest of the work proceeds; a negative result reshapes the layout
(e.g. flatten `_common` into per-concern modules `musefs/_store.py`,
`musefs/_scan.py`, `musefs/_sync.py` imported relatively), so it gates the plan.

## Schema-contract coupling

`EXPECTED_USER_VERSION` lives once, in `musefs_common/constants.py`. A Rust
schema bump becomes a single edit there; both plugins inherit it (Picard after a
re-vendor, enforced by the drift test).

These are two **independent** version numbers, do not conflate them:
- `EXPECTED_USER_VERSION` (= 2) is the SQLite schema contract the library
  targets; it changes only when `schema.rs` MIGRATIONS grows.
- `musefs_common.__version__` is the library's own package SemVer (starts
  `0.1.0`), bumped on library releases. It is **not** equal to
  `EXPECTED_USER_VERSION`. The beets `pyproject.toml` dependency pins/floors the
  package version; the schema target is checked separately at runtime via
  `check_schema_version`.

## Tests and CI

- The duplicated store/scan/sync tests move into `contrib/python-musefs/tests/`
  as the canonical suite: schema-version check, `connect` pragmas, tag
  replacement (including binary-tag survival, #82), art content-addressing and
  dedup, conditional art replacement, `prune_missing`, `realpath_key`
  normalization (incl. non-UTF-8 paths), `run_scan` error paths, and the
  `sync_one`/`sync_files` write-loop including dry-run counting.
- Host test suites shrink to adapter-specific coverage:
  - beets: `map_fields` semantics (multi-value expansion, date formatting),
    album-art-file reading, event/reconcile wiring, the `beet musefs` command.
  - Picard: `map_fields` (first-value), `front_cover`, `resolve_config` /
    `parse_field_map`, options page, callback flow, registration, and the
    vendor byte-identical drift guard.
- CI (`.github/workflows/ci.yml`):
  - A new `python-musefs` job builds the library and runs its test suite, gated
    on `needs.changes.outputs.src`/python changes like the existing `beets` and
    `picard` jobs.
  - **The new job MUST be added to the `ci-ok` aggregator's `needs:` list**
    (currently `[changes, check, interop, beets, picard, e2e]` at `ci.yml:181`).
    `ci-ok` is the single required status check for branch protection; a job
    absent from its `needs:` runs but does not gate merges. (The branch ruleset
    requires `ci-ok`/`coverage-ok` — see the project's branch-protection notes.)
  - The beets job installs `python-musefs` from the local path **before** beets'
    own install, since the dependency is unpublished: `pip install -e
    contrib/python-musefs` (or `pip install ./contrib/python-musefs`) precedes
    installing the beets plugin. A plain `pip install -e contrib/beets` that
    tries to resolve `python-musefs` from PyPI would fail. Document the same
    local-first ordering in the beets README's dev-install steps.
  - The Picard job runs the drift guard so vendor skew fails the build, and
    excludes `musefs/_common/` from its `ruff format --check` step.

## Data flow (unchanged from today, just relayered)

**beets `beet musefs [query]`:** resolve DB path → `run_scan` per target
(autoscan, non-dry-run) → `connect` → `check_schema_version` → build `Record`s
(`map_fields` + album-art bytes) → `sync_files` → `prune_missing` → commit (or
rollback on dry-run) → print summary. Import/write hooks reconcile pending items
at `cli_exit` the same way, now through `sync_files`.

**Picard "Sync to musefs":** `resolve_config` → worker thread → `run_scan` per
file (autoscan, `timeout=120`) → `connect` → `check_schema_version` → build
`Record`s (`map_fields` + `front_cover`) → `sync_files` → single commit →
report `SyncStats` to log/status.

## Migration / sequencing (for the plan)

0. **Spike the Picard subpackage import** against a real Picard install (see
   "Validation required" above). Gate: if `from musefs._common import …` does not
   resolve, switch the vendored layout to relatively-imported flat modules
   before proceeding. Resolve this before any code is moved.
1. Scaffold `contrib/python-musefs/` (pyproject, `src/musefs_common/`, empty
   tests) and move the shared functions in, with relative imports.
2. Move the shared tests into the library; get the library suite green
   standalone. (Tests are **moved/split, not duplicated**: the library suite is
   the authoritative store/scan/sync coverage; host suites lose those cases and
   keep only adapter-specific ones.)
3. Repoint beets: add the local `python-musefs` dependency, slim `_core.py`,
   rewrite `musefs.py` to build `Record`s, do its own art-file read + per-run
   cache + unreadable-`skipped_art` counting, and call `sync_files(stats=…)`;
   restore the verbatim `ui.UserError` strings in the `ScanError` handler; run
   beets tests.
4. Repoint Picard: write `vendor_to_picard.py`, vendor the package, slim
   `_core.py`, rewrite `__init__.py` imports, add the drift guard and the ruff
   exclude; run Picard tests.
5. Wire CI: new `python-musefs` job, **add it to the `ci-ok` `needs:` list
   (`ci.yml:181`)**, beets local-first install ordering, Picard drift guard +
   ruff exclude.
6. Update the three READMEs and `docs/ROADMAP.md` to describe the shared library,
   the beets local-install ordering, and the Picard vendoring step.
