use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use musefs_db::{Db, Format};

use crate::error::{CoreError, Result};
use crate::mapping::tags_to_fields;
use crate::refresh_diff::{ChangeSet, TrackRenderState, partition_changelog};
use crate::template::Template;
use crate::tree::{InodeAllocator, VirtualTree};

use super::{MountConfig, Musefs};

/// Resets a single-flight flag on drop, so a panic (or early return) during a
/// rebuild can't leave `refreshing` stuck `true` and permanently disable refresh.
struct RefreshGuard<'a>(&'a AtomicBool);

impl Drop for RefreshGuard<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

pub(crate) fn retry_backoff_for(poll_interval: std::time::Duration) -> std::time::Duration {
    if poll_interval.is_zero() {
        std::time::Duration::ZERO
    } else {
        poll_interval
            .min(std::time::Duration::from_secs(1))
            .max(std::time::Duration::from_millis(100))
    }
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
    /// Render a single track's path from its tags + format. The one place
    /// `Template::render` is called, shared by full and incremental rebuilds.
    /// Returns `None` when `skip_on_missing` is set and a top-level template
    /// field is unresolved — the caller drops the track from the mount.
    fn render_one(
        template: &Template,
        config: &MountConfig,
        format: musefs_db::Format,
        tags: &[musefs_db::Tag],
    ) -> Option<String> {
        let fields = tags_to_fields(tags);
        if config.skip_on_missing {
            template.render_checked(&fields, &config.fallbacks, format.as_str())
        } else {
            Some(template.render(
                &fields,
                &config.fallbacks,
                &config.default_fallback,
                format.as_str(),
            ))
        }
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
    ///
    /// Tracks that `render_one` drops (`skip_on_missing` + an unresolved top-level
    /// field) enter neither `entries` nor the snapshot, so they never materialize.
    #[allow(clippy::type_complexity)]
    pub(crate) fn render_entries<M>(
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
            let Some(path) = Self::render_one(template, config, t.format, &tags) else {
                continue;
            };
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
    pub(crate) fn order_entries(mut entries: Vec<(i64, String)>) -> Vec<(i64, String)> {
        entries.sort_by_key(|(id, _)| *id);
        entries
    }

    /// Full rebuild: render every track and build the tree from scratch. Used by
    /// `open`, forced `refresh`, and the Stage B fallback. Returns the tree and the
    /// fresh `track_id -> TrackRenderState` snapshot.
    pub(crate) fn build_full<M>(
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
    pub(crate) fn force_full_rebuild(&self, on_changed: &mut impl FnMut(u64)) -> Result<bool> {
        // Read data_version before rebuilding so a successful self-heal also advances
        // the poll stamp: a write that commits mid-rebuild then leaves a newer version
        // for the next poll (one extra rebuild, never a skipped change), rather than
        // forcing an unconditional rebuild on every subsequent poll.
        let version = self.pool.with_poll(|db| Ok(db.data_version()?))?;
        let new_seq = self
            .pool
            .with_poll(|db| Ok(db.changelog_since(i64::MAX)?.max_seq))?;
        // Consume the rebuild request before touching VFS state: a re-poison that
        // re-raises `needs_rebuild` while the rebuild below runs then survives for
        // the next poll instead of being clobbered by a trailing unconditional clear
        // (#369). On success we never clear again, so a concurrent re-raise persists.
        let was_set = self.needs_rebuild.swap(false, Ordering::AcqRel);
        let old_tree = self.tree.load_full();
        let old_snapshot =
            crate::lock::lock_or_flag(&self.snapshot, &self.needs_rebuild, "snapshot").clone();
        let new_snapshot = match self.rebuild_full() {
            Ok(v) => v,
            Err(err) => {
                // Rebuild failed: re-arm the request we consumed so the next poll
                // retries. Only restore when we were the one that cleared it — a
                // concurrent re-raise has already set the flag and must not be undone.
                if was_set {
                    self.needs_rebuild.store(true, Ordering::Release);
                }
                return Err(err);
            }
        };
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
        let mut change = partition_changelog(&prev_states, &log.changed_ids, &keys);

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
                .filter_map(|&id| {
                    let (cv, fmt) = key_of[&id];
                    let tags = tags_by_track.remove(&id).unwrap_or_default();
                    Self::render_one(&self.template, &self.config, fmt, &tags).map(|path| {
                        (
                            id,
                            TrackRenderState {
                                content_version: cv,
                                format: fmt,
                                path,
                            },
                        )
                    })
                })
                .collect()
        };

        // Reconcile `skip_on_missing` drops: a `changed`/`added` id that rendered
        // nothing (`render_one` -> None) produced no `new_states` entry. A changed
        // id was materialized before, so it becomes a removal; an added id never
        // was, so it just disappears from the change set. Keeps the change set and
        // `new_states` agreeing for `apply_changes` and the snapshot mutation below.
        let mut vanished = Vec::new();
        change.changed.retain(|id| {
            if new_states.contains_key(id) {
                true
            } else {
                vanished.push(*id);
                false
            }
        });
        change.added.retain(|id| new_states.contains_key(id));
        change.removed.extend(vanished);

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

    /// The shared debounce/backoff gate: `true` when a `data_version` poll
    /// should be skipped because the poll interval hasn't elapsed since the last
    /// poll, or the retry backoff hasn't elapsed since the last failed refresh.
    /// The single source for both `poll_due` (the advisory dispatch-thread
    /// pre-check) and `poll_refresh_notify` (the authoritative gate), so the two
    /// can't drift out of sync (#89). Advisory-cheap: lock + `Instant::elapsed`,
    /// no DB access.
    fn poll_debounced(&self) -> bool {
        if !self.poll_interval.is_zero()
            && crate::lock::lock_recover(&self.last_poll, "last_poll").elapsed()
                < self.poll_interval
        {
            return true;
        }
        if let Some(last_failed) =
            *crate::lock::lock_recover(&self.last_failed_refresh, "last_failed_refresh")
            && last_failed.elapsed() < self.refresh_retry_backoff
        {
            return true;
        }
        false
    }

    /// Cheap, synchronous "is a `data_version` poll worth dispatching?" predicate
    /// for the FUSE dispatch thread to gate `fire_poll_refresh` on, so a
    /// metadata-op storm doesn't flood the worker pool with no-op poll tasks (#89).
    /// Shares the `poll_debounced` gate with `poll_refresh_notify`. Advisory only:
    /// no DB access, no `data_version` read, no rebuild. A stale `true` costs at
    /// most one task the inner gate short-circuits, and `needs_rebuild` is checked
    /// first so a self-heal is never debounced away.
    pub fn poll_due(&self) -> bool {
        if self.needs_rebuild.load(Ordering::Acquire) {
            return true;
        }
        !self.poll_debounced()
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
        // The debounce / backoff gate is shared with the cheap `poll_due`
        // pre-check the FUSE layer runs on the dispatch thread (#89).
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

        if self.poll_debounced() {
            return Ok(false);
        }
        // Stamp the failure on a broken data_version read too: it propagates before
        // the `refreshing` CAS, so without this stamp a persistently broken poll
        // connection re-dispatches a fast-failing poll on every metadata op, never
        // arming the backoff the rebuild-error paths below rely on (#369).
        let version_read = if self.force_poll_read_error.load(Ordering::Acquire) {
            Err(CoreError::BackingChanged(
                "forced poll-read failure".to_string(),
            ))
        } else {
            self.pool.with_poll(|db| Ok(db.data_version()?))
        };
        let version = match version_read {
            Ok(v) => v,
            Err(err) => {
                *crate::lock::lock_recover(&self.last_failed_refresh, "last_failed_refresh") =
                    Some(std::time::Instant::now());
                return Err(err);
            }
        };
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
    pub fn force_poll_read_errors_for_test(&self, fail: bool) {
        self.force_poll_read_error.store(fail, Ordering::Release);
    }

    #[doc(hidden)]
    pub fn force_apply_failure_for_test(&self, on: bool) {
        self.force_apply_fail.store(on, Ordering::Release);
    }

    /// Force the next `count` binary-tag `content_version` guard checks in
    /// `read_into` to report a stale layout, as if a writer re-tagged this track
    /// between every retry. Used to exercise the retry-exhaustion bound.
    #[cfg(test)]
    pub(crate) fn force_version_mismatches_for_test(&self, count: u64) {
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
}
