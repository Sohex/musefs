use std::collections::{BTreeMap, HashMap};
use std::num::NonZeroU64;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use arc_swap::ArcSwap;
use musefs_db::Db;
use musefs_db::convert::usize_from;

use crate::db_pool::DbPool;
use crate::error::{CoreError, Result};
use crate::freshness::BackingStamp;
use crate::reader::{HeaderCache, ResolvedFile, read_at_into, read_at_with_file_into};
use crate::refresh_diff::TrackRenderState;
use crate::template::Template;
use crate::tree::{InodeAllocator, NodeKind, VirtualTree};

/// How the mount serves file *contents*. The virtual tree is identical either way.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Splice a freshly synthesized metadata region in front of the backing audio.
    Synthesis,
    /// Pure passthrough: serve the original backing file bytes unchanged.
    /// Where the kernel supports FUSE passthrough (6.9+) and the daemon holds
    /// CAP_SYS_ADMIN (the kernel gates backing-fd registration), reads are
    /// served directly from the backing fd registered at open — open-time
    /// validation only: a handle held across a backing-file replacement keeps
    /// serving the inode it opened (plain POSIX fd semantics); new opens
    /// re-resolve. Without the capability, reads fall back to the daemon.
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
    /// Compare filenames case-insensitively (dirs merge, files disambiguate).
    /// Set by the CLI (`--case-insensitive`), default true on macOS.
    pub case_insensitive: bool,
    /// Global read-ahead RAM envelope in bytes. `0` disables read-ahead.
    pub read_ahead_budget: u64,
    /// Enable Phase-2 background prefetch threads. Off by default: Phase-1 read
    /// amplification carries the entire measured read-ahead win (#255); the
    /// prefetch threads add overhead without benefit on the backends tested.
    pub read_ahead_prefetch: bool,
    /// Drop a track from the mount when a top-level template field is unresolved,
    /// instead of substituting `default_fallback`. Per-field fallback chains and
    /// `[...]` sections are unaffected. Set by the CLI (`--skip-on-missing`).
    pub skip_on_missing: bool,
}

/// Attributes the FUSE layer maps onto `fuser::FileAttr`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attr {
    pub inode: u64,
    pub is_dir: bool,
    pub size: u64,
    pub mtime_secs: i64,
}

struct Handle {
    track_id: i64,
    resolved: arc_swap::ArcSwap<ResolvedFile>,
    generation: AtomicU64,
    file: Arc<std::fs::File>,
    readahead: Arc<Mutex<crate::readahead::ReadAhead>>,
    registered: AtomicBool,
    epoch: Arc<AtomicU64>,
    /// Absolute backing offset through which prefetch jobs were already
    /// dispatched, so a sequential stream does not re-request buffered windows.
    prefetched_upto: AtomicU64,
    /// Shared so the read-ahead pool registration is cleaned up on the handle's
    /// FINAL drop, not eagerly in `release_handle`. A read that races a release
    /// holds an `Arc<Handle>` clone, so the buffer (and its budget charge) stays
    /// alive until that read finishes; deregistering here, keyed by the buffer's
    /// address, then frees exactly that stream's charge with no leak or reuse.
    pool: Arc<crate::readahead::ReadAheadPool>,
}

impl Handle {
    /// Stable pool key for this handle's read-ahead buffer: its heap address,
    /// unique for the buffer's lifetime (the handle holds the `Arc`, so the
    /// address can't be reused while still registered).
    fn pool_key(&self) -> usize {
        Arc::as_ptr(&self.readahead) as usize
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        // Runs when the last Arc<Handle> drops — after any in-flight read that
        // re-registered the buffer post-release — so the budget never leaks.
        self.pool.deregister(self.pool_key());
    }
}

/// An owned view of an open handle's backing fd, for FUSE passthrough
/// registration. Holds its own `Arc<Handle>`, so the fd outlives a concurrent
/// slab removal while the registration ioctl is in flight.
pub struct PassthroughFd(Arc<Handle>);

impl std::os::fd::AsFd for PassthroughFd {
    fn as_fd(&self) -> std::os::fd::BorrowedFd<'_> {
        self.0.file.as_fd()
    }
}

/// A cached file size/attr entry: validated at `content_version`, plus the
/// backing-file stamp it was built from so `getattr` can re-stat on a hit and
/// catch an on-disk backing change that left `content_version` untouched (#279).
#[derive(Clone, Copy)]
struct SizeEntry {
    content_version: i64,
    total_len: u64,
    mtime_secs: i64,
    stamp: BackingStamp,
}

