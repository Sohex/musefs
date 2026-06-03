# Phase 6 PR 1 — Refresh O(library) → O(changed) (#69) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make `poll_refresh` strictly O(changed): a trigger-maintained `track_changes` changelog replaces the O(N) `list_render_keys` identity scan, and the render-state snapshot is mutated in place instead of rebuilt.

**Architecture:** `MIGRATION_V3` adds a bounded changelog ring fed by triggers on `tracks` (every metadata edit already funnels through an `UPDATE tracks` via the V1 triggers). The mount keeps an in-memory `last_seq` watermark; a refresh reads only the changelog rows past it, renders only changed/added tracks, mutates the retained snapshot in place (capturing displaced old states for notification), and prunes caches by the exact removed set. A contiguity gap in the ring (mount slept past the cap) falls back to the existing full path, which is retained verbatim.

**Tech Stack:** Rust (rusqlite, SQLite triggers/AUTOINCREMENT), existing proptest equivalence harness, contrib Python mirror (`python-musefs` + vendored Picard copy).

**Spec:** `docs/superpowers/specs/2026-06-03-phase6-perf-sps-design.md` (read it first — especially "PR 1").

**Conventions that bind every task:** original audio bytes are never copied or modified; errors carry their source (no `.map_err(|_| …)` dropping diagnostics); migrations are append-only; only commit what the task names.

---

### Task 1: Branch

- [ ] **Step 1: Create the branch**

```bash
cd /home/cfutro/git/musefs
git checkout main && git checkout -b phase6-pr1-incremental-refresh
```

### Task 2: MIGRATION_V3 — changelog table + triggers

**Files:**
- Modify: `musefs-db/src/schema.rs`

- [ ] **Step 1: Write the failing tests**

Add a new test module after `migration_v2_tests` in `musefs-db/src/schema.rs`:

```rust
#[cfg(test)]
mod migration_v3_tests {
    use rusqlite::Connection;

    fn count_changes(conn: &Connection) -> i64 {
        conn.query_row("SELECT COUNT(*) FROM track_changes", [], |r| r.get(0))
            .unwrap()
    }

    fn insert_track(conn: &Connection, path: &str) {
        conn.execute(
            "INSERT INTO tracks (backing_path, format, audio_offset, audio_length, \
             backing_size, backing_mtime, updated_at) \
             VALUES (?1,'flac',0,1,1,0,0)",
            [path],
        )
        .unwrap();
    }

    #[test]
    fn v3_changelog_records_insert_update_delete() {
        let mut conn = Connection::open_in_memory().unwrap();
        super::migrate(&mut conn).unwrap();
        let uv: i64 = conn
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap();
        assert_eq!(uv, 3);

        insert_track(&conn, "/a.flac"); // tracks AI -> 1 row
        assert_eq!(count_changes(&conn), 1);

        conn.execute(
            "UPDATE tracks SET backing_mtime = 1 WHERE id = 1", // tracks AU -> 1 row
            [],
        )
        .unwrap();
        assert_eq!(count_changes(&conn), 2);

        conn.execute("DELETE FROM tracks WHERE id = 1", []).unwrap(); // tracks AD -> 1 row
        assert_eq!(count_changes(&conn), 3);

        let ids: Vec<i64> = conn
            .prepare("SELECT track_id FROM track_changes ORDER BY seq")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(ids, vec![1, 1, 1]);
    }

    /// Load-bearing nested-trigger dependency (see spec): a bare tag write fires
    /// tags_ai -> UPDATE tracks -> tracks changelog trigger. If this fails, nested
    /// activation is off in this SQLite build; the fix is PRAGMA-level, not schema.
    #[test]
    fn v3_bare_tag_insert_produces_changelog_row_via_nested_trigger() {
        let mut conn = Connection::open_in_memory().unwrap();
        super::migrate(&mut conn).unwrap();
        insert_track(&conn, "/a.flac");
        let before = count_changes(&conn);
        conn.execute(
            "INSERT INTO tags (track_id, key, value, ordinal) VALUES (1,'artist','A',0)",
            [],
        )
        .unwrap();
        assert_eq!(
            count_changes(&conn),
            before + 1,
            "tags_ai's UPDATE tracks must fire the changelog trigger (nested activation)"
        );
        let last_id: i64 = conn
            .query_row(
                "SELECT track_id FROM track_changes ORDER BY seq DESC LIMIT 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(last_id, 1);
    }

    #[test]
    fn v3_prune_keeps_ring_bounded_and_contiguous() {
        let mut conn = Connection::open_in_memory().unwrap();
        super::migrate(&mut conn).unwrap();
        insert_track(&conn, "/a.flac");
        // Drive CAP + 100 changelog inserts via track updates.
        for i in 0..(super::CHANGELOG_CAP + 100) {
            conn.execute("UPDATE tracks SET backing_mtime = ?1 WHERE id = 1", [i])
                .unwrap();
        }
        let (min_seq, max_seq, rows): (i64, i64, i64) = conn
            .query_row(
                "SELECT MIN(seq), MAX(seq), COUNT(*) FROM track_changes",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(rows, super::CHANGELOG_CAP, "ring must hold exactly CAP rows");
        assert_eq!(min_seq, max_seq - super::CHANGELOG_CAP + 1, "contiguous");
    }

    #[test]
    fn v2_db_upgrades_to_v3_preserving_rows() {
        let mut conn = Connection::open_in_memory().unwrap();
        // Apply V1+V2 only, stamp version 2, insert under the V2 schema.
        conn.execute_batch(super::MIGRATIONS[0]).unwrap();
        conn.execute_batch(super::MIGRATIONS[1]).unwrap();
        conn.pragma_update(None, "user_version", 2i64).unwrap();
        insert_track(&conn, "/legacy.flac");

        super::migrate(&mut conn).unwrap();
        let uv: i64 = conn
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap();
        assert_eq!(uv, 3);
        // Pre-migration rows produce no retroactive changelog entries...
        assert_eq!(count_changes(&conn), 0);
        // ...but post-migration edits do.
        conn.execute("UPDATE tracks SET backing_mtime = 9 WHERE id = 1", [])
            .unwrap();
        assert_eq!(count_changes(&conn), 1);
    }

    /// The SQL literal and the exported constant must not drift.
    #[test]
    fn changelog_cap_constant_matches_migration_sql() {
        assert!(super::MIGRATIONS[2].contains(&format!("NEW.seq - {}", super::CHANGELOG_CAP)));
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p musefs-db migration_v3`
Expected: FAIL — `MIGRATIONS[2]` out of bounds / `CHANGELOG_CAP` not found.

