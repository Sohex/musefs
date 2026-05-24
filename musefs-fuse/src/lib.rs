//! FUSE filesystem binding for musefs: translates VFS calls into `musefs-core`
//! operations. Mounted single-threaded (fuser's session loop), matching the
//! `&mut self` read path in `musefs_core::Musefs`.

use fuser::{FileAttr, FileType};
use musefs_core::Attr;
use musefs_core::CoreError;
use std::time::{Duration, SystemTime};

/// Map a core error onto a POSIX errno for the FUSE reply. `Io` errors carry the
/// underlying errno when present; everything structural collapses to `EIO`.
pub fn errno(err: &CoreError) -> i32 {
    match err {
        CoreError::NoEntry(_) | CoreError::TrackNotFound(_) => libc::ENOENT,
        CoreError::IsDir(_) => libc::EISDIR,
        CoreError::Io(e) => e.raw_os_error().unwrap_or(libc::EIO),
        CoreError::BackingChanged(_)
        | CoreError::Db(_)
        | CoreError::Format(_)
        | CoreError::ArtNotSupported => libc::EIO,
    }
}

/// Translate a core `Attr` into a `fuser::FileAttr`. Read-only perms (`0o555`
/// dirs, `0o444` files). A zero `mtime_secs` (e.g. synthetic directories) falls
/// back to `fallback_mtime` so tools don't see a 1970 timestamp.
pub fn to_file_attr(attr: &Attr, uid: u32, gid: u32, fallback_mtime: SystemTime) -> FileAttr {
    let mtime = if attr.mtime_secs > 0 {
        SystemTime::UNIX_EPOCH + Duration::from_secs(attr.mtime_secs as u64)
    } else {
        fallback_mtime
    };
    let (kind, perm, nlink) = if attr.is_dir {
        (FileType::Directory, 0o555, 2)
    } else {
        (FileType::RegularFile, 0o444, 1)
    };
    FileAttr {
        ino: attr.inode,
        size: attr.size,
        blocks: attr.size.div_ceil(512),
        atime: mtime,
        mtime,
        ctime: mtime,
        crtime: mtime,
        kind,
        perm,
        nlink,
        uid,
        gid,
        rdev: 0,
        blksize: 512,
        flags: 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use musefs_core::CoreError;
    use fuser::FileType;
    use musefs_core::Attr;
    use std::time::{Duration, SystemTime};

    #[test]
    fn maps_core_errors_to_errno() {
        assert_eq!(errno(&CoreError::NoEntry(7)), libc::ENOENT);
        assert_eq!(errno(&CoreError::TrackNotFound(7)), libc::ENOENT);
        assert_eq!(errno(&CoreError::IsDir(7)), libc::EISDIR);
        assert_eq!(errno(&CoreError::BackingChanged("x".into())), libc::EIO);
        assert_eq!(errno(&CoreError::ArtNotSupported), libc::EIO);

        let io = CoreError::Io(std::io::Error::from_raw_os_error(libc::ENOENT));
        assert_eq!(errno(&io), libc::ENOENT);
        let io_other = CoreError::Io(std::io::Error::other("boom"));
        assert_eq!(errno(&io_other), libc::EIO);
    }

    #[test]
    fn converts_dir_and_file_attrs() {
        let fallback = SystemTime::UNIX_EPOCH + Duration::from_secs(1000);

        let dir = Attr { inode: 1, is_dir: true, size: 0, mtime_secs: 0 };
        let fa = to_file_attr(&dir, 501, 20, fallback);
        assert_eq!(fa.ino, 1);
        assert_eq!(fa.kind, FileType::Directory);
        assert_eq!(fa.perm, 0o555);
        assert_eq!(fa.uid, 501);
        assert_eq!(fa.gid, 20);
        // mtime_secs == 0 falls back to the supplied mount time.
        assert_eq!(fa.mtime, fallback);

        let file = Attr { inode: 9, is_dir: false, size: 4096, mtime_secs: 1_700_000_000 };
        let fa = to_file_attr(&file, 501, 20, fallback);
        assert_eq!(fa.kind, FileType::RegularFile);
        assert_eq!(fa.perm, 0o444);
        assert_eq!(fa.size, 4096);
        assert_eq!(fa.blocks, 8); // 4096 / 512
        assert_eq!(fa.mtime, SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000));
    }
}
