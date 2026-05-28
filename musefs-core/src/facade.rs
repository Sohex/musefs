use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use arc_swap::ArcSwap;
use musefs_db::Db;

use crate::db_pool::DbPool;
use crate::error::{CoreError, Result};
use crate::mapping::tags_to_fields;
use crate::reader::{read_at, read_at_with_file, HeaderCache, ResolvedFile};
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
    handles: Mutex<HashMap<u64, Arc<Handle>>>,
    next_fh: AtomicU64,
    /// `SizeEntry` keyed by track id. Tiny entries, effectively unbounded; serves
    /// getattr/lookup without a backing stat or full synthesis. Self-invalidates on
    /// a content_version change.
    size_cache: Mutex<HashMap<i64, SizeEntry>>,
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
    /// Last-seen `content_version` per track, snapshotted on each rebuild, used to
    /// report which inodes changed so the FUSE layer can drop stale kernel cache.
    versions: Mutex<HashMap<i64, i64>>,
    force_rebuild_error: AtomicBool,
}

impl Musefs {
    pub fn open(db: Db, config: MountConfig) -> Result<Musefs> {
        let mut alloc = InodeAllocator::new();
        let (tree, versions) = Self::build_tree(&db, &config, &mut alloc)?;
        let last_data_version = db.data_version()?;
        let poll_interval = config.poll_interval;
        Ok(Musefs {
            cache: HeaderCache::new(config.mode),
            last_data_version: AtomicI64::new(last_data_version),
            tree: ArcSwap::from_pointee(tree),
            pool: DbPool::new(db)?,
            config,
            handles: Mutex::new(HashMap::new()),
            next_fh: AtomicU64::new(0),
            size_cache: Mutex::new(HashMap::new()),
            last_poll: Mutex::new(std::time::Instant::now()),
            last_failed_refresh: Mutex::new(None),
            poll_interval,
            refresh_retry_backoff: retry_backoff_for(poll_interval),
            refreshing: AtomicBool::new(false),
            inodes: Mutex::new(alloc),
            versions: Mutex::new(versions),
            force_rebuild_error: AtomicBool::new(false),
        })
    }

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

    /// Rebuild the tree from the current DB contents (used after external edits).
    ///
    /// Not single-flighted: do not run concurrently with `poll_refresh` (or another
    /// `refresh`) — two overlapping rebuilds can publish a stale tree. The production
    /// path goes through `poll_refresh`, which guards entry with the `refreshing` CAS;
    /// this entry point exists for forced, unconditional rebuilds (e.g. tests). It
    /// also refreshes the `content_version` snapshot, so it must not race a
    /// `poll_refresh` whose change-diff relies on that snapshot.
    pub fn refresh(&self) -> Result<()> {
        let versions = self.rebuild()?;
        *self
            .versions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = versions;
        Ok(())
    }

    /// Rebuild + publish the tree; returns the current `track_id -> content_version`
    /// map (the caller decides whether/how to diff it).
    fn rebuild(&self) -> Result<HashMap<i64, i64>> {
        if self.force_rebuild_error.load(Ordering::Acquire) {
            return Err(CoreError::BackingChanged(
                "forced refresh failure".to_string(),
            ));
        }
        let (tree, versions) = self.pool.with(|db| {
            let mut alloc = self
                .inodes
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            Self::build_tree(db, &self.config, &mut alloc)
        })?;
        self.tree.store(Arc::new(tree));
        Ok(versions)
    }

    // Lock order: acquire a DbPool connection (`pool.with`/`with_poll`) FIRST, then
    // any of the in-memory locks (`inodes`, `size_cache`, the header cache's shards,
    // `handles`). `inodes` is held inside `pool.with` during `refresh` — that is the
    // one intentional exception where a pool connection is held around an in-memory
    // lock; all other in-memory locks must never be held while calling into the pool.
    // Those in-memory locks are independent siblings and are each held only briefly
    // (a map get/insert) — never across a pool/DB call (except `inodes` as above).
    // The header cache is internally sharded; `resolve` does its stat/synthesis
    // off-lock and locks a shard only for the get/insert.

    fn handles(&self) -> std::sync::MutexGuard<'_, HashMap<u64, Arc<Handle>>> {
        self.handles
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn size_cache(&self) -> std::sync::MutexGuard<'_, HashMap<i64, SizeEntry>> {
        self.size_cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

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
        let old_versions = self
            .versions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let new_versions = match self.rebuild() {
            Ok(versions) => versions,
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
        self.size_cache().retain(|k, _| live.contains(k));

        // Invalidates inodes for content-changed tracks (path is stable, bytes changed).
        for (tid, ver) in &new_versions {
            if old_versions.get(tid).is_some_and(|old| old != ver) {
                if let Some(ino) = tree.inode_of_track(*tid) {
                    on_changed(ino);
                }
            }
        }

        // Invalidates old inodes for tracks whose path changed or were removed.
        for (tid, old_ver) in &old_versions {
            if !new_versions.contains_key(tid) {
                // Track was removed — invalidate its old inode.
                if let Some(ino) = old_tree.inode_of_track(*tid) {
                    on_changed(ino);
                }
            } else if new_versions.get(tid) != Some(old_ver) {
                // Content changed AND path may have changed — invalidate old inode.
                let old_ino = old_tree.inode_of_track(*tid);
                let new_ino = tree.inode_of_track(*tid);
                if old_ino != new_ino {
                    if let Some(ino) = old_ino {
                        on_changed(ino);
                    }
                }
            }
        }

        *self
            .versions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = new_versions;

        self.last_data_version.store(version, Ordering::Release);
        self.stamp_successful_poll();

        Ok(true)
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
            if let Some(e) = self.size_cache().get(&track_id).copied() {
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
            self.size_cache().insert(
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
            let handle = self.handles().get(&fh).cloned();
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
        let fh = self.next_fh.fetch_add(1, Ordering::Relaxed) + 1;
        self.handles()
            .insert(fh, Arc::new(Handle { resolved, file }));
        Ok(fh)
    }

    /// Drop an open handle (closes its backing fd when the last reference goes).
    pub fn release_handle(&self, fh: u64) {
        self.handles().remove(&fh);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use musefs_format::{RegionLayout, Segment};
    use once_cell::sync::OnceCell;

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
            ogg_index: OnceCell::new(),
            cache_bytes: 0,
        };

        assert!(matches!(
            validate_opened_backing(&replacement, &resolved),
            Err(CoreError::BackingChanged(_))
        ));
    }
}
