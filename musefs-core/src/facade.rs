use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Mutex};

use arc_swap::ArcSwap;
use musefs_db::Db;

use crate::db_pool::DbPool;
use crate::error::{CoreError, Result};
use crate::mapping::tags_to_fields;
use crate::reader::{read_at, read_at_with_file, HeaderCache, ResolvedFile};
use crate::refresh_diff::{partition_changes, ChangeSet, TrackRenderState};
use crate::template::render_path;
use crate::tree::{InodeAllocator, NodeKind, VirtualTree};

/// How the mount serves file *contents*. The virtual tree is identical either way.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Splice a freshly synthesized metadata region in front of the backing audio.
    Synthesis,
    /// Pure passthrough: serve the original backing file bytes unchanged.
    StructureOnly,
}

/// Per-mount configuration for rendering the virtual hierarchy.
#[derive(Debug, Clone)]
pub struct MountConfig {
    pub template: String,
    pub fallbacks: BTreeMap<String, String>,
    pub default_fallback: String,
    pub mode: Mode,
    /// Minimum time between `data_version` polls; a metadata-op storm within this
    /// window skips the poll entirely. `Duration::ZERO` disables debouncing.
    pub poll_interval: std::time::Duration,
}

/// Attributes the FUSE layer maps onto `fuser::FileAttr`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attr {
    pub inode: u64,
    pub is_dir: bool,
    pub size: u64,
    pub mtime_secs: i64,
}

/// An open file handle: the resolved layout and a backing fd opened (and
/// validated) once at `open`, reused for every `read` on this handle.
///
/// A handle intentionally outlives `poll_refresh`: it keeps serving the layout
/// (and backing path) it was opened with, even if a rescan changes the track,
/// until `release`. This is consistent POSIX-like open-fd snapshot behavior and
/// is bounded by the FUSE descriptor's lifetime.
struct Handle {
    resolved: Arc<ResolvedFile>,
    file: std::fs::File,
}

/// A cached file size/attr entry: validated at `content_version`.
#[derive(Clone, Copy)]
struct SizeEntry {
    content_version: i64,
    total_len: u64,
    mtime_secs: i64,
}

/// Resets a single-flight flag on drop, so a panic (or early return) during a
/// rebuild can't leave `refreshing` stuck `true` and permanently disable refresh.
struct RefreshGuard<'a>(&'a AtomicBool);

impl Drop for RefreshGuard<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

fn mtime_secs(meta: &std::fs::Metadata) -> i64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_secs() as i64)
}

fn validate_opened_backing(file: &std::fs::File, resolved: &ResolvedFile) -> Result<()> {
    let meta = file.metadata()?;
    if meta.len() != resolved.backing_size || mtime_secs(&meta) != resolved.backing_mtime_secs {
        return Err(CoreError::BackingChanged(
            resolved.backing_path.to_string_lossy().to_string(),
        ));
    }
    Ok(())
}

fn retry_backoff_for(poll_interval: std::time::Duration) -> std::time::Duration {
    if poll_interval.is_zero() {
        std::time::Duration::ZERO
    } else {
        poll_interval
            .min(std::time::Duration::from_secs(1))
            .max(std::time::Duration::from_millis(100))
    }
}

