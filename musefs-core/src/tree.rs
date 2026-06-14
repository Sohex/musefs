use im::{HashMap as ImHashMap, OrdMap};
use std::borrow::Cow;

/// Case-fold a name for case-insensitive comparison. Unicode-aware lowercasing;
/// ASCII is the floor. Applied identically to every comparison (collision,
/// disambiguation, lookup) - never compare a folded value against an unfolded one.
fn fold(name: &str) -> String {
    name.to_lowercase()
}

/// Why an incremental tree mutation could not complete; the caller falls back to
/// a full rebuild. Carries diagnostics instead of `()` (#95).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RebuildError {
    /// A track collected for rebuild had no entry in `new_paths`.
    MissingRenderedPath(i64),
    /// Test-only injected failure (`force_apply_fail`).
    TestInjected,
}

/// Assigns stable inodes keyed by rendered path, persisted across tree rebuilds:
/// an unchanged path keeps its inode, a new path gets a fresh one, and a retired
/// inode is never recycled (a stale FUSE handle can't alias a different node).
/// Retired paths are dropped by `prune_retired` once they outnumber live ones,
/// bounding the map at 2x the live tree between prunes; a path that returns
/// after a prune gets a fresh inode rather than its old one.
/// In case-insensitive mounts the key is case-folded, so a track keeps its
/// inode when an unrelated deletion flips a merged directory's display casing
/// (#305).
#[derive(Debug, Clone)]
pub struct InodeAllocator {
    paths: ImHashMap<String, u64>,
    next: u64,
    fold_keys: bool,
}

impl InodeAllocator {
    pub fn new(case_insensitive: bool) -> InodeAllocator {
        let mut paths = ImHashMap::new();
        paths.insert(String::new(), VirtualTree::ROOT); // root path "" -> inode 1
        InodeAllocator {
            paths,
            next: 2,
            fold_keys: case_insensitive,
        }
    }