fn validate_opened_backing(file: &std::fs::File, resolved: &ResolvedFile) -> Result<()> {
    let meta = file.metadata()?;
    if BackingStamp::from_metadata(&meta) != resolved.stamp {
        return Err(CoreError::BackingChanged(
            resolved.backing_path.to_string_lossy().into_owned(),
        ));
    }
    Ok(())
}

/// The composed read-only filesystem: the store, the rendered tree, and the
/// lazy synthesis cache. All methods take `&self`; the tree is swapped
/// atomically on refresh, the cache is internally sharded (each shard mutex-guarded),
/// and the data-version stamp is atomic. This makes `Musefs` `Sync`, so the FUSE
/// layer can later share it across a worker pool.
pub struct Musefs {
    pool: DbPool,
    config: MountConfig,
    /// Compiled once from `config.template`; rendering never re-parses.
    template: Template,
    tree: ArcSwap<VirtualTree>,
    cache: HeaderCache,
    last_data_version: AtomicI64,
    /// Bumped on every non-empty refresh (see `poll_refresh_notify`). Open handles
    /// stamp their `gen` with the current value at `open_handle` and re-resolve
    /// when the global value moves ahead of theirs, so a held handle cannot serve
    /// a layout that was invalidated by a refresh the kernel did not yet see.
    refresh_gen: AtomicU64,
    handles: sharded_slab::Slab<Arc<Handle>>,
    readahead_pool: Arc<crate::readahead::ReadAheadPool>,
    prefetch: Option<crate::readahead::PrefetchWorkers>,

    /// Live count of entries in `handles` (telemetry: `sharded_slab` has no O(1)
    /// `len()`). Incremented only on a successful slab insert, decremented only on
    /// a successful remove, so it tracks slab occupancy exactly (#394).
    handles_open: std::sync::atomic::AtomicUsize,
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
    /// Forces the poll `data_version` read to fail, so a test can exercise the
    /// read-error backoff stamp without a really-broken poll connection (#369).
    force_poll_read_error: AtomicBool,
    force_apply_fail: AtomicBool,
    /// Forces the next N binary-tag `content_version` guard checks in
    /// `read_into` to report a stale layout, simulating a writer committing to
    /// the same track on every retry. Lets a test pin the exact retry bound
    /// without racing a real concurrent writer (the mismatch window is too
    /// narrow to hit deterministically). Counts down; 0 disables. Test-only:
    /// the field and its hot-path check are absent from release builds.
    #[cfg(test)]
    force_version_mismatch: AtomicU64,
    /// Polls that took the changelog-gap full-rebuild path (observability for
    /// tests: incremental vs gap is invisible in the resulting tree).
    gap_fallbacks: AtomicU64,
    /// Set when a poisoned VFS-state lock is recovered; the next `poll_refresh`
    /// forces a full rebuild from the DB and clears it (#96).
    needs_rebuild: AtomicBool,
    /// Changelog watermark: the highest `seq` consumed by a successful refresh.
    /// Drives the O(changed) changelog path in `rebuild_incremental`.
    last_seq: AtomicI64,
}

/// A FUSE file handle: the sharded-slab key offset by one, so the wire value
/// is never 0 (`0` on the wire means "no handle" — `read` falls back to inode
/// resolution).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fh(NonZeroU64);

impl Fh {
    /// Sole site of the `+1`: slab key → wire-safe non-zero handle.
    /// `NonZeroU64::MIN.saturating_add` is panic-free, overflow-proof, and
    /// non-zero by construction.
    fn from_slab_key(key: usize) -> Fh {
        Fh(NonZeroU64::MIN.saturating_add(key as u64))
    }

    /// Sole site of the `-1`: handle → slab key.
    fn slab_key(self) -> usize {
        usize_from(self.0.get() - 1)
    }

    /// The raw wire value handed to the kernel.
    pub fn get(self) -> u64 {
        self.0.get()
    }
}

/// Wire → type, for the FUSE layer's boundary conversion.
impl From<NonZeroU64> for Fh {
    fn from(raw: NonZeroU64) -> Fh {
        Fh(raw)
    }
}

/// Map a `sharded_slab::Slab` insert result to a file handle. `None` means the
/// slab is at capacity, surfaced as an explicit error rather than a panic.
fn fh_from_key(key: Option<usize>) -> Result<Fh> {
    key.map(Fh::from_slab_key).ok_or(CoreError::HandleTableFull)
}

