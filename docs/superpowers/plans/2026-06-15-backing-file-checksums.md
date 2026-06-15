# Backing-file Checksums & Move Re-identification Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Give every track a path-independent content identity so a moved/reorganized backing library is retargeted in place on a normal `scan` instead of orphaned.

**Architecture:** Two nullable scanner-owned columns on `tracks` — a cheap `fingerprint` (hash of the probe's parsed `Probed` output) and an authoritative full-file `content_hash` (SHA-256). The fingerprint/full-hash are computed in the parallel probe **worker**; the refind/retarget decision runs on the single **writer**. A normal `scan` (not `revalidate`) looks up an incoming new-path file's fingerprint among rows whose backing file is gone and retargets the unique match. Per-scan checksum tier (`none`/`fingerprint`/`full`) and match strictness (`--fast`/auto/`--strict`) are threaded through `ScanOptions`.

**Tech Stack:** Rust (workspace: `musefs-db` → `musefs-core` → `musefs-cli`/binary), SQLite via `rusqlite`, `sha2`+`base16ct` for hashing, `clap` derive for CLI, `criterion` for benches, Python mirror for contrib plugins.

**Spec:** `docs/superpowers/specs/2026-06-15-backing-file-checksums-design.md`

---

## Critical Constraints (read before every task)

1. **Green-commit rule.** The pre-commit hook runs `cargo fmt --all --check`, `cargo clippy --all-targets -- -D warnings`, and `cargo test --workspace`. Every commit must pass all three. A commit with a red test, a clippy warning, or unformatted code is rejected. Run `cargo fmt` before committing.

2. **Mutant-anchor drift guard (CORE/FORMAT EDITS ONLY).** When a commit stages any `musefs-core/src/*.rs` or `musefs-format/src/*.rs` file, the hook runs `cargo mutants --no-config --list --json` and validates `.cargo/mutants.toml`'s `file:line:col` anchors. `scan.rs` has ~18 line:col anchors; **any line you add or remove above line 1436 shifts them and fails the commit.** For every task that edits `scan.rs`, the commit step includes this re-anchor procedure (run it AFTER the code change, BEFORE `git commit`):

   ```bash
   # Regenerate the mutant list at the new coordinates and re-anchor in place.
   cargo mutants --no-config --list --json > /tmp/mutants-list.json
   python3 scripts/check_mutant_anchors.py --fix --mutants-json /tmp/mutants-list.json
   # Verify it now passes (exit 0, prints "OK: N exclude_re entries validated"):
   python3 scripts/check_mutant_anchors.py --mutants-json /tmp/mutants-list.json
   git add .cargo/mutants.toml
   ```

   If `--fix` reports remaining failures (it can only auto-resolve unambiguous single-site shifts), open `.cargo/mutants.toml`, find each failing entry by its `# guard:` tag (`op=`/`fn=`/`rows=`), locate the new `line:col` of that operator in `scan.rs`, and edit the anchor by hand. `musefs-db` and `musefs-cli` edits do **not** trigger this guard.

3. **`rtk` rewrites `cargo test` output** to a one-line summary; do not grep for "test result". Trust the command's exit code (0 = pass).

4. **Contrib Python test envs.** `python-musefs` and `picard` run with the system Python; `beets`/`lidarr` need their venv (PEP 668). Commands are given per task.

5. **Serena tools.** Per the project's tool policy, use Serena's symbolic tools (`get_symbols_overview`, `find_symbol`, `replace_symbol_body`, `insert_after_symbol`, `replace_content`) for reading/editing Rust code, not the built-in Read/Edit, except for small line edits or non-code files.

---

## File Structure

**Modified:**
- `musefs-db/src/schema.rs` — add `MIGRATION_V2`, register it, schema-render tests auto-cover it.
- `musefs-db/src/models.rs` — add `fingerprint`/`content_hash` to the `Track` read model.
- `musefs-db/src/tracks.rs` — extend `track_select!`/`row_to_track`; add `tracks_by_fingerprint`, `retarget_track`, `set_track_checksums`.
- `musefs-db/src/bulk.rs` — `BulkWriter` equivalents of the three new methods.
- `musefs-core/Cargo.toml` — add `sha2`, `base16ct` deps.
- `musefs-core/src/scan.rs` — `ChecksumTier`/`MatchStrictness` enums; `ScanOptions` fields; `fingerprint_of`/`full_file_hash`; `Unit` fields; worker compute; `TrackSink` additions; refind decision; revalidate backfill gate.
- `musefs-core/src/lib.rs` — re-export the two new enums.
- `musefs-cli/src/lib.rs` — `ChecksumMode` enum, `--checksum`/`--fast`/`--strict` flags, map to `ScanOptions`.
- `contrib/python-musefs/src/musefs_common/schema.py` — regenerated (do not hand-edit).
- `contrib/picard/musefs/_common/schema.py` + `constants.py` — re-vendored.
- `contrib/picard/tests/test_conftest_sanity.py`, `contrib/python-musefs/tests/test_constants.py` — bump version assertions.
- `ARCHITECTURE.md`, `README.md` — document the columns and flags.

**Created:**
- `musefs-core/benches/fingerprint_overhead.rs` — criterion bench for the default-tier decision.

---

## Phase A — Database layer (no scan.rs edits, no re-anchoring)

### Task A1: V2 migration adds the two columns + index

**Files:**
- Modify: `musefs-db/src/schema.rs` (after `MIGRATION_V1`, ends ~line 200; and `MIGRATIONS` array ~line 207)
- Regenerate: `contrib/python-musefs/src/musefs_common/schema.py`
- Re-vendor: `contrib/picard/musefs/_common/schema.py`, `contrib/picard/musefs/_common/constants.py`
- Modify: `contrib/picard/tests/test_conftest_sanity.py:8`, `contrib/python-musefs/tests/test_constants.py:5`

- [ ] **Step 1: Add a failing test for user_version == 2**

In `musefs-db/src/schema.rs`, inside the existing `mod baseline_tests` (near the `baseline_creates_...` test), add via `insert_after_symbol` on the `baseline_creates_value_blob_and_structural_blocks_and_is_idempotent` test:

```rust
    #[test]
    fn migration_v2_adds_fingerprint_and_content_hash_columns() {
        let mut conn = Connection::open_in_memory().unwrap();
        migrate(&mut conn).unwrap();
        assert_eq!(
            conn.pragma_query_value::<i64, _>(None, "user_version", |r| r.get(0))
                .unwrap(),
            2,
            "V2 migration must bump user_version to 2"
        );
        // Both columns exist, are nullable, and default to NULL.
        conn.execute(
            "INSERT INTO tracks
                (backing_path, format, audio_offset, audio_length, backing_size,
                 backing_mtime_ns, backing_ctime_ns, updated_at)
             VALUES ('/x.flac','flac',0,10,10,0,0,0)",
            [],
        )
        .unwrap();
        let (fp, ch): (Option<String>, Option<String>) = conn
            .query_row(
                "SELECT fingerprint, content_hash FROM tracks WHERE backing_path='/x.flac'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(fp, None);
        assert_eq!(ch, None);
    }
```

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo test -p musefs-db migration_v2_adds_fingerprint_and_content_hash_columns`
Expected: FAIL — `user_version` is 1, and the columns don't exist (`no such column: fingerprint`).

- [ ] **Step 3: Add the migration**

In `musefs-db/src/schema.rs`, insert a new const immediately after the `MIGRATION_V1` string const (before `const MIGRATIONS`):

```rust
const MIGRATION_V2: &str = r"
ALTER TABLE tracks ADD COLUMN fingerprint  TEXT;
ALTER TABLE tracks ADD COLUMN content_hash TEXT
    CHECK (content_hash IS NULL OR length(content_hash) = 64);
CREATE INDEX tracks_fingerprint_idx ON tracks(fingerprint);
";
```

Then change the migrations array:

```rust
const MIGRATIONS: &[&str] = &[MIGRATION_V1, MIGRATION_V2];
```

`migrate()` loops `(1i64..).zip(MIGRATIONS)`, so it auto-applies V2 as target 2 and stamps `user_version = 2`. No change to `migrate()` itself.

- [ ] **Step 4: Run the new test + the schema identity tests**

Run: `cargo test -p musefs-db migration_v2_adds_fingerprint_and_content_hash_columns`
Expected: PASS.

Run: `cargo test -p musefs-db schema`
Expected: `schema_py_fixture_is_fresh` FAILS (the on-disk `schema.py` is now stale — expected) and `identity_tests`/`schema_sql_matches_migrate` PASS. The stale-fixture failure is fixed in Step 5.

- [ ] **Step 5: Regenerate the Python mirror and re-vendor**

Run:

```bash
MUSEFS_REGEN_SCHEMA_PY=1 cargo test -p musefs-db schema_py
python3 contrib/python-musefs/vendor_to_picard.py
```

Verify `contrib/python-musefs/src/musefs_common/schema.py` now ends with `PRAGMA user_version = 2;` and has `USER_VERSION = 2`, and the same in `contrib/picard/musefs/_common/schema.py`.

- [ ] **Step 6: Bump the two hardcoded Python version assertions**

In `contrib/picard/tests/test_conftest_sanity.py:8`, change:

```python
        assert conn.execute("PRAGMA user_version").fetchone()[0] == 1
```
to:
```python
        assert conn.execute("PRAGMA user_version").fetchone()[0] == 2
```

In `contrib/python-musefs/tests/test_constants.py:5`, change:

```python
    assert constants.EXPECTED_USER_VERSION == 1
```
to:
```python
    assert constants.EXPECTED_USER_VERSION == 2
```

- [ ] **Step 7: Run the Rust + Python suites**

Run:

```bash
cargo test -p musefs-db
cd contrib/python-musefs && python3 -m pytest -q && cd ../..
cd contrib/picard && python3 -m pytest -q && cd ../..
```

Expected: all PASS. (Picard's real-Picard tests may skip without Qt; the `test_conftest_sanity` test runs regardless.)

- [ ] **Step 8: Commit**

```bash
git add musefs-db/src/schema.rs \
        contrib/python-musefs/src/musefs_common/schema.py \
        contrib/picard/musefs/_common/schema.py \
        contrib/picard/musefs/_common/constants.py \
        contrib/python-musefs/src/musefs_common/constants.py \
        contrib/picard/tests/test_conftest_sanity.py \
        contrib/python-musefs/tests/test_constants.py
git commit -m "feat(musefs-db): V2 migration adds tracks.fingerprint + content_hash (#464)"
```

(Only stage the `constants.py` files if `git status` shows them changed by the vendor/regen step.)

---

### Task A2: Track read model + DB query/update methods

**Files:**
- Modify: `musefs-db/src/models.rs:127-137` (`Track`)
- Modify: `musefs-db/src/tracks.rs:9-18` (`track_select!`), `:32-56` (`row_to_track`), and add three methods
- Modify: `musefs-db/src/bulk.rs` (`BulkWriter` method equivalents)
- Test: in `musefs-db/src/tracks.rs` test module (or `musefs-db/tests/`)

- [ ] **Step 1: Write failing tests for read-back, fingerprint lookup, retarget, and no-clobber**

Add to the `#[cfg(test)] mod tests` in `musefs-db/src/tracks.rs` (use `find_symbol` to locate it; if none exists in this file, add `#[cfg(test)] mod fingerprint_tests` at end of file). Use the existing test helper for opening a writable DB — check how other `tracks.rs` tests build a `Db` (look for `Db::open_in_memory()`):

```rust
#[cfg(test)]
mod checksum_tests {
    use crate::{Db, NewTrack, models::Format};

    fn new_track(path: &str) -> NewTrack {
        NewTrack {
            backing_path: path.to_string(),
            format: Format::Flac,
            audio_offset: 0,
            audio_length: 10,
            backing_size: 10,
            backing_mtime_ns: 0,
            backing_ctime_ns: 0,
        }
    }

    #[test]
    fn set_and_read_back_checksums() {
        let db = Db::open_in_memory().unwrap();
        let id = db.upsert_track(&new_track("/a.flac")).unwrap();
        db.set_track_checksums(id, Some("fp1"), Some(&"d".repeat(64)))
            .unwrap();
        let t = db.get_track(id).unwrap().unwrap();
        assert_eq!(t.fingerprint.as_deref(), Some("fp1"));
        assert_eq!(t.content_hash.as_deref(), Some(&"d".repeat(64)[..]));
    }

    #[test]
    fn set_checksums_none_does_not_clobber_existing() {
        let db = Db::open_in_memory().unwrap();
        let id = db.upsert_track(&new_track("/a.flac")).unwrap();
        db.set_track_checksums(id, Some("fp1"), Some(&"d".repeat(64)))
            .unwrap();
        // A later lower-tier pass passes None and must preserve both.
        db.set_track_checksums(id, None, None).unwrap();
        let t = db.get_track(id).unwrap().unwrap();
        assert_eq!(t.fingerprint.as_deref(), Some("fp1"));
        assert_eq!(t.content_hash.as_deref(), Some(&"d".repeat(64)[..]));
    }

    #[test]
    fn tracks_by_fingerprint_returns_matches() {
        let db = Db::open_in_memory().unwrap();
        let a = db.upsert_track(&new_track("/a.flac")).unwrap();
        let b = db.upsert_track(&new_track("/b.flac")).unwrap();
        db.set_track_checksums(a, Some("shared"), None).unwrap();
        db.set_track_checksums(b, Some("shared"), None).unwrap();
        db.upsert_track(&new_track("/c.flac")).unwrap(); // fingerprint NULL
        let mut ids: Vec<i64> = db
            .tracks_by_fingerprint("shared")
            .unwrap()
            .into_iter()
            .map(|t| t.id)
            .collect();
        ids.sort_unstable();
        assert_eq!(ids, vec![a, b]);
        assert!(db.tracks_by_fingerprint("nope").unwrap().is_empty());
    }

    #[test]
    fn retarget_updates_path_stamp_and_bounds_keeping_id() {
        let db = Db::open_in_memory().unwrap();
        let id = db.upsert_track(&new_track("/old.flac")).unwrap();
        db.set_track_checksums(id, Some("fp"), None).unwrap();
        db.retarget_track(id, "/new.flac", 99, 1234, 5678, 42, 50, None, Some(&"e".repeat(64)))
            .unwrap();
        let t = db.get_track(id).unwrap().unwrap();
        assert_eq!(t.id, id);
        assert_eq!(t.backing_path, "/new.flac");
        assert_eq!(t.backing_size, 99);
        assert_eq!(t.backing_mtime_ns, 1234);
        assert_eq!(t.backing_ctime_ns, 5678);
        assert_eq!(t.bounds.audio_offset(), 42);
        assert_eq!(t.bounds.audio_length(), 50);
        assert_eq!(t.fingerprint.as_deref(), Some("fp")); // None arg preserves
        assert_eq!(t.content_hash.as_deref(), Some(&"e".repeat(64)[..]));
        assert!(db.get_track_by_path("/old.flac").unwrap().is_none());
    }
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p musefs-db checksum_tests`
Expected: FAIL to compile — `Track` has no `fingerprint`/`content_hash`, and the three methods don't exist.

- [ ] **Step 3: Add fields to the `Track` read model**

In `musefs-db/src/models.rs`, edit the `Track` struct (use `replace_symbol_body` on `Track`) to append two fields after `updated_at`:

```rust
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Track {
    pub id: i64,
    pub backing_path: String,
    pub format: Format,
    pub bounds: TrackBounds,
    pub backing_size: u64,
    pub backing_mtime_ns: i64,
    pub backing_ctime_ns: i64,
    pub content_version: i64,
    pub updated_at: i64,
    pub fingerprint: Option<String>,
    pub content_hash: Option<String>,
}
```

- [ ] **Step 4: Extend `track_select!` and `row_to_track`**

In `musefs-db/src/tracks.rs`, edit the `track_select!` macro's column list to add the two columns at the end of the `SELECT` list (before `FROM tracks`):

```rust
macro_rules! track_select {
    ($tail:literal) => {
        concat!(
            "SELECT id, backing_path, format, audio_offset, audio_length, \
             backing_size, backing_mtime_ns, backing_ctime_ns, content_version, updated_at, \
             fingerprint, content_hash \
             FROM tracks ",
            $tail
        )
    };
}
```

Edit `row_to_track` (use `find_symbol` + `replace_symbol_body`) to read the two new columns. Append after the `updated_at` field read; the new columns are indices 10 and 11 (0-based) in the SELECT order above:

```rust
        // ... existing field reads through updated_at (index 9) ...
        fingerprint: row.get(10)?,
        content_hash: row.get(11)?,
```

(Match the existing `row_to_track` construction style exactly — it builds a `Track { ... }`; add the two fields to that literal in the same positions.)

- [ ] **Step 5: Add the three methods to `Db<ReadWrite>`**

In `musefs-db/src/tracks.rs`, inside the `impl` block that holds `get_track_by_path`/`delete_track` (use `insert_after_symbol` on `delete_track`):

```rust
    /// All tracks whose stored fingerprint equals `fp` (rows with NULL
    /// fingerprint never match). Used by the scan refind to find move candidates.
    pub fn tracks_by_fingerprint(&self, fp: &str) -> Result<Vec<Track>> {
        let mut stmt = self
            .conn
            .prepare_cached(track_select!("WHERE fingerprint = ?1 ORDER BY id"))?;
        let rows = stmt.query_map(params![fp], row_to_track)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Set the scanner-owned checksums for a track. A `None` argument leaves the
    /// existing column value intact (COALESCE), so a lower-tier pass never clears
    /// a higher tier's value.
    pub fn set_track_checksums(
        &self,
        id: i64,
        fingerprint: Option<&str>,
        content_hash: Option<&str>,
    ) -> Result<()> {
        self.conn.execute(
            "UPDATE tracks SET
                fingerprint  = COALESCE(?2, fingerprint),
                content_hash = COALESCE(?3, content_hash)
             WHERE id = ?1",
            params![id, fingerprint, content_hash],
        )?;
        Ok(())
    }

    /// Point an existing track at a relocated backing file: update its path,
    /// validation stamp, and audio bounds in place, preserving its `id` (and
    /// thus its tags/art/structural blocks). Checksum args COALESCE like
    /// `set_track_checksums`. `updated_at` is refreshed; `content_version` is
    /// left to the geometry trigger (it bumps only if `backing_mtime_ns`
    /// actually changed — a pure move preserves mtime, so no bump).
    #[allow(clippy::too_many_arguments)]
    pub fn retarget_track(
        &self,
        id: i64,
        new_backing_path: &str,
        backing_size: u64,
        backing_mtime_ns: i64,
        backing_ctime_ns: i64,
        audio_offset: u64,
        audio_length: u64,
        fingerprint: Option<&str>,
        content_hash: Option<&str>,
    ) -> Result<()> {
        self.conn.execute(
            "UPDATE tracks SET
                backing_path     = ?2,
                backing_size     = ?3,
                backing_mtime_ns = ?4,
                backing_ctime_ns = ?5,
                audio_offset     = ?6,
                audio_length     = ?7,
                fingerprint      = COALESCE(?8, fingerprint),
                content_hash     = COALESCE(?9, content_hash),
                updated_at       = CAST(strftime('%s','now') AS INTEGER)
             WHERE id = ?1",
            params![
                id,
                new_backing_path,
                backing_size,
                backing_mtime_ns,
                backing_ctime_ns,
                audio_offset,
                audio_length,
                fingerprint,
                content_hash,
            ],
        )?;
        Ok(())
    }
```

- [ ] **Step 6: Run the DB tests**

Run: `cargo test -p musefs-db checksum_tests`
Expected: PASS.

Run: `cargo test -p musefs-db`
Expected: PASS (the `Track` literal change may surface in other tests that construct `Track` directly — if any fail to compile, add `fingerprint: None, content_hash: None` to those literals).

- [ ] **Step 7: Add `BulkWriter` equivalents**

In `musefs-db/src/bulk.rs`, add to the `impl BulkWriter` block (after `upsert_track`) the same three methods delegating to `&self.tx` (mirror the `Db` bodies but use `self.tx.prepare_cached`/`self.tx.execute`). The refind path runs inside a bulk transaction, so these are required:

```rust
    pub fn tracks_by_fingerprint(&self, fp: &str) -> Result<Vec<Track>> {
        crate::tracks::tracks_by_fingerprint_in(&self.tx, fp)
    }

    pub fn set_track_checksums(
        &self,
        id: i64,
        fingerprint: Option<&str>,
        content_hash: Option<&str>,
    ) -> Result<()> {
        crate::tracks::set_track_checksums_in(&self.tx, id, fingerprint, content_hash)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn retarget_track(
        &self,
        id: i64,
        new_backing_path: &str,
        backing_size: u64,
        backing_mtime_ns: i64,
        backing_ctime_ns: i64,
        audio_offset: u64,
        audio_length: u64,
        fingerprint: Option<&str>,
        content_hash: Option<&str>,
    ) -> Result<()> {
        crate::tracks::retarget_track_in(
            &self.tx, id, new_backing_path, backing_size, backing_mtime_ns,
            backing_ctime_ns, audio_offset, audio_length, fingerprint, content_hash,
        )
    }

    pub fn get_track_by_path(&self, path: &str) -> Result<Option<Track>> {
        crate::tracks::get_track_by_path_in(&self.tx, path)
    }
```

Then in `musefs-db/src/tracks.rs`, refactor the three `Db` methods (and `get_track_by_path`) to delegate to free `*_in(conn, ...)` helpers that take `&rusqlite::Connection` (mirroring the existing `upsert_track_in`/`upsert_art_in` pattern), so `Db` and `BulkWriter` share one body. Example for `set_track_checksums`:

```rust
pub(crate) fn set_track_checksums_in(
    conn: &rusqlite::Connection,
    id: i64,
    fingerprint: Option<&str>,
    content_hash: Option<&str>,
) -> Result<()> {
    conn.execute(
        "UPDATE tracks SET
            fingerprint  = COALESCE(?2, fingerprint),
            content_hash = COALESCE(?3, content_hash)
         WHERE id = ?1",
        params![id, fingerprint, content_hash],
    )?;
    Ok(())
}
```

And the `Db` method becomes `pub fn set_track_checksums(&self, ...) -> Result<()> { set_track_checksums_in(&self.conn, id, fingerprint, content_hash) }`. Do the same `*_in` split for `tracks_by_fingerprint`, `retarget_track`, and `get_track_by_path` (the latter currently inlines `query_optional_track`; extract a `get_track_by_path_in(conn, path)` that runs the `track_select!("WHERE backing_path = ?1")` query).

- [ ] **Step 8: Add a BulkWriter parity test**

Add to `checksum_tests` in `tracks.rs`:

```rust
    #[test]
    fn bulk_writer_retarget_and_checksums_match_db() {
        let mut db = Db::open_in_memory().unwrap();
        let id = {
            let mut bw = db.bulk_writer().unwrap();
            let id = bw.upsert_track(&new_track("/old.flac")).unwrap();
            bw.set_track_checksums(id, Some("fp"), None).unwrap();
            bw.retarget_track(id, "/new.flac", 9, 1, 2, 0, 10, None, None)
                .unwrap();
            bw.commit().unwrap();
            id
        };
        let t = db.get_track(id).unwrap().unwrap();
        assert_eq!(t.backing_path, "/new.flac");
        assert_eq!(t.fingerprint.as_deref(), Some("fp"));
    }
```

(Check the exact `BulkWriter` construction/commit API — `find_symbol` for how existing tests build a `BulkWriter` from `Db`; adjust `db.bulk_writer()`/`bw.commit()` to the real names.)

- [ ] **Step 9: Run, format, commit**

Run: `cargo test -p musefs-db && cargo fmt --all && cargo clippy -p musefs-db --all-targets -- -D warnings`
Expected: PASS / no warnings.

```bash
git add musefs-db/src/models.rs musefs-db/src/tracks.rs musefs-db/src/bulk.rs
git commit -m "feat(musefs-db): fingerprint lookup, retarget, and checksum setters (#464)"
```

---

## Phase B — Core checksum computation (scan.rs; re-anchoring required)

### Task B1: Hash helpers + checksum tier/strictness options

**Files:**
- Modify: `musefs-core/Cargo.toml` (deps)
- Modify: `musefs-core/src/scan.rs` (`ScanOptions`, two enums, two helper fns)
- Modify: `musefs-core/src/lib.rs` (re-export enums)

- [ ] **Step 1: Add deps**

In `musefs-core/Cargo.toml` `[dependencies]`, add (match the versions `musefs-db` uses):

```toml
sha2 = "0.11"
base16ct = "1.0"
```

- [ ] **Step 2: Write failing unit tests for the helpers + enum defaults**

In `musefs-core/src/scan.rs`, inside `mod scan_unit_tests` (use `find_symbol` to locate it), add:

```rust
    #[test]
    fn fingerprint_is_deterministic_and_sensitive_to_content() {
        let p1 = Probed {
            format: Format::Flac,
            audio_offset: 8,
            audio_length: 100,
            tags: vec![("title".into(), "A".into())],
            pictures: Vec::new(),
            binary_tags: Vec::new(),
            structural_blocks: vec![("STREAMINFO".into(), vec![1, 2, 3])],
        };
        let p2 = Probed {
            tags: vec![("title".into(), "A".into())],
            structural_blocks: vec![("STREAMINFO".into(), vec![1, 2, 3])],
            ..clone_probed(&p1)
        };
        assert_eq!(fingerprint_of(&p1), fingerprint_of(&p2), "same content => same fp");

        let mut p3 = clone_probed(&p1);
        p3.audio_length = 101;
        assert_ne!(fingerprint_of(&p1), fingerprint_of(&p3), "length change => fp change");

        let mut p4 = clone_probed(&p1);
        p4.tags = vec![("title".into(), "B".into())];
        assert_ne!(fingerprint_of(&p1), fingerprint_of(&p4), "tag change => fp change");
    }

    #[test]
    fn full_file_hash_matches_known_sha256() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.bin");
        std::fs::write(&path, b"abc").unwrap();
        // sha256("abc")
        assert_eq!(
            full_file_hash(&path).unwrap(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn checksum_tier_defaults_to_fingerprint() {
        assert_eq!(ScanOptions::default().checksum, ChecksumTier::Fingerprint);
        assert_eq!(ScanOptions::default().strictness, MatchStrictness::Auto);
    }
```

Add a small test-only `clone_probed` helper next to these tests (Probed isn't `Clone`):

```rust
    fn clone_probed(p: &Probed) -> Probed {
        Probed {
            format: p.format,
            audio_offset: p.audio_offset,
            audio_length: p.audio_length,
            tags: p.tags.clone(),
            pictures: Vec::new(),
            binary_tags: Vec::new(),
            structural_blocks: p.structural_blocks.clone(),
        }
    }
```

- [ ] **Step 3: Run to verify failure**

Run: `cargo test -p musefs-core fingerprint_is_deterministic`
Expected: FAIL to compile — `fingerprint_of`, `full_file_hash`, `ChecksumTier`, `MatchStrictness`, and the `ScanOptions.checksum`/`.strictness` fields don't exist.

- [ ] **Step 4: Add the two enums**

In `musefs-core/src/scan.rs`, immediately before the `ScanOptions` struct (use `insert_before_symbol` on `ScanOptions`):

```rust
/// How much checksum work a scan does per file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChecksumTier {
    /// No checksums (legacy behavior).
    None,
    /// Compute the cheap fingerprint only (rides the probe).
    Fingerprint,
    /// Fingerprint plus an eager full-file SHA-256.
    Full,
}

/// How a fingerprint match is confirmed before a retarget.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MatchStrictness {
    /// Confirm with the full hash when the candidate has one; else trust the
    /// fingerprint.
    Auto,
    /// Fingerprint match is always sufficient; never read the full file.
    Fast,
    /// Require a full-hash match; refuse the retarget if the candidate has no
    /// stored content_hash.
    Strict,
}
```

- [ ] **Step 5: Add the fields to `ScanOptions` + `Default`**

Edit `ScanOptions` (use `replace_symbol_body`) to add two fields after `progress`, and update its `Default` impl:

```rust
pub struct ScanOptions {
    pub jobs: usize,
    pub window: usize,
    pub batch_bytes: u64,
    pub follow_symlinks: bool,
    pub progress: Option<ProgressSink>,
    /// Which checksums to compute and store this scan.
    pub checksum: ChecksumTier,
    /// How a refind fingerprint match is confirmed before retargeting.
    pub strictness: MatchStrictness,
}
```

In the `Default` impl add:

```rust
            checksum: ChecksumTier::Fingerprint,
            strictness: MatchStrictness::Auto,
```

(Keep the `progress: None` line and the rest unchanged. `replace_symbol_body` may drop the doc comments on the struct/fields — re-include them verbatim.)

- [ ] **Step 6: Add the two helper functions**

Add at the end of `scan.rs` (use `insert_after_symbol` on the last top-level item, e.g. `revalidate`) so no anchored lines shift unnecessarily:

```rust
/// SHA-256 of the probe's parsed output, hex-encoded. This is the cheap content
/// fingerprint: deterministic per file (the parsed `Probed` is window- and
/// format-independent), and excludes every filesystem-stamp field. Length-prefix
/// every variable-length field so concatenation can't alias.
pub(crate) fn fingerprint_of(p: &Probed) -> String {
    use sha2::{Digest, Sha256};
    // Inner fn (not a closure) so it doesn't hold a borrow of `h` across the
    // direct `h.update(...)` calls below.
    fn feed(h: &mut Sha256, bytes: &[u8]) {
        h.update((bytes.len() as u64).to_le_bytes());
        h.update(bytes);
    }
    let mut h = Sha256::new();
    feed(&mut h, p.format.as_str().as_bytes());
    h.update(p.audio_offset.to_le_bytes());
    h.update(p.audio_length.to_le_bytes());
    h.update((p.tags.len() as u64).to_le_bytes());
    for (k, v) in &p.tags {
        feed(&mut h, k.as_bytes());
        feed(&mut h, v.as_bytes());
    }
    h.update((p.pictures.len() as u64).to_le_bytes());
    for pic in &p.pictures {
        feed(&mut h, pic.mime.as_bytes());
        h.update(u64::from(pic.picture_type.get()).to_le_bytes());
        feed(&mut h, &pic.data);
    }
    h.update((p.binary_tags.len() as u64).to_le_bytes());
    for bt in &p.binary_tags {
        feed(&mut h, bt.key.as_bytes());
        feed(&mut h, &bt.payload);
    }
    h.update((p.structural_blocks.len() as u64).to_le_bytes());
    for (kind, body) in &p.structural_blocks {
        feed(&mut h, kind.as_bytes());
        feed(&mut h, body);
    }
    format!("{:x}", base16ct::HexDisplay(&h.finalize()))
}

/// Streaming SHA-256 of an entire backing file, hex-encoded. The authoritative
/// content identity; reads the whole file, so callers gate it on the `Full` tier
/// or a strict-confirmation need.
pub(crate) fn full_file_hash(path: &std::path::Path) -> std::io::Result<String> {
    use sha2::{Digest, Sha256};
    let mut f = std::fs::File::open(path)?;
    let mut h = Sha256::new();
    let mut buf = [0u8; 1 << 16];
    loop {
        let n = std::io::Read::read(&mut f, &mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(format!("{:x}", base16ct::HexDisplay(&h.finalize())))
}
```

(`pic.picture_type.get()` mirrors the existing `ingest_into` usage. Confirm `Format` has `as_str()` — `ingest_into`'s `NewTrack` uses `probed.format` directly and `tracks.rs` calls `t.format.as_str()`, so it exists.)

- [ ] **Step 7: Re-export the enums**

In `musefs-core/src/lib.rs`, find the `pub use scan::{...}` line that exports `ScanOptions` and add `ChecksumTier, MatchStrictness` to it.

- [ ] **Step 8: Run tests, format**

Run: `cargo test -p musefs-core fingerprint_is_deterministic && cargo test -p musefs-core full_file_hash_matches && cargo test -p musefs-core checksum_tier_defaults`
Expected: PASS.

Run: `cargo fmt --all && cargo clippy -p musefs-core --all-targets -- -D warnings`
Expected: no warnings.

- [ ] **Step 9: Re-anchor and commit**

Run the re-anchor procedure from Critical Constraints §2, then:

```bash
git add musefs-core/Cargo.toml musefs-core/src/scan.rs musefs-core/src/lib.rs .cargo/mutants.toml Cargo.lock
git commit -m "feat(musefs-core): fingerprint/full-hash helpers + checksum tier options (#464)"
```

---

### Task B2: Compute checksums in the worker; persist via ingest

**Files:**
- Modify: `musefs-core/src/scan.rs` (`Unit`, worker closure, `run_pipeline`, `ingest`/`ingest_bulk`/`ingest_into`, `TrackSink`)

- [ ] **Step 1: Write a failing integration test for tier behavior**

Create `musefs-core/tests/checksums.rs` (reuse the `tests/common` helpers `make_flac`/`write_flac`/`scan_directory_with`):

```rust
mod common;
use common::*;
use musefs_core::{scan_directory_with, ChecksumTier, ScanOptions};
use musefs_db::Db;

fn opts(tier: ChecksumTier) -> ScanOptions {
    ScanOptions { jobs: 1, checksum: tier, ..Default::default() }
}

#[test]
fn full_tier_populates_both_columns_fingerprint_tier_only_one_none_neither() {
    for (tier, want_fp, want_ch) in [
        (ChecksumTier::None, false, false),
        (ChecksumTier::Fingerprint, true, false),
        (ChecksumTier::Full, true, true),
    ] {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("a.flac"),
            make_flac(&["TITLE=A"], &[0xAB; 32]),
        )
        .unwrap();
        let db = Db::open_in_memory().unwrap();
        scan_directory_with(&db, dir.path(), &opts(tier)).unwrap();
        let t = &db.list_tracks().unwrap()[0];
        assert_eq!(t.fingerprint.is_some(), want_fp, "tier {tier:?} fingerprint");
        assert_eq!(t.content_hash.is_some(), want_ch, "tier {tier:?} content_hash");
        if want_ch {
            assert_eq!(t.content_hash.as_ref().unwrap().len(), 64);
        }
    }
}
```

(Check `tests/common/mod.rs` for the exact `make_flac` signature — the scan integration test in `tests/scan.rs` uses `make_flac(&[(block,body)], audio)` while the CLI test uses `make_flac(&["TAG=V"], audio)`; use whichever the `musefs-core/tests/common` exposes.)

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p musefs-core --test checksums`
Expected: FAIL — all tiers currently leave both columns NULL.

- [ ] **Step 3: Add checksum fields to `Unit`**

Edit the `Unit` struct (`replace_symbol_body` on `Unit`) to add:

```rust
struct Unit {
    abs_path: String,
    stamp: BackingStamp,
    probed: Probed,
    weight: u64,
    fingerprint: Option<String>,
    content_hash: Option<String>,
}
```

- [ ] **Step 4: Compute in the worker**

In `run_pipeline`, the worker closure builds the `Unit` after `payload_weight`. The closure currently captures `window` and `follow_symlinks`; also capture the tier (copy `let tier = opts.checksum;` before spawning workers, alongside the existing `let window = ...`). In the worker, before constructing `Unit`, compute:

```rust
                let fingerprint = match tier {
                    ChecksumTier::None => None,
                    ChecksumTier::Fingerprint | ChecksumTier::Full => Some(fingerprint_of(&probed)),
                };
                let content_hash = match tier {
                    ChecksumTier::Full => match full_file_hash(std::path::Path::new(&abs_path)) {
                        Ok(h) => Some(h),
                        Err(e) => {
                            log::warn!("content hash failed for {abs_path}: {e}");
                            None
                        }
                    },
                    _ => None,
                };
                let unit = Unit {
                    abs_path,
                    stamp,
                    probed,
                    weight,
                    fingerprint,
                    content_hash,
                };
```

(`abs_path` is already computed above this point in the closure; `fingerprint_of` borrows `probed` before it's moved into `Unit`.)

- [ ] **Step 5: Add `set_track_checksums` to `TrackSink` and persist in `ingest_into`**

In `scan.rs`, add to the `TrackSink` trait:

```rust
    fn set_track_checksums(
        &mut self,
        track_id: i64,
        fingerprint: Option<&str>,
        content_hash: Option<&str>,
    ) -> musefs_db::Result<()>;
```

Implement it in both `impl TrackSink for &Db` and `impl TrackSink for &mut BulkWriter` by delegating to the methods added in Task A2 (e.g. `(*self).set_track_checksums(track_id, fingerprint, content_hash)` for `&Db`; `BulkWriter::set_track_checksums` for the bulk impl).

Change `ingest_into`'s signature to thread the checksums and persist them after the track row is written:

```rust
fn ingest_into(
    mut w: impl TrackSink,
    abs_path: &str,
    stamp: BackingStamp,
    probed: Probed,
    fingerprint: Option<&str>,
    content_hash: Option<&str>,
) -> Result<()> {
    let track_id = w.upsert_track(&NewTrack { /* unchanged */ })?;
    w.set_track_checksums(track_id, fingerprint, content_hash)?;
    // ... rest unchanged (tags, binary tags, structural, art) ...
}
```

- [ ] **Step 6: Update `ingest`/`ingest_bulk` call sites**

`ingest_bulk` and `ingest` consume a `Unit`/probed and call `ingest_into`. Update them to pass `unit.fingerprint.as_deref()` and `unit.content_hash.as_deref()`. Find both call sites (`find_referencing_symbols` on `ingest_into`) and add the two args. For the direct `ingest` path (single `&Db`, used by tests), compute the checksums the same way if it doesn't go through a `Unit` — check its body; if `ingest` is only a thin wrapper used in tests, pass `None, None` or compute via tier as appropriate. (The production path is `run_pipeline` → `ingest_bulk`.)

- [ ] **Step 7: Run the test**

Run: `cargo test -p musefs-core --test checksums`
Expected: PASS.

Run: `cargo test -p musefs-core`
Expected: PASS.

- [ ] **Step 8: Format, re-anchor, commit**

Run: `cargo fmt --all && cargo clippy -p musefs-core --all-targets -- -D warnings`, then the re-anchor procedure (§2).

```bash
git add musefs-core/src/scan.rs .cargo/mutants.toml
git commit -m "feat(musefs-core): compute + persist fingerprint/content_hash during scan (#464)"
```

---

## Phase C — Refind / retarget on scan (scan.rs; re-anchoring required)

### Task C: Retarget a moved file on a normal scan

**Files:**
- Modify: `musefs-core/src/scan.rs` (`TrackSink` lookups, refind decision, wire into the ingest path)

- [ ] **Step 1: Write failing integration tests for the refind matrix**

Append to `musefs-core/tests/checksums.rs`:

```rust
use musefs_core::MatchStrictness;

fn full_opts(strictness: MatchStrictness) -> ScanOptions {
    ScanOptions { jobs: 1, checksum: ChecksumTier::Full, strictness, ..Default::default() }
}

fn write_a_flac(dir: &std::path::Path, name: &str, audio: &[u8]) -> std::path::PathBuf {
    let p = dir.join(name);
    std::fs::write(&p, make_flac(&["TITLE=A"], audio)).unwrap();
    p
}

#[test]
fn pure_move_retargets_keeping_id_and_tags() {
    let dir = tempfile::tempdir().unwrap();
    let old = write_a_flac(dir.path(), "old.flac", &[0xAB; 64]);
    let db = Db::open_in_memory().unwrap();
    scan_directory_with(&db, dir.path(), &full_opts(MatchStrictness::Auto)).unwrap();
    let id = db.list_tracks().unwrap()[0].id;

    // Move the file and rescan the directory.
    let new = dir.path().join("new.flac");
    std::fs::rename(&old, &new).unwrap();
    scan_directory_with(&db, dir.path(), &full_opts(MatchStrictness::Auto)).unwrap();

    let tracks = db.list_tracks().unwrap();
    assert_eq!(tracks.len(), 1, "moved file must not create a second row");
    assert_eq!(tracks[0].id, id, "retarget keeps the id");
    assert!(tracks[0].backing_path.ends_with("new.flac"));
}

#[test]
fn copy_with_original_present_inserts_fresh() {
    let dir = tempfile::tempdir().unwrap();
    let orig = write_a_flac(dir.path(), "orig.flac", &[0xAB; 64]);
    let db = Db::open_in_memory().unwrap();
    scan_directory_with(&db, dir.path(), &full_opts(MatchStrictness::Auto)).unwrap();

    std::fs::copy(&orig, dir.path().join("copy.flac")).unwrap();
    scan_directory_with(&db, dir.path(), &full_opts(MatchStrictness::Auto)).unwrap();

    assert_eq!(db.list_tracks().unwrap().len(), 2, "copy must not steal identity");
}

#[test]
fn strict_refuses_when_candidate_has_no_content_hash() {
    let dir = tempfile::tempdir().unwrap();
    let old = write_a_flac(dir.path(), "old.flac", &[0xCD; 64]);
    let db = Db::open_in_memory().unwrap();
    // Seed at fingerprint tier => candidate has fingerprint but no content_hash.
    scan_directory_with(
        &db, dir.path(),
        &ScanOptions { jobs: 1, checksum: ChecksumTier::Fingerprint, ..Default::default() },
    ).unwrap();
    let id = db.list_tracks().unwrap()[0].id;

    std::fs::rename(&old, dir.path().join("new.flac")).unwrap();
    scan_directory_with(&db, dir.path(), &full_opts(MatchStrictness::Strict)).unwrap();

    let tracks = db.list_tracks().unwrap();
    // Strict cannot confirm (no candidate content_hash) => fresh insert, old orphaned.
    assert!(tracks.iter().any(|t| t.id != id && t.backing_path.ends_with("new.flac")));
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p musefs-core --test checksums pure_move_retargets`
Expected: FAIL — a move currently produces a second row (no retarget).

- [ ] **Step 3: Add lookup methods to `TrackSink`**

Add to the `TrackSink` trait:

```rust
    fn track_exists_at(&mut self, path: &str) -> musefs_db::Result<bool>;
    fn tracks_by_fingerprint(&mut self, fp: &str) -> musefs_db::Result<Vec<musefs_db::Track>>;
    #[allow(clippy::too_many_arguments)]
    fn retarget_track(
        &mut self,
        id: i64,
        new_backing_path: &str,
        stamp: BackingStamp,
        audio_offset: u64,
        audio_length: u64,
        fingerprint: Option<&str>,
        content_hash: Option<&str>,
    ) -> musefs_db::Result<()>;
```

Implement in both impls. For `&Db`: `track_exists_at` → `self.get_track_by_path(path)?.is_some()`; `tracks_by_fingerprint` → `(*self).tracks_by_fingerprint(fp)`; `retarget_track` → `(*self).retarget_track(id, new_backing_path, stamp.size, stamp.mtime_ns, stamp.ctime_ns, audio_offset, audio_length, fingerprint, content_hash)`. For `&mut BulkWriter`: delegate to the `BulkWriter` methods from Task A2 (same arg shape).

- [ ] **Step 4: Add the refind decision function**

Add to `scan.rs` (near `ingest_into`, via `insert_after_symbol`):

```rust
/// Decide how to ingest one probed unit: retarget a relocated row when a unique
/// fingerprint match exists whose backing file is gone, otherwise ingest fresh.
fn ingest_unit(
    mut w: impl TrackSink,
    unit: Unit,
    strictness: MatchStrictness,
) -> Result<()> {
    // Known path => ordinary upsert (re-scan of an in-place file).
    if w.track_exists_at(&unit.abs_path)? {
        return ingest_into(
            w, &unit.abs_path, unit.stamp, unit.probed,
            unit.fingerprint.as_deref(), unit.content_hash.as_deref(),
        );
    }
    if let Some(fp) = unit.fingerprint.as_deref() {
        let candidates: Vec<musefs_db::Track> = w
            .tracks_by_fingerprint(fp)?
            .into_iter()
            .filter(|t| !std::path::Path::new(&t.backing_path).exists())
            .collect();
        match candidates.len() {
            1 => {
                let cand = &candidates[0];
                if confirm_match(cand, &unit, strictness)? && !w.track_exists_at(&unit.abs_path)? {
                    w.retarget_track(
                        cand.id, &unit.abs_path, unit.stamp,
                        unit.probed.audio_offset, unit.probed.audio_length,
                        unit.fingerprint.as_deref(), unit.content_hash.as_deref(),
                    )?;
                    return Ok(());
                }
            }
            n if n > 1 => {
                log::warn!(
                    "ambiguous fingerprint match for {} ({n} missing candidates); inserting fresh",
                    unit.abs_path
                );
            }
            _ => {}
        }
    }
    ingest_into(
        w, &unit.abs_path, unit.stamp, unit.probed,
        unit.fingerprint.as_deref(), unit.content_hash.as_deref(),
    )
}

/// Confirm a fingerprint match per the strictness policy. May read the new
/// file's bytes (only on an actual refind candidate) to compute its full hash.
fn confirm_match(
    candidate: &musefs_db::Track,
    unit: &Unit,
    strictness: MatchStrictness,
) -> Result<bool> {
    match strictness {
        MatchStrictness::Fast => Ok(true),
        MatchStrictness::Auto => match &candidate.content_hash {
            None => Ok(true),
            Some(stored) => Ok(new_file_hash(unit)? .as_deref() == Some(stored.as_str())),
        },
        MatchStrictness::Strict => match &candidate.content_hash {
            None => Ok(false),
            Some(stored) => Ok(new_file_hash(unit)?.as_deref() == Some(stored.as_str())),
        },
    }
}

/// The new file's full hash: reuse the worker-computed one if present, else read
/// the file now (the file is present — it's the move destination).
fn new_file_hash(unit: &Unit) -> Result<Option<String>> {
    if unit.content_hash.is_some() {
        return Ok(unit.content_hash.clone());
    }
    Ok(Some(full_file_hash(std::path::Path::new(&unit.abs_path))?))
}
```

(`full_file_hash` returns `io::Result`; ensure `Result` here is the crate scan `Result` with a `From<io::Error>` — check how `revalidate_with` converts `std::fs` errors; it uses `?` on `std::fs::canonicalize`, so the crate error already converts from `io::Error`. If `confirm_match`'s `?` on `full_file_hash` doesn't convert, wrap with `.map_err(...)` to the crate error.)

- [ ] **Step 5: Route the writer through `ingest_unit`**

In `run_pipeline`, the writer's `flush` currently hands a batch to `ingest_bulk`. Change the per-unit ingest so each `Unit` goes through `ingest_unit(&mut bulk_writer, unit, strictness)` instead of the direct `ingest_into`/`ingest_bulk` body. Capture `let strictness = opts.strictness;` near the top of `run_pipeline`. Inspect `ingest_bulk` (it iterates the batch calling `ingest_into`); replace its inner `ingest_into(...)` call with `ingest_unit(&mut *w, unit, strictness)`, threading `strictness` into `ingest_bulk`'s signature. Keep the bulk transaction wrapper unchanged so all units in a batch share one transaction (and `track_exists_at`/`tracks_by_fingerprint` see prior retargets within the batch — the within-scan double-claim guard).

- [ ] **Step 6: Run the refind tests**

Run: `cargo test -p musefs-core --test checksums`
Expected: PASS (move retargets, copy inserts fresh, strict refuses without a hash).

Run: `cargo test -p musefs-core`
Expected: PASS.

- [ ] **Step 7: Format, re-anchor, commit**

Run: `cargo fmt --all && cargo clippy -p musefs-core --all-targets -- -D warnings`, then the re-anchor procedure (§2).

```bash
git add musefs-core/src/scan.rs .cargo/mutants.toml
git commit -m "feat(musefs-core): retarget relocated files on scan via fingerprint (#464)"
```

---

## Phase D — CLI flags (musefs-cli; no re-anchoring)

### Task D: `--checksum`, `--fast`, `--strict`

**Files:**
- Modify: `musefs-cli/src/lib.rs` (`ChecksumMode` enum, `Command::Scan` fields, `run`/`run_scan`)
- Test: `musefs-cli/tests/cli.rs`, `musefs/tests/cli_process.rs`

- [ ] **Step 1: Write failing parse + behavior tests**

In `musefs-cli/tests/cli.rs`, add (mirror the existing `parses_mode_and_revalidate_flags` test):

```rust
#[test]
fn scan_parses_checksum_and_strictness_flags() {
    let cli = Cli::parse_from([
        "musefs", "scan", "/lib", "--db", "/tmp/m.db",
        "--checksum", "full", "--strict",
    ]);
    match cli.command {
        Command::Scan { checksum, strict, fast, .. } => {
            assert_eq!(checksum, musefs_cli::ChecksumMode::Full);
            assert!(strict);
            assert!(!fast);
        }
        _ => panic!("expected scan"),
    }
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p musefs-cli scan_parses_checksum`
Expected: FAIL to compile (`ChecksumMode`, `checksum`/`strict`/`fast` fields absent).

- [ ] **Step 3: Add the `ChecksumMode` enum**

In `musefs-cli/src/lib.rs`, after the `CliMode` enum (mirror its pattern), add:

```rust
/// CLI surface for `musefs_core::ChecksumTier`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum ChecksumMode {
    /// No checksums.
    None,
    /// Cheap fingerprint only (default).
    Fingerprint,
    /// Fingerprint plus full-file SHA-256.
    Full,
}

impl From<ChecksumMode> for musefs_core::ChecksumTier {
    fn from(m: ChecksumMode) -> musefs_core::ChecksumTier {
        match m {
            ChecksumMode::None => musefs_core::ChecksumTier::None,
            ChecksumMode::Fingerprint => musefs_core::ChecksumTier::Fingerprint,
            ChecksumMode::Full => musefs_core::ChecksumTier::Full,
        }
    }
}
```

- [ ] **Step 4: Add the flags to `Command::Scan`**

In the `Command::Scan { ... }` variant, add after `quiet`:

```rust
        /// Which content checksums to compute and store (none|fingerprint|full).
        #[arg(long, value_enum, env = "MUSEFS_CHECKSUM", default_value_t = ChecksumMode::Fingerprint)]
        checksum: ChecksumMode,
        /// Confirm a move only by fingerprint, never reading the full file.
        #[arg(long, value_parser = clap::builder::BoolishValueParser::new())]
        fast: bool,
        /// Require a full-hash match to retarget a moved file.
        #[arg(long, value_parser = clap::builder::BoolishValueParser::new())]
        strict: bool,
```

- [ ] **Step 5: Thread into `run` and `run_scan`**

In `run`, extend the `Command::Scan { .. }` destructure to bind `checksum`, `fast`, `strict` and pass them to `run_scan`. Extend `run_scan`'s signature with `checksum: ChecksumMode, fast: bool, strict: bool`, and build the strictness:

```rust
    let strictness = match (fast, strict) {
        (true, true) => anyhow::bail!("--fast and --strict are mutually exclusive"),
        (true, false) => musefs_core::MatchStrictness::Fast,
        (false, true) => musefs_core::MatchStrictness::Strict,
        (false, false) => musefs_core::MatchStrictness::Auto,
    };
    let opts = musefs_core::ScanOptions {
        jobs,
        follow_symlinks,
        progress: reporter.sink(),
        checksum: checksum.into(),
        strictness,
        ..Default::default()
    };
```

Re-export `ChecksumMode` from `musefs-cli` if the test references `musefs_cli::ChecksumMode` (it's `pub` in `lib.rs`, so it's already accessible).

- [ ] **Step 6: Update the existing `run_scan` callers**

Other call sites of `run_scan` (e.g. `musefs-cli/tests/scan.rs:run_scan(...)`) now need the three extra args. Update them to pass `ChecksumMode::Fingerprint, false, false` (or `ChecksumMode::None` where the test asserts no checksums). Find them with `find_referencing_symbols` on `run_scan`.

- [ ] **Step 7: Add a help-text + end-to-end binary test**

In `musefs/tests/cli_process.rs`, extend `scan_help_lists_env_vars` to also assert `stdout.contains("MUSEFS_CHECKSUM")`. Add an e2e test that a `--checksum full` scan exits 0 (mirror `scan_succeeds_and_ingests_through_the_binary`, adding `.arg("--checksum").arg("full")`).

- [ ] **Step 8: Run, format, commit**

Run: `cargo test -p musefs-cli && cargo test -p musefs --test cli_process && cargo fmt --all && cargo clippy --all-targets -- -D warnings`
Expected: PASS / no warnings.

```bash
git add musefs-cli/src/lib.rs musefs-cli/tests/cli.rs musefs-cli/tests/scan.rs musefs/tests/cli_process.rs
git commit -m "feat(musefs-cli): --checksum / --fast / --strict scan flags (#464)"
```

---

## Phase E — Revalidate backfill, benchmark, docs

### Task E1: Revalidate computes checksums + backfills missing ones

**Files:**
- Modify: `musefs-core/src/scan.rs` (`revalidate_with` skip-unchanged gate)
- Test: `musefs-core/tests/checksums.rs`

- [ ] **Step 1: Write a failing backfill test**

Append to `musefs-core/tests/checksums.rs`:

```rust
use musefs_core::revalidate_with;

#[test]
fn revalidate_backfills_fingerprint_on_unchanged_files() {
    let dir = tempfile::tempdir().unwrap();
    write_a_flac(dir.path(), "a.flac", &[0xAB; 64]);
    let db = Db::open_in_memory().unwrap();
    // Initial scan with no checksums.
    scan_directory_with(
        &db, dir.path(),
        &ScanOptions { jobs: 1, checksum: ChecksumTier::None, ..Default::default() },
    ).unwrap();
    assert!(db.list_tracks().unwrap()[0].fingerprint.is_none());

    // Revalidate at the fingerprint tier: the file is unchanged but missing the
    // fingerprint, so it must be re-processed (backfilled), not skipped.
    let stats = revalidate_with(
        &db, dir.path(),
        &ScanOptions { jobs: 1, checksum: ChecksumTier::Fingerprint, ..Default::default() },
    ).unwrap();
    assert!(db.list_tracks().unwrap()[0].fingerprint.is_some(), "backfilled");
    assert_eq!(stats.updated, 1);
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p musefs-core --test checksums revalidate_backfills`
Expected: FAIL — the unchanged file is skipped, fingerprint stays NULL.

- [ ] **Step 3: Extend the skip-unchanged gate**

In `revalidate_with`, the skip pass loads `existing: HashMap<String, (BackingStamp, i64, Format)>` and skips a file when `BackingStamp::from_metadata == stamp && !needs_backfill`. Add a checksum-backfill condition mirroring the existing FLAC `needs_backfill`:

- When building `existing`, also capture whether each track already has the checksum the tier requires. Change the map value to include the booleans, e.g. load `(stamp, id, format, has_fingerprint, has_content_hash)` (read `t.fingerprint.is_some()` / `t.content_hash.is_some()` from `db.list_tracks()`).
- Compute a `needs_checksum` flag for the tier:

```rust
            let needs_checksum = match opts.checksum {
                ChecksumTier::None => false,
                ChecksumTier::Fingerprint => !has_fingerprint,
                ChecksumTier::Full => !has_fingerprint || !has_content_hash,
            };
            if crate::freshness::BackingStamp::from_metadata(&meta) == stamp
                && !needs_backfill
                && !needs_checksum
            {
                unchanged += 1;
                continue;
            }
```

Since `revalidate_with` calls `run_pipeline(db, changed, opts)` (Task B2 made that compute + persist per tier), backfilled files get their checksums written automatically. No other revalidate change is needed — the prune-on-missing step stays exactly as-is.

- [ ] **Step 4: Run tests**

Run: `cargo test -p musefs-core --test checksums && cargo test -p musefs-core`
Expected: PASS.

- [ ] **Step 5: Format, re-anchor, commit**

Run: `cargo fmt --all && cargo clippy -p musefs-core --all-targets -- -D warnings`, then the re-anchor procedure (§2).

```bash
git add musefs-core/src/scan.rs .cargo/mutants.toml
git commit -m "feat(musefs-core): revalidate backfills missing checksums per tier (#464)"
```

---

### Task E2: Fingerprint-overhead benchmark

**Files:**
- Create: `musefs-core/benches/fingerprint_overhead.rs`
- Modify: `musefs-core/Cargo.toml` (`[[bench]]` entry)

- [ ] **Step 1: Add the bench target to Cargo.toml**

Look at how `benches/read_throughput.rs` is registered in `musefs-core/Cargo.toml` (a `[[bench]] name = "read_throughput" harness = false` block) and add a parallel one:

```toml
[[bench]]
name = "fingerprint_overhead"
harness = false
```

- [ ] **Step 2: Write the bench**

Create `musefs-core/benches/fingerprint_overhead.rs`. Model it on `read_throughput.rs` (corpus on tmpfs per the bench harness). It scans a generated library at `ChecksumTier::None` vs `ChecksumTier::Fingerprint` and reports the delta:

```rust
use criterion::{criterion_group, criterion_main, Criterion};
use musefs_core::{scan_directory_with, ChecksumTier, ScanOptions};
use musefs_db::Db;

fn build_library(n: usize) -> tempfile::TempDir {
    // Prefer $TMPDIR on tmpfs (RAM); the bench measures CPU overhead, not disk.
    let dir = tempfile::tempdir().unwrap();
    for i in 0..n {
        let bytes = musefs_format::fuzz_check::fixtures::make_flac(
            &[(0, vec![0u8; 34]), (4, b"\x00\x00\x00\x00".to_vec())],
            &vec![0xAB; 4096],
        );
        std::fs::write(dir.path().join(format!("t{i}.flac")), &bytes).unwrap();
    }
    dir
}

fn bench_tiers(c: &mut Criterion) {
    let lib = build_library(200);
    let mut g = c.benchmark_group("scan_fingerprint_overhead");
    for tier in [ChecksumTier::None, ChecksumTier::Fingerprint] {
        g.bench_function(format!("{tier:?}"), |b| {
            b.iter(|| {
                let db = Db::open_in_memory().unwrap();
                let opts = ScanOptions { jobs: 1, checksum: tier, ..Default::default() };
                scan_directory_with(&db, lib.path(), &opts).unwrap();
            });
        });
    }
    g.finish();
}

criterion_group!(benches, bench_tiers);
criterion_main!(benches);
```

(Adjust the `make_flac` fixture call to the real `musefs_format::fuzz_check::fixtures` signature used by `musefs-core/tests/common`. If that helper isn't accessible from a bench, replicate the minimal-FLAC byte construction used in `scan_unit_tests`.)

- [ ] **Step 3: Run the bench**

Run: `cargo bench -p musefs-core --bench fingerprint_overhead`
Expected: completes, prints both tiers' times. Record the delta in `BENCHMARKS.md` (per the project's bench-logging convention) and use it to confirm/deny the `fingerprint` default in a follow-up note.

- [ ] **Step 4: Commit**

```bash
git add musefs-core/benches/fingerprint_overhead.rs musefs-core/Cargo.toml BENCHMARKS.md
git commit -m "bench(musefs-core): scan fingerprint-tier overhead (#464)"
```

---

### Task E3: Documentation + fuzz/dep-graph sanity

**Files:**
- Modify: `ARCHITECTURE.md`, `README.md`
- Modify: `docs/superpowers/specs/2026-06-15-backing-file-checksums-design.md` (status)

- [ ] **Step 1: Document the columns**

In `ARCHITECTURE.md`, in the store-schema/contract section, add a short paragraph that `tracks.fingerprint` and `tracks.content_hash` are scanner-owned, read-only-derived columns (like `structural_blocks`), never part of the editable tag contract, and that a normal `scan` uses them to retarget relocated backing files in place.

- [ ] **Step 2: Document the flags**

In `README.md`, under the `scan` usage, document `--checksum=none|fingerprint|full` (default `fingerprint`), `--fast`, `--strict`, and the workflow: move files → run `scan` to retarget; `revalidate` still prunes missing files.

- [ ] **Step 3: Confirm the fuzz crate still builds**

The format layer (`musefs-format`) is untouched, but verify the out-of-workspace fuzz crate compiles against the workspace:

Run: `cargo +nightly fuzz build 2>/dev/null || echo "nightly/cargo-fuzz unavailable — CI will check"`
Expected: builds, or the skip note (CI enforces).

- [ ] **Step 4: Flip the spec status**

In the spec header, change `Status:` to `implemented`.

- [ ] **Step 5: Full workspace gate + commit**

Run: `cargo fmt --all --check && cargo clippy --all-targets -- -D warnings && cargo test --workspace`
Expected: all PASS.

```bash
git add ARCHITECTURE.md README.md docs/superpowers/specs/2026-06-15-backing-file-checksums-design.md
git commit -m "docs: document backing-file checksums + move re-identification (#464)"
```

---

## Final verification (after all tasks)

- [ ] Run the full workspace suite: `cargo test --workspace` → PASS.
- [ ] Run the metrics-feature tests (CI's `check` job runs these; getattr/read counts are unaffected here but confirm): `cargo test -p musefs-core --features metrics` → PASS.
- [ ] Run all contrib Python suites with their correct envs (python-musefs/picard system Python; beets/lidarr venv) → PASS.
- [ ] Confirm `python3 scripts/check_mutant_anchors.py` passes (anchors re-validated) — or that CI will, if cargo-mutants is absent locally.
- [ ] Manual smoke (optional): on the live harness (`~/musefs.db` copy + a sample library), scan at `--checksum full`, move an album, re-`scan`, and confirm the tracks retargeted (same ids, tags intact) and the audio invariant holds (ffmpeg md5 unchanged).
