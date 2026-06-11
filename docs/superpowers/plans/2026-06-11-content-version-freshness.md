# content_version Freshness Superset Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make `content_version` a true superset of every DB-knowable input that affects synthesized bytes (closing #271 art mutations and #272 scanner-geometry/structural changes), and make `getattr` stat the backing file on size-cache hits so it cannot advertise stale attrs after an on-disk change (#279).

**Architecture:** Three SQLite triggers added in a new V5 migration centralize freshness on `content_version` (art immutability + delete-bump; scanner-geometry bump; structural-block bump), so the caches' single-`i64` key stays valid and the read/synthesis hot path is untouched. The one thing the DB cannot observe — an on-disk backing change with no DB write — is caught by adding a stat to `getattr`'s size-cache hit, aligning it with the read/open paths that already stat.

**Tech Stack:** Rust workspace (`musefs-db` → `musefs-format` → `musefs-core`), SQLite via `rusqlite`, triggers in `musefs-db/src/schema.rs`, Python schema mirror under `contrib/python-musefs/`.

**Spec:** `docs/superpowers/specs/2026-06-11-content-version-freshness-design.md`

---

## File Structure

| File | Responsibility | Change |
| ---- | -------------- | ------ |
| `musefs-db/src/schema.rs` | Append-only migration list; trigger DDL | Add `MIGRATION_V5` (5 triggers), append to `MIGRATIONS`, add `migration_v5_tests`, bump 7 `user_version`-is-4 assertions to 5 |
| `musefs-db/tests/triggers.rs` | Integration tests for content_version triggers via the public `Db` API | Add #272 geometry + structural bump tests |
| `musefs-db/tests/tracks.rs` | Track upsert/get integration tests | Invert `rescan_does_not_reset_content_version` (now bumps on geometry change) |
| `musefs-core/src/mapping.rs` | `track_art_to_inputs` + its tests | Rework `track_art_to_inputs_errors_on_negative_byte_len` to plant the malformed row via INSERT (the V5 trigger blocks the old UPDATE) |
| `musefs-core/src/facade.rs` | `getattr`, `SizeEntry` | Add backing stamp to `SizeEntry`; stat + compare on size-cache hit; 2 tests |
| `contrib/python-musefs/src/musefs_common/schema.py` | Generated schema mirror | Regenerate (auto from `MIGRATIONS`) |
| `contrib/python-musefs/` Picard vendored copy | Vendored mirror | Re-vendor via `vendor_to_picard.py` |
| `ARCHITECTURE.md` | Store schema + freshness + external-writer contract docs | Document V5, art immutability, superset semantics |

**Why Task 1 is one atomic commit:** the pre-commit hook runs the full workspace test suite and rejects any red commit (per `CLAUDE.md`). Shipping the V5 triggers *breaks three existing tests at once* — the geometry trigger breaks `rescan_does_not_reset_content_version`, the art-immutability trigger breaks `track_art_to_inputs_errors_on_negative_byte_len`, and the new latest-version is 5 not 4 — and the `schema.py` freshness gate fails until regeneration. All of these must land together to keep the suite green. Task 1's steps are therefore: write/adjust every affected test, confirm the expected red, add the migration, regenerate the mirror, confirm green, commit.

---

## Task 1: V5 migration — content_version freshness superset (#271, #272)

