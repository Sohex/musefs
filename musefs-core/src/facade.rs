use std::collections::{BTreeMap, HashMap};
use std::num::NonZeroU64;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use arc_swap::ArcSwap;
use musefs_db::convert::usize_from;
use musefs_db::{Db, Format};

use crate::db_pool::DbPool;
use crate::error::{CoreError, Result};
use crate::freshness::BackingStamp;
use crate::mapping::tags_to_fields;
use crate::reader::{HeaderCache, ResolvedFile, read_at_into, read_at_with_file_into};
use crate::refresh_diff::{ChangeSet, TrackRenderState, partition_changelog};
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
}

/// Attributes the FUSE layer maps onto `fuser::FileAttr`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attr {
    pub inode: u64,
    pub is_dir: bool,
    pub size: u64,
    pub mtime_secs: i64,
}

/// An open file handle: the resolved layout, the track it belongs to, the
/// generation at which `resolved` was last validated, and a backing fd opened
/// once at `open`.
///
/// A handle survives `poll_refresh`, but is **not** a frozen snapshot: when the
/// global `refresh_gen` advances (a refresh applied changes), the next `read`
/// re-resolves the track (a cheap `content_version`-keyed cache hit when the
/// track is unchanged) and swaps in the fresh layout. This keeps a re-tagged
/// file's handle consistent with the size the kernel sees via getattr, and
/// prevents a stale `Segment::BinaryTag { payload_id }` from serving reused-rowid
/// bytes after a re-tag.
struct Handle {
    track_id: i64,
    resolved: arc_swap::ArcSwap<ResolvedFile>,
    generation: AtomicU64,
    file: std::fs::File,
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

/// Resets a single-flight flag on drop, so a panic (or early return) during a
/// rebuild can't leave `refreshing` stuck `true` and permanently disable refresh.
struct RefreshGuard<'a>(&'a AtomicBool);

impl Drop for RefreshGuard<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
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

impl Musefs {
    pub fn open(db: Db, config: MountConfig) -> Result<Musefs> {
        let mut alloc = InodeAllocator::new();
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
        Ok(Musefs {
            cache: HeaderCache::new(config.mode),
            last_data_version: AtomicI64::new(last_data_version),
            refresh_gen: AtomicU64::new(0),
            tree: ArcSwap::from_pointee(tree),
            pool: DbPool::new(db)?,
            config,
            template,
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
            #[cfg(test)]
            force_version_mismatch: AtomicU64::new(0),
            gap_fallbacks: AtomicU64::new(0),
            needs_rebuild: AtomicBool::new(false),
            last_seq: AtomicI64::new(last_seq),
        })
    }

    /// Render a single track's path from its tags + format. The one place
    /// `Template::render` is called, shared by full and incremental rebuilds.
    fn render_one(
        template: &Template,
        config: &MountConfig,
        format: musefs_db::Format,
        tags: &[musefs_db::Tag],
    ) -> String {
        let fields = tags_to_fields(tags);
        template.render(
            &fields,
            &config.fallbacks,
            &config.default_fallback,
            format.as_str(),
        )
    }

