# Schema v1 reset (collapse migrations) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Collapse the five historical SQLite migrations (`MIGRATION_V1`..`V5`) into a single baseline `MIGRATION_V1` so a fresh database stamps `user_version = 1`, in preparation for the v1.0.0 release.

**Architecture:** Keep the migration framework (`MIGRATIONS` array + `migrate()` loop) but reduce it to one element holding the current post-V5 schema as plain `CREATE` statements. The collapsed schema is *semantically* identical to a DB that ran all five migrations — verified by a throwaway PRAGMA-level A/B diff and by the retained CHECK/trigger/identity tests. Regenerate the vendored Python schema mirror and flip the two hardcoded `user_version == 5` expectations.

**Tech Stack:** Rust (rusqlite, SQLite `PRAGMA`), Python (pytest, ruff), the project's pre-commit gate (fmt, clippy `-D warnings`, full workspace test suite, ruff).

---

## ⚠️ Atomicity — read before starting

This change is **one self-consistent commit, created only in the final task.** Do **not** commit at intermediate task boundaries:

- After Task 2 the `musefs-db` *test target* will not compile (tests still reference removed constants) — Task 3 fixes it.
- Between Task 3 and Task 5 the tree is intentionally red: `schema_py_fixture_is_fresh` and the contrib `== 5` tests fail until the mirror is regenerated and the asserts flipped.

The pre-commit hook runs the full workspace suite and rejects red commits, so everything must be green together. The `MUSEFS_REGEN_SCHEMA_PY=1` regen + re-vendor (Task 5) must run **before** staging. The reference spec is `docs/superpowers/specs/2026-06-12-schema-v1-reset-design.md`.

> **Per the repo owner's standing instruction, do not run `git commit` until the user explicitly approves.** Task 7 prepares the commit; confirm before executing it.

---

## Task 1: Capture the pre-collapse semantic schema (A/B baseline)

A throwaway harness that dumps the *semantic* schema (column shape/order, foreign keys, indexes) of a fully-migrated DB. Run it now (on the current 5-migration code) to capture the baseline, then again after the collapse (Task 3) — the diff must be empty. We dump PRAGMAs, **not** raw `sqlite_master.sql` table text, because the old form (V4 `CREATE` + V5 `ALTER`) legitimately differs textually from one clean `CREATE`.

**Files:**
- Modify: `musefs-db/src/schema.rs` (add a `#[cfg(test)]` module at end of file)

- [ ] **Step 1: Add the throwaway dump test**

Insert this module at the very end of `musefs-db/src/schema.rs` (after `migration_v5_tests`):

```rust
// THROWAWAY: A/B harness for the v1 schema collapse. Dumps the semantic schema
// (PRAGMA-level) of a fully-migrated DB so the collapsed baseline can be proven
// equivalent to the 5-migration schema. Remove in the final task of the plan.
#[cfg(test)]
mod ab_dump {
    use rusqlite::Connection;
    use rusqlite::types::Value;

    #[test]
    #[ignore = "throwaway A/B harness for the schema collapse; remove before commit"]
    fn dump_semantic_schema() {
        let path = std::env::var("MUSEFS_AB_DUMP").expect("set MUSEFS_AB_DUMP to an output path");
        let mut conn = Connection::open_in_memory().unwrap();
        super::migrate(&mut conn).unwrap();
        let mut out = String::new();
        for t in [
            "tracks",
            "tags",
            "art",
            "track_art",
            "structural_blocks",
            "track_changes",
        ] {
            // (pragma, skip_leading_cols): index_list's first column is a volatile
            // creation-order `seq`; drop it so creation order does not cause a
            // false diff. table_info's `cid` (column 0) IS kept — it pins order.
            for (pragma, skip) in [("table_info", 0usize), ("foreign_key_list", 0), ("index_list", 1)] {
                let mut stmt = conn.prepare(&format!("PRAGMA {pragma}('{t}')")).unwrap();
                let ncol = stmt.column_count();
                let mut rows = stmt.query([]).unwrap();
                let mut lines: Vec<String> = Vec::new();
                while let Some(r) = rows.next().unwrap() {
                    let cells: Vec<String> = (skip..ncol)
                        .map(|i| format!("{:?}", r.get::<_, Value>(i).unwrap()))
                        .collect();
                    lines.push(cells.join("|"));
                }
                lines.sort();
                out.push_str(&format!("== {t}.{pragma} ==\n"));
                out.push_str(&lines.join("\n"));
                out.push('\n');
            }
        }
        // Trigger and explicit-index bodies, copied verbatim from the old
        // literals: a typo in a copied body would diverge HERE (the only check
        // that compares against the pre-collapse text — identity_tests is
        // self-referential post-collapse). Autoindexes have NULL sql and are
        // covered by index_list above.
        let mut stmt = conn
            .prepare(
                "SELECT type, name, sql FROM sqlite_master \
                 WHERE type IN ('trigger','index') AND sql IS NOT NULL \
                 AND name NOT LIKE 'sqlite_%' ORDER BY type, name",
            )
            .unwrap();
        let mut rows = stmt.query([]).unwrap();
        let mut lines: Vec<String> = Vec::new();
        while let Some(r) = rows.next().unwrap() {
            lines.push(format!(
                "{}|{}|{}",
                r.get::<_, String>(0).unwrap(),
                r.get::<_, String>(1).unwrap(),
                r.get::<_, String>(2).unwrap()
            ));
        }
        lines.sort();
        out.push_str("== triggers+indexes ==\n");
        out.push_str(&lines.join("\n"));
        out.push('\n');
        std::fs::write(&path, out).unwrap();
    }
}
```

