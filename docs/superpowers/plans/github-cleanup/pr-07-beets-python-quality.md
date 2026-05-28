# PR 7 Beets Python Quality Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Scope Beets pruning safely, add Beets CI coverage, and configure Ruff for repository Python code.

**Architecture:** Scope pruning by musefs `tracks.id` resolved from synced backing paths, not by Beets item IDs. Keep Python tooling changes in the Beets/interop surface only.

**Tech Stack:** Python, Beets plugin, sqlite3, pytest, Ruff, GitHub Actions.

---

### Task 1: Scope Beets Pruning By Musefs Track IDs

**Files:**
- Modify: `contrib/beets/beetsplug/_core.py`
- Modify: `contrib/beets/beetsplug/musefs.py`
- Test: `contrib/beets/tests/test_db.py`
- Test: `contrib/beets/tests/test_plugin.py`

- [ ] **Step 1: Add low-level scoped prune test**

In `test_db.py`, assert `prune_missing(conn, track_ids=[missing_related])`
deletes only that musefs track ID and preserves unrelated missing rows.

- [ ] **Step 2: Implement optional `track_ids` in `_core.prune_missing`**

The function accepts musefs `tracks.id` values only:

```python
def prune_missing(conn, track_ids=None):
    if track_ids is None:
        rows = conn.execute("SELECT id, backing_path FROM tracks")
    else:
        rows = (
            conn.execute("SELECT id, backing_path FROM tracks WHERE id = ?", (tid,)).fetchone()
            for tid in track_ids
        )
    gone = []
    for row in rows:
        if row is None:
            continue
        tid, path = row
        if not os.path.exists(path):
            gone.append((tid,))
    conn.executemany("DELETE FROM tracks WHERE id = ?", gone)
    return len(gone)
```

- [ ] **Step 3: Resolve scope from synced paths**

In `MusefsPlugin`, after scan/sync, compute scope by looking up musefs track IDs
from normalized backing paths:

```python
def _track_ids_for_items(self, conn, items):
    ids = []
    for item in items:
        key = _core.realpath_key(os.fsdecode(item.path))
        tid = _core.track_id_for_path(conn, key)
        if tid is not None:
            ids.append(tid)
    return ids
```

Use this inside `_prune_missing(db_path, items=items)` so callers never pass
Beets item IDs as musefs track IDs.

- [ ] **Step 4: Apply scope to command and reconcile paths**

For queried command syncs and passive reconcile, prune only the resolved synced
track IDs. For full-library command sync with no query, whole-DB prune remains
acceptable because the user asked to sync the whole library.

- [ ] **Step 5: Add plugin-level regression tests**

Tests must cover:
- queried sync preserves unrelated missing rows;
- passive reconcile preserves unrelated missing rows;
- full sync may prune unrelated missing rows.

### Task 2: Add Ruff And Beets CI

**Files:**
- Create: `contrib/beets/ruff.toml` or modify existing Python config
- Modify: `.github/workflows/ci.yml`
- Modify: Python files as required by Ruff

- [ ] **Step 1: Add Ruff config**

Use:

```toml
line-length = 100
target-version = "py311"

[lint]
select = ["E", "F", "I", "N", "W"]

[format]
preview = true
```

- [ ] **Step 2: Add CI job**

Add a `beets` job that runs:

```bash
python -m pip install ruff pytest
python -m pip install -e contrib/beets
ruff check contrib/beets tests/interop
ruff format --check contrib/beets tests/interop
python -m pytest contrib/beets/tests -v
```

Leave action pinning to PR 8.

- [ ] **Step 3: Verify**

Run:

```bash
python3 -m pytest contrib/beets/tests -v
ruff check contrib/beets tests/interop
ruff format --check contrib/beets tests/interop
```

- [ ] **Step 4: Commit**

```bash
git add .github/workflows/ci.yml contrib/beets tests/interop
git commit -m "fix(beets): scope prune and add Python quality gate

Closes #12
Closes #13
Closes #19"
```
