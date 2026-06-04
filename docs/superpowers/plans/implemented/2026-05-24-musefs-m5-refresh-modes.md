# musefs M5 (Structure-Only Mode + Refresh Polish) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Complete the MVP — add a structure-only (passthrough) mount mode, automatic pickup of external DB edits via `data_version` polling, and a `scan --revalidate` maintenance pass that skips unchanged files, prunes missing-backing tracks, and garbage-collects orphaned art.

**Architecture:** Structure-only mode threads a `Mode` through `MountConfig` into the header cache, which then resolves a file as a single whole-backing-file passthrough segment instead of synthesizing metadata. Auto-refresh adds `Musefs::poll_refresh`, called by the FUSE layer on metadata ops, which rebuilds the tree + clears caches when `PRAGMA data_version` changes. `revalidate` reuses the scan ingest path with a stamp-based skip, then deletes tracks whose backing file is gone (cascading tags/track_art) and GCs unreferenced art.

**Tech Stack:** Existing `musefs-db` / `musefs-core` / `musefs-fuse` / `musefs-cli`. SQLite `PRAGMA data_version` (already exposed) for change detection; FK `ON DELETE CASCADE` (already in the schema) for track pruning; `clap` `ValueEnum` for the `--mode` flag.

**Decisions (confirmed):** auto-refresh via `data_version` polling only (the explicit `musefs refresh`/SIGHUP command is **deferred** — polling makes it the "rarely needed" fallback the spec describes, and signal handling in fuser's blocking loop isn't worth the complexity for MVP); `scan --revalidate` does **skip-unchanged + prune-missing + art GC**.

**Scope (this milestone):** structure-only mode, automatic `data_version`-driven refresh, `scan --revalidate`, track pruning + orphan-art GC, and the CLI flags wiring them up. Completes the MVP.

**Explicitly deferred (post-MVP):** the explicit `musefs refresh <mountpoint>` SIGHUP command (auto-refresh covers it); writable mount / path overrides; beets/picard plugins; Ogg/Opus/MP4.

---

## File Structure

- `musefs-db/src/tracks.rs` — add `delete_track` (cascades tags/track_art).
- `musefs-db/src/art.rs` — add `gc_orphan_art`.
- `musefs-db/tests/{tracks,art}.rs` — delete/GC tests.
- `musefs-core/src/facade.rs` — `Mode` enum; `MountConfig.mode`; `Musefs.last_data_version`; `Musefs::poll_refresh`.
- `musefs-core/src/reader.rs` — `HeaderCache::new(mode)`, `HeaderCache::clear`, mode-aware `resolve` (passthrough layout for structure-only).
- `musefs-core/src/scan.rs` — extract an `ingest` helper; add `revalidate` + `RevalidateStats`.
- `musefs-core/src/lib.rs` — re-export `Mode`, `revalidate`, `RevalidateStats`.
- `musefs-core/tests/{reader,facade,scan}.rs` — structure-only, poll_refresh, revalidate tests.
- `musefs-fuse/src/lib.rs` — call `poll_refresh` on metadata ops.
- `musefs-cli/src/lib.rs` — `--mode` on mount, `--revalidate` on scan; wire `run_mount`/`run_scan`.
- `musefs-cli/tests/cli.rs` — parsing coverage.

**Branch:** all work on a new branch `musefs-m5-refresh-modes` cut from `main`.

---

## Task 1: `musefs-db` — `delete_track` + `gc_orphan_art`

**Files:**
- Modify: `musefs-db/src/tracks.rs`, `musefs-db/src/art.rs`
- Test: `musefs-db/tests/tracks.rs`, `musefs-db/tests/art.rs`

- [ ] **Step 1: Write the failing tests**

Append to `musefs-db/tests/tracks.rs`:

```rust
#[test]
fn delete_track_cascades_tags_and_track_art() {
    let db = Db::open_in_memory().unwrap();
    let id = db
        .upsert_track(&NewTrack {
            backing_path: "/x/a.flac".to_string(),
            format: Format::Flac,
            audio_offset: 0,
            audio_length: 0,
            backing_size: 0,
            backing_mtime: 0,
        })
        .unwrap();
    db.replace_tags(id, &[Tag::new("artist", "A", 0)]).unwrap();
    let art_id = db
        .upsert_art(&NewArt {
            mime: "image/png".to_string(),
            width: None,
            height: None,
            data: vec![1, 2, 3],
        })
        .unwrap();
    db.set_track_art(
        id,
        &[TrackArt {
            art_id,
            picture_type: 3,
            description: String::new(),
            ordinal: 0,
        }],
    )
    .unwrap();

    db.delete_track(id).unwrap();

    assert!(db.get_track(id).unwrap().is_none());
    assert!(db.get_tags(id).unwrap().is_empty());
    assert!(db.get_track_art(id).unwrap().is_empty());
    // The art row itself remains (GC is a separate step) until gc_orphan_art runs.
    assert!(db.get_art(art_id).unwrap().is_some());
}
```

(Confirm the top of `musefs-db/tests/tracks.rs` imports the types it now uses — `Db, Format, NewTrack, Tag, NewArt, TrackArt`. Add any missing names to the existing `use musefs_db::{...};` line.)

Append to `musefs-db/tests/art.rs`:

```rust
#[test]
fn gc_orphan_art_removes_unreferenced_rows() {
    let db = Db::open_in_memory().unwrap();
    let track = db
        .upsert_track(&musefs_db::NewTrack {
            backing_path: "/x/a.flac".to_string(),
            format: musefs_db::Format::Flac,
            audio_offset: 0,
            audio_length: 0,
            backing_size: 0,
            backing_mtime: 0,
        })
        .unwrap();
    let referenced = db
        .upsert_art(&NewArt {
            mime: "image/png".to_string(),
            width: None,
            height: None,
            data: vec![1, 2, 3],
        })
        .unwrap();
    let orphan = db
        .upsert_art(&NewArt {
            mime: "image/png".to_string(),
            width: None,
            height: None,
            data: vec![9, 9, 9],
        })
        .unwrap();
    db.set_track_art(
        track,
        &[musefs_db::TrackArt {
            art_id: referenced,
            picture_type: 3,
            description: String::new(),
            ordinal: 0,
        }],
    )
    .unwrap();

    let removed = db.gc_orphan_art().unwrap();
    assert_eq!(removed, 1);
    assert!(db.get_art(referenced).unwrap().is_some());
    assert!(db.get_art(orphan).unwrap().is_none());
}
```

(Ensure `musefs-db/tests/art.rs` imports `Db, NewArt` — it already uses `NewArt`; add `NewTrack`/`TrackArt`/`Format` to the import line or use the fully-qualified `musefs_db::...` forms shown above.)

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p musefs-db --test tracks delete_track --test art gc_orphan`
Expected: FAIL — `delete_track` / `gc_orphan_art` not found.

- [ ] **Step 3: Implement the methods**

In `musefs-db/src/tracks.rs`, add inside the existing `impl Db { ... }` block:

```rust
    /// Delete a track row. Foreign keys cascade to its `tags` and `track_art`
    /// rows; the referenced `art` rows are left for `gc_orphan_art`.
    pub fn delete_track(&self, id: i64) -> Result<()> {
        self.conn
            .execute("DELETE FROM tracks WHERE id = ?1", params![id])?;
        Ok(())
    }
```

In `musefs-db/src/art.rs`, add inside the existing `impl Db { ... }` block:

```rust
    /// Delete `art` rows no longer referenced by any `track_art`. Returns the
    /// number of rows removed.
    pub fn gc_orphan_art(&self) -> Result<usize> {
        let removed = self.conn.execute(
            "DELETE FROM art WHERE id NOT IN (SELECT art_id FROM track_art)",
            [],
        )?;
        Ok(removed)
    }
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p musefs-db`
Expected: PASS (the new delete/GC tests plus all existing db tests; the cascade relies on `PRAGMA foreign_keys = ON`, which `Db::open*` already sets).

- [ ] **Step 5: Confirm zero warnings + fmt, then commit**

Run: `cargo clippy -p musefs-db --all-targets 2>&1 | grep -iE "warning|error" || echo clean` (expected `clean`).
Run: `cargo fmt -p musefs-db` then `cargo fmt --all -- --check`.

```bash
git add musefs-db/src/tracks.rs musefs-db/src/art.rs musefs-db/tests/tracks.rs musefs-db/tests/art.rs
git commit -m "$(printf 'feat(db): delete_track (cascading) and gc_orphan_art\n\nCo-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>')"
```

---

## Task 2: `musefs-core` — structure-only (passthrough) mount mode

**Files:**
- Modify: `musefs-core/src/facade.rs`, `musefs-core/src/reader.rs`, `musefs-core/src/lib.rs`
- Test: `musefs-core/tests/reader.rs` (+ update existing `HeaderCache::new()` call sites in `tests/reader.rs` and `tests/read_at.rs`, and `MountConfig` literals)

- [ ] **Step 1: Write the failing test**

Append to `musefs-core/tests/reader.rs`:

```rust
#[test]
fn structure_only_resolves_to_whole_backing_file() {
    use musefs_core::Mode;
    use musefs_format::Segment;

    let (dir, db, id) = setup();
    let backing = dir.path().join("song.flac");
    let original = std::fs::read(&backing).unwrap();

    let mut cache = HeaderCache::new(Mode::StructureOnly);
    let resolved = cache.resolve(&db, id).unwrap();

    // Passthrough: one whole-file backing segment, size == the real file.
    assert_eq!(resolved.total_len, original.len() as u64);
    assert_eq!(
        resolved.layout.segments,
        vec![Segment::BackingAudio {
            offset: 0,
            len: original.len() as u64
        }]
    );

    // Reading the whole file yields the original bytes unchanged (not synthesized).
    let whole = read_at(&resolved, &db, 0, resolved.total_len).unwrap();
    assert_eq!(whole, original);
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p musefs-core --test reader structure_only`
Expected: FAIL — `Mode` not found / `HeaderCache::new` takes no args.

- [ ] **Step 3: Add `Mode`, thread it through `MountConfig` and `HeaderCache`**

In `musefs-core/src/facade.rs`, add the `Mode` enum above `MountConfig` and a `mode` field on `MountConfig`:

```rust
/// How the mount serves file *contents*. The virtual tree is identical either way.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Splice a freshly synthesized metadata region in front of the backing audio.
    Synthesis,
    /// Pure passthrough: serve the original backing file bytes unchanged.
    StructureOnly,
}
```

Change `MountConfig` to:

```rust
#[derive(Debug, Clone)]
pub struct MountConfig {
    pub template: String,
    pub fallbacks: BTreeMap<String, String>,
    pub default_fallback: String,
    pub mode: Mode,
}
```

In `Musefs::open`, construct the cache with the mode. The current struct literal moves `config` on its `config,` line, so `cache: HeaderCache::new(config.mode)` must be evaluated *before* that — struct-literal initializers run in source order, and `MountConfig` is not `Copy`. Rewrite `open` so `cache` is the first field, reading `config.mode` before `config` is moved:

```rust
    pub fn open(db: Db, config: MountConfig) -> Result<Musefs> {
        let tree = Self::build_tree(&db, &config)?;
        Ok(Musefs {
            cache: HeaderCache::new(config.mode),
            db,
            config,
            tree,
        })
    }