- [ ] **Step 2: Run the dump on the current (5-migration) schema**

Run: `MUSEFS_AB_DUMP=/tmp/schema_before.txt cargo test -p musefs-db ab_dump::dump_semantic_schema -- --ignored --exact`
Expected: PASS. Then `test -s /tmp/schema_before.txt && echo OK` prints `OK` (non-empty file written).

- [ ] **Step 3: Sanity-check the captured baseline**

Run: `grep -c '== ' /tmp/schema_before.txt`
Expected: `19` (6 tables × 3 PRAGMAs, plus one `== triggers+indexes ==` section). Do **not** commit.

---

## Task 2: Collapse the migration constants

Replace the five migration constants with a single `MIGRATION_V1` holding the current post-V5 schema, and reduce `MIGRATIONS` to one element. Every table/trigger/index except `tracks` is copied verbatim from the existing literals; `tracks` is hand-assembled from V4's `CREATE` + V5's two `ALTER`s.

**Files:**
- Modify: `musefs-db/src/schema.rs:4-364` (the constants and the `MIGRATIONS` array)

- [ ] **Step 1: Replace the five `MIGRATION_V*` constants with one**

Delete `MIGRATION_V1`, `MIGRATION_V2`, `MIGRATION_V3`, `MIGRATION_V4`, `MIGRATION_V5` (lines 4–356, keeping the `CHANGELOG_CAP` const at 93–96) and replace the whole constant block with:

```rust
/// Ring capacity of the `track_changes` changelog. Must match the literal in
/// MIGRATION_V1 (guarded by `changelog_cap_constant_matches_migration_sql`).
#[allow(dead_code)]
pub const CHANGELOG_CAP: i64 = 8192;

// The complete v1.0.0 baseline schema. Collapsed from the five historical
// migrations: there are no pre-1.0 databases to upgrade, so the step-by-step
// path was removed. Semantically equivalent to applying the old V1..V5 in
// sequence (verified by the PRAGMA-level A/B check in the schema-reset plan and
// by the CHECK/trigger/identity tests below).
const MIGRATION_V1: &str = r"
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

CREATE TABLE tags (
    track_id   INTEGER NOT NULL REFERENCES tracks(id) ON DELETE CASCADE,
    key        TEXT NOT NULL,
    value      TEXT NOT NULL,
    ordinal    INTEGER NOT NULL DEFAULT 0,
    value_blob BLOB,
    PRIMARY KEY (track_id, key, ordinal),
    CHECK (ordinal >= 0),
    CHECK (value_blob IS NULL OR value = ''),
    CHECK (length(key) <= 256),
    CHECK (length(key) >= 1
           AND key NOT GLOB '*[' || char(1) || '-' || char(31) || ']*'),
    CHECK (length(value) <= 262144),
    CHECK (value_blob IS NULL OR length(value_blob) <= 16711680)
);

CREATE TABLE art (
    id       INTEGER PRIMARY KEY,
    sha256   TEXT NOT NULL UNIQUE,
    mime     TEXT NOT NULL,
    width    INTEGER,
    height   INTEGER,
    byte_len INTEGER NOT NULL,
    data     BLOB NOT NULL,
    CHECK (byte_len = length(data)),
    CHECK (length(sha256) = 64),
    CHECK (width IS NULL OR width >= 0),
    CHECK (height IS NULL OR height >= 0),
    CHECK (length(mime) <= 255),
    CHECK (byte_len <= 16711680)
);

CREATE TABLE track_art (
    track_id     INTEGER NOT NULL REFERENCES tracks(id) ON DELETE CASCADE,
    art_id       INTEGER NOT NULL REFERENCES art(id),
    picture_type INTEGER NOT NULL DEFAULT 3,
    description  TEXT NOT NULL DEFAULT '',
    ordinal      INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (track_id, ordinal),
    CHECK (picture_type BETWEEN 0 AND 20),
    CHECK (ordinal >= 0),
    CHECK (length(description) <= 1024)
);

-- Read-only, derived-from-file structural metadata (FLAC STREAMINFO/SEEKTABLE).
-- NOT part of the editable `tags` contract: external tools never touch it.
CREATE TABLE structural_blocks (
    track_id INTEGER NOT NULL REFERENCES tracks(id) ON DELETE CASCADE,
    kind     TEXT NOT NULL,
    ordinal  INTEGER NOT NULL DEFAULT 0,
    body     BLOB NOT NULL,
    PRIMARY KEY (track_id, kind, ordinal),
    CHECK (kind IN ('STREAMINFO','SEEKTABLE')),
    CHECK (ordinal >= 0),
    CHECK (length(body) <= 16777215)
);

-- Bounded changelog ring for O(changed) refresh. Every metadata edit funnels
-- through an UPDATE on the tracks row (the tags/track_art triggers), so
-- triggers on tracks alone capture all writers. Relies on SQLite nested
-- trigger activation (on by default; distinct from PRAGMA recursive_triggers).
CREATE TABLE track_changes (
    seq      INTEGER PRIMARY KEY AUTOINCREMENT,
    track_id INTEGER NOT NULL
);

-- Index the reverse art -> track_art edge so bulk orphan-GC and the art delete
-- trigger below do not scan the whole join table per deleted row.
CREATE INDEX track_art_art_id_idx ON track_art(art_id);

CREATE TRIGGER tags_ai AFTER INSERT ON tags BEGIN
    UPDATE tracks SET content_version = content_version + 1,
                      updated_at = CAST(strftime('%s','now') AS INTEGER)
    WHERE id = NEW.track_id;
END;
CREATE TRIGGER tags_au AFTER UPDATE ON tags BEGIN
    UPDATE tracks SET content_version = content_version + 1,
                      updated_at = CAST(strftime('%s','now') AS INTEGER)
    WHERE id = NEW.track_id;
END;
CREATE TRIGGER tags_ad AFTER DELETE ON tags BEGIN
    UPDATE tracks SET content_version = content_version + 1,
                      updated_at = CAST(strftime('%s','now') AS INTEGER)
    WHERE id = OLD.track_id;
END;

CREATE TRIGGER track_art_ai AFTER INSERT ON track_art BEGIN
    UPDATE tracks SET content_version = content_version + 1,
                      updated_at = CAST(strftime('%s','now') AS INTEGER)
    WHERE id = NEW.track_id;
END;
CREATE TRIGGER track_art_au AFTER UPDATE ON track_art BEGIN
    UPDATE tracks SET content_version = content_version + 1,
                      updated_at = CAST(strftime('%s','now') AS INTEGER)
    WHERE id = NEW.track_id;
END;
CREATE TRIGGER track_art_ad AFTER DELETE ON track_art BEGIN
    UPDATE tracks SET content_version = content_version + 1,
                      updated_at = CAST(strftime('%s','now') AS INTEGER)
    WHERE id = OLD.track_id;
END;

CREATE TRIGGER tracks_changelog_ai AFTER INSERT ON tracks BEGIN
    INSERT INTO track_changes (track_id) VALUES (NEW.id);
END;
CREATE TRIGGER tracks_changelog_au AFTER UPDATE ON tracks BEGIN
    INSERT INTO track_changes (track_id) VALUES (NEW.id);
END;
CREATE TRIGGER tracks_changelog_ad AFTER DELETE ON tracks BEGIN
    INSERT INTO track_changes (track_id) VALUES (OLD.id);
END;

-- Self-pruning ring: writers maintain it; the mount's read-only connections
-- never need to. Deletes only from the old end, so retained seqs stay contiguous.
CREATE TRIGGER track_changes_prune AFTER INSERT ON track_changes BEGIN
    DELETE FROM track_changes WHERE seq <= NEW.seq - 8192;
END;

-- art rows are content-addressed by sha256: once written, their content
-- columns are immutable. A writer needing different bytes/metadata inserts a
-- NEW row and relinks via track_art (which bumps content_version through the
-- track_art triggers). width/height use IS NOT (NULL-safe) because they are
-- nullable; the NOT NULL columns use <>.
CREATE TRIGGER art_reject_content_update
BEFORE UPDATE ON art
WHEN NEW.data   <> OLD.data
  OR NEW.sha256 <> OLD.sha256
  OR NEW.mime   <> OLD.mime
  OR NEW.byte_len <> OLD.byte_len
  OR NEW.width  IS NOT OLD.width
  OR NEW.height IS NOT OLD.height
BEGIN
    SELECT RAISE(ABORT,
        'art rows are immutable; insert a new content-addressed row and relink via track_art');
END;

-- Deleting an art row that still has track_art references (an orphan an
-- external writer can produce with foreign_keys OFF) bumps every referencing
-- track, so the mount rebuilds and serves a clean EIO on the orphan rather
-- than streaming stale bytes from an old cached layout. Inert on the normal
-- gc_orphan_art path, where the deleted row has no references.
CREATE TRIGGER art_ad AFTER DELETE ON art BEGIN
    UPDATE tracks SET content_version = content_version + 1,
                      updated_at = CAST(strftime('%s','now') AS INTEGER)
    WHERE id IN (SELECT track_id FROM track_art WHERE art_id = OLD.id);
END;

-- Scanner-owned geometry feeds the synthesized layout, but upsert_track does
-- not touch content_version. Bump it whenever a geometry column actually
-- changes, making content_version a true superset of served-byte inputs. The
-- WHEN guard is false on this trigger's own nested UPDATE (only content_version
-- changes), so the recursion terminates after exactly one bump.
CREATE TRIGGER tracks_geometry_au
AFTER UPDATE ON tracks
WHEN NEW.format        <> OLD.format
  OR NEW.audio_offset  <> OLD.audio_offset
  OR NEW.audio_length  <> OLD.audio_length
  OR NEW.backing_size  <> OLD.backing_size
  OR NEW.backing_mtime_ns <> OLD.backing_mtime_ns
BEGIN
    UPDATE tracks SET content_version = content_version + 1 WHERE id = NEW.id;
END;

-- FLAC structural blocks feed synthesized headers and flip the synthesis path
-- (legacy front-read fallback vs streamed fast path), so a change must bump.
-- set_structural_blocks is DELETE-then-INSERT (no UPDATE path exists), so these
-- fire on every rewrite; the resulting over-bump on a byte-identical re-probe
-- is harmless monotone churn (content_version is compared only for equality).
CREATE TRIGGER structural_blocks_ai AFTER INSERT ON structural_blocks BEGIN
    UPDATE tracks SET content_version = content_version + 1 WHERE id = NEW.track_id;
END;
CREATE TRIGGER structural_blocks_ad AFTER DELETE ON structural_blocks BEGIN
    UPDATE tracks SET content_version = content_version + 1 WHERE id = OLD.track_id;
END;
";
```

