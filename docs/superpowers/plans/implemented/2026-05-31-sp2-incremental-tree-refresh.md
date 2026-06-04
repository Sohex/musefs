# SP2 — Incremental Tree Refresh Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make `Musefs::poll_refresh` cost scale with the number of *changed* tracks instead of doing a full O(library) `VirtualTree` rebuild on every `PRAGMA data_version` bump — with zero observable behavior change (the refreshed tree must be structurally identical, inodes included, to a full `VirtualTree::build_with` over the same DB state).

**Architecture:** Two stages on top of the existing refresh skeleton (`facade.rs` `poll_refresh_notify` + single-flight + debounce). **Stage A** adds an in-memory identity diff on the render key `(content_version, format)` (`Db::list_render_keys`) and renders only `changed ∪ added` tracks (`Db::tags_for_tracks`), reusing cached rendered paths for everything else, then calls the *unchanged* `VirtualTree::build_with` — so Stage A equivalence is trivial. **Stage B** migrates `VirtualTree`'s internals to `im` persistent maps (O(1) clone-on-publish behind the existing `ArcSwap`) and replaces `build_with` in the incremental path with in-place mutation + canonical-order ("introducing id") re-disambiguation, with a full-`build_with` fallback safety valve. The headline gate is a property test: incremental tree ≡ `build_with(current_entries, allocator.clone())`.

**Tech Stack:** Rust workspace (`musefs-db`, `musefs-core`); `rusqlite` (SQLite); `arc_swap`; `im` (new dep, Stage B); `proptest` (already used for property tests); the spec is `docs/superpowers/specs/2026-05-30-optimization-pass/SP2-incremental-tree-refresh.md`.

**Key facts pinned from the current code (do not re-derive):**
- `Track.format` is a **`Copy` enum `musefs_db::Format`** (not a `String`), with `fn as_str(self) -> &'static str` and `fn parse(s: &str) -> Option<Format>`. Use `Format` directly in the render key — it is `Copy + Eq`.
- `Track { id: i64, backing_path: String, format: Format, audio_offset: i64, audio_length: i64, backing_size: i64, backing_mtime: i64, content_version: i64, updated_at: i64 }`.
- `Tag { key: String, value: String, ordinal: i64 }` with `Tag::new(key, value, ordinal)`.
- `tags_to_fields(&[Tag]) -> BTreeMap<String,String>` is `pub(crate)` in `mapping.rs`; first value per **lowercased** key wins (relies on rows ordered by `key, ordinal`).
- `render_path(template, fields, fallbacks, default_fallback, ext: &str) -> String` is `pub` in `template.rs`.
- `facade.rs` is 559 lines. The field to replace is `versions: Mutex<HashMap<i64, i64>>` (facade.rs:134). `build_tree` is facade.rs:164-187; `rebuild` 208-223; `refresh` 197-204; `poll_refresh_notify` 260-359; `open` 139-162.
- `HeaderCache::retain(&self, live: &HashSet<i64>)` (reader.rs:195).
- `VirtualTree` internals (tree.rs:48-52): `nodes: HashMap<u64,Node>`, `children: HashMap<u64, BTreeMap<String,u64>>`, `track_to_inode: HashMap<i64,u64>`. `InodeAllocator { paths: HashMap<String,u64>, next: u64 }` with private `intern(&mut self, &str) -> u64`.

**Commands (run from repo root `/home/cfutro/git/musefs`):**
- Build a crate: `cargo build -p musefs-core`
- Test one: `cargo test -p musefs-core <substr> -- --nocapture`
- DB tests: `cargo test -p musefs-db <substr>`
- Lint/format before each commit: `cargo clippy --all-targets` and `cargo fmt`
- Full gate: `cargo test` (excludes the `#[ignore]` FUSE e2e)

---

## File Structure

**Stage A**
- `musefs-db/src/tracks.rs` — add `Db::list_render_keys() -> Result<Vec<(i64, i64, Format)>>`.
- `musefs-db/src/tags.rs` — add `Db::tags_for_tracks(&[i64]) -> Result<HashMap<i64, Vec<Tag>>>` (chunked `IN (…)`).
- `musefs-core/src/facade.rs` — replace `versions` with a `TrackRenderState` snapshot; add `render_one` helper, `build_full` (full render → tree + snapshot), `rebuild_incremental` (diff + changed-only render → tree + snapshot + change partition); rewire `open`/`refresh`/`poll_refresh_notify`.
- `musefs-core/src/refresh_diff.rs` *(new)* — the pure diff function `partition_changes(prev: &HashMap<i64, TrackRenderState>, scan: &[(i64,i64,Format)]) -> ChangeSet` and the `ChangeSet`/`TrackRenderState` types, unit-tested in isolation.
- `musefs-core/tests/incremental_refresh.rs` *(new)* — the Stage A equivalence + change-detection integration tests.

**Stage B**
- `musefs-core/Cargo.toml` — add `im` dependency.
- `musefs-core/src/tree.rs` — migrate internals to `im` maps; add mutation methods (`remove_track`, `insert_track`, `redisambiguate_dir`, introducing-id recompute) and a `clone`-friendly publish path; keep the public API.
- `musefs-core/src/facade.rs` — swap `rebuild_incremental`'s `build_with` call for the in-place mutation path + fallback.
- `musefs-core/tests/incremental_refresh.rs` — extend with the Stage B property oracle (random edit sequences) + dir-vs-file + sibling-subdir-reorder + fallback cases.

**Measurement (end of each stage)**
- `musefs-core/tests/bench_refresh.rs` — add a library-size sweep and an identity-scan-cost row.

---

## Test corpus helper (used by every integration test below)

