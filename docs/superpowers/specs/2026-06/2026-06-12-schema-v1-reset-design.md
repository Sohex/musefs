# Schema v1 reset (collapse migrations for v1.0.0)

## Goal

In preparation for the v1.0.0 release, reset the SQLite store's schema version
to **1** and collapse the five historical migrations (`MIGRATION_V1`..`V5`) into
a single baseline migration representing the current (post-V5) schema. musefs is
pre-release, so there are no deployed databases to upgrade — the historical
step-by-step migration path carries no value and only adds surface area.

This is a mechanical reset, not a schema change. The collapsed baseline is
**semantically equivalent** to a DB that has run all five migrations: identical
tables, columns (and column order), constraints, triggers, and index, and
identical accept/reject/trigger *behavior*. The stored `sqlite_master.sql`
*text* may differ from the old form — V4 rebuilt tables with inline CHECKs and
V5 then `ALTER`-appended a column, whereas the baseline is a single clean
`CREATE` — and that is fine: nothing compares against the old text.
`validate_identity()` checks a live DB against a reference that, after this
change, is **also** built from the new `MIGRATION_V1` (so it is self-consistent),
and the external plugins only check `user_version`, not DDL text.

## Mechanism (decided)

Keep the migration framework. `MIGRATIONS` stays an array and `migrate()` keeps
its per-step `user_version`-stamping loop, so a post-1.0 schema change is added
by appending `MIGRATION_V2`. After this reset the array holds a single element,
so `migrate()` stamps `user_version` to **1**.

## Changes

### 1. Collapse the migrations — `musefs-db/src/schema.rs`

Replace `MIGRATION_V1`..`MIGRATION_V5` with a single `MIGRATION_V1` holding the
final post-V5 schema as plain `CREATE` statements — no `ALTER`, no V4-style
table-rebuild/stash dance.

**Assembly rule (to avoid hand-formatting drift):** copy the `CREATE TABLE`
statements for `tags`/`art`/`track_art`/`structural_blocks`, the
`track_changes` table, the `track_art_art_id_idx` index, and **all** triggers
verbatim from the existing V3/V4/V5 literals — they are already clean `CREATE`
statements and carry the exact constraint/trigger text. Only `tracks` is
hand-assembled, because its final form is V4's `CREATE TABLE` *plus* V5's two
`ALTER`s (rename `backing_mtime` -> `backing_mtime_ns`, which also rewrites the
`backing_mtime >= 0` CHECK; and append `backing_ctime_ns`). Its exact target
DDL is:

```sql
CREATE TABLE tracks (
    id               INTEGER PRIMARY KEY,
    backing_path     TEXT NOT NULL UNIQUE,
    format           TEXT NOT NULL,
    audio_offset     INTEGER NOT NULL,
    audio_length     INTEGER NOT NULL,
    backing_size     INTEGER NOT NULL,
    backing_mtime_ns INTEGER NOT NULL,
    content_version  INTEGER NOT NULL DEFAULT 0,
    updated_at       INTEGER NOT NULL,
    backing_ctime_ns INTEGER NOT NULL DEFAULT 0 CHECK (backing_ctime_ns >= 0),
    CHECK (format IN ('flac','mp3','m4a','opus','vorbis','oggflac','wav')),
    CHECK (audio_offset >= 0),
    CHECK (audio_length >= 0),
    CHECK (backing_size >= 0),
    CHECK (backing_mtime_ns >= 0),
    CHECK (content_version >= 0),
    CHECK (updated_at >= 0),
    CHECK (audio_offset + audio_length <= backing_size)
);
```

