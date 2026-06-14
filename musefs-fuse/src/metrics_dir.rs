//! Synthetic `/proc`-style telemetry namespace: a `.musefs-metrics/` directory
//! at the mount root containing a single `metrics` file (#394). Mirrors the
//! Spotlight marker (`platform/spotlight.rs`) but is all-platform and gated at
//! the call sites by the runtime `expose_metrics` flag rather than `#[cfg]`.
//!
//! Reserved inodes sit at the very top of the u64 space, following the marker's
//! rationale: `InodeAllocator` starts at 2 and only increments with no ceiling,
//! so the top band is unreachable in practice (a fixed mid-range constant would
//! NOT be safe). They are disjoint from the macOS marker (`u64::MAX`).

use std::time::SystemTime;

use fuser::{FileAttr, FileType};

/// Mount root inode (fuser's FUSE root id).
const ROOT_INO: u64 = 1;

/// The synthetic directory's name at the mount root.
pub const METRICS_DIR_NAME: &str = ".musefs-metrics";
/// The single file inside it.
pub const METRICS_FILE_NAME: &str = "metrics";

/// Reserved sentinel inodes (top of the u64 space; disjoint from the macOS
/// Spotlight marker at `u64::MAX`).
pub const METRICS_DIR_INO: u64 = u64::MAX - 1;
pub const METRICS_FILE_INO: u64 = u64::MAX - 2;

/// True if `ino` is one of the two reserved metrics inodes.
pub fn is_metrics_ino(ino: u64) -> bool {
    ino == METRICS_DIR_INO || ino == METRICS_FILE_INO
}

/// Resolve `(parent, name)` to a metrics inode, or `None`. Callers gate this on
/// the `expose_metrics` flag.
pub fn metrics_lookup(parent: u64, name: &str) -> Option<u64> {
    if parent == ROOT_INO && name == METRICS_DIR_NAME {
        Some(METRICS_DIR_INO)
    } else if parent == METRICS_DIR_INO && name == METRICS_FILE_NAME {
        Some(METRICS_FILE_INO)
    } else {
        None
    }
}

/// Attributes for the synthetic directory (read-only, size 0, nlink 2).
pub fn dir_attr(uid: u32, gid: u32, dir_mode: u16, mtime: SystemTime) -> FileAttr {
    crate::convert::make_attr(
        METRICS_DIR_INO,
        0,
        (FileType::Directory, dir_mode, 2),
        uid,
        gid,
        mtime,
    )
}

/// Attributes for the synthetic `metrics` file. Size 0 (`/proc`-style): the
/// content is served at read time via `FOPEN_DIRECT_IO`, so the kernel reads to
/// EOF rather than trusting `st_size`.
pub fn file_attr(uid: u32, gid: u32, file_mode: u16, mtime: SystemTime) -> FileAttr {
    crate::convert::make_attr(
        METRICS_FILE_INO,
        0,
        (FileType::RegularFile, file_mode, 1),
        uid,
        gid,
        mtime,
    )
}

/// The readdir entry to append when listing the root (only the root).
pub fn root_dir_entry(dir_ino: u64) -> Option<(u64, FileType, String)> {
    (dir_ino == ROOT_INO).then(|| {
        (
            METRICS_DIR_INO,
            FileType::Directory,
            METRICS_DIR_NAME.to_string(),
        )
    })
}

/// The full inline listing for `readdir(METRICS_DIR_INO)`: `.`, `..`, `metrics`.
pub fn dir_listing() -> Vec<(u64, FileType, String)> {
    vec![
        (METRICS_DIR_INO, FileType::Directory, ".".to_string()),
        (ROOT_INO, FileType::Directory, "..".to_string()),
        (
            METRICS_FILE_INO,
            FileType::RegularFile,
            METRICS_FILE_NAME.to_string(),
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn reserved_inodes_are_top_of_space_and_disjoint() {
        assert_eq!(METRICS_DIR_INO, u64::MAX - 1);
        assert_eq!(METRICS_FILE_INO, u64::MAX - 2);
        assert_ne!(METRICS_DIR_INO, u64::MAX); // != macOS marker
        assert_ne!(METRICS_FILE_INO, u64::MAX);
        assert!(is_metrics_ino(METRICS_DIR_INO));
        assert!(is_metrics_ino(METRICS_FILE_INO));
        assert!(!is_metrics_ino(1));
        assert!(!is_metrics_ino(u64::MAX));
    }

    #[test]
    fn lookup_resolves_dir_then_file() {
        assert_eq!(metrics_lookup(1, METRICS_DIR_NAME), Some(METRICS_DIR_INO));
        assert_eq!(
            metrics_lookup(METRICS_DIR_INO, METRICS_FILE_NAME),
            Some(METRICS_FILE_INO)
        );
        assert_eq!(metrics_lookup(1, "metrics"), None);
        assert_eq!(metrics_lookup(METRICS_DIR_INO, ".musefs-metrics"), None);
        assert_eq!(metrics_lookup(2, METRICS_DIR_NAME), None);
    }

    #[test]
    fn root_dir_entry_only_at_root() {
        let mt = SystemTime::UNIX_EPOCH + Duration::from_secs(1);
        assert!(root_dir_entry(1).is_some());
        assert!(root_dir_entry(2).is_none());
        assert_eq!(dir_attr(0, 0, 0o555, mt).kind, FileType::Directory);
        assert_eq!(file_attr(0, 0, 0o444, mt).size, 0);
        assert_eq!(dir_listing().len(), 3);
    }
}