```

(Task 3 rewrites this method again to add `last_data_version`, also keeping `cache` first for the same reason.)

In `musefs-core/src/reader.rs`, import `Mode` and store it on the cache. Add near the top imports:

```rust
use crate::facade::Mode;
```

Change `HeaderCache` to hold the mode (it currently `#[derive(Default)]` with a single `map` field). Replace the struct + `new` with:

```rust
/// A per-mount cache of resolved files, keyed by track id and invalidated when a
/// track's `content_version` changes (the DB bumps it on any tag/art edit).
pub struct HeaderCache {
    map: HashMap<i64, Arc<ResolvedFile>>,
    mode: Mode,
}

impl HeaderCache {
    pub fn new(mode: Mode) -> HeaderCache {
        HeaderCache {
            map: HashMap::new(),
            mode,
        }
    }
}
```

(Delete the old `#[derive(Default)]` on `HeaderCache` and the old `impl HeaderCache { pub fn new() -> HeaderCache { HeaderCache::default() } }` — fold `new` into the impl block that already holds `resolve`, or keep a separate impl block as shown. Do not leave a `Default` derive, since `Mode` has no meaningful default.)

In `resolve`, after the existing backing validation + audio-bounds guard + cache-hit check, replace the synthesis block (the part that builds `tags`/`inputs`/`art_inputs`, the `match track.format { ... }`, and `let total_len = layout.total_len();`) with a mode branch. The resulting tail of `resolve` should read:

```rust
        let (layout, total_len, mtime_secs_val) = match self.mode {
            Mode::StructureOnly => {
                // Pure passthrough: the synthesized "file" is the backing file itself.
                let layout = RegionLayout::new(vec![Segment::BackingAudio {
                    offset: 0,
                    len: meta.len(),
                }]);
                (layout, meta.len(), track.backing_mtime)
            }
            Mode::Synthesis => {
                let tags = db.get_tags(track_id)?;
                let inputs = tags_to_inputs(&tags);
                let art_inputs = track_art_to_inputs(db, track_id)?;
                let layout = match track.format {
                    Format::Flac => {
                        let front =
                            read_front(Path::new(&track.backing_path), track.audio_offset as u64)?;
                        let fmeta = flac::read_metadata(&front)?;
                        let scan = FlacScan {
                            audio_offset: track.audio_offset as u64,
                            audio_length: track.audio_length as u64,
                            preserved: fmeta.preserved,
                        };
                        flac::synthesize_layout(&scan, &inputs, &art_inputs)?
                    }
                    Format::Mp3 => mp3::synthesize_layout(
                        track.audio_offset as u64,
                        track.audio_length as u64,
                        &inputs,
                        &art_inputs,
                    )?,
                };
                let total = layout.total_len();
                (layout, total, track.backing_mtime.max(track.updated_at))
            }
        };

        let resolved = Arc::new(ResolvedFile {
            layout,
            total_len,
            content_version: track.content_version,
            backing_path: PathBuf::from(&track.backing_path),
            mtime_secs: mtime_secs_val,
        });
        self.map.insert(track_id, resolved.clone());
        Ok(resolved)
```

Ensure `Segment` is imported in `reader.rs` (it already is — `read_at` uses `musefs_format::Segment`; if the import is local to a function, add `use musefs_format::Segment;` at module scope or reference `musefs_format::Segment` fully-qualified in `resolve`).

In `musefs-core/src/lib.rs`, add `Mode` to the facade re-export:

```rust
pub use facade::{Attr, Mode, MountConfig, Musefs};
```

- [ ] **Step 4: Update existing `HeaderCache::new()` and `MountConfig` call sites**

These now need the mode argument/field. Update:
- `musefs-core/tests/reader.rs`: every `HeaderCache::new()` → `HeaderCache::new(musefs_core::Mode::Synthesis)` (add `use musefs_core::Mode;` at the top, then `HeaderCache::new(Mode::Synthesis)`).
- `musefs-core/tests/read_at.rs`: same — `HeaderCache::new()` in `setup()` → `HeaderCache::new(musefs_core::Mode::Synthesis)`.
- `musefs-core/tests/facade.rs`: the `config()` helper's `MountConfig { ... }` literal → add `mode: musefs_core::Mode::Synthesis,` (add the import or qualify).
- `musefs-fuse/tests/mount.rs`: its `config()` `MountConfig { ... }` literal → add `mode: musefs_core::Mode::Synthesis,`.
- `musefs-cli/src/lib.rs`: `run_mount`'s `MountConfig { ... }` literal → add `mode` (Task 6 reworks this; for now add `mode: musefs_core::Mode::Synthesis,` so the workspace compiles after this task).

Run a workspace build to find any remaining literal: `cargo build --workspace --tests 2>&1 | grep -E "missing field .mode.|HeaderCache::new" || echo "all updated"`.

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p musefs-core --test reader`
Expected: PASS (the new structure-only test + existing reader tests).
Run: `cargo test --workspace 2>&1 | grep -E "test result: FAILED" || echo "workspace green"`
Expected: `workspace green`.

- [ ] **Step 6: Confirm zero warnings + fmt, then commit**

Run: `cargo clippy --workspace --all-targets 2>&1 | grep -iE "warning|error" || echo clean` (expected `clean`).
Run: `cargo fmt --all` then `cargo fmt --all -- --check`.

```bash
git add musefs-core/src/facade.rs musefs-core/src/reader.rs musefs-core/src/lib.rs \
        musefs-core/tests/reader.rs musefs-core/tests/read_at.rs musefs-core/tests/facade.rs \
        musefs-fuse/tests/mount.rs musefs-cli/src/lib.rs
