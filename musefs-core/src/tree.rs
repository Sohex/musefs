use std::collections::{BTreeMap, HashMap};

/// Assigns stable inodes keyed by rendered path, persisted across tree rebuilds:
/// an unchanged path keeps its inode, a new path gets a fresh one, and a retired
/// inode is never recycled (a stale FUSE handle can't alias a different node).
/// The map grows monotonically with the universe of distinct paths ever rendered.
#[derive(Debug)]
pub struct InodeAllocator {
    paths: HashMap<String, u64>,
    next: u64,
}

impl InodeAllocator {
    pub fn new() -> InodeAllocator {
        let mut paths = HashMap::new();
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
    pub name: String,
    pub kind: NodeKind,
}

/// An in-memory virtual filesystem tree: directories derived from path components
/// and files mapped to track ids. Inodes are stable for the lifetime of the tree.
#[derive(Debug, Clone)]
pub struct VirtualTree {
    nodes: HashMap<u64, Node>,
    children: HashMap<u64, BTreeMap<String, u64>>,
    track_to_inode: HashMap<i64, u64>,
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
            nodes: HashMap::new(),
            children: HashMap::new(),
            track_to_inode: HashMap::new(),
        };
        tree.nodes.insert(
            Self::ROOT,
            Node {
                parent: Self::ROOT,
                name: String::new(),
                kind: NodeKind::Dir,
            },
        );
        tree.children.insert(Self::ROOT, BTreeMap::new());
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

    pub fn children(&self, inode: u64) -> Option<&BTreeMap<String, u64>> {
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
        let name = self.disambiguate(dir, comps[comps.len() - 1]);
        let full = join_path(&dir_path, &name);
        let inode = alloc.intern(&full);
        self.track_to_inode.insert(track_id, inode);
        self.nodes.insert(
            inode,
            Node {
                parent: dir,
                name: name.clone(),
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
                kind: NodeKind::Dir,
            },
        );
        self.children.insert(inode, BTreeMap::new());
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
}
