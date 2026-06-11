# Field-Size Caps + Schema Identity Gate Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Harden the read-only serve path against a crafted or stale `--db` by validating schema identity at open and capping over-sized/under-constrained store fields at both write time (V4 `CHECK`s) and read time (fail-closed db-layer guards).

**Architecture:** Two complementary layers. (1) A schema-identity gate (`schema::validate_identity`) compares the opened DB's `sqlite_master` against a reference built by replaying `migrate()` in-memory, plus `PRAGMA foreign_key_check`; it runs in `open_readonly` and post-`migrate` in `configure`, rejecting anything that is not the canonical latest. (2) Length/count caps fold into the existing V4 migration as `CHECK`s and are re-enforced by reader guards that check `length()` SQL-side before materializing a value (a crafted DB can carry the canonical schema yet smuggle a CHECK-violating row via `PRAGMA ignore_check_constraints`).

**Tech Stack:** Rust, `rusqlite` 0.40 (bundled SQLite, `blob`, `fallible_uint`), `thiserror`, `tempfile` (dev). Workspace is strictly layered `musefs-db → musefs-format → musefs-core → musefs-fuse`; the db layer cannot depend on the format layer.

**Spec:** `docs/superpowers/specs/2026-06-11-field-size-caps-design.md`

**Source of truth read before writing this plan (exact current state):**
- `musefs-db/src/schema.rs`: `migrate` (264–284), `MIGRATION_V4` (124–260), `MIGRATIONS` (262), test helpers `fresh`/`insert_track`/`v4_rebuild_preserves_fk_children`, drift helpers `render_schema_sql`/`schema_sql_matches_migrate`/`schema_py_fixture_is_fresh`.
- `musefs-db/src/lib.rs`: `Db::<ReadWrite>::{open,open_in_memory,configure}` (48–98), `Db::<ReadOnly>::open_readonly` (100–119).
- `musefs-db/src/error.rs`: `DbError` (2–14).
- `musefs-db/src/tags.rs`: `get_tags`, `tags_for_tracks`, `tags_grouped`, `tags_grouped_for_keys`, `get_binary_tags`.
- `musefs-db/src/art.rs`: `get_art_meta`, `get_track_art`.
- `musefs-db/src/structural.rs`: `get_structural_blocks` (15–31).
- `musefs-core/src/error.rs`: `CoreError` — has `Db(#[from] DbError)`. `musefs-fuse/src/lib.rs::errno` (91–108) already collapses `CoreError::Db(_)` to `EIO`, so **new `DbError` variants need no fuse change**.
- `musefs-format/src/flac.rs:148`: `pub(crate) const MAX_BLOCK_BODY: u64 = 0x00FF_FFFF`.

**General conventions:**
- Per `~/.claude/CLAUDE.md` this repo uses Serena tools for code reads/edits; the pre-commit hook runs fmt + clippy `-D warnings` + the **full workspace test suite**, so every commit must be green. Docs-only commits skip the cargo gate.
- Run the whole workspace suite before each commit in a code task: `cargo test` (and `cargo clippy --all-targets` if you touched signatures).
- Commit messages: end with the `Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>` trailer (HEREDOC to preserve formatting). Stage files by name.

---

## Task 1: `musefs-db::limits` module (public cap constants)

Single home for every cap, made `pub` so cross-layer drift tests (Task 7) can assert equality and so unused-in-one-commit constants never trip the dead-code lint.

**Files:**
- Create: `musefs-db/src/limits.rs`
- Modify: `musefs-db/src/lib.rs` (add `pub mod limits;`)

- [ ] **Step 1: Write the failing test**

Create `musefs-db/src/limits.rs` with the constants and a self-consistency test:

```rust
//! Size and identity caps enforced at the DB boundary (#267/#269/#278).
//!
//! The `CHECK` constraints in [`crate::schema`] (`MIGRATION_V4`) enforce these
//! at write time for honest writers; the reader guards in [`crate::tags`],
//! [`crate::art`] and [`crate::structural`] re-enforce them at read time,
//! because a crafted DB can carry the canonical schema yet smuggle a
//! CHECK-violating row (`PRAGMA ignore_check_constraints`). Values are public so
//! cross-layer drift tests can assert they match the format ceiling and the
//! scanner caps.

/// Max `tags.key` length. Compared against SQLite `length()` (i64).
pub const MAX_TAG_KEY_LEN: i64 = 256;
/// Max `tags.value` length in bytes — 256 KiB.
pub const MAX_TAG_VALUE_LEN: i64 = 262_144;
/// Max `art.mime` length.
pub const MAX_ART_MIME_LEN: i64 = 255;
/// Max `track_art.description` length — 1 KiB.
pub const MAX_ART_DESCRIPTION_LEN: i64 = 1024;
/// Max `structural_blocks.body` length in bytes. Mirrors
/// `musefs_format::flac::MAX_BLOCK_BODY` (FLAC's 24-bit block limit); the db
/// layer cannot depend on the format layer, so the equality is asserted by a
/// `musefs-core` test (see the plan, Task 7).
pub const MAX_STRUCTURAL_BODY_LEN: i64 = 0x00FF_FFFF;
/// Max tag rows materialized per track, applied to the text and binary sets
/// independently.
pub const MAX_TAGS_PER_TRACK: usize = 4096;
/// Valid `structural_blocks.kind` values. Single source for the V4 `CHECK`
/// (asserted by a drift test) and the `get_structural_blocks` guard.
pub const STRUCTURAL_KINDS: [&str; 2] = ["STREAMINFO", "SEEKTABLE"];
/// `tags.value_blob` length cap in bytes — defense-in-depth `CHECK` only (the
/// blob streams at read time, so no reader guard). Mirrors `musefs-core`'s
/// `MAX_BINARY_TAG_BYTES`.
pub const MAX_BINARY_TAG_BYTES: i64 = 16_711_680;
/// `art.byte_len` cap in bytes — defense-in-depth `CHECK` only. Mirrors
/// `musefs-core`'s `MAX_ART_BYTES`.
pub const MAX_ART_BYTES: i64 = 16_711_680;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cap_values_are_pinned() {
        assert_eq!(MAX_TAG_VALUE_LEN, 256 * 1024);
        assert_eq!(MAX_ART_DESCRIPTION_LEN, 1024);
        assert_eq!(MAX_STRUCTURAL_BODY_LEN, 0x00FF_FFFF);
        assert_eq!(MAX_BINARY_TAG_BYTES, 16 * 1024 * 1024 - 64 * 1024);
        assert_eq!(MAX_ART_BYTES, 16 * 1024 * 1024 - 64 * 1024);
        assert_eq!(STRUCTURAL_KINDS, ["STREAMINFO", "SEEKTABLE"]);
    }
}
```

- [ ] **Step 2: Register the module**

In `musefs-db/src/lib.rs`, add to the module list near the other `mod` declarations (e.g. next to `mod error;`):

```rust
pub mod limits;
```

- [ ] **Step 3: Run the test (expect compile + pass)**

Run: `cargo test -p musefs-db limits::`
Expected: PASS (`cap_values_are_pinned`).

- [ ] **Step 4: Lint**

Run: `cargo clippy -p musefs-db --all-targets`
Expected: no warnings.

- [ ] **Step 5: Commit**

```bash
git add musefs-db/src/limits.rs musefs-db/src/lib.rs
git commit -m "$(cat <<'EOF'
feat(db): add limits module with field-size cap constants

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 2: #270 schema identity gate

Reject any DB whose `sqlite_master` is not byte-identical to a freshly migrated reference, plus `foreign_key_check`. Wire into `open_readonly` (load-bearing) and `configure` (post-migrate assertion).

**Files:**
- Modify: `musefs-db/src/error.rs` (add `SchemaMismatch`)
- Modify: `musefs-db/src/schema.rs` (add `validate_identity`, `reference_objects`, `read_schema_objects`; tests)
- Modify: `musefs-db/src/lib.rs` (call the gate in `open_readonly` and `configure`)

- [ ] **Step 1: Add the `SchemaMismatch` error variant**

In `musefs-db/src/error.rs`, add to `DbError` (after `AudioBoundsOutOfRange`):

```rust
    #[error(
        "database schema does not match the version musefs expects (mismatch at {object}); \
         regenerate the store by running `musefs scan` against the library"
    )]
    SchemaMismatch { object: String },
