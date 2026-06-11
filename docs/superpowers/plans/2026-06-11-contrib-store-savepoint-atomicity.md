# Contrib Store Savepoint Atomicity Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make the contrib Python store's destructive tag/art replace operations atomic regardless of the connection's transaction mode, closing the torn `DELETE`+`INSERT` window flagged in issue #191 without weakening the "caller owns the transaction" contract.

**Architecture:** Add one private `_savepoint` context manager (plus two tiny mode-detection helpers) to `store.py`. Wrap each of `replace_tags`, `merge_tags`, `replace_track_art` in it (approach **A**), and wrap the whole per-record write block in `sync_one` in it (approach **C**). The helper nests via `SAVEPOINT` inside a caller-managed transaction (never committing it) and, on an autocommit connection, has the *outermost* call own a transaction for the operation (commit on success, rollback on failure). An `owns`-the-transaction guard makes the C-over-A nesting correct on autocommit connections.

**Tech Stack:** Python 3.8+ (no version bump), stdlib `sqlite3` + `contextlib`, pytest.

**Design spec:** `docs/superpowers/specs/2026-06-11-contrib-store-savepoint-atomicity-design.md` (read it before starting; the "Transaction semantics" and "legacy-mode trap" sections explain *why* the helper is shaped the way it is).

---

## Critical context (read before any task)

- **The pre-commit hook runs the FULL workspace test suite + clippy + ruff.** Tasks 1–4 touch `.py` files, so **each of those commits also compiles and runs the entire Rust workspace** (the cargo gate is skipped only for commits whose staged paths are *all* under `docs/` or `*.md`). Expect slow commits and ensure a working Rust toolchain. A commit with any red test is rejected, so **never commit a failing test on its own** — within each task, write the test and the implementation, get green, then commit them together. (The "run the test and watch it fail" step is local verification only; do not commit at that point.)
- **Ruff must be clean before committing.** The hook runs `ruff check` and `ruff format --check` over `contrib/python-musefs/` (among others). Run them yourself first (commands in each task).
- **Run the Python suite from the package dir:** `cd contrib/python-musefs` then `python3 -m pytest -q`. The baseline is **56 passed**.
- **Why `sqlite3.Connection` can't be monkeypatched:** instances disallow attribute assignment, so failure-injection tests use a `Connection` *subclass* via `factory=` (provided in Task 1's test file) rather than `monkeypatch.setattr(conn, "executemany", ...)`.
- **Autocommit test connections** must enable foreign keys *and* switch to autocommit: open the connection, `PRAGMA foreign_keys = ON`, then set `conn.isolation_level = None`. A bare `sqlite3.connect(path, isolation_level=None)` leaves FKs off and would make the FK-violation test a silent no-op.

## File structure

| File | Responsibility | Change |
| ---- | -------------- | ------ |
| `contrib/python-musefs/src/musefs_common/store.py` | the store contract: connect, schema check, tag/art writes | Add `_savepoint`/`_is_autocommit`/`_is_legacy`; wrap three functions; docstrings |
| `contrib/python-musefs/src/musefs_common/sync.py` | per-file sync write-loop | Import `_savepoint`; wrap the `sync_one` write block |
| `contrib/python-musefs/tests/test_atomicity.py` | **new** — atomicity/rollback tests for A and C | Create, grow across Tasks 1–4 |
| `ARCHITECTURE.md` | external-writer contract docs | One sentence in the contract section |

---

## Task 1: `_savepoint` helper + wrap `replace_tags`

**Files:**
- Modify: `contrib/python-musefs/src/musefs_common/store.py` (imports near `:1-3`; add helpers before `connect` at `:9`; rewrite `replace_tags` at `:55-70`)
- Test: `contrib/python-musefs/tests/test_atomicity.py` (create)

- [ ] **Step 1: Write the failing tests**

Create `contrib/python-musefs/tests/test_atomicity.py`:

```python
import sqlite3

import pytest
from conftest import insert_track, text_tags

from musefs_common import connect, replace_tags


class _FailInsert(sqlite3.Connection):
    """Connection that raises on the next INSERT executemany, to enter the
    DELETE-then-INSERT torn window deterministically."""

    fail = False

    def executemany(self, sql, parameters):
        if self.fail and sql.lstrip().upper().startswith("INSERT"):
            raise RuntimeError("boom")
        return super().executemany(sql, parameters)


def _autocommit(db_path, factory=None):
    """An autocommit connection with foreign keys on (mirrors connect()'s FK
    pragma), used to exercise the undefended-against-autocommit path."""
    conn = sqlite3.connect(db_path, factory=factory) if factory else sqlite3.connect(db_path)
    conn.execute("PRAGMA foreign_keys = ON")
    conn.isolation_level = None  # legacy autocommit
    return conn


def test_replace_tags_atomic_on_autocommit_failure(db_path):
    # Seed a track + one text tag (committed) on an autocommit connection.
    conn = _autocommit(db_path, factory=_FailInsert)
    try:
        tid = insert_track(conn, "/m/a.flac")
        replace_tags(conn, tid, [("title", "ORIG")])
        # Now force the INSERT half of replace_tags to crash after the DELETE.
        conn.fail = True
        with pytest.raises(RuntimeError):
            replace_tags(conn, tid, [("title", "NEW")])
    finally:
        conn.close()
    # The original tag must survive: the savepoint rolled the torn write back.
    check = connect(db_path)
    try:
        assert text_tags(check, tid) == {"title": ["ORIG"]}
    finally:
        check.close()


def test_replace_tags_no_premature_commit_in_deferred_mode(db_path):
    # The legacy-mode trap guard: in default deferred mode the savepoint must
    # nest, so a caller rollback still wins. A bare savepoint would commit here.
    conn = connect(db_path)
    try:
        tid = insert_track(conn, "/m/a.flac")
        conn.commit()
        replace_tags(conn, tid, [("title", "X")])
        conn.rollback()
    finally:
        conn.close()
    check = connect(db_path)
    try:
        assert text_tags(check, tid) == {}
    finally:
        check.close()
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cd contrib/python-musefs && python3 -m pytest tests/test_atomicity.py -q`
Expected: both FAIL — `test_replace_tags_atomic_on_autocommit_failure` because the torn write is *not* rolled back (the new tag/empty state persists), and `test_replace_tags_no_premature_commit_in_deferred_mode` because today nothing wraps the write so `"X"` is never written and... actually it currently passes vacuously; if so, treat the autocommit test as the primary red. (See note.) The autocommit test MUST be red before implementing.

> Note: the deferred trap-guard test only turns red against a *naive* `SAVEPOINT` (no `BEGIN`). Against today's unwrapped code it passes (the write is never committed because nothing commits). It is kept as a permanent guard so a future "simplify the helper to a bare savepoint" regression is caught. The autocommit test is the one that must be red now.

- [ ] **Step 3: Add the helper to `store.py`**

Add `import contextlib` to the import block (keep alphabetical — it goes first):

```python
import contextlib
import hashlib
import os
import sqlite3
```

Then insert these definitions immediately before `def connect(` (currently `store.py:9`):

```python
_LEGACY = sqlite3.LEGACY_TRANSACTION_CONTROL  # == -1; the getattr default for <3.12


def _is_autocommit(conn):
    """True if the connection auto-commits each statement (no caller-owned
    transaction will be committed for us)."""
    ac = getattr(conn, "autocommit", _LEGACY)  # 3.12+ attribute; _LEGACY on <3.12
    if ac is True:
        return True
    if ac is False:
        return False
    return conn.isolation_level is None  # legacy transaction control


def _is_legacy(conn):
    """True if the connection uses legacy transaction control (the <3.12 default
    and the 3.12+ LEGACY_TRANSACTION_CONTROL mode)."""
    return getattr(conn, "autocommit", _LEGACY) == _LEGACY


@contextlib.contextmanager
def _savepoint(conn, name):
    """Make a DELETE+INSERT block atomic regardless of the connection's
    transaction mode. On a caller-managed connection it nests via SAVEPOINT and
    never commits the enclosing transaction; on an autocommit connection the
    outermost call owns a transaction for the block (commit on success, rollback
    on failure). Nested calls only nest -- they never BEGIN or commit -- so a
    sync_one savepoint may wrap these per-function savepoints safely.

    ``name`` must be a hardcoded SQL identifier (it is interpolated into the SQL,
    so never pass caller-controlled text)."""
    autocommit = _is_autocommit(conn)
    owns = not conn.in_transaction  # outermost call: it opens & owns the txn
    # Legacy mode never auto-BEGINs before SAVEPOINT, so a savepoint opened as
    # the first statement of a batch would become the outermost transaction and
    # commit durably on RELEASE. Force a nesting BEGIN there. PEP-249 modes
    # auto-begin before any statement, so they need no nudge.
    if owns and _is_legacy(conn):
        conn.execute("BEGIN")
    conn.execute(f"SAVEPOINT {name}")
    try:
        yield
    except BaseException:
        try:
            conn.execute(f"ROLLBACK TO {name}")
            conn.execute(f"RELEASE {name}")
            if owns and autocommit:
                conn.rollback()
        except sqlite3.Error:
            pass  # never mask the original exception with a cleanup failure
        raise
    else:
        conn.execute(f"RELEASE {name}")
        if owns and autocommit:
            conn.commit()
```

