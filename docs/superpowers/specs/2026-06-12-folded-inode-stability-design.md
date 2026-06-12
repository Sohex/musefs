# Preserve survivor inodes when folded directory casing changes (#305)

## Problem

In case-insensitive mode, `VirtualTree::ensure_dir` (`musefs-core/src/tree.rs`)
merges directories whose names fold equal and keeps the **first-seen** child's
display casing. Full builds iterate entries ordered by track id, so insertion
order decides which casing wins.

`InodeAllocator::intern` keys inodes by the **disambiguated display path** and
persists across full rebuilds — that persistence is what normally keeps an
inode stable while a track's rendered path is unchanged.

These two facts combine into a stability bug:

- Track 1 at `Foo/A.flac` and track 2 at `foo/B.flac` build one merged `Foo`
  directory (track 1 seen first). Track 2's display path is `Foo/B.flac`;
  `intern` stores that key.
- Track 1 is removed. `poll_refresh_notify` routes folded mounts through the
  full-rebuild path, which rebuilds from `[(2, "foo/B.flac")]` only. Now
  `ensure_dir` creates a fresh `foo` directory; track 2's display path becomes
  `foo/B.flac` — a new `intern` key — so the survivor gets a **different inode**
  even though its rendered path (`foo/B.flac`) never changed.

This is not a stale-content bug (`notify_changed` invalidates the old inode).
It is a stability/contract bug: an unrelated deletion reassigns inodes for
tracks whose rendered inputs did not change, weakening the documented
stable-inode guarantee and churning kernel dentries/cache.

Affected trust boundary: virtual-tree inode stability contract
(case-insensitive mounts). Tracking: #280.

## Scope

**Inode-stability only.** The fix preserves the survivor's inode across the
casing flip. It does **not** pin the visible directory casing: a full rebuild
may still legitimately render the merged directory as `foo` once `Foo`-cased
tracks are gone. Pinning the displayed casing was considered and rejected — it
would require new persistent per-folded-directory casing state and introduces a
"stale casing" wrinkle (continuing to show `Foo` after every `Foo`-cased track
is gone) that is arguably worse than a cosmetic re-case. The issue title scopes
the work to inodes, not casing.

## Approach: fold the inode key in case-insensitive mode

Make the inode a function of the **folded** disambiguated path instead of the
literal display path. `fold` is `to_lowercase()`, and `/` is invariant under
it, so folding a whole path equals folding each component — identical to how
`folded_children` already indexes. Then:

```
fold("Foo/B.flac") == fold("foo/B.flac") == "foo/b.flac"
```

Whichever casing a rebuild picks for the merged directory, the survivor's key —
and therefore its inode — is unchanged.

### Why "fix the casing winner" does not work

Any rule that derives casing from the *currently present* tracks (first-seen,
lexicographically smallest, lowest track id) flips the moment the track
contributing the winning casing is deleted — that is the bug itself. The only
deletion-stable rule that still preserves a real casing is to stop letting
casing affect the inode at all. Hence: fold the key.

## Components touched (all in `musefs-core`)

`musefs-fuse`, `musefs-cli`, and the `musefs` binary are untouched.

### 1. `InodeAllocator` (`musefs-core/src/tree.rs`)

- Add a `fold_keys: bool` field.
- `new(case_insensitive: bool)` sets `fold_keys` from its argument (was `new()`).
- Add a private helper that applies the key transform (`Cow` is already
  imported at `tree.rs:2` — no new `use`):

  ```rust
  fn key<'a>(&self, path: &'a str) -> Cow<'a, str> {
      if self.fold_keys {
          Cow::Owned(fold(path))
      } else {
          Cow::Borrowed(path)
      }
  }
  ```

- `intern` keys its `paths` lookup and insert via `self.key(path)`.
- `prune_retired` builds its `live` map via `self.key(&tree.path_of(ino))`.

Both the intern path and the prune-on-churn path apply the **same** transform,
so the rebuilt `paths` map stays consistent with how inodes were interned.

### 2. `Musefs::open` (`musefs-core/src/facade.rs`)

Construct the live allocator from config:

```rust
let mut alloc = InodeAllocator::new(config.case_insensitive);
```

The allocator's fold mode must match the tree's `case_insensitive`; production
threads both from the same `MountConfig`, so they cannot drift.

### 3. Call-site churn

`InodeAllocator::new()` becomes `InodeAllocator::new(bool)` (~28 mechanical
edits, almost all in `tree.rs` tests, plus the two `facade.rs` sites). Call
sites that build case-sensitive trees construct `InodeAllocator::new(false)`;
`build_with_ci` test sites construct the allocator with the same
`case_insensitive` value they pass as the tree's fold flag, so the allocator's
fold mode matches the tree under test. (`build_with` itself does not construct
an allocator — it receives one and forwards `false` as the *tree's* fold flag;
the bool originates at each call site.)

## Correctness

- **No inode collisions.** In case-insensitive mode `disambiguate`/`taken`
  already fold, so sibling names are folded-unique; folded paths are therefore
  unique per node tree-wide — the same invariant `folded_children` lookups
  already rely on. Two distinct nodes can never share a folded key.
- **Case-sensitive mode is byte-for-byte unchanged.** `fold_keys` is false and
  `key()` borrows the path as-is.
- **`prune_retired` semantics preserved.** Still keyed off live nodes, just
  under the folded key; the "retired inode is never reissued" guarantee and the
  `next` watermark are untouched.
- **The incremental path is CI-unreachable, so it needs no change.**
  `poll_refresh_notify` (`facade.rs`) forces case-insensitive mounts through
  `force_full_rebuild` → `rebuild_full` → `build_with_ci`, so the fold-keyed
  allocator is only ever driven by `build_with_ci` + `prune_retired`, never by
  `apply_changes`/`rebuild_incremental` (including its `debug_assertions`
  reference-tree divergence check, which clones `alloc` but is never entered in
  CI mode). No incremental call site needs touching.

## Testing (TDD)

Each commit must be green (the pre-commit hook runs the full workspace suite),
so the failing-first regression and its fix land in the same commit.

1. **#305 regression (failing first).** Build a case-insensitive tree from
   `[(1, "Foo/A"), (2, "foo/B")]` with a persistent `InodeAllocator`; capture
   track 2's inode. Drop track 1, rebuild from `[(2, "foo/B")]` with the same
   allocator, and assert track 2's inode is unchanged — while allowing the
   directory to now render `foo`.
2. **Uniqueness guard.** Build a case-insensitive tree from
   `[(1, "Dir/Song"), (2, "Dir/song")]`, which disambiguates to `Song` and
   `song (2)`. Assert the two tracks get distinct inodes *and* that their
   fold-keys differ (`dir/song` vs `dir/song (2)`) — guarding that folding never
   collapses a legitimately-disambiguated pair, not merely restating test #1.
3. **Regression safety.** Existing `prune_retired_*` and `case_*` tests stay
   green (they pin the case-sensitive contract and folded lookup behavior).

The change restores the already-documented stable-inode contract rather than
altering it, so no behavioral doc change is expected. The implementation plan
must still grep `ARCHITECTURE.md` for any prose claiming inodes key on the
*display* path specifically; if such a claim exists it gets a one-line
correction (a doc touch, not a design change).