```

- [ ] **Step 2: Write the failing tests for `validate_identity`**

In `musefs-db/src/schema.rs`, add a new test module at the end of the file (after `migration_v4_tests`):

```rust
#[cfg(test)]
mod identity_tests {
    use super::*;
    use crate::error::DbError;

    fn migrated() -> Connection {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.pragma_update(None, "foreign_keys", true).unwrap();
        migrate(&mut conn).unwrap();
        conn
    }

    #[test]
    fn honest_schema_passes() {
        let conn = migrated();
        validate_identity(&conn).unwrap();
    }

    #[test]
    fn honest_schema_with_rows_passes() {
        // A written-to DB gains `sqlite_sequence` (track_changes is AUTOINCREMENT,
        // pumped by the tracks_changelog trigger on insert). The `sqlite_%` filter
        // excludes it from both sides, so an honest populated DB must still pass —
        // this guards against the gate ever false-rejecting a real mount.
        let conn = migrated();
        conn.execute(
            "INSERT INTO tracks (backing_path, format, audio_offset, audio_length, \
             backing_size, backing_mtime, updated_at) VALUES ('/a.flac','flac',0,1,1,0,0)",
            [],
        )
        .unwrap();
        let has_seq: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE name = 'sqlite_sequence'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(has_seq, 1, "precondition: insert created sqlite_sequence");
        validate_identity(&conn).unwrap();
    }

    #[test]
    fn missing_trigger_is_rejected() {
        let conn = migrated();
        conn.execute_batch("DROP TRIGGER tags_ai").unwrap();
        let err = validate_identity(&conn).unwrap_err();
        match err {
            DbError::SchemaMismatch { object } => {
                assert!(object.contains("tags_ai"), "names the object: {object}");
                assert!(object.contains("missing"), "classifies it: {object}");
            }
            other => panic!("expected SchemaMismatch, got {other:?}"),
        }
    }

    #[test]
    fn extra_object_is_rejected() {
        let conn = migrated();
        conn.execute_batch("CREATE TABLE sneaky (x)").unwrap();
        let err = validate_identity(&conn).unwrap_err();
        assert!(matches!(err, DbError::SchemaMismatch { .. }));
    }

    #[test]
    fn altered_table_is_rejected() {
        // Rebuild `tags` without the V4 CHECKs but with the same name.
        let conn = migrated();
        conn.execute_batch(
            "PRAGMA foreign_keys=OFF; \
             DROP TABLE tags; \
             CREATE TABLE tags (track_id INTEGER NOT NULL, key TEXT, value TEXT, \
                ordinal INTEGER, value_blob BLOB, PRIMARY KEY (track_id, key, ordinal));",
        )
        .unwrap();
        let err = validate_identity(&conn).unwrap_err();
        match err {
            DbError::SchemaMismatch { object } => assert!(object.contains("tags")),
            other => panic!("expected SchemaMismatch, got {other:?}"),
        }
    }

    #[test]
    fn foreign_key_violation_is_rejected() {
        let conn = migrated();
        // Insert an orphan track_art with FK enforcement off, then validate.
        conn.execute_batch(
            "PRAGMA foreign_keys=OFF; \
             INSERT INTO art (sha256, mime, byte_len, data) VALUES ('a', 'image/png', 1, X'00'); \
             INSERT INTO track_art (track_id, art_id, picture_type, ordinal) VALUES (999, 1, 3, 0);",
        )
        .unwrap();
        let err = validate_identity(&conn).unwrap_err();
        match err {
            DbError::SchemaMismatch { object } => assert!(object.contains("foreign key")),
            other => panic!("expected SchemaMismatch (fk), got {other:?}"),
        }
    }

    #[test]
    fn first_offender_is_deterministic_in_type_name_order() {
        // Two differences; the (type, name)-ordered first one must be reported.
        // 'index'/'table'/'trigger' sort before 'trigger'; drop a trigger AND a
        // table-level object and assert the lexicographically-first key wins.
        let conn = migrated();
        conn.execute_batch("PRAGMA foreign_keys=OFF; DROP TRIGGER track_art_ai; DROP TRIGGER tags_ai;")
            .unwrap();
        // Both are triggers; (type,name) order makes "tags_ai" < "track_art_ai".
        let err = validate_identity(&conn).unwrap_err();
        match err {
            DbError::SchemaMismatch { object } => assert!(object.contains("tags_ai"), "{object}"),
            other => panic!("expected SchemaMismatch, got {other:?}"),
        }
    }
}
```

- [ ] **Step 3: Run the tests to verify they fail**

Run: `cargo test -p musefs-db identity_tests`
Expected: FAIL — `validate_identity` is not defined.

- [ ] **Step 4: Implement the gate**

In `musefs-db/src/schema.rs`, add `use` lines at the top if not present (`std::collections::BTreeMap`, `std::sync::OnceLock`, `crate::error::DbError`) and insert the gate functions immediately after `migrate` (after line 284):

```rust
/// Canonical schema object set, built once by replaying `migrate()` on a fresh
/// in-memory DB. Keyed by `(type, name)` with the verbatim `sqlite_master.sql`.
/// Process-global and invariant — there is no feature-gated migration variation.
fn reference_objects() -> &'static BTreeMap<(String, String), String> {
    static REF: OnceLock<BTreeMap<(String, String), String>> = OnceLock::new();
    REF.get_or_init(|| {
        let mut conn =
            Connection::open_in_memory().expect("in-memory connection for schema reference");
        migrate(&mut conn).expect("reference migration must succeed on a fresh DB");
        read_schema_objects(&conn).expect("reading reference schema must succeed")
    })
}

/// All user objects (excludes SQLite's internal `sqlite_*` tables/autoindexes),
/// keyed `(type, name)` → verbatim `sql` (NULL `sql` — e.g. autoindexes — maps
/// to "" but those are filtered by the `sqlite_%` clause).
fn read_schema_objects(conn: &Connection) -> Result<BTreeMap<(String, String), String>> {
    let mut stmt = conn.prepare(
        "SELECT type, name, COALESCE(sql, '') FROM sqlite_master \
         WHERE name NOT LIKE 'sqlite_%'",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok((
            (r.get::<_, String>(0)?, r.get::<_, String>(1)?),
            r.get::<_, String>(2)?,
        ))
    })?;
    let mut map = BTreeMap::new();
    for row in rows {
        let ((ty, name), sql) = row?;
        map.insert((ty, name), sql);
    }
    Ok(map)
}

fn schema_mismatch(key: &(String, String), what: &str) -> DbError {
    DbError::SchemaMismatch {
        object: format!("{} {} ({what})", key.0, key.1),
    }
}

/// Reject any DB whose schema is not byte-identical to the canonical latest
/// (#270). The reference is built by replaying `migrate()`, so honest
/// same-version DBs match exactly. Also runs `PRAGMA foreign_key_check`, which
/// works regardless of the `foreign_keys` enforcement pragma (the read-only
/// mount never sets it).
pub(crate) fn validate_identity(conn: &Connection) -> Result<()> {
    let reference = reference_objects();
    let actual = read_schema_objects(conn)?;

    // Walk the union of keys in (type, name) order; report the first that
    // differs. A key is in exactly one class (missing | unexpected | altered),
    // so no per-key tie-break is needed.
    let mut keys: Vec<&(String, String)> = reference.keys().chain(actual.keys()).collect();
    keys.sort();
    keys.dedup();
    for key in keys {
        match (reference.get(key), actual.get(key)) {
            (Some(r), Some(a)) if r != a => return Err(schema_mismatch(key, "altered")),
            (Some(_), None) => return Err(schema_mismatch(key, "missing")),
            (None, Some(_)) => return Err(schema_mismatch(key, "unexpected")),
            _ => {}
        }
    }

    let mut fk = conn.prepare("PRAGMA foreign_key_check")?;
    let mut rows = fk.query([])?;
    if let Some(row) = rows.next()? {
        let table: String = row.get(0)?;
        return Err(DbError::SchemaMismatch {
            object: format!("foreign key violation in table {table}"),
        });
    }
    Ok(())
}
```

- [ ] **Step 5: Run the gate tests to verify they pass**

Run: `cargo test -p musefs-db identity_tests`
Expected: PASS (all six).

- [ ] **Step 6: Wire the gate into the open paths + write the open_readonly file test**

In `musefs-db/src/lib.rs`, in `configure`, change the last line before `Ok(())`:

```rust
        schema::migrate(conn)?;
        schema::validate_identity(conn)?;
        Ok(())
