//! macOS Spotlight hygiene: present a zero-byte `.metadata_never_index` file at
//! the mount root so `mds`/Spotlight skips the volume. macOS-only; on every
//! other OS the marker does not exist and these helpers report absence.

use std::time::SystemTime;

use fuser::{FileAttr, FileType};

/// Mount root inode (fuser's FUSE root id). The marker is a child of the root.
#[cfg(target_os = "macos")]
const ROOT_INO: u64 = 1;

/// Marker filename Spotlight recognizes.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub const MARKER_NAME: &str = ".metadata_never_index";

/// Reserved sentinel inode for the marker. `InodeAllocator` starts at 2 and only
/// ever increments with no upper bound, so `u64::MAX` is unreachable in practice
/// and cannot collide with a real node. (A fixed "high" constant would NOT be
/// safe — there is no allocator ceiling to sit above.)
pub const MARKER_INO: u64 = u64::MAX;

/// The marker's attributes: a zero-byte, read-only regular file owned by the
/// mount, all timestamps set to `mtime` (matching synthetic-node stamping).
pub fn marker_attr(uid: u32, gid: u32, file_mode: u16, mtime: SystemTime) -> FileAttr {
    crate::convert::make_attr(
        MARKER_INO,
        0,
        (FileType::RegularFile, file_mode, 1),
        uid,
        gid,
        mtime,
    )
}

/// Marker inode if `(parent, name)` addresses it; `None` otherwise (always `None`
/// off macOS).
#[cfg(target_os = "macos")]
pub fn marker_lookup(parent: u64, name: &str) -> Option<u64> {
    (parent == ROOT_INO && name == MARKER_NAME).then_some(MARKER_INO)
}

/// True if `ino` is the marker (always `false` off macOS).
#[cfg(target_os = "macos")]
pub fn is_marker(ino: u64) -> bool {
    ino == MARKER_INO
}

/// The readdir entry to append when listing `dir_ino` (only the root, only on
/// macOS); `None` otherwise.
#[cfg(target_os = "macos")]
pub fn marker_dir_entry(dir_ino: u64) -> Option<(u64, FileType, String)> {
    (dir_ino == ROOT_INO).then(|| (MARKER_INO, FileType::RegularFile, MARKER_NAME.to_string()))
}

#[cfg(not(target_os = "macos"))]
pub fn marker_lookup(_parent: u64, _name: &str) -> Option<u64> {
    None
}

#[cfg(not(target_os = "macos"))]
pub fn is_marker(_ino: u64) -> bool {
    false
}

#[cfg(not(target_os = "macos"))]
pub fn marker_dir_entry(_dir_ino: u64) -> Option<(u64, FileType, String)> {
    None
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use fuser::INodeNo;

    use super::*;

    #[test]
    fn marker_attr_is_zero_byte_read_only_file() {
        let mt = SystemTime::UNIX_EPOCH + Duration::from_secs(1000);
        let a = marker_attr(501, 20, 0o444, mt);
        assert_eq!(a.ino, INodeNo(u64::MAX));
        assert_eq!(a.kind, FileType::RegularFile);
        assert_eq!(a.perm, 0o444);
        assert_eq!(a.size, 0);
        assert_eq!(a.nlink, 1);
        assert_eq!(a.uid, 501);
        assert_eq!(a.gid, 20);
        assert_eq!(a.mtime, mt);
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn marker_is_absent_off_macos() {
        assert_eq!(marker_lookup(1, MARKER_NAME), None);
        assert!(!is_marker(MARKER_INO));
        assert_eq!(marker_dir_entry(1), None);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn marker_is_present_on_macos() {
        assert_eq!(marker_lookup(1, MARKER_NAME), Some(MARKER_INO));
        assert_eq!(marker_lookup(2, MARKER_NAME), None);
        assert_eq!(marker_lookup(1, "other"), None);
        assert!(is_marker(MARKER_INO));
        assert_eq!(
            marker_dir_entry(1),
            Some((MARKER_INO, FileType::RegularFile, MARKER_NAME.to_string()))
        );
        assert_eq!(marker_dir_entry(2), None);
    }
}