/// The composed read-only filesystem: the store, the rendered tree, and the
/// lazy synthesis cache. All methods take `&self`; the tree is swapped
/// atomically on refresh, the cache is internally sharded (each shard mutex-guarded),
/// and the data-version stamp is atomic. This makes `Musefs` `Sync`, so the FUSE
/// layer can later share it across a worker pool.
pub struct Musefs {
    pool: DbPool,
    config: MountConfig,
    tree: ArcSwap<VirtualTree>,
    cache: HeaderCache,
    last_data_version: AtomicI64,
    handles: sharded_slab::Slab<Arc<Handle>>,
    /// `SizeEntry` keyed by track id. Tiny entries, effectively unbounded; serves
    /// getattr/lookup without a backing stat or full synthesis. Self-invalidates on
    /// a content_version change.
    size_cache: dashmap::DashMap<i64, SizeEntry>,
    /// Timestamp of the last `data_version` poll; gated by `poll_interval`.
    last_poll: Mutex<std::time::Instant>,
    /// Timestamp of the last failed refresh attempt; used to prevent tight retry loops.
    last_failed_refresh: Mutex<Option<std::time::Instant>>,
    /// Minimum time between `data_version` polls (`Duration::ZERO` disables debouncing).
    poll_interval: std::time::Duration,
    refresh_retry_backoff: std::time::Duration,
    /// Single-flight guard: only the thread that flips this `false → true`
    /// performs the rebuild; concurrent callers see it set and return immediately.
    refreshing: AtomicBool,
    /// Persistent path→inode allocator: carries stable inodes across tree rebuilds
    /// so open FUSE handles continue to resolve to the same node after a refresh.
    inodes: Mutex<InodeAllocator>,
    /// Last-seen render state per track, snapshotted on each rebuild. Drives the
    /// incremental change diff and the `on_changed` cache-invalidation callbacks.
    snapshot: Mutex<HashMap<i64, TrackRenderState>>,
    force_rebuild_error: AtomicBool,
    force_apply_fail: AtomicBool,
}

/// Map a `sharded_slab::Slab` insert result to a FUSE file handle. The slab key
/// is offset by one so the wire `fh` is always non-zero (`fh == 0` means "no
/// handle" — `read` falls back to inode resolution). `None` means the slab is at
/// capacity, surfaced as an explicit error rather than a panic.
fn fh_from_key(key: Option<usize>) -> Result<u64> {
    key.map(|k| k as u64 + 1).ok_or(CoreError::HandleTableFull)
}

impl Musefs {
    pub fn open(db: Db, config: MountConfig) -> Result<Musefs> {
        let mut alloc = InodeAllocator::new();
        let (tree, snapshot) = Self::build_full(&db, &config, &mut alloc)?;
        let last_data_version = db.data_version()?;
        let poll_interval = config.poll_interval;
        Ok(Musefs {
            cache: HeaderCache::new(config.mode),
            last_data_version: AtomicI64::new(last_data_version),
            tree: ArcSwap::from_pointee(tree),
            pool: DbPool::new(db)?,
            config,
            handles: sharded_slab::Slab::new(),
            size_cache: dashmap::DashMap::new(),
            last_poll: Mutex::new(std::time::Instant::now()),
            last_failed_refresh: Mutex::new(None),
            poll_interval,
            refresh_retry_backoff: retry_backoff_for(poll_interval),
            refreshing: AtomicBool::new(false),
            inodes: Mutex::new(alloc),
            snapshot: Mutex::new(snapshot),
            force_rebuild_error: AtomicBool::new(false),
            force_apply_fail: AtomicBool::new(false),
        })
    }

    /// Render a single track's path from its tags + format. The one place
    /// `render_path` is called, shared by full and incremental rebuilds.
    fn render_one(
        config: &MountConfig,
        format: musefs_db::Format,
        tags: &[musefs_db::Tag],
    ) -> String {
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
    ) -> Result<(VirtualTree, HashMap<i64, TrackRenderState>)> {
        let tracks = db.list_tracks()?;
        let mut tags_by_track = db.tags_grouped()?;
        let mut entries = Vec::with_capacity(tracks.len());
        let mut snapshot = HashMap::with_capacity(tracks.len());
        for t in &tracks {
            let tags = tags_by_track.remove(&t.id).unwrap_or_default();
            let path = Self::render_one(config, t.format, &tags);
            snapshot.insert(
                t.id,
                TrackRenderState {
                    content_version: t.content_version,
                    format: t.format,
                    path: path.clone(),
                },
            );
            entries.push((t.id, path));
        }
        Ok((VirtualTree::build_with(&entries, alloc), snapshot))
    }

