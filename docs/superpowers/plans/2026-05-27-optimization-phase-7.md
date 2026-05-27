# Optimization Phase 7 — Notifier-Based Auto Cache-Invalidation Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make `--keep-cache` (Phase 5's opt-in `FOPEN_KEEP_CACHE`) safe under live external edits: when a refresh detects a track whose content changed, push a kernel cache invalidation for that inode so the page cache can never serve stale bytes.

**Architecture:** Core tracks `track_id → content_version` and, on a `poll_refresh` rebuild, reports the inodes whose content changed via a `FnMut(u64)` callback (no `fuser` types leak into core). The FUSE layer holds a `fuser::Notifier` (obtained after the session is created, via an `Arc<OnceLock<Notifier>>` cell the filesystem carries) and, when keep-cache is on, calls `Notifier::inval_inode` for each reported inode. `mount_with`/`spawn_with` switch from `fuser::mount2`/`spawn_mount2` to the explicit `Session` API so the notifier is reachable.

**Tech Stack:** Rust, fuser 0.14 (`Session`/`Notifier`), `ArcSwap`, `std::sync::OnceLock`.

---

## Context the implementer needs

- **Why:** Phase 5 added opt-in `FOPEN_KEEP_CACHE`. With it on, the kernel keeps a file's page cache across opens; after an external re-tag the synthesized header changes, so cached bytes go stale. This phase drops the stale cache automatically on refresh, making `--keep-cache` safe.
- **What "changed" means:** a track whose `content_version` increased (the DB triggers bump it on any tag/art edit) while its rendered path — and therefore its inode (stable via Phase 4's `InodeAllocator`) — is unchanged. A retag that *moves* the file gets a fresh inode (no cached pages); a removed track's inode is retired. So we invalidate exactly: inodes present after the rebuild whose track's `content_version` rose.
- **Layering:** `musefs-core` must NOT depend on `fuser`. Core exposes the changed inodes through a `FnMut(u64)` callback; the FUSE layer supplies a closure that calls the notifier.
- **fuser 0.14 facts (verified against the vendored source):**
  - `Notifier` wraps `Arc<File>`, so it is `Send + Sync`; `Notifier::inval_inode(&self, ino: u64, offset: i64, len: i64) -> io::Result<()>` drops cached data for an inode (`(0,0)` = whole file).
  - `Session::new(filesystem, mountpoint: &Path, options: &[MountOption]) -> io::Result<Session>`; `Session::run(&mut self) -> io::Result<()>`; `Session::spawn(self) -> io::Result<BackgroundSession>` (requires `FS: 'static + Send`); `Session::notifier(&self) -> Notifier`; `BackgroundSession::notifier(&self) -> Notifier`.
  - `fuser::mount2(fs, mp, opts)` is `Session::new(...)?.run()`; `spawn_mount2(fs, mp, opts)` is `Session::new(...)?.spawn()`. So switching to the explicit API is behavior-preserving.
- **The notifier chicken-and-egg:** the filesystem is *moved into* `Session::new`, but the notifier only exists *after* the session is created. Solution: `MusefsFs` carries an `Arc<OnceLock<Notifier>>`; clone the cell out before constructing the session, then `cell.set(session.notifier())` once the session exists. Worker closures read it via `cell.get()` (it's populated before any request is served because `set` happens before `run`/`spawn` returns control to request handling).
- **Existing code shapes you will edit:**
  - `VirtualTree` (`tree.rs`) has fields `nodes`, `children`; `build_with(entries: &[(i64, String)], alloc)`; `insert_file(track_id, path, alloc)` computes `let inode = alloc.intern(&full);`. `track_id(inode)` exists; there is no track→inode map yet.
  - `Musefs` (`facade.rs`) fields include `tree: ArcSwap<VirtualTree>`, `cache`, `size_cache`, `last_data_version`, `refreshing`, `inodes`. `build_tree(db, config, alloc) -> Result<VirtualTree>`. `refresh() -> Result<()>` rebuilds + stores. `poll_refresh() -> Result<bool>` debounces → checks `data_version` → single-flight CAS + `RefreshGuard` → `refresh()` → retain caches → stamp version. `Track` (musefs-db) has `id: i64` and `content_version: i64`; `db.list_tracks()` returns them.
  - `MusefsFs` (`musefs-fuse/src/lib.rs`) fields `core, pool, uid, gid, mount_time, config`; `new(core, config)`. `lookup`/`getattr`/`readdir` each fire poll_refresh fire-and-forget with `{ let core = Arc::clone(&self.core); self.pool.execute(move || { let _ = core.poll_refresh(); }); }`. `mount_with`/`spawn_with` call `fuser::mount2`/`spawn_mount2`. `FuseConfig.keep_cache: bool` exists.

## File structure

- `musefs-core/src/tree.rs` — add `track_to_inode` map + `inode_of_track`.
- `musefs-core/src/facade.rs` — `versions` field; `build_tree` returns versions; private `rebuild`; `refresh` keeps versions in sync; `poll_refresh_notify` (callback) + `poll_refresh` delegate; `open` init.
- `musefs-fuse/src/lib.rs` — `notifier` cell on `MusefsFs`; `fire_poll_refresh` helper; `lookup`/`getattr`/`readdir` use it; `Session`-based `mount_with`/`spawn_with`.

## Out of scope

- Negative-lookup caching (already dropped in the design).
- Invalidating when keep-cache is OFF (the kernel drops cache on open anyway; the callback is simply not wired in that case — the cost stays at today's behavior).

---

## Task 1: `VirtualTree` track→inode map

**Files:**
- Modify: `musefs-core/src/tree.rs`

- [ ] **Step 1: Write the failing test**

In `tree.rs`'s `#[cfg(test)] mod tests`, add:
```rust
    #[test]
    fn inode_of_track_maps_file_nodes() {
        let t = VirtualTree::build(&[
            (10, "Alice/Song.flac".into()),
            (20, "Bob/Tune.flac".into()),
        ]);
        let alice = t.lookup(VirtualTree::ROOT, "Alice").unwrap();
        let song = t.lookup(alice, "Song.flac").unwrap();
        assert_eq!(t.inode_of_track(10), Some(song));
        assert!(t.inode_of_track(20).is_some());
        assert_eq!(t.inode_of_track(999), None);
    }
```

- [ ] **Step 2: Run to verify FAIL**

Run: `cargo test -p musefs-core inode_of_track 2>&1 | head -15`
Expected: FAIL — `no method named inode_of_track`.

- [ ] **Step 3: Add the field and accessor**

In the `VirtualTree` struct definition, add the field:
```rust
pub struct VirtualTree {
    nodes: HashMap<u64, Node>,
    children: HashMap<u64, BTreeMap<String, u64>>,
    track_to_inode: HashMap<i64, u64>,
}
```
In `build_with`, initialize it in the struct literal (alongside `nodes`/`children`):
```rust
        let mut tree = VirtualTree {
            nodes: HashMap::new(),
            children: HashMap::new(),
            track_to_inode: HashMap::new(),
        };
```
In `insert_file`, record the mapping right after the inode is allocated. Change:
```rust
        let inode = alloc.intern(&full);
        self.nodes.insert(
```
to:
```rust
        let inode = alloc.intern(&full);
        self.track_to_inode.insert(track_id, inode);
        self.nodes.insert(
```
Add the accessor (e.g. just after the existing `track_id` method):
```rust
    /// The inode of the file node serving `track_id`, if present.
    pub fn inode_of_track(&self, track_id: i64) -> Option<u64> {
        self.track_to_inode.get(&track_id).copied()
    }
```

- [ ] **Step 4: Run the test + gates**

Run: `cargo test -p musefs-core inode_of_track 2>&1 | tail -8` → PASS.
Run: `cargo test -p musefs-core 2>&1 | tail -8` → all pass (existing tree tests unaffected).
Run: `cargo clippy -p musefs-core --all-targets 2>&1 | tail -5` → no warnings.
Run: `cargo fmt -p musefs-core -- --check` → clean.

- [ ] **Step 5: Commit**

```bash
git add musefs-core/src/tree.rs
git commit -m "feat(core): VirtualTree track->inode map (inode_of_track)"
```

---

## Task 2: Core change-tracking + `poll_refresh_notify`

**Files:**
- Modify: `musefs-core/src/facade.rs`
- Test: `musefs-core/tests/facade.rs`

- [ ] **Step 1: Write the failing test**

In `musefs-core/tests/facade.rs`, add (it uses the existing `config()` helper and the `common` flac helpers already imported at the top of that file):
```rust
#[test]
fn poll_refresh_notify_reports_changed_track_inode() {
    use musefs_db::Tag;
    let dir = tempfile::tempdir().unwrap();
    // Two backing files -> two tracks: Alice/Song and Bob/Tune.
    for (name, artist, title) in [("a.flac", "Alice", "Song"), ("b.flac", "Bob", "Tune")] {
        let bytes = make_flac(
            &[
                (0, streaminfo_body()),
                (
                    4,
                    vorbis_comment_body("v", &[&format!("ARTIST={artist}"), &format!("TITLE={title}")]),
                ),
            ],
            &[0xAB; 64],
        );
        std::fs::write(dir.path().join(name), &bytes).unwrap();
    }
    let db_path = dir.path().join("m.db");
    {
        let db = musefs_db::Db::open(&db_path).unwrap();
        scan_directory(&db, dir.path()).unwrap();
    }
    let fs = Musefs::open(musefs_db::Db::open(&db_path).unwrap(), config()).unwrap();

    let alice = fs.lookup(VirtualTree::ROOT, "Alice").unwrap();
    let alice_song = fs.lookup(alice, "Song.flac").unwrap();

    // Find Alice's track id (the scan assigns ids by discovery order).
    let alice_id = musefs_db::Db::open(&db_path)
        .unwrap()
        .list_tracks()
        .unwrap()
        .into_iter()
        .find(|t| t.backing_path.ends_with("a.flac"))
        .unwrap()
        .id;

    // External edit: retag Alice WITHOUT moving her (same artist/title, extra
    // album tag) so her path/inode is stable but content_version bumps.
    {
        let db2 = musefs_db::Db::open(&db_path).unwrap();
        db2.replace_tags(
            alice_id,
            &[
                Tag::new("artist", "Alice", 0),
                Tag::new("title", "Song", 0),
                Tag::new("album", "New", 0),
            ],
        )
        .unwrap();
    }

    let mut changed = Vec::new();
    assert!(fs.poll_refresh_notify(|ino| changed.push(ino)).unwrap());
    assert_eq!(changed, vec![alice_song], "only Alice's inode changed");
    // Inode stayed stable across the refresh.
    assert_eq!(fs.lookup(fs.lookup(VirtualTree::ROOT, "Alice").unwrap(), "Song.flac").unwrap(), alice_song);
}
```

- [ ] **Step 2: Run to verify FAIL**

Run: `cargo test -p musefs-core --test facade poll_refresh_notify_reports_changed_track_inode 2>&1 | head -20`
Expected: FAIL — `no method named poll_refresh_notify`.

- [ ] **Step 3: Add the `versions` field**

In the `Musefs` struct definition (`facade.rs`), add the field (next to `inodes`):
```rust
    /// Last-seen `content_version` per track, snapshotted on each rebuild, used to
    /// report which inodes changed so the FUSE layer can drop stale kernel cache.
    versions: Mutex<HashMap<i64, i64>>,
```

- [ ] **Step 4: `build_tree` returns the versions map**

Change `build_tree`'s signature and body:
```rust
    fn build_tree(
        db: &Db,
        config: &MountConfig,
        alloc: &mut InodeAllocator,
    ) -> Result<(VirtualTree, HashMap<i64, i64>)> {
        let tracks = db.list_tracks()?;
        let mut tags_by_track = db.tags_grouped()?;
        let mut entries = Vec::with_capacity(tracks.len());
        let mut versions = HashMap::with_capacity(tracks.len());
        for t in &tracks {
            versions.insert(t.id, t.content_version);
            let tags = tags_by_track.remove(&t.id).unwrap_or_default();
            let fields = tags_to_fields(&tags);
            let path = render_path(
                &config.template,
                &fields,
                &config.fallbacks,
                &config.default_fallback,
                t.format.as_str(),
            );
            entries.push((t.id, path));
        }
        Ok((VirtualTree::build_with(&entries, alloc), versions))
    }
```

- [ ] **Step 5: Update `open` for the new return + field**

In `open`, change the build line and add the field to the struct literal. Replace:
```rust
        let mut alloc = InodeAllocator::new();
        let tree = Self::build_tree(&db, &config, &mut alloc)?;
```
with:
```rust
        let mut alloc = InodeAllocator::new();
        let (tree, versions) = Self::build_tree(&db, &config, &mut alloc)?;
```
and add to the returned `Musefs { … }` literal (next to `inodes: Mutex::new(alloc),`):
```rust
            versions: Mutex::new(versions),
```

- [ ] **Step 6: Add `rebuild`, update `refresh`, add `poll_refresh_notify`**

Replace the existing `refresh` method (keep its doc comment) so it delegates to a private `rebuild` and keeps `versions` in sync:
```rust
    /// Rebuild the tree from the current DB contents (used after external edits).
    ///
    /// Not single-flighted: do not run concurrently with `poll_refresh` (or another
    /// `refresh`) — two overlapping rebuilds can publish a stale tree. The production
    /// path goes through `poll_refresh`, which guards entry with the `refreshing` CAS;
    /// this entry point exists for forced, unconditional rebuilds (e.g. tests).
    pub fn refresh(&self) -> Result<()> {
        let versions = self.rebuild()?;
        *self.versions.lock().unwrap_or_else(|p| p.into_inner()) = versions;
        Ok(())
    }

    /// Rebuild + publish the tree; returns the current `track_id -> content_version`
    /// map (caller decides whether/how to diff it).
    fn rebuild(&self) -> Result<HashMap<i64, i64>> {
        let (tree, versions) = self.pool.with(|db| {
            let mut alloc = self.inodes.lock().unwrap_or_else(|p| p.into_inner());
            Self::build_tree(db, &self.config, &mut alloc)
        })?;
        self.tree.store(Arc::new(tree));
        Ok(versions)
    }
```
Then change `poll_refresh` to delegate, and add `poll_refresh_notify` carrying the diff. Replace the entire existing `poll_refresh` method with:
```rust
    /// See `poll_refresh_notify`; this is the no-callback form.
    pub fn poll_refresh(&self) -> Result<bool> {
        self.poll_refresh_notify(|_| {})
    }

    /// Cheap check for external DB commits via `PRAGMA data_version`. On a change,
    /// rebuild the tree, prune cached resolutions to the live track set, invoke
    /// `on_changed(inode)` for every inode whose track's `content_version` rose
    /// (its served bytes changed but its path/inode is stable), then return `true`.
    /// The version stamp is committed only after a successful rebuild.
    ///
    /// Single-flighted: if a rebuild is already in progress, concurrent callers
    /// return `Ok(false)` immediately.
    pub fn poll_refresh_notify(&self, mut on_changed: impl FnMut(u64)) -> Result<bool> {
        if !self.poll_interval.is_zero() {
            let mut last = self.last_poll.lock().unwrap_or_else(|p| p.into_inner());
            if last.elapsed() < self.poll_interval {
                return Ok(false);
            }
            *last = std::time::Instant::now();
        }
        let version = self.pool.with_poll(|db| Ok(db.data_version()?))?;
        if version == self.last_data_version.load(Ordering::Acquire) {
            return Ok(false);
        }
        if self
            .refreshing
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Ok(false);
        }
        // The guard clears `refreshing` on every exit path (incl. panic).
        let _guard = RefreshGuard(&self.refreshing);

        let old_versions = self
            .versions
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone();
        let new_versions = self.rebuild()?;
        let live = self.tree.load().track_ids();
        self.cache.retain(&live);
        self.size_cache().retain(|k, _| live.contains(k));

        // A track whose content_version rose but whose path (inode) is unchanged has
        // stale served bytes; report its inode so the caller can drop the kernel page
        // cache. New/removed tracks have no cache to drop.
        let tree = self.tree.load();
        for (tid, ver) in &new_versions {
            if let Some(old) = old_versions.get(tid) {
                if old != ver {
                    if let Some(ino) = tree.inode_of_track(*tid) {
                        on_changed(ino);
                    }
                }
            }
        }
        *self.versions.lock().unwrap_or_else(|p| p.into_inner()) = new_versions;

        self.last_data_version.store(version, Ordering::Release);
        Ok(true)
    }
```

- [ ] **Step 7: Run the test + gates**

Run: `cargo test -p musefs-core --test facade poll_refresh_notify_reports_changed_track_inode 2>&1 | tail -12` → PASS.
Run: `cargo test -p musefs-core 2>&1 | tail -12` → all pass (existing `poll_refresh*` tests still green via the delegate).
Run: `cargo clippy -p musefs-core --all-targets 2>&1 | tail -5` → no warnings.
Run: `cargo fmt -p musefs-core -- --check` → clean.

- [ ] **Step 8: Commit**

```bash
git add musefs-core/src/facade.rs musefs-core/tests/facade.rs
git commit -m "feat(core): poll_refresh_notify reports changed-content inodes (content_version diff)"
```

---

## Task 3: FUSE notifier plumbing + invalidation on refresh

**Files:**
- Modify: `musefs-fuse/src/lib.rs`
- Test: `musefs-fuse/tests/keep_cache.rs` (new; `#[ignore]` e2e smoke)

> **Testing note:** the deterministic guarantee (correct changed-inode set) is proven by Task 2's core test. This task is FUSE wiring; its runtime effect (kernel page-cache drop) is kernel-timing-dependent and not deterministically assertable in a unit test. Verify with build/clippy/fmt, the existing `#[ignore]` read-through e2e (which now flows through the `Session`-based `mount_with`/`spawn_with`), and one new `#[ignore]` smoke test that a keep-cache mount serves updated tags after an external retag.

- [ ] **Step 1: Add imports**

In `musefs-fuse/src/lib.rs`, add `Notifier` and `Session` to the `use fuser::{…}` list, and `OnceLock` to the std sync import:
```rust
use std::sync::{Arc, OnceLock};
```
```rust
use fuser::{
    BackgroundSession, FileAttr, FileType, Filesystem, KernelConfig, MountOption, Notifier,
    ReplyAttr, ReplyData, ReplyDirectory, ReplyEmpty, ReplyEntry, ReplyOpen, Request, Session,
};
```
(If `Arc` was imported on its own `use std::sync::Arc;` line, replace that line with the combined one above.)

- [ ] **Step 2: Add the `notifier` cell to `MusefsFs` and `new`**

Add the field to the struct:
```rust
pub struct MusefsFs {
    core: Arc<Musefs>,
    pool: ThreadPool,
    uid: u32,
    gid: u32,
    mount_time: SystemTime,
    config: FuseConfig,
    // Set once, right after the session is created (the fs is moved into the
    // session, so the notifier can only be obtained afterward via this shared cell).
    notifier: Arc<OnceLock<Notifier>>,
}
```
In `MusefsFs::new`, add to the struct literal:
```rust
            config,
            notifier: Arc::new(OnceLock::new()),
```

- [ ] **Step 3: Add `notifier_cell` + `fire_poll_refresh` helpers**

In `impl MusefsFs`, add (after `new`):
```rust
    fn notifier_cell(&self) -> Arc<OnceLock<Notifier>> {
        Arc::clone(&self.notifier)
    }

    /// Fire `poll_refresh` on the worker pool (off the dispatch thread). When
    /// keep-cache is enabled, also drop the kernel page cache for every inode whose
    /// content changed, so an external re-tag never serves stale cached bytes.
    fn fire_poll_refresh(&self) {
        let core = Arc::clone(&self.core);
        if self.config.keep_cache {
            let notifier = Arc::clone(&self.notifier);
            self.pool.execute(move || {
                let _ = core.poll_refresh_notify(|ino| {
                    if let Some(n) = notifier.get() {
                        let _ = n.inval_inode(ino, 0, 0);
                    }
                });
            });
        } else {
            self.pool.execute(move || {
                let _ = core.poll_refresh();
            });
        }
    }
```

- [ ] **Step 4: Use the helper in `lookup`, `getattr`, `readdir`**

In each of those three methods, replace the leading fire-and-forget block:
```rust
        {
            let core = Arc::clone(&self.core);
            self.pool.execute(move || {
                let _ = core.poll_refresh();
            });
        }
```
with:
```rust
        self.fire_poll_refresh();
```

- [ ] **Step 5: Switch `mount_with`/`spawn_with` to the explicit `Session` API**

Replace the bodies of `mount_with` and `spawn_with`:
```rust
/// Mount `core` at `mountpoint` with explicit fuse tuning, blocking until unmounted.
pub fn mount_with(
    core: Musefs,
    mountpoint: &Path,
    fs_name: &str,
    config: FuseConfig,
) -> std::io::Result<()> {
    let fs = MusefsFs::new(core, config);
    let cell = fs.notifier_cell();
    let mut session = Session::new(fs, mountpoint, &mount_options(fs_name))?;
    let _ = cell.set(session.notifier());
    session.run()
}

/// Background-session mount with explicit tuning; the handle's `Drop` unmounts.
pub fn spawn_with(
    core: Musefs,
    mountpoint: &Path,
    fs_name: &str,
    config: FuseConfig,
) -> std::io::Result<BackgroundSession> {
    let fs = MusefsFs::new(core, config);
    let cell = fs.notifier_cell();
    let session = Session::new(fs, mountpoint, &mount_options(fs_name))?;
    let bg = session.spawn()?;
    let _ = cell.set(bg.notifier());
    Ok(bg)
}
```
(The 3-arg `mount`/`spawn` still delegate to these with `FuseConfig::default()`.)

- [ ] **Step 6: Build + lint (verify `MusefsFs` stays `Send`)**

Run: `cargo build -p musefs-fuse 2>&1 | tail -8` → PASS. (If this fails on a `Send`/`Sync` bound for the `notifier` field, the `Notifier`/`OnceLock` is the cause — `Notifier` wraps `Arc<File>` and is `Send + Sync`, so it should compile; escalate if not.)
Run: `cargo clippy -p musefs-fuse --all-targets 2>&1 | tail -5` → no warnings.
Run: `cargo fmt -p musefs-fuse -- --check` → clean.
Run: `cargo build --workspace 2>&1 | tail -5` → PASS.

- [ ] **Step 7: Add an `#[ignore]` keep-cache e2e smoke test**

Create `musefs-fuse/tests/keep_cache.rs`. Mirror the mount/setup style of `musefs-fuse/tests/mount.rs` (read that file first for the exact `scan_directory` + `Db` + temp-mount idioms used in this crate). The test:
```rust
//! e2e: a keep-cache mount must reflect an external retag after a refresh.
//! `#[ignore]` — performs a real mount; needs /dev/fuse.

// NOTE: copy the imports + the backing-file/db setup helpers from
// `musefs-fuse/tests/mount.rs` (this crate's tests are not a shared module).

#[test]
#[ignore]
fn keep_cache_mount_reflects_retag_after_refresh() {
    // 1. Scan a one-track flac library into an on-disk DB (as mount.rs does).
    // 2. spawn_with(core, mountpoint, "musefs-keepcache",
    //        FuseConfig { keep_cache: true, poll_interval: ZERO-equivalent, ..default })
    //    — keep_cache ON, debounce off so the next metadata op refreshes.
    // 3. Read the mounted file once (populates any kernel cache).
    // 4. Retag the track via a second Db connection (replace_tags with a new title
    //    or artist) so content_version + data_version bump.
    // 5. Trigger a metadata op (e.g. std::fs::metadata on the file's NEW path, or
    //    re-read the directory) so the FUSE layer fires poll_refresh_notify and
    //    calls inval_inode for the changed inode.
    // 6. Re-read the file and assert its synthesized tag bytes reflect the new tag
    //    (parse the served header, or assert the file size changed as expected).
    //    The assertion proves the served content updated; combined with keep_cache
    //    on, it exercises the invalidation path without crashing.
}
```
Fill in steps 1–6 with concrete code following `mount.rs`'s helpers (do not leave prose — the checkbox is complete only when the test compiles and is `#[ignore]`d). Keep assertions about *served content updating* (deterministic) rather than asserting kernel-internal cache state.

- [ ] **Step 8: Verify e2e**

Run: `cargo test -p musefs-fuse -- --ignored --list 2>&1 | tail -12` → links; lists the new test plus the existing read-throughs.
If `/dev/fuse` is available: `cargo test -p musefs-fuse -- --ignored 2>&1 | tail -25` → all pass (existing read-throughs still byte-identical through the `Session`-based mount; the new keep-cache smoke passes). If unavailable, note it must run on a FUSE host before merge.

- [ ] **Step 9: Commit**

```bash
git add musefs-fuse/src/lib.rs musefs-fuse/tests/keep_cache.rs
git commit -m "feat(fuse): auto-invalidate kernel cache for changed inodes on refresh (keep-cache safe)"
```

---

## Final verification (whole phase)

- [ ] `cargo build --workspace 2>&1 | tail -15` → PASS, no warnings.
- [ ] `cargo test --workspace 2>&1 | tail -25` → all non-ignored pass.
- [ ] `cargo clippy --all-targets 2>&1 | tail -15` → no warnings.
- [ ] `cargo fmt --all -- --check` → clean.
- [ ] metrics feature builds: `cargo build -p musefs-core --features metrics` and `cargo build -p musefs-fuse --features metrics`.
- [ ] e2e on a FUSE host: `cargo test -p musefs-fuse -- --ignored` → read-throughs byte-identical; keep-cache smoke passes.
- [ ] Update Phase 5's `--keep-cache` CLI help to drop the "static libraries only" caveat (now that re-tags auto-invalidate). In `musefs-cli/src/lib.rs`, change the `keep_cache` doc to: `Keep the kernel page cache across opens. External re-tags auto-invalidate the affected inodes on refresh, so cached bytes are dropped when content changes.` Re-run `cargo build -p musefs-cli` + `cargo fmt -p musefs-cli -- --check`. Commit: `docs(cli): --keep-cache is now refresh-safe (auto-invalidation)`.

## Self-review (completed during planning)

- **Goal coverage:** changed-inode detection (Task 2, `content_version` diff + `inode_of_track` from Task 1); notifier obtained past the move barrier via `Arc<OnceLock<Notifier>>` and `Session` API (Task 3); invalidation gated on `keep_cache` (Task 3 `fire_poll_refresh`); core stays `fuser`-free (callback boundary). The CLI caveat update closes the loop with Phase 5.
- **Type consistency:** `inode_of_track(i64) -> Option<u64>` (Task 1) is consumed in Task 2's loop; `build_tree -> Result<(VirtualTree, HashMap<i64,i64>)>` is used identically in `open` and `rebuild`; `poll_refresh_notify(impl FnMut(u64))` is called by `poll_refresh` (no-op closure) and by `fire_poll_refresh` (notifier closure). `Notifier::inval_inode(ino: u64, 0, 0)` matches the fuser 0.14 signature.
- **No placeholders:** all code steps are concrete except Task 3 Step 7 (the e2e smoke), which is intentionally scaffolded against this crate's existing `mount.rs` idioms and must be filled to compile; its purpose and assertions are specified.
- **Behavior preservation:** `mount_with`/`spawn_with` switch to `Session::new(...).run()/.spawn()`, which is exactly what `mount2`/`spawn_mount2` do — existing e2e read-throughs must remain green.