impl Musefs {
    pub fn open(db: Db, config: MountConfig) -> Result<Musefs> {
        let mut alloc = InodeAllocator::new(config.case_insensitive);
        // Capture both freshness stamps BEFORE the build: a write landing during
        // build_full then leaves data_version > stamp (the first poll triggers)
        // and seq > watermark (the changelog replays it) — at worst one redundant
        // refresh. Stamping after the build could record the writer's
        // data_version/seq against a tree that predates it: a permanently missed
        // update, since the next poll would see both stamps as current.
        let last_data_version = db.data_version()?;
        let last_seq = db.changelog_since(i64::MAX)?.max_seq;
        let template = Template::parse(&config.template)?;
        let (tree, snapshot) = Self::build_full(&db, &template, &config, &mut alloc)?;
        let poll_interval = config.poll_interval;
        let read_ahead_budget = config.read_ahead_budget;
        let read_ahead_prefetch = config.read_ahead_prefetch;
        Ok(Musefs {
            cache: HeaderCache::new(config.mode),
            last_data_version: AtomicI64::new(last_data_version),
            refresh_gen: AtomicU64::new(0),
            tree: ArcSwap::from_pointee(tree),
            pool: DbPool::new(db)?,
            config,
            template,
            handles: sharded_slab::Slab::new(),
            readahead_pool: Arc::new(crate::readahead::ReadAheadPool::new(read_ahead_budget)),
            // Phase 2 (background prefetch threads) runs only when read-ahead is
            // on AND explicitly opted in. Off by default: Phase-1 amplification
            // carries the whole win, and the threads add ~10% overhead without
            // benefit on the backends benchmarked (#255).
            prefetch: if read_ahead_budget > 0 && read_ahead_prefetch {
                Some(crate::readahead::PrefetchWorkers::new(2))
            } else {
                None
            },
            handles_open: std::sync::atomic::AtomicUsize::new(0),
            size_cache: dashmap::DashMap::new(),
            last_poll: Mutex::new(std::time::Instant::now()),
            last_failed_refresh: Mutex::new(None),
            poll_interval,
            refresh_retry_backoff: refresh::retry_backoff_for(poll_interval),
            refreshing: AtomicBool::new(false),
            inodes: Mutex::new(alloc),
            snapshot: Mutex::new(snapshot),
            force_rebuild_error: AtomicBool::new(false),
            force_poll_read_error: AtomicBool::new(false),
            force_apply_fail: AtomicBool::new(false),
            #[cfg(test)]
            force_version_mismatch: AtomicU64::new(0),
            gap_fallbacks: AtomicU64::new(0),
            needs_rebuild: AtomicBool::new(false),
            last_seq: AtomicI64::new(last_seq),
        })
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
                        });
                    }
                    NodeKind::File { track_id } => *track_id,
                },
            }
        };
        let (size, mtime_secs) = self.pool.with(|db| {
            // Cheap, indexed: the current content_version drives lazy invalidation.
            // Only the two columns the validation needs — no full-row materialization.
            let (content_version, backing_path) = db
                .track_version_and_path(track_id)?
                .ok_or(CoreError::TrackNotFound(track_id))?;
            // `.map(|e| *e)` copies the SizeEntry (Copy) so the shard Ref drops
            // before the miss-path insert below — same key → same shard, and
            // holding the Ref across the re-lock would deadlock.
            if let Some(e) = self.size_cache.get(&track_id).map(|e| *e)
                && e.content_version == content_version
            {
                // Hit: re-stat the backing file (no synthesis) and compare to
                // the stamp the cached attrs were built from. An on-disk change
                // that left content_version untouched would otherwise let
                // getattr advertise stale attrs — the one metadata surface that
                // could outrun a backing change (read/open already re-stat).
                crate::metrics::on_stat();
                let meta = std::fs::metadata(&backing_path)?;
                if BackingStamp::from_metadata(&meta) != e.stamp {
                    return Err(CoreError::BackingChanged(backing_path));
                }
                return Ok((e.total_len, e.mtime_secs));
            }
            // Miss: full resolve (validates via stat, builds + caches the layout).
            let resolved = self.cache.resolve(db, track_id)?;
            self.size_cache.insert(
                track_id,
                SizeEntry {
                    content_version,
                    total_len: resolved.total_len,
                    mtime_secs: resolved.mtime_secs,
                    stamp: resolved.stamp,
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

    /// Serve a read into `out` (cleared first). The FUSE layer passes a reused
    /// per-worker buffer so the hot path allocates nothing per read (#70).
    /// Serve `[offset, offset+size)` through the per-handle read-ahead buffer,
    /// then (when Phase-2 prefetch is enabled and the stream is sequential)
    /// enqueue depth-adaptive next-window jobs. Shared by the binary-tag
    /// (snapshotted) and plain read branches of `read_into`.
    fn serve_backing<M>(
        &self,
        h: &Handle,
        db: &musefs_db::Db<M>,
        r: &ResolvedFile,
        offset: u64,
        size: u64,
        out: &mut Vec<u8>,
    ) -> Result<()> {
        // Keyed by the buffer address (not the slab key) so the handle's Drop can
        // deregister it after a racing release; see Handle::pool_key / Drop.
        let key = h.pool_key();
        if !h.registered.swap(true, Ordering::AcqRel) {
            self.readahead_pool.register(key, Arc::clone(&h.readahead));
        }
        let backing_len = r.stamp.size;
        let br = crate::readahead::BackingReader::new(
            &h.file,
            &h.readahead,
            &self.readahead_pool,
            key,
            backing_len,
            &h.epoch,
        );
        read_at_with_file_into(r, db, &br, offset, size, out)?;

        let Some(pf) = &self.prefetch else {
            return Ok(());
        };
        // Adaptive depth: keep roughly one per-stream budget share in flight.
        // The window grows geometrically while sequential, so `cap / window`
        // windows of the current size sum to about `cap`; clamp the thread
        // fan-out to a small bound. A seek resets `window` to the floor, which
        // raises depth again — no separate ramp counter is needed. `plan_prefetch`
        // deduplicates against the per-handle watermark so a sequential stream
        // enqueues only the freshly-exposed tail rather than re-requesting
        // already-buffered windows. The watermark read/update sits under the
        // buffer lock that also serialises concurrent reads of this handle.
        let cap = self.readahead_pool.per_stream_cap();
        let (starts, window) = {
            let mut ra = h.readahead.lock().unwrap();
            let start = ra.next_expected();
            let window = ra.window();
            let depth = crate::readahead::prefetch_depth(cap, window);
            let ring = usize::try_from(depth).unwrap_or(4) + 1;
            ra.set_max_windows(ring);
            let (starts, upto) = crate::readahead::plan_prefetch(
                h.prefetched_upto.load(Ordering::Relaxed),
                start,
                window,
                depth,
                backing_len,
            );
            h.prefetched_upto.store(upto, Ordering::Relaxed);
            (starts, window)
        };
        if starts.is_empty() {
            return Ok(());
        }
        let dispatched_epoch = h.epoch.load(Ordering::Acquire);
        for s in starts {
            pf.request(crate::readahead::PrefetchJob {
                file: Arc::clone(&h.file),
                buf: Arc::clone(&h.readahead),
                pool: Arc::clone(&self.readahead_pool),
                epoch: Arc::clone(&h.epoch),
                dispatched_epoch,
                start: s,
                len: window,
                backing_len,
            });
        }
        Ok(())
    }

    pub fn read_into(
        &self,
        inode: u64,
        fh: Option<Fh>,
        offset: u64,
        size: u64,
        out: &mut Vec<u8>,
    ) -> Result<()> {
        out.clear();
        // Fast path: serve from the per-handle fd + cached layout (no open/stat).
        if let Some(fh) = fh {
            let handle = self.handles.get(fh.slab_key()).map(|g| Arc::clone(&g));
            if let Some(h) = handle {
                // Bounded retry absorbs a refresh or same-track re-tag landing
                // mid-read. A batch import touching distinct tracks won't loop
                // here, but a writer tight-looping commits to *this* track can
                // race every attempt and exhaust the bound — see the
                // `BackingChanged` return below for what that surfaces.
                for _attempt in 0..4 {
                    out.clear();
                    let cur = self.refresh_gen.load(Ordering::Acquire);
                    if h.generation.load(Ordering::Acquire) != cur {
                        // A refresh changed something; re-resolve (cheap content_version
                        // cache hit when this track is unchanged) and re-stamp.
                        let fresh = self.pool.with(|db| self.cache.resolve(db, h.track_id))?;
                        // If a refresh raced the resolve, `fresh` may already be stale;
                        // don't publish it under `cur` — retry against the newer gen.
                        if self.refresh_gen.load(Ordering::Acquire) != cur {
                            continue;
                        }
                        h.resolved.store(fresh);
                        h.generation.store(cur, Ordering::Release);
                    }
                    let resolved = h.resolved.load();
                    let r: &ResolvedFile = &resolved;
                    // Re-stat the held fd every read: a pure in-place backing
                    // rewrite (same inode) leaves both DB-side staleness signals
                    // unchanged, so this is the only check that catches it. A
                    // genuine drift is terminal — propagate, don't retry the loop.
                    validate_opened_backing(&h.file, r)?;
                    let served = self.pool.with(|db| -> Result<Option<()>> {
                        if r.has_binary_tag {
                            // Snapshot-consistent: version check + blob reads see one
                            // WAL snapshot, so a reused rowid can't be served.
                            db.begin_read()?;
                            let res = (|| {
                                // A test seam forces the first N checks stale to
                                // drive the same-track retry-exhaustion path
                                // deterministically; compiled out of release builds.
                                #[cfg(test)]
                                let forced = self
                                    .force_version_mismatch
                                    .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                                        n.checked_sub(1)
                                    })
                                    .is_ok();
                                #[cfg(not(test))]
                                let forced = false;
                                if forced
                                    || db.track_content_version(h.track_id)? != r.content_version
                                {
                                    return Ok(None); // stale layout — retry after re-resolve
                                }
                                self.serve_backing(&h, db, r, offset, size, out)?;
                                Ok(Some(()))
                            })();
                            let _ = db.end_read(); // always release the snapshot
                            res
                        } else {
                            self.serve_backing(&h, db, r, offset, size, out)?;
                            Ok(Some(()))
                        }
                    })?;
                    if served.is_some() {
                        return Ok(());
                    }
                    // Stale layout: force a re-resolve next iteration against the live version.
                    let fresh = self.pool.with(|db| self.cache.resolve(db, h.track_id))?;
                    h.resolved.store(fresh);
                    h.generation
                        .store(self.refresh_gen.load(Ordering::Acquire), Ordering::Release);
                }
                // Pathological constant re-tagging raced every attempt; surface a
                // retryable error rather than risk wrong bytes.
                return Err(CoreError::BackingChanged(
                    h.resolved
                        .load()
                        .backing_path
                        .to_string_lossy()
                        .into_owned(),
                ));
            }
        }
        // Fallback (no prior open, or unknown handle): resolve by inode and open.
        let track_id = self.track_id_for(inode)?;
        self.pool.with(|db| {
            let resolved = self.cache.resolve(db, track_id)?;
            read_at_into(&resolved, db, offset, size, out)
        })
    }

    /// Allocating form of `read_into`.
    pub fn read(&self, inode: u64, fh: Option<Fh>, offset: u64, size: u64) -> Result<Vec<u8>> {
        let mut out = Vec::new();
        self.read_into(inode, fh, offset, size, &mut out)?;
        Ok(out)
    }

    /// Resolve a file `inode` to its `track_id` for the read/open fast paths,
    /// erroring on an unknown inode (`NoEntry`) or a directory (`IsDir`). The
    /// `read_into` fallback and `open_handle` share this; `getattr` deliberately
    /// diverges (it returns an attr for directories rather than erroring).
    fn track_id_for(&self, inode: u64) -> Result<i64> {
        let tree = self.tree.load();
        match tree.node(inode) {
            None => Err(CoreError::NoEntry(inode)),
            Some(node) => match &node.kind {
                NodeKind::Dir => Err(CoreError::IsDir(inode)),
                NodeKind::File { track_id } => Ok(*track_id),
            },
        }
    }

    /// Open a file handle: resolve + validate the layout and open the backing fd
    /// once, store it, and return a handle. Subsequent `read`s with this handle
    /// reuse the fd (no per-read open/stat).
    pub fn open_handle(&self, inode: u64) -> Result<Fh> {
        let track_id = self.track_id_for(inode)?;
        // Snapshot the generation BEFORE resolving: if a refresh lands during the
        // resolve, stamping the post-refresh gen onto this (pre-refresh) layout
        // would make the first read skip re-resolution and serve stale bytes. With
        // the pre-resolve gen, a racing refresh leaves gen behind refresh_gen, so
        // the next read re-resolves.
        let generation = self.refresh_gen.load(Ordering::Acquire);
        let resolved = self.pool.with(|db| self.cache.resolve(db, track_id))?;
        crate::metrics::on_open();
        let file = Arc::new(std::fs::File::open(&resolved.backing_path)?);
        validate_opened_backing(&file, &resolved)?;
        let key = self.handles.insert(Arc::new(Handle {
            track_id,
            resolved: arc_swap::ArcSwap::from(resolved),
            generation: AtomicU64::new(generation),
            file,
            readahead: Arc::new(Mutex::new(crate::readahead::ReadAhead::new(
                self.readahead_pool.per_stream_cap(),
            ))),
            registered: AtomicBool::new(false),
            epoch: Arc::new(AtomicU64::new(0)),
            prefetched_upto: AtomicU64::new(0),
            pool: Arc::clone(&self.readahead_pool),
        }));
        if key.is_some() {
            self.handles_open.fetch_add(1, Ordering::Relaxed);
        }
        fh_from_key(key)
    }

    /// Drop an open handle (closes its backing fd when the last reference goes).
    pub fn release_handle(&self, fh: Fh) {
        let key = fh.slab_key();
        if let Some(h) = self.handles.get(key) {
            h.epoch.fetch_add(1, Ordering::AcqRel);
        }
        // Pool deregistration is the handle's Drop responsibility, not done here:
        // a read racing this release still holds an Arc<Handle>, so eagerly
        // deregistering would drop the entry before that read's charge lands,
        // leaking it. Drop runs once the last reference (the in-flight read) goes.
        if self.handles.remove(key) {
            self.handles_open.fetch_sub(1, Ordering::Relaxed);
        }
    }

    /// Test accessor: are the Phase-2 prefetch worker threads running?
    #[cfg(test)]
    pub(crate) fn prefetch_workers_active(&self) -> bool {
        self.prefetch.is_some()
    }

    /// Test accessor: bytes currently charged against the read-ahead budget.
    #[cfg(test)]
    pub(crate) fn pool_charged(&self) -> u64 {
        self.readahead_pool.charged()
    }

    /// The backing fd behind `fh`, for kernel passthrough registration. `Some`
    /// only in StructureOnly mode, where the served bytes ARE the backing file;
    /// in Synthesis mode the bytes are spliced, so no single fd represents
    /// them. `None` also for a stale or released handle.
    pub fn passthrough_fd(&self, fh: Fh) -> Option<PassthroughFd> {
        if self.config.mode != Mode::StructureOnly {
            return None;
        }
        let handle = self.handles.get(fh.slab_key())?;
        Some(PassthroughFd(Arc::clone(&*handle)))
    }

    /// The mount's serving mode (how file contents are produced).
    pub fn mode(&self) -> Mode {
        self.config.mode
    }

    /// Snapshot the core-owned telemetry for the `.musefs-metrics` surface (#394).
    /// Cheap: atomic loads plus three length reads (the `inodes` mutex is taken
    /// briefly; a poisoned lock flags `needs_rebuild` via `lock_or_flag`, the same
    /// self-heal contract as every other VFS-state lock site).
    pub fn telemetry(&self) -> crate::telemetry::CoreTelemetry {
        let tree_nodes = self.tree.load().node_count() as u64;
        let inode_paths = crate::lock::lock_or_flag(&self.inodes, &self.needs_rebuild, "inodes")
            .interned_path_count() as u64;
        crate::telemetry::CoreTelemetry {
            handles_open: self.handles_open.load(Ordering::Relaxed) as u64,
            cache_header_entries: self.cache.entry_count(),
            cache_header_bytes: self.cache.weight_bytes(),
            cache_header_bytes_max: self.cache.budget_bytes(),
            cache_header_hits: self.cache.raw_hits(),
            cache_header_misses: self.cache.raw_misses(),
            cache_size_entries: self.size_cache.len() as u64,
            readahead_budget_bytes: self.readahead_pool.budget(),
            readahead_charged_bytes: self.readahead_pool.charged(),
            tree_nodes,
            inode_paths,
            refresh_generation: self.refresh_gen.load(Ordering::Acquire),
            refresh_gap_fallbacks: self.gap_fallbacks.load(Ordering::Relaxed),
            refresh_needs_rebuild: self.needs_rebuild.load(Ordering::Relaxed),
        }
    }
}

mod refresh;

#[cfg(test)]
mod tests;
