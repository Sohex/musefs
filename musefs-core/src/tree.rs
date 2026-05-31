use im::{HashMap as ImHashMap, OrdMap};

/// Assigns stable inodes keyed by rendered path, persisted across tree rebuilds:
/// an unchanged path keeps its inode, a new path gets a fresh one, and a retired
/// inode is never recycled (a stale FUSE handle can't alias a different node).
/// The map grows monotonically with the universe of distinct paths ever rendered.
#[derive(Debug)]
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
#[derive(Debug, Clone)]
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
    #[allow(dead_code)]
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
}