- [ ] **Step 2: Reduce the `MIGRATIONS` array to one element**

Replace the `MIGRATIONS` const (was lines 357–364):

```rust
const MIGRATIONS: &[&str] = &[MIGRATION_V1];
```

Leave `migrate()` and everything below it (`reference_objects`, `read_schema_objects`, `schema_mismatch`, `validate_identity`) unchanged.

- [ ] **Step 3: Verify the library compiles (test target will not yet — that is Task 3)**

Run: `cargo build -p musefs-db`
Expected: PASS (the `src` lib has no references to the removed constants). Do **not** commit.

---

## Task 3: Reorganize the `schema.rs` test modules

Delete the upgrade-path tests, fix the `MIGRATION_Vn`/`MIGRATIONS[n]` references and `uv == 5` asserts, and consolidate the surviving final-schema tests into purpose-named modules. After this task the crate test target compiles and the A/B diff is run.

**Files:**
- Modify: `musefs-db/src/schema.rs` (the `#[cfg(test)]` modules: `migration_v2_tests`, `migration_v3_tests`, `migration_v4_tests`, `migration_v5_tests`; leave `schema_py_tests`, `identity_tests`, and the `ab_dump` harness alone)

- [ ] **Step 1: Delete the five upgrade-path tests**

Delete these test functions entirely (they reference removed constants / the old `backing_mtime` column and no longer have meaning):

- `migration_v2_tests::v1_rows_survive_v2_migration_with_null_value_blob`
- `migration_v3_tests::v2_db_upgrades_to_v3_preserving_rows`
- `migration_v4_tests::v4_rebuild_preserves_fk_children`
- `migration_v4_tests::v4_rebuild_does_not_pump_changelog_ring`
- `migration_v4_tests::v4_rebuild_preserves_structural_blocks`

Also delete the now-unused helper `migration_v4_tests::insert_track_v3` (only the deleted rebuild tests used the old `backing_mtime` column).

- [ ] **Step 2: Rename the four `migration_vN_tests` modules to purpose-named modules**