- [ ] **Step 3: Implement MIGRATION_V3**

In `musefs-db/src/schema.rs`, after `MIGRATION_V2`:

```rust
/// Ring capacity of the `track_changes` changelog. Must match the literal in
/// MIGRATION_V3 (guarded by `changelog_cap_constant_matches_migration_sql`).
pub const CHANGELOG_CAP: i64 = 8192;

const MIGRATION_V3: &str = r"
-- Bounded changelog ring for O(changed) refresh. Every metadata edit funnels
-- through an UPDATE on the tracks row (the V1 tags/track_art triggers), so
-- triggers on tracks alone capture all writers. Relies on SQLite nested
-- trigger activation (on by default; distinct from PRAGMA recursive_triggers).
CREATE TABLE track_changes (
    seq      INTEGER PRIMARY KEY AUTOINCREMENT,
    track_id INTEGER NOT NULL
);

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
";

const MIGRATIONS: &[&str] = &[MIGRATION_V1, MIGRATION_V2, MIGRATION_V3];
```

(Replace the existing two-element `MIGRATIONS` line.)

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p musefs-db`
Expected: all pass, including the nested-trigger test. **If the nested-trigger test fails** (changelog row count stays flat after a bare tag insert), nested activation is off in this rusqlite/SQLite build: add `conn.pragma_update(None, "recursive_triggers", true)?;` to `Db::configure` in `musefs-db/src/lib.rs` (both read-write and read-only arms) and re-run — and note the deviation in the commit message.

Note: other crates' tests now create V3 DBs; the workspace must still be green:

Run: `cargo test --workspace`
Expected: PASS (the Python-mirror version check is a separate suite, Task 10).

- [ ] **Step 5: Commit**

```bash
git add musefs-db/src/schema.rs
git commit -m "feat(db): MIGRATION_V3 — bounded track_changes changelog ring"
```

### Task 3: Db changed-set API

**Files:**
- Modify: `musefs-db/src/tracks.rs` (next to `list_render_keys`)
- Modify: `musefs-db/src/lib.rs` (re-export `ChangelogRead` if models are re-exported there; mirror how `Format` is exported)

- [ ] **Step 1: Write the failing tests**

Append to `musefs-db/tests/tracks.rs` (it already uses `common::new_track` and `Db::open_in_memory`, like `musefs-db/tests/art.rs`):

```rust
#[test]
fn changelog_since_returns_distinct_ids_and_seq_bounds() {
    let db = Db::open_in_memory().unwrap();
    let id1 = db.upsert_track(&new_track("/a.flac")).unwrap();
    let id2 = db.upsert_track(&new_track("/b.flac")).unwrap();
    db.replace_tags(id1, &[Tag::new("ARTIST", "X", 0)]).unwrap();
    db.replace_tags(id1, &[Tag::new("ARTIST", "Y", 0)]).unwrap();

    let log = db.changelog_since(0).unwrap();
    // Duplicates collapse: id1 appears once despite multiple changelog rows.
    assert_eq!(log.changed_ids, vec![id1, id2]);
    assert!(log.max_seq >= 2);
    assert_eq!(log.min_seq, 1);

    // A watermark past everything returns no ids but the same bounds.
    let later = db.changelog_since(log.max_seq).unwrap();
    assert!(later.changed_ids.is_empty());
    assert_eq!(later.max_seq, log.max_seq);
}

#[test]
fn changelog_since_empty_table_reports_zero_bounds() {
    let db = Db::open_in_memory().unwrap();
    let log = db.changelog_since(0).unwrap();
    assert!(log.changed_ids.is_empty());
    assert_eq!((log.min_seq, log.max_seq), (0, 0));
}

#[test]
fn render_keys_for_returns_only_requested_existing_ids() {
    let db = Db::open_in_memory().unwrap();
    let id1 = db.upsert_track(&new_track("/a.flac")).unwrap();
    let _id2 = db.upsert_track(&new_track("/b.flac")).unwrap();
    let keys = db.render_keys_for(&[id1, 999_999]).unwrap();
    assert_eq!(keys.len(), 1);
    assert_eq!(keys[0].0, id1);
}
```

(Import `Tag`/`ChangelogRead` alongside the file's existing `musefs_db` imports as needed.)

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p musefs-db changelog`
Expected: FAIL — `changelog_since` not found.

- [ ] **Step 3: Implement**

In `musefs-db/src/tracks.rs`, after `list_render_keys`:

```rust
/// One read of the changelog ring past `last_seq`: the distinct changed track
/// ids (ascending) plus the table's retained seq bounds (0/0 when empty). The
/// caller derives gap detection from `min_seq` (see musefs-core's refresh).
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ChangelogRead {
    pub changed_ids: Vec<i64>,
    pub min_seq: i64,
    pub max_seq: i64,
}
```

And in `impl Db`:

```rust
pub fn changelog_since(&self, last_seq: i64) -> Result<ChangelogRead> {
    let (min_seq, max_seq): (i64, i64) = self.conn.query_row(
        "SELECT COALESCE(MIN(seq),0), COALESCE(MAX(seq),0) FROM track_changes",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    let mut stmt = self.conn.prepare(
        "SELECT DISTINCT track_id FROM track_changes WHERE seq > ?1 ORDER BY track_id",
    )?;
    let changed_ids = stmt
        .query_map([last_seq], |r| r.get(0))?
        .collect::<rusqlite::Result<Vec<i64>>>()?;
    Ok(ChangelogRead {
        changed_ids,
        min_seq,
        max_seq,
    })
}

/// Render keys for a specific id set (the changelog ids); ids no longer in
/// `tracks` are simply absent from the result. Chunked like `tags_for_tracks`.
pub fn render_keys_for(&self, ids: &[i64]) -> Result<Vec<(i64, i64, Format)>> {
    const CHUNK: usize = 900;
    let mut out = Vec::with_capacity(ids.len());
    for chunk in ids.chunks(CHUNK) {
        let placeholders = vec!["?"; chunk.len()].join(",");
        let sql = format!(
            "SELECT id, content_version, format FROM tracks \
             WHERE id IN ({placeholders}) ORDER BY id"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let params = rusqlite::params_from_iter(chunk.iter());
        let rows = stmt.query_map(params, |r| {
            let fmt: String = r.get(2)?;
            let format = Format::parse(&fmt).ok_or_else(|| {
                rusqlite::Error::FromSqlConversionFailure(
                    usize::MAX,
                    rusqlite::types::Type::Text,
                    format!("unknown format {fmt}").into(),
                )
            })?;
            Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?, format))
        })?;
        out.extend(rows.collect::<rusqlite::Result<Vec<_>>>()?);
    }
    Ok(out)
}

/// Test-only: delete changelog rows up to and including `seq`, simulating the
/// ring having pruned past a sleeping mount (gap-path coverage). Follows the
/// `set_format_for_test` precedent.
pub fn delete_changelog_through_for_test(&self, seq: i64) -> Result<()> {
    self.conn
        .execute("DELETE FROM track_changes WHERE seq <= ?1", [seq])?;
    Ok(())
}
```

Match `ChangelogRead`'s export to wherever `Format`/`Tag` are re-exported from `musefs-db` (likely `lib.rs` `pub use`).

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p musefs-db`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add musefs-db/src/tracks.rs musefs-db/src/lib.rs
git commit -m "feat(db): changelog_since / render_keys_for changed-set API"
```

### Task 4: Changelog-driven partition

**Files:**
- Modify: `musefs-core/src/refresh_diff.rs`

- [ ] **Step 1: Write the failing tests**

Append to `refresh_diff.rs`'s test module (it has one for `partition_changes`; mirror its style):

```rust
#[test]
fn changelog_partition_classifies_changed_added_removed_and_churn() {
    use musefs_db::Format;
    let state = |cv: i64| TrackRenderState {
        content_version: cv,
        format: Format::Flac,
        path: "p".into(),
    };
    // prev knows 1 (cv 5), 2 (cv 1), 3 (cv 9).
    let prev_states: HashMap<i64, TrackRenderState> =
        [(1, state(5)), (2, state(1)), (3, state(9))].into();
    // Changelog mentioned 1,2,3,4,5. Live keys: 1 unchanged, 2 bumped, 4 new.
    // 3 is gone (removed); 5 was added+deleted between polls (pure churn).
    let changelog_ids = vec![1, 2, 3, 4, 5];
    let keys = vec![
        (1, 5, Format::Flac),
        (2, 2, Format::Flac),
        (4, 0, Format::Flac),
    ];
    let cs = partition_changelog(&prev_states, &changelog_ids, &keys);
    assert_eq!(cs.changed, vec![2]);
    assert_eq!(cs.added, vec![4]);
    assert_eq!(cs.removed, vec![3]); // churn id 5 is in neither output
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p musefs-core --lib refresh_diff`
Expected: FAIL — `partition_changelog` not found.

- [ ] **Step 3: Implement**

In `refresh_diff.rs`, after `partition_changes`:

```rust
/// Partition a changelog read against the previous snapshot. `prev_states` holds
/// the prior states of just the changelog ids (the caller extracts them under a
/// short snapshot lock); `keys` are the live render keys for those ids (absent =
/// no longer in `tracks`). An id that is neither live nor previously known —
/// added and deleted between polls — is pure churn and lands nowhere.
pub(crate) fn partition_changelog(
    prev_states: &HashMap<i64, TrackRenderState>,
    changelog_ids: &[i64],
    keys: &[(i64, i64, Format)],
) -> ChangeSet {
    let live: HashMap<i64, (i64, Format)> =
        keys.iter().map(|&(id, cv, f)| (id, (cv, f))).collect();
    let mut cs = ChangeSet::default();
    for &id in changelog_ids {
        match (live.get(&id), prev_states.get(&id)) {
            (Some(&(cv, fmt)), Some(s)) if s.content_version != cv || s.format != fmt => {
                cs.changed.push(id);
            }
            (Some(_), Some(_)) => {} // no-op touch: render key unchanged
            (Some(_), None) => cs.added.push(id),
            (None, Some(_)) => cs.removed.push(id),
            (None, None) => {} // churn: added+removed between polls
        }
    }
    cs
}
```

(`changelog_ids` is ascending from `changelog_since`, so the output vectors are ascending too — same determinism contract as `partition_changes`.)

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p musefs-core --lib`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add musefs-core/src/refresh_diff.rs
git commit -m "feat(core): changelog-driven change partition"
```

### Task 5: `apply_changes` takes the snapshot map

