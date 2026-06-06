# Issue #140 Plugin Minor Cleanups Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Implement the four code cleanups from issue #140 in the contrib beets/Picard plugins (the fifth bullet, `dry_run` from Picard, was resolved as no-change).

**Architecture:** Four independent micro-changes, one task each, per the approved spec `docs/superpowers/specs/2026-06-06-issue-140-plugin-cleanups-design.md`. No Rust changes, no `musefs_common` library changes, no vendored-copy regeneration. Python-only diff, so the cargo-mutants in-diff gate does not apply (an empty `mutants.diff` is expected — skip the gate, do not run it as a false pass).

**Tech Stack:** Python (beets plugin, Picard plugin), pytest, ruff.

**Test-runner quirks (read before running anything):**
- **beets** tests need the dir-local venv (system Python is PEP 668 externally managed): run from `contrib/beets/` with `.venv/bin/python -m pytest`.
- **Picard** tests have TWO required runs: the default `python -m pytest tests` (skips everything `importorskip("picard")`-gated) AND the real-Picard run on the **system** interpreter:
  ```bash
  cd contrib/picard
  PYTHONPATH=/usr/lib/picard:/usr/lib/python3/dist-packages QT_QPA_PLATFORM=offscreen \
    /usr/bin/python3 -m pytest tests
  ```
  The default run silently skips real-Picard tests; pushing without the second run has bitten before (PR #125). `pytest-qt` is not installed, so `test_options_page`/`test_callback_flow` skip cleanly in both runs — that's expected.
- **CI ruff scope:** CI lints `contrib/beets/` and `contrib/picard/` with `ruff check` + `ruff format --check` (not just `contrib/python-musefs/`). Run both before declaring done.

---

### Task 0: Branch

`main` is protected (ruleset requires CI aggregator checks). Work on a branch.

- [ ] **Step 1: Create the branch**

```bash
cd /home/cfutro/git/musefs
git checkout -b issue-140-plugin-cleanups
```

---

### Task 1: Picard `PLUGIN_API_VERSIONS` declares only the floor

Picard's loader (`pluginmanager._compatible_api_versions`) takes the set
intersection of the plugin's list with `picard.api_versions`, and every
Picard 2.x release keeps that list back-filled to `"2.0"`. Declaring the
floor the plugin requires loads on every 2.x with zero per-release edits.

**Files:**
- Modify: `contrib/picard/musefs/__init__.py` (the `PLUGIN_API_VERSIONS` constant, ~lines 41–59, including the comment above it)
- Create: `contrib/picard/tests/test_api_versions.py`

`PLUGIN_API_VERSIONS` sits *outside* the `if _PICARD:` block, so the new test
must NOT be `importorskip("picard")`-gated — it runs under the default pytest
invocation and catches drift on hosts without Picard. (That's why it's a new
file: `test_plugin_loads.py` is module-level gated.)

- [ ] **Step 1: Write the failing test**

Create `contrib/picard/tests/test_api_versions.py`:

```python
from musefs import PLUGIN_API_VERSIONS


def test_declares_only_the_api_floor():
    """Picard's loader intersects this list with picard.api_versions, which
    every 2.x release keeps back-filled to "2.0" — so the floor alone loads
    everywhere. A hand-extended list would reintroduce the per-release
    maintenance issue #140 complains about."""
    assert PLUGIN_API_VERSIONS == ["2.0"]
```

- [ ] **Step 2: Run it to verify it fails**

```bash
cd /home/cfutro/git/musefs/contrib/picard
python -m pytest tests/test_api_versions.py -v
```

Expected: FAIL — assertion error showing the current 14-entry list vs `["2.0"]`.

- [ ] **Step 3: Shrink the constant and extend the comment**

In `contrib/picard/musefs/__init__.py`, replace the comment + list (currently
`# Floor: 2.0 — ...` followed by the 14-entry `PLUGIN_API_VERSIONS` list) with:

```python
# Floor: 2.0 — all required APIs (BaseAction, register_*_action, OptionsPage,
# register_options_page, config.TextOption/BoolOption, thread.run_task,
# iterfiles, metadata.images, is_front_image) are present since Picard 2.0.0.
# The loader intersects this list with picard.api_versions, which every 2.x
# release keeps back-filled to "2.0", so declaring the floor alone loads on
# all Picard 2.x without per-release edits.
PLUGIN_API_VERSIONS = ["2.0"]
```

(Keep the first three comment lines verbatim — only the last three lines and
the one-element list are new.)

- [ ] **Step 4: Run the test to verify it passes, plus the default Picard suite**

```bash
cd /home/cfutro/git/musefs/contrib/picard
python -m pytest tests/test_api_versions.py -v && python -m pytest tests
```

Expected: PASS; full default suite green (with the usual skips).

- [ ] **Step 5: Commit**

```bash
cd /home/cfutro/git/musefs
git add contrib/picard/musefs/__init__.py contrib/picard/tests/test_api_versions.py
git commit -m "$(cat <<'EOF'
Declare only the Picard API floor in PLUGIN_API_VERSIONS (#140)

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 2: beets `_query_from_args` returns a list on both paths

The `sync`-verb branch returns `args[1:]` — a tuple slice when handed a tuple —
while the fallthrough returns `list(args)`.

**Files:**
- Modify: `contrib/beets/beetsplug/musefs.py` (`MusefsPlugin._query_from_args`, ~lines 65–71)
- Test: `contrib/beets/tests/test_plugin.py` (`test_command_strips_leading_sync_verb`, ~lines 96–102)

- [ ] **Step 1: Extend the existing test to feed a tuple (failing)**

In `contrib/beets/tests/test_plugin.py`, replace `test_command_strips_leading_sync_verb` with:

```python
def test_command_strips_leading_sync_verb():
    """Verify leading 'sync' verb is stripped from query."""
    plugin = MusefsPlugin()
    assert plugin._query_from_args(["sync", "artist:Band"]) == ["artist:Band"]
    assert plugin._query_from_args(["artist:Band"]) == ["artist:Band"]
    assert plugin._query_from_args([]) == []
    # A tuple must not leak through the sync branch as a tuple slice.
    result = plugin._query_from_args(("sync", "artist:Band"))
    assert result == ["artist:Band"]
    assert type(result) is list
```

- [ ] **Step 2: Run it to verify it fails**

```bash
cd /home/cfutro/git/musefs/contrib/beets
.venv/bin/python -m pytest tests/test_plugin.py::test_command_strips_leading_sync_verb -v
```

Expected: FAIL — `("artist:Band",) == ["artist:Band"]` is False (tuple ≠ list).

- [ ] **Step 3: Fix the slice**

In `contrib/beets/beetsplug/musefs.py`, replace `_query_from_args` with:

```python
    @staticmethod
    def _query_from_args(args):
        """Drop an optional leading `sync` verb so `beet musefs sync QUERY`
        and `beet musefs QUERY` both work."""
        if args and args[0] == "sync":
            return list(args[1:])
        return list(args)
```

(One change: `return args[1:]` → `return list(args[1:])`.)

- [ ] **Step 4: Run the beets suite**

```bash
cd /home/cfutro/git/musefs/contrib/beets
.venv/bin/python -m pytest tests
```

Expected: all PASS.

- [ ] **Step 5: Commit**

```bash
cd /home/cfutro/git/musefs
git add contrib/beets/beetsplug/musefs.py contrib/beets/tests/test_plugin.py
git commit -m "$(cat <<'EOF'
Return a list from both _query_from_args paths (#140)

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 3: Picard `_resolved_files` logs dropped duplicates

`seen.setdefault(...)` silently drops a second `File` sharing a realpath key.
Add a `log.debug` when that happens. The suppression rule is identity-based
(`is not`): the same `File` object re-yielded by overlapping selections (e.g.
an Album and one of its Tracks both selected) is not interesting and stays
silent; first file wins either way.

**Files:**
- Modify: `contrib/picard/musefs/__init__.py` (`_resolved_files`, ~lines 88–99, inside the `if _PICARD:` block)
- Create: `contrib/picard/tests/test_resolved_files.py`

`_resolved_files` is only defined when Picard imports, so the test file is
`importorskip("picard")`-gated and only runs under the real-Picard harness.
Picard's `log` is NOT stdlib `logging` — pytest's `caplog` captures nothing
from it. Use the suite's established pattern (`test_callback_flow.py:37`):
`monkeypatch.setattr(musefs.log, "debug", ...)`. The `fake_file` fixture
(conftest `FakeFile`: `.filename` + `.metadata`, `iterfiles()` yields itself)
is ungated and available.

- [ ] **Step 1: Write the failing test**

Create `contrib/picard/tests/test_resolved_files.py`:

```python
import pytest

pytest.importorskip("picard")


def _capture_debug(monkeypatch, musefs):
    logged = []
    monkeypatch.setattr(musefs.log, "debug", lambda fmt, *a: logged.append(fmt % a))
    return logged


def test_duplicate_realpath_is_dropped_and_logged(monkeypatch, fake_file):
    """Two distinct Files resolving to one realpath key: first wins, and the
    drop is visible at debug level instead of silent."""
    import musefs

    first = fake_file("/music/a.flac", None)
    second = fake_file("/music/a.flac", None)
    logged = _capture_debug(monkeypatch, musefs)

    resolved = musefs._resolved_files([first, second])

    assert list(resolved.values()) == [first]
    assert len(logged) == 1
    assert "duplicate" in logged[0]


def test_same_file_yielded_twice_is_silent(monkeypatch, fake_file):
    """The same File object re-yielded by overlapping selections is expected;
    it is deduplicated without a log line."""
    import musefs

    f = fake_file("/music/a.flac", None)
    logged = _capture_debug(monkeypatch, musefs)

    resolved = musefs._resolved_files([f, f])

    assert list(resolved.values()) == [f]
    assert logged == []
```

- [ ] **Step 2: Run it under the real-Picard harness to verify the first test fails**

```bash
cd /home/cfutro/git/musefs/contrib/picard
PYTHONPATH=/usr/lib/picard:/usr/lib/python3/dist-packages QT_QPA_PLATFORM=offscreen \
  /usr/bin/python3 -m pytest tests/test_resolved_files.py -v
```

Expected: `test_duplicate_realpath_is_dropped_and_logged` FAILS (`len(logged)`
is 0 — current code never logs); `test_same_file_yielded_twice_is_silent`
already PASSES (it pins the suppression behavior).

- [ ] **Step 3: Implement the duplicate log**

In `contrib/picard/musefs/__init__.py`, replace the body of `_resolved_files` with:

```python
    def _resolved_files(objs):
        """Resolve a selection (File/Track/Album/Cluster) to a dict of
        realpath-key -> File, de-duplicated (first wins, drops logged at
        debug level). Picard items all implement iterfiles(); a File yields
        itself; a matched Track with no on-disk file yields nothing."""
        seen = {}
        for obj in objs:
            for f in obj.iterfiles():
                if not f.filename:  # unsaved/virtual file: no path to key on
                    continue
                key = realpath_key(f.filename)
                kept = seen.setdefault(key, f)
                if kept is not f:
                    log.debug(
                        "musefs: duplicate file for %s: %r dropped in favor of %r",
                        key,
                        f.filename,
                        kept.filename,
                    )
        return seen
```

- [ ] **Step 4: Run both Picard suites**

```bash
cd /home/cfutro/git/musefs/contrib/picard
PYTHONPATH=/usr/lib/picard:/usr/lib/python3/dist-packages QT_QPA_PLATFORM=offscreen \
  /usr/bin/python3 -m pytest tests
python -m pytest tests
```

Expected: real-Picard run all PASS (2 pytest-qt skips expected); default run
all PASS with the new file skipped via `importorskip("picard")`.

- [ ] **Step 5: Commit**

```bash
cd /home/cfutro/git/musefs
git add contrib/picard/musefs/__init__.py contrib/picard/tests/test_resolved_files.py
git commit -m "$(cat <<'EOF'
Log duplicate-realpath drops in Picard _resolved_files (#140)

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 4: beets `sniff_mime` receives the real path, not the lossy key

Hygiene fix, **behavior-equivalent today** (per the spec: U+FFFD replacement
cannot change `splitext` results against the ASCII-keyed `_EXT_MIME` table),
so there is no failing test to write — the steps are change, re-run existing
coverage, commit.

**Files:**
- Modify: `contrib/beets/beetsplug/_core.py` (`_read_album_art`, ~lines 112–142)

- [ ] **Step 1: Pass the fsdecoded realpath to `sniff_mime`**

In `_read_album_art`, the current tail of the function is:

```python
    try:
        # Open the raw realpath, not realpath_key's lossy U+FFFD form: the file
        # is only opened, not matched against the DB.
        with open(os.path.realpath(artpath), "rb") as fh:
            data = fh.read()
    except OSError:
        stats.skipped_art += 1
        cache[key] = None
        return None
    if len(data) > MAX_ART_BYTES:
        stats.skipped_art += 1
        cache[key] = None
        return None
    art = (data, sniff_mime(data, key))
    cache[key] = art
    return art
```

Replace it with (hoist `realpath` into `real`, feed its fsdecoded form to
`sniff_mime` instead of the lossy `key`):

```python
    # Use the raw realpath, not realpath_key's lossy U+FFFD form: the file is
    # only opened and extension-sniffed, not matched against the DB.
    real = os.path.realpath(artpath)
    try:
        with open(real, "rb") as fh:
            data = fh.read()
    except OSError:
        stats.skipped_art += 1
        cache[key] = None
        return None
    if len(data) > MAX_ART_BYTES:
        stats.skipped_art += 1
        cache[key] = None
        return None
    art = (data, sniff_mime(data, os.fsdecode(real)))
    cache[key] = art
    return art
```

(`artpath` is bytes from beets, so `os.path.realpath` returns bytes;
`os.fsdecode` yields the str that `sniff_mime`'s `os.path.splitext` +
str-keyed `_EXT_MIME` lookup expect.)

- [ ] **Step 2: Run the existing beets suite (covers art reading + mime sniffing)**

```bash
cd /home/cfutro/git/musefs/contrib/beets
.venv/bin/python -m pytest tests
```

Expected: all PASS, no new tests (the spec explicitly adds none — the
distinction is unobservable with the current table).

- [ ] **Step 3: Commit**

```bash
cd /home/cfutro/git/musefs
git add contrib/beets/beetsplug/_core.py
git commit -m "$(cat <<'EOF'
Feed sniff_mime the raw realpath instead of the lossy key (#140)

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 5: Full verification sweep

Everything CI checks for a contrib-Python diff, run locally.

- [ ] **Step 1: All four test suites**

```bash
cd /home/cfutro/git/musefs/contrib/python-musefs && python -m pytest
cd /home/cfutro/git/musefs/contrib/beets && .venv/bin/python -m pytest tests
cd /home/cfutro/git/musefs/contrib/picard && python -m pytest tests
cd /home/cfutro/git/musefs/contrib/picard && \
  PYTHONPATH=/usr/lib/picard:/usr/lib/python3/dist-packages QT_QPA_PLATFORM=offscreen \
  /usr/bin/python3 -m pytest tests
```

Expected: all green. python-musefs is untouched (sanity only). The Picard
vendor-drift test (`test_vendor_sync.py`) must pass unchanged — `musefs_common`
was not modified, so NO re-vendoring.

- [ ] **Step 2: Ruff, matching CI scope**

```bash
cd /home/cfutro/git/musefs
ruff check contrib/beets/ contrib/picard/ contrib/python-musefs/
ruff format --check contrib/beets/ contrib/picard/ contrib/python-musefs/
```

Expected: clean. Fix and amend nothing — if ruff flags the new code, fix it,
re-run the affected suite, and create a NEW commit.

- [ ] **Step 3: Confirm the mutation gate does not apply**

```bash
cd /home/cfutro/git/musefs
git diff "$(git merge-base main HEAD)...HEAD" --stat -- '*.rs'
```

Expected: empty output (no Rust changed). Do NOT run `cargo mutants` against
an empty diff — it mutates nothing and exits 0, a silent false pass. Skip the
gate; CI's Rust jobs are unaffected by a contrib-only diff.

- [ ] **Step 4: Done — hand off**

Implementation complete. Use superpowers:finishing-a-development-branch.
PR notes: body should `Closes #140` and record that the `dry_run` bullet was
resolved as no-change (the parameter is live from beets' `--dry-run`; adding a
Picard dry-run UI was declined as unneeded scope).
