# Backing-File Freshness Strengthening — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the `(size, whole-second mtime)` backing-file freshness stamp with a `(size, nanosecond mtime, ctime)` stamp collected from the same descriptor the scanner probes (with a stat sandwich), so a same-size in-place rewrite — busy or adversarial — can no longer silently pair stale metadata with fresh audio bytes.

**Architecture:** A new `BackingStamp` value type in `musefs-core` captures the three integers from one `fstat` and is compared at all three serve-path sites (`resolve`, `validate_opened_backing`, the `getattr` size-cache) and in the scanner. The `tracks.backing_mtime` column is renamed to `backing_mtime_ns` (nanoseconds) and a `backing_ctime_ns` column is added, edited directly into `MIGRATION_V5` since there are no extant databases. The scanner's `probe_file` opens once, fstats before and after the probe, and drops any file that moved mid-probe.

**Tech Stack:** Rust workspace (`musefs-db` → `musefs-format` → `musefs-core`), SQLite via `rusqlite`, `std::os::unix::fs::MetadataExt` for `mtime`/`ctime` nanoseconds, two generated Python schema mirrors under `contrib/`.

**Spec:** `docs/superpowers/specs/2026-06-12-backing-freshness-design.md`

---

## File Structure

| File | Responsibility | Action |
| ---- | -------------- | ------ |
| `musefs-core/src/freshness.rs` | The `BackingStamp` type: capture from `Metadata`, from a `Track`, compare, display-seconds | **Create** |
| `musefs-core/src/lib.rs` | Module wiring (`mod freshness;`) | Modify |
| `musefs-db/src/schema.rs` | `MIGRATION_V5`: rename `backing_mtime`→`backing_mtime_ns`, add `backing_ctime_ns`, retarget geometry trigger | Modify |
| `musefs-db/src/models.rs` | `Track`/`NewTrack` stamp fields | Modify |
| `musefs-db/src/tracks.rs` | `track_select!`, `row_to_track`, `upsert_track` | Modify |
| `musefs-db/src/bulk.rs` | `BulkWriter::upsert_track` | Modify |
| `musefs-core/src/reader.rs` | `ResolvedFile` stamp, `resolve` compare, displayed-mtime ns→s, drop dup `mtime_secs` | Modify |
| `musefs-core/src/facade.rs` | `validate_opened_backing`, `SizeEntry`, `getattr` size-cache compare, drop dup `mtime_secs` | Modify |
| `musefs-core/src/scan.rs` | `probe_file` fstat sandwich + `ProbeOutcome`, `Unit`/`ingest`/`ingest_bulk` stamp, `raced` counter, revalidate skip, drop dup `mtime_secs` | Modify |
| `contrib/python-musefs/src/musefs_common/schema.py` | Generated mirror | Regenerate |
| `contrib/picard/musefs/_common/schema.py` | Vendored mirror | Re-vendor |
| `ARCHITECTURE.md` | Freshness + contract prose | Modify |

**Why this order:** Task 1 (`BackingStamp`) is additive and compiles against the current schema. Task 2 is the **atomic rename slice** — the column rename breaks the whole workspace at once, and the pre-commit hook runs the full workspace suite, so db + core + all test fixtures must land in one green commit. Tasks 3–4 layer the scanner sandwich and revalidate strengthening on top. Task 5 is docs.

---

## Task 1: `BackingStamp` value type

**Files:**
- Create: `musefs-core/src/freshness.rs`
- Modify: `musefs-core/src/lib.rs:4` (insert `mod freshness;` between `mod facade;` and `mod lock;`)

- [ ] **Step 1: Write the failing test**

Append to the new file `musefs-core/src/freshness.rs` (the `BackingStamp` body in Step 3 goes above this `mod tests`):

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::MetadataExt;

    #[test]
    fn from_metadata_captures_ns_and_display_secs() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("f");
        std::fs::write(&p, b"hello").unwrap();
        let meta = std::fs::metadata(&p).unwrap();

        let s = BackingStamp::from_metadata(&meta);
        assert_eq!(s.size, 5);
        assert_eq!(s.mtime_ns, meta.mtime() * 1_000_000_000 + meta.mtime_nsec());
        assert_eq!(s.ctime_ns, meta.ctime() * 1_000_000_000 + meta.ctime_nsec());
        // Display is whole-second mtime, never the raw nanosecond value.
        assert_eq!(s.display_secs(), meta.mtime());
    }

    #[test]
    fn equality_is_field_wise() {
        let a = BackingStamp { size: 1, mtime_ns: 2, ctime_ns: 3 };
        assert_eq!(a, BackingStamp { size: 1, mtime_ns: 2, ctime_ns: 3 });
        assert_ne!(a, BackingStamp { size: 1, mtime_ns: 2, ctime_ns: 4 });
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p musefs-core --lib freshness 2>&1 | tail -20`
Expected: FAIL — `cannot find type BackingStamp` / module `freshness` not declared.

- [ ] **Step 3: Write minimal implementation**

At the **top** of `musefs-core/src/freshness.rs`:

```rust
//! The backing-file freshness stamp: the identity a `tracks` row records for
//! its backing file, compared on every serve to detect an on-disk change that
//! no database write covers. Strengthened past size + whole-second mtime to
//! nanosecond mtime + ctime (#276) so a same-size in-place rewrite — including
//! an adversarial one that resets mtime — cannot evade the guard.
use std::os::unix::fs::MetadataExt;

const NANOS_PER_SEC: i64 = 1_000_000_000;

/// `(size, mtime_ns, ctime_ns)` captured from one `fstat`. `mtime_ns`/`ctime_ns`
/// are nanoseconds since the Unix epoch (good until ~2262). `ctime` is the
/// adversarial backstop: a writer can reset mtime with `utimensat`, but ctime
/// is bumped by any write and cannot be set backward.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct BackingStamp {
    pub size: u64,
    pub mtime_ns: i64,
    pub ctime_ns: i64,
}

impl BackingStamp {
    pub fn from_metadata(meta: &std::fs::Metadata) -> BackingStamp {
        BackingStamp {
            size: meta.len(),
            mtime_ns: meta.mtime().saturating_mul(NANOS_PER_SEC).saturating_add(meta.mtime_nsec()),
            ctime_ns: meta.ctime().saturating_mul(NANOS_PER_SEC).saturating_add(meta.ctime_nsec()),
        }
    }

