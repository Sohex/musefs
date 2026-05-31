# SP2 — Incremental tree refresh — design

*Date: 2026-05-31 · Part of the [2026-05-30 optimization pass](./README.md)*

## Goal

Make `Musefs::poll_refresh` cost scale with the **number of changed tracks**, not
with library size. Today a single `PRAGMA data_version` bump triggers a **full**
rebuild — `db.list_tracks()` (all rows) + `db.tags_grouped()` (all tag rows) +
`render_path` per track + a from-scratch `VirtualTree::build_with` — regardless of
whether one track or zero tracks actually changed (`facade.rs:164-186`,
`facade.rs:248-359`). At 100k tracks a metadata-edit storm makes this the
dominant refresh cost; SP2 turns it into O(changed).

**Hard constraint: zero observable behavior change.** The refreshed `VirtualTree`
must be *structurally identical* to a full `build_with` over the same DB state —
same rendered paths, same track→inode mapping, same inode assignments. This is the
SP2 analog of SP1's "bounded probe ≡ full probe" equivalence and is the headline
correctness gate.

## Cardinal invariant (preserved structurally)

SP2 changes only *how the virtual tree is assembled on a refresh*. It does not
touch the serve/read path, synthesis, the DB schema, or what gets written to the
DB. The byte-identical-audio guarantee therefore holds by construction —
provided the incrementally-assembled tree equals the full-rebuild tree (the
equivalence gate below). Inode stability is likewise preserved: SP2 reuses the
existing persistent `InodeAllocator`, so an inode changes **only** when a track's
disambiguated path genuinely changes — which is exactly the set that already
churns under today's full rebuild (today re-runs `disambiguate` over all siblings
every refresh).

## Reuse (existing foundation)

- **`Musefs::poll_refresh_notify`** (`facade.rs:260`) — already polls
  `PRAGMA data_version`, debounces (`poll_interval`), single-flights via the
  `refreshing` CAS, and maintains a persisted `versions: HashMap<i64,i64>`
  (track_id → content_version) snapshot that it diffs old-vs-new to drive the
  `on_changed(inode)` cache-invalidation callbacks (`facade.rs:322-348`). SP2
  reuses this skeleton verbatim — the *signal* for "which tracks changed" is
  already half-built; SP2 makes the *work* scale to it.
- **`versions` snapshot** (`facade.rs`, the `track_id → content_version` map) — the
  basis for change detection. SP2 widens it to a `track_id → TrackRenderState
  { content_version, format, path }` snapshot so the diff keys on the full render
  key and unchanged tracks reuse their cached path (Component 2).
- **`VirtualTree::build_with(entries, alloc)`** (`tree.rs:63`) and the persistent
  `InodeAllocator` (`tree.rs:8`, held in `Musefs::inodes`) — Stage A calls
  `build_with` unchanged; Stage B adds mutation methods beside it.
- **`HeaderCache::retain`** (`reader.rs`) + `size_cache` pruning
  (`facade.rs:318-320`) — pruning continues to run; SP2 feeds it the
  already-computed live/removed sets instead of re-deriving them.
- **`bench_refresh`** (`musefs-core/tests/bench_refresh.rs`) — the SP2 measurement
  harness already times refresh-1 vs refresh-N; extended, not replaced.
- **`tree.rs` determinism tests** (`build_with_keeps_inodes_stable…`,
  `disambiguate_*`) — Stage A leaves them untouched (it is a full `build_with`);
  Stage B adds the property-test oracle alongside them.

## Today's costs, ranked (what SP2 eliminates)

Per refresh, dominant → negligible:

1. **`tags_grouped()`** — every tag row in the DB (typically 10–20× the track
   count) streamed through SQLite and grouped into a HashMap. **Heaviest.**
2. **`render_path` per track** — template parse + field substitution × N
   (`facade.rs:177-183`). **Heavy.**
