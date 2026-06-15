# Design: Harden contrib store mutations against torn DELETE+INSERT state (#191)

## Problem

`replace_tags`, `merge_tags`, and `replace_track_art` in
`contrib/python-musefs/src/musefs_common/store.py` each perform a `DELETE`
followed by an `executemany` `INSERT` with no transaction of their own. They
rely entirely on the load-bearing **"caller owns the transaction"** contract
(`sync.py`). The shipped callers (beets `_sync`, Picard) are safe: they run in
Python's default deferred mode and `commit()` once at the end, so a crash
before that commit rolls back cleanly.

The torn-state window opens only for a consumer that deliberately uses an
autocommit connection (`isolation_level=None` / `autocommit=True`) or calls
these functions directly on one: a crash between the `DELETE` and the `INSERT`
leaves a track with its tags/art wiped and not replaced until the next sync or
scan. The contract is undefended against autocommit consumers.

This library is the external-writer contract for plugin authors
(beets/Picard/Lidarr), so a documented-only footgun is too weak; we defend it.

## Goal & invariant

Make the destructive replace operations atomic regardless of the connection's
transaction mode, while preserving the "caller owns the transaction" contract:
no premature commits, and `sync_one`'s `dry_run` rollback still works.

## Why not other approaches

- **Require Python >= 3.12 / use the `autocommit` attribute.** Buys nothing for
  atomicity (autocommit-mode connections still exist in 3.12) and the correct
  primitive is still `SAVEPOINT`. Drops 3.8-3.11 — a real adoption cost for a
  plugin-author library whose consumers (and Picard's bundled runtime) are often
  on older Pythons. Rejected.
- **`with conn:` transaction context manager.** Wrong on every version: it
  *commits* at block exit, which violates "caller owns the transaction" (commits
  the caller's in-progress batch early) and breaks `dry_run` rollback.
- **Connection-level enforcement in `connect()`** (e.g. force
  `autocommit=False`). Doesn't make the functions internally atomic and doesn't
  defend a consumer who flips autocommit or brings their own connection.
  Out of scope.

## Transaction semantics: the two modes

A single helper must serve two caller classes with *different* desired
behavior, and the helper picks behavior from the connection's autocommit
setting:

- **Caller-managed connection** (the shipped contract: deferred legacy mode, or
  3.12+ `autocommit=False`): the helper must make its block atomic via a nested
  `SAVEPOINT` and **never** commit or roll back the enclosing transaction. The
  caller's eventual single `commit()`/`rollback()` governs. This preserves
  batch atomicity (beets syncs N records then commits once) and `dry_run`
  rollback.
- **Autocommit connection** (`isolation_level=None`, or 3.12+
  `autocommit=True`): no one else will commit, so the helper **owns** a
  transaction for the operation — `commit()` on success, `rollback()` on
  failure — making the `DELETE`+`INSERT` atomic and durable.

### The legacy-mode trap (why a bare savepoint is wrong)

Python's `sqlite3` in **legacy mode** (`isolation_level=""`, the default that
`connect()` at `store.py:11` returns) auto-issues an implicit `BEGIN` *only*
before `INSERT`/`UPDATE`/`DELETE`/`REPLACE` — **not** before `SAVEPOINT`. So a
`SAVEPOINT` issued as the *first* write of a batch becomes the **outermost**
transaction, and its matching `RELEASE` **commits durably**. A later caller
`rollback()` then no-ops. Verified empirically on CPython 3.8, 3.11, 3.12, and
3.14.

Concrete impact on the shipped beets path (`musefs.py:221-238`): `connect()`
(legacy) → `check_schema_version()` (a `PRAGMA` *read*, opens no transaction) →
`sync_files`. A naive savepoint in the first `sync_one` would be the first
write, so `RELEASE` commits that record durably; every later record then
self-commits too. beets' final `conn.commit()` becomes a no-op, **batch
atomicity is lost**, and records are committed *before* `persist_managed` runs.
(beets `dry_run` is unaffected either way — it skips the write block at
`sync.py:68`, so no savepoint is opened.)

The fix: in legacy mode, force an explicit `BEGIN` when no transaction is open,
so the `SAVEPOINT` *nests* instead of becoming the outermost. PEP-249 modes
(3.12+ `autocommit` attribute) auto-begin before any statement, so they need no
such nudge.

## Core mechanism: a savepoint context manager

A single private helper in `store.py`, used at all sites:

```python
import contextlib
import sqlite3

_LEGACY = getattr(sqlite3, "LEGACY_TRANSACTION_CONTROL", -1)  # 3.12+ const; == -1, absent on <3.12

def _is_autocommit(conn):
    ac = getattr(conn, "autocommit", _LEGACY)  # 3.12+ attribute; _LEGACY on <3.12
    if ac is True:
        return True
    if ac is False:
        return False
    return conn.isolation_level is None  # legacy control: None == autocommit

def _is_legacy(conn):
    return getattr(conn, "autocommit", _LEGACY) == _LEGACY

@contextlib.contextmanager
def _savepoint(conn, name):
    """Make a DELETE+INSERT block atomic regardless of the connection's
    transaction mode. On a caller-managed connection it nests via SAVEPOINT and
    never commits the enclosing transaction; on an autocommit connection the
    *outermost* call owns a transaction for the block (commit on success,
    rollback on failure). Nested calls (e.g. C's sync_one savepoint wrapping A's
    per-function savepoints) only nest — they never BEGIN or commit."""
    autocommit = _is_autocommit(conn)
    owns = not conn.in_transaction  # outermost call: it opens & owns the txn
    # Legacy mode never auto-BEGINs before SAVEPOINT, so a savepoint opened as
    # the first statement would become the outermost txn and commit on RELEASE.
    # Force a nesting BEGIN there. PEP-249 modes auto-begin already.
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

- `name` is a fixed, hardcoded identifier per call site (no user input → no
  injection). Distinct names per site keep nested `RELEASE`/`ROLLBACK TO`
  unambiguous.
- **`owns` guard is load-bearing.** Only the outermost `_savepoint` (the one
  that found no transaction open on entry) issues `BEGIN` and, in autocommit
  mode, the final `commit()`/`rollback()`. Without it, C's `sync_one` savepoint
  wrapping A's `replace_tags` savepoint on an autocommit connection would (a)
  attempt a second `BEGIN` inside the open transaction and (b) let the inner
  call `commit()` early — committing tags before art and destroying whole-record
  atomicity. The inner calls must only nest.
- `ROLLBACK TO` does not pop the savepoint from the stack, so the error path
  also `RELEASE`s it before re-raising.
- The error-path cleanup is wrapped so that a secondary `sqlite3.Error` (e.g. a
  savepoint already discarded by a `SQLITE_BUSY`/`SQLITE_FULL` auto-rollback)
  cannot mask the original failure.

This is verified across all relevant scenarios (see Testing): legacy
deferred batch + single commit, legacy deferred first-statement + caller
rollback (the trap), legacy autocommit success/failure, **nested C-over-A on an
autocommit connection (success and inner-failure)**, and the 3.12+
`autocommit=True`/`False` equivalents.

## A — per-function atomicity (`store.py`)

Wrap the `DELETE`+`INSERT` body of each function in `_savepoint`:

- `replace_tags` → `_savepoint(conn, "musefs_replace_tags")`
- `merge_tags` → `_savepoint(conn, "musefs_merge_tags")` (same pattern; folded
  in even though #191 names only the other two)
- `replace_track_art` → `_savepoint(conn, "musefs_replace_track_art")`

Defends anyone calling these directly on an autocommit connection — exactly
#191's stated exposure.

## C — whole-record atomicity (`sync.py`)

In `sync_one`, wrap the **entire** `if not dry_run:` write block in
`_savepoint(conn, "musefs_sync_one")` — that means everything inside it: the
`merge_tags`/`replace_tags` call, the `upsert_art` blob inserts, *and* the
`replace_track_art` call (`sync.py:69-78`). This closes the "tags replaced,
then crash before art" gap *between* those calls for an autocommit sync caller,
and rolls back any content-addressed art inserted via `upsert_art` if the
record fails. It nests cleanly over A's inner savepoints. `dry_run` is
unaffected: it skips the write block, so no savepoint is opened.

`_savepoint` is imported into `sync.py` from `store`.

## Behavior on the shipped paths

beets `_sync` and Picard run in default deferred (legacy) mode with one final
`commit()`. With the corrected helper, the first wrapped call issues an explicit
`BEGIN` (because no transaction is open yet — `check_schema_version` is only a
read), and every savepoint thereafter nests inside that one transaction and
`RELEASE`s without committing. The caller's single `commit()` (or, for beets
`dry_run`/error, `rollback()`) still governs the whole batch — **batch
atomicity is preserved** and records are still committed only after
`persist_managed` is reached. The only observable change is that the
previously-implicit transaction is now opened by an explicit `BEGIN`; the commit
boundary is identical. Added robustness accrues only to autocommit/direct
callers.

## Docs

- Tighten the docstrings of the four functions: they run inside a caller-owned
  transaction and are now individually atomic via an internal savepoint (so safe
  on autocommit connections too).
- Add one sentence to `ARCHITECTURE.md` §"The external-writer contract" noting
  the library opens nested savepoints within the caller's transaction, so the
  contract holds regardless of the caller's autocommit setting.

## Testing

`contrib/python-musefs/tests/`, pytest, using the existing fixtures/helpers
(`db_path`, `make_track`, `text_tags`, `musefs_connect`). TDD — tests first.

All test connections that exercise autocommit must be opened via the project's
`connect()` (which sets `PRAGMA foreign_keys = ON`) and *then* switched to
autocommit (`conn.isolation_level = None`), **not** via a bare
`sqlite3.connect(..., isolation_level=None)` — the latter leaves FKs off, which
silently neuters test 1.

1. **`replace_track_art` atomicity (natural FK failure):** seed art +
   `track_art`, then call with an `art_id` absent from `art`. With
   `foreign_keys = ON`, the `INSERT` *after* the `DELETE` raises
   `sqlite3.IntegrityError`. On an autocommit connection assert (a) the call
   raised `IntegrityError` (so the torn window was genuinely entered — guards
   against a future FK-off regression turning this into a no-op) **and** (b) the
   original `track_art` rows survive.
2. **`replace_tags` atomicity:** seed tags; force the `executemany` to raise
   (monkeypatch) on an autocommit connection; assert original text tags survive
   and binary tags (`value_blob NOT NULL`) are untouched.
3. **`merge_tags` atomicity:** same shape — failure leaves the pre-existing
   managed + unmanaged rows intact.
4. **C whole-record atomicity:** `sync_one` on an autocommit connection where
   art linking fails → assert tags *also* rolled back (record is all-or-nothing).
5. **The legacy-mode trap guard (primary regression test):** in default
   deferred mode, call `replace_tags`, then `conn.rollback()`; assert nothing
   persisted (check via a second connection). This is the exact scenario that
   fails against a *bare* savepoint helper, so it is the guard that proves the
   nesting `BEGIN` is in place and the caller's rollback still wins.
6. **Deferred batch atomicity:** in deferred mode, sync several records via
   `sync_files`, then a single `conn.commit()`; assert all persist — and a
   variant where a mid-batch record fails and the caller `rollback()`s leaves
   *nothing* persisted (proves no per-record premature commit).
7. **Happy path unchanged:** existing `test_sync.py`, `test_store_art.py`,
   `test_merge_tags.py` continue to pass.

The helper's mode-handling is version-sensitive in principle (legacy vs. 3.12+
PEP-249 attribute), so the implementation plan should note running the suite on
the 3.8 floor in addition to the dev interpreter. The behavior was confirmed
stable on CPython 3.8/3.11/3.12/3.14 during design.

## Scope / non-goals

- No Python version bump (stays `>=3.8`). The SQL `SAVEPOINT` statement is
  portable; the Python `sqlite3` *driver's* transaction management around it is
  **not** uniform across modes, which is exactly why the helper detects the
  connection mode rather than assuming nesting.
- No connection-level autocommit enforcement in `connect()`.
- No batch-level (cross-record) atomicity — that remains the caller's
  transaction to own.

## Files touched

- `contrib/python-musefs/src/musefs_common/store.py` — `_savepoint` helper
  (plus `_is_autocommit`/`_is_legacy` mode detection) + wrap three functions +
  docstrings.
- `contrib/python-musefs/src/musefs_common/sync.py` — import `_savepoint`, wrap
  `sync_one` write block.
- `contrib/picard/musefs/_common/{store,sync}.py` — **regenerated** by running
  `contrib/python-musefs/vendor_to_picard.py`. Picard does not pip-install the
  library; it vendors a byte-identical copy, and `contrib/picard/tests/
  test_vendor_sync.py` fails CI if the canonical lib changes without re-vendoring.
  This is also what propagates the #191 fix to the Picard consumer.
- `ARCHITECTURE.md` — one sentence in the external-writer contract section.
- `contrib/python-musefs/tests/` — new atomicity tests.
