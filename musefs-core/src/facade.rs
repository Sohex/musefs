use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use arc_swap::ArcSwap;
use musefs_db::Db;

use crate::db_pool::DbPool;
use crate::error::{CoreError, Result};
use crate::mapping::tags_to_fields;
use crate::reader::{read_at, read_at_with_file, HeaderCache, ResolvedFile};
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

/// An open file handle: the resolved layout and a backing fd opened (and
/// validated) once at `open`, reused for every `read` on this handle.
struct Handle {
    resolved: Arc<ResolvedFile>,
    file: Arc<std::fs::File>,
}

/// The composed read-only filesystem: the store, the rendered tree, and the
/// lazy synthesis cache. All methods take `&self`; the tree is swapped
/// atomically on refresh, the cache is mutex-guarded, and the data-version
/// stamp is atomic. This makes `Musefs` `Sync`, so the FUSE layer can later
/// share it across a worker pool.
pub struct Musefs {
    pool: DbPool,
    config: MountConfig,
    tree: ArcSwap<VirtualTree>,
    cache: Mutex<HeaderCache>,
    last_data_version: AtomicI64,
    handles: Mutex<HashMap<u64, Arc<Handle>>>,
    next_fh: AtomicU64,
}

impl Musefs {
    pub fn open(db: Db, config: MountConfig) -> Result<Musefs> {
        let tree = Self::build_tree(&db, &config)?;
        let last_data_version = db.data_version()?;
        Ok(Musefs {
            cache: Mutex::new(HeaderCache::new(config.mode)),
            last_data_version: AtomicI64::new(last_data_version),
            tree: ArcSwap::from_pointee(tree),
            pool: DbPool::new(db)?,
            config,
            handles: Mutex::new(HashMap::new()),
            next_fh: AtomicU64::new(0),
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
    pub fn refresh(&self) -> Result<()> {
        let tree = self.pool.with(|db| Self::build_tree(db, &self.config))?;
        self.tree.store(Arc::new(tree));
        Ok(())
    }

    // Lock order: when both are needed, acquire a DbPool connection
    // (`pool.with`/`with_poll`) FIRST, then `self.cache`. Never call into the
    // pool while holding the cache lock — that would invert the order and can
    // deadlock once the worker pool runs these concurrently.

    /// Lock the header cache, recovering from a poisoned mutex (a worker that
    /// panicked mid-resolve must not permanently break the mount).
    fn cache(&self) -> std::sync::MutexGuard<'_, HeaderCache> {
        self.cache.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn handles(&self) -> std::sync::MutexGuard<'_, HashMap<u64, Arc<Handle>>> {
        self.handles.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Cheap check for external DB commits via `PRAGMA data_version`. On a change,
    /// rebuild the tree and drop cached resolutions, then return `true`; the new
    /// version stamp is committed only after a successful rebuild. The FUSE layer
    /// calls this on metadata operations so external edits (a scan, a beets retag)
    /// appear without remounting.
    pub fn poll_refresh(&self) -> Result<bool> {
        let version = self.pool.with_poll(|db| Ok(db.data_version()?))?;
        if version == self.last_data_version.load(Ordering::Acquire) {
            return Ok(false);
        }
        // Rebuild + drop cached resolutions BEFORE committing the new stamp, so a
        // concurrent reader that sees the new version also sees fresh state.
        self.refresh()?;
        self.cache().clear();
        self.last_data_version.store(version, Ordering::Release);
        Ok(true)
    }

    pub fn lookup(&self, parent: u64, name: &str) -> Option<u64> {
        self.tree.load().lookup(parent, name)
    }

    /// The parent inode of `inode` (root's parent is itself). Forwards to the tree.
    pub fn parent(&self, inode: u64) -> Option<u64> {
        self.tree.load().parent(inode)
    }

    pub fn getattr(&self, inode: u64) -> Result<Attr> {
        let track_id = {
            let tree = self.tree.load();
            match tree.node(inode) {
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
            }
        };
        let resolved = self
            .pool
            .with(|db| self.cache().resolve(db, track_id))?;
        Ok(Attr {
            inode,
            is_dir: false,
            size: resolved.total_len,
            mtime_secs: resolved.mtime_secs,
        })
    }

    /// Directory entries as `(name, child_inode, is_dir)`.
    pub fn readdir(&self, inode: u64) -> Result<Vec<(String, u64, bool)>> {
        let tree = self.tree.load();
        let children = match tree.children(inode) {
            Some(children) => children,
            // Only directories have a children map; tell apart a known
            // non-directory (ENOTDIR) from an unknown inode (ENOENT).
            None if tree.node(inode).is_some() => return Err(CoreError::NotADir(inode)),
            None => return Err(CoreError::NoEntry(inode)),
        };
        Ok(children
            .iter()
            .map(|(name, &child)| (name.clone(), child, tree.is_dir(child)))
            .collect())
    }

    pub fn read(&self, inode: u64, fh: u64, offset: u64, size: u64) -> Result<Vec<u8>> {
        // Fast path: serve from the per-handle fd + cached layout (no open/stat).
        if fh != 0 {
            let handle = self.handles().get(&fh).cloned();
            if let Some(h) = handle {
                return self
                    .pool
                    .with(|db| read_at_with_file(&h.resolved, db, &h.file, offset, size));
            }
        }
        // Fallback (no prior open, or unknown handle): resolve by inode and open.
        let track_id = {
            let tree = self.tree.load();
            match tree.node(inode) {
                None => return Err(CoreError::NoEntry(inode)),
                Some(node) => match &node.kind {
                    NodeKind::Dir => return Err(CoreError::IsDir(inode)),
                    NodeKind::File { track_id } => *track_id,
                },
            }
        };
        self.pool.with(|db| {
            let resolved = self.cache().resolve(db, track_id)?;
            read_at(&resolved, db, offset, size)
        })
    }

    /// Open a file handle: resolve + validate the layout and open the backing fd
    /// once, store it, and return a non-zero handle id. Subsequent `read`s with
    /// this handle reuse the fd (no per-read open/stat).
    pub fn open_handle(&self, inode: u64) -> Result<u64> {
        let track_id = {
            let tree = self.tree.load();
            match tree.node(inode) {
                None => return Err(CoreError::NoEntry(inode)),
                Some(node) => match &node.kind {
                    NodeKind::Dir => return Err(CoreError::IsDir(inode)),
                    NodeKind::File { track_id } => *track_id,
                },
            }
        };
        let resolved = self.pool.with(|db| self.cache().resolve(db, track_id))?;
        crate::metrics::on_open();
        let file = std::fs::File::open(&resolved.backing_path)?;
        let fh = self.next_fh.fetch_add(1, Ordering::Relaxed) + 1; // never 0
        self.handles()
            .insert(fh, Arc::new(Handle { resolved, file: Arc::new(file) }));
        Ok(fh)
    }

    /// Drop an open handle (closes its backing fd when the last reference goes).
    pub fn release_handle(&self, fh: u64) {
        self.handles().remove(&fh);
    }
}