    pub fn from_track(t: &musefs_db::Track) -> BackingStamp {
        BackingStamp {
            size: t.backing_size,
            mtime_ns: t.backing_mtime_ns,
            ctime_ns: t.backing_ctime_ns,
        }
    }

    /// Whole-second mtime for the FUSE `getattr` display surface (never the raw
    /// nanosecond value, which would advertise a ~10^18-second timestamp).
    pub fn display_secs(&self) -> i64 {
        self.mtime_ns / NANOS_PER_SEC
    }
}
```

> **NOTE:** `from_track` references `Track::backing_mtime_ns` / `backing_ctime_ns`, which do **not exist yet** (added in Task 2). To keep Task 1 independently compilable, **omit `from_track` in this task** and add it in Task 2, Step 9. Ship Task 1 with only `from_metadata`, `display_secs`, and the type.

Then add the module declaration in `musefs-core/src/lib.rs` (alphabetical position, after `mod facade;`):

```rust
mod freshness;
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p musefs-core --lib freshness 2>&1 | tail -20`
Expected: PASS (2 tests). A `dead_code` warning on `from_metadata`/`display_secs` is expected until Task 2 wires them in; the crate uses `-D warnings` only in CI clippy, not in `cargo test`, so this is fine for now. To avoid the warning entirely, you may add `#[allow(dead_code)]` on the `impl` block and remove it in Task 2.

- [ ] **Step 5: Commit**

```bash
git add musefs-core/src/freshness.rs musefs-core/src/lib.rs
git commit -m "feat(core): add BackingStamp freshness value type (#276)"
```

---

## Task 2: Rename to nanosecond stamp + strong serve-path comparison (atomic slice)

This is one commit: the column rename breaks the whole workspace and the pre-commit hook runs the full suite. Write the new behavioral tests first (they will fail to compile until the slice lands), then make every edit, then green.

**Files:**
- Modify: `musefs-db/src/schema.rs` (`MIGRATION_V5`, line ~285; geometry trigger ~327; test SQL)
- Modify: `musefs-db/src/models.rs:124` (`Track`), `:137` (`NewTrack`)
- Modify: `musefs-db/src/tracks.rs:9` (`track_select!`), `:32` (`row_to_track`), `:187` (`upsert_track`)
- Modify: `musefs-db/src/bulk.rs:38` (`upsert_track`)
- Modify: `musefs-core/src/freshness.rs` (add `from_track`)
- Modify: `musefs-core/src/reader.rs` (`ResolvedFile` 17-40, `resolve` 110-131, `build` tail 145-320, drop `mtime_secs` 73)
- Modify: `musefs-core/src/facade.rs` (`SizeEntry` 85-95, `mtime_secs` 107, `validate_opened_backing` 114, `getattr` 945-981)
- Modify: `musefs-core/src/scan.rs` (`Unit` 480, `ingest` 504, `ingest_bulk` 581, worker 720-738, oracle 861-863, drop `mtime_secs` 52)
- Test: `musefs-core/tests/backing_changed_fault.rs`, `musefs-core/tests/facade.rs`
- Regenerate: both `contrib/.../schema.py`

- [ ] **Step 1: Write the failing behavioral tests**

Append to `musefs-core/tests/backing_changed_fault.rs`:

```rust
use std::os::unix::fs::MetadataExt;

// A same-size in-place rewrite within the same whole second (the common case in
// a fast test) changed only the sub-second mtime + ctime. The old whole-second
// guard would have passed it; the ns stamp must reject it.
#[test]
fn same_size_subsecond_rewrite_yields_backing_changed() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("a.flac");
    let (audio_offset, audio_length) = common::write_flac(&src, &["TITLE=T"], &[0xAB; 4096]);
    let db = Db::open_in_memory().unwrap();
    let meta = std::fs::metadata(&src).unwrap();
    let id = db
        .upsert_track(&musefs_db::NewTrack {
            backing_path: src.to_string_lossy().into_owned(),
            format: musefs_db::Format::Flac,
            audio_offset,
            audio_length,
            backing_size: meta.len(),
            backing_mtime_ns: meta.mtime() * 1_000_000_000 + meta.mtime_nsec(),
            backing_ctime_ns: meta.ctime() * 1_000_000_000 + meta.ctime_nsec(),
        })
        .unwrap();
    db.replace_tags(id, &[musefs_db::Tag::new("title", "T", 0)]).unwrap();
    HeaderCache::new(Mode::Synthesis).resolve(&db, id).unwrap();

    // Rewrite the same number of bytes in place: size identical, mtime/ctime move.
    std::fs::write(&src, {
        let mut v = std::fs::read(&src).unwrap();
        v[audio_offset as usize] ^= 0xFF;
        v
    })
    .unwrap();

    let err = HeaderCache::new(Mode::Synthesis).resolve(&db, id).unwrap_err();
    assert!(matches!(err, CoreError::BackingChanged(_)), "got {err:?}");
}

// Adversary rewrites in place, then forges mtime back to the stored value.
// mtime_ns now matches; only ctime (un-forgeable) caught the change.
#[test]
fn forged_mtime_is_caught_by_ctime() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("a.flac");
    let (audio_offset, audio_length) = common::write_flac(&src, &["TITLE=T"], &[0xAB; 4096]);
    let db = Db::open_in_memory().unwrap();
    let meta = std::fs::metadata(&src).unwrap();
    let original_modified = meta.modified().unwrap();
    let id = db
        .upsert_track(&musefs_db::NewTrack {
            backing_path: src.to_string_lossy().into_owned(),
            format: musefs_db::Format::Flac,
            audio_offset,
            audio_length,
            backing_size: meta.len(),
            backing_mtime_ns: meta.mtime() * 1_000_000_000 + meta.mtime_nsec(),
            backing_ctime_ns: meta.ctime() * 1_000_000_000 + meta.ctime_nsec(),
        })
        .unwrap();
    db.replace_tags(id, &[musefs_db::Tag::new("title", "T", 0)]).unwrap();
    HeaderCache::new(Mode::Synthesis).resolve(&db, id).unwrap();

    // In-place same-size rewrite, then reset mtime back to the scanned instant.
    let mut v = std::fs::read(&src).unwrap();
    v[audio_offset as usize] ^= 0xFF;
    std::fs::write(&src, v).unwrap();
    let f = std::fs::OpenOptions::new().write(true).open(&src).unwrap();
    f.set_times(std::fs::FileTimes::new().set_modified(original_modified)).unwrap();
    drop(f);

    // mtime_ns now equals the stored stamp; ctime advanced and must trip the guard.
    let err = HeaderCache::new(Mode::Synthesis).resolve(&db, id).unwrap_err();
    assert!(matches!(err, CoreError::BackingChanged(_)), "got {err:?}");
}

// The synthesized file's displayed mtime stays a plausible whole-second value
// (≈ the backing file's mtime), not the renamed column's raw nanoseconds.
#[test]
fn displayed_mtime_is_whole_seconds() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("a.flac");
    let (audio_offset, audio_length) = common::write_flac(&src, &["TITLE=T"], &[0xAB; 4096]);
    let db = Db::open_in_memory().unwrap();
    let meta = std::fs::metadata(&src).unwrap();
    let id = db
        .upsert_track(&musefs_db::NewTrack {
            backing_path: src.to_string_lossy().into_owned(),
            format: musefs_db::Format::Flac,
            audio_offset,
            audio_length,
            backing_size: meta.len(),
            backing_mtime_ns: meta.mtime() * 1_000_000_000 + meta.mtime_nsec(),
            backing_ctime_ns: meta.ctime() * 1_000_000_000 + meta.ctime_nsec(),
        })
        .unwrap();
    db.replace_tags(id, &[musefs_db::Tag::new("title", "T", 0)]).unwrap();
    let resolved = HeaderCache::new(Mode::Synthesis).resolve(&db, id).unwrap();
    // Plausible epoch-seconds (this millennium), never ~10^18.
    assert!(resolved.mtime_secs >= meta.mtime() && resolved.mtime_secs < 32_503_680_000);
}
```

