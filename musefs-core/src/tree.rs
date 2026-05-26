use std::collections::{BTreeMap, HashMap};

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
    next_inode: u64,
}

impl VirtualTree {
    pub const ROOT: u64 = 1;

    pub fn build(entries: &[(i64, String)]) -> VirtualTree {
        let mut tree = VirtualTree {
            nodes: HashMap::new(),
            children: HashMap::new(),
            next_inode: 2,
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
            tree.insert_file(*track_id, path);
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

    fn alloc(&mut self) -> u64 {
        let inode = self.next_inode;
        self.next_inode += 1;
        inode
    }

    fn insert_file(&mut self, track_id: i64, path: &str) {
        let comps: Vec<&str> = path.split('/').filter(|c| !c.is_empty()).collect();
        if comps.is_empty() {
            return;
        }
        let mut dir = Self::ROOT;
        for comp in &comps[..comps.len() - 1] {
            dir = self.ensure_dir(dir, comp);
        }
        let name = self.disambiguate(dir, comps[comps.len() - 1]);
        let inode = self.alloc();
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

    fn ensure_dir(&mut self, parent: u64, name: &str) -> u64 {
        if let Some(&existing) = self.children[&parent].get(name) {
            if self.is_dir(existing) {
                return existing;
            }
        }
        let unique = self.disambiguate(parent, name);
        let inode = self.alloc();
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
        inode
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