    /// DB read + path render with no allocator: the lock-free phase shared by
    /// `build_full` and `rebuild_full`. Confining all `Db` access here is what
    /// lets `rebuild_full` hold `inodes` only across the pure-CPU `build_with`.
    ///
    /// The returned entries are ordered by `order_entries` (ascending by track
    /// `id`), which is what makes both full-rebuild paths establish disambiguation
    /// order locally rather than inheriting it from `list_tracks`'s `ORDER BY id`
    /// (#188): the build path's insertion order decides which member of a colliding
    /// path keeps the bare name, and that must match the incremental path's min-id
    /// rule regardless of the source query's ordering.
    #[allow(clippy::type_complexity)]
    fn render_entries<M>(
        db: &Db<M>,
        template: &Template,
        config: &MountConfig,
    ) -> Result<(Vec<(i64, String)>, HashMap<i64, TrackRenderState>)> {
        let tracks = db.list_tracks()?;
        let field_names = template.referenced_fields();
        let keys: Vec<&str> = field_names.iter().map(String::as_str).collect();
        let mut tags_by_track = db.tags_grouped_for_keys(&keys)?;
        let mut entries = Vec::with_capacity(tracks.len());
        let mut snapshot = HashMap::with_capacity(tracks.len());
        for t in &tracks {
            let tags = tags_by_track.remove(&t.id).unwrap_or_default();
            let path = Self::render_one(template, config, t.format, &tags);
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
        Ok((Self::order_entries(entries), snapshot))
    }

    /// Establish the canonical full-rebuild order: ascending by track `id`. This
    /// is the single point that fixes which member of a colliding rendered path
    /// keeps the bare name in `build_with_ci`'s insertion order (#188); it must NOT
    /// move into the build primitive, whose `tree.rs` tests feed it id-unordered
    /// entries on purpose. Kept as a pure helper so its sort is observable (and
    /// mutation-testable) independent of `list_tracks`'s incidental `ORDER BY id`.
    fn order_entries(mut entries: Vec<(i64, String)>) -> Vec<(i64, String)> {
        entries.sort_by_key(|(id, _)| *id);
        entries
    }

    /// Full rebuild: render every track and build the tree from scratch. Used by
    /// `open`, forced `refresh`, and the Stage B fallback. Returns the tree and the
    /// fresh `track_id -> TrackRenderState` snapshot.
    fn build_full<M>(
        db: &Db<M>,
        template: &Template,
        config: &MountConfig,
        alloc: &mut InodeAllocator,
    ) -> Result<(VirtualTree, HashMap<i64, TrackRenderState>)> {
        let (entries, snapshot) = Self::render_entries(db, template, config)?;
        Ok((
            VirtualTree::build_with_ci(&entries, alloc, config.case_insensitive),
            snapshot,
        ))
    }

    /// Force an unconditional rebuild of the tree from the current DB contents.
    /// Test-only: production code refreshes via `poll_refresh`.
    ///
    /// Serialized against `poll_refresh` (and itself) through the same `refreshing`
    /// single-flight gate the production path uses, so overlapping rebuilds can't
    /// publish a stale tree or race the `content_version` snapshot the change-diff
    /// relies on. Unlike `poll_refresh`, it blocks until it owns the gate rather than
    /// bailing out, so the forced rebuild always happens.
    pub fn refresh_for_test(&self) -> Result<()> {
        while self
            .refreshing
            .compare_exchange_weak(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            std::hint::spin_loop();
        }
        let _guard = RefreshGuard(&self.refreshing);
        let snapshot = self.rebuild_full()?;
        *crate::lock::lock_or_flag(&self.snapshot, &self.needs_rebuild, "snapshot") = snapshot;
        Ok(())
    }

    /// Rebuild + publish the tree via a full render; returns the fresh snapshot
    /// (the caller decides whether/how to diff it). Mirrors `rebuild_incremental`'s
    /// ordering: read + render under the pool connection, then lock `inodes` only
    /// across the pure-CPU `build_with` (#90). That leaves the read→publish window
    /// uncovered by any lock, so overlapping calls could publish a stale tree:
    /// callers must be serialized, which they are — the production path runs inside
    /// `poll_refresh_notify`'s `refreshing` CAS, and `refresh` documents the same
    /// no-concurrent-rebuild contract.
    fn rebuild_full(&self) -> Result<HashMap<i64, TrackRenderState>> {
        if self.force_rebuild_error.load(Ordering::Acquire) {
            return Err(CoreError::BackingChanged(
                "forced refresh failure".to_string(),
            ));
        }
        let (entries, snapshot) = self
            .pool
            .with(|db| Self::render_entries(db, &self.template, &self.config))?;
        let mut alloc = crate::lock::lock_or_flag(&self.inodes, &self.needs_rebuild, "inodes");
        let tree = VirtualTree::build_with_ci(&entries, &mut alloc, self.config.case_insensitive);
        alloc.prune_retired(&tree);
        drop(alloc);
        self.tree.store(Arc::new(tree));
        Ok(snapshot)
    }

    /// Full rebuild used to self-heal after a poisoned VFS-state lock: rebuild
    /// from the DB, publish the tree, diff for cache invalidation, and clear the
    /// flag. Bypasses the poll gates (the caller checks `needs_rebuild`).
    fn force_full_rebuild(&self, on_changed: &mut impl FnMut(u64)) -> Result<bool> {
        // Read data_version before rebuilding so a successful self-heal also advances
        // the poll stamp: a write that commits mid-rebuild then leaves a newer version
        // for the next poll (one extra rebuild, never a skipped change), rather than
        // forcing an unconditional rebuild on every subsequent poll.
        let version = self.pool.with_poll(|db| Ok(db.data_version()?))?;
        let new_seq = self
            .pool
            .with_poll(|db| Ok(db.changelog_since(i64::MAX)?.max_seq))?;
        let old_tree = self.tree.load_full();
        let old_snapshot =
            crate::lock::lock_or_flag(&self.snapshot, &self.needs_rebuild, "snapshot").clone();
        let new_snapshot = self.rebuild_full()?;
        let new_tree = self.tree.load();
        let live = new_tree.track_ids();
        self.cache.retain(&live);
        self.size_cache.retain(|k, _| live.contains(k));
        Self::notify_changed(
            &old_snapshot,
            &new_snapshot,
            &old_tree,
            &new_tree,
            on_changed,
        );
        *crate::lock::lock_or_flag(&self.snapshot, &self.needs_rebuild, "snapshot") = new_snapshot;
        self.last_seq.store(new_seq, Ordering::Release);
        self.last_data_version.store(version, Ordering::Release);
        self.refresh_gen.fetch_add(1, Ordering::AcqRel);
        self.needs_rebuild.store(false, Ordering::Release);
        self.stamp_successful_poll();
        Ok(true)
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
        // Gap iff changes may have been pruned past the watermark: an emptied ring
        // while we held a watermark (external truncation), or a retained window
        // that no longer reaches back to it (min_seq > last_seq + 1; equality is
        // an adjacent — contiguous — read, not a gap).
        let gap = if log.max_seq == 0 {
            last_seq > 0
        } else {
            log.min_seq > last_seq + 1
        };
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
                            path: Self::render_one(&self.template, &self.config, fmt, &tags),
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
            Ok(_) => {
                #[cfg(debug_assertions)]
                {
                    let mut ref_alloc = alloc.clone();
                    let mut entries: Vec<(i64, String)> =
                        snap.iter().map(|(&id, s)| (id, s.path.clone())).collect();
                    entries.sort_by_key(|(id, _)| *id);
                    let reference = VirtualTree::build_with_ci(
                        &entries,
                        &mut ref_alloc,
                        self.config.case_insensitive,
                    );
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
                VirtualTree::build_with_ci(&entries, &mut alloc, self.config.case_insensitive)
            }
        };
        alloc.prune_retired(&tree);
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

    // Lock order: acquire a DbPool connection (`pool.with`/`with_poll`) FIRST, then
    // any in-memory lock (`inodes`, the header cache's shards). Both rebuild paths
    // (`rebuild_full`, `rebuild_incremental`) release the pool connection before
    // locking `inodes`, so the order is uniform: a pool connection is never held
    // around an in-memory lock. `handles` is a lock-free
    // `sharded_slab::Slab`: its `get` guard is cloned-from and dropped before any
    // pool call, so it never participates in lock ordering. Slab keys are
    // generation-encoded, so a reused slot produces a different key; a stale `fh`
    // therefore returns `None` from `get` and falls back to inode resolution rather
    // than aliasing a recycled handle (ABA-safe). `size_cache` is a `DashMap`
    // whose per-shard guards are taken and released per op (the `*e` copy drops
    // the read guard before the `insert`; `retain` is never called while a `Ref`
    // is held), so it imposes no problematic lock ordering / no cross-lock cycle.

    /// Cheap, synchronous "is a `data_version` poll worth dispatching?" predicate
    /// for the FUSE dispatch thread to gate `fire_poll_refresh` on, so a
    /// metadata-op storm doesn't flood the worker pool with no-op poll tasks (#89).
    /// Mirrors the early-return gates in `poll_refresh_notify` — keep the two in
    /// sync. Advisory only: no DB access, no `data_version` read, no rebuild. A
    /// stale `true` costs at most one task the inner gate short-circuits, and
    /// `needs_rebuild` is checked first so a self-heal is never debounced away.
    pub fn poll_due(&self) -> bool {
        if self.needs_rebuild.load(Ordering::Acquire) {
            return true;
        }
        if !self.poll_interval.is_zero()
            && crate::lock::lock_recover(&self.last_poll, "last_poll").elapsed()
                < self.poll_interval
        {
            return false;
        }
        if let Some(last_failed) =
            *crate::lock::lock_recover(&self.last_failed_refresh, "last_failed_refresh")
            && last_failed.elapsed() < self.refresh_retry_backoff
        {
            return false;
        }
        true
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
        // These early-return gates are mirrored by the cheap `poll_due` pre-check
        // the FUSE layer runs on the dispatch thread (#89); keep the two in sync.
        // A poisoned VFS-state lock scheduled a full rebuild: do it now,
        // bypassing the debounce / backoff / data_version gates (#96).
        if self.needs_rebuild.load(Ordering::Acquire) {
            // Single-flight with the same flag the normal path uses.
            if self
                .refreshing
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_err()
            {
                return Ok(false);
            }
            let _guard = RefreshGuard(&self.refreshing);
            return self.force_full_rebuild(&mut on_changed);
        }

        if !self.poll_interval.is_zero()
            && crate::lock::lock_recover(&self.last_poll, "last_poll").elapsed()
                < self.poll_interval
        {
            return Ok(false);
        }
        if let Some(last_failed) =
            *crate::lock::lock_recover(&self.last_failed_refresh, "last_failed_refresh")
            && last_failed.elapsed() < self.refresh_retry_backoff
        {
            return Ok(false);
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

        // A folded tree can't use the incremental path (it navigates by exact
        // rendered name, which a merged/folded tree mismatches), so always
        // full-rebuild. This is intentional — NOT a changelog gap — so route
        // through force_full_rebuild to keep the gap counter and the "changelog
        // gap" diagnostics meaningful (the O(changed) fast path stays
        // case-sensitive-only).
        if self.config.case_insensitive {
            return self.force_full_rebuild(&mut on_changed);
        }

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
                self.gap_fallbacks.fetch_add(1, Ordering::AcqRel);
                let new_seq = self
                    .pool
                    .with(|db| Ok(db.changelog_since(i64::MAX)?.max_seq))?;
                let old_snapshot =
                    crate::lock::lock_or_flag(&self.snapshot, &self.needs_rebuild, "snapshot")
                        .clone();
                let new_snapshot = match self.rebuild_full() {
                    Ok(v) => v,
                    Err(err) => {
                        *crate::lock::lock_recover(
                            &self.last_failed_refresh,
                            "last_failed_refresh",
                        ) = Some(std::time::Instant::now());
                        return Err(err);
                    }
                };
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
            if let Some(os) = old.get(tid)
                && os.content_version != ns.content_version
                && os.path == ns.path
                && let Some(ino) = new_tree.inode_of_track(*tid)
            {
                on_changed(ino);
            }
        }
        for (tid, os) in old {
            let moved_or_gone = match new.get(tid) {
                None => true,
                Some(ns) => ns.path != os.path,
            };
            if moved_or_gone && let Some(ino) = old_tree.inode_of_track(*tid) {
                on_changed(ino);
            }
        }
    }

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
            if os.content_version != ns.content_version
                && os.path == ns.path
                && let Some(ino) = new_tree.inode_of_track(id)
            {
                on_changed(ino);
            }
            if ns.path != os.path
                && let Some(ino) = old_tree.inode_of_track(id)
            {
                on_changed(ino);
            }
        }
        for &id in &change.removed {
            if let Some(ino) = displaced.get(&id).and_then(|_| old_tree.inode_of_track(id)) {
                on_changed(ino);
            }
        }
    }

