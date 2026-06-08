# Beets Computed-Path Tag (`beets_path`) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** The beets plugin writes one extra text tag, `beets_path`, per synced track — the beets library-relative path (`item.destination`, extension stripped) — so users can mount musefs with `--template '$!{beets_path}'` and get a tree mirroring their beets library.

**Architecture:** Path computation lives in `contrib/beets/beetsplug/_core.py` (the existing `build_records` chokepoint that feeds both the import-time listeners and the `musefs` command). It appends `("beets_path", value)` to the existing `Record.pairs` list, which flows unchanged through `replace_tags`/`sync_files`. The shared `python-musefs` library and the musefs Rust core are untouched. Computation reuses `realpath_key`'s lossy-UTF-8 normalization so the value is always SQLite-safe; failures skip just that tag and warn via the plugin logger.

**Tech Stack:** Python (beets 2.x plugin). Tests with pytest in the beets venv. Lint: ruff (`select = ["E","F","I","N","W"]`, line-length 100).

**Reference:** Spec at `docs/superpowers/specs/2026-06-08-beets-computed-path-design.md`. Branch `plugin-computed-paths`.

**Gate notes:**
- The pre-commit hook runs `cargo` fmt/clippy/test + `ruff check` + `ruff format --check`. It does **not** run the Python test suite — run beets pytest manually in each task: `contrib/beets/.venv/bin/python -m pytest contrib/beets/tests -q`.
- Keep Python lines ≤ 100 chars. A plain `except Exception:` is lint-clean here (no BLE rule selected); do **not** add a `# noqa`.

## File structure

- `contrib/beets/beetsplug/_core.py` — add `_computed_path` + `_computed_path_or_skip` helpers; extend `build_records` with `write_path`/`log` params and the `beets_path` append. (No beets imports here; the logger is passed in, duck-typed.)
- `contrib/beets/beetsplug/musefs.py` — register the `write_path: True` config default, add `_write_path()`, thread `write_path`/`log` into the `_sync` → `build_records` call.
- `contrib/beets/tests/conftest.py` — extend `FakeItem` with a `destination()` method (+ a raise toggle).
- `contrib/beets/tests/test_build_records.py` — `_core`-level tests.
- `contrib/beets/tests/test_plugin.py` — DB-level enabled/disabled tests.
- `contrib/beets/README.md` — document the tag, `write_path`, and the `$!{beets_path}` mount.

---

## Task 1: Compute `beets_path` in `_core.build_records`

**Files:**
- Modify: `contrib/beets/beetsplug/_core.py`
- Modify: `contrib/beets/tests/conftest.py` (extend `FakeItem`)
- Test: `contrib/beets/tests/test_build_records.py`

- [x] **Step 1: Extend `FakeItem` with a `destination()` method**

In `contrib/beets/tests/conftest.py`, in `FakeItem.__init__`, the tail currently reads:

```python
        self.day = fields.pop("day", 0)
        for k, v in fields.items():
            setattr(self, k, v)

    def get_album(self):
        return self._album
```

Replace that block with (adds two pops before the catch-all, and a `destination` method):

```python
        self.day = fields.pop("day", 0)
        # beets' Item.destination(relative_to_libdir=True) returns a bytes path;
        # default to the item's own path so existing tests keep working.
        self._destination = fields.pop("destination", path)
        self._destination_raises = fields.pop("destination_raises", False)
        for k, v in fields.items():
            setattr(self, k, v)

    def destination(self, relative_to_libdir=False):
        if self._destination_raises:
            raise RuntimeError("destination boom")
        return self._destination

    def get_album(self):
        return self._album
```

- [x] **Step 2: Write the failing `_core` tests**

Append to `contrib/beets/tests/test_build_records.py`:

```python
class _RecordingLog:
    """Duck-typed stand-in for the plugin's logger."""

    def __init__(self):
        self.warnings = []

    def warning(self, *args):
        self.warnings.append(args)


def test_build_records_writes_beets_path_stripping_extension(fake_item):
    item = fake_item(b"/m/a.flac", title="T", destination=b"Artist/Album/01 Song.flac")
    stats = SyncStats()
    records = _core.build_records([item], fields=None, stats=stats)
    assert ("beets_path", "Artist/Album/01 Song") in records[0].pairs


def test_build_records_omits_beets_path_when_write_path_false(fake_item):
    item = fake_item(b"/m/a.flac", title="T", destination=b"Artist/Album/01 Song.flac")
    stats = SyncStats()
    records = _core.build_records([item], fields=None, stats=stats, write_path=False)
    assert all(k != "beets_path" for k, _ in records[0].pairs)


def test_build_records_skips_beets_path_on_error_and_warns(fake_item):
    item = fake_item(b"/m/a.flac", title="T", destination_raises=True)
    log = _RecordingLog()
    stats = SyncStats()
    records = _core.build_records([item], fields=None, stats=stats, log=log)
    assert all(k != "beets_path" for k, _ in records[0].pairs)
    assert ("title", "T") in records[0].pairs  # other tags still sync
    assert log.warnings  # a warning was emitted


def test_build_records_beets_path_is_utf8_safe_for_non_unicode_paths(fake_item):
    # A non-UTF-8 byte must normalize to valid UTF-8 (U+FFFD), never a lone
    # surrogate that SQLite's TEXT encoder would reject.
    item = fake_item(b"/m/a.flac", destination=b"Art\xffist/Album/01 Song.flac")
    stats = SyncStats()
    records = _core.build_records([item], fields=None, stats=stats)
    value = dict(records[0].pairs)["beets_path"]
    value.encode("utf-8")  # must not raise
    assert value.startswith("Art")
    assert value.endswith("/Album/01 Song")


def test_build_records_uses_real_beets_destination(tmp_path):
    # Exercises the REAL beets API (not FakeItem), so the default test tier
    # covers item.destination(relative_to_libdir=True), not just our decode.
    from beets.library import Item, Library

    lib = Library(
        ":memory:",
        directory=str(tmp_path),
        path_formats=[("default", "$artist/$album/$track $title")],
    )
    item = Item(artist="AC/DC", album="Back in Black", title="Hells Bells", track=1)
    item.path = b"/music/x.flac"  # supplies the .flac extension
    lib.add(item)  # assigns an id and binds the library, required by destination()
    stats = SyncStats()
    records = _core.build_records([item], fields=None, stats=stats)
    # beets sanitizes "AC/DC" -> "AC_DC" and zero-pads $track; we strip ".flac".
    assert ("beets_path", "AC_DC/Back in Black/01 Hells Bells") in records[0].pairs
```

(`_core` and `SyncStats` are already imported at the top of `test_build_records.py`; if not, add `from beetsplug import _core` and `from musefs_common import SyncStats`. The `beets` import is local to the test — beets is installed in the venv.)

- [x] **Step 3: Run the new tests to verify they fail**

Run: `contrib/beets/.venv/bin/python -m pytest contrib/beets/tests/test_build_records.py -q`
Expected: FAIL — `build_records` doesn't accept `write_path`/`log` yet and emits no `beets_path` pair (`TypeError: build_records() got an unexpected keyword argument 'write_path'` for the relevant cases; the first test fails its assertion).

- [x] **Step 4: Add the path helpers to `_core.py`**

In `contrib/beets/beetsplug/_core.py`, insert these two functions immediately above `build_records`:

```python
def _computed_path(item):
    """Beets' library-relative path for ``item``, decoded to a SQLite-safe str
    with the file extension removed (musefs re-appends it at render time).

    Mirrors ``realpath_key``'s lossy normalization (U+FFFD for undecodable
    bytes) so the value is always valid UTF-8, but without realpath's on-disk
    resolution. Returns "" when beets yields no usable path.
    """
    raw = item.destination(relative_to_libdir=True)
    decoded = os.fsdecode(raw)
    safe = decoded.encode("utf-8", "surrogateescape").decode("utf-8", "replace")
    return os.path.splitext(safe)[0].lstrip("/")


def _computed_path_or_skip(item, log):
    """``_computed_path`` guarded so a bad destination never aborts a sync.

    Returns "" (skip the tag) on any failure, warning through ``log`` if given.
    """
    try:
        return _computed_path(item)
    except Exception as exc:
        if log is not None:
            # beets' plugin logger is a StrFormatLogger ({}-style, not %-style).
            log.warning("musefs: skipping beets_path for {!r}: {}", item.path, exc)
        return ""
```

- [x] **Step 5: Thread `write_path`/`log` through `build_records`**

In `contrib/beets/beetsplug/_core.py`, replace the whole `build_records` function with:

```python
def build_records(items, *, fields=None, stats, write_path=True, log=None):
    """Build ``Record``s for beets items: map tags and resolve album art (with a
    per-run cache; unreadable/over-cap covers counted into ``stats.skipped_art``).
    When ``write_path`` is set, also emit a ``beets_path`` tag with the track's
    beets library-relative path (extension stripped); a failed computation is
    skipped and warned through ``log``. ``stats`` is mutated and must be the same
    instance passed to ``sync_files``."""
    records = []
    art_cache = {}
    for item in items:
        cover = _read_album_art(item, art_cache, stats)
        pairs = map_fields(item, fields)
        if write_path:
            path = _computed_path_or_skip(item, log)
            if path:
                pairs.append(("beets_path", path))
        records.append(
            Record(
                key=realpath_key(item.path),
                pairs=pairs,
                art=[ArtImage(*cover)] if cover else None,
            )
        )
    return records
```

- [x] **Step 6: Run the `_core` tests to verify they pass**

Run: `contrib/beets/.venv/bin/python -m pytest contrib/beets/tests/test_build_records.py -q`
Expected: PASS (all five new tests — including the real-beets-`destination` one — plus the existing `build_records` tests).

- [x] **Step 7: Run the full beets suite + ruff to confirm no regressions**

Run: `contrib/beets/.venv/bin/python -m pytest contrib/beets/tests -q && ruff check contrib/beets && ruff format --check contrib/beets`
Expected: PASS, no lint findings. (Existing tests use membership/specific-key assertions, so the added default `beets_path` pair is harmless.)

- [x] **Step 8: Commit**

```bash
git add contrib/beets/beetsplug/_core.py contrib/beets/tests/conftest.py contrib/beets/tests/test_build_records.py
git commit -m "feat(beets): compute beets_path tag in build_records

item.destination(relative_to_libdir=True), decoded with lossy-UTF-8
normalization and extension stripped, appended as a beets_path tag.
write_path-gated; failures skip the tag and warn. FakeItem gains a
destination() method.

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Task 2: Wire the `write_path` config into the plugin

**Files:**
- Modify: `contrib/beets/beetsplug/musefs.py`
- Test: `contrib/beets/tests/test_plugin.py`

- [x] **Step 1: Write the failing plugin tests**

Append to `contrib/beets/tests/test_plugin.py`:

```python
def test_sync_writes_beets_path_when_enabled(
    db_path, make_track, fake_item, tmp_path, monkeypatch
):
    real, tid, item = _real_track(
        tmp_path,
        make_track,
        fake_item,
        title="Song",
        destination=b"Artist/Album/01 Song.flac",
    )
    plugin = MusefsPlugin()
    monkeypatch.setattr(
        plugin,
        "config",
        FakeConfigView({"db": db_path, "fields": {}, "write_path": True}),
        raising=False,
    )
    plugin._sync(db_path, [item])

    conn = connect(db_path)
    try:
        assert (
            conn.execute(
                "SELECT value FROM tags WHERE track_id=? AND key='beets_path'", (tid,)
            ).fetchone()[0]
            == "Artist/Album/01 Song"
        )
    finally:
        conn.close()


def test_sync_omits_beets_path_when_disabled(
    db_path, make_track, fake_item, tmp_path, monkeypatch
):
    real, tid, item = _real_track(
        tmp_path,
        make_track,
        fake_item,
        title="Song",
        destination=b"Artist/Album/01 Song.flac",
    )
    plugin = MusefsPlugin()
    monkeypatch.setattr(
        plugin,
        "config",
        FakeConfigView({"db": db_path, "fields": {}, "write_path": False}),
        raising=False,
    )
    plugin._sync(db_path, [item])

    conn = connect(db_path)
    try:
        assert (
            conn.execute(
                "SELECT value FROM tags WHERE track_id=? AND key='beets_path'", (tid,)
            ).fetchone()
            is None
        )
    finally:
        conn.close()
```

- [x] **Step 2: Run the tests to verify they fail**

Run: `contrib/beets/.venv/bin/python -m pytest contrib/beets/tests/test_plugin.py -q -k beets_path`
Expected: FAIL — `_sync` doesn't pass `write_path`/`log` yet, and there's no `write_path` config default, so `_write_path()` doesn't exist. (The disabled test may pass spuriously since nothing reads the flag yet; the enabled test fails because `_write_path` is undefined once Step 3 lands — confirm both after Step 3.)

- [x] **Step 3: Register the `write_path` default and add the accessor**

In `contrib/beets/beetsplug/musefs.py`, in `__init__`, replace this exact block:

```python
        self.config.add({
            "db": None,
            "fields": {},
            "bin": "musefs",  # musefs executable (PATH name or full path)
            "autoscan": True,  # run `musefs scan` automatically before syncing
        })
