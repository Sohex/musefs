# Single Generated Schema Source Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the three hand-maintained `tests/schema.sql` fixtures with one generated `musefs_common/schema.py` rendered from the Rust migration constants, drift-guarded by a `musefs-db` unit test (issue #98).

**Architecture:** A `#[cfg(test)]` module in `musefs-db/src/schema.rs` renders the canonical SQL (each `MIGRATION_Vn` verbatim + `PRAGMA user_version = n;`) into a Python module. A non-`#[ignore]`d test compares it against the committed `contrib/python-musefs/src/musefs_common/schema.py` (writes it when `MUSEFS_REGEN_SCHEMA_PY=1`); a second test proves the rendered SQL is semantically identical to `migrate()` on a fresh DB. The three contrib conftests import `SCHEMA_SQL` from the module; Picard gets it through the existing `vendor_to_picard.py` pipeline and its drift guard.

**Tech Stack:** Rust (rusqlite, plain unit tests), Python (sqlite3, pytest, ruff).

**Spec:** `docs/superpowers/specs/2026-06-04-schema-single-source-design.md`

**Branch:** `schema-single-source` (already created; spec committed on it).

**File map:**

- Modify: `musefs-db/src/schema.rs` — new `#[cfg(test)] mod schema_py_tests` (render helpers + 2 tests). No production-code change.
- Create (generated): `contrib/python-musefs/src/musefs_common/schema.py`
- Modify: `contrib/python-musefs/src/musefs_common/constants.py` — derive `EXPECTED_USER_VERSION`
- Modify: `contrib/python-musefs/tests/conftest.py`, `contrib/beets/tests/conftest.py`, `contrib/picard/tests/conftest.py` — import `SCHEMA_SQL`
- Modify: `contrib/python-musefs/tests/test_store_db.py` — stale comment
- Delete: `contrib/{python-musefs,beets,picard}/tests/schema.sql`
- Create (vendored): `contrib/picard/musefs/_common/schema.py`; Modify (re-vendored): `contrib/picard/musefs/_common/constants.py`
- Modify: `CLAUDE.md` — contrib section

**Conventions for every commit in this plan:** stage files by name (never `git add -A`), commit message via HEREDOC ending with the `Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>` trailer.

---

### Task 1: Rust render helper + semantic equivalence test

The render helper and its semantic test come first, TDD-style: the test defines what "canonical SQL" means (fresh-DB-equivalent to `migrate()`) before any file generation exists.

**Files:**
- Modify: `musefs-db/src/schema.rs` (append a new `#[cfg(test)]` module after `migration_v3_tests`, which ends at line 386)

- [ ] **Step 1: Write the failing test (and the render helper signature it needs)**

Append to the end of `musefs-db/src/schema.rs`:

```rust
#[cfg(test)]
mod schema_py_tests {
    use rusqlite::Connection;

    use super::MIGRATIONS;

    /// Canonical SQL text: each migration verbatim, preceded by a banner and
    /// followed by the user_version stamp `migrate()` applies after that step.
    /// Equivalent to `migrate()` on a fresh DB only — no fast-path/partial-
    /// upgrade logic — which is what `schema_sql_matches_migrate` proves.
    fn render_schema_sql() -> String {
        let mut sql = String::new();
        for (i, migration) in MIGRATIONS.iter().enumerate() {
            let n = i + 1;
            if i > 0 {
                sql.push('\n');
            }
            sql.push_str(&format!("-- ── MIGRATION_V{n} ──"));
            sql.push_str(migration); // every MIGRATION_Vn starts and ends with '\n'
            sql.push_str(&format!("PRAGMA user_version = {n};\n"));
        }
        sql
    }

    fn dump_master(conn: &Connection) -> Vec<(String, String, String, Option<String>)> {
        conn.prepare("SELECT type, name, tbl_name, sql FROM sqlite_master ORDER BY type, name")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
    }

    fn user_version(conn: &Connection) -> i64 {
        conn.pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap()
    }

    /// The rendering must stay semantically identical to migrate() on a fresh
    /// DB — guards against migrate() ever growing a non-SQL step the
    /// concatenation cannot represent.
    #[test]
    fn schema_sql_matches_migrate() {
        let rendered = Connection::open_in_memory().unwrap();
        rendered.execute_batch(&render_schema_sql()).unwrap();

        let mut migrated = Connection::open_in_memory().unwrap();
        super::migrate(&mut migrated).unwrap();

        assert_eq!(dump_master(&rendered), dump_master(&migrated));
        assert_eq!(user_version(&rendered), user_version(&migrated));
        assert_eq!(user_version(&rendered), MIGRATIONS.len() as i64);
    }
}
```