```

In `open_readonly`, after `conn.busy_timeout(Duration::from_secs(5))?;` and before constructing `Db`:

```rust
        conn.busy_timeout(Duration::from_secs(5))?;
        schema::validate_identity(&conn)?;
```

Then add a test exercising the real read-only file path. Put it in the existing `tests` module in `lib.rs` (or create one if absent):

```rust
#[test]
fn open_readonly_rejects_tampered_schema() {
    let file = tempfile::NamedTempFile::new().unwrap();
    let path = file.path().to_path_buf();
    {
        let db = Db::open(&path).unwrap(); // fresh, fully migrated
        db.conn.execute_batch("DROP TRIGGER tags_ai").unwrap();
    }
    let err = Db::open_readonly(&path).unwrap_err();
    assert!(
        matches!(err, crate::DbError::SchemaMismatch { .. }),
        "tampered RO open must be rejected, got {err:?}"
    );
}

#[test]
fn open_readonly_accepts_honest_schema() {
    let file = tempfile::NamedTempFile::new().unwrap();
    let path = file.path().to_path_buf();
    Db::open(&path).unwrap();
    Db::open_readonly(&path).unwrap();
}

#[test]
fn open_readonly_rejects_foreign_key_violation() {
    // The read-only mount never sets `PRAGMA foreign_keys = ON`, so this proves
    // `foreign_key_check` (which is independent of the enforcement pragma)
    // catches an orphan on the actual RO path (spec Section 6).
    let file = tempfile::NamedTempFile::new().unwrap();
    let path = file.path().to_path_buf();
    {
        let db = Db::open(&path).unwrap();
        db.conn
            .execute_batch(
                "PRAGMA foreign_keys=OFF; \
                 INSERT INTO art (sha256, mime, byte_len, data) VALUES ('a','image/png',1,X'00'); \
                 INSERT INTO track_art (track_id, art_id, picture_type, ordinal) VALUES (999, 1, 3, 0);",
            )
            .unwrap();
    }
    let err = Db::open_readonly(&path).unwrap_err();
    match err {
        crate::DbError::SchemaMismatch { object } => assert!(object.contains("foreign key")),
        other => panic!("expected SchemaMismatch (fk) on RO open, got {other:?}"),
    }
}
```

Note: `Db::open` uses WAL; `open_readonly`'s doc notes it needs a writable directory for the `-shm` index — `NamedTempFile` lives in a writable tmp dir, so this works.

**Serve-path coverage (for the reviewer of this plan):** `DbPool::new` seeds the pool from a connection opened by `Db::open(...).into_read_only()` (`db_pool.rs:54`), whose identity was already validated by the post-`migrate` gate in `configure`; per-thread additional connections are opened via `Db::open_readonly` (`db_pool.rs:90`), gated by the new call site here. Both serve-path connection kinds are therefore covered; `into_read_only` itself needs no gate (it only re-types an already-validated connection).

- [ ] **Step 7: Run the full db suite + lint**

Run: `cargo test -p musefs-db`
Expected: PASS (including the two new lib tests and the prior suite).
Run: `cargo clippy -p musefs-db --all-targets`
Expected: no warnings.

- [ ] **Step 8: Commit**

```bash
git add musefs-db/src/error.rs musefs-db/src/schema.rs musefs-db/src/lib.rs
git commit -m "$(cat <<'EOF'
feat(db): validate schema identity at open instead of trusting user_version (#270)

Reject any DB whose sqlite_master is not byte-identical to a freshly migrated
reference, plus foreign_key_check, in open_readonly and post-migrate configure.
SchemaMismatch states the problem and the remedy (run `musefs scan`).

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 3: V4 `CHECK` constraints + `structural_blocks` rebuild + schema.py regen

Fold every cap into `MIGRATION_V4` and add `structural_blocks` to the rebuild set (fixing the latent cascade-on-`DROP TABLE tracks` data-loss bug). No Rust reader changes here — commits stay green because CHECKs are additive and the schema.py mirror is regenerated in the same commit.

**Files:**
- Modify: `musefs-db/src/schema.rs` (`MIGRATION_V4` body + doc comment; new tests)
- Modify: `contrib/python-musefs/src/musefs_common/schema.py` (regenerated)
- Modify: `contrib/picard/musefs/_common/schema.py` (re-vendored)

- [ ] **Step 1: Add the `tags`/`art`/`track_art` length CHECKs**

In `MIGRATION_V4`, the `tags` table create has `CHECK (ordinal >= 0)` and `CHECK (value_blob IS NULL OR value = '')`. Add two more inside the `tags` `CREATE TABLE`:

```sql
    CHECK (length(key) <= 256),
    CHECK (length(value) <= 262144),
    CHECK (value_blob IS NULL OR length(value_blob) <= 16711680),
```

In the `art` `CREATE TABLE` (which has `CHECK (byte_len = length(data))` etc.), add:

```sql
    CHECK (length(mime) <= 255),
    CHECK (byte_len <= 16711680),
```

In the `track_art` `CREATE TABLE` (which has `CHECK (picture_type BETWEEN 0 AND 20)` and `CHECK (ordinal >= 0)`), add:

```sql
    CHECK (length(description) <= 1024),
```

- [ ] **Step 2: Add `structural_blocks` to the V4 rebuild set**

Four edits inside the `MIGRATION_V4` raw string:

(a) **Stash** — alongside the other `CREATE TEMP TABLE _m4_*` lines at the top:

```sql
CREATE TEMP TABLE _m4_structural AS SELECT * FROM structural_blocks;
```

(b) **Drop before `tracks`** — in the `DROP TABLE` group, add `structural_blocks` so it is dropped before `tracks` (it is a child of `tracks` via `ON DELETE CASCADE`):

```sql
DROP TABLE track_art;
DROP TABLE tags;
DROP TABLE art;
DROP TABLE structural_blocks;
DROP TABLE tracks;
```

(c) **Recreate after `tracks`** — after the `CREATE TABLE track_art (...)` block, add the constrained recreate:

```sql
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
```

(d) **Refill + drop the stash** — alongside the other `INSERT INTO … SELECT … FROM _m4_*` and `DROP TABLE _m4_*` lines:

```sql
INSERT INTO structural_blocks SELECT * FROM _m4_structural;
DROP TABLE _m4_structural;
```

- [ ] **Step 3: Fix the V4 doc comment**

The block comment above `MIGRATION_V4` says `track_changes_prune (ON track_changes) and structural_blocks are NOT rebuilt`. Update it so it states that `structural_blocks` **is** now rebuilt (stash-before-drop preserves its rows past the `tracks` cascade) and only `track_changes` is left untouched. Match the surrounding comment style and reflow.

- [ ] **Step 4: Write the CHECK rejection + boundary tests**

In `migration_v4_tests`, add tests using the existing `fresh`/`seed_track_and_art`/`rejected` helpers. Examples (add the analogous accept/reject pair per field):

```rust
#[test]
fn v4_tags_rejects_oversize_key() {
    let mut conn = Connection::open_in_memory().unwrap();
    fresh(&mut conn);
    insert_track(&conn, "/a.flac");
    let key = "k".repeat(257);
    rejected(
        &conn,
        &format!("INSERT INTO tags (track_id, key, value, ordinal) VALUES (1, '{key}', 'v', 0)"),
    );
}

#[test]
fn v4_tags_accepts_key_at_cap() {
    let mut conn = Connection::open_in_memory().unwrap();
    fresh(&mut conn);
    insert_track(&conn, "/a.flac");
    let key = "k".repeat(256);
    conn.execute(
        &format!("INSERT INTO tags (track_id, key, value, ordinal) VALUES (1, '{key}', 'v', 0)"),
        [],
    )
    .unwrap();
}

#[test]
fn v4_tags_rejects_oversize_value() {
    let mut conn = Connection::open_in_memory().unwrap();
    fresh(&mut conn);
    insert_track(&conn, "/a.flac");
    let big = "v".repeat(262_145);
    rejected(
        &conn,
        &format!("INSERT INTO tags (track_id, key, value, ordinal) VALUES (1, 'k', '{big}', 0)"),
    );
}

#[test]
fn v4_structural_rejects_unknown_kind_and_negative_ordinal_and_oversize_body() {
    let mut conn = Connection::open_in_memory().unwrap();
    fresh(&mut conn);
    insert_track(&conn, "/a.flac");
    rejected(
        &conn,
        "INSERT INTO structural_blocks (track_id, kind, ordinal, body) VALUES (1, 'APPLICATION', 0, X'00')",
    );
    rejected(
        &conn,
        "INSERT INTO structural_blocks (track_id, kind, ordinal, body) VALUES (1, 'STREAMINFO', -1, X'00')",
    );
    // length(body) cap: a blob of MAX+1 zero bytes via zeroblob().
    rejected(
        &conn,
        "INSERT INTO structural_blocks (track_id, kind, ordinal, body) VALUES (1, 'STREAMINFO', 0, zeroblob(16777216))",
    );
}

#[test]
fn v4_structural_accepts_body_at_cap() {
    let mut conn = Connection::open_in_memory().unwrap();
    fresh(&mut conn);
    insert_track(&conn, "/a.flac");
    conn.execute(
        "INSERT INTO structural_blocks (track_id, kind, ordinal, body) VALUES (1, 'STREAMINFO', 0, zeroblob(16777215))",
        [],
    )
    .unwrap();
}

#[test]
fn v4_art_rejects_oversize_mime_and_byte_len() {
    let mut conn = Connection::open_in_memory().unwrap();
    fresh(&mut conn);
    let mime = "x".repeat(256);
    rejected(
        &conn,
        &format!("INSERT INTO art (sha256, mime, byte_len, data) VALUES ('{}', '{mime}', 1, X'00')", "a".repeat(64)),
    );
    // byte_len cap (byte_len must equal length(data), so use a zeroblob).
    rejected(
        &conn,
        &format!("INSERT INTO art (sha256, mime, byte_len, data) VALUES ('{}', 'image/png', 16711681, zeroblob(16711681))", "b".repeat(64)),
    );
}

#[test]
fn v4_track_art_rejects_oversize_description() {
    let mut conn = Connection::open_in_memory().unwrap();
    fresh(&mut conn);
    seed_track_and_art(&conn);
    let desc = "d".repeat(1025);
    rejected(
        &conn,
        &format!("INSERT INTO track_art (track_id, art_id, picture_type, description, ordinal) VALUES (1, 1, 3, '{desc}', 0)"),
    );
}
```

- [ ] **Step 5: Write the cascade-regression test (structural rows survive V2/V3→V4)**

Model it on `v4_rebuild_preserves_fk_children` (which applies V1–V3, stamps `user_version=3`, inserts, then `migrate`s to V4). Add:

```rust
#[test]
fn v4_rebuild_preserves_structural_blocks() {
    let mut conn = Connection::open_in_memory().unwrap();
    conn.pragma_update(None, "foreign_keys", true).unwrap();
    conn.execute_batch(super::MIGRATIONS[0]).unwrap();
    conn.execute_batch(super::MIGRATIONS[1]).unwrap();
    conn.execute_batch(super::MIGRATIONS[2]).unwrap();
    conn.pragma_update(None, "user_version", 3i64).unwrap();

    insert_track(&conn, "/legacy.flac");
    conn.execute(
        "INSERT INTO structural_blocks (track_id, kind, ordinal, body) VALUES (1, 'STREAMINFO', 0, X'AABB')",
        [],
    )
    .unwrap();

    super::migrate(&mut conn).unwrap();

    let n: i64 = conn
        .query_row("SELECT COUNT(*) FROM structural_blocks", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 1, "structural_blocks must survive the V4 tracks rebuild");
    let body: Vec<u8> = conn
        .query_row("SELECT body FROM structural_blocks WHERE track_id = 1", [], |r| r.get(0))
        .unwrap();
    assert_eq!(body, vec![0xAA, 0xBB]);
}
```

- [ ] **Step 6: Write the value-tied drift test**

Add to `migration_v4_tests` (or alongside the schema_py tests). This is the N9 strengthening — exact predicate substrings tied to the `limits` constants:

```rust
#[test]
fn v4_check_literals_match_limits_constants() {
    use crate::limits::*;
    let v4 = super::MIGRATION_V4;
    assert!(v4.contains(&format!("length(key) <= {MAX_TAG_KEY_LEN}")));
    assert!(v4.contains(&format!("length(value) <= {MAX_TAG_VALUE_LEN}")));
    assert!(v4.contains(&format!("length(value_blob) <= {MAX_BINARY_TAG_BYTES}")));
    assert!(v4.contains(&format!("length(mime) <= {MAX_ART_MIME_LEN}")));
    assert!(v4.contains(&format!("byte_len <= {MAX_ART_BYTES}")));
    assert!(v4.contains(&format!("length(description) <= {MAX_ART_DESCRIPTION_LEN}")));
    assert!(v4.contains(&format!("length(body) <= {MAX_STRUCTURAL_BODY_LEN}")));
    let kinds = STRUCTURAL_KINDS
        .iter()
        .map(|k| format!("'{k}'"))
        .collect::<Vec<_>>()
        .join(",");
    assert!(v4.contains(&format!("kind IN ({kinds})")));
}
```

- [ ] **Step 7: Run the new tests (expect FAIL before, PASS after the SQL edits)**

If you wrote the tests before the SQL edits, they fail first. With Steps 1–3 applied:
Run: `cargo test -p musefs-db migration_v4_tests`
Expected: PASS, including the new rejection/boundary/cascade/drift tests.

Then check whether the schema rendering drift test now fails (it should, because the mirror is stale):
Run: `cargo test -p musefs-db schema_py_fixture_is_fresh`
Expected: FAIL — the rendered `SCHEMA_SQL` changed; the committed `schema.py` is stale.

- [ ] **Step 8: Regenerate + re-vendor the Python mirror**

Run: `MUSEFS_REGEN_SCHEMA_PY=1 cargo test -p musefs-db schema_py`
This rewrites `contrib/python-musefs/src/musefs_common/schema.py`. Then re-vendor to Picard:
Run: `python contrib/python-musefs/vendor_to_picard.py`
This rewrites `contrib/picard/musefs/_common/schema.py`.

Re-run to confirm green:
Run: `cargo test -p musefs-db schema_py`
Expected: PASS (`schema_py_fixture_is_fresh` and `schema_sql_matches_migrate`).

- [ ] **Step 9: Full db suite + lint**

Run: `cargo test -p musefs-db`
Expected: PASS.
Run: `cargo clippy -p musefs-db --all-targets`
Expected: no warnings.

- [ ] **Step 10: Commit**

```bash
git add musefs-db/src/schema.rs contrib/python-musefs/src/musefs_common/schema.py contrib/picard/musefs/_common/schema.py
git commit -m "$(cat <<'EOF'
feat(db): cap field sizes as V4 CHECKs and rebuild structural_blocks (#267 #269 #278)

Fold length caps for tags.key/value/value_blob, art.mime/byte_len, and
track_art.description into V4; add structural_blocks to the rebuild set with
kind/ordinal/body constraints, which also fixes the latent
cascade-on-DROP-TABLE-tracks data loss. Regenerate and re-vendor schema.py.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 4: `structural_blocks` reader guard

Fail closed at `get_structural_blocks` for a smuggled oversize body / unknown kind / negative ordinal, checking `length(body)` SQL-side before materializing the blob.

**Files:**
- Modify: `musefs-db/src/error.rs` (add `FieldTooLarge`, `InvalidStructuralBlock`)
- Modify: `musefs-db/src/structural.rs` (`get_structural_blocks` body)

- [ ] **Step 1: Add the error variants and the shared length-only guard helper**

In `musefs-db/src/error.rs`, add to `DbError`:

```rust
    #[error("{table}.{field} length {len} exceeds the {max} cap (crafted or corrupt DB)")]
    FieldTooLarge {
        table: &'static str,
        field: &'static str,
        len: i64,
        max: i64,
    },
    #[error("structural block for track {track_id} is invalid: {detail} (crafted or corrupt DB)")]
    InvalidStructuralBlock { track_id: i64, detail: String },
```

Then add the shared guard helper at module scope in `error.rs` (every reader guard routes its length check through this — it is a **pure function of the SQL-computed `length(col)`** and never receives the value, which is the observable proof that rejection is allocation-free, spec N13):

```rust
/// Reject a field whose SQL-computed `length()` exceeds `max`, before the value
/// is ever materialized. Takes only the length, so by construction it cannot
/// touch the (potentially huge) payload — the allocation-free guarantee the
/// reader guards rely on (spec N13).
pub(crate) fn check_field_len(
    table: &'static str,
    field: &'static str,
    len: i64,
    max: i64,
) -> Result<()> {
    if len > max {
        return Err(DbError::FieldTooLarge { table, field, len, max });
    }
    Ok(())
}

#[cfg(test)]
mod guard_helper_tests {
    use super::check_field_len;

    #[test]
    fn rejects_on_length_only_inclusive_boundary() {
        // The decision is a pure function of length — the value is never passed
        // in, so an over-cap row provably cannot be materialized to reject it.
        assert!(check_field_len("tags", "value", 262_145, 262_144).is_err());
        assert!(check_field_len("tags", "value", 262_144, 262_144).is_ok());
    }
}
```

- [ ] **Step 2: Write the failing tests**

Add a test module to `musefs-db/src/structural.rs`. The crafted rows use `PRAGMA ignore_check_constraints=ON` so the V4 CHECK does not reject the INSERT (simulating a hostile writer that shipped a canonical-schema file):

```rust
#[cfg(test)]
mod guard_tests {
    use crate::error::DbError;
    use crate::{Db, Format, NewTrack};

    fn db_with_track() -> (Db, i64) {
        let db = Db::open_in_memory().unwrap();
        let id = db
            .upsert_track(&NewTrack {
                backing_path: "/a.flac".into(),
                format: Format::Flac,
                audio_offset: 0,
                audio_length: 1,
                backing_size: 1,
                backing_mtime: 0,
            })
            .unwrap();
        (db, id)
    }

    #[test]
    fn rejects_oversize_body() {
        let (db, id) = db_with_track();
        db.conn
            .execute_batch("PRAGMA ignore_check_constraints=ON")
            .unwrap();
        db.conn
            .execute(
                "INSERT INTO structural_blocks (track_id, kind, ordinal, body) \
                 VALUES (?1, 'STREAMINFO', 0, zeroblob(16777216))",
                rusqlite::params![id],
            )
            .unwrap();
        let err = db.get_structural_blocks(id).unwrap_err();
        assert!(matches!(err, DbError::FieldTooLarge { field: "body", .. }), "{err:?}");
    }

    #[test]
    fn accepts_body_at_cap() {
        let (db, id) = db_with_track();
        db.conn
            .execute(
                "INSERT INTO structural_blocks (track_id, kind, ordinal, body) \
                 VALUES (?1, 'STREAMINFO', 0, zeroblob(16777215))",
                rusqlite::params![id],
            )
            .unwrap();
        let rows = db.get_structural_blocks(id).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].body.len(), 16_777_215);
    }

    #[test]
    fn rejects_unknown_kind() {
        let (db, id) = db_with_track();
        db.conn
            .execute_batch("PRAGMA ignore_check_constraints=ON")
            .unwrap();
        db.conn
            .execute(
                "INSERT INTO structural_blocks (track_id, kind, ordinal, body) \
                 VALUES (?1, 'APPLICATION', 0, X'00')",
                rusqlite::params![id],
            )
            .unwrap();
        let err = db.get_structural_blocks(id).unwrap_err();
        assert!(matches!(err, DbError::InvalidStructuralBlock { .. }), "{err:?}");
    }

    #[test]
    fn rejects_negative_ordinal() {
        // Reading ordinal as i64 (not u64) is what lets this reach our guard
        // instead of failing as DbError::Sqlite(OutOfRange) at the column read.
        let (db, id) = db_with_track();
        db.conn
            .execute_batch("PRAGMA ignore_check_constraints=ON")
            .unwrap();
        db.conn
            .execute(
                "INSERT INTO structural_blocks (track_id, kind, ordinal, body) \
                 VALUES (?1, 'STREAMINFO', -1, X'00')",
                rusqlite::params![id],
            )
            .unwrap();
        let err = db.get_structural_blocks(id).unwrap_err();
        assert!(matches!(err, DbError::InvalidStructuralBlock { .. }), "{err:?}");
    }
}
```

- [ ] **Step 3: Run to verify failure**

Run: `cargo test -p musefs-db structural::guard_tests`
Expected: FAIL (compile error: variants used / behavior not implemented).

- [ ] **Step 4: Implement the guard**

Replace the body of `get_structural_blocks` (`musefs-db/src/structural.rs:15-31`) with a manual row loop that checks `length(body)` SQL-side before reading `body`:

```rust
    pub fn get_structural_blocks(&self, track_id: i64) -> Result<Vec<StructuralBlock>> {
        let mut stmt = self.conn.prepare(
            "SELECT kind, ordinal, length(body), body FROM structural_blocks \
             WHERE track_id = ?1 ORDER BY kind, ordinal",
        )?;
        let mut rows = stmt.query(params![track_id])?;
        let mut out = Vec::new();
        while let Some(r) = rows.next()? {
            let kind: String = r.get(0)?;
            // Read ordinal as i64, NOT u64: `StructuralBlock.ordinal` is u64, but
            // rusqlite's `fallible_uint` would reject a smuggled negative value at
            // `get::<u64>` as DbError::Sqlite(OutOfRange) — masking it before our
            // own check and yielding the wrong error variant. Read i64, validate,
            // then cast.
            let ordinal: i64 = r.get(1)?;
            let body_len: i64 = r.get(2)?;
            if !crate::limits::STRUCTURAL_KINDS.contains(&kind.as_str()) {
                return Err(DbError::InvalidStructuralBlock {
                    track_id,
                    detail: format!("unknown kind {kind:?}"),
                });
            }
            if ordinal < 0 {
                return Err(DbError::InvalidStructuralBlock {
                    track_id,
                    detail: format!("negative ordinal {ordinal}"),
                });
            }
            crate::error::check_field_len(
                "structural_blocks",
                "body",
                body_len,
                crate::limits::MAX_STRUCTURAL_BODY_LEN,
            )?;
            out.push(StructuralBlock {
                kind,
                ordinal: u64::try_from(ordinal).expect("ordinal guarded >= 0 above"),
                body: r.get(3)?,
            });
        }
        Ok(out)
    }