Append to `musefs-core/tests/facade.rs` (uses the existing `config()` helper and the already-imported `scan_directory`, `Musefs`, `VirtualTree`, `CoreError`):

```rust
// Template-agnostic: walk readdir from the root to the first non-dir entry.
fn first_file_inode(fs: &Musefs) -> u64 {
    fn walk(fs: &Musefs, inode: u64) -> Option<u64> {
        for (name, child, is_dir) in fs.readdir(inode).unwrap() {
            if name == "." || name == ".." { continue; }
            if is_dir {
                if let Some(f) = walk(fs, child) { return Some(f); }
            } else {
                return Some(child);
            }
        }
        None
    }
    walk(fs, VirtualTree::ROOT).expect("a file inode under root")
}

// getattr's warm size-cache hit must re-stat with the full stamp (#276/#279):
// a same-size sub-second rewrite after the first getattr must surface
// BackingChanged, not stale attrs.
#[test]
fn getattr_size_cache_rejects_subsecond_rewrite() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("a.flac");
    common::write_flac(&src, &["TITLE=T", "ARTIST=A"], &[0xAB; 4096]);
    let db = musefs_db::Db::open_in_memory().unwrap();
    scan_directory(&db, dir.path()).unwrap();
    let fs = Musefs::open(db, config()).unwrap();

    let inode = first_file_inode(&fs);
    fs.getattr(inode).unwrap(); // warm the size cache

    let mut v = std::fs::read(&src).unwrap();
    *v.last_mut().unwrap() ^= 0xFF; // same size, new mtime/ctime
    std::fs::write(&src, v).unwrap();

    let err = fs.getattr(inode).unwrap_err();
    assert!(matches!(err, CoreError::BackingChanged(_)), "got {err:?}");
}
```

> The recursive `walk` skips `.`/`..` defensively; the facade `readdir` returns real entries (the existing test at `facade.rs:44-46` consumes them directly), so the guard is belt-and-suspenders.

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p musefs-core --test backing_changed_fault --test facade 2>&1 | tail -20`
Expected: FAIL to compile — `NewTrack` has no field `backing_mtime_ns` / `backing_ctime_ns`. (This is the expected TDD red.)

- [ ] **Step 3: Edit the schema (`musefs-db/src/schema.rs`)**

At the very top of the `MIGRATION_V5` raw string (immediately after `const MIGRATION_V5: &str = r"`), insert:

```sql
-- #276: strengthen the freshness stamp. No extant databases, so edit V5 in
-- place rather than adding a migration (user_version stays 5). RENAME COLUMN
-- auto-rewrites the CHECK (backing_mtime >= 0) and is picked up by the geometry
-- trigger recreated below. The column now holds nanoseconds since the epoch.
ALTER TABLE tracks RENAME COLUMN backing_mtime TO backing_mtime_ns;
ALTER TABLE tracks ADD COLUMN backing_ctime_ns INTEGER NOT NULL DEFAULT 0 CHECK (backing_ctime_ns >= 0);
```

In the same migration, change the `tracks_geometry_au` trigger's `WHEN` clause line from:

```sql
  OR NEW.backing_mtime <> OLD.backing_mtime
```
to:
```sql
  OR NEW.backing_mtime_ns <> OLD.backing_mtime_ns
```

Do **not** add `backing_ctime_ns` to the trigger (spec: pure freshness identity, not a served-byte input).

Then fix the in-file test SQL: every `INSERT INTO tracks (... backing_mtime ...)` and any `UPDATE tracks SET backing_mtime ...` in `schema.rs`'s `#[cfg(test)]` modules must use `backing_mtime_ns` (grep `backing_mtime` within `schema.rs`; the `backing_ctime_ns` default of 0 lets you omit it from INSERT column lists). The densest cluster is the `migration_v4_tests` module (raw `INSERT INTO tracks` strings around `schema.rs:462, 507, 561, 801, 937-1046`), including `v4_tracks_rejects_negative_backing_mtime` — these are **runtime** failures (the compiler does not check SQL strings), so sweep the whole module rather than relying on build errors.

- [ ] **Step 4: Rename the model fields (`musefs-db/src/models.rs`)**

`Track` (line 124): replace `pub backing_mtime: i64,` with:
```rust
    pub backing_mtime_ns: i64,
    pub backing_ctime_ns: i64,
```
`NewTrack` (line 137): replace `pub backing_mtime: i64,` with:
```rust
    pub backing_mtime_ns: i64,
    pub backing_ctime_ns: i64,
```