- [ ] **Step 4: Wrap `replace_tags`**

Replace the body of `replace_tags` (`store.py:55-70`) with:

```python
def replace_tags(conn, track_id, pairs):
    """Replace all tags for a track. Duplicate keys get incrementing ordinals
    (mirroring musefs scan ingest).

    Atomic via an internal savepoint (see ``_savepoint``), so a crash between the
    DELETE and the INSERT can never leave the track's text tags wiped -- safe
    even when called on an autocommit connection."""
    # Scope to the plugin-owned text rows: scanner-written binary tags
    # (value_blob NOT NULL) must survive a sync (#82).
    with _savepoint(conn, "musefs_replace_tags"):
        conn.execute("DELETE FROM tags WHERE track_id = ? AND value_blob IS NULL", (track_id,))
        ordinals = {}
        rows = []
        for key, value in pairs:
            ordinal = ordinals.get(key, 0)
            ordinals[key] = ordinal + 1
            rows.append((track_id, key, value, ordinal))
        conn.executemany(
            "INSERT INTO tags (track_id, key, value, ordinal) VALUES (?, ?, ?, ?)",
            rows,
        )
```

- [ ] **Step 5: Run the new tests and the full Python suite**

Run: `cd contrib/python-musefs && python3 -m pytest tests/test_atomicity.py -q && python3 -m pytest -q`
Expected: `test_atomicity.py` both PASS; full suite **58 passed** (56 baseline + 2 new).

- [ ] **Step 6: Lint (auto-fix, then verify)**

The hook enforces ruff's import-sorting (`I`) and formatting, so let ruff arrange them first, then verify clean:

Run: `cd /home/cfutro/git/musefs && ruff check --fix contrib/python-musefs/ && ruff format contrib/python-musefs/ && ruff check contrib/python-musefs/ && ruff format --check contrib/python-musefs/`
Expected: the first two commands may rewrite imports/formatting; the final two report no errors. Re-stage any files ruff touched.

- [ ] **Step 7: Commit** (runs the full pre-commit hook incl. the Rust suite)

```bash
git add contrib/python-musefs/src/musefs_common/store.py contrib/python-musefs/tests/test_atomicity.py
git commit -m "fix(contrib): make replace_tags atomic via savepoint (#191)"
```

---

## Task 2: Wrap `merge_tags`

**Files:**
- Modify: `contrib/python-musefs/src/musefs_common/store.py` (rewrite `merge_tags` at `:73-98`)
- Test: `contrib/python-musefs/tests/test_atomicity.py` (add one test + import)

- [ ] **Step 1: Write the failing test**

In `test_atomicity.py`, add `merge_tags` to the import and append the test:

```python
from musefs_common import connect, merge_tags, replace_tags
```

```python
def test_merge_tags_atomic_on_autocommit_failure(db_path):
    conn = _autocommit(db_path, factory=_FailInsert)
    try:
        tid = insert_track(conn, "/m/a.flac")
        # Seed both a managed key we'll overwrite and an unmanaged baseline key.
        merge_tags(conn, tid, [("artist", "ORIG")], [])
        conn.execute(
            "INSERT INTO tags (track_id, key, value, ordinal) VALUES (?, 'genre', 'Jazz', 0)",
            (tid,),
        )
        conn.fail = True
        with pytest.raises(RuntimeError):
            merge_tags(conn, tid, [("artist", "NEW")], [])
    finally:
        conn.close()
    check = connect(db_path)
    try:
        # The failed merge rolled back: both the managed and baseline rows remain.
        assert text_tags(check, tid) == {"artist": ["ORIG"], "genre": ["Jazz"]}
    finally:
        check.close()
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cd contrib/python-musefs && python3 -m pytest tests/test_atomicity.py::test_merge_tags_atomic_on_autocommit_failure -q`
Expected: FAIL — the per-key DELETE wiped `artist` and the INSERT crashed, so without a savepoint `artist` is gone (result `{"genre": ["Jazz"]}`).