**Files:**
- Modify: `musefs-core/src/tree.rs` (`apply_changes`, `rebuild_subtree`, their tests)
- Modify: `musefs-core/src/facade.rs` (`rebuild_incremental` call site — compile fix only; full rework is Task 7)

`apply_changes(new_paths: &HashMap<i64, String>, …)` requires every current track's path (sibling disambiguation in `rebuild_subtree`). To avoid materializing an O(N) string-clone map per refresh, change the parameter type to the snapshot itself.

- [ ] **Step 1: Change the signatures**

In `tree.rs`, change `apply_changes` and `rebuild_subtree` to take
`new_paths: &std::collections::HashMap<i64, crate::refresh_diff::TrackRenderState>`, and change every path access from `new_paths.get(&id)` (a `&String`) to `new_paths.get(&id).map(|s| s.path.as_str())`. Concretely, inside `apply_changes` the three lookup sites become:

```rust
let new_path = new_paths
    .get(&id)
    .map(|s| s.path.as_str())
    .ok_or(RebuildError::MissingRenderedPath(id))?;
```

and the comparison `&self.path_of(ino) == new_path` becomes `self.path_of(ino) == new_path`. Apply the same lookup change inside `rebuild_subtree` (find its uses of `new_paths`). If `apply_changes` is `pub` and the compiler complains about the `pub(crate)` `TrackRenderState` in a public signature, demote `apply_changes`/`rebuild_subtree` to `pub(crate)` — they are only called from `facade.rs`.

- [ ] **Step 2: Fix the two call sites**

In `facade.rs::rebuild_incremental`, the `new_paths` construction currently builds `HashMap<i64, String>`; as a *temporary compile fix* pass `&new_snapshot` directly to `tree.apply_changes(…)` and adjust the debug-assert/fallback arms to build their entries from it:

```rust
let mut entries: Vec<(i64, String)> = new_snapshot
    .iter()
    .map(|(&id, s)| (id, s.path.clone()))
    .collect();
```

(Both arms already sort by id; keep that.) Delete the now-unused `new_paths` map. Update any `tree.rs` unit tests that call `apply_changes` with a `HashMap<i64, String>` to build `HashMap<i64, TrackRenderState>` instead (path in `.path`, any `content_version`/`format` — the tree only reads `.path`).

- [ ] **Step 3: Run the equivalence gates**

Run: `cargo test -p musefs-core`
Expected: PASS — in particular `incremental_refresh` (the 64-case proptest) and the `tree` tests.

- [ ] **Step 4: Commit**

```bash
git add musefs-core/src/tree.rs musefs-core/src/facade.rs
git commit -m "refactor(core): apply_changes reads paths from the render snapshot"
```

### Task 6: `HeaderCache::remove`

**Files:**
- Modify: `musefs-core/src/reader.rs` (`impl HeaderCache`, `impl Shard`)

- [ ] **Step 1: Write the failing test**

In `reader.rs`'s test module, directly after `header_cache_retain_drops_absent_tracks` (reader.rs:1059) — it is the template; reuse its `write_flac_local`/`mk` fixture verbatim:

```rust
#[test]
fn header_cache_remove_drops_one_track_only() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open_in_memory().unwrap();
    let mk = |name: &str| {
        let path = dir.path().join(name);
        let (audio_offset, audio_length) = write_flac_local(&path);
        let meta = std::fs::metadata(&path).unwrap();
        db.upsert_track(&NewTrack {
            backing_path: path.to_string_lossy().to_string(),
            format: Format::Flac,
            audio_offset,
            audio_length,
            backing_size: meta.len() as i64,
            backing_mtime: mtime_secs(&meta),
        })
        .unwrap()
    };
    let keep = mk("keep.flac");
    let gone = mk("gone.flac");
    let cache = HeaderCache::new(Mode::Synthesis);
    let keep_a = cache.resolve(&db, keep).unwrap();
    let gone_a = cache.resolve(&db, gone).unwrap();

    cache.remove(gone);

    // The kept track stays the same cached Arc; the removed one re-resolves fresh.
    assert!(Arc::ptr_eq(&keep_a, &cache.resolve(&db, keep).unwrap()));
    assert!(!Arc::ptr_eq(&gone_a, &cache.resolve(&db, gone).unwrap()));
}

#[test]
fn shard_remove_key_reaccounts_bytes() {
    let mut s = Shard::new(1000);
    s.insert(1, entry(0, 100));
    s.insert(2, entry(0, 100));
    s.remove_key(1);
    assert!(s.get(1).is_none());
    assert!(s.get(2).is_some());
    assert_eq!(s.bytes, 100);
}
```