    fn stamp_successful_poll(&self) {
        if !self.poll_interval.is_zero() {
            *crate::lock::lock_recover(&self.last_poll, "last_poll") = std::time::Instant::now();
        }
        *crate::lock::lock_recover(&self.last_failed_refresh, "last_failed_refresh") = None;
    }

    #[doc(hidden)]
    pub fn force_rebuild_errors_for_test(&self, fail: bool) {
        self.force_rebuild_error.store(fail, Ordering::Release);
    }

    #[doc(hidden)]
    pub fn force_apply_failure_for_test(&self, on: bool) {
        self.force_apply_fail.store(on, Ordering::Release);
    }

    /// Force the next `count` binary-tag `content_version` guard checks in
    /// `read_into` to report a stale layout, as if a writer re-tagged this track
    /// between every retry. Used to exercise the retry-exhaustion bound.
    #[cfg(test)]
    fn force_version_mismatches_for_test(&self, count: u64) {
        self.force_version_mismatch.store(count, Ordering::Release);
    }

    /// How many polls took the changelog-gap full-rebuild path. Test-only
    /// observability: the gap and incremental paths produce identical trees, so
    /// only this counter distinguishes them.
    #[doc(hidden)]
    pub fn gap_fallbacks_for_test(&self) -> u64 {
        self.gap_fallbacks.load(Ordering::Acquire)
    }

