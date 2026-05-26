# musefs Optimization — Phase 4 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make refresh cheap and non-blocking at scale — debounce the `data_version` poll, run the tree rebuild off the FUSE dispatch thread with single-flight de-duplication, replace the N+1 tag query with a batched one, and keep inodes stable across rebuilds via a persistent path→inode allocator.

**Architecture:** `poll_refresh` gains a time-based debounce (skip the `PRAGMA data_version` if polled within an interval) and an `AtomicBool` single-flight guard around the rebuild; the FUSE layer fires `poll_refresh` on the worker pool (fire-and-forget) instead of inline, so a rebuild never blocks dispatch and readers keep the prior `ArcSwap` tree snapshot until the swap. `build_tree` reads all tags in one grouped query (2 queries total, not N+1). A persistent `InodeAllocator` keyed by rendered path is carried in `Musefs` across rebuilds so an unchanged path keeps its inode (active streams survive a refresh); retired inodes are never recycled.

**Tech Stack:** Rust, `fuser` 0.14, `rusqlite` (WAL), existing `arc-swap`/`threadpool`. No new dependencies.

**Builds on (already on `main`):** Phases 1–3 — `Musefs` is `&self` interior-mutable (`ArcSwap<VirtualTree>` tree, sharded `HeaderCache`, size cache, `AtomicI64` data-version, handle table); `DbPool` (`with`/`with_poll`); FUSE ops offload to a `threadpool`; `poll_refresh` currently runs **inline** on the dispatch thread at the top of `lookup`/`getattr`/`readdir` and does: `with_poll(data_version)` → on change `refresh()` (O(library), N+1 `get_tags`) → prune caches via `retain(track_ids)` → store the stamp. `metrics` feature with counters + fault injection; `metrics.rs` tests serialize via a `METRICS_LOCK` static.

**Invariant:** served audio stays byte-identical (e2e gate: `cargo test -p musefs-fuse -- --ignored --test-threads=1`; `/dev/fuse` available). Every task also runs `cargo fmt --check` and `cargo clippy --all-targets`.

**Behavior note (intended):** with the FUSE-level fire-and-forget poll, an external DB edit becomes visible *shortly after* the next metadata op (once the background rebuild swaps the tree), not synchronously on that op — bounded by the debounce interval + rebuild time. This is the documented trade-off for never blocking dispatch. The core `poll_refresh` remains synchronous (returns `bool`), so the existing `poll_refresh_picks_up_external_db_edits` test keeps working.

---

## File Structure

- `musefs-core/src/facade.rs` — `MountConfig.poll_interval`; `Musefs` gains `last_poll: Mutex<Instant>`, `refreshing: AtomicBool`, `inodes: Mutex<InodeAllocator>`; `poll_refresh` (debounce + single-flight); `build_tree` (batched query + allocator); `open`/`refresh` wiring.
- `musefs-db/src/tags.rs` — `tags_grouped()` batched query.
- `musefs-core/src/tree.rs` — `InodeAllocator` + `VirtualTree::build_with`; `build` delegates with a fresh allocator (behavior-identical).
- `musefs-fuse/src/lib.rs` — fire `poll_refresh` on the worker pool instead of inline.
- `musefs-cli/src/*` — `--poll-interval-ms` flag → `MountConfig.poll_interval`.
- Test config() helpers (`tests/facade.rs`, `tests/metrics.rs`, `benches/read_throughput.rs`) — add `poll_interval`.

---

## Task 4a: Debounce `poll_refresh`

**Files:** `musefs-core/src/facade.rs`; test config() helpers; `musefs-cli/src/*`.

Add a configurable debounce so a metadata-walk storm makes at most one `PRAGMA data_version` per interval instead of one per op.

- [ ] **Step 1: Add `poll_interval` to `MountConfig` and update the test/bench config() helpers + CLI**

In `facade.rs`, add to `MountConfig` (keep existing fields):
```rust
    /// Minimum time between `data_version` polls; a metadata-op storm within this
    /// window skips the poll entirely. `Duration::ZERO` disables debouncing.
    pub poll_interval: std::time::Duration,
```
Update EVERY `MountConfig { ... }` literal in the codebase to set it. In the three test/bench helpers (`musefs-core/tests/facade.rs` `config()`, `musefs-core/tests/metrics.rs` `config()`, `musefs-core/benches/read_throughput.rs` `config()`) use **`poll_interval: std::time::Duration::ZERO,`** (tests must not be debounced, or `poll_refresh`-detection tests would break). Find them: `grep -rn "MountConfig {" musefs-core`.