(`Shard::new`/`entry` are the helpers `shard_retain_keys_drops_dead_and_reaccounts` at reader.rs:1091 already uses.)

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p musefs-core --lib header_cache_remove`
Expected: FAIL — `remove` not found.

- [ ] **Step 3: Implement**

In `impl Shard` (next to `retain_keys`):

```rust
fn remove_key(&mut self, id: i64) {
    self.unlink(id);
    if let Some(n) = self.map.remove(&id) {
        self.bytes -= n.value.cache_bytes;
    }
}
```

In `impl HeaderCache` (next to `retain`):

```rust
/// Drop one track's cached resolution (changelog-refresh removal path).
pub fn remove(&self, id: i64) {
    crate::lock::lock_or_clear(self.shard(id), "header-cache shard (remove)").remove_key(id);
}
```

(Match `shard(id)`'s actual receiver/return — it exists at `reader.rs:188`.)

- [ ] **Step 4: Run tests, then commit**

Run: `cargo test -p musefs-core --lib`
Expected: PASS

```bash
git add musefs-core/src/reader.rs
git commit -m "feat(core): HeaderCache::remove for changed-set pruning"
```

### Task 7: Facade — changelog refresh flow

**Files:**
- Modify: `musefs-core/src/facade.rs`

This is the core task. Re-read the current `poll_refresh_notify` (facade.rs:452), `rebuild_incremental` (:315), `force_full_rebuild` (:283), `notify_changed` (:549), and `open` (:161) before starting.

- [ ] **Step 1: Add the watermark field**

To `struct Musefs`: `last_seq: AtomicI64,`. In `open()`, before constructing:

```rust
let last_seq = db.changelog_since(i64::MAX)?.max_seq;
```

and `last_seq: AtomicI64::new(last_seq),` in the struct literal. (Reading `max_seq` *after* `build_full` is the safe order: rows written during the build leave `seq > last_seq` for the first poll — one redundant re-check, never a missed change. Place the read after the `build_full` call.)

- [ ] **Step 2: Rework `rebuild_incremental`**

Replace the existing `rebuild_incremental` with a changelog-driven version. It returns `Ok(None)` on a ring gap (caller takes the full path):

```rust
/// Outcome of a successful changelog-driven incremental refresh: everything
/// `poll_refresh_notify` needs to notify and stamp without an O(N) pass.
struct IncrementalOutcome {
    change: ChangeSet,
    /// Old states displaced by the in-place mutation (changed ∪ removed ids).
    displaced: std::collections::HashMap<i64, TrackRenderState>,
    /// Freshly rendered states (changed ∪ added ids).
    new_states: std::collections::HashMap<i64, TrackRenderState>,
    new_seq: i64,
}

