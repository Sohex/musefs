use std::collections::BTreeMap;

use musefs_db::Db;

use crate::error::{CoreError, Result};
use crate::mapping::tags_to_fields;
use crate::reader::{read_at, HeaderCache};
use crate::template::render_path;
use crate::tree::{NodeKind, VirtualTree};

/// How the mount serves file *contents*. The virtual tree is identical either way.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Splice a freshly synthesized metadata region in front of the backing audio.
    Synthesis,
    /// Pure passthrough: serve the original backing file bytes unchanged.
    StructureOnly,
}

/// Per-mount configuration for rendering the virtual hierarchy.
#[derive(Debug, Clone)]
pub struct MountConfig {
    pub template: String,
    pub fallbacks: BTreeMap<String, String>,
    pub default_fallback: String,
    pub mode: Mode,
}

/// Attributes the FUSE layer maps onto `fuser::FileAttr`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attr {
    pub inode: u64,
    pub is_dir: bool,
    pub size: u64,
    pub mtime_secs: i64,
}

/// The composed read-only filesystem: the store, the rendered tree, and the lazy
/// synthesis cache. Methods take `&mut self` (the cache mutates); the FUSE layer
/// mounts this single-threaded.
pub struct Musefs {
    db: Db,
    config: MountConfig,
    tree: VirtualTree,
    cache: HeaderCache,
    last_data_version: i64,
}

impl Musefs {
    pub fn open(db: Db, config: MountConfig) -> Result<Musefs> {
        let tree = Self::build_tree(&db, &config)?;
        let last_data_version = db.data_version()?;
        Ok(Musefs {
            cache: HeaderCache::new(config.mode),
            last_data_version,
            db,
            config,
            tree,
        })
    }

    fn build_tree(db: &Db, config: &MountConfig) -> Result<VirtualTree> {
        let tracks = db.list_tracks()?;
        let mut entries = Vec::with_capacity(tracks.len());
        for t in &tracks {
            let tags = db.get_tags(t.id)?;
            let fields = tags_to_fields(&tags);
            let path = render_path(
                &config.template,
                &fields,
                &config.fallbacks,
                &config.default_fallback,
                t.format.as_str(),
            );
            entries.push((t.id, path));
        }
        Ok(VirtualTree::build(&entries))
    }

    /// Rebuild the tree from the current DB contents (used after external edits).
    pub fn refresh(&mut self) -> Result<()> {
        self.tree = Self::build_tree(&self.db, &self.config)?;
        Ok(())
    }

    /// Cheap check for external DB commits via `PRAGMA data_version`. On a change,
    /// rebuild the tree and drop cached resolutions, then return `true`; the new
    /// version stamp is committed only after a successful rebuild. The FUSE layer
    /// calls this on metadata operations so external edits (a scan, a beets retag)
    /// appear without remounting.
    pub fn poll_refresh(&mut self) -> Result<bool> {
        let version = self.db.data_version()?;
        if version == self.last_data_version {
            return Ok(false);
        }
        // Rebuild before committing the new stamp: if build_tree fails, the stamp
        // stays put so the next poll retries instead of silently serving a stale
        // tree until the next unrelated external commit bumps data_version again.
        self.refresh()?;
        self.last_data_version = version;
        self.cache.clear();
        Ok(true)
    }

    pub fn lookup(&self, parent: u64, name: &str) -> Option<u64> {
        self.tree.lookup(parent, name)
    }

    /// The parent inode of `inode` (root's parent is itself). Forwards to the tree.
    pub fn parent(&self, inode: u64) -> Option<u64> {
        self.tree.parent(inode)
    }

    pub fn getattr(&mut self, inode: u64) -> Result<Attr> {
        let track_id = match self.tree.node(inode) {
            None => return Err(CoreError::NoEntry(inode)),
            Some(node) => match &node.kind {
                NodeKind::Dir => {
                    return Ok(Attr {
                        inode,
                        is_dir: true,
                        size: 0,
                        mtime_secs: 0,
                    })
                }
                NodeKind::File { track_id } => *track_id,
            },
        };
        let resolved = self.cache.resolve(&self.db, track_id)?;
        Ok(Attr {
            inode,
            is_dir: false,
            size: resolved.total_len,
            mtime_secs: resolved.mtime_secs,
        })
    }

    /// Directory entries as `(name, child_inode, is_dir)`.
    pub fn readdir(&self, inode: u64) -> Result<Vec<(String, u64, bool)>> {
        let children = match self.tree.children(inode) {
            Some(children) => children,
            // Only directories have a children map; tell apart a known
            // non-directory (ENOTDIR) from an unknown inode (ENOENT).
            None if self.tree.node(inode).is_some() => return Err(CoreError::NotADir(inode)),
            None => return Err(CoreError::NoEntry(inode)),
        };
        Ok(children
            .iter()
            .map(|(name, &child)| (name.clone(), child, self.tree.is_dir(child)))
            .collect())
    }

    pub fn read(&mut self, inode: u64, offset: u64, size: u64) -> Result<Vec<u8>> {
        let track_id = match self.tree.node(inode) {
            None => return Err(CoreError::NoEntry(inode)),
            Some(node) => match &node.kind {
                NodeKind::Dir => return Err(CoreError::IsDir(inode)),
                NodeKind::File { track_id } => *track_id,
            },
        };
        let resolved = self.cache.resolve(&self.db, track_id)?;
        read_at(&resolved, &self.db, offset, size)
    }
}