    /// Transform a path into its map key: case-folded when the mount is
    /// case-insensitive (so a survivor keeps its inode when an unrelated deletion
    /// flips a merged directory's display casing, #305), identity otherwise.
    fn key<'a>(&self, path: &'a str) -> Cow<'a, str> {
        if self.fold_keys {
            Cow::Owned(fold(path))
        } else {
            Cow::Borrowed(path)
        }
    }

    /// The inode for `path` (the disambiguated path from root), reused if seen
    /// before, else freshly allocated.
    fn intern(&mut self, path: &str) -> u64 {
        let key = self.key(path);
        if let Some(&ino) = self.paths.get(key.as_ref()) {
            return ino;
        }
        let ino = self.next;
        self.next += 1;
        self.paths.insert(key.into_owned(), ino);
        ino
    }

    /// Rebuild `paths` from the live tree once retired entries outnumber live
    /// ones (map > 2x live nodes), keeping each live path's existing inode.
    /// `next` is untouched, so a retired inode is never reissued. A retired
    /// path that reappears after a prune gets a fresh inode: a kernel dentry
    /// cached for its old inode resolves ENOENT for at most one entry TTL,
    /// the same degradation as any vanished path.
    pub(crate) fn prune_retired(&mut self, tree: &VirtualTree) {
        if self.paths.len() <= 2 * tree.nodes.len() {
            return;
        }
        let mut live = ImHashMap::new();
        for &ino in tree.nodes.keys() {
            live.insert(self.key(&tree.path_of(ino)).into_owned(), ino);
        }
        self.paths = live;
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NodeKind {
    Dir,
    File { track_id: i64 },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Node {
    pub parent: u64,
    pub name: String,          // disambiguated name
    pub rendered_name: String, // pre-disambiguation base name
    pub kind: NodeKind,
}

/// An in-memory virtual filesystem tree: directories derived from path components
/// and files mapped to track ids. Inodes are stable for the lifetime of the tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VirtualTree {
    nodes: ImHashMap<u64, Node>,
    children: ImHashMap<u64, OrdMap<String, u64>>,
    rendered_children: ImHashMap<u64, OrdMap<String, OrdMap<String, u64>>>,
    /// `parent -> fold(name) -> inode`, populated ONLY when `case_insensitive`.
    /// Kept in sync with `children` on every insert AND removal, so a folded
    /// `lookup` never resolves a stale inode even if the tree is mutated in place
    /// (production folded mounts only ever full-rebuild, but the public
    /// `build_with_ci` + mutators must stay consistent). Gives O(1) folded lookup
    /// and collision checks. Part of structural equality; built deterministically,
    /// so fresh and re-rendered folded trees compare equal.
    folded_children: ImHashMap<u64, ImHashMap<String, u64>>,
    track_to_inode: ImHashMap<i64, u64>,
    /// When true, names are compared case-insensitively (dirs merge, files
    /// disambiguate). Defaults to false (exact matching, identical to Linux).
    case_insensitive: bool,
}

impl VirtualTree {
    pub const ROOT: u64 = 1;

    pub fn build(entries: &[(i64, String)]) -> VirtualTree {
        VirtualTree::build_with(entries, &mut InodeAllocator::new(false))
    }

    /// Case-sensitive build (the default everywhere except a folded mount).
    /// Inodes are assigned via `alloc` (keyed by rendered path), stable across
    /// rebuilds that reuse the same allocator.
    pub fn build_with(entries: &[(i64, String)], alloc: &mut InodeAllocator) -> VirtualTree {
        VirtualTree::build_with_ci(entries, alloc, false)
    }

    /// Build with explicit case-folding. With `case_insensitive`, names fold:
    /// case-variant directories merge, case-variant files disambiguate.
    pub fn build_with_ci(
        entries: &[(i64, String)],
        alloc: &mut InodeAllocator,
        case_insensitive: bool,
    ) -> VirtualTree {
        let mut tree = VirtualTree {
            nodes: ImHashMap::new(),
            children: ImHashMap::new(),
            rendered_children: ImHashMap::new(),
            folded_children: ImHashMap::new(),
            track_to_inode: ImHashMap::new(),
            case_insensitive,
        };
        tree.nodes.insert(
            Self::ROOT,
            Node {
                parent: Self::ROOT,
                name: String::new(),
                rendered_name: String::new(),
                kind: NodeKind::Dir,
            },
        );
        tree.children.insert(Self::ROOT, OrdMap::new());
        tree.rendered_children.insert(Self::ROOT, OrdMap::new());
        for (track_id, path) in entries {
            tree.insert_file(*track_id, path, alloc);
        }
        tree
    }

    /// Structural equality for the equivalence oracle: identical track→inode map,
    /// node set, AND children maps. Delegates to the derived `PartialEq` so adding a
    /// field to `VirtualTree` can never silently weaken the oracle. See SP2 Testing item 1.
    pub fn equiv(&self, other: &VirtualTree) -> bool {
        self == other
    }

    pub fn node(&self, inode: u64) -> Option<&Node> {
        self.nodes.get(&inode)
    }

    /// The parent inode of `inode` (root's parent is itself), or `None` if `inode`
    /// is unknown. Used by the FUSE layer to emit `..` directory entries.
    pub fn parent(&self, inode: u64) -> Option<u64> {
        self.nodes.get(&inode).map(|n| n.parent)
    }

    pub fn children(&self, inode: u64) -> Option<&OrdMap<String, u64>> {
        self.children.get(&inode)
    }

    pub fn lookup(&self, parent: u64, name: &str) -> Option<u64> {
        if self.case_insensitive {
            self.folded_children
                .get(&parent)
                .and_then(|b| b.get(&fold(name)).copied())
        } else {
            self.children
                .get(&parent)
                .and_then(|c| c.get(name).copied())
        }
    }

    pub fn is_dir(&self, inode: u64) -> bool {
        matches!(self.nodes.get(&inode).map(|n| &n.kind), Some(NodeKind::Dir))
    }

    pub fn track_id(&self, inode: u64) -> Option<i64> {
        match self.nodes.get(&inode).map(|n| &n.kind) {
            Some(NodeKind::File { track_id }) => Some(*track_id),
            _ => None,
        }
    }

    /// The inode of the file node serving `track_id`, if present.
    pub fn inode_of_track(&self, track_id: i64) -> Option<u64> {
        self.track_to_inode.get(&track_id).copied()
    }

    fn insert_file(&mut self, track_id: i64, path: &str, alloc: &mut InodeAllocator) {
        let comps: Vec<&str> = path
            .split('/')
            .filter(|c| !c.is_empty() && *c != "." && *c != "..")
            .collect();
        if comps.is_empty() {
            return;
        }
        let mut dir = Self::ROOT;
        let mut dir_path = String::new();
        for comp in &comps[..comps.len() - 1] {
            let (child, child_path) = self.ensure_dir(dir, &dir_path, comp, alloc);
            dir = child;
            dir_path = child_path;
        }
        let raw_name = comps[comps.len() - 1];
        let truncated = truncate_component(raw_name, true);
        let raw_name = truncated.as_ref();
        let name = self.disambiguate(dir, raw_name);
        let full = join_path(&dir_path, &name);
        let inode = alloc.intern(&full);
        self.track_to_inode.insert(track_id, inode);
        self.nodes.insert(
            inode,
            Node {
                parent: dir,
                name: name.clone(),
                rendered_name: raw_name.to_string(),
                kind: NodeKind::File { track_id },
            },
        );
        self.children
            .get_mut(&dir)
            .unwrap()
            .insert(name.clone(), inode);
        self.insert_rendered_child(dir, raw_name, &name, inode);
        self.insert_folded_child(dir, &name, inode);
    }

    fn ensure_dir(
        &mut self,
        parent: u64,
        parent_path: &str,
        name: &str,
        alloc: &mut InodeAllocator,
    ) -> (u64, String) {
        let truncated = truncate_component(name, false);
        let name = truncated.as_ref();
        if let Some(existing) = self.dir_child_named(parent, name) {
            let stored = self
                .node(existing)
                .expect("dir_child_named node")
                .name
                .clone();
            return (existing, join_path(parent_path, &stored));
        }
        let unique = self.disambiguate(parent, name);
        let full = join_path(parent_path, &unique);
        let inode = alloc.intern(&full);
        self.nodes.insert(
            inode,
            Node {
                parent,
                name: unique.clone(),
                rendered_name: name.to_string(),
                kind: NodeKind::Dir,
            },
        );
        self.children.insert(inode, OrdMap::new());
        self.rendered_children.insert(inode, OrdMap::new());
        self.children
            .get_mut(&parent)
            .unwrap()
            .insert(unique.clone(), inode);
        self.insert_rendered_child(parent, name, &unique, inode);
        self.insert_folded_child(parent, &unique, inode);
        (inode, full)
    }

    /// Return `name` if free in `dir`, else append ` (k)` before the extension.
    /// Freeness is case-folded when the tree is case-insensitive.
    fn disambiguate(&self, dir: u64, name: &str) -> String {
        if !self.taken(dir, name) {
            return name.to_string();
        }
        for k in 2u32.. {
            let candidate = suffix_candidate(name, k);
            if !self.taken(dir, &candidate) {
                return candidate;
            }
        }
        unreachable!("an unoccupied candidate rank always exists")
    }

    /// Mirror a child insertion into the folded index (no-op unless folding).
    fn insert_folded_child(&mut self, parent: u64, name: &str, inode: u64) {
        if !self.case_insensitive {
            return;
        }
        self.folded_children
            .entry(parent)
            .or_default()
            .insert(fold(name), inode);
    }

    /// Mirror a child removal out of the folded index, dropping an emptied parent
    /// bucket so lookups never resolve a stale inode (no-op unless folding).
    fn remove_folded_child(&mut self, parent: u64, name: &str) {
        if !self.case_insensitive {
            return;
        }
        if let Some(bucket) = self.folded_children.get_mut(&parent) {
            bucket.remove(&fold(name));
            if bucket.is_empty() {
                self.folded_children.remove(&parent);
            }
        }
    }

    /// True if `name` is already taken in `dir` (folded when case-insensitive).
    fn taken(&self, dir: u64, name: &str) -> bool {
        if self.case_insensitive {
            self.folded_children
                .get(&dir)
                .is_some_and(|b| b.contains_key(&fold(name)))
        } else {
            self.children
                .get(&dir)
                .is_some_and(|c| c.contains_key(name))
        }
    }

    /// An existing *directory* child of `dir` matching `name` (folded when
    /// case-insensitive), for merge reuse. `None` if absent or a non-dir.
    fn dir_child_named(&self, dir: u64, name: &str) -> Option<u64> {
        let ino = if self.case_insensitive {
            self.folded_children
                .get(&dir)
                .and_then(|b| b.get(&fold(name)).copied())
        } else {
            self.children.get(&dir).and_then(|c| c.get(name).copied())
        }?;
        self.is_dir(ino).then_some(ino)
    }

    /// Cheap rename-relevance gate for the child of `dir` whose current key is
    /// `name` and pre-disambiguation name is `rendered`: true if perturbing that
    /// child (removal, or a dir's introducing-id change) could rename any sibling
    /// in a fresh build. O(log)-probe based on the fresh-build invariant: a
    /// non-empty collision group always occupies its base key, with no free
    /// candidate rank below an occupied member. False positives only cost a
    /// redundant subtree rebuild; false negatives would break equivalence.
    fn collision_gate(&self, dir: u64, name: &str, rendered: &str) -> bool {
        // A suffixed member, or a literal name shaped like one (it may be the key
        // a pushed group member reclaims), is always rename-relevant.
        if name != rendered || is_suffix_shaped(name) {
            return true;
        }
        let Some(kids) = self.children.get(&dir) else {
            return false;
        };
        // `name == rendered`: this child owns its base key. Rename-relevant iff
        // some other child (file or dir) shares the rendered name — probe the
        // generated candidate keys until the first free rank.
        for k in 2u32.. {
            let candidate = suffix_candidate(rendered, k);
            match kids.get(&candidate) {
                None => return false,
                Some(c) => {
                    if self
                        .nodes
                        .get(c)
                        .is_some_and(|n| n.rendered_name == rendered)
                    {
                        return true;
                    }
                }
            }
        }
        unreachable!("probe terminates at the first free candidate rank")
    }

    /// All track ids referenced by file nodes (used to prune stale cache entries).
    pub fn track_ids(&self) -> std::collections::HashSet<i64> {
        self.nodes
            .values()
            .filter_map(|n| match &n.kind {
                NodeKind::File { track_id } => Some(*track_id),
                NodeKind::Dir => None,
            })
            .collect()
    }

    /// Inodes of `dir`'s direct children whose pre-disambiguation name is `rendered`.
    fn children_by_rendered_with_examined(&self, dir: u64, rendered: &str) -> (Vec<u64>, usize) {
        match self
            .rendered_children
            .get(&dir)
            .and_then(|kids| kids.get(rendered))
        {
            None => (Vec::new(), 0),
            Some(same_rendered) => {
                let children: Vec<u64> = same_rendered.values().copied().collect();
                let examined = same_rendered.len();
                (children, examined)
            }
        }
    }

    /// Inodes of `dir`'s direct children whose pre-disambiguation name is `rendered`.
    pub fn children_by_rendered(&self, dir: u64, rendered: &str) -> Vec<u64> {
        self.children_by_rendered_with_examined(dir, rendered).0
    }

    #[cfg(test)]
    fn children_by_rendered_examined_for_test(&self, dir: u64, rendered: &str) -> usize {
        self.children_by_rendered_with_examined(dir, rendered).1
    }

    fn insert_rendered_child(&mut self, parent: u64, rendered: &str, name: &str, inode: u64) {
        self.rendered_children
            .entry(parent)
            .or_default()
            .entry(rendered.to_string())
            .or_default()
            .insert(name.to_string(), inode);
    }

    fn remove_rendered_child(&mut self, parent: u64, rendered: &str, name: &str) {
        let Some(by_rendered) = self.rendered_children.get_mut(&parent) else {
            return;
        };
        let remove_bucket = match by_rendered.get_mut(rendered) {
            Some(same_rendered) => {
                same_rendered.remove(name);
                same_rendered.is_empty()
            }
            None => false,
        };
        if remove_bucket {
            by_rendered.remove(rendered);
        }
    }

    /// Minimum descendant track id under `ino` (a file's own id; a dir's min over
    /// files). Returns `i64::MAX` if `ino` has no file descendants (empty subtree).
    pub fn introducing_id(&self, ino: u64) -> i64 {
        if let Some(NodeKind::File { track_id }) = self.nodes.get(&ino).map(|n| &n.kind) {
            return *track_id;
        }
        let mut min = i64::MAX;
        let mut stack = vec![ino];
        while let Some(n) = stack.pop() {
            match self.nodes.get(&n).map(|x| &x.kind) {
                Some(NodeKind::File { track_id }) => min = min.min(*track_id),
                _ => {
                    if let Some(kids) = self.children.get(&n) {
                        for &c in kids.values() {
                            stack.push(c);
                        }
                    }
                }
            }
        }
        min
    }

    /// The full disambiguated path from root to `inode` (root returns "").
    fn path_of(&self, inode: u64) -> String {
        if inode == Self::ROOT {
            return String::new();
        }
        let mut parts = Vec::new();
        let mut cur = inode;
        while cur != Self::ROOT {
            let Some(n) = self.nodes.get(&cur) else {
                break;
            };
            parts.push(n.name.clone());
            cur = n.parent;
        }
        parts.reverse();
        parts.join("/")
    }

    /// Remove the file node for `track_id` and prune now-empty ancestor dirs. Returns
    /// the inode of the nearest surviving ancestor directory plus, when dirs were
    /// pruned, the `(name, rendered_name)` of the topmost pruned dir (the direct
    /// child the survivor lost — for collision-gated dirty bookkeeping).
    pub fn remove_track(
        &mut self,
        track_id: i64,
        _alloc: &mut InodeAllocator,
    ) -> Option<(u64, Option<(String, String)>)> {
        let ino = self.track_to_inode.remove(&track_id)?;
        let parent = self.nodes.get(&ino)?.parent;
        let names = self
            .nodes
            .get(&ino)
            .map(|n| (n.name.clone(), n.rendered_name.clone()));
        self.nodes.remove(&ino);
        if let Some((name, rendered)) = names {
            if let Some(kids) = self.children.get_mut(&parent) {
                kids.remove(&name);
            }
            self.remove_rendered_child(parent, &rendered, &name);
            self.remove_folded_child(parent, &name);
        }
        Some(self.prune_empty_dirs_upward(parent))
    }

    /// Rebuild the subtree rooted at directory `dir` so its disambiguation matches a
    /// fresh `build_with`: collect every track currently under `dir`, remove them all
    /// (pruning), then re-insert in ascending track-id order using each track's
    /// RENDERED path from `new_paths`. `ensure_dir` reuses ancestors above `dir`, so
    /// only `dir`'s subtree is rebuilt. Errs if a collected track has no entry in
    /// `new_paths` (caller falls back to a full rebuild). See SP2 Component 3.
    pub(crate) fn rebuild_subtree(
        &mut self,
        dir: u64,
        new_paths: &std::collections::HashMap<i64, crate::refresh_diff::TrackRenderState>,
        alloc: &mut InodeAllocator,
    ) -> std::result::Result<(), RebuildError> {
        let mut ids = Vec::new();
        let mut stack = vec![dir];
        while let Some(n) = stack.pop() {
            match self.nodes.get(&n).map(|x| x.kind.clone()) {
                Some(NodeKind::File { track_id }) => ids.push(track_id),
                _ => {
                    if let Some(kids) = self.children.get(&n) {
                        for &c in kids.values() {
                            stack.push(c);
                        }
                    }
                }
            }
        }
        for id in &ids {
            self.remove_track(*id, alloc);
        }
        ids.sort_unstable();
        for id in ids {
            let path = new_paths
                .get(&id)
                .map(|s| s.path.as_str())
                .ok_or(RebuildError::MissingRenderedPath(id))?;
            self.insert_file(id, path, alloc);
        }
        Ok(())
    }

    /// Apply an incremental change set in place, producing a tree byte-identical to a
    /// full `build_with` over the same final track set. `new_paths` maps every CURRENT
    /// Removal-side dirty propagation: walk `leaf` -> root. At each level the
    /// parent is dirtied only when the child link is rename-relevant
    /// (`collision_gate`, O(log)) AND the removed `id` was the child's
    /// introducing id (for the leaf itself `introducing_id` is an O(1) read of
    /// its own track id; the O(subtree) walk runs only at gated dir levels). A
    /// gated level where the min did NOT change ends the walk: `id` can't be
    /// the min of any ancestor either.
    fn dirty_removed_ancestors(
        &self,
        leaf: u64,
        id: i64,
        dirty: &mut std::collections::HashSet<u64>,
    ) {
        let mut child = leaf;
        while child != Self::ROOT {
            let Some((parent, name, rendered)) = self
                .node(child)
                .map(|n| (n.parent, n.name.clone(), n.rendered_name.clone()))
            else {
                break;
            };
            if self.collision_gate(parent, &name, &rendered) {
                if self.introducing_id(child) == id {
                    dirty.insert(parent);
                } else {
                    break;
                }
            }
            child = parent;
        }
    }

    /// Add-side dirty propagation: min-flip walk `d` -> root, collision-gated
    /// per level. A gated level where the added `id` is not the new min ends
    /// the walk: ancestor mins only decrease, so no higher flip is possible.
    fn dirty_min_flip_ancestors(
        &self,
        d: u64,
        id: i64,
        dirty: &mut std::collections::HashSet<u64>,
    ) {
        let mut child = d;
        while child != Self::ROOT {
            let Some((parent, name, rendered)) = self
                .node(child)
                .map(|n| (n.parent, n.name.clone(), n.rendered_name.clone()))
            else {
                break;
            };
            if self.collision_gate(parent, &name, &rendered) {
                if id < self.introducing_id(child) {
                    dirty.insert(parent);
                } else {
                    break;
                }
            }
            child = parent;
        }
    }

    /// track id to its rendered path. Returns `Err(RebuildError)` on any inconsistency
    /// (caller falls back to full build). See SP2 Component 3.
    ///
    /// Cost is O(changed) when no rendered names collide (#69): a parent dir is
    /// dirtied — and its subtree rebuilt — only when `collision_gate` says the
    /// change can actually rename a sibling, and the O(subtree) `introducing_id`
    /// walks run only at gated levels. Returns the number of `rebuild_subtree`
    /// calls performed — the tests' observability for the O(changed) contract
    /// (a needless rebuild produces the same tree, so only the count can pin it).
    pub(crate) fn apply_changes(
        &mut self,
        new_paths: &std::collections::HashMap<i64, crate::refresh_diff::TrackRenderState>,
        changed: &[i64],
        added: &[i64],
        removed: &[i64],
        alloc: &mut InodeAllocator,
    ) -> std::result::Result<usize, RebuildError> {
        use std::collections::HashSet;
        let mut dirty: HashSet<u64> = HashSet::new();

        // Partition `changed` into path-moved vs unchanged-path (using current tree).
        let mut moved_out: Vec<i64> = Vec::new(); // remove old position
        let mut moved_in: Vec<i64> = Vec::new(); // insert new position
        for &id in changed {
            let new_path = new_paths
                .get(&id)
                .map(|s| s.path.as_str())
                .ok_or(RebuildError::MissingRenderedPath(id))?;
            match self.inode_of_track(id) {
                Some(ino) if self.path_of(ino) == new_path => { /* path stable: nothing */ }
                Some(_) => {
                    moved_out.push(id);
                    moved_in.push(id);
                }
                None => {
                    // A `changed` id absent from the current tree is unexpected,
                    // but the add path is a superset that still yields a correct
                    // tree, so we tolerate it as an insert rather than bailing.
                    moved_in.push(id);
                }
            }
        }

        // (1) Dirty set on the OLD tree, BEFORE mutating.
        for &id in removed.iter().chain(moved_out.iter()) {
            let Some(leaf) = self.inode_of_track(id) else {
                continue;
            };
            self.dirty_removed_ancestors(leaf, id, &mut dirty);
        }
        for &id in added.iter().chain(moved_in.iter()) {
            let rendered = new_paths
                .get(&id)
                .map(|s| s.path.as_str())
                .ok_or(RebuildError::MissingRenderedPath(id))?;
            let comps: Vec<&str> = rendered.split('/').filter(|c| !c.is_empty()).collect();
            let (d, consumed) = self.deepest_existing_ancestor(rendered);
            // The first new component is the only insertion into existing
            // structure; it can shift siblings only if its base key is occupied
            // (fresh-build invariant: a non-empty group owns its base key).
            if comps.get(consumed).is_some_and(|c| {
                self.children
                    .get(&d)
                    .is_some_and(|kids| kids.contains_key(*c))
            }) {
                dirty.insert(d);
            }
            self.dirty_min_flip_ancestors(d, id, &mut dirty);
        }

        // (2) Structural mutation. A pruned dir chain is rename-relevant for the
        // surviving parent only on a rendered-name collision (gated like step 1).
        for &id in removed.iter().chain(moved_out.iter()) {
            if let Some((surv, Some((name, rendered)))) = self.remove_track(id, alloc)
                && self.collision_gate(surv, &name, &rendered)
            {
                dirty.insert(surv);
            }
        }
        // Insert in ascending id order: two pending ids landing on the same fresh
        // key (no existing collision, so no rebuild repairs it) must rank by id
        // exactly like a fresh build, not by added-before-moved processing order.
        let mut to_insert: Vec<i64> = added.iter().chain(moved_in.iter()).copied().collect();
        to_insert.sort_unstable();
        for id in to_insert {
            let rendered = new_paths
                .get(&id)
                .map(|s| s.path.as_str())
                .ok_or(RebuildError::MissingRenderedPath(id))?;
            self.insert_file(id, rendered, alloc);
        }

        // (3) Keep only dirty dirs that still exist; (4) reduce to top-most and rebuild.
        let mut live_dirty: Vec<u64> = dirty
            .into_iter()
            .filter(|d| self.node(*d).is_some())
            .collect();
        // Shallow first, by component count: ROOT's path is "" (0 components),
        // which must sort before single-component dirs ("A", also 0 slashes).
        live_dirty.sort_by_key(|d| {
            self.path_of(*d)
                .split('/')
                .filter(|c| !c.is_empty())
                .count()
        });
        let mut done: HashSet<u64> = HashSet::new();
        let mut rebuilds = 0usize;
        for d in live_dirty {
            // Re-check: a shallower rebuild_subtree may have pruned this dir.
            if self.node(d).is_none() {
                continue;
            }
            // Skip if an ancestor is already rebuilt.
            if self.ancestor_in(d, &done) {
                continue;
            }
            self.rebuild_subtree(d, new_paths, alloc)?;
            rebuilds += 1;
            done.insert(d);
        }
        Ok(rebuilds)
    }

    /// The deepest directory that exists in the current tree along the RENDERED path
    /// `rendered` (navigating by `rendered_name`), plus the number of leading path
    /// components it consumed — `components[consumed]` is the first component that
    /// would be newly created. Returns (ROOT, 0) if nothing below root exists.
    fn deepest_existing_ancestor(&self, rendered: &str) -> (u64, usize) {
        let comps: Vec<&str> = rendered.split('/').filter(|c| !c.is_empty()).collect();
        let mut dir = Self::ROOT;
        let mut consumed = 0;
        // walk dir components only (exclude the final filename component)
        for comp in &comps[..comps.len().saturating_sub(1)] {
            let next = self
                .children_by_rendered(dir, comp)
                .into_iter()
                .find(|&c| self.is_dir(c));
            match next {
                Some(c) => {
                    dir = c;
                    consumed += 1;
                }
                None => break,
            }
        }
        (dir, consumed)
    }

    /// True if any inode in `set` is an ancestor of `node` (or equals it).
    fn ancestor_in(&self, node: u64, set: &std::collections::HashSet<u64>) -> bool {
        let mut cur = node;
        loop {
            if set.contains(&cur) {
                return true;
            }
            if cur == Self::ROOT {
                return false;
            }
            cur = match self.node(cur) {
                Some(n) => n.parent,
                None => return false,
            };
        }
    }

    /// Walk up from `dir`, removing empty directories; return the first non-empty
    /// (surviving) ancestor and the `(name, rendered_name)` of the last (topmost)
    /// dir removed, if any.
    fn prune_empty_dirs_upward(&mut self, mut dir: u64) -> (u64, Option<(String, String)>) {
        let mut last_pruned = None;
        while dir != Self::ROOT && self.children.get(&dir).is_none_or(OrdMap::is_empty) {
            let Some(node) = self.nodes.get(&dir) else {
                break;
            };
            let parent = node.parent;
            let names = self
                .nodes
                .get(&dir)
                .map(|n| (n.name.clone(), n.rendered_name.clone()));
            self.children.remove(&dir);
            self.rendered_children.remove(&dir);
            self.folded_children.remove(&dir);
            self.nodes.remove(&dir);
            if let Some((name, rendered)) = &names {
                if let Some(kids) = self.children.get_mut(&parent) {
                    kids.remove(name);
                }
                self.remove_rendered_child(parent, rendered, name);
                self.remove_folded_child(parent, name);
            }
            last_pruned = names;
            dir = parent;
        }
        (dir, last_pruned)
    }
}

/// Join a parent path and a child name with `/`, treating an empty parent as root.
fn join_path(parent: &str, name: &str) -> String {
    if parent.is_empty() {
        name.to_string()
    } else {
        format!("{parent}/{name}")
    }
}

/// Split `name` the way `disambiguate` does: the extension is everything after
/// the last `.`, unless the dot is the leading character (a dotfile stays whole).
fn split_suffix_parts(name: &str) -> (&str, Option<&str>) {
    match name.rfind('.') {
        Some(i) if i > 0 => (&name[..i], Some(&name[i + 1..])),
        _ => (name, None),
    }
}

/// Linux NAME_MAX: a single path component may be at most this many *bytes*.
/// A longer component makes lookup/readdir/stat fail with ENAMETOOLONG.
const NAME_MAX: usize = 255;

/// Largest char-boundary prefix of `s` that is at most `max` bytes.
fn truncate_bytes(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// Truncate a rendered component to NAME_MAX bytes. For a leaf (`preserve_ext`),
/// the stem is trimmed so `stem.ext` fits while the extension survives; if the
/// extension alone is too long, the whole name is truncated. Borrows when the
/// component already fits, so the common short-name path allocates nothing.
fn truncate_component(name: &str, preserve_ext: bool) -> Cow<'_, str> {
    if name.len() <= NAME_MAX {
        return Cow::Borrowed(name);
    }
    if preserve_ext && let (stem, Some(ext)) = split_suffix_parts(name) {
        // +1 for the '.' separator.
        if let Some(budget) = NAME_MAX.checked_sub(ext.len() + 1).filter(|b| *b > 0) {
            let stem_t = truncate_bytes(stem, budget);
            return Cow::Owned(format!("{stem_t}.{ext}"));
        }
    }
    Cow::Owned(truncate_bytes(name, NAME_MAX).to_string())
}

/// The rank-`k` disambiguated candidate for `name` (k >= 2): ` (k)` appended to
/// the stem, before any extension. Single source of the suffix format —
/// `disambiguate` generates with it and `collision_gate` probes with it.
fn suffix_candidate(name: &str, k: u32) -> String {
    let (stem, ext) = split_suffix_parts(name);
    let suffix = format!(" ({k})");
    let ext_part = match ext {
        Some(e) => format!(".{e}"),
        None => String::new(),
    };
    let budget = NAME_MAX.saturating_sub(suffix.len() + ext_part.len());
    let stem_t = truncate_bytes(stem, budget);
    format!("{stem_t}{suffix}{ext_part}")
}

/// True if `name` could be a `suffix_candidate` output. Such a name may be the
/// key another rendered-name group's member would claim in a fresh build, so
/// perturbing it must conservatively trigger a parent rebuild.
fn is_suffix_shaped(name: &str) -> bool {
    let (stem, _) = split_suffix_parts(name);
    let Some(open) = stem.rfind(" (") else {
        return false;
    };
    let Some(inner) = stem[open + 2..].strip_suffix(')') else {
        return false;
    };
    inner.parse::<u32>().is_ok_and(|k| k >= 2)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::refresh_diff::TrackRenderState;
    use musefs_db::Format;

    fn trs(path: &str) -> TrackRenderState {
        TrackRenderState {
            content_version: 0,
            format: Format::Flac,
            path: path.into(),
        }
    }

    #[test]
    fn build_with_keeps_inodes_stable_across_rebuilds() {
        let mut alloc = InodeAllocator::new(false);
        let t1 = VirtualTree::build_with(&[(10, "Alice/Song.flac".into())], &mut alloc);
        let alice1 = t1.lookup(VirtualTree::ROOT, "Alice").unwrap();
        let song1 = t1.lookup(alice1, "Song.flac").unwrap();
        let t2 = VirtualTree::build_with(
            &[
                (10, "Alice/Song.flac".into()),
                (20, "Bob/Other.flac".into()),
            ],
            &mut alloc,
        );
        let alice2 = t2.lookup(VirtualTree::ROOT, "Alice").unwrap();
        let song2 = t2.lookup(alice2, "Song.flac").unwrap();
        assert_eq!(alice1, alice2);
        assert_eq!(song1, song2);
        let bob2 = t2.lookup(VirtualTree::ROOT, "Bob").unwrap();
        assert!(bob2 != alice2 && bob2 != song2);
    }

    #[test]
    fn inode_of_track_maps_file_nodes() {
        let t = VirtualTree::build(&[(10, "Alice/Song.flac".into()), (20, "Bob/Tune.flac".into())]);
        let alice = t.lookup(VirtualTree::ROOT, "Alice").unwrap();
        let song = t.lookup(alice, "Song.flac").unwrap();
        assert_eq!(t.inode_of_track(10), Some(song));
        assert!(t.inode_of_track(20).is_some());
        assert_eq!(t.inode_of_track(999), None);
    }

    #[test]
    fn build_with_does_not_recycle_a_vanished_inode() {
        let mut alloc = InodeAllocator::new(false);
        let t1 = VirtualTree::build_with(&[(10, "Gone/X.flac".into())], &mut alloc);
        let gone = t1.lookup(VirtualTree::ROOT, "Gone").unwrap();
        let x = t1.lookup(gone, "X.flac").unwrap();
        let t2 = VirtualTree::build_with(&[(20, "New/Y.flac".into())], &mut alloc);
        let new = t2.lookup(VirtualTree::ROOT, "New").unwrap();
        let y = t2.lookup(new, "Y.flac").unwrap();
        assert!(new != gone && new != x && y != gone && y != x);
    }

    #[test]
    fn prune_retired_bounds_map_under_churn() {
        let mut alloc = InodeAllocator::new(false);
        for generation in 0..100 {
            let entries = vec![(1, format!("Gen{generation}/a.flac"))];
            let tree = VirtualTree::build_with(&entries, &mut alloc);
            alloc.prune_retired(&tree);
            assert!(
                alloc.paths.len() <= 2 * tree.nodes.len(),
                "gen {generation}: map {} exceeds 2x live {}",
                alloc.paths.len(),
                tree.nodes.len()
            );
        }
    }

    #[test]
    fn prune_retired_keeps_live_inodes_stable() {
        let mut alloc = InodeAllocator::new(false);
        let tree = VirtualTree::build_with(&[(1, "Keep/song.flac".into())], &mut alloc);
        let keep_dir = tree.lookup(VirtualTree::ROOT, "Keep").unwrap();
        let keep_file = tree.lookup(keep_dir, "song.flac").unwrap();
        let mut last = tree;
        for generation in 0..10 {
            let entries = vec![
                (1, "Keep/song.flac".to_string()),
                (2, format!("Gen{generation}/x.flac")),
            ];
            last = VirtualTree::build_with(&entries, &mut alloc);
            alloc.prune_retired(&last);
        }
        let d = last.lookup(VirtualTree::ROOT, "Keep").unwrap();
        let f = last.lookup(d, "song.flac").unwrap();
        assert_eq!((d, f), (keep_dir, keep_file), "live paths must keep inodes");
    }

    #[test]
    fn pruned_path_reborn_gets_fresh_inode_never_recycled() {
        let mut alloc = InodeAllocator::new(false);
        let t1 = VirtualTree::build_with(&[(1, "Gone/x.flac".into())], &mut alloc);
        let gone_dir = t1.lookup(VirtualTree::ROOT, "Gone").unwrap();
        let gone_file = t1.lookup(gone_dir, "x.flac").unwrap();
        // Churn well past the threshold so a prune drops the retired entries.
        for generation in 0..10 {
            let t = VirtualTree::build_with(&[(1, format!("Gen{generation}/x.flac"))], &mut alloc);
            alloc.prune_retired(&t);
        }
        assert!(
            !alloc.paths.contains_key("Gone"),
            "retired path must be pruned"
        );
        // Rebirth: same rendered path, strictly fresh inodes (next is monotone).
        let t2 = VirtualTree::build_with(&[(1, "Gone/x.flac".into())], &mut alloc);
        let d2 = t2.lookup(VirtualTree::ROOT, "Gone").unwrap();
        let f2 = t2.lookup(d2, "x.flac").unwrap();
        assert!(
            d2 > gone_file && f2 > gone_file,
            "fresh inodes, never recycled"
        );
        assert_ne!(d2, gone_dir);
        assert_ne!(f2, gone_file);
    }

    #[test]
    fn prune_retired_waits_for_threshold() {
        // Drives the map to exactly 2x live nodes: prune must NOT fire at
        // equality (pins the `<=` boundary for the mutation gate).
        let mut alloc = InodeAllocator::new(false);
        let t1 = VirtualTree::build_with(&[(1, "A/x.flac".into())], &mut alloc);
        let a_dir = t1.lookup(VirtualTree::ROOT, "A").unwrap();
        // paths: "", A, A/x.flac = 3
        let t2 = VirtualTree::build_with(&[(1, "B/x.flac".into())], &mut alloc);
        alloc.prune_retired(&t2); // paths 5, live 3 -> 5 <= 6, no prune
        let t3 = VirtualTree::build_with(&[(1, "B/y.flac".into())], &mut alloc);
        alloc.prune_retired(&t3); // paths 6, live 3 -> 6 <= 6, still no prune
        assert_eq!(
            alloc.paths.get("A"),
            Some(&a_dir),
            "at exactly 2x live the retired entries must survive"
        );
        let t4 = VirtualTree::build_with(&[(1, "C/x.flac".into())], &mut alloc);
        alloc.prune_retired(&t4); // paths 8 > 6: prune fires
        assert!(
            !alloc.paths.contains_key("A"),
            "past 2x live the prune must fire"
        );
        assert_eq!(
            alloc.paths.len(),
            t4.nodes.len(),
            "pruned map is exactly the live set"
        );
    }

    #[test]
    fn disambiguate_keeps_dotfile_whole_and_splits_normal_ext() {
        let t = VirtualTree::build(&[
            (10, "D/.hidden".into()),
            (20, "D/.hidden".into()),
            (30, "D/a.ext".into()),
            (40, "D/a.ext".into()),
        ]);
        let d = t.lookup(VirtualTree::ROOT, "D").unwrap();
        assert!(t.lookup(d, ".hidden").is_some());
        assert!(t.lookup(d, ".hidden (2)").is_some());
        assert!(
            t.lookup(d, " (2).hidden").is_none(),
            "must not split at the index-0 dot"
        );
        assert!(t.lookup(d, "a.ext").is_some());
        assert!(t.lookup(d, "a (2).ext").is_some());
    }

    #[test]
    fn disambiguate_resolves_three_way_collision() {
        let t = VirtualTree::build(&[
            (10, "D/song.flac".into()),
            (20, "D/song.flac".into()),
            (30, "D/song.flac".into()),
        ]);
        let d = t.lookup(VirtualTree::ROOT, "D").unwrap();
        assert!(t.lookup(d, "song.flac").is_some());
        assert!(t.lookup(d, "song (2).flac").is_some());
        assert!(t.lookup(d, "song (3).flac").is_some());
    }

    #[test]
    fn case_insensitive_merges_directories() {
        // Two artist dirs differing only by case collapse into one; both titles
        // live under the first-seen casing.
        let entries = vec![(1i64, "Foo/A".to_string()), (2i64, "foo/B".to_string())];
        let tree = VirtualTree::build_with_ci(&entries, &mut InodeAllocator::new(true), true);
        let foo = tree.lookup(VirtualTree::ROOT, "Foo").expect("Foo dir");
        // Case-insensitive lookup resolves any casing to the same inode.
        assert_eq!(tree.lookup(VirtualTree::ROOT, "foo"), Some(foo));
        assert_eq!(tree.lookup(VirtualTree::ROOT, "FOO"), Some(foo));
        // Exactly one child of root (the merged dir), with both files under it.
        assert_eq!(tree.children(VirtualTree::ROOT).unwrap().len(), 1);
        assert!(tree.lookup(foo, "A").is_some());
        assert!(tree.lookup(foo, "B").is_some());
        assert_eq!(tree.children(foo).unwrap().len(), 2);
    }

    #[test]
    fn case_insensitive_disambiguates_leaf_files() {
        // Two files in the same dir whose names differ only by case must NOT
        // collide: the second is disambiguated.
        let entries = vec![
            (1i64, "Dir/Song".to_string()),
            (2i64, "Dir/song".to_string()),
        ];
        let tree = VirtualTree::build_with_ci(&entries, &mut InodeAllocator::new(true), true);
        let dir = tree.lookup(VirtualTree::ROOT, "Dir").expect("Dir");
        let names: Vec<String> = tree.children(dir).unwrap().keys().cloned().collect();
        // First-seen "Song" keeps its name; "song" becomes "song (2)".
        assert!(names.contains(&"Song".to_string()));
        assert!(names.contains(&"song (2)".to_string()));
        assert_eq!(names.len(), 2);
    }

    #[test]
    fn case_sensitive_keeps_both_dirs_separate() {
        // Control: with folding OFF, "Foo" and "foo" are distinct dirs and a
        // differently-cased query misses.
        let entries = vec![(1i64, "Foo/A".to_string()), (2i64, "foo/B".to_string())];
        let tree = VirtualTree::build_with_ci(&entries, &mut InodeAllocator::new(false), false);
        assert_eq!(tree.children(VirtualTree::ROOT).unwrap().len(), 2);
        assert_ne!(
            tree.lookup(VirtualTree::ROOT, "Foo"),
            tree.lookup(VirtualTree::ROOT, "foo")
        );
        assert_eq!(tree.lookup(VirtualTree::ROOT, "FOO"), None);
    }

    #[test]
    fn case_insensitive_removal_keeps_folded_lookup_consistent() {
        // A folded tree mutated in place must not resolve a removed name via the
        // folded index (the index is maintained on removal, not just insert).
        let entries = vec![
            (1i64, "Dir/Song".to_string()),
            (2i64, "Dir/Other".to_string()),
        ];
        let mut tree = VirtualTree::build_with_ci(&entries, &mut InodeAllocator::new(true), true);
        let dir = tree.lookup(VirtualTree::ROOT, "dir").expect("dir");
        assert!(tree.lookup(dir, "song").is_some());

        tree.remove_track(1, &mut InodeAllocator::new(false));

        // The removed file no longer resolves (any casing); the sibling still does.
        assert_eq!(tree.lookup(dir, "song"), None);
        assert_eq!(tree.lookup(dir, "Song"), None);
        assert!(tree.lookup(dir, "other").is_some());
    }

    #[test]
    fn folded_dir_recase_keeps_survivor_inode() {
        // #305: track 1 seen first wins the merged directory's casing ("Foo");
        // track 2 merges under it. Removing track 1 lets the next full rebuild
        // re-derive the casing from track 2 alone, rendering "foo". Track 2's
        // rendered path is unchanged, so its inode must be stable.
        let mut alloc = InodeAllocator::new(true);
        let entries = vec![(1i64, "Foo/A".to_string()), (2i64, "foo/B".to_string())];
        let tree = VirtualTree::build_with_ci(&entries, &mut alloc, true);
        let before = tree.inode_of_track(2).expect("track 2 inode");

        let rebuilt = VirtualTree::build_with_ci(&[(2i64, "foo/B".to_string())], &mut alloc, true);
        let after = rebuilt
            .inode_of_track(2)
            .expect("track 2 inode after rebuild");

        assert_eq!(
            before, after,
            "survivor inode must survive a folded dir re-case"
        );
        // Scope is inode stability only: the directory legitimately re-cases to "foo".
        assert!(rebuilt.lookup(VirtualTree::ROOT, "foo").is_some());
    }

    #[test]
    fn folded_inode_key_keeps_disambiguated_siblings_distinct() {
        // Guard that folding the key never collapses a legitimately-disambiguated
        // pair: "Song" and "song" fold equal, so the second becomes "song (2)";
        // their fold-keys ("dir/song" vs "dir/song (2)") differ, so do their inodes.
        let mut alloc = InodeAllocator::new(true);
        let entries = vec![
            (1i64, "Dir/Song".to_string()),
            (2i64, "Dir/song".to_string()),
        ];
        let tree = VirtualTree::build_with_ci(&entries, &mut alloc, true);
        let a = tree.inode_of_track(1).expect("track 1 inode");
        let b = tree.inode_of_track(2).expect("track 2 inode");
        assert_ne!(
            a, b,
            "disambiguated folded siblings must not collapse to one inode"
        );

        let dir = tree.lookup(VirtualTree::ROOT, "Dir").expect("Dir");
        assert_eq!(tree.lookup(dir, "Song"), Some(a));
        assert_eq!(tree.lookup(dir, "song (2)"), Some(b));
    }

    #[test]
    fn child_by_rendered_finds_disambiguated_node() {
        let t = VirtualTree::build(&[(10, "D/song.flac".into()), (20, "D/song.flac".into())]);
        let d = t.lookup(VirtualTree::ROOT, "D").unwrap();
        let base = t.lookup(d, "song.flac").unwrap();
        let suffixed = t.lookup(d, "song (2).flac").unwrap();

        assert_eq!(t.children_by_rendered(d, "song.flac"), vec![suffixed, base]);
        assert_eq!(t.children_by_rendered_examined_for_test(d, "song.flac"), 2);
    }

    #[test]
    fn deepest_existing_ancestor_preserves_rendered_dir_vs_file_order() {
        let t = VirtualTree::build(&[(1, "X".into()), (2, "X/a.flac".into())]);
        let file = t.lookup(VirtualTree::ROOT, "X").unwrap();
        let dir = t.lookup(VirtualTree::ROOT, "X (2)").unwrap();

        assert_eq!(
            t.children_by_rendered(VirtualTree::ROOT, "X"),
            vec![file, dir]
        );
        assert_eq!(
            t.deepest_existing_ancestor("X/new.flac"),
            (dir, 1),
            "same-rendered file must not hide the matching directory"
        );
    }

    #[test]
    fn children_by_rendered_updates_when_collision_member_removed() {
        let mut alloc = InodeAllocator::new(false);
        let mut t = VirtualTree::build_with(
            &[(10, "D/song.flac".into()), (20, "D/song.flac".into())],
            &mut alloc,
        );
        let d = t.lookup(VirtualTree::ROOT, "D").unwrap();
        let survivor = t.lookup(d, "song (2).flac").unwrap();

        t.remove_track(10, &mut alloc);

        assert_eq!(t.children_by_rendered(d, "song.flac"), vec![survivor]);
        assert_eq!(t.children_by_rendered_examined_for_test(d, "song.flac"), 1);
    }

    #[test]
    fn children_by_rendered_updates_when_empty_dir_pruned() {
        let mut alloc = InodeAllocator::new(false);
        let mut t = VirtualTree::build_with(
            &[(10, "A/B/x.flac".into()), (20, "A/C/y.flac".into())],
            &mut alloc,
        );
        let a = t.lookup(VirtualTree::ROOT, "A").unwrap();
        assert_eq!(t.children_by_rendered_examined_for_test(a, "B"), 1);

        t.remove_track(10, &mut alloc);

        assert!(t.children_by_rendered(a, "B").is_empty());
        assert_eq!(t.children_by_rendered_examined_for_test(a, "B"), 0);
        assert_eq!(t.children_by_rendered_examined_for_test(a, "C"), 1);
    }

    #[test]
    fn deepest_existing_ancestor_rendered_miss_does_not_scan_root_fanout() {
        let entries: Vec<(i64, String)> = (0..1024)
            .map(|i| {
                (
                    i64::from(i),
                    format!("Artist {i:04}/Album {i:04}/Track.flac"),
                )
            })
            .collect();
        let t = VirtualTree::build(&entries);

        assert_eq!(
            t.deepest_existing_ancestor("Unknown/Unknown/new.flac"),
            (VirtualTree::ROOT, 0)
        );
        assert_eq!(
            t.children_by_rendered_examined_for_test(VirtualTree::ROOT, "Unknown"),
            0,
            "rendered-name miss should examine no same-rendered candidates"
        );
    }

    #[test]
    fn introducing_id_is_min_descendant_track_id() {
        let mut alloc = InodeAllocator::new(false);
        let t = VirtualTree::build_with(
            &[(30, "A/B/x.flac".into()), (10, "A/C/y.flac".into())],
            &mut alloc,
        );
        let a = t.lookup(VirtualTree::ROOT, "A").unwrap();
        assert_eq!(t.introducing_id(a), 10);
    }

    #[test]
    fn remove_track_prunes_empty_ancestors_b() {
        let mut alloc = InodeAllocator::new(false);
        let mut t = VirtualTree::build_with(
            &[(10, "A/B/x.flac".into()), (20, "C/y.flac".into())],
            &mut alloc,
        );
        t.remove_track(10, &mut alloc);
        assert!(t.inode_of_track(10).is_none());
        assert!(t.lookup(VirtualTree::ROOT, "A").is_none());
        assert!(t.lookup(VirtualTree::ROOT, "C").is_some());
    }

    #[test]
    fn remove_track_keeps_parent_with_surviving_sibling() {
        let mut alloc = InodeAllocator::new(false);
        let mut t = VirtualTree::build_with(
            &[(10, "A/x.flac".into()), (20, "A/y.flac".into())],
            &mut alloc,
        );
        let surviving = t.remove_track(10, &mut alloc);
        assert!(t.inode_of_track(10).is_none());
        // `A` must survive because `y.flac` still lives under it; remove_track
        // returns A's inode (nearest surviving ancestor) and no pruned dir.
        let a = t.lookup(VirtualTree::ROOT, "A").expect("A must survive");
        assert_eq!(surviving, Some((a, None)));
        assert!(t.lookup(a, "y.flac").is_some());
    }

    fn paths_of(t: &VirtualTree) -> std::collections::BTreeMap<String, u64> {
        let mut out = std::collections::BTreeMap::new();
        let mut stack = vec![(VirtualTree::ROOT, String::new())];
        while let Some((ino, pfx)) = stack.pop() {
            if let Some(kids) = t.children(ino) {
                for (name, &child) in kids {
                    let p = if pfx.is_empty() {
                        name.clone()
                    } else {
                        format!("{pfx}/{name}")
                    };
                    if t.is_dir(child) {
                        stack.push((child, p));
                    } else {
                        out.insert(p, child);
                    }
                }
            }
        }
        out
    }

    #[test]
    fn rebuild_subtree_reports_missing_rendered_path() {
        use std::collections::HashMap;
        let mut alloc = InodeAllocator::new(false);
        let mut tree = VirtualTree::build_with(&[(10, "Alice/Song.flac".into())], &mut alloc);
        let dir = tree.lookup(VirtualTree::ROOT, "Alice").unwrap();
        let new_paths: HashMap<i64, TrackRenderState> = HashMap::new(); // omits track 10
        let err = tree
            .rebuild_subtree(dir, &new_paths, &mut alloc)
            .unwrap_err();
        assert_eq!(err, RebuildError::MissingRenderedPath(10));
    }

    #[test]
    fn rebuild_subtree_reclaims_freed_base_name() {
        let mut alloc = InodeAllocator::new(false);
        let mut t = VirtualTree::build_with(
            &[(10, "D/song.flac".into()), (20, "D/song.flac".into())],
            &mut alloc,
        );
        let d = t.lookup(VirtualTree::ROOT, "D").unwrap();
        t.remove_track(10, &mut alloc);
        // new_paths after removal: only id 20 remains, rendered "D/song.flac".
        let mut np = std::collections::HashMap::new();
        np.insert(20, trs("D/song.flac"));
        t.rebuild_subtree(d, &np, &mut alloc).unwrap();
        let reborn = t.lookup(d, "song.flac").unwrap();
        assert_eq!(t.inode_of_track(20), Some(reborn));
        assert!(t.lookup(d, "song (2).flac").is_none());
    }

    #[test]
    fn rebuild_subtree_matches_build_for_dir_vs_file() {
        // $album="X.flac" produces dir "X.flac"; a sibling file also "X.flac".
        let entries = vec![
            (1, "P/X.flac".to_string()),
            (2, "P/X.flac/t.flac".to_string()),
        ];
        let reference = VirtualTree::build(&entries);
        let mut alloc = InodeAllocator::new(false);
        let mut t = VirtualTree::build_with(&entries, &mut alloc);
        let p = t.lookup(VirtualTree::ROOT, "P").unwrap();
        let np: std::collections::HashMap<i64, TrackRenderState> =
            entries.iter().map(|&(id, ref p)| (id, trs(p))).collect();
        t.rebuild_subtree(p, &np, &mut alloc).unwrap();
        assert_eq!(
            paths_of(&t).keys().collect::<Vec<_>>(),
            paths_of(&reference).keys().collect::<Vec<_>>()
        );
    }

    #[test]
    fn apply_changes_handles_dir_vs_file_min_id_flip() {
        // P has dir "X.flac" (from $album="X.flac", tracks 1 & 9) and file "X.flac"
        // (track 5). Ascending id: dir introduced by 1 claims "X.flac"; file 5 -> "X (2).flac".
        let entries = vec![
            (1, "X.flac/a.flac".to_string()),
            (9, "X.flac/b.flac".to_string()),
            (5, "X.flac".to_string()), // a FILE rendered "X.flac" in root
        ];
        let mut alloc = InodeAllocator::new(false);
        let mut t = VirtualTree::build_with(&entries, &mut alloc);
        // Delete track 1 (the dir's min). Dir's introducing id rises to 9; file 5 (id 5 < 9)
        // should now claim base "X.flac" and the dir become "X.flac (2)".
        // Production establishes the build path's canonical order by sorting
        // ascending by id in `render_entries` (its `order_entries` helper, #188)
        // — not by inheriting `list_tracks`'s ORDER BY. The reference must use
        // that same canonical order to be a meaningful oracle; the inner build
        // primitive deliberately does NOT sort (these tests feed it id-unordered
        // inputs).
        let mut new_entries = vec![(9, "X.flac/b.flac".to_string()), (5, "X.flac".to_string())];
        new_entries.sort_by_key(|(id, _)| *id);
        let reference = VirtualTree::build(&new_entries);
        let new_paths: std::collections::HashMap<i64, TrackRenderState> = new_entries
            .iter()
            .map(|&(id, ref p)| (id, trs(p)))
            .collect();
        t.apply_changes(&new_paths, &[], &[], &[1], &mut alloc)
            .unwrap();
        assert_eq!(
            paths_of(&t).keys().collect::<Vec<_>>(),
            paths_of(&reference).keys().collect::<Vec<_>>(),
            "dir-vs-file min-id flip must match a full rebuild"
        );
    }

    #[test]
    fn apply_changes_handles_add_side_min_id_flip() {
        // Initial: file "X.flac" (id 2) claims the base name; dir "X.flac"
        // (introduced by id 5) is disambiguated to "X.flac (2)".
        let entries = vec![(2, "X.flac".to_string()), (5, "X.flac/a.flac".to_string())];
        let mut alloc = InodeAllocator::new(false);
        let mut t = VirtualTree::build_with(&entries, &mut alloc);
        // ADD track 1 under the dir: its id (1) is now the dir's min (< file's 2), so a
        // full rebuild gives the DIR the base name and the file becomes "X.flac (2)".
        let mut new_entries = vec![
            (1, "X.flac/b.flac".to_string()),
            (2, "X.flac".to_string()),
            (5, "X.flac/a.flac".to_string()),
        ];
        new_entries.sort_by_key(|(id, _)| *id);
        let reference = VirtualTree::build(&new_entries);
        let new_paths: std::collections::HashMap<i64, TrackRenderState> = new_entries
            .iter()
            .map(|&(id, ref p)| (id, trs(p)))
            .collect();
        t.apply_changes(&new_paths, &[], &[1], &[], &mut alloc)
            .unwrap();
        assert_eq!(
            paths_of(&t).keys().collect::<Vec<_>>(),
            paths_of(&reference).keys().collect::<Vec<_>>(),
            "add-side dir-vs-file min-id flip must match a full rebuild"
        );
    }

    #[test]
    fn apply_changes_handles_moved_track_across_dirs() {
        // A track moves from one album dir to another (a `changed` id whose rendered
        // path differs from its current placement): it must be removed from the old
        // leaf and inserted at the new one, matching a canonical full rebuild.
        let entries = vec![
            (1, "Old/a.flac".to_string()),
            (2, "Old/b.flac".to_string()),
            (3, "New/c.flac".to_string()),
        ];
        let mut alloc = InodeAllocator::new(false);
        let mut t = VirtualTree::build_with(&entries, &mut alloc);
        // Track 2 moves Old -> New.
        let mut new_entries = vec![
            (1, "Old/a.flac".to_string()),
            (2, "New/b.flac".to_string()),
            (3, "New/c.flac".to_string()),
        ];
        new_entries.sort_by_key(|(id, _)| *id);
        let reference = VirtualTree::build(&new_entries);
        let new_paths: std::collections::HashMap<i64, TrackRenderState> = new_entries
            .iter()
            .map(|&(id, ref p)| (id, trs(p)))
            .collect();
        t.apply_changes(&new_paths, &[2], &[], &[], &mut alloc)
            .unwrap();
        assert_eq!(
            paths_of(&t).keys().collect::<Vec<_>>(),
            paths_of(&reference).keys().collect::<Vec<_>>(),
            "moved track must match a full rebuild"
        );
    }

    #[test]
    fn apply_changes_unchanged_path_is_noop_for_changed_id() {
        // A `changed` id whose rendered path is identical (e.g. a tag edit that does
        // not affect the template) must leave structure untouched.
        let entries = vec![(1, "A/x.flac".to_string()), (2, "A/y.flac".to_string())];
        let mut alloc = InodeAllocator::new(false);
        let mut t = VirtualTree::build_with(&entries, &mut alloc);
        let reference = VirtualTree::build(&entries);
        let new_paths: std::collections::HashMap<i64, TrackRenderState> =
            entries.iter().map(|&(id, ref p)| (id, trs(p))).collect();
        let rebuilds = t
            .apply_changes(&new_paths, &[1], &[], &[], &mut alloc)
            .unwrap();
        assert_eq!(rebuilds, 0, "a stable-path changed id must rebuild nothing");
        assert_eq!(
            paths_of(&t).keys().collect::<Vec<_>>(),
            paths_of(&reference).keys().collect::<Vec<_>>(),
            "unchanged-path changed id must be a no-op"
        );
    }

    /// Oracle helper for the collision pins below: apply `changed`/`added`/`removed`
    /// against `before`, then require full `equiv` (inodes included) with a
    /// `build_with` over `after` on a cloned allocator — the same oracle the
    /// facade's debug-assert uses — AND that exactly `expected_rebuilds`
    /// subtree rebuilds ran (the O(changed) contract: a needless rebuild yields
    /// the same tree, so only the count can pin it).
    ///
    /// `after` is sorted ascending by id before building the reference, mirroring
    /// the canonical order production establishes in `render_entries` (#188). The
    /// `build_with` primitive itself does NOT sort, so the oracle must.
    fn assert_apply_matches_build(
        before: &[(i64, String)],
        after: &[(i64, String)],
        changed: &[i64],
        added: &[i64],
        removed: &[i64],
        expected_rebuilds: usize,
    ) {
        let mut alloc = InodeAllocator::new(false);
        let mut t = VirtualTree::build_with(before, &mut alloc);
        let mut after_sorted = after.to_vec();
        after_sorted.sort_by_key(|(id, _)| *id);
        let new_paths: std::collections::HashMap<i64, TrackRenderState> = after_sorted
            .iter()
            .map(|&(id, ref p)| (id, trs(p)))
            .collect();
        let rebuilds = t
            .apply_changes(&new_paths, changed, added, removed, &mut alloc)
            .unwrap();
        assert_eq!(rebuilds, expected_rebuilds, "subtree rebuild count");
        let mut ref_alloc = alloc.clone();
        let reference = VirtualTree::build_with(&after_sorted, &mut ref_alloc);
        assert!(
            t.equiv(&reference),
            "incremental tree diverged from build_with\n  applied: {:?}\n  reference: {:?}",
            paths_of(&t).keys().collect::<Vec<_>>(),
            paths_of(&reference).keys().collect::<Vec<_>>(),
        );
    }

    #[test]
    fn apply_changes_remove_base_of_collision_group_renames_survivor() {
        // ids 1,2 both render "A/t.flac": 1 -> "t.flac", 2 -> "t (2).flac".
        // Removing 1 must give 2 the base name back.
        let before = vec![(1, "A/t.flac".to_string()), (2, "A/t.flac".to_string())];
        let after = vec![(2, "A/t.flac".to_string())];
        assert_apply_matches_build(&before, &after, &[], &[], &[1], 1);
    }

    #[test]
    fn apply_changes_remove_literal_suffix_name_frees_key_for_pushed_member() {
        // id 2's RENDERED name is literally "t (2).flac" (its name equals its
        // rendered name), which pushed group-"t.flac" member id 3 to "t (3).flac".
        // Removing 2 frees the key: a fresh build puts 3 at "t (2).flac".
        let before = vec![
            (1, "A/t.flac".to_string()),
            (2, "A/t (2).flac".to_string()),
            (3, "A/t.flac".to_string()),
        ];
        let after = vec![(1, "A/t.flac".to_string()), (3, "A/t.flac".to_string())];
        assert_apply_matches_build(&before, &after, &[], &[], &[2], 1);
    }

    #[test]
    fn apply_changes_add_smaller_id_into_collision_group_shifts_member() {
        // id 5 holds base "t.flac"; adding id 2 with the same rendered name must
        // re-rank the group: 2 -> "t.flac", 5 -> "t (2).flac".
        let before = vec![(5, "A/t.flac".to_string())];
        let after = vec![(2, "A/t.flac".to_string()), (5, "A/t.flac".to_string())];
        assert_apply_matches_build(&before, &after, &[], &[2], &[], 1);
    }

    #[test]
    fn apply_changes_dir_reclaims_base_name_when_colliding_file_removed() {
        // File id 1 rendered "X" owns the base; the dir for id 2's "X/a.flac" was
        // disambiguated to "X (2)". Removing the file must rename the dir to "X".
        let before = vec![(1, "X".to_string()), (2, "X/a.flac".to_string())];
        let after = vec![(2, "X/a.flac".to_string())];
        assert_apply_matches_build(&before, &after, &[], &[], &[1], 1);
    }

    #[test]
    fn apply_changes_remove_min_id_without_collisions_matches_build() {
        // The removed id is the introducing id of its whole ancestor chain but no
        // rendered names collide: equivalence must hold with no sibling churn.
        let before = vec![
            (1, "A/B/t1.flac".to_string()),
            (2, "A/B/t2.flac".to_string()),
            (3, "C/u.flac".to_string()),
        ];
        let after = vec![(2, "A/B/t2.flac".to_string()), (3, "C/u.flac".to_string())];
        assert_apply_matches_build(&before, &after, &[], &[], &[1], 0);
    }

    #[test]
    fn apply_changes_add_new_top_level_dir_with_min_id_matches_build() {
        // The added id is smaller than every existing id (a root-wide min flip)
        // and lands under a brand-new top-level dir with no collisions.
        let before = vec![(5, "A/t1.flac".to_string()), (6, "A/t2.flac".to_string())];
        let after = vec![
            (1, "B/u.flac".to_string()),
            (5, "A/t1.flac".to_string()),
            (6, "A/t2.flac".to_string()),
        ];
        assert_apply_matches_build(&before, &after, &[], &[1], &[], 0);
    }

    #[test]
    fn apply_changes_moved_and_added_ids_colliding_on_fresh_key_rank_by_id() {
        // id 3 (moved) and id 7 (added) both land on the brand-new key "B/t.flac".
        // Neither collides with an EXISTING child, so no rebuild is triggered; the
        // insertions themselves must still rank by id (3 -> base, 7 -> " (2)"),
        // not by added-before-moved processing order.
        let before = vec![
            (3, "A/old.flac".to_string()),
            (5, "A/keep.flac".to_string()),
        ];
        let after = vec![
            (3, "B/t.flac".to_string()),
            (5, "A/keep.flac".to_string()),
            (7, "B/t.flac".to_string()),
        ];
        assert_apply_matches_build(&before, &after, &[3], &[7], &[], 0);
    }

    #[test]
    fn apply_changes_same_dir_move_with_colliding_dir_rebuilds_root_once() {
        // Dir "D" (introduced by id 1) collides with file id 2 rendered "D", and
        // id 1 moves WITHIN D. The removal-side walk conservatively dirties ROOT
        // (it can't know the track lands back inside D), but exactly once — the
        // add-side equality (id == D's introducing id) must not double anything.
        let before = vec![
            (1, "D/x.flac".to_string()),
            (2, "D".to_string()),
            (3, "D/z.flac".to_string()),
        ];
        let after = vec![
            (1, "D/y.flac".to_string()),
            (2, "D".to_string()),
            (3, "D/z.flac".to_string()),
        ];
        assert_apply_matches_build(&before, &after, &[1], &[], &[], 1);
    }

    #[test]
    fn apply_changes_added_min_id_under_colliding_dir_rebuilds_parent() {
        // File id 2 rendered "D" owns the base key; the dir for id 5's
        // "D/x.flac" was disambiguated to "D (2)". Adding id 1 under the dir
        // flips its introducing id below the file's — a fresh build now gives
        // the DIR the base name. The add-side min-flip walk must catch this.
        let before = vec![(2, "D".to_string()), (5, "D/x.flac".to_string())];
        let after = vec![
            (1, "D/y.flac".to_string()),
            (2, "D".to_string()),
            (5, "D/x.flac".to_string()),
        ];
        assert_apply_matches_build(&before, &after, &[], &[1], &[], 1);
    }

    #[test]
    fn apply_changes_rebuilds_topmost_dirty_dir_only() {
        // Two collision-gated removals dirty ROOT and "A". Shallow-first
        // reduction must rebuild ROOT once and skip "A" as covered.
        let before = vec![
            (1, "t.flac".to_string()),
            (2, "t.flac".to_string()),
            (10, "A/u.flac".to_string()),
            (11, "A/u.flac".to_string()),
        ];
        let after = vec![(2, "t.flac".to_string()), (11, "A/u.flac".to_string())];
        assert_apply_matches_build(&before, &after, &[], &[], &[1, 10], 1);
    }

    #[test]
    fn rebuild_subtree_recurses_multi_level_and_prunes_intermediate() {
        // A two-level subtree under the rebuilt dir: the DFS must reach `t.flac`
        // through the intermediate `Sub` dir, and re-inserting only the survivor
        // must prune the now-empty intermediate dir.
        let entries = vec![
            (1, "P/Sub/t.flac".to_string()),
            (2, "P/Sub/u.flac".to_string()),
            (3, "P/keep.flac".to_string()),
        ];
        let mut alloc = InodeAllocator::new(false);
        let mut t = VirtualTree::build_with(&entries, &mut alloc);
        let p = t.lookup(VirtualTree::ROOT, "P").unwrap();
        // Drop both tracks under Sub; rebuild P from the survivors only.
        t.remove_track(1, &mut alloc);
        t.remove_track(2, &mut alloc);
        let new_entries = vec![(3, "P/keep.flac".to_string())];
        let reference = VirtualTree::build(&new_entries);
        let np: std::collections::HashMap<i64, TrackRenderState> = new_entries
            .iter()
            .map(|&(id, ref p)| (id, trs(p)))
            .collect();
        t.rebuild_subtree(p, &np, &mut alloc).unwrap();
        let p2 = t.lookup(VirtualTree::ROOT, "P").unwrap();
        assert!(
            t.lookup(p2, "Sub").is_none(),
            "empty intermediate dir pruned"
        );
        assert!(t.lookup(p2, "keep.flac").is_some());
        assert_eq!(
            paths_of(&t).keys().collect::<Vec<_>>(),
            paths_of(&reference).keys().collect::<Vec<_>>()
        );
    }

    #[test]
    fn equiv_distinguishes_structurally_different_trees() {
        // `build` is deterministic (fresh allocator from the same base), so two
        // builds of identical entries are equiv; different structure is not.
        let a = VirtualTree::build(&[(10, "A/x.flac".into())]);
        let same = VirtualTree::build(&[(10, "A/x.flac".into())]);
        let different = VirtualTree::build(&[(10, "B/x.flac".into())]);
        assert!(a.equiv(&same), "identical builds must be equiv");
        assert!(
            !a.equiv(&different),
            "different structure must not be equiv (guards equiv->true)"
        );
    }

    #[test]
    fn path_of_returns_full_disambiguated_path() {
        let t = VirtualTree::build(&[(10, "A/B/x.flac".into())]);
        let a = t.lookup(VirtualTree::ROOT, "A").unwrap();
        let b = t.lookup(a, "B").unwrap();
        let x = t.lookup(b, "x.flac").unwrap();
        assert_eq!(t.path_of(x), "A/B/x.flac");
        assert_eq!(t.path_of(b), "A/B");
        assert_eq!(t.path_of(VirtualTree::ROOT), "");
    }

    #[test]
    fn deepest_existing_ancestor_walks_existing_rendered_dirs() {
        let t = VirtualTree::build(&[(10, "A/B/x.flac".into())]);
        let a = t.lookup(VirtualTree::ROOT, "A").unwrap();
        let b = t.lookup(a, "B").unwrap();
        // Both A and B exist: the deepest existing dir for a file under A/B is B,
        // having consumed both dir components.
        assert_eq!(t.deepest_existing_ancestor("A/B/new.flac"), (b, 2));
        // A exists but Q does not: the walk stops at A after one component.
        assert_eq!(t.deepest_existing_ancestor("A/Q/new.flac"), (a, 1));
        // Nothing below root exists along this path.
        assert_eq!(
            t.deepest_existing_ancestor("Z/new.flac"),
            (VirtualTree::ROOT, 0)
        );
    }

    #[test]
    fn ancestor_in_detects_ancestor_and_self() {
        let t = VirtualTree::build(&[(10, "A/B/x.flac".into())]);
        let a = t.lookup(VirtualTree::ROOT, "A").unwrap();
        let b = t.lookup(a, "B").unwrap();
        let mut set = std::collections::HashSet::new();
        set.insert(a);
        assert!(t.ancestor_in(b, &set), "A is an ancestor of B");
        assert!(
            t.ancestor_in(a, &set),
            "a node is its own ancestor for this check"
        );
        let empty = std::collections::HashSet::new();
        assert!(
            !t.ancestor_in(b, &empty),
            "no ancestor present (guards ancestor_in->false)"
        );
    }

    #[test]
    fn over_long_leaf_truncates_to_255_keeping_extension() {
        let path = format!("{}.flac", "t".repeat(300));
        let t = VirtualTree::build(&[(10, path)]);
        let kids = t.children(VirtualTree::ROOT).unwrap();
        assert_eq!(kids.len(), 1);
        let name = kids.keys().next().unwrap();
        assert!(name.len() <= 255, "leaf is {} bytes", name.len());
        assert!(
            std::path::Path::new(name)
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("flac")),
            "extension preserved"
        );
    }

    #[test]
    fn over_long_directory_component_truncates_to_255() {
        let path = format!("{}/Song.flac", "d".repeat(300));
        let t = VirtualTree::build(&[(10, path)]);
        let dir = t
            .children(VirtualTree::ROOT)
            .unwrap()
            .keys()
            .next()
            .unwrap()
            .clone();
        assert!(dir.len() <= 255, "dir is {} bytes", dir.len());
    }

    #[test]
    fn over_long_component_truncates_on_utf8_boundary() {
        let path = format!("{}.flac", "€".repeat(100));
        let t = VirtualTree::build(&[(10, path)]);
        let name = t
            .children(VirtualTree::ROOT)
            .unwrap()
            .keys()
            .next()
            .unwrap()
            .clone();
        assert!(name.len() <= 255);
        assert!(
            std::path::Path::new(&name)
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("flac"))
        );
    }

    #[test]
    fn colliding_over_long_leaves_stay_distinct_and_within_255() {
        let path = format!("{}.flac", "x".repeat(300));
        let entries: Vec<(i64, String)> = (0..12).map(|i| (i, path.clone())).collect();
        let t = VirtualTree::build(&entries);
        let kids = t.children(VirtualTree::ROOT).unwrap();
        assert_eq!(kids.len(), 12, "all collisions disambiguated distinctly");
        for name in kids.keys() {
            // Each name packs to the full NAME_MAX: the base leaf trims its stem to
            // leave room for `.flac`, and every ` (k)`-disambiguated sibling trims
            // per-rank to land on exactly 255 bytes. Pinning the exact length (not
            // just `<= 255`) guards suffix_candidate's budget arithmetic.
            assert_eq!(name.len(), 255, "{name:?}");
        }
    }

    #[test]
    fn over_long_leaf_with_oversize_extension_truncates_whole_name() {
        // When the extension alone is NAME_MAX-1 bytes there is zero byte budget for
        // any stem, so the leaf falls back to a plain whole-name truncation rather
        // than emitting an empty-stem `.ext`. Guards truncate_component's
        // `budget > 0` filter.
        let path = format!("{}.{}", "s".repeat(300), "e".repeat(254));
        let t = VirtualTree::build(&[(10, path)]);
        let name = t
            .children(VirtualTree::ROOT)
            .unwrap()
            .keys()
            .next()
            .unwrap()
            .clone();
        assert!(name.len() <= 255, "{} bytes", name.len());
        assert!(
            !name.starts_with('.'),
            "no empty-stem leading dot: {name:?}"
        );
        assert!(
            name.starts_with('s'),
            "whole-name truncation keeps the stem prefix"
        );
    }

    #[test]
    fn over_long_collisions_render_deterministically() {
        let path = format!("{}.flac", "y".repeat(300));
        let entries: Vec<(i64, String)> = (0..5).map(|i| (i, path.clone())).collect();
        let a = VirtualTree::build(&entries);
        let b = VirtualTree::build(&entries);
        let ak: Vec<_> = a
            .children(VirtualTree::ROOT)
            .unwrap()
            .keys()
            .cloned()
            .collect();
        let bk: Vec<_> = b
            .children(VirtualTree::ROOT)
            .unwrap()
            .keys()
            .cloned()
            .collect();
        assert_eq!(ak, bk);
    }

    #[test]
    fn dot_and_dotdot_plain_components_are_dropped() {
        // A plain field rendering to exactly "." or ".." (e.g. an artist tagged ".")
        // must not create a directory that collides with readdir's hardcoded "."/"..".
        let t = VirtualTree::build(&[(10, "./Song.flac".into()), (20, "../Tune.flac".into())]);
        assert!(t.lookup(VirtualTree::ROOT, ".").is_none());
        assert!(t.lookup(VirtualTree::ROOT, "..").is_none());
        // The dropped level collapses; the leaf lands directly under root.
        assert!(t.lookup(VirtualTree::ROOT, "Song.flac").is_some());
        assert!(t.lookup(VirtualTree::ROOT, "Tune.flac").is_some());
    }

    /// Builds a many-album library and re-tags ONE track (renaming its leaf
    /// within its own album dir, no rendered-name collision). The
    /// `apply_changes` rebuild-subtree count for such a change is 0 — it is
    /// handled by remove+insert — and must stay 0 *regardless of library size*.
    /// A regression that over-dirties (rebuilding album subtrees on every change)
    /// would make the count scale with album count, tripping this gate. (This
    /// guards `apply_changes`'s O(changed) contract; it does NOT guard the
    /// facade's choice to call `apply_changes` vs a full `build_with`.)
    fn library(albums: usize) -> Vec<(i64, String)> {
        let mut e = Vec::new();
        for a in 0..albums {
            for t in 0..3 {
                let id = i64::try_from(a * 3 + t).unwrap();
                e.push((id, format!("Album{a:04}/t{t}.flac")));
            }
        }
        e
    }

    fn rebuilds_for_one_retag(albums: usize) -> usize {
        let entries = library(albums);
        let mut alloc = InodeAllocator::new(false);
        let mut t = VirtualTree::build_with(&entries, &mut alloc);
        // Re-tag track id 1 (Album0000/t1.flac) → renamed leaf in the SAME dir.
        let changed_id: i64 = 1;
        let mut new_entries = entries.clone();
        for (id, path) in &mut new_entries {
            if *id == changed_id {
                *path = "Album0000/renamed.flac".to_string();
            }
        }
        let new_paths: std::collections::HashMap<i64, TrackRenderState> = new_entries
            .iter()
            .map(|&(id, ref p)| (id, trs(p)))
            .collect();
        t.apply_changes(&new_paths, &[changed_id], &[], &[], &mut alloc)
            .unwrap()
    }

    #[test]
    fn apply_changes_rebuild_count_is_size_invariant() {
        const EXPECTED_REBUILDS: usize = 0;
        let small = rebuilds_for_one_retag(43); // 129 tracks
        let large = rebuilds_for_one_retag(683); // 2049 tracks
        assert_eq!(small, EXPECTED_REBUILDS, "small-library rebuild count");
        assert_eq!(
            large, EXPECTED_REBUILDS,
            "large-library rebuild count must not scale with size (O(changed), not O(N))",
        );
    }
}