    /// Rebuild the tree from the current DB contents (used after external edits).
    ///
    /// Not single-flighted: do not run concurrently with `poll_refresh` (or another
    /// `refresh`) — two overlapping rebuilds can publish a stale tree. The production
    /// path goes through `poll_refresh`, which guards entry with the `refreshing` CAS;
    /// this entry point exists for forced, unconditional rebuilds (e.g. tests). It
    /// also refreshes the `content_version` snapshot, so it must not race a
    /// `poll_refresh` whose change-diff relies on that snapshot.
    pub fn refresh(&self) -> Result<()> {
        let snapshot = self.rebuild_full()?;
        *self
            .snapshot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = snapshot;
        Ok(())
    }

    /// Rebuild + publish the tree via a full render; returns the fresh snapshot
    /// (the caller decides whether/how to diff it).
    fn rebuild_full(&self) -> Result<HashMap<i64, TrackRenderState>> {
        if self.force_rebuild_error.load(Ordering::Acquire) {
            return Err(CoreError::BackingChanged(
                "forced refresh failure".to_string(),
            ));
        }
        let (tree, snapshot) = self.pool.with(|db| {
            let mut alloc = self
                .inodes
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            Self::build_full(db, &self.config, &mut alloc)
        })?;
        self.tree.store(Arc::new(tree));
        Ok(snapshot)
    }

