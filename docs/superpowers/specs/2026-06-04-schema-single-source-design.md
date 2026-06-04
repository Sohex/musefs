# Single generated schema source for contrib test suites

**Date:** 2026-06-04
**Issue:** #98 — musefs schema duplicated as hand-maintained schema.sql across three contrib test suites
**Status:** Approved

## Problem

The musefs SQLite schema is defined authoritatively in `musefs-db/src/schema.rs`
(the `MIGRATION_V*` constants applied by `migrate()`). It is mirrored as a
hand-maintained `schema.sql` test fixture in three places:

- `contrib/beets/tests/schema.sql`
- `contrib/picard/tests/schema.sql`
- `contrib/python-musefs/tests/schema.sql`

Nothing guards these against drifting from the Rust schema or from each other,
and they have already drifted textually (python-musefs's copy lacks the
intermediate `PRAGMA user_version = 2` line the other two carry). A fixture
that lags the Rust schema still passes CI while real plugin writes against an
upgraded database could fail. Separately, `EXPECTED_USER_VERSION` in
`musefs_common/constants.py` is a second hand-mirrored value (must equal
`MIGRATIONS.len()`), enforced only by a CLAUDE.md note.

## Design

Replace the three fixtures with **one generated Python module**,
`contrib/python-musefs/src/musefs_common/schema.py`, rendered from the Rust
migration constants. An env-gated drift test in `musefs-db` keeps it fresh;
the existing Picard vendor pipeline propagates it untouched.

The module form (rather than a `.sql` package-data file) is what makes the
single copy reachable everywhere with no plumbing: python-musefs and beets
import it from the installed/`pythonpath` package, and `vendor_to_picard.py`
already copies `*.py` files — so the Picard copy is produced and drift-guarded
by the existing vendor machinery with zero changes.

### 1. The generated artifact: `musefs_common/schema.py`

- Header comment:
  `# GENERATED from musefs-db/src/schema.rs — do not edit.` plus the
  regeneration command
  (`MUSEFS_REGEN_SCHEMA_PY=1 cargo test -p musefs-db schema_py`).
- `SCHEMA_SQL: str` — for each migration `n` (1-based), uniformly including
  `n = 1`: a `-- ── MIGRATION_Vn ──` banner, the `MIGRATION_Vn` text verbatim,
  then `PRAGMA user_version = n;`. This intentionally differs from the old
  fixtures (which omitted the V1 banner/pragma and disagreed with each other
  about intermediate pragmas) — the old files are deleted, and the uniform
  rule is the canonical format. The result is *equivalent to* `migrate()` on
  a fresh database (`execute_batch(sql)` + `user_version` update per step);
  it does not reproduce `migrate()`'s fast-path/partial-upgrade behavior,
  which is why `schema_sql_matches_migrate` exists.
- `USER_VERSION: int` — `MIGRATIONS.len()`.
- The emitted text must pass `ruff format --check` (python-musefs CI lints the
  whole tree); a comment header, one triple-quoted string assignment, and one
  int assignment satisfy this trivially. The SQL contains no `"""` or
  backslashes, so verbatim embedding in a triple-quoted string is safe.

### 2. Rust side: generator + drift guards

All in `musefs-db/src/schema.rs` inside `#[cfg(test)]` — the tests must live
in this module (the migration constants are private), there is no new public
API, and test code is invisible to cargo-mutants:

- A render helper producing the full `schema.py` text from `MIGRATIONS`.
- **`schema_py_fixture_is_fresh`** — compares the rendered text against
  `{CARGO_MANIFEST_DIR}/../contrib/python-musefs/src/musefs_common/schema.py`
  and fails on mismatch with a message naming the regen command. When
  `MUSEFS_REGEN_SCHEMA_PY=1` is set, it writes the file instead of comparing.
  The test is **not** `#[ignore]`d — unlike `interop_emit` (which is ignored
  and opt-in), the compare path must run under plain `cargo test` or the CI
  gate this design depends on silently doesn't exist. Only the *write*
  behavior is env-gated.
- **`schema_sql_matches_migrate`** — applies the rendered SQL via
  `execute_batch` to one in-memory connection, runs `migrate()` on another,
  and asserts identical `sqlite_master` contents (name/type/sql rows) and
  `user_version`. This guards the rendering *semantically*, not just
  textually — e.g. if `migrate()` ever grows a non-SQL step that the
  concatenation cannot represent.

### 3. Python consumers

- Delete all three `tests/schema.sql` files.
- One-line conftest change per suite:
  - python-musefs, beets: `from musefs_common.schema import SCHEMA_SQL`
  - Picard: `from musefs._common.schema import SCHEMA_SQL`
    (tests already import `musefs._common` without Picard installed, so this
    adds no new import risk)
- `constants.py`: `EXPECTED_USER_VERSION` becomes a re-export of
  `schema.USER_VERSION` (`constants` imports from `schema`, never the
  reverse — `schema.py` imports nothing, so no cycle). The import surface
  for `store.py` and existing tests is unchanged.
- Run `vendor_to_picard.py` once to vendor the new module; Picard's
  `test_vendor_sync.py` then covers it byte-for-byte with zero changes. The
  vendored copy carries two generated headers (the vendor script's on top of
  the Rust generator's) — expected and harmless.
- Clean up comments that reference the deleted fixtures: the three conftest
  docstrings ("temp musefs DB with the V2 schema applied") and
  `test_store_db.py`'s "schema.sql applies user_version=3" note.

### Drift-guard chain

```
musefs-db/src/schema.rs
    │  cargo test -p musefs-db (schema_py_fixture_is_fresh)
    ▼
contrib/python-musefs/src/musefs_common/schema.py
    │  contrib/picard tests (test_vendor_sync.py)
    ▼
contrib/picard/musefs/_common/schema.py
```

beets consumes `musefs_common` as a pip dependency — nothing to guard.

### 4. Documentation

Update CLAUDE.md's contrib section:

- `EXPECTED_USER_VERSION` drops off the manual-mirror list; only
  `MAX_ART_BYTES` remains hand-mirrored.
- The regen command joins the vendoring note: after a schema change, run
  `MUSEFS_REGEN_SCHEMA_PY=1 cargo test -p musefs-db schema_py`, then
  `python contrib/python-musefs/vendor_to_picard.py`.

## Testing

- The two new Rust tests above run in plain `cargo test` (CI's existing gate).
- All three contrib suites keep their existing tests; only the fixture's
  source changes. `test_store_db.py`'s `check_schema_version` assertion now
  exercises the derived `EXPECTED_USER_VERSION` path end to end.
- Failure modes covered:
  - Schema change without regen → `schema_py_fixture_is_fresh` fails in CI.
  - Regen without re-vendoring → `test_vendor_sync.py` fails in CI.
  - Hand-edit of the generated module → caught by `schema_py_fixture_is_fresh`.
  - Render/migrate divergence → `schema_sql_matches_migrate` fails.

## Out of scope

- `MAX_ART_BYTES` stays hand-mirrored: it lives in `musefs-core/src/scan.rs`,
  not the schema, and a second generator for one constant isn't worth it.
- Plugins creating databases at runtime from `SCHEMA_SQL`: DB creation stays
  `musefs scan`'s job. The module ships in the package as a natural
  consequence of its form, but it is a test/contract artifact.