- [ ] **Step 3: Wrap `merge_tags`**

Replace the body of `merge_tags` (`store.py:73-98`) with:

```python
def merge_tags(conn, track_id, managed_pairs, delete_keys):
    """Per-key replace of the plugin-managed text tags, leaving unmanaged text
    rows (the scan-seeded baseline) intact. ``managed_pairs`` is an ordered list
    of (key, value); every key it names is cleared and rewritten with contiguous
    ordinals. ``delete_keys`` names keys to clear without rewriting (tags the
    plugin previously managed and the user has now removed). Both deletes are
    scoped to ``value_blob IS NULL`` so scanner-written binary tags survive.

    Atomic via an internal savepoint (see ``_savepoint``): the per-key deletes
    and the rewrite either all land or none do, even on an autocommit
    connection."""
    with _savepoint(conn, "musefs_merge_tags"):
        by_key = {}
        for key, value in managed_pairs:
            by_key.setdefault(key, []).append(value)

        for key in set(by_key) | set(delete_keys or ()):
            conn.execute(
                "DELETE FROM tags WHERE track_id = ? AND key = ? AND value_blob IS NULL",
                (track_id, key),
            )

        rows = [
            (track_id, key, value, ordinal)
            for key, values in by_key.items()
            for ordinal, value in enumerate(values)
        ]
        conn.executemany(
            "INSERT INTO tags (track_id, key, value, ordinal) VALUES (?, ?, ?, ?)",
            rows,
        )
```

- [ ] **Step 4: Run the test + full suite**

Run: `cd contrib/python-musefs && python3 -m pytest tests/test_atomicity.py -q && python3 -m pytest -q`
Expected: atomicity tests PASS; full suite **59 passed**.

- [ ] **Step 5: Lint (auto-fix, then verify)**

Run: `cd /home/cfutro/git/musefs && ruff check --fix contrib/python-musefs/ && ruff format contrib/python-musefs/ && ruff check contrib/python-musefs/ && ruff format --check contrib/python-musefs/`
Expected: the final two commands report no errors. Re-stage any files ruff touched.

- [ ] **Step 6: Commit**

```bash
git add contrib/python-musefs/src/musefs_common/store.py contrib/python-musefs/tests/test_atomicity.py
git commit -m "fix(contrib): make merge_tags atomic via savepoint (#191)"
```

---

## Task 3: Wrap `replace_track_art`

**Files:**
- Modify: `contrib/python-musefs/src/musefs_common/store.py` (rewrite `replace_track_art` at `:135-147`)
- Test: `contrib/python-musefs/tests/test_atomicity.py` (add one test + imports)

- [ ] **Step 1: Write the failing test**

In `test_atomicity.py`, extend the import and append the test. This one uses a **natural** failure: an `art_id` with no matching `art` row violates the `track_art.art_id` foreign key on the INSERT-after-DELETE (FK enforcement is on via `_autocommit`).

```python
from conftest import JPEG, insert_track, text_tags
from musefs_common import connect, merge_tags, replace_tags, replace_track_art, upsert_art
```

