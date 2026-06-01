use im::{HashMap as ImHashMap, OrdMap};

/// Assigns stable inodes keyed by rendered path, persisted across tree rebuilds:
/// an unchanged path keeps its inode, a new path gets a fresh one, and a retired
/// inode is never recycled (a stale FUSE handle can't alias a different node).
/// The map grows monotonically with the universe of distinct paths ever rendered.
#[derive(Debug, Clone)]
pub struct InodeAllocator {
    paths: ImHashMap<String, u64>,
    next: u64,
}

impl InodeAllocator {
    pub fn new() -> InodeAllocator {
        let mut paths = ImHashMap::new();
        paths.insert(String::new(), VirtualTree::ROOT); // root path "" -> inode 1
        InodeAllocator { paths, next: 2 }
    }
    /// The inode for `path` (the disambiguated path from root), reused if seen
    /// before, else freshly allocated.
    fn intern(&mut self, path: &str) -> u64 {
        if let Some(&ino) = self.paths.get(path) {
            return ino;
        }
        let ino = self.next;
        self.next += 1;
        self.paths.insert(path.to_string(), ino);
        ino
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
    track_to_inode: ImHashMap<i64, u64>,
}

impl VirtualTree {
    pub const ROOT: u64 = 1;

    pub fn build(entries: &[(i64, String)]) -> VirtualTree {
        VirtualTree::build_with(entries, &mut InodeAllocator::new())
    }