    /// Incremental rebuild (Stage A): scan render keys, diff against the previous
    /// snapshot, render only changed/added tracks (reusing cached paths otherwise),
    /// then assemble entries and call the unchanged `build_with`. Returns the new
    /// snapshot and the `ChangeSet`. The tree is published here. See SP2 Component 2.
    fn rebuild_incremental(
        &self,
        prev_snapshot: &std::collections::HashMap<i64, TrackRenderState>,
    ) -> Result<(std::collections::HashMap<i64, TrackRenderState>, ChangeSet)> {
        if self.force_rebuild_error.load(Ordering::Acquire) {
            return Err(CoreError::BackingChanged(
                "forced refresh failure".to_string(),
            ));
        }
        let (new_snapshot, change) = self.pool.with(|db| {
            let scan = db.list_render_keys()?;
            let change = partition_changes(prev_snapshot, &scan);

            let mut to_render: Vec<i64> = change.changed.clone();
            to_render.extend(change.added.iter().copied());
            let render_set: std::collections::HashSet<i64> = to_render.iter().copied().collect();
            let mut tags_by_track = db.tags_for_tracks(&to_render)?;

            let mut new_snapshot = std::collections::HashMap::with_capacity(scan.len());
            for &(id, cv, fmt) in &scan {
                let state = if render_set.contains(&id) {
                    let tags = tags_by_track.remove(&id).unwrap_or_default();
                    TrackRenderState {
                        content_version: cv,
                        format: fmt,
                        path: Self::render_one(&self.config, fmt, &tags),
                    }
                } else {
                    prev_snapshot[&id].clone()
                };
                new_snapshot.insert(id, state);
            }
            Ok::<_, CoreError>((new_snapshot, change))
        })?;

        let new_paths: std::collections::HashMap<i64, String> = new_snapshot
            .iter()
            .map(|(&id, s)| (id, s.path.clone()))
            .collect();

        let mut alloc = self
            .inodes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut tree = (*self.tree.load_full()).clone(); // O(1) im clone
        let applied = if self.force_apply_fail.swap(false, Ordering::AcqRel) {
            Err(()) // test injection
        } else {
            tree.apply_changes(
                &new_paths,
                &change.changed,
                &change.added,
                &change.removed,
                &mut alloc,
            )
        };
        // Two materially distinct arms (debug-assert vs. full-rebuild fallback), so a
        // `match` reads clearer than the `if let .. else` clippy would prefer.
        #[allow(clippy::single_match_else)]
        let tree = match applied {
            Ok(()) => {
                #[cfg(debug_assertions)]
                {
                    let mut ref_alloc = alloc.clone();
                    let mut entries: Vec<(i64, String)> =
                        new_paths.iter().map(|(&id, p)| (id, p.clone())).collect();
                    entries.sort_by_key(|(id, _)| *id);
                    let reference = VirtualTree::build_with(&entries, &mut ref_alloc);
                    debug_assert!(
                        tree.equiv(&reference),
                        "incremental tree diverged from build_with"
                    );
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
    }

    // Lock order: acquire a DbPool connection (`pool.with`/`with_poll`) FIRST, then
    // any in-memory lock (`inodes`, the header cache's shards). `inodes` is held
    // inside `pool.with` during `refresh` — the one intentional exception where a
    // pool connection is held around an in-memory lock. `handles` is a lock-free
    // `sharded_slab::Slab`: its `get` guard is cloned-from and dropped before any
    // pool call, so it never participates in lock ordering. Slab keys are
    // generation-encoded, so a reused slot produces a different key; a stale `fh`
    // therefore returns `None` from `get` and falls back to inode resolution rather
    // than aliasing a recycled handle (ABA-safe). `size_cache` is a `DashMap`
    // whose per-shard guards are taken and released per op (the `*e` copy drops
    // the read guard before the `insert`; `retain` is never called while a `Ref`
    // is held), so it imposes no problematic lock ordering / no cross-lock cycle.

    /// See `poll_refresh_notify`; this is the no-callback form.
    pub fn poll_refresh(&self) -> Result<bool> {
        self.poll_refresh_notify(|_| {})
    }

    /// Cheap check for external DB commits via `PRAGMA data_version`. On a change,
    /// rebuild the tree, prune cached resolutions to the live track set, invoke
    /// `on_changed(inode)` for every inode whose track's `content_version` changed
    /// (its served bytes changed but its path/inode is stable), then return `true`.
    /// The version stamp is committed only after a successful rebuild.
    ///
    /// Single-flighted: if a rebuild is already in progress, concurrent callers
    /// return `Ok(false)` immediately.
    pub fn poll_refresh_notify(&self, mut on_changed: impl FnMut(u64)) -> Result<bool> {
        if !self.poll_interval.is_zero()
            && self
                .last_poll
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .elapsed()
                < self.poll_interval
        {
            return Ok(false);
        }
        if let Some(last_failed) = *self
            .last_failed_refresh
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
        {
            if last_failed.elapsed() < self.refresh_retry_backoff {
                return Ok(false);
            }
        }
        let version = self.pool.with_poll(|db| Ok(db.data_version()?))?;
        if version == self.last_data_version.load(Ordering::Acquire) {
            self.stamp_successful_poll();
            return Ok(false);
        }
        // Single-flight: only the caller that flips the flag false->true rebuilds;
        // concurrent callers see it's being handled and return without duplicating
        // the O(library) work.
        if self
            .refreshing
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Ok(false);
        }
        // The guard clears `refreshing` on every exit path (incl. panic).
        let _guard = RefreshGuard(&self.refreshing);

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
        // Load the (newly rebuilt) tree. The Guard is held only across in-memory
        // cache ops, so no blocking or I/O occurs under the pin.
        let tree = self.tree.load();
        let live = tree.track_ids();
        self.cache.retain(&live);
        self.size_cache.retain(|k, _| live.contains(k));

        Self::notify_changed(
            &old_snapshot,
            &new_snapshot,
            &old_tree,
            &tree,
            &mut on_changed,
        );

        *self
            .snapshot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = new_snapshot;

        self.last_data_version.store(version, Ordering::Release);
        self.stamp_successful_poll();

        Ok(true)
    }

    /// Fire `on_changed` for every inode that must drop kernel cache: a track whose
    /// served bytes changed (content_version rose, path stable) and the OLD inode of
    /// any track that was removed or whose path moved (incl. a format-only move that
    /// did not bump content_version). Path-move detection is decoupled from
    /// content_version. See SP2 Component 2.
    fn notify_changed(
        old: &HashMap<i64, TrackRenderState>,
        new: &HashMap<i64, TrackRenderState>,
        old_tree: &VirtualTree,
        new_tree: &VirtualTree,
        on_changed: &mut impl FnMut(u64),
    ) {
        for (tid, ns) in new {
            if let Some(os) = old.get(tid) {
                if os.content_version != ns.content_version && os.path == ns.path {
                    if let Some(ino) = new_tree.inode_of_track(*tid) {
                        on_changed(ino);
                    }
                }
            }
        }
        for (tid, os) in old {
            let moved_or_gone = match new.get(tid) {
                None => true,
                Some(ns) => ns.path != os.path,
            };
            if moved_or_gone {
                if let Some(ino) = old_tree.inode_of_track(*tid) {
                    on_changed(ino);
                }
            }
        }
    }

    fn stamp_successful_poll(&self) {
        if !self.poll_interval.is_zero() {
            *self
                .last_poll
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = std::time::Instant::now();
        }
        *self
            .last_failed_refresh
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
    }

    #[doc(hidden)]
    pub fn force_rebuild_errors_for_test(&self, fail: bool) {
        self.force_rebuild_error.store(fail, Ordering::Release);
    }

    #[doc(hidden)]
    pub fn force_apply_failure_for_test(&self, on: bool) {
        self.force_apply_fail.store(on, Ordering::Release);
    }

    #[doc(hidden)]
    pub fn lookup_track_inode_for_test(&self, track_id: i64) -> Option<u64> {
        self.tree.load().inode_of_track(track_id)
    }

    /// Backdates `last_poll` so the next `poll_refresh` is past the debounce
    /// window, letting tests cross the window deterministically without sleeping.
    #[doc(hidden)]
    pub fn expire_poll_debounce_for_test(&self) {
        let past = std::time::Instant::now()
            .checked_sub(self.poll_interval)
            .expect("poll_interval exceeds monotonic clock base; cannot backdate last_poll");
        *self
            .last_poll
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = past;
    }

    pub fn lookup(&self, parent: u64, name: &str) -> Option<u64> {
        self.tree.load().lookup(parent, name)
    }

    /// The parent inode of `inode` (root's parent is itself). Forwards to the tree.
    pub fn parent(&self, inode: u64) -> Option<u64> {
        self.tree.load().parent(inode)
    }

    pub fn getattr(&self, inode: u64) -> Result<Attr> {
        let track_id = {
            let tree = self.tree.load();
            match tree.node(inode) {
                None => return Err(CoreError::NoEntry(inode)),
                Some(node) => match &node.kind {
                    NodeKind::Dir => {
                        return Ok(Attr {
                            inode,
                            is_dir: true,
                            size: 0,
                            mtime_secs: 0,
                        })
                    }
                    NodeKind::File { track_id } => *track_id,
                },
            }
        };
        let (size, mtime_secs) = self.pool.with(|db| {
            // Cheap, indexed: the current content_version drives lazy invalidation.
            let track = db
                .get_track(track_id)?
                .ok_or(CoreError::TrackNotFound(track_id))?;
            // `.map(|e| *e)` copies the SizeEntry (Copy) so the shard Ref drops
            // before the miss-path insert below — same key → same shard, and
            // holding the Ref across the re-lock would deadlock.
            if let Some(e) = self.size_cache.get(&track_id).map(|e| *e) {
                if e.content_version == track.content_version {
                    // Hit: no backing stat, no synthesis. NOTE: a backing file
                    // changed in place without a rescan would leave mtime/size
                    // stale until the next scan bumps content_version — acceptable
                    // for a read-only mount (reads still validate at open()).
                    return Ok((e.total_len, e.mtime_secs));
                }
            }
            // Miss: full resolve (validates via stat, builds + caches the layout).
            let resolved = self.cache.resolve(db, track_id)?;
            self.size_cache.insert(
                track_id,
                SizeEntry {
                    content_version: track.content_version,
                    total_len: resolved.total_len,
                    mtime_secs: resolved.mtime_secs,
                },
            );
            Ok((resolved.total_len, resolved.mtime_secs))
        })?;
        Ok(Attr {
            inode,
            is_dir: false,
            size,
            mtime_secs,
        })
    }

    /// Directory entries as `(name, child_inode, is_dir)`.
    pub fn readdir(&self, inode: u64) -> Result<Vec<(String, u64, bool)>> {
        let tree = self.tree.load();
        let children = match tree.children(inode) {
            Some(children) => children,
            // Only directories have a children map; tell apart a known
            // non-directory (ENOTDIR) from an unknown inode (ENOENT).
            None if tree.node(inode).is_some() => return Err(CoreError::NotADir(inode)),
            None => return Err(CoreError::NoEntry(inode)),
        };
        Ok(children
            .iter()
            .map(|(name, &child)| (name.clone(), child, tree.is_dir(child)))
            .collect())
    }

    pub fn read(&self, inode: u64, fh: u64, offset: u64, size: u64) -> Result<Vec<u8>> {
        // Fast path: serve from the per-handle fd + cached layout (no open/stat).
        if fh != 0 {
            let handle = self.handles.get((fh - 1) as usize).map(|g| Arc::clone(&g));
            if let Some(h) = handle {
                return self
                    .pool
                    .with(|db| read_at_with_file(&h.resolved, db, &h.file, offset, size));
            }
        }
        // Fallback (no prior open, or unknown handle): resolve by inode and open.
        let track_id = {
            let tree = self.tree.load();
            match tree.node(inode) {
                None => return Err(CoreError::NoEntry(inode)),
                Some(node) => match &node.kind {
                    NodeKind::Dir => return Err(CoreError::IsDir(inode)),
                    NodeKind::File { track_id } => *track_id,
                },
            }
        };
        self.pool.with(|db| {
            let resolved = self.cache.resolve(db, track_id)?;
            read_at(&resolved, db, offset, size)
        })
    }

    /// Open a file handle: resolve + validate the layout and open the backing fd
    /// once, store it, and return a non-zero handle id. Subsequent `read`s with
    /// this handle reuse the fd (no per-read open/stat).
    pub fn open_handle(&self, inode: u64) -> Result<u64> {
        let track_id = {
            let tree = self.tree.load();
            match tree.node(inode) {
                None => return Err(CoreError::NoEntry(inode)),
                Some(node) => match &node.kind {
                    NodeKind::Dir => return Err(CoreError::IsDir(inode)),
                    NodeKind::File { track_id } => *track_id,
                },
            }
        };
        let resolved = self.pool.with(|db| self.cache.resolve(db, track_id))?;
        crate::metrics::on_open();
        let file = std::fs::File::open(&resolved.backing_path)?;
        validate_opened_backing(&file, &resolved)?;
        fh_from_key(self.handles.insert(Arc::new(Handle { resolved, file })))
    }

    /// Drop an open handle (closes its backing fd when the last reference goes).
    pub fn release_handle(&self, fh: u64) {
        if fh != 0 {
            self.handles.remove((fh - 1) as usize);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use musefs_format::{RegionLayout, Segment};
    
    #[test]
    fn fh_from_key_offsets_by_one_and_maps_full_to_error() {
        // None (slab at capacity) -> HandleTableFull.
        assert!(matches!(fh_from_key(None), Err(CoreError::HandleTableFull)));
        // Some(key) -> key + 1, so the fh is always non-zero (0 == "no handle").
        assert_eq!(fh_from_key(Some(0)).unwrap(), 1);
        assert_eq!(fh_from_key(Some(41)).unwrap(), 42);
    }

    #[test]
    fn validate_opened_backing_rejects_mismatched_descriptor_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let expected_path = dir.path().join("expected.flac");
        let replacement_path = dir.path().join("replacement.flac");
        std::fs::write(&expected_path, [1_u8; 8]).unwrap();
        std::fs::write(&replacement_path, [2_u8; 16]).unwrap();
        let expected_meta = std::fs::metadata(&expected_path).unwrap();
        let replacement = std::fs::File::open(&replacement_path).unwrap();

        let resolved = ResolvedFile {
            layout: RegionLayout::new(vec![Segment::BackingAudio { offset: 0, len: 8 }]),
            total_len: 8,
            content_version: 1,
            backing_path: expected_path,
            backing_size: expected_meta.len(),
            backing_mtime_secs: mtime_secs(&expected_meta),
            mtime_secs: mtime_secs(&expected_meta),
            cache_bytes: 0,
        };

        assert!(matches!(
            validate_opened_backing(&replacement, &resolved),
            Err(CoreError::BackingChanged(_))
        ));
    }
}
