# Issue #114: Rendered-Name Child Index

## Problem

Issue #114 identifies the remaining fan-out-proportional step in incremental
tree refresh. `VirtualTree::deepest_existing_ancestor` navigates a rendered path
by calling `children_by_rendered` at each directory level. Today
`children_by_rendered` scans every disambiguated child under that directory and
filters by `Node.rendered_name`.

For a moved or added track under a template such as `$artist/$album/$title`, a
single-track change can therefore scan every top-level artist at the root before
finding the matching rendered directory. The rest of the #69 refresh path is
collision-gated and O(changed); this sibling scan is the remaining cost that
scales with directory fan-out even when rendered names do not collide.

## Goals

- Make rendered-name child lookup independent of unrelated sibling fan-out.
- Preserve the existing virtual tree semantics: public lookup and readdir remain
  keyed by disambiguated names, while internal refresh navigation uses rendered
  names.
- Keep the change local to `musefs-core/src/tree.rs` and the existing refresh
  benchmark/test surfaces.
- Provide both focused tree-level regression coverage and end-to-end benchmark
  evidence.

## Non-Goals

- Do not change template rendering, collision disambiguation, inode allocation,
  or FUSE behavior.
- Do not address refresh costs outside rendered-name child lookup. The current
  changelog path already renders only changed/added tracks and mutates the
  render-state snapshot in place.
- Do not introduce a new runtime fallback path for index inconsistency; the
  index is maintained as an internal tree invariant and covered by tests.

## Chosen Approach

Add a secondary rendered-name child index to `VirtualTree`:

```rust
children: parent -> disambiguated_name -> inode
rendered_children: parent -> rendered_name -> disambiguated_name -> inode
```

The existing `children` map remains the public directory map for mounted paths,
because mounted paths use post-disambiguation names. The new index is only for
internal operations that need to navigate by pre-disambiguation rendered names,
especially `deepest_existing_ancestor`.

`children_by_rendered(dir, rendered)` becomes an indexed lookup. In the common
no-collision case, it returns a one-element set without scanning siblings. When
several direct children share a rendered name, the lookup scans only that
rendered-name bucket, which is the actual ambiguity the current algorithm already
has to resolve.

## Data Structure

Use an immutable map shape consistent with the current tree representation:

```rust
rendered_children: ImHashMap<u64, OrdMap<String, OrdMap<String, u64>>>
```

The inner `OrdMap<String, u64>` is keyed by the child's disambiguated name. This
preserves the current `children_by_rendered` iteration order, because today's
implementation scans `children[parent]` in disambiguated-name order and filters
by `Node.rendered_name`. Preserving that order matters because
`deepest_existing_ancestor` picks the first same-rendered child that is a
directory.

The field should be part of the `VirtualTree` struct, so existing `self == other`
equivalence also validates the secondary index.

The name `rendered_children` avoids colliding with the existing
`children_by_rendered` method name. The method can keep its public test-facing
API and return a `Vec<u64>` collected from the inner map's values.

## Maintenance Rules

All updates to the secondary index happen at the same boundary as updates to
`nodes` and `children`.

On root initialization:
- Insert an empty `children` entry for `VirtualTree::ROOT`.
- Insert an empty `rendered_children` entry for `VirtualTree::ROOT`.

On inserting a file:
- Insert the node into `nodes`.
- Insert the disambiguated name into `children[parent]`.
- Insert the inode into
  `rendered_children[parent][raw_file_name][disambiguated_file_name]`.

On ensuring a new directory:
- Preserve current semantics: first check `children[parent].get(raw_dir_name)`.
  If the base public key is occupied by a directory, return it. Do not search the
  rendered-name bucket for some other same-rendered directory, because that would
  change collision/disambiguation behavior.
- Otherwise insert the new directory node and empty child maps.
- Insert its disambiguated name into `children[parent]`.
- Insert its inode into
  `rendered_children[parent][raw_dir_name][disambiguated_dir_name]`.

On removing a file:
- Read the node's parent, disambiguated name, and rendered name before deleting
  it.
- Remove the track from `track_to_inode`.
- Remove the node from `nodes`.
- Remove the disambiguated name from `children[parent]`.
- Remove the disambiguated name from
  `rendered_children[parent][rendered_name]`; remove the rendered-name bucket if
  it becomes empty.

On pruning an empty directory:
- Read the directory node's parent, disambiguated name, and rendered name before
  deleting it.
- Remove the directory's own empty `children` and `rendered_children` entries.
- Remove the directory node from `nodes`.
- Remove the disambiguated child link from `children[parent]`.
- Remove the disambiguated child link from
  `rendered_children[parent][rendered_name]`; remove the rendered-name bucket if
  it becomes empty.