- [ ] **Step 2: Run the test to verify it passes (it should — the helper is written against the existing constants)**

Run: `cargo test -p musefs-db schema_sql_matches_migrate`
Expected: `test schema::schema_py_tests::schema_sql_matches_migrate ... ok` — 1 passed.

(If it fails, the rendering rule is wrong; fix `render_schema_sql`, not the test. The likely failure is `sqlite_master` row differences from a malformed concatenation.)

- [ ] **Step 3: Sanity-check the negative — the test actually bites**

Temporarily change `PRAGMA user_version = {n};` to `PRAGMA user_version = 0;` in `render_schema_sql`, run the same command, confirm it FAILS on the `user_version` assertion, then revert. This proves the test isn't vacuous.

- [ ] **Step 4: fmt + clippy**

Run: `cargo fmt --all && cargo clippy --all-targets -p musefs-db`
Expected: no warnings.

- [ ] **Step 5: Commit**

```bash
git add musefs-db/src/schema.rs
git commit -m "test(db): canonical schema SQL rendering, proven equivalent to migrate()"
```

---

### Task 2: Freshness drift-guard test + generate schema.py

**Files:**
- Modify: `musefs-db/src/schema.rs` (inside `mod schema_py_tests` from Task 1)
- Create (generated): `contrib/python-musefs/src/musefs_common/schema.py`

- [ ] **Step 1: Write the failing freshness test**

Add inside `mod schema_py_tests`, after `render_schema_sql`:

```rust
    /// Full content of the generated musefs_common/schema.py. Must stay
    /// `ruff format --check`-clean (comment header + two assignments is).
    fn render_schema_py() -> String {
        format!(
            "# GENERATED from musefs-db/src/schema.rs — do not edit.\n\
             # Regenerate: MUSEFS_REGEN_SCHEMA_PY=1 cargo test -p musefs-db schema_py\n\
             # Re-vendor:  python contrib/python-musefs/vendor_to_picard.py\n\
             \n\
             SCHEMA_SQL = \"\"\"\\\n\
             {sql}\"\"\"\n\
             \n\
             USER_VERSION = {version}\n",
            sql = render_schema_sql(),
            version = MIGRATIONS.len()
        )
    }
```

And after `schema_sql_matches_migrate`:

```rust
    /// NOT #[ignore]d on purpose: the compare path must run under plain
    /// `cargo test` or the CI drift gate doesn't exist. Only the write
    /// behavior is env-gated.
    #[test]
    fn schema_py_fixture_is_fresh() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../contrib/python-musefs/src/musefs_common/schema.py");
        let rendered = render_schema_py();
        if std::env::var_os("MUSEFS_REGEN_SCHEMA_PY").is_some() {
            std::fs::write(&path, &rendered).expect("write schema.py");
            return;
        }
        let on_disk = std::fs::read_to_string(&path).expect(
            "musefs_common/schema.py missing — regenerate with \
             MUSEFS_REGEN_SCHEMA_PY=1 cargo test -p musefs-db schema_py",
        );
        assert_eq!(
            on_disk, rendered,
            "musefs_common/schema.py is stale. Regenerate: \
             MUSEFS_REGEN_SCHEMA_PY=1 cargo test -p musefs-db schema_py, \
             then: python contrib/python-musefs/vendor_to_picard.py"
        );
    }
```