Rename in place (keep each module's surviving tests and the helpers they use):

- `mod migration_v3_tests` → `mod changelog_tests`
- `mod migration_v4_tests` → `mod constraint_tests`
- `mod migration_v5_tests` → `mod art_immutability_tests`
- `mod migration_v2_tests` → `mod baseline_tests`

Then move three tests so each module holds only its kind of invariant:

- Move `v4_metadata_edit_bumps_version_and_appends_one_changelog_row` from `constraint_tests` into `changelog_tests`. **Rewrite its setup** — its body calls `fresh(&mut conn)`, but `fresh` lives only in `constraint_tests` (the old `migration_v4_tests`); the assertion doesn't need `foreign_keys` ON, so replace the `fresh(&mut conn)` call with `super::migrate(&mut conn).unwrap()`. `changelog_tests` already has its own `insert_track` and `count_changes`, so no helper duplication is needed. The exact rewritten body is given in Step 4.
- Move `changelog_cap_constant_matches_migration_sql` from `changelog_tests` into `baseline_tests`.
- Move `v4_check_literals_match_limits_constants` from `constraint_tests` into `baseline_tests` (renamed `check_literals_match_limits_constants` — see Step 3).

**Delete the now-orphaned `count_changes` helper from `constraint_tests`** (schema.rs:1256). Its only two callers were `v4_rebuild_does_not_pump_changelog_ring` (deleted in Step 1) and `v4_metadata_edit...` (moved out, above); leaving it would trip clippy `dead_code` under `-D warnings` and fail the pre-commit hook. `changelog_tests` keeps its own separate `count_changes` (schema.rs:561).

(Other helper fns — `insert_track`, `fresh`, `rejected`, `seed_track_and_art` — stay in the modules whose surviving tests use them. After the moves/deletes, confirm no helper is left unused: `cargo clippy --all-targets` in Step 6 of Task 7 is the backstop, but a missed orphan is the most likely reorg bug.)

- [ ] **Step 3: Fix the constant/index references in the moved drift guards**

In `baseline_tests`, update the two moved guards:

```rust
/// The SQL literal and the exported constant must not drift.
#[test]
fn changelog_cap_constant_matches_migration_sql() {
    assert!(super::MIGRATION_V1.contains(&format!("NEW.seq - {}", super::CHANGELOG_CAP)));
}
```

```rust
#[test]
fn check_literals_match_limits_constants() {
    use crate::limits::*;
    let sql = super::MIGRATION_V1;
    assert!(sql.contains(&format!("length(key) <= {MAX_TAG_KEY_LEN}")));
    assert!(sql.contains(&format!("length(value) <= {MAX_TAG_VALUE_LEN}")));
    assert!(sql.contains(&format!("length(value_blob) <= {MAX_BINARY_TAG_BYTES}")));
    assert!(sql.contains(&format!("length(mime) <= {MAX_ART_MIME_LEN}")));
    assert!(sql.contains(&format!("byte_len <= {MAX_ART_BYTES}")));
    assert!(sql.contains(&format!("length(description) <= {MAX_ART_DESCRIPTION_LEN}")));
    assert!(sql.contains(&format!("length(body) <= {MAX_STRUCTURAL_BODY_LEN}")));
    let kinds = STRUCTURAL_KINDS
        .iter()
        .map(|k| format!("'{k}'"))
        .collect::<Vec<_>>()
        .join(",");
    assert!(sql.contains(&format!("kind IN ({kinds})")));
}
```

- [ ] **Step 4: Flip every `assert_eq!(uv, 5)` to `1` in the surviving tests**

In the renamed modules, the following tests assert the migrated `user_version`; change each `assert_eq!(uv, 5)` (and the `uv2` re-run assert) to `1`:

- `constraint_tests::v4_valid_rows_migrate_and_read_cleanly`
- `changelog_tests::v3_changelog_records_insert_update_delete`
- (`v2_db_upgrades_to_v3_preserving_rows` also asserted `5` but was deleted in Step 1.)

Place the moved `v4_metadata_edit_bumps_version_and_appends_one_changelog_row` into `changelog_tests` with this exact body (the only change from the original is `fresh(&mut conn)` → `super::migrate(&mut conn).unwrap()`):

```rust
#[test]
fn v4_metadata_edit_bumps_version_and_appends_one_changelog_row() {
    let mut conn = Connection::open_in_memory().unwrap();
    super::migrate(&mut conn).unwrap();
    insert_track(&conn, "/a.flac");
    let cv_before: i64 = conn
        .query_row("SELECT content_version FROM tracks WHERE id=1", [], |r| {
            r.get(0)
        })
        .unwrap();
    let changes_before = count_changes(&conn);

    conn.execute(
        "INSERT INTO tags (track_id, key, value, ordinal) VALUES (1,'artist','A',0)",
        [],
    )
    .unwrap();

    let cv_after: i64 = conn
        .query_row("SELECT content_version FROM tracks WHERE id=1", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(cv_after, cv_before + 1, "content_version must bump by one");
    assert_eq!(
        count_changes(&conn),
        changes_before + 1,
        "exactly one changelog row from the edit (nested trigger)"
    );
}
```

Also retarget the two version-naming tests in `baseline_tests`:

Replace `baseline_tests::v2_adds_value_blob_and_structural_blocks_and_is_idempotent` with this rewrite (single migration, asserts `user_version == 1`):

```rust
#[test]
fn baseline_creates_value_blob_and_structural_blocks_and_is_idempotent() {
    let mut conn = Connection::open_in_memory().unwrap();
    super::migrate(&mut conn).unwrap();
    let uv: i64 = conn
        .pragma_query_value(None, "user_version", |r| r.get(0))
        .unwrap();
    assert_eq!(uv, 1);

    // value_blob exists on tags and defaults to NULL.
    conn.execute(
        "INSERT INTO tracks (backing_path, format, audio_offset, audio_length, \
         backing_size, backing_mtime_ns, updated_at) \
         VALUES ('/a.flac','flac',0,1,1,0,0)",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO tags (track_id, key, value, ordinal) VALUES (1,'artist','A',0)",
        [],
    )
    .unwrap();
    let blob_is_null: bool = conn
        .query_row(
            "SELECT value_blob IS NULL FROM tags WHERE track_id=1 AND key='artist'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(blob_is_null);

    // structural_blocks table accepts a row.
    conn.execute(
        "INSERT INTO structural_blocks (track_id, kind, ordinal, body) \
         VALUES (1,'STREAMINFO',0,X'00')",
        [],
    )
    .unwrap();

    // Re-running migrate is a no-op (idempotent).
    super::migrate(&mut conn).unwrap();
    let uv2: i64 = conn
        .pragma_query_value(None, "user_version", |r| r.get(0))
        .unwrap();
    assert_eq!(uv2, 1);
}
```

Rename `art_immutability_tests::migration_reaches_user_version_5` → `migration_reaches_user_version_1` and change its assert to `assert_eq!(uv, 1)`.

- [ ] **Step 5: Rewrite the trigger-presence test to assert the full 15-trigger set**

In `constraint_tests`, replace `v4_recreates_all_destroyed_triggers` with a fresh-DB invariant covering **all 15** baseline triggers (the old list omitted the five V5 triggers):

```rust
#[test]
fn fresh_db_has_all_baseline_triggers() {
    let mut conn = Connection::open_in_memory().unwrap();
    fresh(&mut conn);
    let names: Vec<String> = conn
        .prepare("SELECT name FROM sqlite_master WHERE type='trigger' ORDER BY name")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    for expected in [
        "tags_ai",
        "tags_au",
        "tags_ad",
        "track_art_ai",
        "track_art_au",
        "track_art_ad",
        "tracks_changelog_ai",
        "tracks_changelog_au",
        "tracks_changelog_ad",
        "track_changes_prune",
        "art_reject_content_update",
        "art_ad",
        "tracks_geometry_au",
        "structural_blocks_ai",
        "structural_blocks_ad",
    ] {
        assert!(
            names.iter().any(|n| n == expected),
            "missing trigger on fresh DB: {expected}"
        );
    }
    assert_eq!(names.len(), 15, "unexpected trigger count: {names:?}");
}
```

- [ ] **Step 6: Compile the test target**

Run: `cargo test -p musefs-db --no-run`
Expected: PASS (compiles). If it fails, it is almost always a leftover reference to `MIGRATION_V2`/`V3`/`V4`/`V5` or `MIGRATIONS[1]`/`[2]` — grep and fix:
Run: `grep -nE 'MIGRATION_V[2-5]|MIGRATIONS\[[1-9]' musefs-db/src/schema.rs`
Expected: no matches.

- [ ] **Step 7: Run the A/B diff — the collapse must be semantically equivalent**

Run: `MUSEFS_AB_DUMP=/tmp/schema_after.txt cargo test -p musefs-db ab_dump::dump_semantic_schema -- --ignored --exact && diff /tmp/schema_before.txt /tmp/schema_after.txt && echo EQUIVALENT`
Expected: prints `EQUIVALENT` (empty diff). A non-empty diff means a column/type/default/FK/index drifted — fix the `MIGRATION_V1` DDL before continuing.

- [ ] **Step 8: Run the `musefs-db` suite (one known failure expected)**

Run: `cargo test -p musefs-db`
Expected: all pass **except** `schema_py_tests::schema_py_fixture_is_fresh` (the vendored mirror is still the old 5-migration text — fixed in Task 5). Confirm that is the *only* failure. Do **not** commit.

---

## Task 4: Fix the remaining Rust references

Two small fixes outside `schema.rs`.

**Files:**
- Modify: `musefs-db/src/limits.rs:3`
- Modify: `musefs-db/tests/schema.rs:6,15,20`

- [ ] **Step 1: Fix the `limits.rs` doc reference**

In `musefs-db/src/limits.rs`, the module doc comment reads `... in [`crate::schema`] (`MIGRATION_V4`) ...`. Change `MIGRATION_V4` to `MIGRATION_V1`.

- [ ] **Step 2: Fix the integration-test `user_version` asserts**

In `musefs-db/tests/schema.rs`, change the three `user_version` assertions from `5` to `1` (lines ~6, ~15, ~20). Leave the `== 0` unmigrated-`Default` test (line ~29) unchanged.

Run: `grep -n ', 5)' musefs-db/tests/schema.rs`
Expected: no matches after the edit (the three `assert_eq!(..., 5)` sites are now `1`; the `== 0` unmigrated test on line ~29 is unaffected).

- [ ] **Step 3: Re-run the `musefs-db` suite**

Run: `cargo test -p musefs-db`
Expected: still only `schema_py_fixture_is_fresh` fails. Do **not** commit.

---

## Task 5: Regenerate and re-vendor the Python schema mirror

Regenerate `schema.py` from the collapsed `MIGRATIONS`, re-vendor it into Picard, and flip the two hardcoded `== 5` expectations.

**Files:**
- Regenerate: `contrib/python-musefs/src/musefs_common/schema.py`
- Re-vendor: `contrib/picard/musefs/_common/schema.py`
- Modify: `contrib/python-musefs/tests/test_constants.py`
- Modify: `contrib/picard/tests/test_conftest_sanity.py`

- [ ] **Step 1: Regenerate the mirror**

Run: `MUSEFS_REGEN_SCHEMA_PY=1 cargo test -p musefs-db schema_py`
Expected: PASS. Then verify the regenerated file:
Run: `grep -nE 'MIGRATION_V|USER_VERSION' contrib/python-musefs/src/musefs_common/schema.py`
Expected: exactly one `-- ── MIGRATION_V1 ──` banner, one `PRAGMA user_version = 1;`, and `USER_VERSION = 1`.

- [ ] **Step 2: Re-vendor into Picard**

Run: `python contrib/python-musefs/vendor_to_picard.py`
Expected: rewrites `contrib/picard/musefs/_common/schema.py`. Verify:
Run: `grep -nE 'MIGRATION_V|USER_VERSION' contrib/picard/musefs/_common/schema.py`
Expected: same single-`MIGRATION_V1` / `USER_VERSION = 1` result.

- [ ] **Step 3: Flip the python-musefs constant expectation**

In `contrib/python-musefs/tests/test_constants.py`, change:

```python
    assert constants.EXPECTED_USER_VERSION == 1
```

(was `== 5`).

- [ ] **Step 4: Flip the Picard conftest sanity expectation**

In `contrib/picard/tests/test_conftest_sanity.py`, change the assert (line ~8) from `== 5` to:

```python
        assert conn.execute("PRAGMA user_version").fetchone()[0] == 1
```

- [ ] **Step 5: Confirm `schema_py_fixture_is_fresh` now passes**

Run: `cargo test -p musefs-db schema_py`
Expected: PASS (no failures).

- [ ] **Step 6: Run the python-musefs test suite**

Run: `cd contrib/python-musefs && python -m pytest -q ; cd -`
Expected: PASS. (`test_errors.py`'s `SchemaMismatch(5)` and `test_public_api.py`'s `"0.1.0" != "1"` are independent of the expected version and still pass without edits.)

- [ ] **Step 7: Run the beets and Picard suites**

Run (beets, needs its venv): `contrib/beets/.venv/bin/python -m pytest contrib/beets -q`
Expected: PASS (conftest rebuilds from the regenerated `SCHEMA_SQL`).

Run (real Picard, or it silently skips): `PYTHONPATH=/usr/lib/picard /usr/bin/python3 -m pytest contrib/picard -q`
Expected: PASS. Do **not** commit.

---

## Task 6: Rewrite the migration-version documentation

Collapse the V1–V5 narrative in `ARCHITECTURE.md` and drop the stray "As of V5" qualifier in the python-musefs README. Preserve all feature/contract prose.

**Files:**
- Modify: `ARCHITECTURE.md` (the "The SQLite store" section, ~lines 112–201, plus lines 210 and 268)
- Modify: `contrib/python-musefs/README.md:135,144`
- Modify: `docs/OGG.md:85`

- [ ] **Step 1: Collapse the `ARCHITECTURE.md` schema narrative**

In `ARCHITECTURE.md`:

1. Line ~114–115: change "defines the schema as an append-only list of migrations (`MIGRATIONS`); `user_version` records how many have been applied." to "defines the schema as a single baseline migration (`MIGRATIONS`); `user_version` records the schema version (1)."
2. Replace the five bullets (`- **V1** — ...` through the end of the `- **V5** — ...` bullet) with one bullet that retains the feature prose, e.g.:

```markdown
- The **baseline schema** (`MIGRATION_V1`): the core tables — `tracks` (one row
  per backing file: path, format, audio byte range, size/nanosecond-mtime/ctime
  stamps, `content_version`), `tags` (multi-value key/value rows ordered by
  `ordinal`, with an optional `value_blob` for binary tags), `art`
  (content-addressed, deduplicated image blobs), `track_art` (per-track art
  links with picture type and ordering), and `structural_blocks` (read-only,
  derived-from-file FLAC `STREAMINFO`/`SEEKTABLE` metadata, **not** part of the
  editable contract). Deleting a track cascades to its `tags` and `track_art`
  rows. Triggers bump the owning track's `content_version`/`updated_at` on any
  `tags`/`track_art` edit; `CHECK` constraints enforce the contract invariants
  below at commit time. A bounded, self-pruning `track_changes` ring (capacity
  8192, `CHANGELOG_CAP`) fed by triggers on `tracks` gives O(changed) refresh —
  every metadata edit funnels through an `UPDATE` on the tracks row, relying on
  SQLite's nested trigger activation (on by default). Freshness-superset
  triggers make `content_version` cover every DB-knowable input to synthesized
  bytes: `art_reject_content_update` (art is content-addressed and immutable),
  `art_ad` (a deleted art row bumps referencing tracks so an orphan rebuilds to
  a clean serve-time error), `tracks_geometry_au` (scanner-owned geometry
  changes), and `structural_blocks_ai`/`_ad`.
```

3. Line ~164: change "**What the store enforces.** As of V4, SQLite `CHECK` constraints reject..." to "**What the store enforces.** SQLite `CHECK` constraints reject...".
4. Line ~194: change "as of V5 a trigger rejects any in-place `UPDATE`..." to "a trigger rejects any in-place `UPDATE`...".
5. Line ~210: change "The store's V4 `CHECK` rejects art over `MAX_ART_BYTES`..." to "The store's `CHECK` rejects art over `MAX_ART_BYTES`...".
6. Line ~268: change "...and FLAC structural-block changes (V5)." to "...and FLAC structural-block changes." (drop the trailing `(V5)`).

Leave the "external-writer contract" bullet list, the "Schema identity" paragraph, and everything not version-stamped verbatim.

- [ ] **Step 2: Drop the version qualifiers in the python-musefs README**

In `contrib/python-musefs/README.md`:
- Line ~135: change "run `musefs scan` (i.e. `run_scan`). V4 `CHECK` constraints reject malformed" to "run `musefs scan` (i.e. `run_scan`). `CHECK` constraints reject malformed".
- Line ~144: change "**Art rows are immutable.** As of V5 a trigger rejects in-place updates of an" to "**Art rows are immutable.** A trigger rejects in-place updates of an".

- [ ] **Step 3: Drop the "V4" qualifier in OGG.md**

In `docs/OGG.md:85`, change "is rejected by the store's V4 `CHECK`, with a resolve-time cap" to "is rejected by the store's `CHECK`, with a resolve-time cap".

- [ ] **Step 4: Confirm no live doc still cites a removed migration version**

Run: `grep -rnE 'MIGRATION_V[2-5]|\bV[2-5]\b' ARCHITECTURE.md CONTRIBUTING.md README.md contrib/*/README.md docs/*.md 2>/dev/null`
Expected: no matches (matches inside `docs/superpowers/specs|plans/` are out of scope and not searched here). Do **not** commit.

---

## Task 7: Remove the A/B harness, run the full gate, and commit

**Files:**
- Modify: `musefs-db/src/schema.rs` (delete the `ab_dump` module)

- [ ] **Step 1: Delete the throwaway A/B harness**

Remove the entire `mod ab_dump { ... }` module (and its leading `// THROWAWAY` comment) added in Task 1.

Run: `grep -n 'ab_dump\|MUSEFS_AB_DUMP' musefs-db/src/schema.rs`
Expected: no matches.

- [ ] **Step 2: Format and lint**

Run: `cargo fmt --all && cargo fmt --all --check && cargo clippy --all-targets`
Expected: clippy clean (no warnings; the workspace denies them).

- [ ] **Step 3: Full workspace test suite**

Run: `cargo test`
Expected: PASS (all crates; FUSE e2e excluded as usual).

- [ ] **Step 4: Metrics-feature tests (separate from the default run)**

Run: `cargo test -p musefs-core --features metrics`
Expected: PASS. (These assert exact getattr/read stat counts and are not in the default workspace run; the schema change does not touch the read path, but the gate's `check` job runs them.)

- [ ] **Step 5: Re-run the contrib Python suites**

Run: `cd contrib/python-musefs && python -m pytest -q ; cd -`
Run: `contrib/beets/.venv/bin/python -m pytest contrib/beets -q`
Run: `PYTHONPATH=/usr/lib/picard /usr/bin/python3 -m pytest contrib/picard -q`
Expected: all PASS.

- [ ] **Step 6: Review the diff, then commit (only after user approval)**

Run: `git status && git diff --stat`
Expected staged set (by name — do not `git add -A`): `musefs-db/src/schema.rs`, `musefs-db/src/limits.rs`, `musefs-db/tests/schema.rs`, `contrib/python-musefs/src/musefs_common/schema.py`, `contrib/picard/musefs/_common/schema.py`, `contrib/python-musefs/tests/test_constants.py`, `contrib/picard/tests/test_conftest_sanity.py`, `ARCHITECTURE.md`, `contrib/python-musefs/README.md`, `docs/OGG.md`, and the spec/plan docs under `docs/superpowers/`.

Confirm with the user, then:

```bash
git add musefs-db/src/schema.rs musefs-db/src/limits.rs musefs-db/tests/schema.rs \
  contrib/python-musefs/src/musefs_common/schema.py \
  contrib/picard/musefs/_common/schema.py \
  contrib/python-musefs/tests/test_constants.py \
  contrib/picard/tests/test_conftest_sanity.py \
  ARCHITECTURE.md contrib/python-musefs/README.md docs/OGG.md \
  docs/superpowers/specs/2026-06-12-schema-v1-reset-design.md \
  docs/superpowers/plans/2026-06-12-schema-v1-reset.md
git commit -m "$(cat <<'EOF'
refactor(db): collapse migrations into a single v1 baseline schema

Reset the SQLite schema version to 1 for the v1.0.0 release. musefs is
pre-release, so there are no databases to upgrade: the V1..V5 migration
path is collapsed into one MIGRATION_V1 holding the current schema. The
collapsed schema is semantically identical to the post-V5 result
(verified via a PRAGMA-level A/B diff and the retained CHECK/trigger/
identity tests). Regenerates and re-vendors the Python schema mirror and
updates the docs that narrated the migration history.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>
EOF
)"
```

The pre-commit hook re-runs fmt/clippy/full-suite/ruff; if it rejects, fix the reported issue, re-stage by name, and create a **new** commit (never `--amend`, never `--no-verify`).

---

## Self-review notes

- **Spec coverage:** §1 collapse → Task 2; §2 in-tree fixups → Tasks 3–4; §3 docs → Task 6; §4 test surgery → Task 3; §5 Python regen → Task 5; semantic A/B + atomic-commit verification → Tasks 1/3/7. All spec sections map to a task.
- **No placeholders:** every code/SQL block is concrete; relocations name exact source/destination modules and the exact assert edits.
- **Naming consistency:** `MIGRATION_V1`, `MIGRATIONS`, `CHANGELOG_CAP`, the new module names (`constraint_tests`, `changelog_tests`, `art_immutability_tests`, `baseline_tests`), and the renamed tests (`check_literals_match_limits_constants`, `baseline_creates_value_blob_and_structural_blocks_and_is_idempotent`, `migration_reaches_user_version_1`, `fresh_db_has_all_baseline_triggers`) are used consistently across tasks.