There is **no** `write_min_corpus` in `tests/common/`. The verified primitive is
`prepare(&CorpusParams::single(fmt, albums, tracks_per_album))`, which generates a
corpus under a tempdir and returns a `Target { corpus_dir: PathBuf, db_path:
PathBuf, is_real_library: bool, .. }` that **owns the tempdir** (so the returned
`Target` must stay in scope for the test's duration). Define this helper once at
the top of `musefs-core/tests/incremental_refresh.rs`:

```rust
use common::corpus::{prepare, CorpusParams, Target};
use musefs_db::Format;

/// A small single-album FLAC corpus with `n` tracks. The returned `Target` owns
/// the tempdir — keep it alive for the whole test. Generated FLACs carry minimal
/// tags, so under `$artist/$album/$title` many render to the same fallback path
/// (`Unknown/Unknown/Unknown.flac`) — which deliberately exercises disambiguation.
fn small_corpus(n: usize) -> Target {
    prepare(&CorpusParams::single(Format::Flac, 1, n))
}
```

**Substitution rule for this plan:** wherever a task below writes
`common::corpus::write_min_corpus(&corpus, N)` and then uses `corpus`/`db_path`,
instead write:

```rust
let target = small_corpus(N);              // keep `target` alive
let db_path = target.db_path.clone();
let corpus = target.corpus_dir.clone();
```

`proptest = "1"` is already in `musefs-core/Cargo.toml` `[dev-dependencies]` and
`Db::open_in_memory()` / `Db::delete_track(id)` exist — no need to add them.

---

# STAGE A — change detection + changed-only render

## Task A1: `Db::list_render_keys` (the cheap identity scan)

**Files:**
- Modify: `musefs-db/src/tracks.rs` (add method on `impl Db`)
- Test: `musefs-db/src/tracks.rs` (in the file's `#[cfg(test)] mod tests`, or add one)

- [ ] **Step 1: Write the failing test**

Add to `musefs-db/src/tracks.rs` test module (mirror the existing track tests — open an in-memory or temp `Db`, insert tracks + tags, assert). If no test module exists in this file, add `#[cfg(test)] mod render_key_tests { use super::*; ... }` at the bottom.

```rust
#[cfg(test)]
mod render_key_tests {
    use super::*;
    use crate::{Format, NewTrack, Tag};

    fn open_mem() -> Db {
        Db::open_in_memory().unwrap()
    }

    fn new_track(path: &str, fmt: Format) -> NewTrack {
        NewTrack {
            backing_path: path.to_string(),
            format: fmt,
            audio_offset: 0,
            audio_length: 1,
            backing_size: 1,
            backing_mtime: 0,
        }
    }

    #[test]
    fn list_render_keys_returns_id_version_format_sorted_by_id() {
        let db = open_mem();
        let a = db.upsert_track(&new_track("/a.flac", Format::Flac)).unwrap();
        let b = db.upsert_track(&new_track("/b.mp3", Format::Mp3)).unwrap();
        // Bump a's content_version via a tag write (trigger).
        db.replace_tags(a, &[Tag::new("TITLE", "x", 0)]).unwrap();

        let keys = db.list_render_keys().unwrap();
        assert_eq!(keys.len(), 2);
        // Sorted by id ascending.
        assert_eq!(keys[0].0, a);
        assert_eq!(keys[1].0, b);
        // a's content_version rose above 0 after the tag write; b's stayed 0.
        assert!(keys[0].1 >= 1, "a content_version should have risen");
        assert_eq!(keys[1].1, 0, "b content_version untouched");
        assert_eq!(keys[0].2, Format::Flac);
        assert_eq!(keys[1].2, Format::Mp3);
    }
}
```

> If `Db::open_in_memory` does not exist, use the same constructor the other tests in this file use (check the top of `tracks.rs`'s existing test module for the helper, e.g. `Db::open(":memory:")` or a tempfile). Match the file's existing pattern exactly.

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p musefs-db list_render_keys_returns -- --nocapture`
Expected: FAIL — `no method named list_render_keys`.

- [ ] **Step 3: Write minimal implementation**

Add to `impl Db` in `musefs-db/src/tracks.rs` (near `list_tracks`). Note `Format` is already in scope via `crate::models` (the file already references `Format` in `row_to_track`).

```rust
/// Cheap render-key identity scan for incremental refresh: `(id, content_version,
/// format)` for every track, ordered by id. No tags, no path columns — just the
/// two track-level inputs that determine a rendered path. See SP2 Component 1.
pub fn list_render_keys(&self) -> Result<Vec<(i64, i64, Format)>> {
    let mut stmt = self
        .conn
        .prepare("SELECT id, content_version, format FROM tracks ORDER BY id")?;
    let rows = stmt.query_map([], |r| {
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
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p musefs-db list_render_keys_returns -- --nocapture`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
cargo fmt && cargo clippy -p musefs-db --all-targets
git add musefs-db/src/tracks.rs
git commit -m "feat(db): add list_render_keys for incremental refresh diff (SP2 A1)"
```

---

## Task A2: `Db::tags_for_tracks` (batched, chunked, ordered)

**Files:**
- Modify: `musefs-db/src/tags.rs`
- Test: `musefs-db/src/tags.rs` test module

- [ ] **Step 1: Write the failing test**

```rust
#[cfg(test)]
mod tags_for_tracks_tests {
    use super::*;
    use crate::{Format, NewTrack, Tag};

    fn open_mem() -> Db {
        Db::open_in_memory().unwrap()
    }
    fn new_track(path: &str) -> NewTrack {
        NewTrack { backing_path: path.into(), format: Format::Flac,
            audio_offset: 0, audio_length: 1, backing_size: 1, backing_mtime: 0 }
    }

    #[test]
    fn tags_for_tracks_returns_only_requested_ordered_by_key_ordinal() {
        let db = open_mem();
        let a = db.upsert_track(&new_track("/a.flac")).unwrap();
        let b = db.upsert_track(&new_track("/b.flac")).unwrap();
        let c = db.upsert_track(&new_track("/c.flac")).unwrap();
        // Multi-value key on `a`, inserted out of ordinal order to prove ORDER BY.
        db.replace_tags(a, &[
            Tag::new("ARTIST", "second", 1),
            Tag::new("ARTIST", "first", 0),
        ]).unwrap();
        db.replace_tags(b, &[Tag::new("ARTIST", "bee", 0)]).unwrap();
        db.replace_tags(c, &[Tag::new("ARTIST", "cee", 0)]).unwrap();

        let got = db.tags_for_tracks(&[a, b]).unwrap();
        assert_eq!(got.len(), 2, "c was not requested");
        assert!(!got.contains_key(&c));
        // a's ARTIST values come back lowest-ordinal first.
        let a_tags = &got[&a];
        assert_eq!(a_tags[0].value, "first");
        assert_eq!(a_tags[1].value, "second");
    }

    #[test]
    fn tags_for_tracks_chunks_beyond_sqlite_variable_limit() {
        let db = open_mem();
        let mut ids = Vec::new();
        for i in 0..1500 {
            let id = db.upsert_track(&new_track(&format!("/t{i}.flac"))).unwrap();
            db.replace_tags(id, &[Tag::new("TITLE", &format!("t{i}"), 0)]).unwrap();
            ids.push(id);
        }
        let got = db.tags_for_tracks(&ids).unwrap();
        assert_eq!(got.len(), 1500, "all chunks fetched");
    }

    #[test]
    fn tags_for_tracks_empty_input_is_empty_map() {
        let db = open_mem();
        assert!(db.tags_for_tracks(&[]).unwrap().is_empty());
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p musefs-db tags_for_tracks -- --nocapture`
Expected: FAIL — `no method named tags_for_tracks`.

- [ ] **Step 3: Write minimal implementation**

Add to `impl Db` in `musefs-db/src/tags.rs`. SQLite's default `SQLITE_MAX_VARIABLE_NUMBER` is 999 on older builds; chunk at 900 to be safe. Order by `track_id, key, ordinal` to match `tags_grouped` and satisfy `tags_to_fields` (lowest-ordinal-first per key).

```rust
/// Tags for a specific set of track ids, grouped by track id, ordered within each
/// track by `key, ordinal` (same as `tags_grouped`, so `tags_to_fields` sees the
/// lowest-ordinal value of each key first). The `IN (…)` list is chunked to stay
/// under SQLite's bound-variable limit. Used by incremental refresh to render only
/// changed/added tracks. See SP2 Component 2.
pub fn tags_for_tracks(
    &self,
    track_ids: &[i64],
) -> Result<std::collections::HashMap<i64, Vec<Tag>>> {
    const CHUNK: usize = 900;
    let mut out: std::collections::HashMap<i64, Vec<Tag>> = std::collections::HashMap::new();
    for chunk in track_ids.chunks(CHUNK) {
        let placeholders = vec!["?"; chunk.len()].join(",");
        let sql = format!(
            "SELECT track_id, key, value, ordinal FROM tags \
             WHERE track_id IN ({placeholders}) ORDER BY track_id, key, ordinal"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let params = rusqlite::params_from_iter(chunk.iter());
        let rows = stmt.query_map(params, |r| {
            Ok((
                r.get::<_, i64>(0)?,
                Tag { key: r.get(1)?, value: r.get(2)?, ordinal: r.get(3)? },
            ))
        })?;
        for row in rows {
            let (track_id, tag) = row?;
            out.entry(track_id).or_default().push(tag);
        }
    }
    Ok(out)
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p musefs-db tags_for_tracks -- --nocapture`
Expected: PASS (all three).

- [ ] **Step 5: Commit**

```bash
cargo fmt && cargo clippy -p musefs-db --all-targets
git add musefs-db/src/tags.rs
git commit -m "feat(db): add tags_for_tracks (chunked, ordered) for incremental render (SP2 A2)"
```

---

## Task A3: `TrackRenderState` + the pure diff (`refresh_diff.rs`)

**Files:**
- Create: `musefs-core/src/refresh_diff.rs`
- Modify: `musefs-core/src/lib.rs` (add `mod refresh_diff;`)
- Test: inside `refresh_diff.rs`

- [ ] **Step 1: Write the failing test**

Create `musefs-core/src/refresh_diff.rs` with the types and a test, but leave `partition_changes` unimplemented (`todo!()`):

```rust
use std::collections::HashMap;

use musefs_db::Format;

/// Per-track state persisted between refreshes so unchanged tracks need no
/// re-render. `(content_version, format)` is the render key (the only track-level
/// inputs to `render_path`); `path` is the last rendered path, reused verbatim for
/// unchanged tracks. See SP2 Component 1/2.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TrackRenderState {
    pub content_version: i64,
    pub format: Format,
    pub path: String,
}

/// The result of diffing the previous snapshot against a fresh render-key scan.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct ChangeSet {
    /// In both, render key differs (must re-render).
    pub changed: Vec<i64>,
    /// New ids (must render).
    pub added: Vec<i64>,
    /// Ids gone from the scan (must drop).
    pub removed: Vec<i64>,
}

impl ChangeSet {
    pub fn is_empty(&self) -> bool {
        self.changed.is_empty() && self.added.is_empty() && self.removed.is_empty()
    }
}

/// Partition a fresh `(id, content_version, format)` scan against the previous
/// snapshot. `scan` is ordered by id (as `list_render_keys` returns it); outputs
/// are id-ascending so downstream rendering and tree assembly are deterministic.
pub(crate) fn partition_changes(
    prev: &HashMap<i64, TrackRenderState>,
    scan: &[(i64, i64, Format)],
) -> ChangeSet {
    todo!()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn st(cv: i64, fmt: Format, path: &str) -> TrackRenderState {
        TrackRenderState { content_version: cv, format: fmt, path: path.into() }
    }

    #[test]
    fn partitions_changed_added_removed() {
        let mut prev = HashMap::new();
        prev.insert(1, st(0, Format::Flac, "A/1.flac")); // unchanged
        prev.insert(2, st(0, Format::Flac, "A/2.flac")); // content changes
        prev.insert(3, st(0, Format::Flac, "A/3.flac")); // removed
        let scan = vec![
            (1, 0, Format::Flac),         // unchanged
            (2, 1, Format::Flac),         // content_version bumped -> changed
            (4, 0, Format::Flac),         // new -> added
        ];
        let cs = partition_changes(&prev, &scan);
        assert_eq!(cs.changed, vec![2]);
        assert_eq!(cs.added, vec![4]);
        assert_eq!(cs.removed, vec![3]);
    }

    #[test]
    fn format_only_change_is_changed() {
        let mut prev = HashMap::new();
        prev.insert(1, st(5, Format::Flac, "A/1.flac"));
        // content_version identical, format differs -> changed (path extension moves)
        let scan = vec![(1, 5, Format::Mp3)];
        let cs = partition_changes(&prev, &scan);
        assert_eq!(cs.changed, vec![1]);
        assert!(cs.added.is_empty() && cs.removed.is_empty());
    }

    #[test]
    fn no_changes_is_empty() {
        let mut prev = HashMap::new();
        prev.insert(1, st(0, Format::Flac, "A/1.flac"));
        let scan = vec![(1, 0, Format::Flac)];
        assert!(partition_changes(&prev, &scan).is_empty());
    }
}
```

Add to `musefs-core/src/lib.rs` after `mod reader;` (keep alphabetical-ish with the existing list):

```rust
mod refresh_diff;
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p musefs-core partition -- --nocapture`
Expected: FAIL — `partition_changes` panics with `not yet implemented` (or panics in all three tests).

- [ ] **Step 3: Write minimal implementation**

Replace the `todo!()` body:

```rust
pub(crate) fn partition_changes(
    prev: &HashMap<i64, TrackRenderState>,
    scan: &[(i64, i64, Format)],
) -> ChangeSet {
    let mut cs = ChangeSet::default();
    let mut seen = std::collections::HashSet::with_capacity(scan.len());
    for &(id, cv, fmt) in scan {
        seen.insert(id);
        match prev.get(&id) {
            None => cs.added.push(id),
            Some(s) if s.content_version != cv || s.format != fmt => cs.changed.push(id),
            Some(_) => {}
        }
    }
    for &id in prev.keys() {
        if !seen.contains(&id) {
            cs.removed.push(id);
        }
    }
    cs.removed.sort_unstable();
    // changed/added already id-ascending because `scan` is ordered by id.
    cs
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p musefs-core partition -- --nocapture`
Expected: PASS (all three).

- [ ] **Step 5: Commit**

```bash
cargo fmt && cargo clippy -p musefs-core --all-targets
git add musefs-core/src/refresh_diff.rs musefs-core/src/lib.rs
git commit -m "feat(core): render-key diff (TrackRenderState/ChangeSet/partition_changes) (SP2 A3)"
```

---

## Task A4: Swap `versions` for the `TrackRenderState` snapshot (behavior-preserving refactor)

This task changes the persisted state from `HashMap<i64,i64>` to `HashMap<i64, TrackRenderState>` and rewrites `build_tree`/`open`/`refresh`/`poll_refresh_notify` to use it — **while keeping full rebuild behavior** (no incremental render yet). The existing crate tests are the regression gate.

**Files:**
- Modify: `musefs-core/src/facade.rs`

- [ ] **Step 1: Add the `render_one` helper and `build_full`**

In `facade.rs`, add `use crate::refresh_diff::{partition_changes, ChangeSet, TrackRenderState};` to the imports (alongside the existing `use crate::...` lines, ~line 8-13).

Replace `build_tree` (facade.rs:164-187) with `build_full`, which returns the snapshot instead of the version map:

```rust
/// Render a single track's path from its tags + format. The one place
/// `render_path` is called, shared by full and incremental rebuilds.
fn render_one(config: &MountConfig, format: musefs_db::Format, tags: &[musefs_db::Tag]) -> String {
    let fields = tags_to_fields(tags);
    render_path(
        &config.template,
        &fields,
        &config.fallbacks,
        &config.default_fallback,
        format.as_str(),
    )
}

/// Full rebuild: render every track and build the tree from scratch. Used by
/// `open`, forced `refresh`, and the Stage B fallback. Returns the tree and the
/// fresh `track_id -> TrackRenderState` snapshot.
fn build_full(
    db: &Db,
    config: &MountConfig,
    alloc: &mut InodeAllocator,
) -> Result<(VirtualTree, std::collections::HashMap<i64, TrackRenderState>)> {
    let tracks = db.list_tracks()?;
    let mut tags_by_track = db.tags_grouped()?;
    let mut entries = Vec::with_capacity(tracks.len());
    let mut snapshot = std::collections::HashMap::with_capacity(tracks.len());
    for t in &tracks {
        let tags = tags_by_track.remove(&t.id).unwrap_or_default();
        let path = render_one(config, t.format, &tags);
        snapshot.insert(
            t.id,
            TrackRenderState { content_version: t.content_version, format: t.format, path: path.clone() },
        );
        entries.push((t.id, path));
    }
    Ok((VirtualTree::build_with(&entries, alloc), snapshot))
}
```

- [ ] **Step 2: Update the `Musefs` struct field and `open`**

Change the field (facade.rs:132-134):

```rust
    /// Last-seen render state per track, snapshotted on each rebuild. Drives the
    /// incremental change diff and the `on_changed` cache-invalidation callbacks.
    snapshot: Mutex<std::collections::HashMap<i64, TrackRenderState>>,
```

In `open` (facade.rs:139-162), change the build call and field init:

```rust
        let (tree, snapshot) = Self::build_full(&db, &config, &mut alloc)?;
```
and
```rust
            snapshot: Mutex::new(snapshot),
```
(remove the old `versions: Mutex::new(versions)`).

- [ ] **Step 3: Update `rebuild` and `refresh`**

Replace `rebuild` (facade.rs:208-223) so it returns the snapshot:

```rust
/// Rebuild + publish the tree via a full render; returns the fresh snapshot.
fn rebuild_full(&self) -> Result<std::collections::HashMap<i64, TrackRenderState>> {
    if self.force_rebuild_error.load(Ordering::Acquire) {
        return Err(CoreError::BackingChanged("forced refresh failure".to_string()));
    }
    let (tree, snapshot) = self.pool.with(|db| {
        let mut alloc = self.inodes.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        Self::build_full(db, &self.config, &mut alloc)
    })?;
    self.tree.store(Arc::new(tree));
    Ok(snapshot)
}
```

Replace `refresh` (facade.rs:197-204):

```rust
pub fn refresh(&self) -> Result<()> {
    let snapshot = self.rebuild_full()?;
    *self.snapshot.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = snapshot;
    Ok(())
}
```

- [ ] **Step 4: Update `poll_refresh_notify` to use the snapshot (still full rebuild)**

In `poll_refresh_notify` (facade.rs:298-353), replace the `old_versions`/`new_versions` block with snapshot-based logic that preserves today's `on_changed` semantics. Replace lines 298-353 with:

```rust
        let old_tree = self.tree.load_full();
        let old_snapshot = self
            .snapshot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let new_snapshot = match self.rebuild_full() {
            Ok(s) => s,
            Err(err) => {
                *self
                    .last_failed_refresh
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) =
                    Some(std::time::Instant::now());
                return Err(err);
            }
        };
        let tree = self.tree.load();
        let live = tree.track_ids();
        self.cache.retain(&live);
        self.size_cache().retain(|k, _| live.contains(k));

        Self::notify_changed(&old_snapshot, &new_snapshot, &old_tree, &tree, &mut on_changed);

        *self
            .snapshot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = new_snapshot;
        self.last_data_version.store(version, Ordering::Release);
        self.stamp_successful_poll();
        Ok(true)
```

Add the `notify_changed` helper (it fires on **content change OR path move**, which subsumes the old content_version logic and also covers format-only moves — SP2 Component 2):

```rust
/// Fire `on_changed` for every inode that must drop kernel cache: a track whose
/// served bytes changed (content_version rose, path stable) and the OLD inode of
/// any track that was removed or whose path moved (incl. a format-only move that
/// did not bump content_version). Path-move detection is decoupled from
/// content_version. See SP2 Component 2.
fn notify_changed(
    old: &std::collections::HashMap<i64, TrackRenderState>,
    new: &std::collections::HashMap<i64, TrackRenderState>,
    old_tree: &VirtualTree,
    new_tree: &VirtualTree,
    on_changed: &mut impl FnMut(u64),
) {
    // Content changed but path stable -> same inode, bytes differ.
    for (tid, ns) in new {
        if let Some(os) = old.get(tid) {
            if os.content_version != ns.content_version && os.path == ns.path {
                if let Some(ino) = new_tree.inode_of_track(*tid) {
                    on_changed(ino);
                }
            }
        }
    }
    // Removed or path-moved -> invalidate the OLD inode.
    for (tid, os) in old {
        let moved_or_gone = match new.get(tid) {
            None => true,                       // removed
            Some(ns) => ns.path != os.path,     // path move (tag- or format-driven)
        };
        if moved_or_gone {
            if let Some(ino) = old_tree.inode_of_track(*tid) {
                on_changed(ino);
            }
        }
    }
}
```

- [ ] **Step 5: Build and run the full existing test suite (regression gate)**

Run: `cargo test -p musefs-core -- --nocapture`
Expected: PASS — all existing facade/refresh tests still green (this task is behavior-preserving except that format-only moves now also notify, which no existing test asserts against).

If a compile error mentions `build_tree`, search for stale references: `grep -rn "build_tree\|\.versions" musefs-core/src`. There should be none left.

- [ ] **Step 6: Commit**

```bash
cargo fmt && cargo clippy -p musefs-core --all-targets
git add musefs-core/src/facade.rs
git commit -m "refactor(core): replace versions map with TrackRenderState snapshot (SP2 A4)"
```

---

## Task A5: Changed-only render (`rebuild_incremental`)

Now make the rebuild render only `changed ∪ added`, reusing cached paths for the rest, and call `build_with`. The headline equivalence test lands here.

**Files:**
- Modify: `musefs-core/src/facade.rs`
- Create: `musefs-core/tests/incremental_refresh.rs`

- [ ] **Step 1: Write the failing equivalence test**

Create `musefs-core/tests/incremental_refresh.rs`. This drives random edit sequences through a live `Musefs` (incremental) and compares the resulting tree to a freshly-built `Musefs` over the same DB (full). It uses only public API plus a small test-only accessor added in Step 3.

```rust
mod common;

use std::collections::BTreeMap;
use std::time::Duration;

use musefs_core::{scan_directory, Mode, MountConfig, Musefs};
use musefs_db::{Db, Format, Tag};

fn config() -> MountConfig {
    MountConfig {
        template: "$artist/$album/$title".into(),
        fallbacks: BTreeMap::new(),
        default_fallback: "Unknown".into(),
        mode: Mode::Synthesis,
        poll_interval: Duration::ZERO,
    }
}

/// Snapshot of (track_id -> rendered path) for the whole tree, derived from the
/// public API by walking from root. Two trees are equivalent iff these match AND
/// the inode for each path matches.
fn tree_fingerprint(fs: &Musefs) -> BTreeMap<String, u64> {
    let mut out = BTreeMap::new();
    let mut stack = vec![(1u64, String::new())];
    while let Some((ino, prefix)) = stack.pop() {
        for (name, child, is_dir) in fs.readdir(ino).unwrap() {
            let path = if prefix.is_empty() { name.clone() } else { format!("{prefix}/{name}") };
            if is_dir {
                stack.push((child, path));
            } else {
                out.insert(path, child);
            }
        }
    }
    out
}

#[test]
fn incremental_refresh_matches_full_rebuild_over_edits() {
    let dir = tempfile::tempdir().unwrap();
    let corpus = dir.path().join("corpus");
    // Build a tiny multi-track corpus the scanner accepts.
    common::corpus::write_min_corpus(&corpus, 8); // see Step 2 note
    let db_path = dir.path().join("musefs.db");

    let db = Db::open(&db_path).unwrap();
    scan_directory(&db, &corpus).unwrap();
    let fs = Musefs::open(Db::open(&db_path).unwrap(), config()).unwrap();

    // Apply a deterministic edit sequence via a separate writer, refreshing after each.
    let writer = Db::open(&db_path).unwrap();
    let ids: Vec<i64> = writer.list_tracks().unwrap().iter().map(|t| t.id).collect();

    // Edit kinds (Component 1/Testing item 1): tag change, path-move, delete.
    writer.replace_tags(ids[0], &[Tag::new("ARTIST", "Zed", 0), Tag::new("TITLE", "moved", 0)]).unwrap();
    fs.poll_refresh().unwrap();
    writer.replace_tags(ids[1], &[Tag::new("ALBUM", "NewAlbum", 0)]).unwrap();
    fs.poll_refresh().unwrap();
    // Delete a track row + its tags.
    writer.delete_track(ids[2]).unwrap(); // see Step 2 note
    fs.poll_refresh().unwrap();

    // Reference: a fresh full-rebuild Musefs over the final DB state.
    let reference = Musefs::open(Db::open(&db_path).unwrap(), config()).unwrap();

    assert_eq!(
        tree_fingerprint(&fs).keys().collect::<Vec<_>>(),
        tree_fingerprint(&reference).keys().collect::<Vec<_>>(),
        "incremental and full-rebuild paths must match"
    );
}
```

> **Step 2 notes (verified):**
> - Use the `small_corpus(n)` helper from the "Test corpus helper" section above (there is no `write_min_corpus`). Replace the `let dir = ...; let corpus = ...; write_min_corpus(...)` lines with `let target = small_corpus(8); let db_path = target.db_path.clone(); let corpus = target.corpus_dir.clone();` and keep `target` in scope.
> - `Db::delete_track(id: i64) -> Result<()>` exists in `musefs-db/src/tracks.rs:96` (`DELETE FROM tracks WHERE id = ?1`, FK-cascades to tags/track_art). Use it directly.

- [ ] **Step 3: Run the test — expect it to PASS already (Stage A4 still full-rebuilds, which is trivially equivalent)**

Run: `cargo test -p musefs-core --test incremental_refresh -- --nocapture`
Expected: PASS. (At this point `poll_refresh` is still a full rebuild, so equivalence holds trivially. This test is the guard we must keep green as A5/Stage B make it incremental.)

> This is deliberate: we write the equivalence oracle BEFORE the incremental optimization, so it catches any divergence the optimization introduces.

- [ ] **Step 4: Implement `rebuild_incremental` and wire `poll_refresh_notify` to it**

Add to `facade.rs`:

```rust
/// Incremental rebuild (Stage A): scan render keys, diff against the previous
/// snapshot, render only changed/added tracks (reusing cached paths otherwise),
/// then assemble entries and call the unchanged `build_with`. Returns the new
/// tree's snapshot and the `ChangeSet`. The tree is published here. See SP2
/// Component 2.
fn rebuild_incremental(
    &self,
    prev_snapshot: &std::collections::HashMap<i64, TrackRenderState>,
) -> Result<(std::collections::HashMap<i64, TrackRenderState>, ChangeSet)> {
    if self.force_rebuild_error.load(Ordering::Acquire) {
        return Err(CoreError::BackingChanged("forced refresh failure".to_string()));
    }
    let (new_snapshot, change) = self.pool.with(|db| {
        let scan = db.list_render_keys()?;
        let change = partition_changes(prev_snapshot, &scan);

        // Build the new snapshot: reuse unchanged entries, re-render changed/added.
        let mut to_render: Vec<i64> = change.changed.clone();
        to_render.extend(change.added.iter().copied());
        let mut tags_by_track = db.tags_for_tracks(&to_render)?;
        // Need format for the to_render set: it's already in `scan`.
        let fmt_by_id: std::collections::HashMap<i64, musefs_db::Format> =
            scan.iter().map(|&(id, _, fmt)| (id, fmt)).collect();
        let cv_by_id: std::collections::HashMap<i64, i64> =
            scan.iter().map(|&(id, cv, _)| (id, cv)).collect();

        let mut new_snapshot = std::collections::HashMap::with_capacity(scan.len());
        for &(id, _, _) in &scan {
            let state = if change.changed.contains(&id) || change.added.contains(&id) {
                let tags = tags_by_track.remove(&id).unwrap_or_default();
                let fmt = fmt_by_id[&id];
                TrackRenderState {
                    content_version: cv_by_id[&id],
                    format: fmt,
                    path: render_one(&self.config, fmt, &tags),
                }
            } else {
                // Unchanged: reuse cached path (and key) verbatim.
                prev_snapshot[&id].clone()
            };
            new_snapshot.insert(id, state);
        }

        Ok::<_, CoreError>((new_snapshot, change))
    })?;

    // Assemble entries (id-ascending: scan order) and rebuild the tree.
    let mut entries: Vec<(i64, String)> =
        new_snapshot.iter().map(|(&id, s)| (id, s.path.clone())).collect();
    entries.sort_by_key(|(id, _)| *id);
    {
        let mut alloc = self.inodes.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let tree = VirtualTree::build_with(&entries, &mut alloc);
        self.tree.store(Arc::new(tree));
    }
    Ok((new_snapshot, change))
}
```

> **`contains` cost note:** `change.changed.contains(&id)` is O(changed) per track → O(N·changed). For Stage A correctness this is fine on the small changed-sets we target, but to keep it O(N) build a `HashSet` of `to_render` once and test membership against it. Use:
> ```rust
> let render_set: std::collections::HashSet<i64> = to_render.iter().copied().collect();
> ```
> and replace the `if change.changed.contains(&id) || change.added.contains(&id)` with `if render_set.contains(&id)`.

Now change `poll_refresh_notify` (the block edited in A4) to call `rebuild_incremental` instead of `rebuild_full`, and keep `notify_changed`:

```rust
        let old_tree = self.tree.load_full();
        let old_snapshot = self
            .snapshot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let (new_snapshot, _change) = match self.rebuild_incremental(&old_snapshot) {
            Ok(v) => v,
            Err(err) => {
                *self
                    .last_failed_refresh
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) =
                    Some(std::time::Instant::now());
                return Err(err);
            }
        };
        let tree = self.tree.load();
        let live = tree.track_ids();
        self.cache.retain(&live);
        self.size_cache().retain(|k, _| live.contains(k));
        Self::notify_changed(&old_snapshot, &new_snapshot, &old_tree, &tree, &mut on_changed);
        *self
            .snapshot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = new_snapshot;
        self.last_data_version.store(version, Ordering::Release);
        self.stamp_successful_poll();
        Ok(true)
```

- [ ] **Step 5: Run the equivalence test + full suite**

Run: `cargo test -p musefs-core --test incremental_refresh -- --nocapture`
Expected: PASS (now exercising the incremental path).

Run: `cargo test -p musefs-core -- --nocapture`
Expected: PASS (all existing tests).

- [ ] **Step 6: Commit**

```bash
cargo fmt && cargo clippy -p musefs-core --all-targets
git add musefs-core/src/facade.rs musefs-core/tests/incremental_refresh.rs musefs-core/tests/common
git commit -m "feat(core): changed-only incremental render via rebuild_incremental (SP2 A5)"
```

---

## Task A6: Change-detection + format-only invalidation integration tests

**Files:**
- Modify: `musefs-core/tests/incremental_refresh.rs`

- [ ] **Step 1: Write the tests**

Append:

```rust
#[test]
fn non_render_column_edit_is_noop_refresh() {
    // A tracks-table edit to a non-render column must not be observed as a change
    // (empty partition -> poll_refresh returns false). We simulate the closest
    // available signal: re-running scan_directory over an unchanged corpus bumps
    // data_version (upsert touches updated_at) but changes no render key.
    let dir = tempfile::tempdir().unwrap();
    let corpus = dir.path().join("corpus");
    common::corpus::write_min_corpus(&corpus, 4);
    let db_path = dir.path().join("musefs.db");
    let db = Db::open(&db_path).unwrap();
    scan_directory(&db, &corpus).unwrap();
    let fs = Musefs::open(Db::open(&db_path).unwrap(), config()).unwrap();

    // Re-scan: upsert_track updates updated_at (data_version bump) but no tag/art
    // change, so content_version and format are unchanged for every track.
    let db2 = Db::open(&db_path).unwrap();
    scan_directory(&db2, &corpus).unwrap();

    let before = tree_fingerprint(&fs);
    let rebuilt = fs.poll_refresh().unwrap();
    let after = tree_fingerprint(&fs);
    // Tree is unchanged either way; the point is correctness, not the bool.
    assert_eq!(before, after, "non-render edit must not change the tree");
    let _ = rebuilt; // value depends on whether scan bumped data_version; tree-equality is the gate
}

#[test]
fn format_only_change_notifies_old_inode() {
    let dir = tempfile::tempdir().unwrap();
    let corpus = dir.path().join("corpus");
    common::corpus::write_min_corpus(&corpus, 2);
    let db_path = dir.path().join("musefs.db");
    let db = Db::open(&db_path).unwrap();
    scan_directory(&db, &corpus).unwrap();
    let fs = Musefs::open(Db::open(&db_path).unwrap(), config()).unwrap();

    let writer = Db::open(&db_path).unwrap();
    let id = writer.list_tracks().unwrap()[0].id;
    let old_ino = fs.lookup_track_inode_for_test(id).unwrap(); // see Step 2

    // Force a format change directly (no tags trigger), bumping data_version.
    writer.set_format_for_test(id, Format::Mp3).unwrap(); // see Step 2

    let mut notified = Vec::new();
    fs.poll_refresh_notify(|ino| notified.push(ino)).unwrap();

    assert!(notified.contains(&old_ino),
        "format-only move must invalidate the old inode (extension changed)");
}
```

> **Step 2 notes:**
> - Add a `#[doc(hidden)] pub fn lookup_track_inode_for_test(&self, track_id: i64) -> Option<u64>` to `facade.rs` that returns `self.tree.load().inode_of_track(track_id)`.
> - `set_format_for_test`: add a `#[doc(hidden)] pub fn set_format_for_test(&self, id: i64, fmt: Format) -> Result<()>` to `musefs-db/src/tracks.rs` running `UPDATE tracks SET format = ?1, updated_at = CAST(strftime('%s','now') AS INTEGER) WHERE id = ?2`. This is the only way to exercise a format-only change (no production path mutates format without a rescan). Keep it test-gated via `#[doc(hidden)]` (the codebase already uses `#[doc(hidden)] pub fn ..._for_test` — see `force_rebuild_errors_for_test`).
> - If `write_min_corpus` produces a path that changes its extension on a format flip, the fingerprint path key changes; the test asserts on the OLD inode notification, which is the invariant that matters.

- [ ] **Step 2: Run to verify they fail, then add the test-only accessors**

Run: `cargo test -p musefs-core --test incremental_refresh format_only -- --nocapture`
Expected: FAIL — missing `lookup_track_inode_for_test` / `set_format_for_test`.

Add both accessors (per Step 2 notes), rebuild.

- [ ] **Step 3: Run to verify they pass**

Run: `cargo test -p musefs-core --test incremental_refresh -- --nocapture`
Expected: PASS.

- [ ] **Step 4: Commit**

```bash
cargo fmt && cargo clippy -p musefs-core --all-targets && cargo clippy -p musefs-db --all-targets
git add musefs-core/src/facade.rs musefs-db/src/tracks.rs musefs-core/tests/incremental_refresh.rs
git commit -m "test(core): change-detection + format-only invalidation cases (SP2 A6)"
```

---

## Task A7: Stage A bench — library-size sweep + identity-scan row

**Files:**
- Modify: `musefs-core/tests/bench_refresh.rs`

- [ ] **Step 1: Add the sweep**

Add a second `#[ignore]` bench beside `bench_refresh_one_vs_many` that runs the fixed single-track touch across several corpus sizes, printing refresh-1 wall time per size so "independent of N_total" is visible. Reuse the existing `time_refresh` and `RunReport`.

```rust
#[test]
#[ignore = "SP2 timing harness; run with --ignored --nocapture"]
fn bench_refresh_one_across_library_sizes() {
    use common::corpus::write_min_corpus;
    let tier = std::env::var("MUSEFS_BENCH_TIER").unwrap_or_else(|_| "ci".into());
    println!("\n{}", RunReport::header());
    for n in [100usize, 1000, 5000] {
        let dir = tempfile::tempdir().unwrap();
        let corpus = dir.path().join("corpus");
        write_min_corpus(&corpus, n);
        let db_path = dir.path().join("musefs.db");
        let db = Db::open(&db_path).unwrap();
        scan_directory(&db, &corpus).unwrap();
        let fs = Musefs::open(Db::open(&db_path).unwrap(), config()).unwrap();
        let one_ms = time_refresh(&db_path, &fs, 1);
        println!("{}", RunReport {
            label: format!("refresh-1@{n}"),
            format: "flac".into(),
            tier: tier.clone(),
            storage: "tempfs".into(),
            wall_ms: one_ms,
            opens: 0, preads: 0, fsyncs: None, bytes_read: 0, peak_rss_kib: None,
        }.row());
    }
    println!();
}
```

> If `write_min_corpus` is too slow to write 5000 fixtures, lower the top size or reuse the SP0a corpus generator the other benches use (`CorpusParams`). The goal is 3 points showing refresh-1 flat vs size.

- [ ] **Step 2: Run it once to confirm it executes**

Run: `cargo test -p musefs-core --test bench_refresh bench_refresh_one_across_library_sizes -- --ignored --nocapture`
Expected: prints 3 `refresh-1@N` rows; wall_ms should be roughly flat across N for Stage A (full build_with is still O(N) — so it may NOT be flat yet; record the numbers as the Stage A baseline. Stage B is where flatness appears).

- [ ] **Step 3: Record Stage A numbers in BENCHMARKS.md + results log**

Append a Stage A subsection to repo-root `BENCHMARKS.md` (per the project convention) and a line to the spec README results log with the refresh-1/refresh-N and per-size numbers.

- [ ] **Step 4: Commit**

```bash
cargo fmt && cargo clippy -p musefs-core --all-targets
git add musefs-core/tests/bench_refresh.rs BENCHMARKS.md docs/superpowers/specs/2026-05-30-optimization-pass/README.md
git commit -m "bench(core): library-size sweep for refresh; record Stage A baseline (SP2 A7)"
```

---

## Stage A checkpoint

Run the full gate: `cargo test` (excludes `#[ignore]` FUSE e2e) and `cargo clippy --all-targets`. All green is the gate to start Stage B. The Stage A equivalence test (`incremental_refresh_matches_full_rebuild_over_edits`) MUST be green before any Stage B work.

---

# STAGE B — in-place tree mutation (strict O(changed))

> Stage B is one mechanical prerequisite — **Task B1** (the `im` migration) — then
> the design notes and **Tasks B2–B7**. Execution order: Stage A (A1–A7) → B1 →
> B2–B7 → Final checklist.

## Task B1: Migrate `VirtualTree` internals to `im` persistent maps

This is a pure internal swap: the public API (`build`, `build_with`, `node`, `parent`, `children`, `lookup`, `is_dir`, `track_id`, `inode_of_track`, `track_ids`, `ROOT`) is unchanged. Only field types and the insert sites change.

**Files:**
- Modify: `musefs-core/Cargo.toml`, `musefs-core/src/tree.rs`

- [ ] **Step 1: Add the `im` dependency**

In `musefs-core/Cargo.toml` under `[dependencies]`:

```toml
im = "15"
```

- [ ] **Step 2: Run the existing tree tests to confirm baseline green**

Run: `cargo test -p musefs-core --lib tree -- --nocapture`
Expected: PASS (the existing `tree.rs` unit tests).

- [ ] **Step 3: Swap the field types**

In `tree.rs`, change imports and the struct (tree.rs:1, 48-52):

```rust
use im::{HashMap as ImHashMap, OrdMap};

#[derive(Debug, Clone)]
pub struct VirtualTree {
    nodes: ImHashMap<u64, Node>,
    children: ImHashMap<u64, OrdMap<String, u64>>,
    track_to_inode: ImHashMap<i64, u64>,
}
```

`Node` must be `Clone` (it already is) and stored by value — `im::HashMap` requires `Clone` values (`Node` derives `Clone`). `OrdMap` preserves the sorted-by-`String` iteration order that `BTreeMap` gave `readdir`.

Update every constructor/insert site in `tree.rs`:
- `build_with` initial maps: `ImHashMap::new()` / `OrdMap::new()` instead of `HashMap::new()`/`BTreeMap::new()`.
- `tree.children.insert(Self::ROOT, OrdMap::new());`
- In `insert_file`/`ensure_dir`: `self.children.get_mut(&dir)` — `im` maps DO have `get_mut` (clone-on-write via `Arc::make_mut`); keep the same call shape. If a borrow-checker issue arises with `im`'s `get_mut` returning through `entry`, use `self.children.entry(dir).or_default().insert(name, inode)` patterns. The `entry` API exists on `im::HashMap`/`OrdMap`.
- `children` return type: `pub fn children(&self, inode: u64) -> Option<&OrdMap<String, u64>>` — update the signature. Check callers in `facade.rs::readdir` (it iterates `children.iter()` and calls `.get(name)` in `lookup`) — `OrdMap` supports both, so callers compile unchanged.
- `lookup`: `.and_then(|c| c.get(name).copied())` — `OrdMap::get` returns `Option<&u64>`, `.copied()` works.

- [ ] **Step 4: Build and run tree tests + full suite**

Run: `cargo test -p musefs-core --lib tree -- --nocapture`
Expected: PASS (same behavior, im-backed).

Run: `cargo test -p musefs-core -- --nocapture`
Expected: PASS — including the Stage A equivalence test (still using `build_with`, now im-backed).

> If `children()`'s changed return type breaks an external caller, `grep -rn "\.children(" musefs-core musefs-fuse` and adjust. `readdir` (facade.rs:454) uses `children.iter()` which `OrdMap` supports.

- [ ] **Step 5: Commit**

```bash
cargo fmt && cargo clippy -p musefs-core --all-targets
git add musefs-core/Cargo.toml musefs-core/src/tree.rs Cargo.lock
git commit -m "refactor(core): back VirtualTree with im persistent maps for O(1) clone (SP2 B1)"
```

---

## Final checklist (run before opening the PR)

- [ ] `cargo test` green (all crates).
- [ ] `cargo test -p musefs-fuse -- --ignored` green (byte-identical audio).
- [ ] `cargo clippy --all-targets` clean.
- [ ] `cargo test -p musefs-core --test incremental_refresh` and the proptest green.
- [ ] `read_throughput` `sequential_read` median within 10% of baseline.
- [ ] Spec README status row updated to "Implemented" with the plan link; results log + `BENCHMARKS.md` updated.
- [ ] No `versions`/`build_tree` references remain (`grep -rn "build_tree\|\.versions\b" musefs-core/src` is empty).

---

## Stage B design — in-place mutation with precise introducing-id re-disambiguation

**Two facts drive the design:**
1. **Rendered paths ≠ tree paths.** A track's *rendered* path (e.g. `D/song.flac`) is the pre-disambiguation template output; the *tree* path is post-disambiguation (`D/song (2).flac`). Re-disambiguation must work from **rendered** names (so a freed base name can be reclaimed) and must be able to **navigate the tree by rendered path**. We therefore add a `rendered_name: String` to `Node` and thread it through `insert_file`/`ensure_dir`/`build_with`.
2. **The only correctness-hard case** is a directory whose disambiguation changes *without* a direct membership change — i.e. a **dir-name-vs-file-name collision** (`ensure_dir` only disambiguates a directory against a *same-named file*, tree.rs:154-158; same-named directories merge) whose claim order flips when a child subtree's **introducing id** (its minimum descendant track id) changes. We handle this with precise introducing-id propagation.

**Algorithm (per refresh).** Given `new_paths: HashMap<i64,String>` (rendered path for every current track) and the `ChangeSet`:
1. **Compute the dirty set on the OLD tree, before mutating** (introducing-id is read from the pre-change tree):
   - *remove/move-out* track `t` at old leaf `L`: mark `parent(L)` dirty; then while walking up, for each ancestor `A` with `introducing_id(A) == t` (t was its min), mark `parent(A)` dirty and continue (the min will rise → A's name in its parent may change).
   - *add/move-in* track `t` with rendered path `R`: find the **deepest existing ancestor** `D` of `R` (navigate by `rendered_name`); mark `D` dirty (the new chain, if any, attaches and rebuilds under `D`); then while `t < introducing_id(A)` for ancestors `A` from `D` up, mark `parent(A)` dirty (t becomes the new min → A's name may change).
2. **Apply structural changes:** `remove_track` every removed + moved-out leaf (pruning empty ancestors; record each pruned dir's surviving parent into the dirty set). `insert_file` every added + moved-in leaf from its rendered path (disambiguation here may be *temporarily wrong* — step 4 corrects it).
3. **Reduce dirty to top-most survivors:** drop any dirty dir that is a descendant of another dirty dir, or that was pruned (map it to its nearest surviving ancestor, which is already dirty).
4. **Rebuild each top-most dirty subtree from rendered paths in ascending track-id order** (`rebuild_subtree`). This reproduces `build_with` restricted to that subtree exactly (same inputs, same order, same allocator), fixing any temporary mis-disambiguation from step 2 and reclaiming freed base names.
5. **Fallback:** any navigation/structural inconsistency returns `Err(())` → caller does a full `build_with`. A debug-only assert compares the result to `build_with(entries, allocator.clone())` (paths **and** inodes).

Correctness rests on step 4 (provably equal for a whole subtree) plus a dirty set that is a **superset** of the truly-affected dirs. The property oracle (Task B6) is the gate; if it finds a divergence, the fix is always "widen the dirty set," never "change rebuild_subtree."

**Perf note (the superset is bounded by the affected subtree, not the library).** The introducing-id propagation marks ancestors up to the level where the removed/added id stops being the subtree's min. For a *non-colliding* edit this over-marks (e.g. deleting an album's lowest-id track marks the album — and, if that id is also the artist's min, the artist — dirty), so `rebuild_subtree` rebuilds that artist/album subtree rather than just the one dir. That is still **O(affected subtree) ≪ O(library)** — it never reaches the SP2 anti-goal of scaling with library size unless the touched id is the *global* minimum. A later optimization could propagate only when a dir-vs-file collision actually exists at that level (the only case the cascade can change a name); that is YAGNI for now and out of scope — correctness-first superset is the chosen posture and the benches (B7) will confirm it stays sublinear in library size.

---

## Task B2: `Node.rendered_name` + navigation/introducing-id/remove primitives

**Files:** Modify `musefs-core/src/tree.rs`.

- [ ] **Step 1: Add `rendered_name` to `Node` and thread it through inserts**

Change `Node` (tree.rs:38-43):

```rust
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Node {
    pub parent: u64,
    pub name: String,          // disambiguated name (as today)
    pub rendered_name: String, // pre-disambiguation base name (NEW)
    pub kind: NodeKind,
}
```

In `insert_file` and `ensure_dir`, set `rendered_name` to the **pre-disambiguation** component (the value passed to `disambiguate`), and `name` to the disambiguated result. The root node's `rendered_name` is `String::new()`. Update the root insert in `build_with` and the two node inserts in `insert_file`/`ensure_dir` accordingly (the disambiguated `name`/`unique` stays in `name`; the original `comp`/`name` argument goes to `rendered_name`).

- [ ] **Step 2: Run existing tree tests (regression)**

Run: `cargo test -p musefs-core --lib tree -- --nocapture`
Expected: PASS (adding a field, existing behavior unchanged). Fix any `Node { .. }` literal in `facade.rs` tests that now needs `rendered_name` (the `validate_opened_backing` test builds a `ResolvedFile`, not a `Node`, so likely none — but `grep -rn "Node {" musefs-core` to be sure).

- [ ] **Step 3: Add navigation + introducing-id + remove/prune helpers (with failing tests first)**

Add tests:

```rust
#[test]
fn child_by_rendered_finds_disambiguated_node() {
    let t = VirtualTree::build(&[(10, "D/song.flac".into()), (20, "D/song.flac".into())]);
    let d = t.lookup(VirtualTree::ROOT, "D").unwrap();
    // Both children have rendered_name "song.flac" but disambiguated names differ.
    let by_rendered: Vec<u64> = t.children_by_rendered(d, "song.flac");
    assert_eq!(by_rendered.len(), 2);
}

#[test]
fn introducing_id_is_min_descendant_track_id() {
    let mut alloc = InodeAllocator::new();
    let t = VirtualTree::build_with(&[(30, "A/B/x.flac".into()), (10, "A/C/y.flac".into())], &mut alloc);
    let a = t.lookup(VirtualTree::ROOT, "A").unwrap();
    assert_eq!(t.introducing_id(a), 10);
}

#[test]
fn remove_track_prunes_empty_ancestors_b() {
    let mut alloc = InodeAllocator::new();
    let mut t = VirtualTree::build_with(&[(10, "A/B/x.flac".into()), (20, "C/y.flac".into())], &mut alloc);
    t.remove_track(10, &mut alloc);
    assert!(t.inode_of_track(10).is_none());
    assert!(t.lookup(VirtualTree::ROOT, "A").is_none());
    assert!(t.lookup(VirtualTree::ROOT, "C").is_some());
}
```

Run: `cargo test -p musefs-core --lib "child_by_rendered|introducing_id_is_min|remove_track_prunes_empty_ancestors_b" -- --nocapture`
Expected: FAIL (methods missing).

Implement on `impl VirtualTree`:

```rust
/// Inodes of `dir`'s direct children whose pre-disambiguation name is `rendered`.
pub fn children_by_rendered(&self, dir: u64, rendered: &str) -> Vec<u64> {
    match self.children.get(&dir) {
        None => Vec::new(),
        Some(kids) => kids.values().copied()
            .filter(|&c| self.nodes.get(&c).is_some_and(|n| n.rendered_name == rendered))
            .collect(),
    }
}

/// Minimum descendant track id under `ino` (a file's own id; a dir's min over files).
pub fn introducing_id(&self, ino: u64) -> i64 {
    match self.nodes.get(&ino).map(|n| &n.kind) {
        Some(NodeKind::File { track_id }) => *track_id,
        _ => {
            let mut min = i64::MAX;
            let mut stack = vec![ino];
            while let Some(n) = stack.pop() {
                match self.nodes.get(&n).map(|x| &x.kind) {
                    Some(NodeKind::File { track_id }) => min = min.min(*track_id),
                    _ => if let Some(kids) = self.children.get(&n) {
                        for &c in kids.values() { stack.push(c); }
                    },
                }
            }
            min
        }
    }
}

/// The full disambiguated path from root to `inode` (root → "").
fn path_of(&self, inode: u64) -> String {
    if inode == Self::ROOT { return String::new(); }
    let mut parts = Vec::new();
    let mut cur = inode;
    while cur != Self::ROOT {
        let n = match self.nodes.get(&cur) { Some(n) => n, None => break };
        parts.push(n.name.clone());
        cur = n.parent;
    }
    parts.reverse();
    parts.join("/")
}

/// Remove the file node for `track_id` and prune now-empty ancestor dirs. Returns
/// the inode of the nearest surviving ancestor directory (for dirty bookkeeping).
pub fn remove_track(&mut self, track_id: i64, _alloc: &mut InodeAllocator) -> Option<u64> {
    let ino = self.track_to_inode.remove(&track_id)?;
    let parent = self.nodes.get(&ino)?.parent;
    let name = self.nodes.get(&ino).map(|n| n.name.clone());
    self.nodes.remove(&ino);
    if let (Some(name), Some(kids)) = (name, self.children.get_mut(&parent)) {
        kids.remove(&name);
    }
    Some(self.prune_empty_dirs_upward(parent))
}

/// Walk up from `dir`, removing empty directories; return the first non-empty
/// (surviving) ancestor.
fn prune_empty_dirs_upward(&mut self, mut dir: u64) -> u64 {
    while dir != Self::ROOT
        && self.children.get(&dir).map_or(true, |c| c.is_empty())
    {
        let parent = match self.nodes.get(&dir) { Some(n) => n.parent, None => break };
        let name = self.nodes.get(&dir).map(|n| n.name.clone());
        self.children.remove(&dir);
        self.nodes.remove(&dir);
        if let (Some(name), Some(kids)) = (name, self.children.get_mut(&parent)) {
            kids.remove(&name);
        }
        dir = parent;
    }
    dir
}
```

> `insert_file` stays private and is reused as-is (it now also sets `rendered_name`). Make it `pub(crate)`-callable from within `impl VirtualTree` methods (it already is, same impl block).

Run: `cargo test -p musefs-core --lib "child_by_rendered|introducing_id_is_min|remove_track_prunes_empty_ancestors_b" -- --nocapture`
Expected: PASS.

- [ ] **Step 4: Commit**

```bash
cargo fmt && cargo clippy -p musefs-core --all-targets
git add musefs-core/src/tree.rs musefs-core/src/facade.rs
git commit -m "feat(core): Node.rendered_name + nav/introducing-id/remove primitives (SP2 B2)"
```

---

## Task B3: `rebuild_subtree` (clear + reinsert from rendered paths, id-order)

The workhorse: rebuild one directory's subtree so its disambiguation equals a fresh build.

**Files:** Modify `musefs-core/src/tree.rs`.

- [ ] **Step 1: Write failing tests (reclamation + dir-vs-file), comparing to `build`**

```rust
fn paths_of(t: &VirtualTree) -> std::collections::BTreeMap<String, u64> {
    let mut out = std::collections::BTreeMap::new();
    let mut stack = vec![(VirtualTree::ROOT, String::new())];
    while let Some((ino, pfx)) = stack.pop() {
        if let Some(kids) = t.children(ino) {
            for (name, &child) in kids.iter() {
                let p = if pfx.is_empty() { name.clone() } else { format!("{pfx}/{name}") };
                if t.is_dir(child) { stack.push((child, p)); } else { out.insert(p, child); }
            }
        }
    }
    out
}

#[test]
fn rebuild_subtree_reclaims_freed_base_name() {
    let mut alloc = InodeAllocator::new();
    let mut t = VirtualTree::build_with(
        &[(10, "D/song.flac".into()), (20, "D/song.flac".into())], &mut alloc);
    let d = t.lookup(VirtualTree::ROOT, "D").unwrap();
    t.remove_track(10, &mut alloc);
    // new_paths after removal: only id 20 remains, rendered "D/song.flac".
    let mut np = std::collections::HashMap::new();
    np.insert(20, "D/song.flac".to_string());
    t.rebuild_subtree(d, &np, &mut alloc).unwrap();
    let reborn = t.lookup(d, "song.flac").unwrap();
    assert_eq!(t.inode_of_track(20), Some(reborn));
    assert!(t.lookup(d, "song (2).flac").is_none());
}

#[test]
fn rebuild_subtree_matches_build_for_dir_vs_file() {
    // $album="X.flac" produces dir "X.flac"; a sibling file also "X.flac".
    let entries = vec![(1, "P/X.flac".to_string()), (2, "P/X.flac/t.flac".to_string())];
    let reference = VirtualTree::build(&entries);
    let mut alloc = InodeAllocator::new();
    let mut t = VirtualTree::build_with(&entries, &mut alloc);
    let p = t.lookup(VirtualTree::ROOT, "P").unwrap();
    let np: std::collections::HashMap<i64,String> = entries.iter().cloned().collect();
    t.rebuild_subtree(p, &np, &mut alloc).unwrap();
    assert_eq!(paths_of(&t).keys().collect::<Vec<_>>(),
               paths_of(&reference).keys().collect::<Vec<_>>());
}
```

Run: `cargo test -p musefs-core --lib rebuild_subtree -- --nocapture`
Expected: FAIL (`no method named rebuild_subtree`).

- [ ] **Step 2: Implement `rebuild_subtree`**

```rust
/// Rebuild the subtree rooted at directory `dir` so its disambiguation matches a
/// fresh `build_with`: collect every track currently under `dir`, remove them all
/// (pruning), then re-insert in ascending track-id order using each track's
/// RENDERED path from `new_paths`. `ensure_dir` reuses ancestors above `dir`, so
/// only `dir`'s subtree is rebuilt. Errs if a collected track has no entry in
/// `new_paths` (caller falls back to a full rebuild). See SP2 Component 3.
#[allow(clippy::result_unit_err)]
pub fn rebuild_subtree(
    &mut self,
    dir: u64,
    new_paths: &std::collections::HashMap<i64, String>,
    alloc: &mut InodeAllocator,
) -> std::result::Result<(), ()> {
    let mut ids = Vec::new();
    let mut stack = vec![dir];
    while let Some(n) = stack.pop() {
        match self.nodes.get(&n).map(|x| x.kind.clone()) {
            Some(NodeKind::File { track_id }) => ids.push(track_id),
            _ => if let Some(kids) = self.children.get(&n) {
                for &c in kids.values() { stack.push(c); }
            },
        }
    }
    for id in &ids { self.remove_track(*id, alloc); }
    ids.sort_unstable();
    for id in ids {
        let path = new_paths.get(&id).ok_or(())?;
        self.insert_file(id, path, alloc);
    }
    Ok(())
}
```

> Note: after removing all of `dir`'s tracks, `dir` itself may be pruned. Re-inserting via full rendered paths recreates it through `ensure_dir` from root, reusing surviving ancestors — correct. The caller (Task B5) only calls `rebuild_subtree` on top-most dirty dirs, so subtrees never double-rebuild.

Run: `cargo test -p musefs-core --lib rebuild_subtree -- --nocapture`
Expected: PASS.

- [ ] **Step 3: Commit**

```bash
cargo fmt && cargo clippy -p musefs-core --all-targets
git add musefs-core/src/tree.rs
git commit -m "feat(core): rebuild_subtree (rendered-path, id-order) reproduces build_with (SP2 B3)"
```

---

## Task B4: `apply_changes` (dirty-set + propagation) and the concrete cascade test

**Files:** Modify `musefs-core/src/tree.rs`.

- [ ] **Step 1: Write the concrete cascade test (the case the earlier plan missed)**

```rust
#[test]
fn apply_changes_handles_dir_vs_file_min_id_flip() {
    // P has dir "X.flac" (from $album="X.flac", tracks 1 & 9) and file "X.flac"
    // (track 5). Ascending id: dir introduced by 1 claims "X.flac"; file 5 -> "X (2).flac".
    let entries = vec![
        (1, "X.flac/a.flac".to_string()),
        (9, "X.flac/b.flac".to_string()),
        (5, "X.flac".to_string()),               // a FILE rendered "X.flac" in root
    ];
    let mut alloc = InodeAllocator::new();
    let mut t = VirtualTree::build_with(&entries, &mut alloc);
    // Delete track 1 (the dir's min). Dir's introducing id rises to 9; file 5 (id 5 < 9)
    // should now claim base "X.flac" and the dir become "X.flac (2)".
    let new_entries = vec![
        (9, "X.flac/b.flac".to_string()),
        (5, "X.flac".to_string()),
    ];
    let reference = VirtualTree::build(&new_entries);
    let new_paths: std::collections::HashMap<i64,String> = new_entries.iter().cloned().collect();
    t.apply_changes(&new_paths, &[], &[], &[1], &mut alloc).unwrap();
    assert_eq!(paths_of(&t).keys().collect::<Vec<_>>(),
               paths_of(&reference).keys().collect::<Vec<_>>(),
               "dir-vs-file min-id flip must match a full rebuild");
}

#[test]
fn apply_changes_handles_add_side_min_id_flip() {
    // Initial: file "X.flac" (id 2) claims the base name; dir "X.flac"
    // (introduced by id 5) is disambiguated to "X.flac (2)".
    let entries = vec![(2, "X.flac".to_string()), (5, "X.flac/a.flac".to_string())];
    let mut alloc = InodeAllocator::new();
    let mut t = VirtualTree::build_with(&entries, &mut alloc);
    // ADD track 1 under the dir: its id (1) is now the dir's min (< file's 2), so a
    // full rebuild gives the DIR the base name and the file becomes "X.flac (2)".
    let new_entries = vec![
        (1, "X.flac/b.flac".to_string()),
        (2, "X.flac".to_string()),
        (5, "X.flac/a.flac".to_string()),
    ];
    let reference = VirtualTree::build(&new_entries);
    let new_paths: std::collections::HashMap<i64, String> = new_entries.iter().cloned().collect();
    t.apply_changes(&new_paths, &[], &[1], &[], &mut alloc).unwrap();
    assert_eq!(paths_of(&t).keys().collect::<Vec<_>>(),
               paths_of(&reference).keys().collect::<Vec<_>>(),
               "add-side dir-vs-file min-id flip must match a full rebuild");
}
```

Run: `cargo test -p musefs-core --lib apply_changes_handles -- --nocapture`
Expected: FAIL (`no method named apply_changes`); both tests compile-fail until Step 2.

- [ ] **Step 2: Implement `apply_changes`**

```rust
/// Apply an incremental change set in place, producing a tree byte-identical to a
/// full `build_with` over the same final track set. `new_paths` maps every CURRENT
/// track id to its rendered path. Returns Err(()) on any inconsistency (caller
/// falls back to full build). See SP2 Component 3.
#[allow(clippy::result_unit_err)]
pub fn apply_changes(
    &mut self,
    new_paths: &std::collections::HashMap<i64, String>,
    changed: &[i64],
    added: &[i64],
    removed: &[i64],
    alloc: &mut InodeAllocator,
) -> std::result::Result<(), ()> {
    use std::collections::HashSet;
    let mut dirty: HashSet<u64> = HashSet::new();

    // Partition `changed` into path-moved vs unchanged-path (using current tree).
    let mut moved_out: Vec<i64> = Vec::new(); // remove old position
    let mut moved_in: Vec<i64> = Vec::new();  // insert new position
    for &id in changed {
        let new_path = new_paths.get(&id).ok_or(())?;
        match self.inode_of_track(id) {
            Some(ino) if &self.path_of(ino) == new_path => { /* path stable: nothing */ }
            Some(_) => { moved_out.push(id); moved_in.push(id); }
            None => { moved_in.push(id); } // expected present; treat as add
        }
    }

    // (1) Dirty set on the OLD tree, BEFORE mutating.
    for &id in removed.iter().chain(moved_out.iter()) {
        if let Some(leaf) = self.inode_of_track(id) {
            if let Some(p) = self.node(leaf).map(|n| n.parent) { dirty.insert(p); }
            // propagate up while `id` was the introducing (min) id.
            let mut child = self.node(leaf).map(|n| n.parent).unwrap_or(Self::ROOT);
            while child != Self::ROOT && self.introducing_id(child) == id {
                let p = self.node(child).map(|n| n.parent).unwrap_or(Self::ROOT);
                dirty.insert(p);
                child = p;
            }
        }
    }
    for &id in added.iter().chain(moved_in.iter()) {
        let rendered = new_paths.get(&id).ok_or(())?;
        let d = self.deepest_existing_ancestor(rendered);
        dirty.insert(d);
        // propagate up while `id` would become the new min.
        let mut child = d;
        while child != Self::ROOT && id < self.introducing_id(child) {
            let p = self.node(child).map(|n| n.parent).unwrap_or(Self::ROOT);
            dirty.insert(p);
            child = p;
        }
    }

    // (2) Structural mutation. Record surviving parents of pruned dirs as dirty.
    for &id in removed.iter().chain(moved_out.iter()) {
        if let Some(surv) = self.remove_track(id, alloc) { dirty.insert(surv); }
    }
    for &id in added.iter().chain(moved_in.iter()) {
        let rendered = new_paths.get(&id).ok_or(())?;
        self.insert_file(id, rendered, alloc);
    }

    // (3) Keep only dirty dirs that still exist; map pruned ones to ROOT-side survivor.
    let mut live_dirty: Vec<u64> = dirty.into_iter().filter(|d| self.node(*d).is_some()).collect();
    // (4) Reduce to top-most and rebuild each subtree.
    live_dirty.sort_by_key(|d| self.path_of(*d).matches('/').count()); // shallow first
    let mut done: HashSet<u64> = HashSet::new();
    for d in live_dirty {
        if self.node(d).is_none() { continue; }
        // Skip if an ancestor is already rebuilt.
        if self.ancestor_in(d, &done) { continue; }
        self.rebuild_subtree(d, new_paths, alloc)?;
        done.insert(d);
    }
    Ok(())
}

/// The deepest directory that exists in the current tree along the RENDERED path
/// `rendered` (navigating by `rendered_name`). Returns ROOT if none below it exist.
fn deepest_existing_ancestor(&self, rendered: &str) -> u64 {
    let comps: Vec<&str> = rendered.split('/').filter(|c| !c.is_empty()).collect();
    let mut dir = Self::ROOT;
    // walk dir components only (exclude the final filename component)
    for comp in &comps[..comps.len().saturating_sub(1)] {
        let next = self.children_by_rendered(dir, comp).into_iter()
            .find(|&c| self.is_dir(c));
        match next { Some(c) => dir = c, None => break }
    }
    dir
}

/// True if any inode in `set` is an ancestor of `node` (or equals it).
fn ancestor_in(&self, node: u64, set: &std::collections::HashSet<u64>) -> bool {
    let mut cur = node;
    loop {
        if set.contains(&cur) { return true; }
        if cur == Self::ROOT { return false; }
        cur = match self.nodes.get(&cur) { Some(n) => n.parent, None => return false };
    }
}
```

> The dirty set is intended a **superset** of truly-affected dirs; rebuilding a superset is still correct (just slightly more work). If the proptest (B6) finds a divergence, widen the seeds/propagation here — never touch `rebuild_subtree`.

Run: `cargo test -p musefs-core --lib apply_changes_handles_dir_vs_file -- --nocapture`
Expected: PASS.

- [ ] **Step 3: Commit**

```bash
cargo fmt && cargo clippy -p musefs-core --all-targets
git add musefs-core/src/tree.rs
git commit -m "feat(core): apply_changes with introducing-id dirty propagation (SP2 B4)"
```

---

## Task B5: Wire `apply_changes` into `rebuild_incremental` + fallback + equiv

**Files:** Modify `musefs-core/src/tree.rs` (`equiv`, `#[derive(Clone)]`), `musefs-core/src/facade.rs`.

- [ ] **Step 1: `#[derive(Clone)]` on `InodeAllocator` + `equiv` (incl. children)**

In `tree.rs`, change `#[derive(Debug)]` on `InodeAllocator` to `#[derive(Debug, Clone)]`, and add:

```rust
/// Structural equality for the equivalence oracle: identical track→inode map,
/// node set, AND children maps. See SP2 Testing item 1.
pub fn equiv(&self, other: &VirtualTree) -> bool {
    self.track_to_inode == other.track_to_inode
        && self.nodes == other.nodes
        && self.children == other.children
}
```

> `im::HashMap<K,V>` and `im::OrdMap<K,V>` implement `PartialEq` when `V: PartialEq` (`Node`, `u64` do), so the `==` comparisons compile.

- [ ] **Step 2: Replace the tail of `rebuild_incremental` (from Task A5) with the mutation path**

Replace the entries-assembly + `build_with` tail of `rebuild_incremental` with:

```rust
    let new_paths: std::collections::HashMap<i64, String> =
        new_snapshot.iter().map(|(&id, s)| (id, s.path.clone())).collect();

    let mut alloc = self.inodes.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut tree = (*self.tree.load_full()).clone(); // O(1) im clone
    let applied = if self.force_apply_fail.swap(false, Ordering::AcqRel) {
        Err(()) // test injection (Task B7)
    } else {
        tree.apply_changes(&new_paths, &change.changed, &change.added, &change.removed, &mut alloc)
    };
    let tree = match applied {
        Ok(()) => {
            #[cfg(debug_assertions)]
            {
                let mut ref_alloc = alloc.clone();
                let mut entries: Vec<(i64, String)> =
                    new_paths.iter().map(|(&id, p)| (id, p.clone())).collect();
                entries.sort_by_key(|(id, _)| *id);
                let reference = VirtualTree::build_with(&entries, &mut ref_alloc);
                debug_assert!(tree.equiv(&reference), "incremental tree diverged from build_with");
            }
            tree
        }
        Err(()) => {
            eprintln!("musefs: incremental tree mutation failed; falling back to full rebuild");
            let mut entries: Vec<(i64, String)> =
                new_paths.iter().map(|(&id, p)| (id, p.clone())).collect();
            entries.sort_by_key(|(id, _)| *id);
            VirtualTree::build_with(&entries, &mut alloc)
        }
    };
    self.tree.store(Arc::new(tree));
    drop(alloc);
    Ok((new_snapshot, change))
```

Add the `force_apply_fail: AtomicBool` field to `Musefs` (init `false` in `open`) and a `#[doc(hidden)] pub fn force_apply_failure_for_test(&self, on: bool) { self.force_apply_fail.store(on, Ordering::Release); }`.

> Lock order preserved: `inodes` is the only in-memory lock held, and no `pool.with` call happens while it's held here (the DB work — `list_render_keys`/`tags_for_tracks` — finished earlier in `rebuild_incremental` inside its own `pool.with`).

- [ ] **Step 3: Run the Stage A equivalence test against the now-incremental tree + full suite**

Run: `cargo test -p musefs-core --test incremental_refresh -- --nocapture`
Expected: PASS (the debug_assert is active in test builds and guards inode identity).

Run: `cargo test -p musefs-core -- --nocapture`
Expected: PASS.

- [ ] **Step 4: Commit**

```bash
cargo fmt && cargo clippy -p musefs-core --all-targets
git add musefs-core/src/tree.rs musefs-core/src/facade.rs
git commit -m "feat(core): wire apply_changes into rebuild_incremental + fallback (SP2 B5)"
```

---

## Task B6: Property oracle (random edits incl. collisions + cascade)

**Files:** Modify `musefs-core/tests/incremental_refresh.rs`.

- [ ] **Step 1: Add the proptest** (`proptest = "1"` is already a dev-dep)

```rust
use proptest::prelude::*;

#[derive(Clone, Debug)]
enum Op {
    Retag(usize, String, String), // retag the i-th LIVE track (forces collisions → moves)
    Delete(usize),                // delete the i-th LIVE track (remove-cascade + prune)
    Add(String, String),          // add a brand-new DB track row (added-side propagation)
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]
    #[test]
    fn incremental_equivalent_to_full_under_random_edits(
        ops in proptest::collection::vec(
            prop_oneof![
                (0usize..8, "[A-B]", "[x-y]").prop_map(|(i, a, t)| Op::Retag(i, a, t)),
                (0usize..8).prop_map(Op::Delete),
                ("[A-B]", "[x-y]").prop_map(|(a, t)| Op::Add(a, t)),
            ], 0..24)
    ) {
        let target = small_corpus(6);
        let db_path = target.db_path.clone();
        let corpus = target.corpus_dir.clone();
        let db = Db::open(&db_path).unwrap();
        scan_directory(&db, &corpus).unwrap();
        let fs = Musefs::open(Db::open(&db_path).unwrap(), config()).unwrap();
        let writer = Db::open(&db_path).unwrap();
        let mut add_seq = 0u32;

        for op in ops {
            // Re-query the live id set each step (deletes/adds change it).
            let live: Vec<i64> = writer.list_tracks().unwrap().iter().map(|t| t.id).collect();
            match op {
                Op::Retag(i, album, title) if !live.is_empty() => {
                    let _ = writer.replace_tags(live[i % live.len()], &[
                        Tag::new("ARTIST", "X", 0),
                        Tag::new("ALBUM", &album, 0),
                        Tag::new("TITLE", &title, 0),
                    ]);
                }
                Op::Delete(i) if !live.is_empty() => {
                    let _ = writer.delete_track(live[i % live.len()]);
                }
                Op::Add(album, title) => {
                    add_seq += 1;
                    // DB-only track: tree-building never reads the backing file, and
                    // both fs and reference read the same DB, so equivalence holds.
                    let new = musefs_db::NewTrack {
                        backing_path: format!("/virt/added-{add_seq}.flac"),
                        format: Format::Flac,
                        audio_offset: 0, audio_length: 1, backing_size: 1, backing_mtime: 0,
                    };
                    if let Ok(id) = writer.upsert_track(&new) {
                        let _ = writer.replace_tags(id, &[
                            Tag::new("ARTIST", "X", 0),
                            Tag::new("ALBUM", &album, 0),
                            Tag::new("TITLE", &title, 0),
                        ]);
                    }
                }
                _ => {}
            }
            fs.poll_refresh().unwrap();
            let reference = Musefs::open(Db::open(&db_path).unwrap(), config()).unwrap();
            prop_assert_eq!(
                tree_fingerprint(&fs).keys().collect::<Vec<_>>(),
                tree_fingerprint(&reference).keys().collect::<Vec<_>>()
            );
        }
    }
}
```

> Inode-identity equivalence is enforced by the in-process `debug_assert` in B5 (it runs in these test builds, using a clone of the live allocator). Do not assert raw inode numbers across the two independent `Musefs` instances — they have separate allocator histories and legitimately differ in inode numbers (only the path↔structure must match across instances). The `Op::Add` arm exercises the **added-side** introducing-id propagation; `Op::Delete` exercises remove-cascade pruning — both were missing from the earlier draft.

Run: `cargo test -p musefs-core incremental_equivalent_to_full -- --nocapture`
Expected: PASS. On failure, proptest prints the minimal failing op sequence — widen the dirty seeds in `apply_changes` (Task B4) accordingly; never change `rebuild_subtree`.

- [ ] **Step 2: Commit**

```bash
cargo fmt && cargo clippy -p musefs-core --all-targets
git add musefs-core/tests/incremental_refresh.rs
git commit -m "test(core): property oracle — incremental == full build_with (SP2 B6)"
```

---

## Task B7: Fallback test + Stage B bench + regression gate

**Files:** `musefs-core/tests/incremental_refresh.rs`, `musefs-core/tests/bench_refresh.rs`, `BENCHMARKS.md`, spec README.

- [ ] **Step 1: Fallback test** (uses `force_apply_failure_for_test` from B5)

```rust
#[test]
fn apply_failure_falls_back_to_full_rebuild() {
    let target = small_corpus(4);
    let db_path = target.db_path.clone();
    let corpus = target.corpus_dir.clone();
    let db = Db::open(&db_path).unwrap();
    scan_directory(&db, &corpus).unwrap();
    let fs = Musefs::open(Db::open(&db_path).unwrap(), config()).unwrap();
    let writer = Db::open(&db_path).unwrap();
    let id = writer.list_tracks().unwrap()[0].id;
    writer.replace_tags(id, &[Tag::new("TITLE", "moved", 0)]).unwrap();

    fs.force_apply_failure_for_test(true);
    fs.poll_refresh().unwrap();

    let reference = Musefs::open(Db::open(&db_path).unwrap(), config()).unwrap();
    assert_eq!(
        tree_fingerprint(&fs).keys().collect::<Vec<_>>(),
        tree_fingerprint(&reference).keys().collect::<Vec<_>>(),
    );
}
```

Run: `cargo test -p musefs-core apply_failure_falls_back -- --nocapture`
Expected: PASS.

- [ ] **Step 2: Re-run the library-size sweep — expect flat refresh-1 across sizes**

Run: `cargo test -p musefs-core --test bench_refresh bench_refresh_one_across_library_sizes -- --ignored --nocapture`
Expected: `refresh-1@N` wall time **flat across N** (strict O(changed)) vs the Stage A baseline from A7.

- [ ] **Step 3: Full regression gate**

```bash
cargo test                               # all crates, excludes #[ignore] e2e
cargo test -p musefs-fuse -- --ignored   # byte-identical audio e2e (needs /dev/fuse)
cargo bench -p musefs-core --bench read_throughput   # ci sequential_read median within 10%
cargo clippy --all-targets
```
Expected: all green; read median within 10% of baseline.

- [ ] **Step 4: Record numbers + flip status**

Append Stage A-vs-B refresh numbers to `BENCHMARKS.md` and the spec README results log; update the README status row: `| SP2 | Implemented | SP2-incremental-tree-refresh.md | 2026-05-31-sp2-incremental-tree-refresh.md | ... |`.

- [ ] **Step 5: Commit**

```bash
cargo fmt && cargo clippy --all-targets
git add musefs-core/tests BENCHMARKS.md docs/superpowers/specs/2026-05-30-optimization-pass/README.md
git commit -m "test+bench(core): Stage B fallback + flat-across-size refresh; SP2 done (SP2 B7)"
```

---

# RESUME NOTE — Stage A complete; pick up at B1 (added 2026-05-31)

Stage A (A1–A7) is **implemented, reviewed, and committed** on branch
`sp2-incremental-tree-refresh` (HEAD `3aa97b7` at handoff). Verified checkpoint:
`cargo test` → 515 passed / 20 ignored; `cargo clippy --all-targets` → clean;
working tree clean. Stage A commits (one per task):

- A1 `d4de29c` `Db::list_render_keys`
- A2 `40217a8` `Db::tags_for_tracks`
- A3 `7af9df9` `refresh_diff.rs` (`TrackRenderState`/`ChangeSet`/`partition_changes`)
- A4 `e59cc13` versions map → `TrackRenderState` snapshot + `notify_changed`
- A5 `6cd6b71` `rebuild_incremental` (changed-only render) + equivalence oracle
- A6 `efd433e` change-detection + format-only invalidation tests
- A7 `3aa97b7` library-size refresh bench + recorded Stage A baseline

**Start at Task B1.** Use superpowers:subagent-driven-development. The B1–B7 task
text above is authoritative; the notes below are session-discovered facts that are
NOT otherwise in the plan and will save a failed-commit or two.

1. **Pre-commit hook runs `cargo clippy --all-targets -- -D warnings`.** Every
   commit is rejected on ANY warning (unused import, `dead_code`, unused var).
   Run `cargo fmt && cargo clippy --all-targets -- -D warnings` before each commit.
   Gate types staged ahead of their caller with a targeted `#[allow(dead_code)]`;
   a redundant `allow` on a now-used item is harmless.

2. **`tree.rs` and `musefs-core/Cargo.toml` are pristine vs. the plan's pins** —
   Stage A did not touch them, so B1's `VirtualTree` internals
   (`nodes`/`children`/`track_to_inode`, `InodeAllocator`) and the `im` dep-add are
   exactly as the plan describes.

3. **Test helpers already exist** in `musefs-core/tests/incremental_refresh.rs`
   (created A5/A6): `small_corpus(n)`, `config()`, `tree_fingerprint(fs)`. **B6
   appends to this file — do NOT redefine them.**

4. **`write_min_corpus` does not exist** (see the "Test corpus helper" section near
   the top). Use `small_corpus(n)` / `prepare(&CorpusParams::single(Format::Flac, 1, n))`.

5. **Format-type clash in test files (bit A6):** `small_corpus`/`CorpusParams::single`
   take **`common::corpus::Format`**, while `NewTrack`/`set_format_for_test` take
   **`musefs_db::Format`**. The test file already imports `Format` from
   `common::corpus`, so B6's `Op::Add` (`musefs_db::NewTrack { format: … }`) must
   write **`musefs_db::Format::Flac` fully-qualified** to avoid the clash.

6. **B5 wiring:** `poll_refresh_notify` currently discards the changeset as
   `let (new_snapshot, _change) = self.rebuild_incremental(&old_snapshot)?` — B5
   wires `_change` into `apply_changes`. `refresh_diff.rs` still carries harmless
   `#[allow(dead_code)]` on `ChangeSet`/`is_empty`/`partition_changes`.

7. **B4 is the hard task.** `apply_changes` (introducing-id dirty propagation) is
   where correctness lives. Honor the plan's discipline: if the B6 property oracle
   finds a divergence, **widen the dirty set in `apply_changes` — never change
   `rebuild_subtree`.** Use a capable model for B4/B5 and treat the B6 proptest +
   the B5 debug-assert (`tree.equiv(&build_with(...))`, paths AND inodes) as the
   gates.

8. **A4 contract note (context, not action):** `notify_changed` fires the OLD inode
   on a path move and the (stable) inode on an in-place content change; a moved
   track's new path gets a freshly-allocated inode with no kernel cache, so it is
   intentionally NOT notified. One pre-existing facade test was updated to assert
   this. Cross-instance equivalence compares **path keys only** (two independent
   `Musefs` allocators legitimately assign different inode numbers); inode
   *stability within one instance* is what B5's debug-assert guards.