```

Add `use crate::error::DbError;` to the top of `structural.rs` if not already imported. Keep the existing `use rusqlite::params;`.

- [ ] **Step 5: Run to verify pass + full db suite + lint**

Run: `cargo test -p musefs-db structural`
Expected: PASS (existing `structural` tests + `guard_tests`).
Run: `cargo test -p musefs-db && cargo clippy -p musefs-db --all-targets`
Expected: PASS, no warnings.

- [ ] **Step 6: Commit**

```bash
git add musefs-db/src/error.rs musefs-db/src/structural.rs
git commit -m "$(cat <<'EOF'
feat(db): fail closed on oversize/invalid structural blocks at read time (#269)

get_structural_blocks checks length(body) SQL-side before materializing, and
rejects unknown kind / negative ordinal, so a crafted DB carrying the canonical
schema cannot smuggle a CHECK-violating block past the serve path.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 5: tag reader guards + shared validated mapper

Guard `tags.key`/`tags.value` length and per-track row count across all five `tags` readers, checking lengths SQL-side before materializing.

**Files:**
- Modify: `musefs-db/src/error.rs` (add `TooManyValues`)
- Modify: `musefs-db/src/tags.rs` (refactor the five readers + shared helper; fix the `get_binary_tags` doc comment)

- [ ] **Step 1: Add the `TooManyValues` variant**

In `musefs-db/src/error.rs`, add to `DbError`:

```rust
    #[error("track {track_id} has {count} tag rows, exceeds the {max}-row cap (crafted or corrupt DB)")]
    TooManyValues { track_id: i64, count: usize, max: usize },
```

- [ ] **Step 2: Write the failing tests**

Add to the `tags_for_tracks_tests` module in `tags.rs`:

```rust
    #[test]
    fn get_tags_rejects_oversize_value() {
        let db = open_mem();
        let a = db.upsert_track(&new_track("/a.flac")).unwrap();
        db.conn.execute_batch("PRAGMA ignore_check_constraints=ON").unwrap();
        let big = "v".repeat(262_145);
        db.conn
            .execute(
                "INSERT INTO tags (track_id, key, value, ordinal) VALUES (?1, 'k', ?2, 0)",
                rusqlite::params![a, big],
            )
            .unwrap();
        let err = db.get_tags(a).unwrap_err();
        assert!(matches!(err, crate::DbError::FieldTooLarge { field: "value", .. }), "{err:?}");
    }

    #[test]
    fn get_tags_accepts_value_at_cap() {
        let db = open_mem();
        let a = db.upsert_track(&new_track("/a.flac")).unwrap();
        let at = "v".repeat(262_144);
        db.conn
            .execute(
                "INSERT INTO tags (track_id, key, value, ordinal) VALUES (?1, 'k', ?2, 0)",
                rusqlite::params![a, at],
            )
            .unwrap();
        assert_eq!(db.get_tags(a).unwrap()[0].value.len(), 262_144);
    }

    #[test]
    fn get_binary_tags_rejects_oversize_key() {
        let db = open_mem();
        let a = db.upsert_track(&new_track("/a.flac")).unwrap();
        db.conn.execute_batch("PRAGMA ignore_check_constraints=ON").unwrap();
        let key = "k".repeat(257);
        db.conn
            .execute(
                "INSERT INTO tags (track_id, key, value, value_blob, ordinal) VALUES (?1, ?2, '', X'00', 0)",
                rusqlite::params![a, key],
            )
            .unwrap();
        let err = db.get_binary_tags(a).unwrap_err();
        assert!(matches!(err, crate::DbError::FieldTooLarge { table: "tags", field: "key", .. }), "{err:?}");
    }

    #[test]
    fn per_track_count_cap_text_and_binary() {
        let db = open_mem();
        let a = db.upsert_track(&new_track("/a.flac")).unwrap();
        // 4097 text rows -> TooManyValues on get_tags.
        {
            let tx = db.conn.unchecked_transaction().unwrap();
            let mut stmt = tx
                .prepare("INSERT INTO tags (track_id, key, value, ordinal) VALUES (?1, 'k', 'v', ?2)")
                .unwrap();
            for i in 0..4097 {
                stmt.execute(rusqlite::params![a, i]).unwrap();
            }
            drop(stmt);
            tx.commit().unwrap();
        }
        let err = db.get_tags(a).unwrap_err();
        assert!(matches!(err, crate::DbError::TooManyValues { .. }), "{err:?}");
    }

    #[test]
    fn bulk_reader_rejects_one_oversized_track_in_batch() {
        let db = open_mem();
        let a = db.upsert_track(&new_track("/a.flac")).unwrap();
        let b = db.upsert_track(&new_track("/b.flac")).unwrap();
        db.replace_tags(b, &[Tag::new("ok", "fine", 0)]).unwrap();
        db.conn.execute_batch("PRAGMA ignore_check_constraints=ON").unwrap();
        let big = "v".repeat(262_145);
        db.conn
            .execute(
                "INSERT INTO tags (track_id, key, value, ordinal) VALUES (?1, 'k', ?2, 0)",
                rusqlite::params![a, big],
            )
            .unwrap();
        let err = db.tags_for_tracks(&[a, b]).unwrap_err();
        assert!(matches!(err, crate::DbError::FieldTooLarge { field: "value", .. }), "{err:?}");
    }
```

- [ ] **Step 3: Run to verify failure**

Run: `cargo test -p musefs-db tags_for_tracks_tests`
Expected: FAIL (variants/behavior not implemented).

- [ ] **Step 4: Add the shared length helper**

At the top of the `impl<M> Db<M>` block in `tags.rs` (or as a free function in the module), add:

```rust
use crate::error::{check_field_len, DbError};
use crate::limits::{MAX_TAG_KEY_LEN, MAX_TAG_VALUE_LEN, MAX_TAGS_PER_TRACK};

/// Reject an over-cap text-tag row from its `length(key)`/`length(value)`
/// columns *before* the strings are materialized. Routes through the shared
/// `check_field_len`, so the allocation-free guarantee is the same one its
/// unit test pins (spec N13).
fn check_tag_lengths(key_len: i64, value_len: i64) -> Result<()> {
    check_field_len("tags", "key", key_len, MAX_TAG_KEY_LEN)?;
    check_field_len("tags", "value", value_len, MAX_TAG_VALUE_LEN)?;
    Ok(())
}
```

Place these `use` lines and the free `fn check_tag_lengths` at **module scope** (e.g. immediately after the existing `use` block at the top of `tags.rs`, before `impl<M> Db<M>`) — a free `fn` cannot live inside an `impl`. `DbError` is needed for the `TooManyValues` returns below.

- [ ] **Step 5: Rewrite `get_tags`**

```rust
    pub fn get_tags(&self, track_id: i64) -> Result<Vec<Tag>> {
        let mut stmt = self.conn.prepare(
            "SELECT length(key), length(value), key, value, ordinal FROM tags \
             WHERE track_id = ?1 AND value_blob IS NULL ORDER BY key, ordinal",
        )?;
        let mut rows = stmt.query(params![track_id])?;
        let mut out = Vec::new();
        while let Some(r) = rows.next()? {
            check_tag_lengths(r.get(0)?, r.get(1)?)?;
            out.push(Tag { key: r.get(2)?, value: r.get(3)?, ordinal: r.get(4)? });
            if out.len() > MAX_TAGS_PER_TRACK {
                return Err(DbError::TooManyValues { track_id, count: out.len(), max: MAX_TAGS_PER_TRACK });
            }
        }
        Ok(out)
    }
```

- [ ] **Step 6: Rewrite `tags_for_tracks` and `tags_grouped` (bulk, per-track streaming count)**

`tags_for_tracks` — keep the existing `for chunk in track_ids.chunks(CHUNK)` loop **and the `let placeholders = vec!["?"; chunk.len()].join(",");` line that precedes the `format!`**. Replace only from the `let sql = format!(...)` line through the end of the existing `for row in rows { … }` block with the manual loop below (the `format!` SQL gains the two `length(...)` columns):

```rust
            let placeholders = vec!["?"; chunk.len()].join(",");
            let sql = format!(
                "SELECT track_id, length(key), length(value), key, value, ordinal FROM tags \
                 WHERE track_id IN ({placeholders}) AND value_blob IS NULL \
                 ORDER BY track_id, key, ordinal"
            );
            let mut stmt = self.conn.prepare(&sql)?;
            let params = rusqlite::params_from_iter(chunk.iter());
            let mut rows = stmt.query(params)?;
            while let Some(r) = rows.next()? {
                let track_id: i64 = r.get(0)?;
                check_tag_lengths(r.get(1)?, r.get(2)?)?;
                let entry = out.entry(track_id).or_default();
                entry.push(Tag { key: r.get(3)?, value: r.get(4)?, ordinal: r.get(5)? });
                if entry.len() > MAX_TAGS_PER_TRACK {
                    return Err(DbError::TooManyValues { track_id, count: entry.len(), max: MAX_TAGS_PER_TRACK });
                }
            }
```

`tags_grouped` — same shape, with the unfiltered query:

```rust
    pub fn tags_grouped(&self) -> Result<std::collections::HashMap<i64, Vec<Tag>>> {
        let mut stmt = self.conn.prepare(
            "SELECT track_id, length(key), length(value), key, value, ordinal FROM tags \
             WHERE value_blob IS NULL ORDER BY track_id, key, ordinal",
        )?;
        let mut rows = stmt.query([])?;
        let mut out: std::collections::HashMap<i64, Vec<Tag>> = std::collections::HashMap::new();
        while let Some(r) = rows.next()? {
            let track_id: i64 = r.get(0)?;
            check_tag_lengths(r.get(1)?, r.get(2)?)?;
            let entry = out.entry(track_id).or_default();
            entry.push(Tag { key: r.get(3)?, value: r.get(4)?, ordinal: r.get(5)? });
            if entry.len() > MAX_TAGS_PER_TRACK {
                return Err(DbError::TooManyValues { track_id, count: entry.len(), max: MAX_TAGS_PER_TRACK });
            }
        }
        Ok(out)
    }
```

- [ ] **Step 7: Rewrite `tags_grouped_for_keys` (key-filtered, subset count)**

Keep the existing `for chunk in keys.chunks(CHUNK)` loop **and the `let lowered: Vec<String> = …` and `let placeholders = vec!["?"; lowered.len()].join(",");` lines** that precede the `format!`. Replace only from `let sql = format!(...)` through the `for row in rows { … }` block:

```rust
            let lowered: Vec<String> = chunk.iter().map(|k| k.to_ascii_lowercase()).collect();
            let placeholders = vec!["?"; lowered.len()].join(",");
            let sql = format!(
                "SELECT track_id, length(key), length(value), key, value, ordinal FROM tags \
                 WHERE value_blob IS NULL AND lower(key) IN ({placeholders}) \
                 ORDER BY track_id, key, ordinal"
            );
            let mut stmt = self.conn.prepare(&sql)?;
            let params = rusqlite::params_from_iter(lowered.iter());
            let mut rows = stmt.query(params)?;
            while let Some(r) = rows.next()? {
                let track_id: i64 = r.get(0)?;
                check_tag_lengths(r.get(1)?, r.get(2)?)?;
                let entry = out.entry(track_id).or_default();
                entry.push(Tag { key: r.get(3)?, value: r.get(4)?, ordinal: r.get(5)? });
                if entry.len() > MAX_TAGS_PER_TRACK {
                    return Err(DbError::TooManyValues { track_id, count: entry.len(), max: MAX_TAGS_PER_TRACK });
                }
            }
```

- [ ] **Step 8: Rewrite `get_binary_tags` (guard key + count) and fix its doc comment**

Replace the self-referential doc comment ("Ordered by (key, ordinal) to match `get_binary_tags`/synthesis order") with one referencing the chunk reader/layout order, and guard `length(key)` + per-track count:

```rust
    /// Binary tag rows for a track: streaming handle (rowid), key, and payload
    /// length. Ordered by (key, ordinal) to match the layout builder's emission
    /// order. The blob bytes stream at read time; only `key` (materialized here)
    /// is length-guarded, plus the per-track row count.
    pub fn get_binary_tags(&self, track_id: i64) -> Result<Vec<BinaryTagRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT length(key), rowid, key, length(value_blob) FROM tags \
             WHERE track_id = ?1 AND value_blob IS NOT NULL ORDER BY key, ordinal",
        )?;
        let mut rows = stmt.query(params![track_id])?;
        let mut out = Vec::new();
        while let Some(r) = rows.next()? {
            check_field_len("tags", "key", r.get(0)?, MAX_TAG_KEY_LEN)?;
            out.push(BinaryTagRow { rowid: r.get(1)?, key: r.get(2)?, byte_len: r.get(3)? });
            if out.len() > MAX_TAGS_PER_TRACK {
                return Err(DbError::TooManyValues { track_id, count: out.len(), max: MAX_TAGS_PER_TRACK });
            }
        }
        Ok(out)
    }
```

- [ ] **Step 9: Run the tag tests + full db suite + lint**

Run: `cargo test -p musefs-db tags`
Expected: PASS (new guard tests + the existing `tags_for_tracks_tests`, which use only valid-size data and still pass).
Run: `cargo test -p musefs-db && cargo clippy -p musefs-db --all-targets`
Expected: PASS, no warnings.

- [ ] **Step 10: Commit**

```bash
git add musefs-db/src/error.rs musefs-db/src/tags.rs
git commit -m "$(cat <<'EOF'
feat(db): fail closed on oversize tag key/value and per-track row floods (#267)

All five tags readers (incl. the binary get_binary_tags) check length() SQL-side
before materializing and cap rows per track, routed through one shared helper so
a crafted DB cannot smuggle an oversize key/value or a row explosion.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 6: art reader guards

Guard `art.mime` (in `get_art_meta`) and `track_art.description` (in `get_track_art`), checking lengths SQL-side.

**Files:**
- Modify: `musefs-db/src/art.rs` (`get_art_meta`, `get_track_art`)

- [ ] **Step 1: Write the failing tests**

Add a test module to `musefs-db/src/art.rs`:

```rust
#[cfg(test)]
mod guard_tests {
    use crate::error::DbError;
    use crate::models::{NewArt, TrackArt};
    use crate::{Db, Format, NewTrack};

    fn db_track_art() -> (Db, i64, i64) {
        let db = Db::open_in_memory().unwrap();
        let track = db
            .upsert_track(&NewTrack {
                backing_path: "/a.flac".into(),
                format: Format::Flac,
                audio_offset: 0,
                audio_length: 1,
                backing_size: 1,
                backing_mtime: 0,
            })
            .unwrap();
        let art = db
            .upsert_art(&NewArt { mime: "image/png".into(), width: None, height: None, data: vec![0u8] })
            .unwrap();
        (db, track, art)
    }

    #[test]
    fn get_art_meta_rejects_oversize_mime() {
        let (db, _t, art) = db_track_art();
        db.conn.execute_batch("PRAGMA ignore_check_constraints=ON").unwrap();
        let mime = "x".repeat(256);
        db.conn
            .execute("UPDATE art SET mime = ?1 WHERE id = ?2", rusqlite::params![mime, art])
            .unwrap();
        let err = db.get_art_meta(art).unwrap_err();
        assert!(matches!(err, DbError::FieldTooLarge { table: "art", field: "mime", .. }), "{err:?}");
    }

    #[test]
    fn get_track_art_rejects_oversize_description() {
        let (db, track, art) = db_track_art();
        db.conn.execute_batch("PRAGMA ignore_check_constraints=ON").unwrap();
        let desc = "d".repeat(1025);
        db.set_track_art(track, &[TrackArt { art_id: art, picture_type: 3, description: desc, ordinal: 0 }]).unwrap();
        let err = db.get_track_art(track).unwrap_err();
        assert!(matches!(err, DbError::FieldTooLarge { table: "track_art", field: "description", .. }), "{err:?}");
    }

    #[test]
    fn get_track_art_accepts_description_at_cap() {
        let (db, track, art) = db_track_art();
        let desc = "d".repeat(1024);
        db.set_track_art(track, &[TrackArt { art_id: art, picture_type: 3, description: desc, ordinal: 0 }]).unwrap();
        assert_eq!(db.get_track_art(track).unwrap()[0].description.len(), 1024);
    }
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p musefs-db art::guard_tests`
Expected: FAIL (behavior not implemented).

- [ ] **Step 3: Implement the guards**

Add `use crate::error::check_field_len;` and `use crate::limits::{MAX_ART_MIME_LEN, MAX_ART_DESCRIPTION_LEN};` to the top of `art.rs`. Rewrite `get_art_meta` to select `length(mime)` first:

```rust
    pub fn get_art_meta(&self, id: i64) -> Result<Option<ArtMeta>> {
        let mut stmt = self
            .conn
            .prepare("SELECT length(mime), mime, width, height, byte_len FROM art WHERE id = ?1")?;
        let mut rows = stmt.query(params![id])?;
        match rows.next()? {
            Some(r) => {
                check_field_len("art", "mime", r.get(0)?, MAX_ART_MIME_LEN)?;
                Ok(Some(ArtMeta { mime: r.get(1)?, width: r.get(2)?, height: r.get(3)?, byte_len: r.get(4)? }))
            }
            None => Ok(None),
        }
    }
```

Rewrite `get_track_art` to select `length(description)` and guard each row:

```rust
    pub fn get_track_art(&self, track_id: i64) -> Result<Vec<TrackArt>> {
        let mut stmt = self.conn.prepare(
            "SELECT length(description), art_id, picture_type, description, ordinal
             FROM track_art WHERE track_id = ?1 ORDER BY ordinal",
        )?;
        let mut rows = stmt.query(params![track_id])?;
        let mut out = Vec::new();
        while let Some(r) = rows.next()? {
            check_field_len("track_art", "description", r.get(0)?, MAX_ART_DESCRIPTION_LEN)?;
            out.push(TrackArt { art_id: r.get(1)?, picture_type: r.get(2)?, description: r.get(3)?, ordinal: r.get(4)? });
        }
        Ok(out)
    }
```

- [ ] **Step 4: Run to verify pass + full db suite + lint**

Run: `cargo test -p musefs-db art`
Expected: PASS.
Run: `cargo test -p musefs-db && cargo clippy -p musefs-db --all-targets`
Expected: PASS, no warnings.

- [ ] **Step 5: Commit**

```bash
git add musefs-db/src/art.rs
git commit -m "$(cat <<'EOF'
feat(db): fail closed on oversize art.mime / track_art.description (#278)

get_art_meta and get_track_art check length() SQL-side before materializing,
mirroring the V4 CHECKs as a fail-closed read guard.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 7: cross-layer cap-equality tests

Pin the cross-layer invariants the db layer cannot assert itself: `MAX_STRUCTURAL_BODY_LEN == format::MAX_BLOCK_BODY`, and the db blob caps == the scanner's ingestion caps.

**Files:**
- Modify: `musefs-format/src/flac.rs:148` (widen `MAX_BLOCK_BODY` to `pub`)
- Modify: `musefs-core/src/lib.rs` (cross-layer test for the structural body cap)
- Modify: `musefs-core/src/scan.rs` (test asserting scan caps == db limits)

- [ ] **Step 1: Widen `MAX_BLOCK_BODY` to `pub`**

In `musefs-format/src/flac.rs:148`, change:

```rust
pub const MAX_BLOCK_BODY: u64 = 0x00FF_FFFF;
```

- [ ] **Step 2: Write the cross-layer structural-cap test**

Add a test module to `musefs-core/src/lib.rs` (it depends on both `musefs-db` and `musefs-format`):

```rust
#[cfg(test)]
mod cross_layer_caps {
    #[test]
    fn structural_body_cap_matches_flac_block_limit() {
        assert_eq!(
            u64::try_from(musefs_db::limits::MAX_STRUCTURAL_BODY_LEN).unwrap(),
            musefs_format::flac::MAX_BLOCK_BODY,
            "db structural body cap must equal FLAC's 24-bit block limit",
        );
    }
}
```

- [ ] **Step 3: Write the scan-cap equality test**

The scanner caps `MAX_ART_BYTES`/`MAX_BINARY_TAG_BYTES` are private `const`s in `scan.rs`, so assert from inside its own test module. Add to the existing test module in `musefs-core/src/scan.rs` (the one that already asserts `MAX_ART_BYTES == 16_711_680`):

```rust
    #[test]
    fn scan_caps_match_db_limits() {
        assert_eq!(i64::try_from(MAX_ART_BYTES).unwrap(), musefs_db::limits::MAX_ART_BYTES);
        assert_eq!(i64::try_from(MAX_BINARY_TAG_BYTES).unwrap(), musefs_db::limits::MAX_BINARY_TAG_BYTES);
    }
```

- [ ] **Step 4: Run the cross-layer tests + the format/core suites + lint**

Run: `cargo test -p musefs-core cross_layer_caps && cargo test -p musefs-core scan_caps_match_db_limits`
Expected: PASS.
Run: `cargo test -p musefs-format && cargo clippy -p musefs-format -p musefs-core --all-targets`
Expected: PASS, no warnings (the `pub` widening has no other callers to break).

- [ ] **Step 5: Commit**

```bash
git add musefs-format/src/flac.rs musefs-core/src/lib.rs musefs-core/src/scan.rs
git commit -m "$(cat <<'EOF'
test: pin cross-layer cap equality (structural body == FLAC block, scan == db)

Widen MAX_BLOCK_BODY to pub and assert the db caps that mirror the format
ceiling and the scanner ingestion caps cannot drift.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 8: docs (ARCHITECTURE.md) + fuzz check + final verification

**Files:**
- Modify: `ARCHITECTURE.md` (external-writer contract)

- [ ] **Step 1: Update the external-writer contract**

Anchor: the section headed `### The external-writer contract` in `ARCHITECTURE.md` (find it with `grep -n "external-writer contract" ARCHITECTURE.md`). Its existing sentence enumerates the V4-rejected shapes — it begins "As of V4, SQLite `CHECK` constraints reject the malformed *shapes* at commit — an unknown `format` string, …". Extend that enumeration to also list: a `tags.key` over 256 chars or `tags.value` over 256 KiB, a `value_blob` over `MAX_BINARY_TAG_BYTES`, an `art.mime` over 255 chars or `byte_len` over `MAX_ART_BYTES`, a `track_art.description` over 1 KiB, and a `structural_blocks` row with an unknown `kind`, negative `ordinal`, or `body` over the FLAC 24-bit block limit. Then add one sentence to the same section stating that the read-only mount now also validates schema identity at open (`schema::validate_identity`: a `sqlite_master` comparison against a freshly-migrated reference plus `PRAGMA foreign_key_check`) and rejects anything that is not the canonical latest schema with a message telling the user to run `musefs scan`. Update the V2 bullet (or the V4 bullet) that currently calls `structural_blocks` "not part of the editable contract" only if it also implies it is unconstrained — it is now a constrained, rebuilt-in-V4 table.

Verify the edit landed: `grep -n "256 KiB\|validate_identity\|schema identity" ARCHITECTURE.md` should return your new lines (this is the green-check for an otherwise un-gated docs step).

- [ ] **Step 2: Verify the fuzz crate still builds (format API unchanged except a pub widening)**

Run: `cargo +nightly fuzz build`
Expected: builds (the only format-layer change is widening `MAX_BLOCK_BODY` to `pub`; no signatures changed). If nightly/`cargo-fuzz` is unavailable in the environment, note that and rely on CI's fuzz smoke job.

- [ ] **Step 3: Full workspace verification**

Run: `cargo fmt --all --check`
Expected: clean.
Run: `cargo clippy --all-targets`
Expected: no warnings.
Run: `cargo test`
Expected: PASS across the workspace.

- [ ] **Step 4: Commit**

```bash
git add ARCHITECTURE.md
git commit -m "$(cat <<'EOF'
docs: document field-size caps and the schema-identity gate in ARCHITECTURE

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Post-Implementation Checklist

- [ ] All eight tasks committed; `cargo test` green workspace-wide.
- [ ] `MUSEFS_REGEN_SCHEMA_PY=1 cargo test -p musefs-db schema_py` produces no diff (mirror is current).
- [ ] Picard vendored copy in sync (`python contrib/python-musefs/vendor_to_picard.py` produces no diff).
- [ ] Spec requirements traced: #270 gate (Task 2, incl. honest-with-rows + RO foreign-key-violation tests), tags.key/value + per-track count incl. binary (Task 5), value_blob/art blob CHECKs (Task 3), structural_blocks body/kind/ordinal CHECK + reader guard incl. negative-ordinal test (Tasks 3, 4), art.mime/description (Tasks 3, 6), drift + cross-layer equality (Tasks 3, 7), N13 observable allocation-free guard via the pure `check_field_len` helper + its unit test (Task 4, reused by Tasks 5–6), error→errno via existing `CoreError::Db` arm (no change needed — verified), ARCHITECTURE.md (Task 8).