git commit -m "$(printf 'feat(core): structure-only passthrough mount mode\n\nCo-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>')"
```

---

## Task 3: `musefs-core` — auto-refresh via `data_version` polling

**Files:**
- Modify: `musefs-core/src/facade.rs`, `musefs-core/src/reader.rs`
- Test: `musefs-core/tests/facade.rs`

- [ ] **Step 1: Write the failing test**

Append to `musefs-core/tests/facade.rs`:

```rust
#[test]
fn poll_refresh_picks_up_external_db_edits() {
    use musefs_db::{Format, NewTrack, Tag};

    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("m.db");

    // Seed one track (Alice) and open a mount over the on-disk DB.
    {
        let db = musefs_db::Db::open(&db_path).unwrap();
        let id = db
            .upsert_track(&NewTrack {
                backing_path: "/x/a.flac".to_string(),
                format: Format::Flac,
                audio_offset: 0,
                audio_length: 0,
                backing_size: 0,
                backing_mtime: 0,
            })
            .unwrap();
        db.replace_tags(
            id,
            &[Tag::new("artist", "Alice", 0), Tag::new("title", "A", 0)],
        )
        .unwrap();
    }
    let db = musefs_db::Db::open(&db_path).unwrap();
    let mut fs = Musefs::open(db, config()).unwrap();
    assert!(fs.lookup(VirtualTree::ROOT, "Alice").is_some());
    assert!(fs.lookup(VirtualTree::ROOT, "Bob").is_none());

    // A separate connection adds a track (as beets/picard would).
    {
        let db2 = musefs_db::Db::open(&db_path).unwrap();
        let id = db2
            .upsert_track(&NewTrack {
                backing_path: "/x/b.flac".to_string(),
                format: Format::Flac,
                audio_offset: 0,
                audio_length: 0,
                backing_size: 0,
                backing_mtime: 0,
            })
            .unwrap();
        db2.replace_tags(
            id,
            &[Tag::new("artist", "Bob", 0), Tag::new("title", "B", 0)],
        )
        .unwrap();
    }

    // Polling notices the external commit and rebuilds the tree.
    assert!(fs.poll_refresh().unwrap());
    assert!(fs.lookup(VirtualTree::ROOT, "Bob").is_some());
    // A second poll with no further change is a no-op.
    assert!(!fs.poll_refresh().unwrap());
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p musefs-core --test facade poll_refresh`
Expected: FAIL — no method `poll_refresh`.

- [ ] **Step 3: Implement `poll_refresh` + `HeaderCache::clear`**

In `musefs-core/src/reader.rs`, add a `clear` method inside `impl HeaderCache`:

```rust
    /// Drop all cached resolutions (used when the DB changed underneath the mount).
    pub fn clear(&mut self) {
        self.map.clear();
    }
```

In `musefs-core/src/facade.rs`, add a `last_data_version` field to `Musefs`:

```rust
pub struct Musefs {
    db: Db,
    config: MountConfig,
    tree: VirtualTree,
    cache: HeaderCache,
    last_data_version: i64,
}
```

In `Musefs::open`, initialize it (after building the tree):

```rust
    pub fn open(db: Db, config: MountConfig) -> Result<Musefs> {
        let tree = Self::build_tree(&db, &config)?;
        let last_data_version = db.data_version()?;
        Ok(Musefs {
            cache: HeaderCache::new(config.mode),
            last_data_version,
            db,
            config,
            tree,
        })
    }
```

Add the `poll_refresh` method to `impl Musefs` (e.g. next to `refresh`):

```rust
    /// Cheap check for external DB commits via `PRAGMA data_version`. On a change,
    /// rebuild the tree and drop cached resolutions, then return `true`. The FUSE
    /// layer calls this on metadata operations so external edits (a scan, a beets
    /// retag) appear without remounting.
    pub fn poll_refresh(&mut self) -> Result<bool> {
        let version = self.db.data_version()?;
        if version == self.last_data_version {
            return Ok(false);
        }
        self.last_data_version = version;
        self.tree = Self::build_tree(&self.db, &self.config)?;
        self.cache.clear();
        Ok(true)
    }
```

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test -p musefs-core --test facade poll_refresh`
Expected: PASS.

- [ ] **Step 5: Confirm zero warnings + fmt, then commit**

Run: `cargo clippy -p musefs-core --all-targets 2>&1 | grep -iE "warning|error" || echo clean` (expected `clean`).
Run: `cargo fmt --all` then `cargo fmt --all -- --check`.

```bash
git add musefs-core/src/facade.rs musefs-core/src/reader.rs musefs-core/tests/facade.rs
git commit -m "$(printf 'feat(core): poll_refresh rebuilds the tree on external DB edits\n\nCo-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>')"
```

---

## Task 4: `musefs-fuse` — poll for refresh on metadata operations

**Files:**
- Modify: `musefs-fuse/src/lib.rs`

- [ ] **Step 1: Add best-effort `poll_refresh` calls**

The FUSE `Filesystem` impl in `musefs-fuse/src/lib.rs` has `lookup`, `getattr`, and `readdir` methods that call `self.core.<op>(...)`. At the very start of each of those three methods (before the existing logic), add a best-effort refresh poll:

```rust
        let _ = self.core.poll_refresh();
```

Place it as the first statement in `lookup`, `getattr`, and `readdir`. Do NOT add it to `read` (reads stream from an already-resolved file; refreshing mid-stream would be wrong, and new opens pick up changes). The result is intentionally ignored: a transient poll failure must not fail an otherwise-serviceable VFS call (a genuinely broken DB surfaces as an error from the operation itself).

Note: `poll_refresh` takes `&mut self` on the core; the fuser trait methods already take `&mut self`, so `self.core.poll_refresh()` is fine, and it sequences before the subsequent `&self`/`&mut self` core call.

- [ ] **Step 2: Verify it builds and the default + gated suites pass**

Run: `cargo build -p musefs-fuse 2>&1 | tail -3` (expected: builds).
Run: `cargo test -p musefs-fuse 2>&1 | tail -5` (expected: unit tests pass; the mount test is ignored).
Run: `cargo test -p musefs-fuse -- --ignored 2>&1 | grep -E "end_to_end.*ok"` (expected: the gated mount test still passes — `poll_refresh` on a stable DB returns `false`, a no-op).

(There is no fine-grained unit test for the FUSE wiring itself — the polling logic is unit-tested in `musefs-core` (Task 3) and the gated mount test exercises the integration. The change here is three identical one-line calls.)

- [ ] **Step 3: Confirm zero warnings + fmt, then commit**

Run: `cargo clippy -p musefs-fuse --all-targets 2>&1 | grep -iE "warning|error" || echo clean` (expected `clean`).
Run: `cargo fmt --all` then `cargo fmt --all -- --check`.

```bash
git add musefs-fuse/src/lib.rs
git commit -m "$(printf 'feat(fuse): poll for external DB refresh on metadata ops\n\nCo-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>')"
```

---

## Task 5: `musefs-core` — `scan --revalidate` (skip-unchanged + prune + GC)

**Files:**
- Modify: `musefs-core/src/scan.rs`, `musefs-core/src/lib.rs`
- Test: `musefs-core/tests/scan.rs`

- [ ] **Step 1: Write the failing test**

Append to `musefs-core/tests/scan.rs` (reuses the existing `mod common` + `make_flac`/`streaminfo_body`/`vorbis_comment_body` helpers):

```rust
#[test]
fn revalidate_skips_unchanged_prunes_missing_and_gcs_art() {
    use musefs_core::revalidate;

    let dir = tempfile::tempdir().unwrap();
    let a = make_flac(
        &[(0, streaminfo_body()), (4, vorbis_comment_body("v", &["TITLE=A"]))],
        &[0xAA; 30],
    );
    std::fs::write(dir.path().join("a.flac"), &a).unwrap();
    let gone = make_flac(
        &[(0, streaminfo_body()), (4, vorbis_comment_body("v", &["TITLE=G"]))],
        &[0xBB; 30],
    );
    std::fs::write(dir.path().join("gone.flac"), &gone).unwrap();

    let db = Db::open_in_memory().unwrap();
    scan_directory(&db, dir.path()).unwrap();
    assert_eq!(db.list_tracks().unwrap().len(), 2);

    // An external edit to a's tags that a revalidate must NOT clobber (the file is
    // unchanged on disk, so revalidate should skip re-reading it).
    let a_id = db
        .list_tracks()
        .unwrap()
        .into_iter()
        .find(|t| t.backing_path.ends_with("a.flac"))
        .unwrap()
        .id;
    db.replace_tags(a_id, &[Tag::new("title", "Edited", 0)]).unwrap();

    // Delete gone.flac from disk so revalidate prunes its track.
    std::fs::remove_file(dir.path().join("gone.flac")).unwrap();

    let stats = revalidate(&db, dir.path()).unwrap();
    assert_eq!(stats.unchanged, 1); // a.flac (size+mtime match) is skipped
    assert_eq!(stats.pruned, 1); // gone.flac's track is removed

    let tracks = db.list_tracks().unwrap();
    assert_eq!(tracks.len(), 1);
    // The skipped file kept its externally-edited tag (not re-seeded from disk).
    let tags = db.get_tags(tracks[0].id).unwrap();
    assert!(tags.iter().any(|t| t.key == "title" && t.value == "Edited"));
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p musefs-core --test scan revalidate`
Expected: FAIL — `revalidate` not found in crate.

- [ ] **Step 3: Refactor an `ingest` helper and implement `revalidate`**

In `musefs-core/src/scan.rs`, extract the per-file ingest (currently inline in `scan_directory`) into a helper, then add `revalidate` + `RevalidateStats`.

First, add imports/the stats struct near the top (after `ScanStats`):

```rust
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RevalidateStats {
    pub updated: u64,
    pub unchanged: u64,
    pub pruned: u64,
}
```

Add the shared ingest helper (the body currently inside `scan_directory`'s loop, after `probe`):

```rust
/// Upsert a track from a probed backing file: write the track row, replace its
/// seeded tags, and ingest its embedded art (capped, deduped, clamped).
fn ingest(db: &Db, abs_path: &str, meta: &std::fs::Metadata, probed: Probed) -> Result<()> {
    let track_id = db.upsert_track(&NewTrack {
        backing_path: abs_path.to_string(),
        format: probed.format,
        audio_offset: probed.audio_offset as i64,
        audio_length: probed.audio_length as i64,
        backing_size: meta.len() as i64,
        backing_mtime: mtime_secs(meta),
    })?;

    let mut tags = Vec::new();
    let mut ordinals: HashMap<String, i64> = HashMap::new();
    for (field, value) in probed.tags {
        let key = field.to_lowercase();
        let ord = ordinals.entry(key.clone()).or_insert(0);
        tags.push(Tag::new(&key, &value, *ord));
        *ord += 1;
    }
    db.replace_tags(track_id, &tags)?;

    let mut track_arts = Vec::new();
    let accepted = probed
        .pictures
        .into_iter()
        .filter(|p| p.data.len() <= MAX_ART_BYTES);
    for (ordinal, pic) in accepted.enumerate() {
        let art_id = db.upsert_art(&NewArt {
            mime: pic.mime,
            width: (pic.width != 0).then_some(pic.width as i64),
            height: (pic.height != 0).then_some(pic.height as i64),
            data: pic.data,
        })?;
        let picture_type = if pic.picture_type <= 20 {
            pic.picture_type as i64
        } else {
            0
        };
        track_arts.push(TrackArt {
            art_id,
            picture_type,
            description: pic.description,
            ordinal: ordinal as i64,
        });
    }
    db.set_track_art(track_id, &track_arts)?;
    Ok(())
}
```

Now rewrite `scan_directory`'s loop body to call `ingest` (replacing the inline upsert/tag/art code). The loop becomes:

```rust
    for path in files {
        let bytes = std::fs::read(&path)?;
        let probed = match probe(&path, &bytes) {
            Some(p) => p,
            None => {
                stats.skipped += 1;
                continue;
            }
        };
        let meta = std::fs::metadata(&path)?;
        let abs = std::fs::canonicalize(&path)?;
        ingest(db, &abs.to_string_lossy(), &meta, probed)?;
        stats.scanned += 1;
    }
```

(Everything else in `scan_directory` — `collect_audio`, the `ScanStats` init, the return — stays.)

Add `revalidate` at the end of the file:

```rust
/// Re-validate an already-scanned library: re-probe only files whose size/mtime
/// changed since the last scan (skipping unchanged ones so external tag edits in
/// the DB are preserved), delete tracks whose backing file is gone (cascading
/// tags/art links), and garbage-collect now-unreferenced art.
pub fn revalidate(db: &Db, root: &Path) -> Result<RevalidateStats> {
    let mut files = Vec::new();
    collect_audio(root, &mut files)?;

    let mut stats = RevalidateStats {
        updated: 0,
        unchanged: 0,
        pruned: 0,
    };
    for path in files {
        let meta = std::fs::metadata(&path)?;
        let abs = std::fs::canonicalize(&path)?;
        let abs_str = abs.to_string_lossy().to_string();

        if let Some(existing) = db.get_track_by_path(&abs_str)? {
            if existing.backing_size == meta.len() as i64
                && existing.backing_mtime == mtime_secs(&meta)
            {
                stats.unchanged += 1;
                continue;
            }
        }

        let bytes = std::fs::read(&path)?;
        if let Some(probed) = probe(&path, &bytes) {
            ingest(db, &abs_str, &meta, probed)?;
            stats.updated += 1;
        }
    }

    // Prune tracks whose backing file no longer exists (anywhere, not just `root`).
    for track in db.list_tracks()? {
        if std::fs::metadata(&track.backing_path).is_err() {
            db.delete_track(track.id)?;
            stats.pruned += 1;
        }
    }
    db.gc_orphan_art()?;

    Ok(stats)
}
```

In `musefs-core/src/lib.rs`, extend the scan re-export to include `revalidate` and `RevalidateStats`:

```rust
pub use scan::{revalidate, scan_directory, RevalidateStats, ScanStats};
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p musefs-core --test scan`
Expected: PASS — the new revalidate test plus all existing scan tests (the `ingest` refactor preserves `scan_directory` behavior exactly).

- [ ] **Step 5: Confirm zero warnings + fmt, then commit**

Run: `cargo clippy -p musefs-core --all-targets 2>&1 | grep -iE "warning|error" || echo clean` (expected `clean`).
Run: `cargo fmt --all` then `cargo fmt --all -- --check`.

```bash
git add musefs-core/src/scan.rs musefs-core/src/lib.rs musefs-core/tests/scan.rs
git commit -m "$(printf 'feat(core): revalidate skips unchanged, prunes missing, GCs art\n\nCo-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>')"
```

---

## Task 6: `musefs-cli` — `--mode` on mount, `--revalidate` on scan

**Files:**
- Modify: `musefs-cli/src/lib.rs`
- Test: `musefs-cli/tests/cli.rs`

- [ ] **Step 1: Write the failing test**

Append to `musefs-cli/tests/cli.rs`:

```rust
#[test]
fn parses_mode_and_revalidate_flags() {
    use musefs_cli::CliMode;

    let cli = Cli::parse_from([
        "musefs",
        "mount",
        "/mnt/x",
        "--db",
        "/tmp/m.db",
        "--mode",
        "structure-only",
    ]);
    match cli.command {
        Command::Mount { mode, .. } => assert_eq!(mode, CliMode::StructureOnly),
        _ => panic!("expected mount"),
    }

    // Mode defaults to synthesis.
    let cli = Cli::parse_from(["musefs", "mount", "/mnt/x", "--db", "/tmp/m.db"]);
    match cli.command {
        Command::Mount { mode, .. } => assert_eq!(mode, CliMode::Synthesis),
        _ => panic!("expected mount"),
    }

    // Scan --revalidate flag.
    let cli = Cli::parse_from(["musefs", "scan", "/music", "--db", "/tmp/m.db", "--revalidate"]);
    match cli.command {
        Command::Scan { revalidate, .. } => assert!(revalidate),
        _ => panic!("expected scan"),
    }
    let cli = Cli::parse_from(["musefs", "scan", "/music", "--db", "/tmp/m.db"]);
    match cli.command {
        Command::Scan { revalidate, .. } => assert!(!revalidate),
        _ => panic!("expected scan"),
    }
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p musefs-cli --test cli parses_mode`
Expected: FAIL — `CliMode` not found / `Command::Mount` has no `mode` / `Command::Scan` has no `revalidate`.

- [ ] **Step 3: Add `CliMode`, the flags, and wire them**

First, update the top-of-file import: `run_scan` will no longer return `ScanStats` (it prints its own summary), so drop the now-unused name to keep the clippy gate clean — change `use musefs_core::{MountConfig, Musefs, ScanStats};` to:

```rust
use musefs_core::{MountConfig, Musefs};
```

In `musefs-cli/src/lib.rs`, add a clap value enum (near the top, after the imports) that maps to `musefs_core::Mode`:

```rust
/// Mount content mode (CLI surface for `musefs_core::Mode`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum CliMode {
    /// Synthesize a fresh metadata region in front of the audio (default).
    Synthesis,
    /// Serve the original backing file bytes unchanged.
    StructureOnly,
}

impl From<CliMode> for musefs_core::Mode {
    fn from(m: CliMode) -> musefs_core::Mode {
        match m {
            CliMode::Synthesis => musefs_core::Mode::Synthesis,
            CliMode::StructureOnly => musefs_core::Mode::StructureOnly,
        }
    }
}
```

Add `revalidate` to the `Scan` variant and `mode` to the `Mount` variant of `Command`:

```rust
    /// Walk a backing directory, ingesting FLAC/MP3 files into the SQLite store.
    Scan {
        /// Directory of backing audio files to scan recursively.
        backing_dir: PathBuf,
        /// Path to the SQLite database (created if absent).
        #[arg(long)]
        db: PathBuf,
        /// Re-validate: skip unchanged files, prune tracks whose backing file is
        /// gone, and garbage-collect orphaned art.
        #[arg(long)]
        revalidate: bool,
    },
    /// Mount a read-only FUSE view of the store.
    Mount {
        /// Empty directory to mount at.
        mountpoint: PathBuf,
        /// Path to the SQLite database.
        #[arg(long)]
        db: PathBuf,
        /// Path template, e.g. "$albumartist/$album/$title".
        #[arg(long, default_value = "$artist/$title")]
        template: String,
        /// Fallback value substituted for any missing template field.
        #[arg(long, default_value = "Unknown")]
        default_fallback: String,
        /// How file contents are served.
        #[arg(long, value_enum, default_value_t = CliMode::Synthesis)]
        mode: CliMode,
    },
```

Update `run_scan` to take the flag and dispatch:

```rust
/// Open (creating/migrating) the DB at `db_path` and scan `backing_dir`. With
/// `revalidate`, run the maintenance pass (skip-unchanged, prune, GC) instead of
/// a full ingest.
pub fn run_scan(db_path: &Path, backing_dir: &Path, revalidate: bool) -> Result<()> {
    let db =
        Db::open(db_path).with_context(|| format!("opening database at {}", db_path.display()))?;
    if revalidate {
        let stats = musefs_core::revalidate(&db, backing_dir)
            .with_context(|| format!("revalidating {}", backing_dir.display()))?;
        println!(
            "revalidated: {} updated, {} unchanged, {} pruned",
            stats.updated, stats.unchanged, stats.pruned
        );
    } else {
        let stats = musefs_core::scan_directory(&db, backing_dir)
            .with_context(|| format!("scanning {}", backing_dir.display()))?;
        println!(
            "scanned {} file(s), skipped {}",
            stats.scanned, stats.skipped
        );
    }
    Ok(())
}
```

Update `run_mount` to take and apply the mode (replacing the Task-2 placeholder `mode: musefs_core::Mode::Synthesis`):

```rust
/// Build a `Musefs` from the DB at `db_path` and mount it (blocking) at
/// `mountpoint`.
pub fn run_mount(
    db_path: &Path,
    mountpoint: &Path,
    template: String,
    default_fallback: String,
    mode: musefs_core::Mode,
) -> Result<()> {
    let db =
        Db::open(db_path).with_context(|| format!("opening database at {}", db_path.display()))?;
    let config = MountConfig {
        template,
        fallbacks: BTreeMap::new(),
        default_fallback,
        mode,
    };
    let core = Musefs::open(db, config).context("building the virtual filesystem")?;
    musefs_fuse::mount(core, mountpoint, "musefs")
        .with_context(|| format!("mounting at {}", mountpoint.display()))?;
    Ok(())
}
```

Update `run`'s dispatch to pass the new args:

```rust
pub fn run(cli: Cli) -> Result<()> {
    match cli.command {
        Command::Scan {
            backing_dir,
            db,
            revalidate,
        } => run_scan(&db, &backing_dir, revalidate),
        Command::Mount {
            mountpoint,
            db,
            template,
            default_fallback,
            mode,
        } => run_mount(&db, &mountpoint, template, default_fallback, mode.into()),
    }
}
```

Note: `run_scan` now returns `Result<()>` (was `Result<ScanStats>`) and prints its own summary. The old `tests/scan.rs` integration test in `musefs-cli` asserted `run_scan(...).unwrap()` returned stats — update that test to the new signature: call `run_scan(&db_path, backing.path(), false).unwrap();` and then assert against the DB directly (open the DB and check `list_tracks()`), since the count is no longer returned. (Keep the rest of that test.)

Also update the **existing** `parses_scan_and_mount_invocations` test in `tests/cli.rs`: its struct patterns are exhaustive (`Command::Scan { backing_dir, db }` and `Command::Mount { mountpoint, db, template, default_fallback }`), so adding the `revalidate`/`mode` fields turns them into non-exhaustive patterns (compile error E0027 — "pattern does not mention field"). Add the new fields to each pattern. The simplest fix is a trailing `..`:

```rust
        Command::Scan { backing_dir, db, .. } => { /* existing asserts */ }
```
```rust
        Command::Mount { mountpoint, db, template, default_fallback, .. } => { /* existing asserts */ }
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p musefs-cli`
Expected: PASS (the parsing test, the updated scan integration test, and the existing cli test).
Run: `cargo run -p musefs-cli -- mount --help 2>&1 | grep -iE "mode|structure-only"` (expected: the `--mode` option with `synthesis`/`structure-only` is listed).

- [ ] **Step 5: Confirm zero warnings + fmt, then commit**

Run: `cargo clippy -p musefs-cli --all-targets 2>&1 | grep -iE "warning|error" || echo clean` (expected `clean`).
Run: `cargo fmt --all` then `cargo fmt --all -- --check`.

```bash
git add musefs-cli/src/lib.rs musefs-cli/tests/cli.rs musefs-cli/tests/scan.rs
git commit -m "$(printf 'feat(cli): --mode on mount and --revalidate on scan\n\nCo-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>')"
```

---

## Task 7: Whole-workspace verification + manual end-to-end

**Files:** none (verification only).

- [ ] **Step 1: Run the entire workspace test suite**

Run: `cargo test`
Expected: PASS across all crates (db delete/GC, core structure-only/poll_refresh/revalidate, fuse, cli).

- [ ] **Step 2: Run the gated FUSE mount test (regression)**

Run: `cargo test -p musefs-fuse -- --ignored`
Expected: PASS (`end_to_end_read_through_mount`).

- [ ] **Step 3: Confirm a clean, warning-free, formatted workspace**

Run: `cargo clippy --workspace --all-targets 2>&1 | grep -iE "warning|error" || echo "clean"` (expected `clean`).
Run: `cargo fmt --all -- --check && echo "fmt clean"` (expected `fmt clean`).

- [ ] **Step 4: Manual end-to-end smoke (optional, real binary + mount)**

```bash
cargo build -p musefs-cli
BACK=$(mktemp -d); DB=$(mktemp -u --suffix=.db); MNT=$(mktemp -d)
# place a few real .flac/.mp3 files in $BACK
./target/debug/musefs scan "$BACK" --db "$DB"

# structure-only mount: files are the originals, reorganized by the template.
./target/debug/musefs mount "$MNT" --mode structure-only --db "$DB" --template '$artist/$title' &
sleep 1; ls -R "$MNT"; cmp <(cat "$MNT"/*/*.flac | head -c 4) <(printf 'fLaC')  # passthrough
fusermount3 -u "$MNT"

# auto-refresh: mount, then edit the DB from another process; the tree updates.
./target/debug/musefs mount "$MNT" --db "$DB" --template '$artist/$title' &
sleep 1
# (in the backing dir: delete a file, then) ./target/debug/musefs scan "$BACK" --db "$DB" --revalidate
ls -R "$MNT"   # reflects the pruned/added tracks without remounting
fusermount3 -u "$MNT"
```

Expected: structure-only serves byte-identical originals under the templated tree; after an external `scan`/`revalidate`, a fresh `ls` of the live mount reflects the change. Documentation for the operator; not automated.

- [ ] **Step 5: Commit any cleanup**

This task changes no source files, so there is normally nothing to commit. If a prior task left an uncommitted fmt/clippy fix, stage **only** those specific files by explicit path (never `git add -A` — the untracked `.serena/` directory must not be committed) and commit:

```bash
git status --short   # confirm exactly what changed
# git add <explicit/path/that/changed> ...
git commit -m "$(printf 'chore: M5 cleanup, no warnings\n\nCo-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>')" || echo "nothing to commit"
```

---

## Self-Review Notes

- **Spec coverage (M5 scope):** structure-only passthrough mode (Tasks 2, 6 — `Mode::StructureOnly` resolves to a single whole-file `BackingAudio` segment, `st_size = backing_size`, `read = pread`); `data_version` polling + automatic refresh (Tasks 3, 4 — `poll_refresh` rebuilds the tree on external commits, called by the FUSE metadata ops); `scan --revalidate` (Task 5 — skip-unchanged via stored stamps, prune missing-backing tracks, GC orphaned art); plus the carried-forward orphan-art GC and the new `delete_track`/`gc_orphan_art` DB primitives (Task 1). Completes the MVP.
- **Correctly deferred:** the explicit `musefs refresh <mountpoint>` SIGHUP command (auto-refresh via polling covers external edits — confirmed decision; signal handling in fuser's blocking session loop isn't worth the complexity for MVP); writable mount / path overrides, beets/picard plugins, and Ogg/Opus/MP4 (all post-MVP per the spec).
- **Skip-unchanged preserves external edits:** a plain `scan` re-reads every file and re-seeds tags (clobbering DB-side edits for unchanged files); `revalidate` skips files whose size+mtime are unchanged, so a beets/picard retag held only in the DB survives a maintenance pass. The new test asserts exactly this.
- **Type consistency:** `Mode { Synthesis, StructureOnly }` (Copy) added to `MountConfig` and threaded into `HeaderCache::new(Mode)`; `Musefs { ..., last_data_version }` + `poll_refresh() -> Result<bool>`; `HeaderCache::clear()`; `Db::delete_track(i64) -> Result<()>` and `Db::gc_orphan_art() -> Result<usize>`; `revalidate(&Db, &Path) -> Result<RevalidateStats>` with `RevalidateStats { updated, unchanged, pruned }`; the extracted `ingest(&Db, &str, &Metadata, Probed) -> Result<()>` is shared by `scan_directory` and `revalidate`; `CliMode { Synthesis, StructureOnly }` (clap `ValueEnum`) maps to `musefs_core::Mode`. `run_scan` gains a `revalidate: bool` and now returns `Result<()>` (prints its own summary); `run_mount` gains a `mode: musefs_core::Mode`.
- **Placeholder discipline:** Task 2 sets `MountConfig.mode` literals to `Mode::Synthesis` at every existing construction site so the workspace compiles after that task; Task 6 replaces the CLI one with the real flag. Every code step ships complete, compilable code.
- **Borrow/threading notes:** `poll_refresh` is `&mut self`; the FUSE ops are `&mut self`, so the leading `let _ = self.core.poll_refresh();` sequences cleanly before the operation. Resolve still validates backing size/mtime and audio bounds *before* the mode branch, so structure-only also rejects a changed backing file with `BackingChanged`.
```