```python
def test_replace_track_art_atomic_on_fk_violation(db_path):
    conn = _autocommit(db_path)  # FK on, autocommit; no failure injection needed
    try:
        tid = insert_track(conn, "/m/a.flac")
        art_id = upsert_art(conn, JPEG, "image/jpeg")
        replace_track_art(conn, tid, [(art_id, 3, "")])
        before_cv = conn.execute(
            "SELECT content_version FROM tracks WHERE id = ?", (tid,)
        ).fetchone()[0]
        # 999999 has no row in `art`: the INSERT (after the DELETE) trips the FK.
        with pytest.raises(sqlite3.IntegrityError):
            replace_track_art(conn, tid, [(999999, 3, "")])
        rows = conn.execute(
            "SELECT art_id, picture_type, ordinal FROM track_art WHERE track_id = ?", (tid,)
        ).fetchall()
        after_cv = conn.execute(
            "SELECT content_version FROM tracks WHERE id = ?", (tid,)
        ).fetchone()[0]
    finally:
        conn.close()
    # The DELETE + the content_version trigger bump both rolled back with the FK failure.
    assert rows == [(art_id, 3, 0)]
    assert after_cv == before_cv
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cd contrib/python-musefs && python3 -m pytest tests/test_atomicity.py::test_replace_track_art_atomic_on_fk_violation -q`
Expected: FAIL — `pytest.raises(IntegrityError)` is satisfied, but the assertions fail: today the DELETE already committed (autocommit), so `rows == []` and `after_cv != before_cv`.

- [ ] **Step 3: Wrap `replace_track_art`**

Replace the body of `replace_track_art` (`store.py:135-147`) with:

```python
def replace_track_art(conn, track_id, arts):
    """Replace the track's art rows. ``arts`` is an ordered list of
    ``(art_id, picture_type, description)``; each row's ``ordinal`` is its
    list index.

    Atomic via an internal savepoint (see ``_savepoint``): the DELETE and the
    re-insert either both land or neither does, even on an autocommit
    connection."""
    with _savepoint(conn, "musefs_replace_track_art"):
        conn.execute("DELETE FROM track_art WHERE track_id = ?", (track_id,))
        conn.executemany(
            "INSERT INTO track_art (track_id, art_id, picture_type, description, "
            "ordinal) VALUES (?, ?, ?, ?, ?)",
            [
                (track_id, art_id, picture_type, description, i)
                for i, (art_id, picture_type, description) in enumerate(arts)
            ],
        )
```

- [ ] **Step 4: Run the test + full suite**

Run: `cd contrib/python-musefs && python3 -m pytest tests/test_atomicity.py -q && python3 -m pytest -q`
Expected: atomicity tests PASS; full suite **60 passed**.

- [ ] **Step 5: Lint (auto-fix, then verify)**

Run: `cd /home/cfutro/git/musefs && ruff check --fix contrib/python-musefs/ && ruff format contrib/python-musefs/ && ruff check contrib/python-musefs/ && ruff format --check contrib/python-musefs/`
Expected: the final two commands report no errors. Re-stage any files ruff touched.

- [ ] **Step 6: Commit**

```bash
git add contrib/python-musefs/src/musefs_common/store.py contrib/python-musefs/tests/test_atomicity.py
git commit -m "fix(contrib): make replace_track_art atomic via savepoint (#191)"
```

---

## Task 4: Whole-record atomicity in `sync_one` (approach C)

**Files:**
- Modify: `contrib/python-musefs/src/musefs_common/sync.py` (import at `:6`; wrap the `sync_one` write block at `:68-78`)
- Test: `contrib/python-musefs/tests/test_atomicity.py` (add three tests + imports)

- [ ] **Step 1: Write the failing tests**

In `test_atomicity.py`, extend the imports and append the tests. Note `ArtImage`, `Record`, `SyncStats`, `sync_files`, `sync_one` are all exported from `musefs_common`.

```python
from musefs_common import (
    ArtImage,
    Record,
    SyncStats,
    connect,
    merge_tags,
    replace_tags,
    replace_track_art,
    sync_files,
    sync_one,
    upsert_art,
)
```