3. **`VirtualTree::build_with`** — N path-splits + HashMap inserts
   (`nodes`/`children`/`track_to_inode`) + per-dir disambiguation, all in-memory
   (`tree.rs:120-145`). **Light** — low-tens-of-ms even at 100k, no I/O.

Costs 1–2 are O(library) SQLite + parsing; cost 3 is cheap in-memory work. This
ranking is *the* rationale for the staged plan: Stage A removes 1 and 2 (the bulk
of the win) while keeping 3; Stage B then removes 3 so refresh is strictly
O(changed). Both stages ship as part of SP2; the A→B boundary is a verification
checkpoint (A's equivalence + regression gates green before B is layered on), not
a go/no-go on B.

## Component 1 — Change detection (in-memory identity diff)

On a `data_version` bump (inside the existing single-flight guard), run a
**lightweight identity scan** — a new `Db::list_render_keys()` returning
`Vec<(i64, i64, String)>` from `SELECT id, content_version, format FROM tracks
ORDER BY id`. The per-track **render key** is `(content_version, format)`: these
are exactly the two track-level inputs that determine a rendered path. Everything
else `render_path` consumes is either per-mount-fixed (`template`, `fallbacks`,
`default_fallback`) or derived from tags — and `content_version` is the
trigger-maintained counter that rises on **any tag or art write** (schema.rs
triggers on `tags`/`track_art` only — there is no trigger on the `tracks` table),
while `format` is the one `tracks`-column that feeds the path (it is the file
extension, `facade.rs:184` `t.format.as_str()`). Diff the scan against the
persisted render-key snapshot to partition tracks into three sets in one pass:

- **`changed`** — id present in both, render key `(content_version, format)` differs.
- **`added`** — id present now, absent from the snapshot.
- **`removed`** — id in the snapshot, absent now.

**Edit-kind coverage (precise, not "every edit kind").** This detects exactly the
edits that change a rendered path or a content version: tag writes, art writes,
and `format` changes (all covered by the render key). A *direct* `tracks`-table
edit to a non-render column (`audio_offset`/`audio_length`/`backing_size`/
`backing_mtime`) bumps `data_version` (so the poll fires) but **not** the render
key, so it yields an **empty partition and a no-op refresh** — which is observably
identical to today: a full rebuild over such an edit reproduces the *same* tree
(`render_path` ignores those columns) and the *same* `content_version` snapshot,
so today's rebuild is itself a no-op-shaped full rebuild. The serve-path caches
(`HeaderCache`/`size_cache`) remain `content_version`-keyed exactly as today, so
SP2 introduces **no** caching regression for those columns (their lazy
re-validation on `resolve`, including the `backing_size`/`mtime` `BackingChanged`
check, is unchanged). The honest scope: SP2's incremental path observes render-key
changes; non-render `tracks`-column edits are a no-op for the tree, as they
effectively are today.

The scan is O(N) but cheap (an estimated low-tens-of-ms at ~1M rows — two ints +
a short string per row, ordered by the primary key; confirmed by a `bench_refresh`
row, not assumed); the *expensive* work downstream is O(|changed ∪ added|). No
schema change and no index are required, and deletions fall out of the same pass
(a `WHERE`-filtered query could not see them — see Out of scope).

## Component 2 — Changed-only render (Stage A)

The persisted state on `Musefs` grows so unchanged tracks need no re-render. The
existing `versions: HashMap<i64,i64>` (track_id → content_version) is widened to a
**render-key + path snapshot**:

- `snapshot: HashMap<i64, TrackRenderState>` (**replaces `versions`**), where
  `TrackRenderState { content_version: i64, format: String, path: String }`. The
  `(content_version, format)` pair is the change-detection render key (Component
  1); `path` is the last rendered path, reused verbatim for unchanged tracks.

On a refresh:

1. For **`changed ∪ added`**: load their tags via a new
   `Db::tags_for_tracks(&[i64]) -> HashMap<i64, Vec<Tag>>` whose SQL is
   `... WHERE track_id IN (…) ORDER BY track_id, key, ordinal` — the **same
   ordering as `tags_grouped`** (tags.rs:41), which is load-bearing: `tags_to_fields`
   takes the lowest-`ordinal` value per key (mapping.rs:17-26), so a different row
   order would silently change a multi-value key's rendered value and break
   equivalence. The `IN (…)` list is **chunked** to stay under SQLite's
   `SQLITE_MAX_VARIABLE_NUMBER`. Then `render_path` each and update its
   `snapshot` entry (new `content_version`, `format`, `path`).
2. For **`removed`**: drop from `snapshot`.
3. For **unchanged**: reuse the cached `snapshot[id].path` — no tag load, no render.
4. Assemble the full `entries: Vec<(i64, String)>` from the snapshot's `path`
   fields and call the existing `VirtualTree::build_with(&entries, &mut alloc)`.

Because step 4 is an unmodified full `build_with`, the structure and
disambiguation are **identical to today by construction** — Stage A's equivalence
is trivial and the existing determinism tests need no change. The win is that
costs 1 and 2 (all-tags load + all-render) become O(changed), leaving only the
cheap in-memory cost 3.

**Forced full rebuild (`refresh()`) must reset the snapshot.** `Musefs::refresh()`
(facade.rs:197-204) is a non-single-flighted forced full rebuild that overwrites
`versions` today; under SP2 it must rebuild the **whole** `snapshot` (render every
track, populate `content_version`/`format`/`path`) so the next incremental
`poll_refresh` diffs against a consistent baseline rather than a stale one.

**Cache invalidation reuse.** The `changed` / `removed` / path-moved sets computed
here directly feed the existing `HeaderCache::retain` + `size_cache` pruning and
the `on_changed(inode)` callbacks, replacing today's separate old-vs-new re-diff
(`facade.rs:322-348`) with the partition we already have. **`on_changed` fires on
any path move, decoupled from `content_version`.** Today's invalidation loops key
on a `content_version` difference (`facade.rs:323-324, 338`), but a **`format`-only
change** moves the file (new extension → new path) **without** bumping
`content_version` (no `tracks` trigger). Since SP2 now treats `format` as a
first-class change signal (the render key), the path-move invalidation must fire
on the move itself, regardless of `content_version` — otherwise a `--keep-cache`
mount would serve the old extension's stale cached header bytes after a format
change. **Old-inode capture:** today the removed/path-moved `on_changed` inode is
looked up on the *old* tree (facade.rs:335, 340). Under Stage B there is no old
tree, so the mutation must capture each removed/moved node's inode (via
`track_to_inode` / the node's path) **before** unlinking it, and feed those
captured inodes to `on_changed`.

## Component 3 — In-place tree mutation (Stage B)

Stage B makes refresh strictly O(changed) by replacing the from-scratch
`build_with` (cost 3) with in-place mutation of the persistent `VirtualTree`. The
mutation must yield a tree byte-identical to a full `build_with`, so it is defined
as a **precise ordered algorithm**, not "mutate and hope."

### Canonical order (the equivalence anchor)

A full `build_with` processes entries in `list_tracks` order = **ascending track
id**, lazily `ensure_dir`-ing each path's directories then `disambiguate`-ing the
leaf. Two consequences the incremental path must reproduce exactly:

- A directory child's disambiguated **name is decided by the lowest track id that
  introduces it** — for a file child that is the file's own track id; for a
  subdirectory child it is the minimum track id among all tracks routed through
  it. Call this the child's **introducing id**. An introducing id is a pure
  function of the child's *current* routed track set, so **it can change on any
  edit**: removing the lowest-id track under a subdirectory raises that subdir's
  introducing id, which can re-order it against a colliding sibling and **flip
  which one claims the base name** — a swap that cascades to every descendant
  inode. This is why re-disambiguation must recompute introducing ids, not reuse
  stale ones (see the algorithm's bottom-up step).
- `ensure_dir` reuses an existing same-named child **only if it is a directory**
  (tree.rs:154-158); a same-named *file* child does not block a directory, so the
  directory is disambiguated to ` (k)` instead — an asymmetry the recomputation
  must honor (the **dir-vs-file collision** case).

The **canonical order for a directory's children** is therefore ascending
introducing id; `disambiguate` is replayed over the children in that order, the
first claimant taking the base name and later collisions taking ` (2)`, ` (3)`, …

### Per-refresh mutation algorithm (strict ordering)

Compute the path delta first: for each `changed` track re-render its path; classify
each affected track as remove-only (`removed`), add-only (`added`), unchanged-path
(content/version change with identical path → **no structural mutation**, cache
invalidation only), or path-moved (remove-old + add-new). Then apply, in this
order:

1. **Unlink** every removed and old-side-of-move file node from its parent's
   `children`; drop it from `nodes` and `track_to_inode`. Record its inode for
   `on_changed` *before* dropping (see Component 2 old-inode capture).
2. **Prune empty ancestors, bottom-up:** for each directory that lost a child,
   walk upward removing any directory with an empty `children` set (a full rebuild
   never materializes a childless directory).
3. **Insert** every added and new-side-of-move file node: `ensure_dir` down its
   path (interning ancestor dirs as needed) and place the leaf.
4. **Recompute introducing ids bottom-up**, then **re-disambiguate every directory
   whose child set changed, top-down.** Two sub-points are equivalence-critical:
   - **Recompute, don't reuse.** Because a removal/add can change a subdir's
     introducing id (Canonical order, above), the affected subtrees' introducing
     ids must be re-derived bottom-up *before* the top-down pass — a sibling-subdir
     name swap (and its descendant inode swaps) is in scope, exactly as a full
     rebuild would produce it.
   - **Clear-then-reinsert, not in-place `disambiguate`.** The existing
     `disambiguate(dir, name)` (tree.rs:179) reads the *live* children map and only
     ever appends the next free ` (k)`; called over an already-bound map it can
     never *reclaim* a freed base name. So re-disambiguating a directory means
     **clearing its child name-bindings and re-interning the children one by one in
     ascending introducing-id order**, re-deriving each name — mirroring a fresh
     subtree build. This is what promotes `song (2).flac` back to `song.flac` after
     the base-name holder is deleted, and resolves dir-vs-file collisions
     identically to a full rebuild.

   Renames here retire the old disambiguated path's inode and intern the new path
   against the **same persistent `InodeAllocator`** (so an unchanged path keeps its
   inode and a genuinely-changed one churns exactly as a full rebuild does today),
   firing `on_changed` for affected inodes.

The affected set in steps 2–4 is bounded by the directories touched plus their
colliding groups and those groups' descendants — small, because collisions are
localized (duplicate editions, multi-disc albums, fallback pile-ups), never
O(library).

### Correctness posture (fallback safety valve)

Stage B is intended correct-by-construction and is gated by the full-rebuild
property-test oracle (Testing item 1). Because a wrong tree would serve the wrong
file's bytes, it also carries a **production safety valve**: if the mutation hits
an internal invariant violation it cannot resolve (e.g. `disambiguate` finds no
free name, an expected node is missing, or an inode-allocation inconsistency), it
**falls back to a full `VirtualTree::build_with`** for that refresh and logs the
fallback. Tests additionally run a `debug_assert`-level full-equivalence check
after each incremental refresh; production does **not** pay the O(N) equivalence
check on the hot path. **The oracle's reference tree must be built with a *clone of
the live `InodeAllocator`*, not a fresh one** — inodes are keyed on accumulated
allocator history (monotonic, never recycled), so the equivalence target is
`build_with(current_entries, allocator.clone())`, which interns existing paths to
their existing inodes and only fresh paths to new ones. Comparing against a
from-zero allocator would mismatch inode numbers and make the assert meaningless.
The disambiguation/equivalence risk is the entire reason for both the oracle and
the fallback.

## Component 4 — Measurement

- **`bench_refresh`** today times refresh-1 vs refresh-N on a single `fs` at one
  corpus size (FLAC-only), asserting only that a rebuild happened. SP0's baseline
  shows refresh-1 ≈ refresh-N (full rebuild regardless of change count). SP2's
  signal: **refresh-N scales with the changed set; refresh-1 is near-constant and
  independent of total library size.** Showing "independent of N_total" requires a
  **new sweep dimension — library size** (the same fixed single-track touch run
  against several corpus sizes, comparing refresh-1 wall time across them), which
  is a structural change to the harness (an outer size loop, each with its own
  `prepare`/scan), not merely a wider parameter.
- **Stateful-snapshot note:** the existing harness times refresh-1 then refresh-N
  on the *same* `fs` sequentially. Once refreshes are incremental, the
  `snapshot` left by the first call is the diff baseline for the second; the bench
  must account for that ordering (e.g. measure refresh-1 and refresh-N from
  comparable warm states) so the two numbers stay interpretable.
- Stage A and Stage B numbers are recorded separately so the contribution of the
  changed-only render (A) and the in-place mutation (B) are each visible.
- The O(N) identity-scan cost (Component 1) gets its own row so the
  "low-tens-of-ms at scale" estimate is measured, not assumed.
- Before/after numbers land in the umbrella [results log](./README.md#results-log)
  and the repo-root [`BENCHMARKS.md`](../../../../BENCHMARKS.md) (recording
  optimization-pass baselines there is the established convention).

## Testing & acceptance gate

1. **Full-rebuild equivalence (headline).** Over random edit sequences on a
   generated corpus, the incrementally-refreshed `VirtualTree` is **structurally
   identical** to a fresh `VirtualTree::build_with` over the same DB state:
   identical `(track_id → rendered path)` mapping, identical directory structure,
   and identical inode assignments for every surviving path. The generator must
   exercise: tag add/change/delete; art add/delete; **`format` change** (changes
   the rendered extension → a path move with no tag change); path-move via a
   template-field tag edit; a **multi-value-key track** (multiple values for one
   tag key, lowest ordinal wins — guards the `tags_for_tracks` ordering); collision
   groups (duplicate renders) including the **dir-name-vs-file-name collision**
   through `ensure_dir` and the **sibling-subdirectory introducing-id reorder**
   (delete the lowest-id track under one of two colliding subdirs so the base-name
   claim swaps and descendant inodes swap with it — the cascade most likely to
   diverge); and deletions that empty a directory. For Stage A this is
   near-trivial (it *is* a full `build_with`); for Stage B it is the core property
   test and the analog of SP1's bounded≡full guard.
2. **Change-detection units.** The `list_render_keys` diff yields the correct
   `changed`/`added`/`removed` partition for the render-key-affecting edits (tag
   write, art write, `format` change), and yields an **empty partition** for a
   direct `tracks`-table edit to a non-render column
   (`audio_offset`/`audio_length`/`backing_size`/`backing_mtime`) — verified to be
   a no-op refresh whose resulting tree equals a full rebuild's (the documented
   scope boundary). Also: a content-version bump that leaves the rendered path
   unchanged drives cache invalidation but no tree mutation.
3. **Path-move correctness.** A tag edit that changes a rendered template field
   moves the file to the new path, retires the old inode (degrades to `ENOENT`),
   allocates-or-reuses the new path's inode per the allocator, and fires
   `on_changed` for the old inode. **Format-only move:** a `format` change (new
   extension, **no** `content_version` bump) likewise moves the file and fires
   `on_changed`, proving invalidation is decoupled from `content_version` (the
   `--keep-cache` stale-bytes guard).
4. **Disambiguation reclamation + dir-vs-file (Stage B).** Deleting the track that
   holds the base name `song.flac` promotes the lowest-introducing-id
   `song (k).flac` sibling back to `song.flac`, matching a full rebuild exactly;
   adding a colliding track assigns the next free `(k)` in canonical
   (introducing-id) order. Separately, a file and a subdirectory that render to the
   same name resolve with the `ensure_dir` asymmetry (the lower introducing id
   wins the base name; a same-named file does not block the directory) — identical
   to a full rebuild.
5. **Empty-dir pruning + fallback (Stage B).** Removing the last file in a
   directory prunes the now-empty directory (and its now-empty ancestors), matching
   a full rebuild. **Fallback:** an injected mutation-invariant violation triggers
   the full-`build_with` safety valve and the refresh still yields the correct,
   equivalence-passing tree (logged as a fallback).
6. **Cache + notify integration.** `HeaderCache`/`size_cache` retain exactly the
   live track set after a refresh; `on_changed` fires for content-changed,
   removed, and path-moved inodes (the `--keep-cache` `inval_inode` contract).
7. **Concurrency.** Single-flight + debounce semantics are unchanged; a metadata-op
   storm still costs at most one refresh per `poll_interval`, and a held-open
   descriptor keeps resolving across a refresh (inode stability).
8. **Regression gates.** All crate tests + the `#[ignore]` FUSE e2e mount suite
   stay green; byte-identical audio holds; the Criterion `ci` `sequential_read`
   median stays within the **<10%** run-over-run rule (SP2 does not touch the read
   path, but the gate is enforced).
9. **Benches / evidence.** `bench_refresh` shows refresh-1 flat across library
   sizes and refresh-N tracking the changed set; numbers recorded in the results
   log. Compute-bound, so tempfs (`ci` + `large-compute`) is sufficient per the
   pass conventions (no storage-bound validation required for SP2).

## Out of scope (YAGNI)

- **DB-side `updated_at` index + tombstone table** for a `WHERE updated_at > ?`
  changed-set query — avoids the O(N) identity scan but is clock-based, cannot see
  deletions without tombstones, and shares `content_version`'s blind spot (the same
  triggers maintain `updated_at`, so it would miss a `format`-only change too); the
  cheap render-key scan is estimated at low-tens-of-ms at 1M rows and is measured
  by a bench row. A possible follow-up if the identity scan ever dominates.
- **"Sticky" disambiguation** (a disambiguated name surviving after its collision
  clears) — rejected: it is a behavior change that forfeits the full-rebuild
  equivalence oracle.
- **Parallelizing the diff or the per-changed-track render** — the changed set is
  small by assumption; not worth the machinery.
- **Any serve/read-path, synthesis, or schema change** — SP2 is refresh-only.

## Implementation sequencing (for the plan)

Two stages with a verification checkpoint between them, mirroring SP1's A/B
structure. Both stages ship as part of SP2:

1. **Stage A — change detection + changed-only render.** Add
   `Db::list_render_keys` and `Db::tags_for_tracks`, the `TrackRenderState`
   snapshot (replacing `versions`, including the `refresh()` reset), and the
   diff-driven render that assembles `entries` cheaply and calls the existing
   `build_with`. Gated green by the equivalence gate (trivial here) and all
   regression gates. Record `bench_refresh` numbers.
2. **Stage B — in-place tree mutation.** Add the mutation methods to
   `VirtualTree` and the canonical-order re-disambiguation, replacing the
   from-scratch `build_with`. Gated by the full-rebuild property-test oracle
   (Testing item 1) plus items 4–5. Record `bench_refresh` numbers.

Stages ship as one or two PRs at the implementer's discretion; the gating order
(Stage A's equivalence + regression gates green before Stage B is layered on) is
the requirement.