    #[doc(hidden)]
    pub fn mark_needs_rebuild_for_test(&self) {
        self.needs_rebuild
            .store(true, std::sync::atomic::Ordering::Release);
    }

    #[doc(hidden)]
    pub fn needs_rebuild_is_set_for_test(&self) -> bool {
        self.needs_rebuild
            .load(std::sync::atomic::Ordering::Acquire)
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
        *crate::lock::lock_recover(&self.last_poll, "last_poll") = past;
    }

    /// Stamps a failed-refresh time of "now" so the backoff gate is active, for
    /// tests exercising `poll_due`'s backoff branch without a real failure.
    #[doc(hidden)]
    pub fn fail_refresh_now_for_test(&self) {
        *crate::lock::lock_recover(&self.last_failed_refresh, "last_failed_refresh") =
            Some(std::time::Instant::now());
    }

    /// Backdates the failed-refresh stamp past the retry-backoff window so the
    /// backoff gate no longer blocks (companion to `expire_poll_debounce_for_test`).
    #[doc(hidden)]
    pub fn expire_refresh_backoff_for_test(&self) {
        let past = std::time::Instant::now()
            .checked_sub(self.refresh_retry_backoff)
            .expect("refresh_retry_backoff exceeds monotonic clock base");
        *crate::lock::lock_recover(&self.last_failed_refresh, "last_failed_refresh") = Some(past);
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
            let track = db
                .get_track(track_id)?
                .ok_or(CoreError::TrackNotFound(track_id))?;
            // `.map(|e| *e)` copies the SizeEntry (Copy) so the shard Ref drops
            // before the miss-path insert below — same key → same shard, and
            // holding the Ref across the re-lock would deadlock.
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
                if BackingStamp::from_metadata(&meta) != e.stamp {
                    return Err(CoreError::BackingChanged(track.backing_path.clone()));
                }
                return Ok((e.total_len, e.mtime_secs));
            }
            // Miss: full resolve (validates via stat, builds + caches the layout).
            let resolved = self.cache.resolve(db, track_id)?;
            self.size_cache.insert(
                track_id,
                SizeEntry {
                    content_version: track.content_version,
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
                                read_at_with_file_into(r, db, &h.file, offset, size, out)?;
                                Ok(Some(()))
                            })();
                            let _ = db.end_read(); // always release the snapshot
                            res
                        } else {
                            read_at_with_file_into(r, db, &h.file, offset, size, out)?;
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
            read_at_into(&resolved, db, offset, size, out)
        })
    }

    /// Allocating form of `read_into`.
    pub fn read(&self, inode: u64, fh: Option<Fh>, offset: u64, size: u64) -> Result<Vec<u8>> {
        let mut out = Vec::new();
        self.read_into(inode, fh, offset, size, &mut out)?;
        Ok(out)
    }

    /// Open a file handle: resolve + validate the layout and open the backing fd
    /// once, store it, and return a handle. Subsequent `read`s with this handle
    /// reuse the fd (no per-read open/stat).
    pub fn open_handle(&self, inode: u64) -> Result<Fh> {
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
        // Snapshot the generation BEFORE resolving: if a refresh lands during the
        // resolve, stamping the post-refresh gen onto this (pre-refresh) layout
        // would make the first read skip re-resolution and serve stale bytes. With
        // the pre-resolve gen, a racing refresh leaves gen behind refresh_gen, so
        // the next read re-resolves.
        let generation = self.refresh_gen.load(Ordering::Acquire);
        let resolved = self.pool.with(|db| self.cache.resolve(db, track_id))?;
        crate::metrics::on_open();
        let file = std::fs::File::open(&resolved.backing_path)?;
        validate_opened_backing(&file, &resolved)?;
        fh_from_key(self.handles.insert(Arc::new(Handle {
            track_id,
            resolved: arc_swap::ArcSwap::from(resolved),
            generation: AtomicU64::new(generation),
            file,
        })))
    }

    /// Drop an open handle (closes its backing fd when the last reference goes).
    pub fn release_handle(&self, fh: Fh) {
        self.handles.remove(fh.slab_key());
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use musefs_format::{RegionLayout, Segment};

    #[test]
    fn fh_round_trips_slab_key_and_maps_full_to_error() {
        // None (slab at capacity) -> HandleTableFull.
        assert!(matches!(fh_from_key(None), Err(CoreError::HandleTableFull)));
        // Wire value is the slab key + 1, so the kernel never sees 0 ("no
        // handle"). Non-zero needs no runtime assertion — NonZeroU64 makes a
        // zero handle unrepresentable.
        assert_eq!(fh_from_key(Some(0)).unwrap().get(), 1);
        assert_eq!(fh_from_key(Some(41)).unwrap().get(), 42);
        // The two private conversion methods invert each other.
        assert_eq!(Fh::from_slab_key(0).slab_key(), 0);
        assert_eq!(Fh::from_slab_key(41).slab_key(), 41);
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
            layout: RegionLayout::validated(vec![Segment::BackingAudio { offset: 0, len: 8 }])
                .unwrap(),
            total_len: 8,
            content_version: 1,
            backing_path: expected_path,
            stamp: crate::freshness::BackingStamp::from_metadata(&expected_meta),
            mtime_secs: crate::freshness::BackingStamp::from_metadata(&expected_meta)
                .display_secs(),
            last_page: std::sync::Mutex::new(None),
            cache_bytes: 0,
            has_binary_tag: false,
        };

        assert!(matches!(
            validate_opened_backing(&replacement, &resolved),
            Err(CoreError::BackingChanged(_))
        ));
    }

    #[test]
    fn open_handle_reresolves_after_content_version_bump() {
        use crate::scan::scan_directory;
        use id3::TagLike;
        use std::collections::BTreeMap;

        let dir = tempfile::tempdir().unwrap();
        {
            let mut tag = id3::Tag::new();
            tag.set_artist("Pix");
            tag.set_title("Song");
            let mut bytes = Vec::new();
            tag.write_to(&mut bytes, id3::Version::Id3v24).unwrap();
            bytes.extend_from_slice(&[0xFF, 0xFB, 1, 2, 3, 4]);
            std::fs::write(dir.path().join("a.mp3"), &bytes).unwrap();
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
        let fh = fs.open_handle(file_inode).unwrap();
        let len_before = fs.read(file_inode, Some(fh), 0, 1 << 20).unwrap().len();
        assert!(len_before > 0, "baseline read must be non-empty");

        // Out-of-band re-tag: a long comment grows the synthesized ID3v2 region.
        {
            let db = musefs_db::Db::open(&db_path).unwrap();
            let track_id = db.list_tracks().unwrap().into_iter().next().unwrap().id;
            db.replace_tags(
                track_id,
                &[musefs_db::Tag::new("comment", &"x".repeat(4096), 0)],
            )
            .unwrap();
        }
        assert!(
            fs.poll_refresh().unwrap(),
            "poll_refresh must detect the change"
        );

        // Same handle: must re-resolve and serve the larger layout.
        let len_after = fs.read(file_inode, Some(fh), 0, 1 << 20).unwrap().len();
        assert!(
            len_after > len_before,
            "handle did not re-resolve: {len_before} -> {len_after}"
        );
        fs.release_handle(fh);
    }

    /// The safety property the transactional `content_version` guard exists to
    /// protect: a handle holding a `Segment::BinaryTag { payload_id }` must never
    /// serve the bytes of a *different* row that later reused that rowid under the
    /// stale layout's framing.
    ///
    /// We free the original PRIV row's rowid and reuse it with a different-length
    /// payload **without** calling `poll_refresh`, so `refresh_gen` does not move
    /// and the gen-gated re-resolve cannot mask the bug — the content_version
    /// guard is the only thing standing between the read and torn bytes. With the
    /// guard, a successful read is byte-identical to a fresh resolve of the new DB
    /// state (the guard forces a re-resolve on the version mismatch); a clean
    /// `Err` is the only other acceptable outcome. Without the guard the stale
    /// handle would serve `len_a` bytes off the reused rowid, framed by the old
    /// header — neither the original nor a valid new file.
    #[test]
    fn binary_tag_handle_never_serves_reused_rowid_bytes() {
        use crate::scan::scan_directory;
        use id3::frame::{Content, Unknown};
        use id3::{Encoder, Frame, TagLike, Version};
        use std::collections::BTreeMap;

        let needle_a = [0xDEu8, 0xAD, 0xBE, 0xEF, 0x01, 0x02];
        let needle_b = [0x11u8, 0x22, 0x33]; // different bytes AND different length

        let dir = tempfile::tempdir().unwrap();
        {
            // PRIV-only tag: text frames are omitted because the `id3` crate's
            // reader errors on a `Content::Unknown` frame it round-tripped, which
            // would drop the text tags (the raw binary walker is unaffected). The
            // track therefore renders under the `$artist/$title` fallback path.
            let mut tag = id3::Tag::new();
            tag.add_frame(Frame::with_content(
                "PRIV",
                Content::Unknown(Unknown {
                    data: needle_a.to_vec(),
                    version: Version::Id3v24,
                }),
            ));
            let mut bytes = Vec::new();
            Encoder::new()
                .version(Version::Id3v24)
                .encode(&tag, &mut bytes)
                .unwrap();
            bytes.extend_from_slice(&[0xFF, 0xFB, 0x90, 0x00, 0, 0, 0, 0]);
            std::fs::write(dir.path().join("a.mp3"), &bytes).unwrap();
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

        let artist = fs
            .lookup(VirtualTree::ROOT, "Unknown")
            .expect("fallback artist dir");
        let (_, file_inode, _) = fs.readdir(artist).unwrap().into_iter().next().unwrap();

        // Open the handle and read the original synthesized file (carries needle_a).
        let fh = fs.open_handle(file_inode).unwrap();
        let whole_a = fs.read(file_inode, Some(fh), 0, 1 << 20).unwrap();
        assert!(
            whole_a.windows(needle_a.len()).any(|w| w == needle_a),
            "baseline must carry the original PRIV body"
        );

        // Out-of-band: free the PRIV row's rowid, then reuse it with a different
        // payload. With no other tag rows present, deleting the PRIV row empties
        // `tags` and the next insert reclaims the freed rowid (plain INTEGER
        // PRIMARY KEY, no AUTOINCREMENT). Both writes bump content_version. No
        // poll_refresh, so refresh_gen stays put — only the guard can catch this.
        {
            let db = musefs_db::Db::open(&db_path).unwrap();
            let track_id = db.list_tracks().unwrap().into_iter().next().unwrap().id;
            db.set_binary_tags(track_id, &[]).unwrap();
            db.set_binary_tags(
                track_id,
                &[musefs_db::BinaryTag {
                    key: "PRIV".into(),
                    payload: needle_b.to_vec(),
                    ordinal: 0,
                }],
            )
            .unwrap();
        }

        // What a freshly resolved handle serves for the *current* DB state.
        let fh2 = fs.open_handle(file_inode).unwrap();
        let whole_b = fs.read(file_inode, Some(fh2), 0, 1 << 20).unwrap();
        fs.release_handle(fh2);
        assert!(
            whole_b.windows(needle_b.len()).any(|w| w == needle_b),
            "fresh resolve must carry the new PRIV body"
        );
        assert!(
            !whole_b.windows(needle_a.len()).any(|w| w == needle_a),
            "fresh resolve must not carry the freed payload"
        );
        assert_ne!(
            whole_a.len(),
            whole_b.len(),
            "test setup: payloads must differ in length to expose stale framing"
        );

        // The stale handle: either a clean error, or — via the guard's forced
        // re-resolve — byte-identical to the fresh resolve. Never torn bytes.
        // Err is acceptable too (the guard can surface a retryable error).
        if let Ok(bytes) = fs.read(file_inode, Some(fh), 0, 1 << 20) {
            assert_eq!(
                bytes, whole_b,
                "stale handle served torn/reused-rowid bytes instead of re-resolving"
            );
        }
        fs.release_handle(fh);
    }

    /// The per-handle fast-path read loop retries a stale binary-tag layout a
    /// bounded number of times (`0..4`) before surfacing a retryable
    /// `BackingChanged`, which the FUSE layer maps to `EIO`. A writer
    /// tight-looping commits to one track can lose the `content_version` race on
    /// every attempt; this pins the exact bound — three forced same-track misses
    /// still serve on the final attempt, a fourth exhausts the loop and errors.
    /// (#187)
    #[test]
    fn same_track_retag_storm_exhausts_read_retry_into_backing_changed() {
        use crate::scan::scan_directory;
        use id3::frame::{Content, Unknown};
        use id3::{Encoder, Frame, TagLike, Version};
        use std::collections::BTreeMap;

        let needle = [0xDEu8, 0xAD, 0xBE, 0xEF];
        let dir = tempfile::tempdir().unwrap();
        {
            // PRIV-only tag → a binary-tag layout under the fallback path, so the
            // transactional `content_version` guard (and its test seam) is live.
            let mut tag = id3::Tag::new();
            tag.add_frame(Frame::with_content(
                "PRIV",
                Content::Unknown(Unknown {
                    data: needle.to_vec(),
                    version: Version::Id3v24,
                }),
            ));
            let mut bytes = Vec::new();
            Encoder::new()
                .version(Version::Id3v24)
                .encode(&tag, &mut bytes)
                .unwrap();
            bytes.extend_from_slice(&[0xFF, 0xFB, 0x90, 0x00, 0, 0, 0, 0]);
            std::fs::write(dir.path().join("a.mp3"), &bytes).unwrap();
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

        let artist = fs
            .lookup(VirtualTree::ROOT, "Unknown")
            .expect("fallback artist dir");
        let (_, file_inode, _) = fs.readdir(artist).unwrap().into_iter().next().unwrap();
        let fh = fs.open_handle(file_inode).unwrap();

        let baseline = fs.read(file_inode, Some(fh), 0, 1 << 20).unwrap();
        assert!(
            baseline.windows(needle.len()).any(|w| w == needle),
            "baseline read must serve the binary-tag layout"
        );

        // bound-1 same-track misses: attempts retry, the final attempt serves.
        fs.force_version_mismatches_for_test(3);
        let after_three = fs
            .read(file_inode, Some(fh), 0, 1 << 20)
            .expect("three retries must still serve on the final attempt");
        assert_eq!(
            after_three, baseline,
            "bytes served after surviving the retries must match the layout"
        );

        // One miss per attempt with none left over: the loop exhausts.
        fs.force_version_mismatches_for_test(4);
        match fs.read(file_inode, Some(fh), 0, 1 << 20) {
            Err(CoreError::BackingChanged(_)) => {}
            other => panic!("exhausted retry must return BackingChanged, got {other:?}"),
        }

        // Seam drained: the handle is otherwise healthy and serves again.
        let recovered = fs.read(file_inode, Some(fh), 0, 1 << 20).unwrap();
        assert_eq!(recovered, baseline, "handle must recover after the storm");
        fs.release_handle(fh);
    }

    #[test]
    fn render_entries_returns_paths_and_snapshot() {
        use crate::scan::scan_directory;
        use id3::TagLike;

        let dir = tempfile::tempdir().unwrap();
        {
            let mut tag = id3::Tag::new();
            tag.set_artist("Pix");
            tag.set_title("Song");
            let mut bytes = Vec::new();
            tag.write_to(&mut bytes, id3::Version::Id3v24).unwrap();
            bytes.extend_from_slice(&[0xFF, 0xFB, 1, 2, 3, 4]);
            std::fs::write(dir.path().join("a.mp3"), &bytes).unwrap();
        }
        let db = musefs_db::Db::open(dir.path().join("m.db")).unwrap();
        scan_directory(&db, dir.path()).unwrap();

        let cfg = MountConfig {
            template: "$artist/$title".to_string(),
            fallbacks: BTreeMap::new(),
            default_fallback: "Unknown".to_string(),
            mode: Mode::Synthesis,
            poll_interval: std::time::Duration::ZERO,
            case_insensitive: false,
        };

        let (entries, snapshot) = Musefs::render_entries(
            &db,
            &Template::parse(&cfg.template).expect("valid template"),
            &cfg,
        )
        .unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].1, "Pix/Song.mp3");
        let id = entries[0].0;
        assert_eq!(snapshot[&id].path, "Pix/Song.mp3");
        assert!(snapshot[&id].content_version >= 1);
    }

    #[test]
    fn needs_rebuild_flag_forces_full_rebuild_on_next_poll() {
        use crate::scan::scan_directory;
        use id3::TagLike;
        use std::collections::BTreeMap;

        let dir = tempfile::tempdir().unwrap();
        {
            let mut tag = id3::Tag::new();
            tag.set_artist("Pix");
            tag.set_title("Song");
            let mut bytes = Vec::new();
            tag.write_to(&mut bytes, id3::Version::Id3v24).unwrap();
            bytes.extend_from_slice(&[0xFF, 0xFB, 1, 2, 3, 4]);
            std::fs::write(dir.path().join("a.mp3"), &bytes).unwrap();
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

        // data_version is unchanged since open, so a normal poll is a no-op.
        assert!(!fs.poll_refresh().unwrap(), "baseline poll must be a no-op");

        // Advance data_version out-of-band so the forced rebuild has newer DB state
        // to incorporate and stamp; the trailing normal poll then proves it stamped.
        {
            let db = musefs_db::Db::open(&db_path).unwrap();
            let track_id = db.list_tracks().unwrap().into_iter().next().unwrap().id;
            db.replace_tags(track_id, &[musefs_db::Tag::new("comment", "hi", 0)])
                .unwrap();
        }

        // Simulate recovery from a poisoned VFS-state lock.
        fs.mark_needs_rebuild_for_test();
        assert!(
            fs.needs_rebuild_is_set_for_test(),
            "flag reads set after marking"
        );
        assert!(
            fs.poll_refresh().unwrap(),
            "a set needs_rebuild flag must force a rebuild"
        );
        assert!(
            !fs.needs_rebuild_is_set_for_test(),
            "flag cleared after rebuild"
        );

        // The forced rebuild incorporated the out-of-band write and stamped its
        // data_version, so a subsequent normal poll detects no change.
        assert!(
            !fs.poll_refresh().unwrap(),
            "forced rebuild must stamp data_version (next poll is a no-op)"
        );
    }

    fn fs_with_poll_interval(interval: std::time::Duration) -> (tempfile::TempDir, Musefs) {
        let dir = tempfile::tempdir().unwrap();
        let cfg = MountConfig {
            template: "$artist/$title".to_string(),
            fallbacks: BTreeMap::new(),
            default_fallback: "Unknown".to_string(),
            mode: Mode::Synthesis,
            poll_interval: interval,
            case_insensitive: false,
        };
        let fs = Musefs::open(musefs_db::Db::open(dir.path().join("m.db")).unwrap(), cfg).unwrap();
        (dir, fs)
    }

    #[test]
    fn poll_due_false_within_interval_true_after_expiry() {
        let (_d, fs) = fs_with_poll_interval(std::time::Duration::from_hours(1));
        assert!(!fs.poll_due(), "fresh open is within the debounce window");
        fs.expire_poll_debounce_for_test();
        assert!(fs.poll_due(), "past the debounce window");
    }

    #[test]
    fn poll_due_true_when_needs_rebuild_regardless_of_interval() {
        let (_d, fs) = fs_with_poll_interval(std::time::Duration::from_hours(1));
        assert!(!fs.poll_due());
        fs.mark_needs_rebuild_for_test();
        assert!(fs.poll_due(), "needs_rebuild bypasses the debounce");
    }

    #[test]
    fn poll_due_true_when_interval_zero() {
        let (_d, fs) = fs_with_poll_interval(std::time::Duration::ZERO);
        assert!(fs.poll_due(), "zero interval disables the debounce");
    }

    #[test]
    fn poll_due_respects_failure_backoff_window() {
        let (_d, fs) = fs_with_poll_interval(std::time::Duration::from_hours(1));
        fs.expire_poll_debounce_for_test(); // get past the debounce gate first
        fs.fail_refresh_now_for_test();
        assert!(!fs.poll_due(), "inside the retry backoff window");
        fs.expire_refresh_backoff_for_test();
        assert!(fs.poll_due(), "past the retry backoff window");
    }

    #[test]
    fn passthrough_fd_exposes_backing_only_in_structure_only() {
        use crate::scan::scan_directory;
        use id3::TagLike;
        use std::collections::BTreeMap;
        use std::os::fd::AsFd;
        use std::os::unix::fs::MetadataExt;

        let dir = tempfile::tempdir().unwrap();
        {
            let mut tag = id3::Tag::new();
            tag.set_artist("Pix");
            tag.set_title("Song");
            let mut bytes = Vec::new();
            tag.write_to(&mut bytes, id3::Version::Id3v24).unwrap();
            bytes.extend_from_slice(&[0xFF, 0xFB, 1, 2, 3, 4]);
            std::fs::write(dir.path().join("a.mp3"), &bytes).unwrap();
        }
        let db_path = dir.path().join("m.db");
        {
            let db = musefs_db::Db::open(&db_path).unwrap();
            scan_directory(&db, dir.path()).unwrap();
        }
        let cfg = |mode| MountConfig {
            template: "$artist/$title".to_string(),
            fallbacks: BTreeMap::new(),
            default_fallback: "Unknown".to_string(),
            mode,
            poll_interval: std::time::Duration::ZERO,
            case_insensitive: false,
        };

        // StructureOnly: exposed, and the fd refers to the backing inode.
        let fs = Musefs::open(
            musefs_db::Db::open(&db_path).unwrap(),
            cfg(Mode::StructureOnly),
        )
        .unwrap();
        let artist = fs.lookup(VirtualTree::ROOT, "Pix").expect("artist dir");
        let (_, file_inode, _) = fs.readdir(artist).unwrap().into_iter().next().unwrap();
        let fh = fs.open_handle(file_inode).unwrap();
        let pfd = fs
            .passthrough_fd(fh)
            .expect("StructureOnly exposes the backing fd");
        let fd_meta = std::fs::File::from(pfd.as_fd().try_clone_to_owned().unwrap())
            .metadata()
            .unwrap();
        let backing_meta = std::fs::metadata(dir.path().join("a.mp3")).unwrap();
        assert_eq!(
            (fd_meta.dev(), fd_meta.ino()),
            (backing_meta.dev(), backing_meta.ino()),
            "passthrough fd must be the backing file"
        );

        // A released handle no longer resolves.
        fs.release_handle(fh);
        assert!(fs.passthrough_fd(fh).is_none());

        // Synthesis: never exposed, even for a live handle.
        let fs =
            Musefs::open(musefs_db::Db::open(&db_path).unwrap(), cfg(Mode::Synthesis)).unwrap();
        let artist = fs.lookup(VirtualTree::ROOT, "Pix").expect("artist dir");
        let (_, file_inode, _) = fs.readdir(artist).unwrap().into_iter().next().unwrap();
        let fh = fs.open_handle(file_inode).unwrap();
        assert!(fs.passthrough_fd(fh).is_none());
    }

    #[test]
    fn order_entries_sorts_ascending_by_id() {
        // A real Db never hands render_entries id-unordered rows (list_tracks is
        // ORDER BY id), so this descending input is constructed directly to pin
        // the sort itself. Deleting/mutating order_entries' sort fails this test.
        let unordered = vec![
            (9_i64, "z.flac".to_string()),
            (2_i64, "a.flac".to_string()),
            (5_i64, "m.flac".to_string()),
        ];
        let ordered = Musefs::order_entries(unordered);
        let ids: Vec<i64> = ordered.iter().map(|(id, _)| *id).collect();
        assert_eq!(
            ids,
            vec![2, 5, 9],
            "order_entries must sort ascending by id"
        );
        // The pairing is preserved, not just the id column.
        assert_eq!(
            ordered,
            vec![
                (2_i64, "a.flac".to_string()),
                (5_i64, "m.flac".to_string()),
                (9_i64, "z.flac".to_string()),
            ]
        );
    }

    #[test]
    fn full_rebuild_gives_bare_colliding_name_to_lower_id() {
        use musefs_db::{Format, NewTrack, Tag};
        use std::collections::BTreeMap;

        let db = musefs_db::Db::open_in_memory().unwrap();
        // Two tracks whose `$title` both render to "Same" -> colliding "Same.flac".
        // Insertion order fixes ascending ids: id_a < id_b.
        let id_a = db
            .upsert_track(&NewTrack {
                backing_path: "/a.flac".into(),
                format: Format::Flac,
                audio_offset: 0,
                audio_length: 1,
                backing_size: 1,
                backing_mtime_ns: 0,
                backing_ctime_ns: 0,
            })
            .unwrap();
        let id_b = db
            .upsert_track(&NewTrack {
                backing_path: "/b.flac".into(),
                format: Format::Flac,
                audio_offset: 0,
                audio_length: 1,
                backing_size: 1,
                backing_mtime_ns: 0,
                backing_ctime_ns: 0,
            })
            .unwrap();
        assert!(id_a < id_b, "insertion assigns ascending ids");
        db.replace_tags(id_a, &[Tag::new("title", "Same", 0)])
            .unwrap();
        db.replace_tags(id_b, &[Tag::new("title", "Same", 0)])
            .unwrap();

        let config = MountConfig {
            template: "$title".to_string(),
            fallbacks: BTreeMap::new(),
            default_fallback: "Unknown".to_string(),
            mode: Mode::Synthesis,
            poll_interval: std::time::Duration::ZERO,
            case_insensitive: false,
        };
        let template = Template::parse(&config.template).expect("valid template");

        let mut alloc = InodeAllocator::new();
        let (tree, _snapshot) = Musefs::build_full(&db, &template, &config, &mut alloc).unwrap();

        let root = VirtualTree::ROOT;
        let bare = tree.lookup(root, "Same.flac").expect("bare name exists");
        let suffixed = tree
            .lookup(root, "Same (2).flac")
            .expect("suffixed name exists");
        // The LOWER id owns the bare name; the higher id is disambiguated. This
        // matches the incremental path's min-id rule (tree.rs introducing_id).
        assert_eq!(tree.inode_of_track(id_a), Some(bare));
        assert_eq!(tree.inode_of_track(id_b), Some(suffixed));
    }

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

    #[test]
    fn open_rejects_template_with_control_byte() {
        let db = musefs_db::Db::open_in_memory().unwrap();
        let config = MountConfig {
            template: "a\0b/$title".to_string(),
            fallbacks: std::collections::BTreeMap::new(),
            default_fallback: "Unknown".to_string(),
            mode: Mode::Synthesis,
            poll_interval: std::time::Duration::ZERO,
            case_insensitive: false,
        };
        assert!(matches!(
            Musefs::open(db, config),
            Err(crate::CoreError::InvalidTemplate(_))
        ));
    }
}