```python
def test_sync_one_whole_record_atomic_on_autocommit(db_path):
    # On an autocommit connection, if art linking fails the tags written earlier
    # in the same record must roll back too (record is all-or-nothing).
    conn = _autocommit(db_path, factory=_FailInsert)
    try:
        tid = insert_track(conn, "/m/a.flac")
        # _FailInsert fails the FIRST insert executemany once fail=True. In
        # sync_one the tag INSERT runs before the art INSERT, so to target the
        # art step specifically we let tags write, then fail on the art INSERT.
        conn.fail = "art"  # sentinel handled below
        with pytest.raises(RuntimeError):
            sync_one(
                conn,
                Record(key="/m/a.flac", pairs=[("title", "T")], art=[ArtImage(JPEG, "image/jpeg")]),
                SyncStats(),
            )
    finally:
        conn.close()
    check = connect(db_path)
    try:
        assert text_tags(check, tid) == {}
        assert check.execute("SELECT COUNT(*) FROM track_art").fetchone()[0] == 0
    finally:
        check.close()


def test_sync_files_deferred_batch_commits_atomically(db_path):
    # Shipped beets-style path: deferred mode, several records, one final commit.
    conn = connect(db_path)
    try:
        insert_track(conn, "/m/a.flac")
        insert_track(conn, "/m/b.flac")
        conn.commit()
        records = [
            Record(key="/m/a.flac", pairs=[("title", "A")]),
            Record(key="/m/b.flac", pairs=[("title", "B")]),
        ]
        stats = sync_files(conn, records)
        assert stats.synced == 2
        conn.commit()
    finally:
        conn.close()
    check = connect(db_path)
    try:
        got = {
            path: check.execute(
                "SELECT value FROM tags t JOIN tracks tr ON tr.id = t.track_id "
                "WHERE tr.backing_path = ? AND t.key = 'title'",
                (path,),
            ).fetchone()[0]
            for path in ("/m/a.flac", "/m/b.flac")
        }
        assert got == {"/m/a.flac": "A", "/m/b.flac": "B"}
    finally:
        check.close()


def test_sync_files_deferred_batch_rolls_back_as_unit(db_path):
    # Deferred mode: per-record savepoints must NOT self-commit, so a caller
    # rollback abandons the whole batch.
    conn = connect(db_path)
    try:
        tid = insert_track(conn, "/m/a.flac")
        conn.commit()
        sync_one(conn, Record(key="/m/a.flac", pairs=[("title", "A")]), SyncStats())
        conn.rollback()  # caller abandons the batch after the write
    finally:
        conn.close()
    check = connect(db_path)
    try:
        assert text_tags(check, tid) == {}
    finally:
        check.close()
```

The first test references `conn.fail = "art"`; update `_FailInsert` to honor it so only the **art** INSERT fails (tags still write). Replace the class body with:

```python
class _FailInsert(sqlite3.Connection):
    """Connection that raises on a chosen INSERT executemany, to enter a
    DELETE-then-INSERT torn window deterministically. ``fail`` may be:
    False (never), True (any INSERT), or "art" (only the track_art INSERT)."""

    fail = False

    def executemany(self, sql, parameters):
        upper = sql.lstrip().upper()
        if upper.startswith("INSERT"):
            if self.fail is True or (self.fail == "art" and "TRACK_ART" in upper):
                raise RuntimeError("boom")
        return super().executemany(sql, parameters)
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cd contrib/python-musefs && python3 -m pytest tests/test_atomicity.py -q`
Expected: `test_sync_one_whole_record_atomic_on_autocommit` FAILS — without C, the tag write autocommitted before the art INSERT crashed, so `text_tags` is `{"title": ["T"]}` not `{}`. The two `sync_files` deferred tests pass already (deferred mode never had the leak when the caller commits/rolls back as one unit), but they are kept as permanent guards against a future per-record self-commit regression.

- [ ] **Step 3: Wrap the `sync_one` write block**

In `sync.py`, add `_savepoint` to the store import (`sync.py:6`):

```python
from .store import (
    _savepoint,
    merge_tags,
    replace_tags,
    replace_track_art,
    track_id_for_path,
    upsert_art,
)
```

Then wrap the existing `if not dry_run:` block (`sync.py:68-78`). Replace:

```python
    if not dry_run:
        if merge:
            merge_tags(conn, track_id, record.pairs, record.delete_keys or [])
        else:
            replace_tags(conn, track_id, record.pairs)
        if will_link_art:
            arts = [
                (upsert_art(conn, img.data, img.mime), img.picture_type, img.description)
                for img in kept
            ]
            replace_track_art(conn, track_id, arts)
```

with:

```python
    if not dry_run:
        with _savepoint(conn, "musefs_sync_one"):
            if merge:
                merge_tags(conn, track_id, record.pairs, record.delete_keys or [])
            else:
                replace_tags(conn, track_id, record.pairs)
            if will_link_art:
                arts = [
                    (upsert_art(conn, img.data, img.mime), img.picture_type, img.description)
                    for img in kept
                ]
                replace_track_art(conn, track_id, arts)
```

- [ ] **Step 4: Run the tests + full suite**