    /// Build the tree assigning inodes via `alloc` (keyed by rendered path), so
    /// inodes are stable across rebuilds that reuse the same allocator.
    pub fn build_with(entries: &[(i64, String)], alloc: &mut InodeAllocator) -> VirtualTree {
        let mut tree = VirtualTree {
            nodes: ImHashMap::new(),
            children: ImHashMap::new(),
            track_to_inode: ImHashMap::new(),
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
        self.children
            .get(&parent)
            .and_then(|c| c.get(name).copied())
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
        let comps: Vec<&str> = path.split('/').filter(|c| !c.is_empty()).collect();
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
        self.children.get_mut(&dir).unwrap().insert(name, inode);
    }

    fn ensure_dir(
        &mut self,
        parent: u64,
        parent_path: &str,
        name: &str,
        alloc: &mut InodeAllocator,
    ) -> (u64, String) {
        if let Some(&existing) = self.children[&parent].get(name) {
            if self.is_dir(existing) {
                return (existing, join_path(parent_path, name));
            }
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
        self.children
            .get_mut(&parent)
            .unwrap()
            .insert(unique, inode);
        (inode, full)
    }

    /// Return `name` if free in `dir`, else append ` (k)` before the extension.
    fn disambiguate(&self, dir: u64, name: &str) -> String {
        let existing = &self.children[&dir];
        if !existing.contains_key(name) {
            return name.to_string();
        }
        let (stem, ext) = match name.rfind('.') {
            Some(i) if i > 0 => (&name[..i], Some(&name[i + 1..])),
            _ => (name, None),
        };
        let mut k = 2u32;
        loop {
            let candidate = match ext {
                Some(e) => format!("{stem} ({k}).{e}"),
                None => format!("{stem} ({k})"),
            };
            if !existing.contains_key(&candidate) {
                return candidate;
            }
            k += 1;
        }
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
    pub fn children_by_rendered(&self, dir: u64, rendered: &str) -> Vec<u64> {
        match self.children.get(&dir) {
            None => Vec::new(),
            Some(kids) => kids
                .values()
                .copied()
                .filter(|&c| {
                    self.nodes
                        .get(&c)
                        .is_some_and(|n| n.rendered_name == rendered)
                })
                .collect(),
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
    /// the inode of the nearest surviving ancestor directory (for dirty bookkeeping).
    pub fn remove_track(&mut self, track_id: i64, _alloc: &mut InodeAllocator) -> Option<u64> {
        let ino = self.track_to_inode.remove(&track_id)?;
        let parent = self.nodes.get(&ino)?.parent;
        let name = self.nodes.get(&ino).map(|n| n.name.clone());
        self.nodes.remove(&ino);
        if let (Some(name), Some(kids)) = (name, self.children.get_mut(&parent)) {
            kids.remove(&name);
        }
        Some(self.prune_empty_dirs_upward(parent))
    }

    /// Rebuild the subtree rooted at directory `dir` so its disambiguation matches a
    /// fresh `build_with`: collect every track currently under `dir`, remove them all
    /// (pruning), then re-insert in ascending track-id order using each track's
    /// RENDERED path from `new_paths`. `ensure_dir` reuses ancestors above `dir`, so
    /// only `dir`'s subtree is rebuilt. Errs if a collected track has no entry in
    /// `new_paths` (caller falls back to a full rebuild). See SP2 Component 3.
    #[allow(clippy::result_unit_err)]
    pub fn rebuild_subtree(
        &mut self,
        dir: u64,
        new_paths: &std::collections::HashMap<i64, String>,
        alloc: &mut InodeAllocator,
    ) -> std::result::Result<(), ()> {
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
            let path = new_paths.get(&id).ok_or(())?;
            self.insert_file(id, path, alloc);
        }
        Ok(())
    }

    /// Apply an incremental change set in place, producing a tree byte-identical to a
    /// full `build_with` over the same final track set. `new_paths` maps every CURRENT
    /// track id to its rendered path. Returns Err(()) on any inconsistency (caller
    /// falls back to full build). See SP2 Component 3.
    #[allow(clippy::result_unit_err)]
    pub fn apply_changes(
        &mut self,
        new_paths: &std::collections::HashMap<i64, String>,
        changed: &[i64],
        added: &[i64],
        removed: &[i64],
        alloc: &mut InodeAllocator,
    ) -> std::result::Result<(), ()> {
        use std::collections::HashSet;
        let mut dirty: HashSet<u64> = HashSet::new();

        // Partition `changed` into path-moved vs unchanged-path (using current tree).
        let mut moved_out: Vec<i64> = Vec::new(); // remove old position
        let mut moved_in: Vec<i64> = Vec::new(); // insert new position
        for &id in changed {
            let new_path = new_paths.get(&id).ok_or(())?;
            match self.inode_of_track(id) {
                Some(ino) if &self.path_of(ino) == new_path => { /* path stable: nothing */ }
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
            if let Some(leaf) = self.inode_of_track(id) {
                if let Some(p) = self.node(leaf).map(|n| n.parent) {
                    dirty.insert(p);
                }
                // propagate up while `id` was the introducing (min) id.
                let mut child = self.node(leaf).map_or(Self::ROOT, |n| n.parent);
                while child != Self::ROOT && self.introducing_id(child) == id {
                    let p = self.node(child).map_or(Self::ROOT, |n| n.parent);
                    dirty.insert(p);
                    child = p;
                }
            }
        }
        for &id in added.iter().chain(moved_in.iter()) {
            let rendered = new_paths.get(&id).ok_or(())?;
            let d = self.deepest_existing_ancestor(rendered);
            dirty.insert(d);
            // propagate up while `id` would become the new min.
            let mut child = d;
            while child != Self::ROOT && id < self.introducing_id(child) {
                let p = self.node(child).map_or(Self::ROOT, |n| n.parent);
                dirty.insert(p);
                child = p;
            }
        }

        // (2) Structural mutation. Record surviving parents of pruned dirs as dirty.
        for &id in removed.iter().chain(moved_out.iter()) {
            if let Some(surv) = self.remove_track(id, alloc) {
                dirty.insert(surv);
            }
        }
        for &id in added.iter().chain(moved_in.iter()) {
            let rendered = new_paths.get(&id).ok_or(())?;
            self.insert_file(id, rendered, alloc);
        }

        // (3) Keep only dirty dirs that still exist; (4) reduce to top-most and rebuild.
        let mut live_dirty: Vec<u64> = dirty
            .into_iter()
            .filter(|d| self.node(*d).is_some())
            .collect();
        live_dirty.sort_by_key(|d| self.path_of(*d).matches('/').count()); // shallow first
        let mut done: HashSet<u64> = HashSet::new();
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
            done.insert(d);
        }
        Ok(())
    }

    /// The deepest directory that exists in the current tree along the RENDERED path
    /// `rendered` (navigating by `rendered_name`). Returns ROOT if none below it exist.
    fn deepest_existing_ancestor(&self, rendered: &str) -> u64 {
        let comps: Vec<&str> = rendered.split('/').filter(|c| !c.is_empty()).collect();
        let mut dir = Self::ROOT;
        // walk dir components only (exclude the final filename component)
        for comp in &comps[..comps.len().saturating_sub(1)] {
            let next = self
                .children_by_rendered(dir, comp)
                .into_iter()
                .find(|&c| self.is_dir(c));
            match next {
                Some(c) => dir = c,
                None => break,
            }
        }
        dir
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
    /// (surviving) ancestor.
    fn prune_empty_dirs_upward(&mut self, mut dir: u64) -> u64 {
        while dir != Self::ROOT && self.children.get(&dir).is_none_or(OrdMap::is_empty) {
            let Some(node) = self.nodes.get(&dir) else {
                break;
            };
            let parent = node.parent;
            let name = self.nodes.get(&dir).map(|n| n.name.clone());
            self.children.remove(&dir);
            self.nodes.remove(&dir);
            if let (Some(name), Some(kids)) = (name, self.children.get_mut(&parent)) {
                kids.remove(&name);
            }
            dir = parent;
        }
        dir
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_with_keeps_inodes_stable_across_rebuilds() {
        let mut alloc = InodeAllocator::new();
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
        let mut alloc = InodeAllocator::new();
        let t1 = VirtualTree::build_with(&[(10, "Gone/X.flac".into())], &mut alloc);
        let gone = t1.lookup(VirtualTree::ROOT, "Gone").unwrap();
        let x = t1.lookup(gone, "X.flac").unwrap();
        let t2 = VirtualTree::build_with(&[(20, "New/Y.flac".into())], &mut alloc);
        let new = t2.lookup(VirtualTree::ROOT, "New").unwrap();
        let y = t2.lookup(new, "Y.flac").unwrap();
        assert!(new != gone && new != x && y != gone && y != x);
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
    fn child_by_rendered_finds_disambiguated_node() {
        let t = VirtualTree::build(&[(10, "D/song.flac".into()), (20, "D/song.flac".into())]);
        let d = t.lookup(VirtualTree::ROOT, "D").unwrap();
        let by_rendered: Vec<u64> = t.children_by_rendered(d, "song.flac");
        assert_eq!(by_rendered.len(), 2);
    }

    #[test]
    fn introducing_id_is_min_descendant_track_id() {
        let mut alloc = InodeAllocator::new();
        let t = VirtualTree::build_with(
            &[(30, "A/B/x.flac".into()), (10, "A/C/y.flac".into())],
            &mut alloc,
        );
        let a = t.lookup(VirtualTree::ROOT, "A").unwrap();
        assert_eq!(t.introducing_id(a), 10);
    }

    #[test]
    fn remove_track_prunes_empty_ancestors_b() {
        let mut alloc = InodeAllocator::new();
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
        let mut alloc = InodeAllocator::new();
        let mut t = VirtualTree::build_with(
            &[(10, "A/x.flac".into()), (20, "A/y.flac".into())],
            &mut alloc,
        );
        let surviving = t.remove_track(10, &mut alloc);
        assert!(t.inode_of_track(10).is_none());
        // `A` must survive because `y.flac` still lives under it; remove_track
        // returns A's inode (nearest surviving ancestor), not ROOT.
        let a = t.lookup(VirtualTree::ROOT, "A").expect("A must survive");
        assert_eq!(surviving, Some(a));
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
    fn rebuild_subtree_reclaims_freed_base_name() {
        let mut alloc = InodeAllocator::new();
        let mut t = VirtualTree::build_with(
            &[(10, "D/song.flac".into()), (20, "D/song.flac".into())],
            &mut alloc,
        );
        let d = t.lookup(VirtualTree::ROOT, "D").unwrap();
        t.remove_track(10, &mut alloc);
        // new_paths after removal: only id 20 remains, rendered "D/song.flac".
        let mut np = std::collections::HashMap::new();
        np.insert(20, "D/song.flac".to_string());
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
        let mut alloc = InodeAllocator::new();
        let mut t = VirtualTree::build_with(&entries, &mut alloc);
        let p = t.lookup(VirtualTree::ROOT, "P").unwrap();
        let np: std::collections::HashMap<i64, String> = entries.iter().cloned().collect();
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
        let mut alloc = InodeAllocator::new();
        let mut t = VirtualTree::build_with(&entries, &mut alloc);
        // Delete track 1 (the dir's min). Dir's introducing id rises to 9; file 5 (id 5 < 9)
        // should now claim base "X.flac" and the dir become "X.flac (2)".
        // Production always feeds `build_with` entries sorted ascending by id
        // (`list_tracks` ORDER BY id; `rebuild_incremental` sort_by_key). The
        // reference must use that same canonical order to be a meaningful oracle.
        let mut new_entries = vec![(9, "X.flac/b.flac".to_string()), (5, "X.flac".to_string())];
        new_entries.sort_by_key(|(id, _)| *id);
        let reference = VirtualTree::build(&new_entries);
        let new_paths: std::collections::HashMap<i64, String> =
            new_entries.iter().cloned().collect();
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
        let mut alloc = InodeAllocator::new();
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
        let new_paths: std::collections::HashMap<i64, String> =
            new_entries.iter().cloned().collect();
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
        let mut alloc = InodeAllocator::new();
        let mut t = VirtualTree::build_with(&entries, &mut alloc);
        // Track 2 moves Old -> New.
        let mut new_entries = vec![
            (1, "Old/a.flac".to_string()),
            (2, "New/b.flac".to_string()),
            (3, "New/c.flac".to_string()),
        ];
        new_entries.sort_by_key(|(id, _)| *id);
        let reference = VirtualTree::build(&new_entries);
        let new_paths: std::collections::HashMap<i64, String> =
            new_entries.iter().cloned().collect();
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
        let mut alloc = InodeAllocator::new();
        let mut t = VirtualTree::build_with(&entries, &mut alloc);
        let reference = VirtualTree::build(&entries);
        let new_paths: std::collections::HashMap<i64, String> = entries.iter().cloned().collect();
        t.apply_changes(&new_paths, &[1], &[], &[], &mut alloc)
            .unwrap();
        assert_eq!(
            paths_of(&t).keys().collect::<Vec<_>>(),
            paths_of(&reference).keys().collect::<Vec<_>>(),
            "unchanged-path changed id must be a no-op"
        );
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
        let mut alloc = InodeAllocator::new();
        let mut t = VirtualTree::build_with(&entries, &mut alloc);
        let p = t.lookup(VirtualTree::ROOT, "P").unwrap();
        // Drop both tracks under Sub; rebuild P from the survivors only.
        t.remove_track(1, &mut alloc);
        t.remove_track(2, &mut alloc);
        let new_entries = vec![(3, "P/keep.flac".to_string())];
        let reference = VirtualTree::build(&new_entries);
        let np: std::collections::HashMap<i64, String> = new_entries.into_iter().collect();
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
        // Both A and B exist: the deepest existing dir for a file under A/B is B.
        assert_eq!(t.deepest_existing_ancestor("A/B/new.flac"), b);
        // A exists but Q does not: the walk stops at A.
        assert_eq!(t.deepest_existing_ancestor("A/Q/new.flac"), a);
        // Nothing below root exists along this path.
        assert_eq!(t.deepest_existing_ancestor("Z/new.flac"), VirtualTree::ROOT);
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
}