`backing_ctime_ns` is the last column with its CHECK attached at the column
level (matching V5's `ALTER ... ADD COLUMN ... CHECK`). Nothing in the Rust code
depends on `tracks` column order — `tracks.rs:14` selects an explicit column
list, and the only `SELECT *` on `tracks` is inside V4's rebuild (being deleted)
— so this ordering is for fidelity, not correctness.

The full baseline thus contains:

- **Tables:** `tracks` (above), `tags` (`value_blob` + V4 CHECKs), `art`
  (V4 CHECKs), `track_art` (V4 CHECKs), `structural_blocks` (V4 CHECKs),
  `track_changes`.
- **Index:** `track_art_art_id_idx`.
- **Triggers (15):** `tags_ai`/`au`/`ad`, `track_art_ai`/`au`/`ad`,
  `tracks_changelog_ai`/`au`/`ad`, `track_changes_prune`,
  `art_reject_content_update`, `art_ad`, `tracks_geometry_au`,
  `structural_blocks_ai`/`ad`.

`MIGRATIONS` becomes `&[MIGRATION_V1]`. `migrate()`, `reference_objects()`,
`read_schema_objects()`, `schema_mismatch()`, and `validate_identity()` are
unchanged. `CHANGELOG_CAP` stays (it now guards the `MIGRATION_V1` literal).

### 2. In-tree reference fixups

- `musefs-db/src/schema.rs` `CHANGELOG_CAP` rustdoc (currently "Must match the
  literal in MIGRATION_V3"): `MIGRATION_V3` -> `MIGRATION_V1`.
- `changelog_cap_constant_matches_migration_sql`: `MIGRATIONS[2]` -> `MIGRATION_V1`.
- `v4_check_literals_match_limits_constants`: `MIGRATION_V4` -> `MIGRATION_V1`.
- `musefs-db/src/limits.rs:3` doc comment: `MIGRATION_V4` -> `MIGRATION_V1`.
- `musefs-db/tests/schema.rs`: the three `user_version == 5` asserts -> `== 1`.
  The `== 0` unmigrated-`Default` test is unchanged (its "always-migrated 1"
  comment already describes the new state).

### 3. Documentation rewrites (in scope)

- **`ARCHITECTURE.md` "The SQLite store" section (~lines 112–201).** The V1–V5
  bullet narrative (`- V1 — ...` through `- V5 — ...`) and the "append-only list
  of migrations" framing describe history that no longer exists. Collapse the
  five bullets into one "the baseline schema" description that **retains the
  feature prose** (core tables + cascade/version triggers, binary tags +
  structural blocks, the self-pruning `track_changes` changelog ring, the CHECK
  constraints, the freshness-superset triggers), and reword the contract
  section's version-stamped phrasings: "As of V4, SQLite `CHECK` constraints…"
  (line 164) -> "The store enforces…"; "as of V5 a trigger rejects…" (lines
  143/194) -> drop the "as of V5" qualifier. Keep it minimal — preserve the
  external-writer-contract content verbatim where it is not version-stamped.
- **`contrib/python-musefs/README.md`** — drop the version qualifiers at line
  135 ("V4 `CHECK` constraints…") and line 144 ("As of V5 a trigger rejects…").
- **`docs/OGG.md:85`** — drop the "V4" qualifier ("the store's V4 `CHECK`" ->
  "the store's `CHECK`").
- A `grep -rnE 'MIGRATION_V[2-5]|\bV[2-5]\b'` over the tracked non-historical
  docs (ARCHITECTURE, CONTRIBUTING, READMEs, `docs/*.md`) must come back empty
  after the rewrites.
- Historical plan docs under `docs/superpowers/plans/` (e.g. the
  backing-freshness plan referencing `MIGRATION_V5`) are **left as-is** — the
  repo treats `plans/` as a historical record.

### 4. Test surgery — `schema.rs` test modules

Delete the upgrade-path tests (they validate `Vn -> Vn+1` transitions that no
longer exist; they are exactly the tests referencing `MIGRATIONS[1]`/`[2]` and
the old-`backing_mtime` `insert_track_v3` helper, so they would not even
compile):

- `v1_rows_survive_v2_migration_with_null_value_blob`
- `v2_db_upgrades_to_v3_preserving_rows`
- `v4_rebuild_preserves_fk_children`
- `v4_rebuild_does_not_pump_changelog_ring`
- `v4_rebuild_preserves_structural_blocks`

Keep every final-schema invariant test, fixing any `uv == 5` -> `uv == 1`, and
reorganize the version-named modules into purpose-named ones. Target layout
(every surviving test gets an explicit home):

- **`constraint_tests`** — all CHECK rejection/acceptance tests for
  `tracks`/`tags`/`art`/`track_art`/`structural_blocks` (the bulk of
  `migration_v4_tests`), plus `v4_valid_rows_migrate_and_read_cleanly` and the
  trigger-presence test (see below).
- **`changelog_tests`** — `v3_changelog_records_insert_update_delete`,
  `v3_bare_tag_insert_produces_changelog_row_via_nested_trigger`,
  `v3_prune_keeps_ring_bounded_and_contiguous`.
- **`art_immutability_tests`** — `art_content_update_is_rejected`,
  `art_noop_update_is_allowed`, `deleting_referenced_art_bumps_tracks`,
  `deleting_unreferenced_art_bumps_nothing`.
- **`baseline_tests`** — the migrate-idempotency + `value_blob`/structural
  existence test (rewritten from `v2_adds_value_blob_and_structural_blocks_and_is_idempotent`
  against the single migration, asserting `user_version == 1`), the
  `migration_reaches_user_version_5` test (renamed/retargeted to assert
  `user_version == 1`), and the two SQL-literal drift guards
  (`changelog_cap_constant_matches_migration_sql`,
  `v4_check_literals_match_limits_constants`, with the constant-name fixups from
  §2).
- **`identity_tests`** and **`schema_py_tests`** — unchanged (already generic
  over `MIGRATIONS`).

The `v4_recreates_all_destroyed_triggers` test is rewritten as a plain
"fresh DB has all expected triggers" invariant in `constraint_tests`, asserting
the **full 15-trigger set** above — including the five V5 triggers
(`art_reject_content_update`, `art_ad`, `tracks_geometry_au`,
`structural_blocks_ai`/`ad`), which the current 10-name list omits. This
preserves coverage that the collapse did not silently drop a trigger.

No invariant coverage is lost in the delete-set: the FK-cascade-survival and
changelog-not-pumped behaviors those tests proved were V4-rebuild-specific and
genuinely no longer exist.

### 5. Regenerate + re-vendor the Python mirror

`MUSEFS_REGEN_SCHEMA_PY=1 cargo test -p musefs-db schema_py` rewrites
`contrib/python-musefs/src/musefs_common/schema.py` (single
`-- ── MIGRATION_V1 ──` banner, `USER_VERSION = 1`). Then
`python contrib/python-musefs/vendor_to_picard.py` re-vendors the Picard copy
(`contrib/picard/musefs/_common/schema.py`). Then flip the two hardcoded
expectations:

- `contrib/python-musefs/tests/test_constants.py`: `== 5` -> `== 1`.
- `contrib/picard/tests/test_conftest_sanity.py`: `== 5` -> `== 1`.

No change needed to `test_errors.py` (`SchemaMismatch(5)` is a literal `found`
value, independent of the expected version) or `test_public_api.py`
(`"0.1.0" != "1"` still holds). beets/lidarr/picard conftests build from
`SCHEMA_SQL` and get the regenerated (semantically identical) schema; the
`PRAGMA user_version = 99` mismatch tests are unaffected.

## Verification

- `cargo test` (full workspace) + `cargo clippy --all-targets` + `cargo fmt --check`.
- `cargo test -p musefs-core --features metrics` (CI `check` job; not in the
  default workspace run).
- Contrib Python suites: beets venv (`contrib/beets/.venv/bin/python`),
  system-Picard (`/usr/bin/python3` + `PYTHONPATH` to `/usr/lib/picard` and
  dist-packages PyQt5), python-musefs.
- **One-shot semantic A/B** to replace the lost cross-version guard (the
  retained `schema_sql_matches_migrate` only proves render == `migrate()`, and
  after collapse both sides are the new `MIGRATION_V1`): build a DB from the
  pre-collapse schema (the five migrations on `main`/HEAD) and a DB from the
  collapsed baseline, then diff their *semantic* schema — per-table
  `PRAGMA table_info`, `PRAGMA foreign_key_list`, `PRAGMA index_list`/`index_info`,
  and the set of trigger `{name, sql}` from `sqlite_master`. These must be
  identical. Do **not** diff raw `sqlite_master.sql` table text — it will
  legitimately differ (V4 CREATE + V5 ALTER vs one clean CREATE). Table-level
  CHECK constraints are not enumerable via PRAGMA; they are covered by the
  retained CHECK-rejection tests. This is a throwaway implementation-time check,
  not a committed test (the old `MIGRATION_V2`..`V5` literals no longer exist).

**Atomicity.** The schema collapse, the in-tree reference fixups, the mirror
regen + re-vendor, and all `== 5` assert flips form one self-consistent change:
deleting the migrations makes `schema_py_fixture_is_fresh` and the contrib
`== 5` tests red until the regen/re-vendor/flips land. The pre-commit hook runs
the full workspace suite and rejects red commits, so these must be staged
together as one commit, and the `MUSEFS_REGEN_SCHEMA_PY=1` regen + re-vendor
must run *before* staging.

`sqlite_sequence` is a non-issue: `read_schema_objects` filters
`name NOT LIKE 'sqlite_%'`, so the AUTOINCREMENT-created table never enters the
identity reference; the collapse does not change this.

## Out of scope

- No behavioral schema change — served bytes and accepted/rejected rows are
  unchanged from today.
- No new migration-version doc; no unrelated refactoring or cleanup beyond the
  documentation rewrites in §3.
- The `fuzz/` crate (out of workspace) is unaffected — no format-layer API
  changes here.