Run: `cd contrib/python-musefs && python3 -m pytest tests/test_atomicity.py -q && python3 -m pytest -q`
Expected: all atomicity tests PASS; full suite **63 passed** (60 + 3 new).

- [ ] **Step 5: Lint (auto-fix, then verify)**

Run: `cd /home/cfutro/git/musefs && ruff check --fix contrib/python-musefs/ && ruff format contrib/python-musefs/ && ruff check contrib/python-musefs/ && ruff format --check contrib/python-musefs/`
Expected: the final two commands report no errors. Re-stage any files ruff touched. (Importing the underscore-prefixed `_savepoint` across modules is fine; ruff's `F401`/`N` rules don't object to a used private import.)

- [ ] **Step 6: Commit**

```bash
git add contrib/python-musefs/src/musefs_common/sync.py contrib/python-musefs/tests/test_atomicity.py
git commit -m "fix(contrib): make sync_one record write atomic via savepoint (#191)"
```

---

## Task 5: Document the contract guarantee in `ARCHITECTURE.md`

**Files:**
- Modify: `ARCHITECTURE.md` (the "The external-writer contract" section, the paragraph beginning "The shared Python library", around `:167`)

This is a docs-only commit (the cargo gate is skipped), so no test step.

- [ ] **Step 1: Add the atomicity sentence**

Find this sentence (`ARCHITECTURE.md:167-170`):

```
The shared Python library (`contrib/python-musefs/`) encodes this contract
for plugin authors, including a generated copy of the schema
(`musefs_common/schema.py`, regenerated from `schema.rs` by a drift-guarded
test — see [CONTRIBUTING](CONTRIBUTING.md)).
```

Append one sentence to that paragraph, immediately after the `see [CONTRIBUTING](CONTRIBUTING.md)).` clause and before ` The [Lidarr integration]`:

```
 Its tag/art replace operations each wrap their `DELETE`+`INSERT` in a SQLite
savepoint, so they are individually atomic and the "caller owns the transaction"
guarantee holds even on an autocommit connection.
```

- [ ] **Step 2: Verify the prose reads correctly**

Run: `cd /home/cfutro/git/musefs && git diff ARCHITECTURE.md`
Expected: the new sentence sits inside the "shared Python library" paragraph; no stray formatting.

- [ ] **Step 3: Commit**

```bash
git add ARCHITECTURE.md
git commit -m "docs: note contrib store replace ops are savepoint-atomic (#191)"
```

---

## Final verification

- [ ] **Step 1: Full Python suite green**

Run: `cd contrib/python-musefs && python3 -m pytest -q`
Expected: **63 passed**.

- [ ] **Step 2: Lint clean**

Run: `cd /home/cfutro/git/musefs && ruff check contrib/python-musefs/ && ruff format --check contrib/python-musefs/`
Expected: no errors.

- [ ] **Step 3: Confirm the fuzz/contract jobs are unaffected**

No Rust signatures changed and no schema change was made, so the `fuzz/` crate and the schema-mirror regen are untouched. No action needed — just confirm `git status` shows only the four files from this plan were modified.

- [ ] **Step 4 (optional): cross-version sanity on the 3.8 floor**

The helper's behavior is mode- and version-sensitive in principle. If a 3.8 interpreter is available, run the suite under it: `cd contrib/python-musefs && python3.8 -m pytest -q`. (Behavior was confirmed stable on CPython 3.8/3.11/3.12/3.14 during design.)

---

## Spec coverage check

- Approach **A** (per-function savepoints): Tasks 1 (`replace_tags`), 2 (`merge_tags`), 3 (`replace_track_art`). ✓
- Approach **C** (whole-record savepoint in `sync_one`, wrapping the entire `if not dry_run:` body incl. `upsert_art`): Task 4. ✓
- `_savepoint` helper with `owns`-guard + mode detection: Task 1. ✓
- Legacy-mode trap guard test: Task 1 (`...no_premature_commit_in_deferred_mode`). ✓
- FK-on test setup + assert `IntegrityError` fires: Task 3. ✓
- Whole-record autocommit atomicity + deferred batch atomicity tests: Task 4. ✓
- Docstring updates on all four functions: Tasks 1–4. ✓
- `ARCHITECTURE.md` contract sentence: Task 5. ✓
- Non-goals respected: no Python version bump, no `connect()` enforcement, no cross-record atomicity. ✓