- [ ] **Step 2: Run the test to verify it fails (file doesn't exist yet)**

Run: `cargo test -p musefs-db schema_py_fixture_is_fresh`
Expected: FAIL — panic `musefs_common/schema.py missing — regenerate with ...`.

- [ ] **Step 3: Generate the file**

Run: `MUSEFS_REGEN_SCHEMA_PY=1 cargo test -p musefs-db schema_py`
Expected: PASS (both `schema_py_tests` tests run under this filter; the freshness test takes the write path).

- [ ] **Step 4: Run again without the env var to verify the compare path passes**

Run: `cargo test -p musefs-db schema_py`
Expected: 2 passed (`schema_sql_matches_migrate`, `schema_py_fixture_is_fresh`).

- [ ] **Step 5: Inspect the generated file**

The file must look like this (middle elided here — the real file has the full SQL of all three migrations):

```python
# GENERATED from musefs-db/src/schema.rs — do not edit.
# Regenerate: MUSEFS_REGEN_SCHEMA_PY=1 cargo test -p musefs-db schema_py
# Re-vendor:  python contrib/python-musefs/vendor_to_picard.py

SCHEMA_SQL = """\
-- ── MIGRATION_V1 ──
CREATE TABLE tracks (
    ...
-- ── MIGRATION_V3 ──
    ...
PRAGMA user_version = 3;
"""

USER_VERSION = 3
```

Check: uniform banner + `PRAGMA user_version = n;` for **all three** migrations including V1 (the old fixtures omitted V1's — the uniform rule is the new canonical format), and the file ends with a trailing newline.

- [ ] **Step 6: Verify it's ruff-clean and importable**

```bash
cd contrib/python-musefs && ruff check src/musefs_common/schema.py && ruff format --check src/musefs_common/schema.py
python -c "import sys; sys.path.insert(0, 'src'); from musefs_common.schema import SCHEMA_SQL, USER_VERSION; assert USER_VERSION == 3; assert SCHEMA_SQL.rstrip().endswith('PRAGMA user_version = 3;')"
cd ../..
```

Expected: ruff passes both checks; the python one-liner exits 0.

- [ ] **Step 7: fmt + commit**

```bash
cargo fmt --all
git add musefs-db/src/schema.rs contrib/python-musefs/src/musefs_common/schema.py
git commit -m "feat(db): generate musefs_common/schema.py from the Rust migrations (#98)"
```

---

### Task 3: python-musefs consumes the generated module

**Files:**
- Modify: `contrib/python-musefs/src/musefs_common/constants.py`
- Modify: `contrib/python-musefs/tests/conftest.py:1-17`
- Modify: `contrib/python-musefs/tests/test_store_db.py:21`
- Delete: `contrib/python-musefs/tests/schema.sql`

- [ ] **Step 1: Derive EXPECTED_USER_VERSION in constants.py**

Replace the full contents of `contrib/python-musefs/src/musefs_common/constants.py` with:

```python
from .schema import USER_VERSION

EXPECTED_USER_VERSION = USER_VERSION

MAX_ART_BYTES = 16 * 1024 * 1024 - 64 * 1024

# Wall-clock cap (seconds) for a single `musefs scan` shell-out; a wedged scan
# (stuck disk, DB lock) raises ScanError(kind="timeout") rather than hanging.
SCAN_TIMEOUT_SECONDS = 120
```

(The intermediate `USER_VERSION` name, rather than `import ... as EXPECTED_USER_VERSION`, keeps ruff's F401 quiet — the import is *used* by the assignment. The relative import works identically in the vendored Picard copy, where the package is `musefs._common`.)

- [ ] **Step 2: Switch the conftest to the module import**

In `contrib/python-musefs/tests/conftest.py`, replace lines 1–9:

```python
import sqlite3
import time
from pathlib import Path

import pytest

from musefs_common import connect as musefs_connect

SCHEMA_SQL = (Path(__file__).parent / "schema.sql").read_text()
```

with:

```python
import sqlite3
import time

import pytest

from musefs_common import connect as musefs_connect
from musefs_common.schema import SCHEMA_SQL
```

(`Path` was only used to read the fixture; leaving the import would trip ruff F401.)

And update the now-stale `db_path` docstring (line 17):

```python
    """A temp musefs DB with the V2 schema applied."""
```

becomes (version-agnostic, so it can't go stale on V4):

```python
    """A temp musefs DB with the full schema applied."""
```

- [ ] **Step 3: Fix the stale comment in test_store_db.py**

Line 21 of `contrib/python-musefs/tests/test_store_db.py`:

```python
        check_schema_version(conn)  # schema.sql applies user_version=3
```

becomes:

```python
        check_schema_version(conn)  # SCHEMA_SQL stamps the latest user_version
```

- [ ] **Step 4: Delete the fixture**

```bash
git rm contrib/python-musefs/tests/schema.sql
```

- [ ] **Step 5: Run the suite + linters**

```bash
cd contrib/python-musefs && python -m pytest && ruff check . && ruff format --check . && cd ../..
```

Expected: all tests pass (including `test_check_schema_version_passes_on_v3`, which now exercises the derived `EXPECTED_USER_VERSION` end to end); ruff clean.

- [ ] **Step 6: Commit**

```bash
git add contrib/python-musefs/src/musefs_common/constants.py contrib/python-musefs/tests/conftest.py contrib/python-musefs/tests/test_store_db.py
git commit -m "refactor(python-musefs): consume generated schema module, derive EXPECTED_USER_VERSION (#98)"
```

(`git rm` already staged the deletion.)

---

### Task 4: beets consumes the generated module

beets depends on python-musefs via pip (editable install), so the import is the installed-package path.

**Files:**
- Modify: `contrib/beets/tests/conftest.py:1-13`
- Delete: `contrib/beets/tests/schema.sql`

- [ ] **Step 1: Switch the conftest import**

In `contrib/beets/tests/conftest.py`, replace lines 1–8:

```python
import sqlite3
import time
from pathlib import Path

import pytest
from musefs_common import connect as musefs_connect

SCHEMA_SQL = (Path(__file__).parent / "schema.sql").read_text()
```

with:

```python
import sqlite3
import time

import pytest
from musefs_common import connect as musefs_connect
from musefs_common.schema import SCHEMA_SQL
```

(Note: this conftest has no blank line between `import pytest` and the `musefs_common` import — preserve that; ruff format accepts both.)

And make the `db_path` docstring (line 13) version-agnostic:

```python
    """A temp musefs DB with the V3 schema applied."""
```

becomes:

```python
    """A temp musefs DB with the full schema applied."""
```

- [ ] **Step 2: Delete the fixture**

```bash
git rm contrib/beets/tests/schema.sql
```

- [ ] **Step 3: Run the suite + linters**

python-musefs is UNPUBLISHED — install the local lib first or resolution fails (per CLAUDE.md):

```bash
cd contrib/beets && pip install -e ../python-musefs && pip install -e ".[test]" && python -m pytest tests && ruff check . && ruff format --check . && cd ../..
```

Expected: all tests pass; ruff clean.

- [ ] **Step 4: Commit**

```bash
git add contrib/beets/tests/conftest.py
git commit -m "refactor(beets): consume generated schema module (#98)"
```

---

### Task 5: Picard — re-vendor and consume

The vendor script copies every `*.py` in `src/musefs_common/`, so it picks up the new `schema.py` and the changed `constants.py` with zero script changes; `tests/test_vendor_sync.py` then guards both.

**Files:**
- Create (vendored): `contrib/picard/musefs/_common/schema.py`
- Modify (re-vendored): `contrib/picard/musefs/_common/constants.py`
- Modify: `contrib/picard/tests/conftest.py:1-17`
- Delete: `contrib/picard/tests/schema.sql`

- [ ] **Step 1: Re-vendor**

```bash
python contrib/python-musefs/vendor_to_picard.py
git status --short contrib/picard/musefs/_common/
```

Expected: `schema.py` added, `constants.py` modified. The vendored `schema.py` carries two generated headers (the vendor script's on top of the Rust generator's) — expected and harmless.

- [ ] **Step 2: Switch the conftest import**

In `contrib/picard/tests/conftest.py`, replace lines 1–9:

```python
import sqlite3
import time
from pathlib import Path

import pytest

from musefs._common import connect as musefs_connect

SCHEMA_SQL = (Path(__file__).parent / "schema.sql").read_text()
```

with:

```python
import sqlite3
import time

import pytest

from musefs._common import connect as musefs_connect
from musefs._common.schema import SCHEMA_SQL
```

(Picard tests already import `musefs._common` without Picard installed — no new import risk.)

And the `db_path` docstring (line 17):

```python
    """A temp musefs DB with the V2 schema applied."""
```

becomes:

```python
    """A temp musefs DB with the full schema applied."""
```

- [ ] **Step 3: Delete the fixture**

```bash
git rm contrib/picard/tests/schema.sql
```

- [ ] **Step 4: Run the suite + linters**

```bash
cd contrib/picard && python -m pytest tests && ruff check . && ruff format --check . && cd ../..
```

Expected: all tests pass — in particular `test_vendor_sync.py` (now covering the vendored `schema.py` byte-for-byte) and the conftest-based DB tests. Qt-fixture tests skip if pytest-qt is absent (normal, per PR #123); skips are fine, failures are not.

- [ ] **Step 5: Commit**

```bash
git add contrib/picard/musefs/_common/schema.py contrib/picard/musefs/_common/constants.py contrib/picard/tests/conftest.py
git commit -m "refactor(picard): re-vendor with generated schema module (#98)"
```

---

### Task 6: CLAUDE.md

**Files:**
- Modify: `CLAUDE.md:63-75`

- [ ] **Step 1: Update the contrib intro paragraph**

Replace (CLAUDE.md lines 63–70):

```markdown
The `contrib/` plugins share one library, `python-musefs` (import package
`musefs_common`, in `contrib/python-musefs/`): beets depends on it via pip,
Picard vendors a committed copy into `musefs/_common/` (re-vendor with
`python contrib/python-musefs/vendor_to_picard.py`; a drift-guard test enforces
freshness). Mirror these constants when the Rust schema changes:
`EXPECTED_USER_VERSION` (= `MIGRATIONS` length in `musefs-db/src/schema.rs`) and
`MAX_ART_BYTES` (mirrors `musefs-core/src/scan.rs`) in
`contrib/python-musefs/src/musefs_common/constants.py`.
```

with:

```markdown
The `contrib/` plugins share one library, `python-musefs` (import package
`musefs_common`, in `contrib/python-musefs/`): beets depends on it via pip,
Picard vendors a committed copy into `musefs/_common/` (re-vendor with
`python contrib/python-musefs/vendor_to_picard.py`; a drift-guard test enforces
freshness). `musefs_common/schema.py` (`SCHEMA_SQL`, `USER_VERSION` — from
which `EXPECTED_USER_VERSION` derives) is GENERATED from
`musefs-db/src/schema.rs`: after a schema change, run
`MUSEFS_REGEN_SCHEMA_PY=1 cargo test -p musefs-db schema_py`, then re-vendor.
Drift is enforced by a `musefs-db` unit test and the Picard vendor-sync test.
Still hand-mirrored when the Rust side changes: `MAX_ART_BYTES` (mirrors
`musefs-core/src/scan.rs`) in
`contrib/python-musefs/src/musefs_common/constants.py`.
```

- [ ] **Step 2: Add schema.py to the responsibility list**

Replace (CLAUDE.md line 75):

```markdown
  `store.py`, `paths.py`, `constants.py`, `errors.py`.
```

with:

```markdown
  `store.py`, `paths.py`, `constants.py`, `errors.py`, and the generated
  `schema.py`.
```

- [ ] **Step 3: Commit**

```bash
git add CLAUDE.md
git commit -m "docs: schema.py is generated; EXPECTED_USER_VERSION no longer hand-mirrored (#98)"
```

---

### Task 7: Full verification

- [ ] **Step 1: Rust gates (CI parity)**

```bash
cargo fmt --all --check
cargo clippy --all-targets
cargo test --workspace
```

Expected: fmt clean (check the exit status directly — CI has a fmt gate), no clippy warnings, all tests pass (FUSE e2e stays `#[ignore]`d as always).

- [ ] **Step 2: In-diff mutation gate (CI parity)**

```bash
git diff "$(git merge-base main HEAD)...HEAD" -- '*.rs' > mutants.diff
grep -q '^@@ ' mutants.diff
cargo mutants --in-diff mutants.diff -j2 --exclude 'musefs-latencyfs/**' --output /tmp/mutants-out/in-diff
```

Expected: the diff is non-empty (the `grep` exits 0 — `schema.rs` changed), but **all Rust changes are `#[cfg(test)]`-only, so cargo-mutants finds 0 mutants to test**. That outcome is correct here, not a silent false pass.

- [ ] **Step 3: All three Python suites once more, from a clean shell**

```bash
cd contrib/python-musefs && python -m pytest && ruff check . && ruff format --check . && cd ../..
cd contrib/beets && python -m pytest tests && ruff check . && ruff format --check . && cd ../..
cd contrib/picard && python -m pytest tests && ruff check . && ruff format --check . && cd ../..
```

Expected: all pass (Picard Qt tests may skip).

- [ ] **Step 4: Confirm no schema.sql remains and the tree is clean**

```bash
find contrib -name 'schema.sql'
git status --short
```

Expected: no output from either.

- [ ] **Step 5: End-to-end drift-guard demonstration (optional but cheap)**

Hand-edit one character in `contrib/python-musefs/src/musefs_common/schema.py`, run `cargo test -p musefs-db schema_py_fixture_is_fresh` → must FAIL with the regen message; restore via `git checkout -- contrib/python-musefs/src/musefs_common/schema.py`, re-run → PASS.
