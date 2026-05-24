//! FUSE filesystem binding for musefs: translates VFS calls into `musefs-core`
//! operations. Mounted single-threaded (fuser's session loop), matching the
//! `&mut self` read path in `musefs_core::Musefs`.