```

with:

```python
        self.config.add({
            "db": None,
            "fields": {},
            "bin": "musefs",  # musefs executable (PATH name or full path)
            "autoscan": True,  # run `musefs scan` automatically before syncing
            "write_path": True,  # emit a beets_path tag for $!{beets_path} mounts
        })
```

Then add this accessor next to `_autoscan`/`_bin`:

```python
    def _write_path(self):
        return bool(self.config["write_path"].get(bool))
```

- [x] **Step 4: Pass `write_path`/`log` into `build_records`**

In `contrib/beets/beetsplug/musefs.py`, in `_sync`, replace this line:

```python
        records = _core.build_records(items, fields=self._fields(), stats=stats)
```

with:

```python
        records = _core.build_records(
            items,
            fields=self._fields(),
            stats=stats,
            write_path=self._write_path(),
            log=self._log,
        )
```

- [x] **Step 5: Run the plugin tests to verify they pass**

Run: `contrib/beets/.venv/bin/python -m pytest contrib/beets/tests/test_plugin.py -q`
Expected: PASS (both new tests and all existing plugin tests).

- [x] **Step 6: Full beets suite + ruff**

Run: `contrib/beets/.venv/bin/python -m pytest contrib/beets/tests -q && ruff check contrib/beets && ruff format --check contrib/beets`
Expected: PASS, no lint findings.

- [x] **Step 7: Commit**

```bash
git add contrib/beets/beetsplug/musefs.py contrib/beets/tests/test_plugin.py
git commit -m "feat(beets): write_path config gates the beets_path tag

Registers write_path (default on) and threads it plus the plugin logger
into build_records.

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Task 3: Documentation

**Files:**
- Modify: `contrib/beets/README.md`

- [x] **Step 1: Add the `$!{beets_path}` mount to the Workflow example**

In `contrib/beets/README.md`, in the `## Workflow (test drive)` code block, after these lines:

```bash
# Mount the re-tagged view.
musefs mount ~/mnt --db ~/musefs.db \
    --template '$albumartist/$album/$tracknumber - $title'
```

add:

```bash

# ...or mirror your beets library layout exactly, via the computed beets_path tag.
musefs mount ~/mnt --db ~/musefs.db --template '$!{beets_path}'
```

- [x] **Step 2: Add a Notes bullet documenting the tag and `write_path`**

In `contrib/beets/README.md`, in the `## Notes` list, add this bullet (after the **Cover art** bullet):

```markdown
- **Computed path (`beets_path`):** each sync also writes a `beets_path` text tag
  holding the track's beets library-relative path (from your `paths:` config, via
  `item.destination`), with the file extension removed — musefs re-appends it. Mount
  with `--template '$!{beets_path}'` (the `$!{}` path field keeps `/` as directory
  separators) to mirror your beets layout, including layouts musefs's own template
  engine can't express. Set `write_path: no` in the `musefs:` config to skip it.
  Do not add an extension in a template that consumes `beets_path`. See the
  computed-tag workflow in [ARCHITECTURE.md](../../ARCHITECTURE.md).
```

- [x] **Step 3: Verify docs render and links resolve**

Run: `test -f ARCHITECTURE.md && grep -q "beets_path\|computed" ARCHITECTURE.md && echo "cross-ref target OK"`
Expected: `cross-ref target OK` (the computed-tag workflow was documented in `ARCHITECTURE.md` by PR #170).

- [x] **Step 4: Commit**

```bash
git add contrib/beets/README.md
git commit -m "docs(beets): document beets_path computed-path tag and write_path

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Final verification

- [x] **Full beets suite (incl. opt-in tiers off by default) + lint:**

Run: `contrib/beets/.venv/bin/python -m pytest contrib/beets/tests -v && ruff check contrib/beets && ruff format --check contrib/beets`
Expected: all green, no lint findings.

- [ ] **Manual end-to-end (optional; needs a real library + the musefs binary):**

```bash
beet musefs                                            # sync, writes beets_path
sqlite3 ~/musefs.db "SELECT value FROM tags WHERE key='beets_path' LIMIT 3;"
musefs mount ~/mnt --db ~/musefs.db --template '$!{beets_path}'
```

Confirm: `beets_path` values are extension-less relative paths, and the mounted tree mirrors the beets library layout. (Cardinal invariant unchanged — only paths/metadata are virtual; audio bytes untouched.)