**Files:**
- Modify: `musefs-db/src/schema.rs` (add `MIGRATION_V5`, append to `MIGRATIONS`, add `migration_v5_tests`, update 7 assertions)
- Modify: `musefs-db/tests/triggers.rs` (add #272 tests)
- Modify: `musefs-db/tests/tracks.rs` (invert rescan test)
- Modify: `musefs-core/src/mapping.rs` (rework malformed-row test)
- Regenerate: `contrib/python-musefs/src/musefs_common/schema.py` + Picard vendored copy

- [ ] **Step 1: Add the `migration_v5_tests` module to `musefs-db/src/schema.rs`**

Insert this module immediately after the existing `migration_v4_tests` module (it does not matter exactly where among the `#[cfg(test)]` modules, as long as it is inside the file's test region):

```rust
#[cfg(test)]
mod migration_v5_tests {
    use rusqlite::{params, Connection};

    /// A fresh, fully-migrated DB. NOTE: `super::migrate` runs on a bare
    /// connection, so `foreign_keys` defaults to OFF here — that is what lets
    /// `deleting_referenced_art_bumps_tracks` produce the orphan case.
    fn migrated() -> Connection {
        let mut conn = Connection::open_in_memory().unwrap();
        super::migrate(&mut conn).unwrap();
        conn
    }

    fn insert_track(conn: &Connection, path: &str) -> i64 {
        conn.execute(
            "INSERT INTO tracks (backing_path, format, audio_offset, audio_length, \
             backing_size, backing_mtime, updated_at) \
             VALUES (?1,'flac',0,1,1,0,0)",
            [path],
        )
        .unwrap();
        conn.last_insert_rowid()
    }

    fn insert_art(conn: &Connection, sha: &str, data: &[u8]) -> i64 {
        conn.execute(
            "INSERT INTO art (sha256, mime, width, height, byte_len, data) \
             VALUES (?1,'image/png',NULL,NULL,?2,?3)",
            params![sha, data.len() as i64, data],
        )
        .unwrap();
        conn.last_insert_rowid()
    }

    #[test]
    fn migration_reaches_user_version_5() {
        let conn = migrated();
        let uv: i64 = conn
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap();
        assert_eq!(uv, 5);
    }

    #[test]
    fn art_content_update_is_rejected() {
        let conn = migrated();
        let a = insert_art(&conn, &"a".repeat(64), &[1, 2, 3]);
        // Mutating any content column aborts the statement.
        assert!(conn
            .execute("UPDATE art SET mime='image/jpeg' WHERE id=?1", [a])
            .is_err());
        assert!(conn
            .execute("UPDATE art SET byte_len=99 WHERE id=?1", [a])
            .is_err());
        assert!(conn
            .execute("UPDATE art SET data=X'04050607' WHERE id=?1", [a])
            .is_err());
        assert!(conn
            .execute("UPDATE art SET width=10 WHERE id=?1", [a])
            .is_err());
        assert!(conn
            .execute(
                "UPDATE art SET sha256=?1 WHERE id=?2",
                params![&"b".repeat(64), a],
            )
            .is_err());
    }

    #[test]
    fn art_noop_update_is_allowed() {
        let conn = migrated();
        let a = insert_art(&conn, &"a".repeat(64), &[1, 2, 3]);
        // No content column changes value: the WHEN guard is false, so the
        // trigger body never runs and the UPDATE proceeds.
        conn.execute("UPDATE art SET mime=mime WHERE id=?1", [a])
            .unwrap();
    }

    #[test]
    fn deleting_referenced_art_bumps_tracks() {
        let conn = migrated();
        let t = insert_track(&conn, "/a.flac");
        let a = insert_art(&conn, &"a".repeat(64), &[1, 2, 3]);
        conn.execute(
            "INSERT INTO track_art (track_id, art_id, picture_type, ordinal) \
             VALUES (?1,?2,3,0)",
            [t, a],
        )
        .unwrap();
        // The track_art INSERT already bumped once; capture that baseline.
        let cv0: i64 = conn
            .query_row("SELECT content_version FROM tracks WHERE id=?1", [t], |r| {
                r.get(0)
            })
            .unwrap();
        conn.execute("DELETE FROM art WHERE id=?1", [a]).unwrap();
        let cv1: i64 = conn
            .query_row("SELECT content_version FROM tracks WHERE id=?1", [t], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(cv1, cv0 + 1, "art delete must bump the referencing track");
    }

    #[test]
    fn deleting_unreferenced_art_bumps_nothing() {
        let conn = migrated();
        let t = insert_track(&conn, "/a.flac");
        let a = insert_art(&conn, &"a".repeat(64), &[1, 2, 3]);
        let cv0: i64 = conn
            .query_row("SELECT content_version FROM tracks WHERE id=?1", [t], |r| {
                r.get(0)
            })
            .unwrap();
        // Orphan-GC style: the art row has no track_art references.
        conn.execute("DELETE FROM art WHERE id=?1", [a]).unwrap();
        let cv1: i64 = conn
            .query_row("SELECT content_version FROM tracks WHERE id=?1", [t], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(cv1, cv0, "deleting an unreferenced art row must not bump");
    }
}
```

- [ ] **Step 2: Add the #272 trigger tests to `musefs-db/tests/triggers.rs`**

Append these three tests to the existing file (which already has `mod common; use common::new_track; use musefs_db::{Db, Tag};` at the top). Add `StructuralBlock` to the `musefs_db` import line so it reads `use musefs_db::{Db, StructuralBlock, Tag};`:

```rust
#[test]
fn geometry_change_bumps_content_version_by_exactly_one() {
    let db = Db::open_in_memory().unwrap();
    // Tagless track: no replace_tags to mask the geometry change. The first
    // upsert is an INSERT (cv stays 0); the geometry trigger fires only on the
    // ON CONFLICT UPDATE below.
    let id = db.upsert_track(&new_track("/m/a.flac")).unwrap();
    assert_eq!(db.track_content_version(id).unwrap(), 0);

    let mut changed = new_track("/m/a.flac");
    changed.audio_offset = 222;
    db.upsert_track(&changed).unwrap();

    // Exactly 1 proves the WHEN guard halts the trigger's own nested UPDATE
    // (a recursing trigger would bump more than once or error).
    assert_eq!(
        db.track_content_version(id).unwrap(),
        1,
        "geometry change must bump content_version exactly once"
    );
}

#[test]
fn identical_rescan_does_not_bump_content_version() {
    let db = Db::open_in_memory().unwrap();
    let id = db.upsert_track(&new_track("/m/a.flac")).unwrap();
    // Re-upsert identical geometry: updated_at is rewritten but no geometry
    // column changes, so the WHEN guard is false and content_version holds.
    db.upsert_track(&new_track("/m/a.flac")).unwrap();
    assert_eq!(db.track_content_version(id).unwrap(), 0);
}

#[test]
fn structural_block_change_bumps_content_version() {
    let db = Db::open_in_memory().unwrap();
    let id = db.upsert_track(&new_track("/m/a.flac")).unwrap();
    let before = db.track_content_version(id).unwrap();
    db.set_structural_blocks(
        id,
        &[StructuralBlock {
            kind: "STREAMINFO".to_string(),
            ordinal: 0,
            body: vec![1, 2, 3, 4],
        }],
    )
    .unwrap();
    // set_structural_blocks is DELETE-then-INSERT, so the bump may exceed 1;
    // correctness only needs a strict increase (caches compare for equality).
    assert!(
        db.track_content_version(id).unwrap() > before,
        "structural block write must bump content_version"
    );
}
```

- [ ] **Step 3: Invert the rescan test in `musefs-db/tests/tracks.rs`**

Replace the existing `rescan_does_not_reset_content_version` (currently asserting the geometry rescan does NOT bump) with its inverse. Find the function at `musefs-db/tests/tracks.rs:56` and replace the whole function with:

```rust
#[test]
fn rescan_with_changed_geometry_bumps_content_version() {
    let db = Db::open_in_memory().unwrap();
    let id = db.upsert_track(&new_track("/music/a.flac")).unwrap();
    db.replace_tags(id, &[Tag::new("title", "T", 0)]).unwrap();
    let cv_before = db.track_content_version(id).unwrap();
    assert!(cv_before > 0);

    // Re-scan the same path with changed offsets. content_version is now a
    // superset of scanner-owned geometry, so this MUST bump (was pinned the
    // other way before the V5 geometry trigger — see issue #272).
    let mut rescan = new_track("/music/a.flac");
    rescan.audio_offset = 100;
    rescan.audio_length = 900;
    db.upsert_track(&rescan).unwrap();

    assert_eq!(
        db.track_content_version(id).unwrap(),
        cv_before + 1,
        "a geometry-changing rescan must bump content_version exactly once"
    );
}
```

Note: `new_track` defaults `audio_offset = 100`, so to actually change geometry the rescan must differ from the default — here `audio_length` changes from `1000` to `900` (and `audio_offset` stays `100`), so the geometry trigger fires once.

- [ ] **Step 4: Rework the malformed-row test in `musefs-core/src/mapping.rs`**

In `track_art_to_inputs_errors_on_negative_byte_len` (starts at `musefs-core/src/mapping.rs:281`), the test currently plants the bad row by `upsert_art` + `UPDATE art SET byte_len = -1`. The V5 `art_reject_content_update` trigger blocks that UPDATE. Replace the `bad` art creation, the `set_track_art` call, and the raw `UPDATE` block (everything from the `let bad = db.upsert_art(...)` line through the `assert!(super::track_art_to_inputs(&db, tid).is_err(), ...)` closing) with:

```rust
        // Plant a malformed art row directly. art rows are immutable once
        // written (the V5 `art_reject_content_update` trigger blocks UPDATEs of
        // content columns), and the V4 `byte_len = length(data)` CHECK rejects
        // byte_len = -1 on a normal connection — so INSERT the bad row on a raw
        // connection with CHECK enforcement off. The trigger guards only
        // UPDATE, so a fresh malformed INSERT (the realistic FK/CHECK-disabled
        // external write) still reaches the row-reader defensive path this test
        // pins.
        let raw = rusqlite::Connection::open(&path).unwrap();
        raw.pragma_update(None, "ignore_check_constraints", true)
            .unwrap();
        raw.execute(
            "INSERT INTO art (sha256, mime, width, height, byte_len, data) \
             VALUES (?1, 'image/png', NULL, NULL, -1, X'0909090909')",
            [&"9".repeat(64)],
        )
        .unwrap();
        raw.pragma_update(None, "ignore_check_constraints", false)
            .unwrap();
        let bad: i64 = raw
            .query_row("SELECT id FROM art WHERE sha256 = ?1", [&"9".repeat(64)], |r| {
                r.get(0)
            })
            .unwrap();
        drop(raw);

        db.set_track_art(
            tid,
            &[
                TrackArt {
                    art_id: good,
                    picture_type: 3,
                    description: String::new(),
                    ordinal: 0,
                },
                TrackArt {
                    art_id: bad,
                    picture_type: 3,
                    description: String::new(),
                    ordinal: 1,
                },
            ],
        )
        .unwrap();

        assert!(
            super::track_art_to_inputs(&db, tid).is_err(),
            "negative byte_len must error at row-read, not be skipped"
        );
```

Keep the test's existing header (the `let dir`, `let path`, `let db`, `let tid`, and the `good` art `upsert_art` with `data: vec![1, 2, 3, 4]`) unchanged. The `bad` row is now created by the raw INSERT above instead of `upsert_art`, and `set_track_art` now runs after the bad row exists.

- [ ] **Step 5: Run the full workspace suite to confirm the expected RED**

Run: `cargo test`
Expected: FAIL. Specifically:
- `migration_v5_tests::migration_reaches_user_version_5` — fails (`user_version` is 4).
- `migration_v5_tests::art_content_update_is_rejected` — fails (UPDATE succeeds, no trigger).
- `migration_v5_tests::deleting_referenced_art_bumps_tracks` — fails (no `art_ad`).
- `triggers::geometry_change_bumps_content_version_by_exactly_one` — fails (no geometry trigger).
- `triggers::structural_block_change_bumps_content_version` — fails (no structural trigger).
- `tracks::rescan_with_changed_geometry_bumps_content_version` — fails (no bump yet).
- The seven `assert_eq!(uv, 4)` assertions still pass at this point (we change them in Step 7).

This confirms the new tests fail for the right reason before implementing the migration.

- [ ] **Step 6: Add `MIGRATION_V5` and append it to `MIGRATIONS` in `musefs-db/src/schema.rs`**

Add this const immediately after the `MIGRATION_V4` string literal (which ends with `";` at `musefs-db/src/schema.rs:261`) and before `const MIGRATIONS`:

```rust
const MIGRATION_V5: &str = r"
-- art rows are content-addressed by sha256: once written, their content
-- columns are immutable. A writer needing different bytes/metadata inserts a
-- NEW row and relinks via track_art (which bumps content_version through the
-- V1 track_art triggers). This closes #271, where an in-place art edit changed
-- served bytes without bumping any referencing track. width/height use IS NOT
-- (NULL-safe) because they are nullable; the NOT NULL columns use <>.
CREATE TRIGGER art_reject_content_update
BEFORE UPDATE ON art
WHEN NEW.data   <> OLD.data
  OR NEW.sha256 <> OLD.sha256
  OR NEW.mime   <> OLD.mime
  OR NEW.byte_len <> OLD.byte_len
  OR NEW.width  IS NOT OLD.width
  OR NEW.height IS NOT OLD.height
BEGIN
    SELECT RAISE(ABORT, 'art rows are immutable; insert a new content-addressed row and relink via track_art');
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
-- changes, making content_version a true superset of served-byte inputs
-- (#272). The WHEN guard is false on this trigger's own nested UPDATE (only
-- content_version changes), so the recursion terminates after exactly one bump.
CREATE TRIGGER tracks_geometry_au
AFTER UPDATE ON tracks
WHEN NEW.format        <> OLD.format
  OR NEW.audio_offset  <> OLD.audio_offset
  OR NEW.audio_length  <> OLD.audio_length
  OR NEW.backing_size  <> OLD.backing_size
  OR NEW.backing_mtime <> OLD.backing_mtime
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

Then change the `MIGRATIONS` line (`musefs-db/src/schema.rs:263`) from:

```rust
const MIGRATIONS: &[&str] = &[MIGRATION_V1, MIGRATION_V2, MIGRATION_V3, MIGRATION_V4];
```

to:

```rust
const MIGRATIONS: &[&str] = &[
    MIGRATION_V1,
    MIGRATION_V2,
    MIGRATION_V3,
    MIGRATION_V4,
    MIGRATION_V5,
];
```

- [ ] **Step 7: Bump the seven `user_version == 4` assertions to 5**

The latest `user_version` is now 5. Update each of these assertions in `musefs-db/src/schema.rs` (all assert the post-migrate latest version) from `4` to `5`:
- `musefs-db/src/schema.rs:299` — `assert_eq!(uv, 4);` → `assert_eq!(uv, 5);`
- `musefs-db/src/schema.rs:336` — `assert_eq!(uv2, 4);` → `assert_eq!(uv2, 5);`
- `musefs-db/src/schema.rs:365` — `assert_eq!(uv, 4);` → `assert_eq!(uv, 5);`
- `musefs-db/src/schema.rs:417` — `assert_eq!(uv, 4);` → `assert_eq!(uv, 5);`
- `musefs-db/src/schema.rs:509` — `assert_eq!(uv, 4);` → `assert_eq!(uv, 5);`
- `musefs-db/src/schema.rs:658` — `assert_eq!(uv, 4);` → `assert_eq!(uv, 5);`
- `musefs-db/src/schema.rs:733` — `assert_eq!(uv, 4);` → `assert_eq!(uv, 5);`

(Line numbers will drift after inserting `MIGRATION_V5`; if so, search for `assert_eq!(uv, 4)` and `assert_eq!(uv2, 4)` and update every occurrence — each is a "migrate reaches latest version" assertion. Do NOT touch the `pragma_update(None, "user_version", 1i64/2i64/3i64)` lines, which deliberately set a starting version for partial-upgrade tests.)

- [ ] **Step 8: Regenerate the Python schema mirror and re-vendor**

The `schema_py_fixture_is_fresh` test compares the vendored `schema.py` to one rendered from `MIGRATIONS`; it is stale until regenerated. Run:

```bash
MUSEFS_REGEN_SCHEMA_PY=1 cargo test -p musefs-db schema_py
python contrib/python-musefs/vendor_to_picard.py
```

Expected: the first command rewrites `contrib/python-musefs/src/musefs_common/schema.py` (now containing the V5 triggers and `USER_VERSION = 5`); the second propagates it to the Picard vendored copy.

- [ ] **Step 9: Run the full workspace suite to confirm GREEN**

Run: `cargo test`
Expected: PASS, including all `migration_v5_tests`, the three new `triggers.rs` tests, the inverted `rescan_with_changed_geometry_bumps_content_version`, the reworked `track_art_to_inputs_errors_on_negative_byte_len`, and `schema_py_fixture_is_fresh`.

Also run the lint and format gates the pre-commit hook enforces:

Run: `cargo clippy --all-targets && cargo fmt --all --check`
Expected: no warnings, no formatting diff.

- [ ] **Step 10: Commit**

```bash
git add musefs-db/src/schema.rs musefs-db/tests/triggers.rs musefs-db/tests/tracks.rs \
        musefs-core/src/mapping.rs \
        contrib/python-musefs/src/musefs_common/schema.py
# Add any files vendor_to_picard.py modified (check `git status` for the Picard copy):
git add -p   # stage the vendored Picard schema mirror; do NOT blanket-add untracked files
git commit -m "$(cat <<'EOF'
feat(db): V5 triggers make content_version a freshness superset (#271, #272)

art rows are immutable (art_reject_content_update) and deleting a referenced
art row bumps referencing tracks (art_ad); scanner geometry changes bump via
tracks_geometry_au; structural-block writes bump via structural_blocks_ai/_ad.
This makes the content_version cache key a true superset of DB-knowable
served-byte inputs, so the caches' single-i64 key stays valid.

Reworks the mapping.rs malformed-row test to INSERT (the immutability trigger
blocks the old UPDATE) and inverts the rescan test to expect a bump.
Regenerates the Python schema mirror.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 2: getattr stats the backing file on size-cache hits (#279)

**Files:**
- Modify: `musefs-core/src/facade.rs:85-91` (`SizeEntry` struct)
- Modify: `musefs-core/src/facade.rs:923-976` (`getattr`)
- Test: `musefs-core/src/facade.rs` (`tests` module)

- [ ] **Step 1: Write the failing test**

Add this test to the `tests` module in `musefs-core/src/facade.rs` (the module already imports `CoreError`, `MountConfig`, `Mode`, `Musefs`, `VirtualTree`, `id3`, and uses `crate::scan::scan_directory`, as in `open_handle_reresolves_after_content_version_bump`):

```rust
#[test]
fn getattr_size_cache_hit_detects_backing_change() {
    use crate::scan::scan_directory;
    use id3::TagLike;
    use std::collections::BTreeMap;

    let dir = tempfile::tempdir().unwrap();
    let backing = dir.path().join("a.mp3");
    {
        let mut tag = id3::Tag::new();
        tag.set_artist("Pix");
        tag.set_title("Song");
        let mut bytes = Vec::new();
        tag.write_to(&mut bytes, id3::Version::Id3v24).unwrap();
        bytes.extend_from_slice(&[0xFF, 0xFB, 1, 2, 3, 4]);
        std::fs::write(&backing, &bytes).unwrap();
    }

    let db_path = dir.path().join("m.db");
    {
        let db = musefs_db::Db::open(&db_path).unwrap();
        scan_directory(&db, dir.path()).unwrap();
    }
    let cfg = MountConfig {
        template: "$artist/$title".to_string(),
        fallbacks: BTreeMap::new(),
        default_fallback: "Unknown".to_string(),
        mode: Mode::Synthesis,
        poll_interval: std::time::Duration::ZERO,
        case_insensitive: false,
    };
    let fs = Musefs::open(musefs_db::Db::open(&db_path).unwrap(), cfg).unwrap();

    let artist = fs.lookup(VirtualTree::ROOT, "Pix").expect("artist dir");
    let (_, file_inode, _) = fs.readdir(artist).unwrap().into_iter().next().unwrap();

    // First getattr populates size_cache (miss path: full resolve).
    let attr1 = fs.getattr(file_inode).unwrap();
    assert!(attr1.size > 0, "baseline attr must be non-empty");

    // Second getattr with the file unchanged is a clean cache hit.
    let attr2 = fs.getattr(file_inode).unwrap();
    assert_eq!(attr1.size, attr2.size, "unchanged backing must stay a hit");

    // Change the backing file out-of-band, without any DB write — so
    // content_version is unchanged and the size_cache would otherwise hit.
    {
        use std::io::Write as _;
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&backing)
            .unwrap();
        f.write_all(&[0u8; 64]).unwrap();
    }

    // getattr must now refuse to advertise stale attrs.
    assert!(
        matches!(fs.getattr(file_inode), Err(CoreError::BackingChanged(_))),
        "getattr must degrade to BackingChanged after an on-disk backing change"
    );
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p musefs-core getattr_size_cache_hit_detects_backing_change`
Expected: FAIL — the final assertion fails because the current hit path returns stale attrs (`Ok`) instead of `Err(BackingChanged)`.

- [ ] **Step 3: Add the backing stamp to `SizeEntry`**

Replace the `SizeEntry` struct at `musefs-core/src/facade.rs:85-91`:

```rust
/// A cached file size/attr entry: validated at `content_version`.
#[derive(Clone, Copy)]
struct SizeEntry {
    content_version: i64,
    total_len: u64,
    mtime_secs: i64,
}
```

with:

```rust
/// A cached file size/attr entry: validated at `content_version`, plus the
/// backing-file stamp it was built from so `getattr` can re-stat on a hit and
/// catch an on-disk backing change that left `content_version` untouched (#279).
#[derive(Clone, Copy)]
struct SizeEntry {
    content_version: i64,
    total_len: u64,
    mtime_secs: i64,
    backing_size: u64,
    backing_mtime_secs: i64,
}
```

- [ ] **Step 4: Stat on the hit path and populate the stamp on the miss path in `getattr`**

In `getattr` (`musefs-core/src/facade.rs`), replace the cache-hit block:

```rust
            if let Some(e) = self.size_cache.get(&track_id).map(|e| *e)
                && e.content_version == track.content_version
            {
                // Hit: no backing stat, no synthesis. NOTE: a backing file
                // changed in place without a rescan would leave mtime/size
                // stale until the next scan bumps content_version — acceptable
                // for a read-only mount (reads still validate at open()).
                return Ok((e.total_len, e.mtime_secs));
            }
```

with:

```rust
            if let Some(e) = self.size_cache.get(&track_id).map(|e| *e)
                && e.content_version == track.content_version
            {
                // Hit: re-stat the backing file (no synthesis) and compare to
                // the stamp the cached attrs were built from. An on-disk change
                // that left content_version untouched would otherwise let
                // getattr advertise stale attrs — the one metadata surface that
                // could outrun a backing change (read/open already re-stat).
                crate::metrics::on_stat();
                let meta = std::fs::metadata(&track.backing_path)?;
                if meta.len() != e.backing_size || mtime_secs(&meta) != e.backing_mtime_secs {
                    return Err(CoreError::BackingChanged(track.backing_path.clone()));
                }
                return Ok((e.total_len, e.mtime_secs));
            }
```

Then replace the miss-path insert:

```rust
            self.size_cache.insert(
                track_id,
                SizeEntry {
                    content_version: track.content_version,
                    total_len: resolved.total_len,
                    mtime_secs: resolved.mtime_secs,
                },
            );
```

with:

```rust
            self.size_cache.insert(
                track_id,
                SizeEntry {
                    content_version: track.content_version,
                    total_len: resolved.total_len,
                    mtime_secs: resolved.mtime_secs,
                    backing_size: resolved.backing_size,
                    backing_mtime_secs: resolved.backing_mtime_secs,
                },
            );
```

(`resolved` is an `Arc<ResolvedFile>`; `ResolvedFile` already exposes `backing_size: u64` and `backing_mtime_secs: i64` — see `musefs-core/src/reader.rs:17-40`. `mtime_secs` is the module-level helper at `musefs-core/src/facade.rs:104`.)

- [ ] **Step 5: Run the test to verify it passes**

Run: `cargo test -p musefs-core getattr_size_cache_hit_detects_backing_change`
Expected: PASS.

- [ ] **Step 6: Run the broader facade/getattr tests and lint gates**

Run: `cargo test -p musefs-core facade`
Expected: PASS (no regressions in existing getattr/size_cache tests).

Run: `cargo clippy --all-targets && cargo fmt --all --check`
Expected: no warnings, no formatting diff.

- [ ] **Step 7: Commit**

```bash
git add musefs-core/src/facade.rs
git commit -m "$(cat <<'EOF'
feat(core): getattr re-stats backing on size-cache hits (#279)

SizeEntry now carries the backing size/mtime stamp; a size-cache hit re-stats
the backing file and degrades to BackingChanged on drift instead of advertising
stale attrs, aligning getattr with the read/open paths. Catches an on-disk
backing change that left content_version untouched.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 3: Documentation — external-writer contract and freshness semantics

**Files:**
- Modify: `ARCHITECTURE.md` (store-schema section ~135-140, external-writer contract ~142-170, freshness section ~212-225)
- Modify: `contrib/python-musefs/` contract docs (the human-readable contract notes, not the generated `schema.py`)

- [ ] **Step 1: Add the V5 entry to the store-schema list in `ARCHITECTURE.md`**

After the `- **V4** — ...` bullet (`ARCHITECTURE.md:135-140`), add:

```markdown
- **V5** — freshness-superset triggers that make `content_version` cover every
  DB-knowable input to synthesized bytes, not only tag/`track_art` edits:
  `art_reject_content_update` (rejects in-place mutation of an `art` row's
  content columns — art is content-addressed, so a changed image is a new row),
  `art_ad` (bumps every track referencing a deleted `art` row, so an orphaned
  reference rebuilds to a clean serve-time error rather than streaming stale
  bytes), `tracks_geometry_au` (bumps when scanner-owned geometry — `format`,
  audio bounds, backing size/mtime — changes), and `structural_blocks_ai`/`_ad`
  (bump when FLAC structural blocks change).
```

- [ ] **Step 2: Update the external-writer contract in `ARCHITECTURE.md`**

In the "Ownership" / "What the store enforces" region (`ARCHITECTURE.md:142-170`), add a paragraph documenting art immutability. Insert after the "What the store enforces" list (after the `- a picture_type outside 0..=20.` bullet at `ARCHITECTURE.md:159`):

```markdown
**Art is immutable once written.** `art` rows are content-addressed by
`sha256`; as of V5 a trigger rejects any in-place `UPDATE` of an art row's
content columns (`data`, `sha256`, `mime`, `byte_len`, `width`, `height`) with
`RAISE(ABORT)` — a multi-row `UPDATE art` touching any content column aborts the
whole statement. To change a track's art, insert a new content-addressed row
and relink it via `track_art` (which bumps `content_version`); do not mutate an
existing row. Deleting an `art` row still referenced by `track_art` (possible
only with `foreign_keys` OFF) bumps every referencing track so the mount serves
a clean `EIO` on the now-orphaned reference instead of stale bytes.
```

- [ ] **Step 3: Update the freshness section in `ARCHITECTURE.md`**

In "Freshness: two version counters" (`ARCHITECTURE.md:212-225`), update the `content_version` description so it no longer says the triggers bump *only* on tag/art edits. Replace the sentence "The DB triggers increment it on any tag/art edit." (`ARCHITECTURE.md:217`) with:

```markdown
The DB triggers increment it on any input the database can see that changes
synthesized bytes: tag and `track_art` edits, `art`-row deletes that orphan a
reference, scanner-owned geometry changes (`format`, audio bounds, backing
size/mtime), and FLAC structural-block changes (V5). It is therefore a
superset key — the one input it cannot cover is an on-disk backing change with
no DB write, which `resolve` (and, since #279, a size-cache `getattr` hit)
catches by re-statting the backing file and degrading to `BackingChanged`.
```

- [ ] **Step 4: Mirror the art-immutability rule in the contrib contract docs**

Locate the human-readable external-writer contract notes under `contrib/python-musefs/` (search: `grep -rl "track_art" contrib/python-musefs --include=*.md --include=*.py`; the contract is documented alongside the shared library, not in the generated `schema.py`). Add a note that art rows are immutable: writers must insert a new content-addressed row and relink via `track_art` rather than updating an existing `art` row. Match the surrounding doc style; keep it to two or three sentences.

If no human-readable contract doc exists under `contrib/python-musefs/` (only the generated `schema.py` and code), skip this step — the `ARCHITECTURE.md` external-writer contract is the canonical statement — and note in the commit message that the contrib mirror had no prose contract to update.

- [ ] **Step 5: Commit**

```bash
git add ARCHITECTURE.md
# Add the contrib contract doc only if Step 4 modified one:
git add -p   # stage any contrib doc change from Step 4
git commit -m "$(cat <<'EOF'
docs: document V5 freshness-superset triggers and art immutability

ARCHITECTURE.md: add the V5 schema entry, document that art rows are immutable
(insert-and-relink, not in-place UPDATE), and update the freshness section to
describe content_version as a superset key with the on-disk backing change as
the one stat-caught exception (#271, #272, #279).

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Self-Review

**Spec coverage:**
- #271 art immutability → Task 1 `art_reject_content_update` + `art_content_update_is_rejected`/`art_noop_update_is_allowed` tests. ✓
- #271 art delete-bump → Task 1 `art_ad` + `deleting_referenced_art_bumps_tracks`/`deleting_unreferenced_art_bumps_nothing` tests. ✓
- #272 geometry bump → Task 1 `tracks_geometry_au` + `geometry_change_bumps_content_version_by_exactly_one`/`identical_rescan_does_not_bump_content_version` + inverted `tracks.rs` test. ✓
- #272 structural blocks → Task 1 `structural_blocks_ai`/`_ad` + `structural_block_change_bumps_content_version`. ✓
- #272 alternative (validate-at-hit) → explicitly rejected in spec; not implemented (correct). ✓
- #279 getattr stat → Task 2 `SizeEntry` stamp + `getattr` stat + `getattr_size_cache_hit_detects_backing_change` (plus the preserved cache-hit assertion `attr1.size == attr2.size`). ✓
- Migration V5 append-only → Task 1 Steps 6-7. ✓
- Python schema mirror regen + vendor → Task 1 Step 8. ✓
- mapping.rs malformed-row test rework (the spec's flagged blocker) → Task 1 Step 4. ✓
- Changelog double-pump → no over-tight changelog-row-count assertion is written (the new tests assert `content_version`, not `track_changes` counts), consistent with the spec's warning. ✓
- Docs (ARCHITECTURE + contrib) → Task 3. ✓
- Fuzz crate → unaffected (no format-layer signature change); no step needed, matching the spec. ✓

**Placeholder scan:** No TBD/TODO; every code and SQL step is complete. Step 4 of Task 3 has a conditional ("if no contract doc exists, skip") with an explicit fallback, not a placeholder.

**Type/name consistency:** `SizeEntry` fields `backing_size`/`backing_mtime_secs` match `ResolvedFile`'s field names (`reader.rs:17-40`) and the `mtime_secs` helper (`facade.rs:104`). Trigger names (`art_reject_content_update`, `art_ad`, `tracks_geometry_au`, `structural_blocks_ai`, `structural_blocks_ad`) are used consistently across the migration SQL, tests, and docs. `StructuralBlock { kind, ordinal, body }` matches `models.rs:229`. The inverted test name `rescan_with_changed_geometry_bumps_content_version` matches the spec's suggested rename.

**Scope:** One cohesive freshness-correctness change across three tasks; not decomposable further without splitting an atomic schema commit.