`rebuild_subtree` already removes and reinserts tracks through these primitives,
so it should maintain the index without special-case code.

## Algorithm Changes

`children_by_rendered` changes from a sibling scan to an indexed lookup:

```rust
fn children_by_rendered(&self, dir: u64, rendered: &str) -> Vec<u64> {
    self.rendered_children
        .get(&dir)
        .and_then(|kids| kids.get(rendered))
        .map(|same_rendered| same_rendered.values().copied().collect())
        .unwrap_or_default()
}
```

`deepest_existing_ancestor` keeps its current behavior:

- Split the rendered path into components.
- Walk directory components only, excluding the final file component.
- At each level, ask for children whose `rendered_name` matches the component.
- Pick the first child that is a directory.
- Stop at the first missing rendered directory.

The complexity changes from O(number of siblings at each walked level) to
O(size of the matching rendered-name bucket at each walked level). For normal
non-colliding library layouts this is O(path depth), independent of root artist
count or album fan-out.

## Edge Cases

- Multiple files with the same rendered file name remain in the same rendered
  bucket and continue to disambiguate through `children`.
- A file and a directory with the same rendered name remain in the same rendered
  bucket; `deepest_existing_ancestor` filters the bucket for a directory.
- Removing one member of a rendered-name collision bucket leaves the remaining
  members indexed.
- Removing the final member of a rendered-name bucket removes that bucket.
- Pruning an empty directory removes its own child-map entries and the parent
  rendered-name index entry.
- A full rebuild and an equivalent incremental rebuild compare equal, including
  the secondary index.

## Testing

Focused tree tests in `musefs-core/src/tree.rs`:

- Keep or extend `child_by_rendered_finds_disambiguated_node` to assert that
  multiple same-rendered children are returned from the indexed path.
- Add a file-vs-directory rendered-name collision test proving
  `deepest_existing_ancestor` still finds the directory when another bucket
  member is a file.
- Add a removal test proving that deleting one same-rendered file leaves the
  other bucket member reachable through `children_by_rendered`.
- Add a prune test proving that removing the only file under a directory deletes
  that directory from its parent's rendered-name bucket.
- Add a root fan-out regression test that builds many top-level rendered
  directories and resolves a path under an absent rendered root such as
  `Unknown/Unknown/new.flac`.
- Make test-only lookup observability mandatory. Implement `children_by_rendered`
  through a small helper that returns both the matching children and the number
  of same-rendered candidates examined. In non-test builds the count is ignored;
  in tests, the root fan-out miss asserts the count is `0`, proving the lookup
  did not scan unrelated root siblings. Collision tests can assert the count is
  the rendered bucket size.

Existing equivalence tests and the incremental-refresh property oracle should
remain green. Because `VirtualTree::equiv` delegates to derived equality, adding
the new field strengthens those tests automatically.

## Benchmark Evidence

Extend `musefs-core/tests/bench_refresh.rs` with an ignored benchmark for the
root fan-out case described in issue #114.

The current sweep uses `CorpusParams::single(Format::Flac, 1, n)`, producing one
artist and one album with many sibling tracks. The new benchmark should instead
use `CorpusParams::single(Format::Flac, n, 1)`, producing many top-level artists
under `$artist/$album/$title`.

The benchmark should:

- Build independent corpora at sizes such as 100, 1000, 5000, and 20000.
- Open `Musefs` with the existing benchmark config.
- Retag one track to remove its artist/album/title tags, so the rendered path
  moves to the fallback `Unknown/Unknown/...` root entry. Before this issue is
  fixed, looking for absent rendered root `Unknown` scans every existing artist;
  after the fix, it is one indexed miss.
- Print rows such as `refresh-root-fanout-1@5000`.
- Record the implementation results in `BENCHMARKS.md`.

The benchmark is evidence, not a normal CI gate. The focused tree tests provide
the deterministic regression coverage.

## Verification Plan

Implementation should run at least:

```bash
cargo test -p musefs-core --lib tree
cargo test -p musefs-core --test incremental_refresh
cargo test -p musefs-core --release --test bench_refresh bench_refresh_root_fanout_one_across_library_sizes -- --ignored --nocapture
```

Then run the broader core/workspace checks selected by the implementation plan,
including clippy if the Rust diff is non-trivial.

## Open Questions Resolved

- Success evidence will include both focused tree tests and an end-to-end refresh
  benchmark.
- The chosen approach is a full rendered-name child index, not a directory-only
  shortcut or refresh-path memoization.
- The scope is limited to #114; broader O(N) refresh work remains out of scope.
