//! FUSE filesystem binding for musefs: translates VFS calls into `musefs-core`
//! operations. Mounted single-threaded (fuser's session loop), matching the
//! `&mut self` read path in `musefs_core::Musefs`.

use musefs_core::CoreError;

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

#[cfg(test)]
mod tests {
    use super::*;
    use musefs_core::CoreError;

    #[test]
    fn maps_core_errors_to_errno() {
        assert_eq!(errno(&CoreError::NoEntry(7)), libc::ENOENT);
        assert_eq!(errno(&CoreError::TrackNotFound(7)), libc::ENOENT);
        assert_eq!(errno(&CoreError::IsDir(7)), libc::EISDIR);
        assert_eq!(errno(&CoreError::BackingChanged("x".into())), libc::EIO);
        assert_eq!(errno(&CoreError::ArtNotSupported), libc::EIO);

        let io = CoreError::Io(std::io::Error::from_raw_os_error(libc::ENOENT));
        assert_eq!(errno(&io), libc::ENOENT);
        let io_other = CoreError::Io(std::io::Error::new(std::io::ErrorKind::Other, "boom"));
        assert_eq!(errno(&io_other), libc::EIO);
    }
}