/// Changelog-driven incremental rebuild (#69): read only the changelog rows past
/// `last_seq`, render only changed/added tracks, mutate the snapshot in place,
/// and apply the delta to the tree. `Ok(None)` = the ring pruned past our
/// watermark (or was externally truncated); the caller falls back to the full
/// scan path. The tree is published here on success.
fn rebuild_incremental(&self) -> Result<Option<IncrementalOutcome>> {
    if self.force_rebuild_error.load(Ordering::Acquire) {
        return Err(CoreError::BackingChanged(
            "forced refresh failure".to_string(),
        ));
    }
    let last_seq = self.last_seq.load(Ordering::Acquire);

    // Phase 1 (DB, no VFS locks): changelog + live render keys.
    let (log, keys) = self.pool.with(|db| {
        let log = db.changelog_since(last_seq)?;
        let keys = db.render_keys_for(&log.changed_ids)?;
        Ok::<_, CoreError>((log, keys))
    })?;
    let gap = (log.max_seq == 0 && last_seq > 0)
        || (log.max_seq > 0 && log.min_seq > last_seq + 1);
    if gap {
        return Ok(None);
    }
    let new_seq = log.max_seq.max(last_seq);

    // Phase 2 (short snapshot lock): prior states of just the changelog ids.
    let prev_states: std::collections::HashMap<i64, TrackRenderState> = {
        let snap = crate::lock::lock_or_flag(&self.snapshot, &self.needs_rebuild, "snapshot");
        log.changed_ids
            .iter()
            .filter_map(|id| snap.get(id).map(|s| (*id, s.clone())))
            .collect()
    };
    let change = partition_changelog(&prev_states, &log.changed_ids, &keys);

    // Phase 3 (DB, no VFS locks): render changed ∪ added.
    let mut to_render: Vec<i64> = change.changed.clone();
    to_render.extend(change.added.iter().copied());
    let key_of: std::collections::HashMap<i64, (i64, Format)> =
        keys.iter().map(|&(id, cv, f)| (id, (cv, f))).collect();
    let new_states: std::collections::HashMap<i64, TrackRenderState> = if to_render.is_empty() {
        std::collections::HashMap::new()
    } else {
        let mut tags_by_track = self.pool.with(|db| Ok(db.tags_for_tracks(&to_render)?))?;
        to_render
            .iter()
            .map(|&id| {
                let (cv, fmt) = key_of[&id];
                let tags = tags_by_track.remove(&id).unwrap_or_default();
                (
                    id,
                    TrackRenderState {
                        content_version: cv,
                        format: fmt,
                        path: Self::render_one(&self.config, fmt, &tags),
                    },
                )
            })
            .collect()
    };

    // Phase 4 (snapshot + inodes locks, pure CPU): mutate in place, apply delta.
    let mut snap = crate::lock::lock_or_flag(&self.snapshot, &self.needs_rebuild, "snapshot");
    let mut displaced = std::collections::HashMap::new();
    for &id in &change.removed {
        if let Some(old) = snap.remove(&id) {
            displaced.insert(id, old);
        }
    }
    for (&id, state) in &new_states {
        if let Some(old) = snap.insert(id, state.clone()) {
            displaced.insert(id, old);
        }
    }

    let mut alloc = crate::lock::lock_or_flag(&self.inodes, &self.needs_rebuild, "inodes");
    let mut tree = (*self.tree.load_full()).clone(); // O(1) im clone
    let applied = if self.force_apply_fail.swap(false, Ordering::AcqRel) {
        Err(crate::tree::RebuildError::TestInjected) // test injection
    } else {
        tree.apply_changes(
            &snap,
            &change.changed,
            &change.added,
            &change.removed,
            &mut alloc,
        )
    };
    #[allow(clippy::single_match_else)]
    let tree = match applied {
        Ok(()) => {
            #[cfg(debug_assertions)]
            {
                let mut ref_alloc = alloc.clone();
                let mut entries: Vec<(i64, String)> =
                    snap.iter().map(|(&id, s)| (id, s.path.clone())).collect();
                entries.sort_by_key(|(id, _)| *id);
                let reference = VirtualTree::build_with(&entries, &mut ref_alloc);
                debug_assert!(
                    tree.equiv(&reference),
                    "incremental tree diverged from build_with"
                );
            }
            tree
        }
        Err(reason) => {
            log::warn!(
                "incremental tree mutation failed ({reason:?}); falling back to full rebuild"
            );
            let mut entries: Vec<(i64, String)> =
                snap.iter().map(|(&id, s)| (id, s.path.clone())).collect();
            entries.sort_by_key(|(id, _)| *id);
            VirtualTree::build_with(&entries, &mut alloc)
        }
    };
    self.tree.store(Arc::new(tree));
    drop(alloc);
    drop(snap);
    Ok(Some(IncrementalOutcome {
        change,
        displaced,
        new_states,
        new_seq,
    }))
}
```

Notes for the implementer:
- Import `partition_changelog` next to the existing `partition_changes` import; import `musefs_db::Format` if not already in scope.
- Lock order is snapshot → inodes, both held only across pure-CPU work (the #90 discipline). The mutated snapshot is published in place — there is no separate "store new snapshot" step anymore.
- Single-flighting (the `refreshing` CAS in the caller) is what makes the unlocked phase-2→4 window safe; do not call `rebuild_incremental` outside it.

- [ ] **Step 3: Add the delta notifier**

Next to `notify_changed` (which stays — the full/gap path uses it):

```rust
/// ChangeSet-driven counterpart of `notify_changed` (#69): same notification
/// rules, evaluated only over changed/removed ids. `displaced` holds the old
/// states the in-place mutation returned; `new_states` the fresh renders.
fn notify_changed_delta(
    change: &ChangeSet,
    displaced: &HashMap<i64, TrackRenderState>,
    new_states: &HashMap<i64, TrackRenderState>,
    old_tree: &VirtualTree,
    new_tree: &VirtualTree,
    on_changed: &mut impl FnMut(u64),
) {
    for &id in &change.changed {
        let (Some(os), Some(ns)) = (displaced.get(&id), new_states.get(&id)) else {
            continue;
        };
        if os.content_version != ns.content_version && os.path == ns.path {
            if let Some(ino) = new_tree.inode_of_track(id) {
                on_changed(ino);
            }
        }
        if ns.path != os.path {
            if let Some(ino) = old_tree.inode_of_track(id) {
                on_changed(ino);
            }
        }
    }
    for &id in &change.removed {
        if let Some(ino) = displaced
            .get(&id)
            .and_then(|_| old_tree.inode_of_track(id))
        {
            on_changed(ino);
        }
    }
}
```

- [ ] **Step 4: Rewire `poll_refresh_notify`**

Replace the block from `let old_tree = self.tree.load_full();` through `self.stamp_successful_poll();` (keeping all the early-return gates and the `refreshing` CAS + guard exactly as they are) with:

```rust
let old_tree = self.tree.load_full();
match self.rebuild_incremental() {
    Ok(Some(out)) => {
        // O(changed) cache maintenance: drop exactly the removed tracks.
        for &id in &out.change.removed {
            self.cache.remove(id);
            self.size_cache.remove(&id);
        }
        let tree = self.tree.load();
        Self::notify_changed_delta(
            &out.change,
            &out.displaced,
            &out.new_states,
            &old_tree,
            &tree,
            &mut on_changed,
        );
        self.last_seq.store(out.new_seq, Ordering::Release);
        self.last_data_version.store(version, Ordering::Release);
        if !out.change.is_empty() {
            self.refresh_gen.fetch_add(1, Ordering::AcqRel);
        }
        self.stamp_successful_poll();
        Ok(true)
    }
    Ok(None) => {
        // Ring gap: the mount slept past CHANGELOG_CAP changes (or the ring
        // was truncated). Take the retained full path — correct by
        // construction, and a bulk change wants a full rebuild anyway.
        log::info!("changelog gap; falling back to full refresh");
        let new_seq = self.pool.with(|db| Ok(db.changelog_since(i64::MAX)?.max_seq))?;
        let old_snapshot =
            crate::lock::lock_or_flag(&self.snapshot, &self.needs_rebuild, "snapshot").clone();
        let new_snapshot = match self.rebuild_full() {
            Ok(v) => v,
            Err(err) => {
                *crate::lock::lock_recover(&self.last_failed_refresh, "last_failed_refresh") =
                    Some(std::time::Instant::now());
                return Err(err);
            }
        };
        let tree = self.tree.load();
        let live = tree.track_ids();
        self.cache.retain(&live);
        self.size_cache.retain(|k, _| live.contains(k));
        Self::notify_changed(&old_snapshot, &new_snapshot, &old_tree, &tree, &mut on_changed);
        *crate::lock::lock_or_flag(&self.snapshot, &self.needs_rebuild, "snapshot") =
            new_snapshot;
        self.last_seq.store(new_seq, Ordering::Release);
        self.last_data_version.store(version, Ordering::Release);
        self.refresh_gen.fetch_add(1, Ordering::AcqRel);
        self.stamp_successful_poll();
        Ok(true)
    }
    Err(err) => {
        *crate::lock::lock_recover(&self.last_failed_refresh, "last_failed_refresh") =
            Some(std::time::Instant::now());
        Err(err)
    }
}
```

(`ChangeSet` needs an `is_empty()` — add it in `refresh_diff.rs` if missing: `self.changed.is_empty() && self.added.is_empty() && self.removed.is_empty()`.)

- [ ] **Step 5: Stamp `last_seq` on the other full-rebuild paths**

In `force_full_rebuild`, after the `let version = …` line add:

```rust
let new_seq = self.pool.with_poll(|db| Ok(db.changelog_since(i64::MAX)?.max_seq))?;
```

and `self.last_seq.store(new_seq, Ordering::Release);` next to the existing `last_data_version` store. Apply the same to the public `refresh()` method if it stamps versions (read its body; mirror whatever it does for `last_data_version`).

- [ ] **Step 6: Run the full equivalence suite**

Run: `cargo test -p musefs-core`
Expected: PASS — the entire existing `incremental_refresh.rs` suite (proptest equivalence, fallback injection, notify rules, no-op refresh) now exercises the changelog path and is the primary correctness gate. Debug-build runs also exercise the `build_with` debug-assert oracle on every refresh.

Run: `cargo test -p musefs-fuse`
Expected: PASS

- [ ] **Step 7: Commit**

```bash
git add musefs-core/src/facade.rs musefs-core/src/refresh_diff.rs
git commit -m "feat(core): changelog-driven O(changed) refresh (#69)"
```

### Task 8: Gap-fallback and O(changed)-maintenance tests

**Files:**
- Modify: `musefs-core/tests/incremental_refresh.rs`

- [ ] **Step 1: Write the tests**

```rust
#[test]
fn changelog_gap_falls_back_to_full_rebuild() {
    let target = small_corpus(4);
    let db_path = target.db_path.clone();
    let corpus = target.corpus_dir.clone();
    let db = Db::open(&db_path).unwrap();
    scan_directory(&db, &corpus).unwrap();
    let fs = Musefs::open(Db::open(&db_path).unwrap(), config()).unwrap();

    let writer = Db::open(&db_path).unwrap();
    let ids: Vec<i64> = writer.list_tracks().unwrap().iter().map(|t| t.id).collect();
    writer
        .replace_tags(ids[0], &[Tag::new("TITLE", "moved-by-gap", 0)])
        .unwrap();
    // Simulate the ring having pruned past the mount's watermark: drop every
    // retained row. The next poll must detect the gap and full-rebuild.
    let max_seq = writer.changelog_since(0).unwrap().max_seq;
    writer.delete_changelog_through_for_test(max_seq).unwrap();

    assert!(fs.poll_refresh().unwrap());
    let reference = Musefs::open(Db::open(&db_path).unwrap(), config()).unwrap();
    assert_eq!(
        tree_fingerprint(&fs).into_keys().collect::<Vec<_>>(),
        tree_fingerprint(&reference).into_keys().collect::<Vec<_>>(),
        "gap fallback must produce a tree identical to a fresh open"
    );
}