- [ ] **Step 5: Update reads/writes (`musefs-db/src/tracks.rs`)**

`track_select!` column list (line 12-13): change to
```rust
            "SELECT id, backing_path, format, audio_offset, audio_length, \
             backing_size, backing_mtime_ns, backing_ctime_ns, content_version, updated_at \
             FROM tracks ",
```

`row_to_track` (line 45-54): replace `backing_mtime: r.get("backing_mtime")?,` with
```rust
        backing_mtime_ns: r.get("backing_mtime_ns")?,
        backing_ctime_ns: r.get("backing_ctime_ns")?,
```

`upsert_track` (line 187): add the two columns to the INSERT list, `VALUES`, `ON CONFLICT DO UPDATE SET`, and `params!`:
```rust
        self.conn.execute(
            "INSERT INTO tracks
                (backing_path, format, audio_offset, audio_length, backing_size,
                 backing_mtime_ns, backing_ctime_ns, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, CAST(strftime('%s','now') AS INTEGER))
             ON CONFLICT(backing_path) DO UPDATE SET
                format          = excluded.format,
                audio_offset    = excluded.audio_offset,
                audio_length    = excluded.audio_length,
                backing_size    = excluded.backing_size,
                backing_mtime_ns = excluded.backing_mtime_ns,
                backing_ctime_ns = excluded.backing_ctime_ns,
                updated_at      = CAST(strftime('%s','now') AS INTEGER)",
            params![
                t.backing_path,
                t.format.as_str(),
                t.audio_offset,
                t.audio_length,
                t.backing_size,
                t.backing_mtime_ns,
                t.backing_ctime_ns,
            ],
        )?;
```

- [ ] **Step 6: Update the bulk writer (`musefs-db/src/bulk.rs:38`)**

```rust
    pub fn upsert_track(&mut self, t: &NewTrack) -> Result<i64> {
        self.tx.execute(
            "INSERT INTO tracks
                (backing_path, format, audio_offset, audio_length, backing_size,
                 backing_mtime_ns, backing_ctime_ns, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, CAST(strftime('%s','now') AS INTEGER))
             ON CONFLICT(backing_path) DO UPDATE SET
                format=excluded.format, audio_offset=excluded.audio_offset,
                audio_length=excluded.audio_length, backing_size=excluded.backing_size,
                backing_mtime_ns=excluded.backing_mtime_ns,
                backing_ctime_ns=excluded.backing_ctime_ns,
                updated_at=CAST(strftime('%s','now') AS INTEGER)",
            params![t.backing_path, t.format.as_str(), t.audio_offset, t.audio_length,
                    t.backing_size, t.backing_mtime_ns, t.backing_ctime_ns],
        )?;
        Ok(self.tx.query_row(
            "SELECT id FROM tracks WHERE backing_path = ?1",
            params![t.backing_path],
            |r| r.get(0),
        )?)
    }
```

- [ ] **Step 7: Fix db-layer test fixtures**