In the CLI (`musefs-cli` — find the `mount` command's `MountConfig` construction with `grep -rn "MountConfig" musefs-cli/src`), add a clap flag and use it:
```rust
    /// Debounce window for picking up external DB edits, in milliseconds.
    #[arg(long, default_value_t = 1000)]
    poll_interval_ms: u64,
```
and set `poll_interval: std::time::Duration::from_millis(args.poll_interval_ms)` (adapt names to the CLI's actual arg struct/flow).

- [ ] **Step 2: Write the failing debounce test**

Add to `musefs-core/tests/facade.rs` (mirror `poll_refresh_picks_up_external_db_edits`'s file-backed setup):
```rust
#[test]
fn poll_refresh_debounces_within_interval() {
    use musefs_db::{Format, NewTrack, Tag};
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("m.db");
    {
        let db = musefs_db::Db::open(&db_path).unwrap();
        let id = db.upsert_track(&NewTrack {
            backing_path: "/x/a.flac".to_string(), format: Format::Flac,
            audio_offset: 0, audio_length: 0, backing_size: 0, backing_mtime: 0,
        }).unwrap();
        db.replace_tags(id, &[Tag::new("artist", "Alice", 0), Tag::new("title", "A", 0)]).unwrap();
    }
    // A long debounce interval: the first poll after open is within the window.
    let cfg = MountConfig {
        poll_interval: std::time::Duration::from_secs(3600),
        ..config()
    };
    let fs = Musefs::open(musefs_db::Db::open(&db_path).unwrap(), cfg).unwrap();

    // External commit, then poll — but we're within the debounce window of open.
    {
        let db2 = musefs_db::Db::open(&db_path).unwrap();
        let id = db2.upsert_track(&NewTrack {
            backing_path: "/x/b.flac".to_string(), format: Format::Flac,
            audio_offset: 0, audio_length: 0, backing_size: 0, backing_mtime: 0,
        }).unwrap();
        db2.replace_tags(id, &[Tag::new("artist", "Bob", 0), Tag::new("title", "B", 0)]).unwrap();
    }
    // Debounced: returns false and does NOT pick up the change yet.
    assert!(!fs.poll_refresh().unwrap());
    assert!(fs.lookup(VirtualTree::ROOT, "Bob").is_none());
}
```
This requires `MountConfig` to derive enough for `..config()` struct-update (it already derives `Clone`; struct-update needs the rest of the fields to be moved/copied from `config()` — that works with `..config()`). Run `cargo test -p musefs-core --test facade poll_refresh_debounces_within_interval 2>&1 | head -20` → FAIL (no `poll_interval` field yet / not debounced).

- [ ] **Step 3: Add debounce state + logic**

Add to `Musefs` (keep existing fields):
```rust
    last_poll: Mutex<std::time::Instant>,
    poll_interval: std::time::Duration,
```
In `Musefs::open`, initialize (note: store `config.poll_interval` before moving `config`):
```rust
        let poll_interval = config.poll_interval;
        Ok(Musefs {
            // ... existing fields ...
            last_poll: Mutex::new(std::time::Instant::now()),
            poll_interval,
        })
```
At the TOP of `poll_refresh` (before the `with_poll` data_version read), add the debounce gate:
```rust
    pub fn poll_refresh(&self) -> Result<bool> {
        // Debounce: skip the poll (and its PRAGMA) if we polled within the window.
        if !self.poll_interval.is_zero() {
            let mut last = self.last_poll.lock().unwrap_or_else(|p| p.into_inner());
            if last.elapsed() < self.poll_interval {
                return Ok(false);
            }
            *last = std::time::Instant::now();
        }
        // ... existing body (with_poll data_version, compare, refresh, retain, store) ...
    }
```

- [ ] **Step 4: Run tests**

Run: `cargo test -p musefs-core` → all pass, incl. the new debounce test AND the existing `poll_refresh_picks_up_external_db_edits` (its config uses `Duration::ZERO`, so it still detects).
Run: `cargo build` (workspace, incl. CLI) → clean.
Run: `cargo clippy --all-targets` and `cargo fmt --check` → clean.

- [ ] **Step 5: Commit**

```bash
git add musefs-core/src/facade.rs musefs-core/tests/facade.rs musefs-core/tests/metrics.rs musefs-core/benches/read_throughput.rs musefs-cli
git commit -m "$(cat <<'EOF'
feat(core): debounce poll_refresh (configurable interval; --poll-interval-ms)

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 4b: Batched tree-build query (eliminate N+1)

**Files:** `musefs-db/src/tags.rs`; `musefs-core/src/facade.rs` (`build_tree`).

`build_tree` currently calls `get_tags` per track (N+1). Replace with `list_tracks` + one grouped tags query.

- [ ] **Step 1: Write the failing db test**

Add to `musefs-db/src/tags.rs`'s inline `#[cfg(test)] mod tests` (or the tags integration test file `musefs-db/tests/tags.rs` — match where the existing tag tests live; check with `grep -rn "fn .*tag" musefs-db/tests/tags.rs`):
```rust
    #[test]
    fn tags_grouped_returns_all_tags_by_track() {
        let db = Db::open_in_memory().unwrap();
        let a = db.upsert_track(&NewTrack { backing_path: "/a".into(), format: Format::Flac, audio_offset: 0, audio_length: 0, backing_size: 0, backing_mtime: 0 }).unwrap();
        let b = db.upsert_track(&NewTrack { backing_path: "/b".into(), format: Format::Flac, audio_offset: 0, audio_length: 0, backing_size: 0, backing_mtime: 0 }).unwrap();
        db.replace_tags(a, &[Tag::new("artist", "Alice", 0), Tag::new("title", "A", 0)]).unwrap();
        db.replace_tags(b, &[Tag::new("artist", "Bob", 0)]).unwrap();

        let grouped = db.tags_grouped().unwrap();
        assert_eq!(grouped.get(&a).map(|v| v.len()), Some(2));
        assert_eq!(grouped.get(&b).map(|v| v.len()), Some(1));
        // grouping must match per-track get_tags exactly (same order).
        assert_eq!(grouped.get(&a), Some(&db.get_tags(a).unwrap()));
        assert_eq!(grouped.get(&b), Some(&db.get_tags(b).unwrap()));
    }
```
(Ensure the test module imports `NewTrack`, `Format`, `Tag` — match the existing tests' imports.) Run `cargo test -p musefs-db tags_grouped 2>&1 | head -20` → FAIL (no `tags_grouped`).

- [ ] **Step 2: Implement `tags_grouped` in `tags.rs`**

Add to `impl Db` (mirror `get_tags`'s ordering `ORDER BY key, ordinal` per track so the grouped result is byte-identical to per-track `get_tags`):
```rust
    /// All tags for all tracks in one query, grouped by track id. Matches
    /// `get_tags`'s per-track ordering (`key, ordinal`), so callers can use it as
    /// a drop-in batch replacement for N calls to `get_tags`.
    pub fn tags_grouped(&self) -> Result<std::collections::HashMap<i64, Vec<Tag>>> {
        let mut stmt = self.conn.prepare(
            "SELECT track_id, key, value, ordinal FROM tags ORDER BY track_id, key, ordinal",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                Tag { key: r.get(1)?, value: r.get(2)?, ordinal: r.get(3)? },
            ))
        })?;
        let mut out: std::collections::HashMap<i64, Vec<Tag>> = std::collections::HashMap::new();
        for row in rows {
            let (track_id, tag) = row?;
            out.entry(track_id).or_default().push(tag);
        }
        Ok(out)
    }
```
(`Tag` is in scope in `tags.rs`. Confirm the `use` for `Result`/`params` matches the file.)

- [ ] **Step 3: Use it in `build_tree` (facade.rs)**

Replace the N+1 loop. Current `build_tree`:
```rust
    fn build_tree(db: &Db, config: &MountConfig) -> Result<VirtualTree> {
        let tracks = db.list_tracks()?;
        let mut entries = Vec::with_capacity(tracks.len());
        for t in &tracks {
            let tags = db.get_tags(t.id)?;
            let fields = tags_to_fields(&tags);
            let path = render_path(&config.template, &fields, &config.fallbacks, &config.default_fallback, t.format.as_str());
            entries.push((t.id, path));
        }
        Ok(VirtualTree::build(&entries))
    }
```
becomes:
```rust
    fn build_tree(db: &Db, config: &MountConfig) -> Result<VirtualTree> {
        let tracks = db.list_tracks()?;
        let mut tags_by_track = db.tags_grouped()?;
        let mut entries = Vec::with_capacity(tracks.len());
        for t in &tracks {
            let tags = tags_by_track.remove(&t.id).unwrap_or_default();
            let fields = tags_to_fields(&tags);
            let path = render_path(&config.template, &fields, &config.fallbacks, &config.default_fallback, t.format.as_str());
            entries.push((t.id, path));
        }
        Ok(VirtualTree::build(&entries))
    }
```
(`remove` moves the `Vec<Tag>` out — no clone; a track with no tags gets `unwrap_or_default()` = empty, identical to `get_tags` returning empty.)

- [ ] **Step 4: Run tests**

Run: `cargo test -p musefs-db tags_grouped` → PASS.
Run: `cargo test -p musefs-core` → all pass (tree-build behavior unchanged: same entries → same tree). Especially `lookup_getattr_readdir_and_read_through_the_facade` and `poll_refresh_*`.
Run: `cargo test -p musefs-fuse -- --ignored --test-threads=1` → 5 e2e pass.
Run: `cargo clippy --all-targets` and `cargo fmt --check` → clean.

- [ ] **Step 5: Commit**

```bash
git add musefs-db/src/tags.rs musefs-db/tests/tags.rs musefs-core/src/facade.rs
git commit -m "$(cat <<'EOF'
perf(db,core): batch tree-build tags into one grouped query (drop N+1)

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```
(Drop `musefs-db/tests/tags.rs` from the `git add` if you put the test in the inline module instead.)

---

## Task 4c: Off-thread + single-flight rebuild

**Files:** `musefs-core/src/facade.rs` (`poll_refresh` single-flight); `musefs-fuse/src/lib.rs` (fire poll on the pool).

Make the (now-batched, debounced) rebuild run off the dispatch thread and never run twice concurrently.

- [ ] **Step 1: Write the failing single-flight test**

Add to `musefs-core/tests/facade.rs`:
```rust
#[test]
fn poll_refresh_single_flights_concurrent_callers() {
    use musefs_db::{Format, NewTrack, Tag};
    use std::sync::Arc;
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("m.db");
    {
        let db = musefs_db::Db::open(&db_path).unwrap();
        let id = db.upsert_track(&NewTrack { backing_path: "/x/a.flac".into(), format: Format::Flac, audio_offset: 0, audio_length: 0, backing_size: 0, backing_mtime: 0 }).unwrap();
        db.replace_tags(id, &[Tag::new("artist", "Alice", 0), Tag::new("title", "A", 0)]).unwrap();
    }
    // Debounce off so every caller reaches the data_version check.
    let cfg = MountConfig { poll_interval: std::time::Duration::ZERO, ..config() };
    let fs = Arc::new(Musefs::open(musefs_db::Db::open(&db_path).unwrap(), cfg).unwrap());

    // External commit.
    {
        let db2 = musefs_db::Db::open(&db_path).unwrap();
        let id = db2.upsert_track(&NewTrack { backing_path: "/x/b.flac".into(), format: Format::Flac, audio_offset: 0, audio_length: 0, backing_size: 0, backing_mtime: 0 }).unwrap();
        db2.replace_tags(id, &[Tag::new("artist", "Bob", 0), Tag::new("title", "B", 0)]).unwrap();
    }

    // Many threads poll concurrently; exactly ONE performs the rebuild (returns true).
    let trues: usize = std::thread::scope(|s| {
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let fs = Arc::clone(&fs);
                s.spawn(move || if fs.poll_refresh().unwrap() { 1usize } else { 0 })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).sum()
    });
    assert_eq!(trues, 1, "single-flight: exactly one caller rebuilds");
    assert!(fs.lookup(VirtualTree::ROOT, "Bob").is_some(), "change is visible after the rebuild");
}
```
Run `cargo test -p musefs-core --test facade poll_refresh_single_flights_concurrent_callers 2>&1 | head -20` → likely FAIL (without single-flight, multiple callers can each detect the change and return true / race the rebuild).

- [ ] **Step 2: Add the single-flight guard to `poll_refresh`**

Add to `Musefs`:
```rust
    refreshing: std::sync::atomic::AtomicBool,
```
Init in `open`: `refreshing: std::sync::atomic::AtomicBool::new(false),`.
Restructure the post-debounce body of `poll_refresh` so the rebuild is single-flighted (only the thread that flips `refreshing` false→true rebuilds; others bail):
```rust
        let version = self.pool.with_poll(|db| Ok(db.data_version()?))?;
        if version == self.last_data_version.load(Ordering::Acquire) {
            return Ok(false);
        }
        // Single-flight: only one caller rebuilds at a time; others see the change
        // is being handled and return without duplicating the O(library) work.
        use std::sync::atomic::AtomicBool;
        if self
            .refreshing
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Ok(false);
        }
        // Ensure the flag is always cleared, even on an early `?` return.
        let result = (|| {
            self.refresh()?;
            let live = self.tree.load().track_ids();
            self.cache.retain(&live);
            self.size_cache().retain(|k, _| live.contains(k));
            self.last_data_version.store(version, Ordering::Release);
            Ok(true)
        })();
        self.refreshing.store(false, Ordering::Release);
        result
```
(Keep the debounce block from Task 4a above this. Remove the old non-single-flight refresh/retain/store lines — they're replaced by the closure above. The `use std::sync::atomic::AtomicBool;` is only needed if not already imported; `Ordering` is already imported.)

- [ ] **Step 3: Fire `poll_refresh` on the worker pool in the FUSE layer**

In `musefs-fuse/src/lib.rs`, the three inline `let _ = self.core.poll_refresh();` calls (in `lookup`, `getattr`, `readdir`) each become a fire-and-forget pool job so the rebuild never blocks dispatch:
```rust
        {
            let core = Arc::clone(&self.core);
            self.pool.execute(move || {
                let _ = core.poll_refresh();
            });
        }
```
Replace each of the three inline calls with this block (it's debounced + single-flighted inside, so it's cheap and self-deduplicating). The metadata op then proceeds with the current tree snapshot; the external edit appears once the background rebuild swaps the tree.

- [ ] **Step 4: Run tests**

Run: `cargo test -p musefs-core --test facade poll_refresh_single_flights_concurrent_callers` → PASS (`trues == 1`). Run it 5× for non-flakiness: `for i in 1 2 3 4 5; do cargo test -p musefs-core --test facade poll_refresh_single_flights_concurrent_callers 2>&1 | grep "test result"; done`.
Run: `cargo test -p musefs-core` → all pass incl. existing `poll_refresh_picks_up_external_db_edits` (single caller → it wins the flag → true).
Run: `cargo test -p musefs-fuse -- --ignored --test-threads=1` → 5 e2e pass (reads still byte-identical; the mount now refreshes off-thread).
Run: `cargo clippy --all-targets` and `cargo fmt --check` → clean.

- [ ] **Step 5: Commit**

```bash
git add musefs-core/src/facade.rs musefs-core/tests/facade.rs musefs-fuse/src/lib.rs
git commit -m "$(cat <<'EOF'
perf(core,fuse): single-flight rebuild; fire poll_refresh off the dispatch thread

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 4d: Stable inodes across rebuilds (persistent path→inode allocator)

**Files:** `musefs-core/src/tree.rs` (`InodeAllocator` + `build_with`); `musefs-core/src/facade.rs` (carry the allocator, use it in `build_tree`).

So an active stream's inode survives a refresh; a vanished path's inode is never recycled into a new path.

- [ ] **Step 1: Write the failing test (tree.rs inline tests)**

Add to `tree.rs`'s inline `#[cfg(test)] mod tests` (check the module name with `grep -n "mod tests" musefs-core/src/tree.rs`; match its `use super::*;`):
```rust
    #[test]
    fn build_with_keeps_inodes_stable_across_rebuilds() {
        let mut alloc = InodeAllocator::new();
        let t1 = VirtualTree::build_with(&[(10, "Alice/Song.flac".into())], &mut alloc);
        let alice1 = t1.lookup(VirtualTree::ROOT, "Alice").unwrap();
        let song1 = t1.lookup(alice1, "Song.flac").unwrap();

        // Rebuild with the SAME allocator + an added track.
        let t2 = VirtualTree::build_with(
            &[(10, "Alice/Song.flac".into()), (20, "Bob/Other.flac".into())],
            &mut alloc,
        );
        let alice2 = t2.lookup(VirtualTree::ROOT, "Alice").unwrap();
        let song2 = t2.lookup(alice2, "Song.flac").unwrap();
        // Unchanged paths keep their inodes.
        assert_eq!(alice1, alice2);
        assert_eq!(song1, song2);
        // The new path got a fresh inode distinct from the existing ones.
        let bob2 = t2.lookup(VirtualTree::ROOT, "Bob").unwrap();
        assert!(bob2 != alice2 && bob2 != song2);
    }

    #[test]
    fn build_with_does_not_recycle_a_vanished_inode() {
        let mut alloc = InodeAllocator::new();
        let t1 = VirtualTree::build_with(&[(10, "Gone/X.flac".into())], &mut alloc);
        let gone = t1.lookup(VirtualTree::ROOT, "Gone").unwrap();
        let x = t1.lookup(gone, "X.flac").unwrap();
        // Rebuild WITHOUT the old track, WITH a new one.
        let t2 = VirtualTree::build_with(&[(20, "New/Y.flac".into())], &mut alloc);
        let new = t2.lookup(VirtualTree::ROOT, "New").unwrap();
        let y = t2.lookup(new, "Y.flac").unwrap();
        // New paths must not reuse the retired inodes.
        assert!(new != gone && new != x && y != gone && y != x);
    }
```
Run `cargo test -p musefs-core build_with 2>&1 | head -20` → FAIL (`InodeAllocator` / `build_with` not found).

- [ ] **Step 2: Add `InodeAllocator` and `build_with`; make `build` delegate**

In `tree.rs`, add the allocator (near the top, after the imports):
```rust
/// Assigns stable inodes keyed by rendered path, persisted across tree rebuilds:
/// an unchanged path keeps its inode, a new path gets a fresh one, and a retired
/// inode is never recycled (a stale FUSE handle can't alias a different node).
#[derive(Debug, Default)]
pub struct InodeAllocator {
    paths: HashMap<String, u64>,
    next: u64,
}

impl InodeAllocator {
    pub fn new() -> InodeAllocator {
        let mut paths = HashMap::new();
        paths.insert(String::new(), VirtualTree::ROOT); // root path "" -> inode 1
        InodeAllocator { paths, next: 2 }
    }
    /// The inode for `path` (the disambiguated path from root), reused if seen
    /// before, else freshly allocated.
    fn intern(&mut self, path: &str) -> u64 {
        if let Some(&ino) = self.paths.get(path) {
            return ino;
        }
        let ino = self.next;
        self.next += 1;
        self.paths.insert(path.to_string(), ino);
        ino
    }
}
```
Refactor `build` to delegate to a new `build_with` using a fresh allocator (behavior-identical for a single build — see Step 4 note), and add the path-tracking variants of `insert_file`/`ensure_dir`. Replace `build`, `insert_file`, `ensure_dir` and add `build_with`:
```rust
    pub fn build(entries: &[(i64, String)]) -> VirtualTree {
        VirtualTree::build_with(entries, &mut InodeAllocator::new())
    }

    /// Build the tree assigning inodes via `alloc` (keyed by rendered path), so
    /// inodes are stable across rebuilds that reuse the same allocator.
    pub fn build_with(entries: &[(i64, String)], alloc: &mut InodeAllocator) -> VirtualTree {
        let mut tree = VirtualTree {
            nodes: HashMap::new(),
            children: HashMap::new(),
            next_inode: 0, // unused; inodes now come from `alloc`
        };
        tree.nodes.insert(
            Self::ROOT,
            Node { parent: Self::ROOT, name: String::new(), kind: NodeKind::Dir },
        );
        tree.children.insert(Self::ROOT, BTreeMap::new());
        for (track_id, path) in entries {
            tree.insert_file(*track_id, path, alloc);
        }
        tree
    }

    fn insert_file(&mut self, track_id: i64, path: &str, alloc: &mut InodeAllocator) {
        let comps: Vec<&str> = path.split('/').filter(|c| !c.is_empty()).collect();
        if comps.is_empty() {
            return;
        }
        let mut dir = Self::ROOT;
        let mut dir_path = String::new(); // disambiguated path of `dir`
        for comp in &comps[..comps.len() - 1] {
            let (child, child_path) = self.ensure_dir(dir, &dir_path, comp, alloc);
            dir = child;
            dir_path = child_path;
        }
        let name = self.disambiguate(dir, comps[comps.len() - 1]);
        let full = join_path(&dir_path, &name);
        let inode = alloc.intern(&full);
        self.nodes.insert(
            inode,
            Node { parent: dir, name: name.clone(), kind: NodeKind::File { track_id } },
        );
        self.children.get_mut(&dir).unwrap().insert(name, inode);
    }

    fn ensure_dir(
        &mut self,
        parent: u64,
        parent_path: &str,
        name: &str,
        alloc: &mut InodeAllocator,
    ) -> (u64, String) {
        if let Some(&existing) = self.children[&parent].get(name) {
            if self.is_dir(existing) {
                return (existing, join_path(parent_path, name));
            }
        }
        let unique = self.disambiguate(parent, name);
        let full = join_path(parent_path, &unique);
        let inode = alloc.intern(&full);
        self.nodes.insert(
            inode,
            Node { parent, name: unique.clone(), kind: NodeKind::Dir },
        );
        self.children.insert(inode, BTreeMap::new());
        self.children.get_mut(&parent).unwrap().insert(unique, inode);
        (inode, full)
    }
```
Add a small free helper near the bottom of `tree.rs` (outside the `impl`):
```rust
/// Join a parent path and a child name with `/`, treating an empty parent as root.
fn join_path(parent: &str, name: &str) -> String {
    if parent.is_empty() {
        name.to_string()
    } else {
        format!("{parent}/{name}")
    }
}
```
The `alloc()` method (`self.next_inode += 1`) is now unused — remove it (`grep -n "fn alloc" tree.rs`; confirm no callers remain) to avoid a dead-code warning. `next_inode` field is now write-only — you may keep it (set to 0) or remove it; if removing, drop it from the struct + the `build_with` initializer. Simplest: **remove the `next_inode` field and the `alloc` method** since inodes come solely from the allocator.

- [ ] **Step 3: Carry the allocator in `Musefs` and use it in `build_tree`**

In `facade.rs`, add a field:
```rust
    inodes: Mutex<crate::tree::InodeAllocator>,
```
(Add `use crate::tree::InodeAllocator;` or reference the full path.) Change `build_tree` to take the allocator, and update `open`/`refresh`:
```rust
    fn build_tree(db: &Db, config: &MountConfig, alloc: &mut InodeAllocator) -> Result<VirtualTree> {
        let tracks = db.list_tracks()?;
        let mut tags_by_track = db.tags_grouped()?;
        let mut entries = Vec::with_capacity(tracks.len());
        for t in &tracks {
            let tags = tags_by_track.remove(&t.id).unwrap_or_default();
            let fields = tags_to_fields(&tags);
            let path = render_path(&config.template, &fields, &config.fallbacks, &config.default_fallback, t.format.as_str());
            entries.push((t.id, path));
        }
        Ok(VirtualTree::build_with(&entries, alloc))
    }
```
`open` builds the initial tree with a fresh allocator, then stores it:
```rust
    pub fn open(db: Db, config: MountConfig) -> Result<Musefs> {
        let mut alloc = InodeAllocator::new();
        let tree = Self::build_tree(&db, &config, &mut alloc)?;
        let last_data_version = db.data_version()?;
        let poll_interval = config.poll_interval;
        Ok(Musefs {
            cache: HeaderCache::new(config.mode),
            last_data_version: AtomicI64::new(last_data_version),
            tree: ArcSwap::from_pointee(tree),
            pool: DbPool::new(db)?,
            // ... handles, next_fh, size_cache, last_poll, poll_interval, refreshing ...
            inodes: Mutex::new(alloc),
            config,
        })
    }
```
`refresh` reuses the persistent allocator:
```rust
    pub fn refresh(&self) -> Result<()> {
        let tree = self.pool.with(|db| {
            let mut alloc = self.inodes.lock().unwrap_or_else(|p| p.into_inner());
            Self::build_tree(db, &self.config, &mut alloc)
        })?;
        self.tree.store(Arc::new(tree));
        Ok(())
    }
```
(The `inodes` lock is held only during the rebuild, which is single-flighted — no contention. Note this nests the `inodes` lock inside `pool.with`, consistent with the documented order: pool → in-memory locks. Update the lock-order comment to include `inodes`.)

- [ ] **Step 4: Write a Musefs-level stable-inode test + run everything**

Add to `musefs-core/tests/facade.rs` (proves stability through the public API across a refresh):
```rust
#[test]
fn inode_is_stable_across_refresh() {
    use musefs_db::{Format, NewTrack, Tag};
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("m.db");
    {
        let db = musefs_db::Db::open(&db_path).unwrap();
        let id = db.upsert_track(&NewTrack { backing_path: "/x/a.flac".into(), format: Format::Flac, audio_offset: 0, audio_length: 0, backing_size: 0, backing_mtime: 0 }).unwrap();
        db.replace_tags(id, &[Tag::new("artist", "Alice", 0), Tag::new("title", "A", 0)]).unwrap();
    }
    let cfg = MountConfig { poll_interval: std::time::Duration::ZERO, ..config() };
    let fs = Musefs::open(musefs_db::Db::open(&db_path).unwrap(), cfg).unwrap();
    let alice = fs.lookup(VirtualTree::ROOT, "Alice").unwrap();
    let (_, song_before, _) = fs.readdir(alice).unwrap().into_iter().next().unwrap();

    // Unrelated external commit + refresh.
    {
        let db2 = musefs_db::Db::open(&db_path).unwrap();
        let id = db2.upsert_track(&NewTrack { backing_path: "/x/b.flac".into(), format: Format::Flac, audio_offset: 0, audio_length: 0, backing_size: 0, backing_mtime: 0 }).unwrap();
        db2.replace_tags(id, &[Tag::new("artist", "Bob", 0), Tag::new("title", "B", 0)]).unwrap();
    }
    assert!(fs.poll_refresh().unwrap());

    // Alice and her song keep the SAME inodes across the refresh.
    let alice_after = fs.lookup(VirtualTree::ROOT, "Alice").unwrap();
    let (_, song_after, _) = fs.readdir(alice_after).unwrap().into_iter().next().unwrap();
    assert_eq!(alice, alice_after);
    assert_eq!(song_before, song_after);
}
```
Run: `cargo test -p musefs-core` → all pass (incl. the tree-level `build_with` tests, this Musefs-level test, and all existing tree/facade tests — single-build inode numbers are unchanged because a fresh allocator interns in the same traversal order the old `alloc()` used).
Run: `cargo test -p musefs-fuse -- --ignored --test-threads=1` → 5 e2e pass.
Run: `cargo clippy --all-targets` and `cargo fmt --check` → clean.

- [ ] **Step 5: Commit**

```bash
git add musefs-core/src/tree.rs musefs-core/src/facade.rs musefs-core/tests/facade.rs
git commit -m "$(cat <<'EOF'
feat(core): stable inodes across rebuilds via a persistent path-keyed allocator

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Self-Review notes (addressed)

- **Spec coverage (Phase 4):** debounce `poll_refresh` → Task 4a; batched query (drop N+1) → Task 4b; off-thread rebuild + single-flight → Task 4c; stable inodes (persistent path→inode allocator, no recycling) → Task 4d. All four Phase-4 spec bullets are covered.
- **Out of scope (later phases):** Phase 5 (kernel/mount tuning: `KernelConfig` `max_readahead`/`FUSE_CAP_ASYNC_READ`/parallel-dirops, configurable entry/attr TTL, worker-queue back-pressure — incl. the `FOPEN_KEEP_CACHE` decision deferred from Phase 2.3) and Phase 6 (bounded-memory M4A `moov` read) remain.
- **Behavior change (documented at top):** FUSE-level `poll_refresh` is now fire-and-forget on the pool — external edits appear shortly after the next metadata op (bounded by debounce + rebuild), not synchronously. Core `poll_refresh` stays synchronous so `poll_refresh_picks_up_external_db_edits` still asserts `true`.
- **Type/behavior consistency:** `MountConfig.poll_interval: Duration` (tests use `Duration::ZERO`); `Db::tags_grouped() -> HashMap<i64, Vec<Tag>>` (ordering matches `get_tags`); `Musefs` new fields `last_poll: Mutex<Instant>`, `poll_interval: Duration`, `refreshing: AtomicBool`, `inodes: Mutex<InodeAllocator>`; `InodeAllocator::{new, intern}`; `VirtualTree::build_with(entries, &mut InodeAllocator)` with `build` delegating; `build_tree(db, config, &mut InodeAllocator)`. Single-build inode numbers are unchanged (fresh allocator interns in the same traversal order the removed `alloc()` used), so existing tree/facade tests keep passing.
- **Lock order:** the `inodes` lock (4d) and the rebuild all sit under the single-flight guard and follow the pool→in-memory order; the lock-order comment is updated to name `inodes`.
- **Verify-at-implementation:** the CLI arg-struct/flow for `--poll-interval-ms` (Task 4a), the exact tags-test module location (4b), and the `tree.rs` inline test module name (4d) must be matched against the real code — flagged in the steps.