#[test]
fn removed_track_is_pruned_and_refresh_recovers_after_gap() {
    // After a gap-driven full rebuild, subsequent incremental refreshes work again
    // (the watermark re-anchors to the ring).
    let target = small_corpus(4);
    let db_path = target.db_path.clone();
    let corpus = target.corpus_dir.clone();
    let db = Db::open(&db_path).unwrap();
    scan_directory(&db, &corpus).unwrap();
    let fs = Musefs::open(Db::open(&db_path).unwrap(), config()).unwrap();
    let writer = Db::open(&db_path).unwrap();
    let ids: Vec<i64> = writer.list_tracks().unwrap().iter().map(|t| t.id).collect();

    let max_seq = writer.changelog_since(0).unwrap().max_seq;
    writer.delete_changelog_through_for_test(max_seq).unwrap();
    writer.delete_track(ids[0]).unwrap(); // post-truncation edit: min_seq > last_seq + 1? No —
    // the new row's seq continues from max_seq, so this is contiguous-after-truncation;
    // poll sees min_seq == max_seq + 1 > last_seq + 1 only if last_seq < max_seq. Here
    // last_seq == max_seq (open read it), so NO gap: the incremental path handles the delete.
    assert!(fs.poll_refresh().unwrap());

    writer.delete_track(ids[1]).unwrap();
    assert!(fs.poll_refresh().unwrap());

    let reference = Musefs::open(Db::open(&db_path).unwrap(), config()).unwrap();
    assert_eq!(
        tree_fingerprint(&fs).into_keys().collect::<Vec<_>>(),
        tree_fingerprint(&reference).into_keys().collect::<Vec<_>>()
    );
}
```

Adjust the second test's comment to match observed behavior — the point it pins is that deletes flow through the incremental path and the tree matches a fresh open.

- [ ] **Step 2: Run, fix, commit**

Run: `cargo test -p musefs-core --test incremental_refresh`
Expected: PASS

```bash
git add musefs-core/tests/incremental_refresh.rs
git commit -m "test(core): changelog gap fallback + post-gap recovery"
```

### Task 9: Extend the bench sweep

**Files:**
- Modify: `musefs-core/tests/bench_refresh.rs:101`

- [ ] **Step 1: Extend the sweep**

Change `for n in [100usize, 1000, 5000] {` to `for n in [100usize, 1000, 5000, 20000] {`.

- [ ] **Step 2: Commit**

```bash
git add musefs-core/tests/bench_refresh.rs
git commit -m "bench: extend refresh sweep to 20k tracks"
```

### Task 10: Python contract mirror

**Files:**
- Modify: `contrib/python-musefs/src/musefs_common/constants.py:1` (`EXPECTED_USER_VERSION = 3`)
- Modify: `contrib/python-musefs/tests/test_constants.py:5` (`== 3`)
- Regenerate: `contrib/picard/musefs/_common/` (vendored copy)

- [ ] **Step 1: Bump and re-vendor**

Edit the two files above, then:

```bash
python /home/cfutro/git/musefs/contrib/python-musefs/vendor_to_picard.py
```

- [ ] **Step 2: Run the three Python suites**

```bash
cd /home/cfutro/git/musefs/contrib/python-musefs && python -m pytest && ruff check . && ruff format --check .
cd /home/cfutro/git/musefs/contrib/beets && python -m pytest tests
cd /home/cfutro/git/musefs/contrib/picard && python -m pytest tests
```

Expected: PASS (the Picard drift-guard test validates the re-vendor; beets may need its editable installs per CLAUDE.md if the venv is stale). If beets/picard suites fail on a *version-check* assertion, those tests also pin the version — update them to 3.

- [ ] **Step 3: Commit**

```bash
git add contrib/python-musefs contrib/picard/musefs/_common contrib/beets
git commit -m "feat(contrib): mirror schema user_version 3"
```

### Task 11: Benchmarks — before/after

- [ ] **Step 1: Record "before" on main (4-point sweep)**

```bash
git stash --include-untracked   # only if the tree is dirty
git checkout main
# Apply the Task 9 one-line sweep edit UNCOMMITTED so before/after share sizes:
#   bench_refresh.rs: [100usize, 1000, 5000] -> [100usize, 1000, 5000, 20000]
cargo test -p musefs-core --release --test bench_refresh \
  bench_refresh_one_across_library_sizes -- --ignored --nocapture | tee /tmp/refresh-before.txt
git checkout -- musefs-core/tests/bench_refresh.rs
git checkout phase6-pr1-incremental-refresh
git stash pop   # only if stashed
```

- [ ] **Step 2: Record "after" on the branch**

```bash
cargo test -p musefs-core --release --test bench_refresh \
  bench_refresh_one_across_library_sizes -- --ignored --nocapture | tee /tmp/refresh-after.txt
cargo test -p musefs-core --release --test bench_refresh \
  bench_refresh_one_vs_many -- --ignored --nocapture | tee /tmp/refresh-one-vs-many.txt
```

**Acceptance:** the after sweep is flat — refresh-1@20000 within a few ms of refresh-1@100 (no linear slope), vs the before sweep's growth. If the slope persists, something is still O(N) on the refresh path: profile before proceeding (suspects: a leftover full-map clone, the snapshot lock clone in the gap path being hit, the debug-assert running in release — it must not).

- [ ] **Step 3: Write the BENCHMARKS.md section**

Add a `## Phase 6 PR 1 — Refresh O(changed) (#69)` section at the end of `BENCHMARKS.md`, matching the SP2 section's style: the before/after sweep table (wall ms per library size), the one-vs-many table, machine/storage caveats, and the reproduce commands from Steps 1–2.

- [ ] **Step 4: Commit**

```bash
git add BENCHMARKS.md
git commit -m "bench: record refresh O(changed) before/after (#69)"
```

### Task 12: Docs riders

**Files:**
- Modify: `docs/ROADMAP.md` (Phase 6 section: strike through #69 in the Phase 0–5 style, with a one-line summary)
- Modify: `docs/superpowers/specs/2026-05-30-optimization-pass/README.md` (status table: `SP4 | Not started | — | —` → `Implemented`, spec `SP4-storage-aware-serving.md`, plan `../../plans/2026-06-01-sp4-storage-aware-serving.md`)

- [ ] **Step 1: Make both edits, then commit**

```bash
git add docs/ROADMAP.md docs/superpowers/specs/2026-05-30-optimization-pass/README.md
git commit -m "docs: mark #69 done; fix stale SP4 status row"
```

### Task 13: Validation gates + PR

- [ ] **Step 1: Format, lint, full tests**

```bash
cargo fmt --all --check
cargo clippy --all-targets
cargo test --workspace
```

Expected: all clean. (`cargo fmt --all --check` must exit 0 — CI gates on it.)

- [ ] **Step 2: FUSE e2e suite**

```bash
cargo test -p musefs-fuse -- --ignored
```

Expected: PASS (needs `/dev/fuse`; the byte-identical PCM-sha test is the cardinal-invariant gate).

- [ ] **Step 3: In-diff mutation gate (CI parity)**

All killing tests must be **committed** first, then:

```bash
cd /home/cfutro/git/musefs
git diff "$(git merge-base main HEAD)...HEAD" -- '*.rs' > mutants.diff
# Sanity: non-empty and matching the changed-file count, or the gate silently false-passes.
grep -c '^diff --git ' mutants.diff && grep -c '^@@ ' mutants.diff
TMPDIR=/home/cfutro/.cache/mutants-tmp cargo mutants --in-diff mutants.diff -j4 \
  --exclude 'musefs-latencyfs/**' --output mutants-out/in-diff
cat mutants-out/in-diff/mutants.out/missed.txt
rm -rf /home/cfutro/.cache/mutants-tmp mutants-out mutants.diff
```

Expected: `missed.txt` empty (0 missed; unviable is fine). Kill survivors with targeted tests, commit, regenerate `mutants.diff`, re-run. CI's gate (unpinned cargo-mutants) remains authoritative.

- [ ] **Step 4: Push and open the PR**

```bash
git push -u origin phase6-pr1-incremental-refresh
gh pr create --title "Phase 6 PR 1: changelog-driven O(changed) refresh (#69)" --body "$(cat <<'EOF'
Closes #69.

MIGRATION_V3 adds a trigger-maintained, self-pruning `track_changes` ring;
refresh reads only the rows past an in-memory watermark, renders only
changed/added tracks, mutates the render snapshot in place, and prunes
caches/notifies inodes from the exact ChangeSet. Ring gaps fall back to the
retained full path. user_version 2->3 mirrored into python-musefs + the
vendored Picard copy.

Bench: refresh-1 sweep flat across 100..20000 tracks (was linear) — see
BENCHMARKS.md "Phase 6 PR 1". Spec:
docs/superpowers/specs/2026-06-03-phase6-perf-sps-design.md.

🤖 Generated with [Claude Code](https://claude.com/claude-code)
EOF
)"
```

(The spec/plan docs commits ride this branch.) Wait for CI (`ci-ok` / `coverage-ok` aggregators + the mutants gate) before merging.