Across `musefs-db` (`tests/tracks.rs`, `tests/art.rs`, `tests/common/mod.rs`, and `#[cfg(test)]` blocks in `tracks.rs`), update every `NewTrack { ... }` / `Track { ... }` literal: replace `backing_mtime: X,` with `backing_mtime_ns: X, backing_ctime_ns: 0,` (or `0,0` where the value is a don't-care). The compiler enumerates each site. Also update any raw SQL test strings referencing the `backing_mtime` column to `backing_mtime_ns`.

- [ ] **Step 8: Verify the db crate alone is green**

Run: `cargo test -p musefs-db 2>&1 | tail -25`
Expected: PASS. If `schema_py` fails, that is expected here — it is fixed in Step 13 (regen). You may defer it: `cargo test -p musefs-db -- --skip schema_py`.

- [ ] **Step 9: Add `BackingStamp::from_track` (`musefs-core/src/freshness.rs`)**

Add the `from_track` method shown in Task 1, Step 3 (it now compiles — `Track` has the fields). Remove any `#[allow(dead_code)]` you added.

- [ ] **Step 10: Switch the serve path in `reader.rs`**

Delete the `mtime_secs` helper (lines 73-78). Change `ResolvedFile` (17-40): replace the two fields
```rust
    pub backing_size: u64,
    pub backing_mtime_secs: i64,
```
with
```rust
    pub stamp: crate::freshness::BackingStamp,
```
(keep `pub mtime_secs: i64,` — that is the *displayed* mtime).

In `resolve` (lines 117-121), replace the stat compare:
```rust
        crate::metrics::on_stat();
        let meta = std::fs::metadata(&track.backing_path)?;
        if crate::freshness::BackingStamp::from_metadata(&meta)
            != crate::freshness::BackingStamp::from_track(&track)
        {
            return Err(CoreError::BackingChanged(track.backing_path.clone()));
        }
```

In `build`, fix the two displayed-mtime sites. Line 149 (StructureOnly arm):
```rust
                (layout, meta.len(), crate::freshness::BackingStamp::from_track(track).display_secs())
```
Line 289 (Synthesis arm):
```rust
                (layout, total, crate::freshness::BackingStamp::from_track(track).display_secs().max(track.updated_at))
```

In the `ResolvedFile` construction (lines 309-320), replace
```rust
            backing_size: track.backing_size,
            backing_mtime_secs: track.backing_mtime,
            mtime_secs: mtime_secs_val,
```
with
```rust
            stamp: crate::freshness::BackingStamp::from_track(track),
            mtime_secs: mtime_secs_val,
```

Update every `ResolvedFile { ... }` literal in `reader.rs`'s `#[cfg(test)]` module (and the construction above): replace `backing_size: N, backing_mtime_secs: M,` with `stamp: crate::freshness::BackingStamp { size: N, mtime_ns: M, ctime_ns: 0 },`.

Deleting the `mtime_secs` helper also orphans its in-file test callers — every `backing_mtime:` field in a `NewTrack`/`Track`/`ResolvedFile` literal in `reader.rs`'s `#[cfg(test)]` module that the compiler now flags. These appear in several forms: `backing_mtime: mtime_secs(&meta),`, inline `backing_mtime: meta.modified()…as_secs().cast_signed(),`, and bare `backing_mtime: 0,`. Add a small test helper at the top of the `#[cfg(test)]` module and use it for the file-backed sites:
```rust
fn meta_stamp(p: &std::path::Path) -> crate::freshness::BackingStamp {
    crate::freshness::BackingStamp::from_metadata(&std::fs::metadata(p).unwrap())
}
```
Then a file-backed `backing_mtime: <secs>,` becomes `backing_mtime_ns: meta_stamp(&path).mtime_ns, backing_ctime_ns: meta_stamp(&path).ctime_ns,` (use the `path`/`meta` each test already has — `BackingStamp::from_metadata(&meta)` inline where only `meta` is in scope). A bare `backing_mtime: 0,` (don't-care, no real file) becomes `backing_mtime_ns: 0, backing_ctime_ns: 0,`. Do not assume a fixed line set — fix each site the compiler reports.

- [ ] **Step 11: Switch the serve path in `facade.rs`**

Delete the `mtime_secs` helper (lines 107-112). `SizeEntry` (85-95): replace
```rust
    backing_size: u64,
    backing_mtime_secs: i64,
```
with
```rust
    stamp: crate::freshness::BackingStamp,
```
(keep `mtime_secs: i64` — displayed value.)

`validate_opened_backing` (114-122):
```rust
fn validate_opened_backing(file: &std::fs::File, resolved: &ResolvedFile) -> Result<()> {
    let meta = file.metadata()?;
    if crate::freshness::BackingStamp::from_metadata(&meta) != resolved.stamp {
        return Err(CoreError::BackingChanged(
            resolved.backing_path.to_string_lossy().into_owned(),
        ));
    }
    Ok(())
}
```

`getattr` size-cache hit compare (lines 961-965):
```rust
                crate::metrics::on_stat();
                let meta = std::fs::metadata(&track.backing_path)?;
                if crate::freshness::BackingStamp::from_metadata(&meta) != e.stamp {
                    return Err(CoreError::BackingChanged(track.backing_path.clone()));
                }
                return Ok((e.total_len, e.mtime_secs));
```

`SizeEntry` insert (lines 970-979):
```rust
            self.size_cache.insert(
                track_id,
                SizeEntry {
                    content_version: track.content_version,
                    total_len: resolved.total_len,
                    mtime_secs: resolved.mtime_secs,
                    stamp: resolved.stamp,
                },
            );
```

Deleting `facade.rs`'s `mtime_secs` helper also orphans its in-file test caller at `facade.rs:1216-1217` (a `ResolvedFile`-shaped literal): replace
```rust
            backing_mtime_secs: mtime_secs(&expected_meta),
            mtime_secs: mtime_secs(&expected_meta),
```
with
```rust
            stamp: crate::freshness::BackingStamp::from_metadata(&expected_meta),
            mtime_secs: crate::freshness::BackingStamp::from_metadata(&expected_meta).display_secs(),
```
(adjust if that literal also names `backing_size:` — fold it into the `stamp`).

- [ ] **Step 12: Switch the scanner write sites in `scan.rs` (stamp from the path-stat for now)**

Delete the `mtime_secs` helper (lines 52-56). `Unit` (480-487):
```rust
struct Unit {
    abs_path: String,
    stamp: crate::freshness::BackingStamp,
    probed: Probed,
    weight: u64,
}
```

Worker `Unit` construction (lines 732-738): replace `meta_len`/`meta_mtime` with
```rust
                        let unit = Unit {
                            abs_path: abs.to_string_lossy().into_owned(),
                            stamp: crate::freshness::BackingStamp::from_metadata(&meta),
                            probed,
                            weight,
                        };
```

Writer destructure (lines 768-777): replace the `meta_len`/`meta_mtime` bindings with `stamp`, and pass it to `ingest_bulk`:
```rust
        for Unit {
            abs_path,
            stamp,
            probed,
            weight,
        } in batch.drain(..)
        {
            weights.push(weight);
            ingest_bulk(&mut bw, &abs_path, stamp, probed)?;
            *scanned += 1;
        }
```

`ingest_bulk` (581): change signature + the `NewTrack`:
```rust
fn ingest_bulk(
    bw: &mut musefs_db::BulkWriter<'_>,
    abs_path: &str,
    stamp: crate::freshness::BackingStamp,
    probed: Probed,
) -> Result<()> {
    let track_id = bw.upsert_track(&NewTrack {
        backing_path: abs_path.to_string(),
        format: probed.format,
        audio_offset: probed.audio_offset,
        audio_length: probed.audio_length,
        backing_size: stamp.size,
        backing_mtime_ns: stamp.mtime_ns,
        backing_ctime_ns: stamp.ctime_ns,
    })?;
    // ... rest unchanged ...
```

`ingest` (504): replace the `meta: &std::fs::Metadata` param with `stamp: crate::freshness::BackingStamp`, and the `NewTrack` body as above (`backing_size: stamp.size`, `backing_mtime_ns: stamp.mtime_ns`, `backing_ctime_ns: stamp.ctime_ns`). Its only non-test caller is `scan_directory_full_oracle` (line 863) — change it to:
```rust
        let meta = std::fs::metadata(&path)?;
        let abs = std::fs::canonicalize(&path)?;
        ingest(db, &abs.to_string_lossy(), crate::freshness::BackingStamp::from_metadata(&meta), probed)?;
```
and update the in-file `ingest(` test callers (scan.rs ~1724, ~1790) similarly (`crate::freshness::BackingStamp::from_metadata(&meta)` in place of `&meta`).

The two in-file `ingest_bulk(` test callers pass `meta_len`/`meta_mtime` **positionally** and must also change:
- `scan.rs:1748` — `ingest_bulk(&mut bw, "/a.mp3", 1, 0, probed_with_mixed_binary_tags())` → replace the `1, 0` with `crate::freshness::BackingStamp { size: 1, mtime_ns: 0, ctime_ns: 0 }`.
- `scan.rs:1814` — the multi-line `ingest_bulk(` call; replace its `meta_len, meta_mtime` (`1, 0`) args with `crate::freshness::BackingStamp { size: 1, mtime_ns: 0, ctime_ns: 0 }`.
(These back synthetic `/a.*` paths never stat-compared, so a zero stamp is fine.)

- [ ] **Step 13: Fix core test fixtures and regenerate the Python mirrors**

Across `musefs-core` tests, update each `NewTrack`/`Track` literal. The compiler enumerates them; the named sites below need specific handling (do not blanket-replace, because some pass a **seconds** value that must become **nanoseconds**):

- **`tests/metrics.rs:230`** — a `/x/ghost.mp3` track never stat-compared: `backing_mtime: 0,` → `backing_mtime_ns: 0, backing_ctime_ns: 0,`. (Re-compiled by the Step 14 `--features metrics` run.)
- **`tests/external_contract.rs:34`** and **`tests/interop_emit.rs:170, :233`** — these back **real on-disk files** and pass each file's whole-second `real_mtime(...)` (a local helper in each file returning seconds). A seconds value in a `_ns` field would mismatch the real file's nanosecond stat and spuriously trip `BackingChanged`. Replace `backing_mtime: real_mtime(x),` with a real nanosecond stamp:
  ```rust
  backing_mtime_ns: { use std::os::unix::fs::MetadataExt; let m = std::fs::metadata(x).unwrap(); m.mtime() * 1_000_000_000 + m.mtime_nsec() },
  backing_ctime_ns: { use std::os::unix::fs::MetadataExt; let m = std::fs::metadata(x).unwrap(); m.ctime() * 1_000_000_000 + m.ctime_nsec() },
  ```
  (`x` is `&audio_path` / `src` respectively.) Their now-unused local `real_mtime` helpers can be deleted, or kept if still referenced elsewhere in the file.
- **`fuzz/fuzz_targets/serve.rs:29`** — the `fuzz/` crate is **outside the workspace**, so `cargo test`/`cargo build`/`clippy --all-targets` never compile it; this rename will break it invisibly to the Task-14 gates. Fix it here in the slice: change `backing_mtime: i64::try_from(...)` to `backing_mtime_ns: i64::try_from(...)` (keep the same value expression) and add `backing_ctime_ns: 0,`. This track backs a fuzz-synthesized file; a zero ctime is acceptable for the harness.
- All other flagged `NewTrack`/`Track` literals (e.g. `tests/read_at.rs`, `tests/reader.rs`, `tests/concurrent_reads.rs`, `tests/flac_binary_tags.rs`, `tests/incremental_refresh.rs`, `tests/proptest_read_fidelity.rs`, `tests/reader_faults.rs`, `tests/db_corruption_fault.rs`): if the value is a real file's mtime, use the nanosecond form above (or the `common::real_stamp` helper); if it is a don't-care `0`, use `backing_mtime_ns: 0, backing_ctime_ns: 0`.

For `tests/backing_changed_fault.rs`'s pre-existing `shrinking_the_backing_file...` test, replace `backing_mtime: common::real_mtime(&src),` with:
```rust
            backing_mtime_ns: { let m = std::fs::metadata(&src).unwrap(); use std::os::unix::fs::MetadataExt; m.mtime() * 1_000_000_000 + m.mtime_nsec() },
            backing_ctime_ns: { let m = std::fs::metadata(&src).unwrap(); use std::os::unix::fs::MetadataExt; m.ctime() * 1_000_000_000 + m.ctime_nsec() },
```
(or add a `common::real_stamp(&src) -> (i64, i64)` helper to `tests/common/mod.rs` and use it across the core tests to keep this DRY.) Update `ResolvedFile { ... }` literals in `tests/read_at.rs` / `tests/reader.rs` the same way as Step 10.

Regenerate both schema mirrors:
```bash
MUSEFS_REGEN_SCHEMA_PY=1 cargo test -p musefs-db schema_py
python contrib/python-musefs/vendor_to_picard.py
```

- [ ] **Step 14: Run the full workspace suite + the new tests**

```bash
cargo test 2>&1 | tail -30
cargo test -p musefs-core --features metrics 2>&1 | tail -30
cargo test -p musefs-core --test backing_changed_fault --test facade 2>&1 | tail -20
cargo +nightly fuzz build serve 2>&1 | tail -5   # out-of-workspace; not covered by `cargo test`
```
Expected: all PASS, including the four new tests. The `metrics` run must still pass with **no** assertion edits (the serve-path stat counts are unchanged — one `metadata` per getattr hit). The fuzz build must succeed — the `NewTrack` rename touched `fuzz/fuzz_targets/serve.rs` (Step 13) and the fuzz crate is invisible to the other gates.

- [ ] **Step 15: Lint + format**

```bash
cargo clippy --all-targets 2>&1 | tail -20
cargo fmt --all
```
Expected: no warnings (CI uses `-D warnings`).

- [ ] **Step 16: Commit**

```bash
git add -A
git commit -m "feat: nanosecond mtime + ctime freshness stamp (#276)"
```

---

## Task 3: Scanner fstat sandwich (`probe_file`)

Make `probe_file` open once, fstat before and after the probe, and report `Raced` when the file moved mid-probe — so the stored stamp and the probed bytes provably share one inode held still across the probe.

**Files:**
- Modify: `musefs-core/src/scan.rs` (`ScanStats`/`RevalidateStats`, `probe_file` 279-370, worker 716-752, `run_pipeline` counters 702-703, test call sites 2010/2050)

- [ ] **Step 1: Write the failing test**

In `scan.rs`'s `#[cfg(test)] mod tests`, add (model the file setup on `oversize_wav_is_served_via_data_header` at line 2014):

```rust
    #[test]
    fn probe_file_reports_raced_on_mid_probe_mutation() {
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.wav");

        // Minimal valid WAV the probe accepts (fmt + tiny data).
        let mut fmt = Vec::new();
        for v in [1u16, 1, 0, 0, 0, 16] { fmt.extend_from_slice(&v.to_le_bytes()); }
        // (sample_rate/byte_rate left 0; the chunk walk does not validate them.)
        let mut front = b"RIFF".to_vec();
        front.extend_from_slice(&0u32.to_le_bytes());
        front.extend_from_slice(b"WAVE");
        front.extend_from_slice(b"fmt ");
        front.extend_from_slice(&(fmt.len() as u32).to_le_bytes());
        front.extend_from_slice(&fmt);
        front.extend_from_slice(b"data");
        front.extend_from_slice(&64u32.to_le_bytes());
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(&front).unwrap();
        f.set_len(front.len() as u64 + 64).unwrap();
        drop(f);

        let pc = path.clone();
        set_after_s1_hook(move || {
            let mut g = std::fs::OpenOptions::new().append(true).open(&pc).unwrap();
            g.write_all(&[0u8; 4096]).unwrap(); // size moves → S2 != S1
        });
        let out = probe_file(&path, WINDOW);
        clear_after_s1_hook();
        assert!(matches!(out, Ok(ProbeOutcome::Raced)), "got {out:?}");
    }
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p musefs-core --lib probe_file_reports_raced 2>&1 | tail -20`
Expected: FAIL — `ProbeOutcome` / `set_after_s1_hook` undefined, and `probe_file` still takes `file_len`.

- [ ] **Step 3: Add the outcome enum, test hook, and `raced` counters**

Near the top of `scan.rs` (module scope):

```rust
/// Outcome of probing one backing file. `Raced` means the file changed under us
/// between the pre- and post-probe `fstat` — the probe may be torn, so nothing
/// is committed for it (#276).
#[derive(Debug)]
enum ProbeOutcome {
    Probed(Probed, crate::freshness::BackingStamp),
    Unsupported,
    Raced,
}

#[cfg(test)]
thread_local! {
    static AFTER_S1_HOOK: std::cell::RefCell<Option<Box<dyn FnMut()>>> =
        const { std::cell::RefCell::new(None) };
}
#[cfg(test)]
fn fire_after_s1() {
    AFTER_S1_HOOK.with(|h| { if let Some(f) = h.borrow_mut().as_mut() { f() } });
}
#[cfg(test)]
fn set_after_s1_hook(f: impl FnMut() + 'static) {
    AFTER_S1_HOOK.with(|h| *h.borrow_mut() = Some(Box::new(f)));
}
#[cfg(test)]
fn clear_after_s1_hook() {
    AFTER_S1_HOOK.with(|h| *h.borrow_mut() = None);
}
```

Add `pub raced: u64,` to both `ScanStats` (line 38) and `RevalidateStats` (line 45), and update every literal:
- `run_pipeline`'s returned `ScanStats { ... }` (line 831): `raced: raced.load(Ordering::Relaxed)` (the new Arc counter from Step 5).
- `scan_directory_full_oracle`'s `ScanStats { ... }` (line 850): `raced: 0` (whole-file oracle path; no sandwich).
- `revalidate_with`'s `RevalidateStats { ... }` (line 952): `raced: scan.raced` (carry it through from the pipeline result).
- Any `ScanStats`/`RevalidateStats` literal or field-assert in tests the compiler flags.

- [ ] **Step 4: Rewrite `probe_file`'s open/stat/probe shell**

Change the signature and wrap the existing probe body. The internal probe logic (m4a arm, front-anchored widen loop, fallbacks) is unchanged except it no longer receives `file_len` as a parameter — derive it from S1 and feed S1's `size` everywhere the old `file_len` was used (including `mp4::read_structure_from(&mut f, file_len)`):

```rust
fn probe_file(path: &Path, window: usize) -> std::io::Result<ProbeOutcome> {
    let file = std::fs::File::open(path)?;
    crate::metrics::on_scan_open();
    let s1 = crate::freshness::BackingStamp::from_metadata(&file.metadata()?);
    #[cfg(test)]
    fire_after_s1();
    let file_len = s1.size;

    // ---- existing probe body, verbatim, but using `file_len = s1.size` ----
    let probed: Option<Probed> = { /* the current body, returning the Probed or None */ };
    // -----------------------------------------------------------------------

    let s2 = crate::freshness::BackingStamp::from_metadata(&file.metadata()?);
    if s1 != s2 {
        log::warn!("skipping {}: changed during probe", path.display());
        return Ok(ProbeOutcome::Raced);
    }
    Ok(match probed {
        Some(p) => ProbeOutcome::Probed(p, s1),
        None => ProbeOutcome::Unsupported,
    })
}
```

> Mechanical refactor of the body: the current function has many early `return Ok(Some(p))` / `return Ok(None)` points. Convert them so the body yields an `Option<Probed>` bound to `probed` instead of returning directly (e.g. extract the current body into an inner closure `let probe = |file_len: u64| -> std::io::Result<Option<Probed>> { ... };` and call `let probed = probe(file_len)?;`). The inner closure keeps using the same `file` handle (capture `&file`).

- [ ] **Step 5: Update the worker loop (`scan.rs:719-751`)**

Remove the pre-probe `std::fs::metadata(&path)` at line 720 (open failure inside `probe_file` now routes to `failed`), and switch the match:

```rust
                let Some(path) = next else { break };
                match probe_file(&path, window) {
                    Ok(ProbeOutcome::Probed(probed, stamp)) => {
                        let Ok(abs) = std::fs::canonicalize(&path) else {
                            failed.fetch_add(1, Ordering::Relaxed);
                            continue;
                        };
                        let weight = payload_weight(&probed);
                        budget.acquire(weight);
                        let unit = Unit {
                            abs_path: abs.to_string_lossy().into_owned(),
                            stamp,
                            probed,
                            weight,
                        };
                        if tx.send(unit).is_err() {
                            budget.release(weight);
                            break;
                        }
                    }
                    Ok(ProbeOutcome::Unsupported) => { skipped.fetch_add(1, Ordering::Relaxed); }
                    Ok(ProbeOutcome::Raced) => { raced.fetch_add(1, Ordering::Relaxed); }
                    Err(_) => { failed.fetch_add(1, Ordering::Relaxed); }
                }
```

Add a `raced` `Arc<AtomicU64>` alongside `skipped`/`failed` (lines 702-703, cloned into each worker like the others), and fold its final value into the returned `ScanStats.raced`.

- [ ] **Step 6: Update the existing `probe_file` unit-test call sites**

Line 2010: `assert!(matches!(probe_file(&path, WINDOW), Ok(ProbeOutcome::Unsupported)));`
Line 2050-2052:
```rust
        let probed = match probe_file(&path, WINDOW).unwrap() {
            ProbeOutcome::Probed(p, _) => p,
            other => panic!("oversize wav should probe, got {other:?}"),
        };
```

- [ ] **Step 7: Run the test + suite**

```bash
cargo test -p musefs-core --lib probe_file 2>&1 | tail -20
cargo test 2>&1 | tail -20
cargo clippy --all-targets 2>&1 | tail -10
```
Expected: PASS, no warnings.

- [ ] **Step 8: Commit**

```bash
git add -A
git commit -m "feat(scan): fstat-sandwich probe_file, stamp from the probed fd (#276)"
```

---

## Task 4: Revalidate skip pass compares the full stamp

`scan --revalidate`'s pre-dispatch skip must treat a ctime-only change as "changed" so an adversarial mtime-reset-after-rewrite is re-probed instead of skipped.

**Files:**
- Modify: `musefs-core/src/scan.rs` (`revalidate_with` `existing` map 896-905 and skip compare 924-929)
- Test: `musefs-core/tests/` (new integration test; create `revalidate_freshness.rs` or extend an existing revalidate test file if one exists — check `grep -rl revalidate musefs-core/tests`)

- [ ] **Step 1: Write the failing test**

Create `musefs-core/tests/revalidate_freshness.rs`:

```rust
mod common;
use musefs_db::Db;

// A ctime-only change (mtime forged back after an in-place same-size rewrite)
// must NOT be skipped as "unchanged": revalidate re-probes it.
#[test]
fn revalidate_reprobes_on_ctime_only_change() {
    use std::os::unix::fs::MetadataExt;
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("a.flac");
    common::write_flac(&src, &["TITLE=Old"], &[0xAB; 4096]);
    let db_path = dir.path().join("m.db");
    {
        let db = Db::open(&db_path).unwrap();
        musefs_core::scan_directory(&db, dir.path()).unwrap();
    }
    let original_modified = std::fs::metadata(&src).unwrap().modified().unwrap();

    // Rewrite in place (same size, new tag), then forge mtime back. ctime moved.
    common::write_flac(&src, &["TITLE=New"], &[0xCD; 4096]);
    let f = std::fs::OpenOptions::new().write(true).open(&src).unwrap();
    f.set_times(std::fs::FileTimes::new().set_modified(original_modified)).unwrap();
    drop(f);

    let db = Db::open(&db_path).unwrap();
    let stats = musefs_core::revalidate(&db, dir.path()).unwrap();
    assert_eq!(stats.updated, 1, "ctime-only change must be re-probed");
}
```

> `musefs_core::revalidate(db, root) -> RevalidateStats { updated, unchanged, pruned, failed, raced }` is re-exported from the crate root (`lib.rs:22`). `write_flac` returns `(audio_offset, audio_length)`; ignore them here.

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p musefs-core --test revalidate_freshness 2>&1 | tail -20`
Expected: FAIL — `updated == 0` (the old skip pass compares only size + whole-second mtime, both unchanged after the forge).

- [ ] **Step 3: Widen the `existing` map and skip compare (`scan.rs`)**

Change the `existing` map (lines 896-905) to carry `backing_ctime_ns`:

```rust
    let existing: HashMap<String, (crate::freshness::BackingStamp, i64, Format)> = db
        .list_tracks()?
        .into_iter()
        .map(|t| {
            (
                t.backing_path.clone(),
                (crate::freshness::BackingStamp::from_track(&t), t.id, t.format),
            )
        })
        .collect();
```

Change the skip check (lines 924-929):

```rust
        if let Some((stamp, id, format)) = existing.get(&key).copied() {
            let needs_backfill = format == Format::Flac && !have_structural.contains(&id);
            if crate::freshness::BackingStamp::from_metadata(&meta) == stamp && !needs_backfill {
                unchanged += 1;
                continue;
            }
        }
```

> `from_track` borrows `t`; clone `backing_path` before the map closure moves it (shown above). `BackingStamp` is `Copy`, so `.copied()` on the `get` works.

- [ ] **Step 4: Run the test + suite**

```bash
cargo test -p musefs-core --test revalidate_freshness 2>&1 | tail -20
cargo test 2>&1 | tail -20
cargo clippy --all-targets 2>&1 | tail -10
```
Expected: PASS, no warnings.

- [ ] **Step 5: Commit**

```bash
git add -A
git commit -m "feat(scan): revalidate compares the full freshness stamp (#276)"
```

---

## Task 5: Documentation

**Files:**
- Modify: `ARCHITECTURE.md`

- [ ] **Step 1: Update the freshness/contract prose**

Edit `ARCHITECTURE.md`:
- The `tracks` column list (around line 157-158, "Ownership"): replace `backing_mtime` with `backing_mtime_ns` and add `backing_ctime_ns` to the scanner-owned set.
- "What musefs defends at serve time" (around 193-203): the serve-time guard now compares size + nanosecond mtime + ctime, not whole-second mtime.
- "Freshness: two version counters" (around 249-268): describe the stamp as `(size, mtime_ns, ctime_ns)`; note the scanner stamps from the **probed descriptor** with a pre/post `fstat` sandwich and drops files that change mid-probe; note `ctime` defeats an mtime-forging writer.
- V5 description (around 143-151) and the V1 `tracks` column description (around 119-120): reflect the renamed/added columns and that the geometry trigger keys on `backing_mtime_ns`.

Keep edits factual and concise; do not restructure sections.

- [ ] **Step 2: Final full verification**

```bash
cargo fmt --all --check
cargo clippy --all-targets 2>&1 | tail -10
cargo test 2>&1 | tail -20
cargo test -p musefs-core --features metrics 2>&1 | tail -20
cargo +nightly fuzz build 2>&1 | tail -5   # re-confirm; serve.rs was fixed in Task 2 Step 13
```
Expected: all green. (The `fuzz/` crate is out-of-workspace and consumes `NewTrack` in `serve.rs`; it was updated and built in Task 2. This is a final re-confirmation that the whole fuzz crate still builds.)

- [ ] **Step 3: Commit**

```bash
git add ARCHITECTURE.md
git commit -m "docs: nanosecond mtime + ctime freshness stamp (#276)"
```

---

## Done-when

- Three serve-path compare sites (`resolve`, `validate_opened_backing`, `getattr` size-cache) and the revalidate skip use `(size, mtime_ns, ctime_ns)`.
- The scanner stamps from the same descriptor it probes and drops mid-probe-mutated files (`raced`).
- A same-size sub-second rewrite, and an adversarial mtime-reset-after-rewrite, both yield `BackingChanged`; the synthesized displayed mtime stays whole seconds.
- `user_version` is still 5; both `schema.py` mirrors regenerated; full workspace suite + `metrics` feature green.
