//! Pure, platform-neutral conversions between `musefs-core` types and the FUSE
//! layer's `fuser` types, plus the `/proc/self/status` capability parser.
//!
//! These helpers carry the only mutation-tested logic in `musefs-fuse`: they
//! are the one file in this crate left in scope by `.cargo/mutants.toml`. The
//! `Filesystem` trait adapter and session glue in `lib.rs`, and the
//! `cfg(macos)` platform code, are excluded (glue / uncoverable on the Linux
//! mutation runner). See the spec at
//! `docs/superpowers/specs/2026-06/2026-06-10-mutants-fuse-convert-gating-design.md`.

use std::time::{Duration, SystemTime};

use fuser::{FileAttr, FileType, INodeNo};
use musefs_core::Attr;

/// Translate a core `Attr` into a `fuser::FileAttr`. Read-only perms (`0o555`
/// dirs, `0o444` files). A zero `mtime_secs` (e.g. synthetic directories) falls
/// back to `fallback_mtime` so tools don't see a 1970 timestamp.
pub(crate) fn to_file_attr(
    attr: &Attr,
    uid: u32,
    gid: u32,
    fallback_mtime: SystemTime,
) -> FileAttr {
    let mtime = if attr.mtime_secs > 0 {
        SystemTime::UNIX_EPOCH
            + Duration::from_secs(
                u64::try_from(attr.mtime_secs).expect("guarded by mtime_secs > 0"),
            )
    } else {
        fallback_mtime
    };
    let (kind, perm, nlink) = if attr.is_dir {
        (FileType::Directory, 0o555, 2)
    } else {
        (FileType::RegularFile, 0o444, 1)
    };
    FileAttr {
        ino: INodeNo(attr.inode),
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

/// Assemble a directory's readdir listing: `.`, `..`, the children, then the
/// optional Spotlight marker. Pure (no DB/tree access) so it is unit-testable.
pub(crate) fn assemble_dir_listing(
    ino: u64,
    parent: u64,
    entries: Vec<(String, u64, bool)>,
    marker: Option<(u64, FileType, String)>,
) -> Vec<(u64, FileType, String)> {
    let mut listing: Vec<(u64, FileType, String)> = Vec::with_capacity(entries.len() + 2);
    listing.push((ino, FileType::Directory, ".".to_string()));
    listing.push((parent, FileType::Directory, "..".to_string()));
    for (name, child, is_dir) in entries {
        let kind = if is_dir {
            FileType::Directory
        } else {
            FileType::RegularFile
        };
        listing.push((child, kind, name));
    }
    if let Some(entry) = marker {
        listing.push(entry);
    }
    listing
}

/// Parse the `CapEff:` line of `/proc/self/status`; `None` when absent or
/// malformed. Pure string parsing, so it lives here (OS-neutral) rather than in
/// the Linux-only passthrough module.
///
/// Gated `cfg(any(target_os = "linux", test))`: its only non-test caller,
/// `platform::passthrough`'s `definitely_lacks_cap_sys_admin`, is Linux-only, so
/// a `pub(crate)` fn left compiled-but-unused on a non-Linux **non-test** build
/// would trip the `-D warnings` dead_code gate (the macOS clippy job is the only
/// non-Linux gate; FreeBSD is cross-linted). This gate compiles it exactly where
/// it is used — the Linux lib build and every platform's test build.
#[cfg(any(target_os = "linux", test))]
pub(crate) fn cap_eff_has_sys_admin(status: &str) -> Option<bool> {
    const CAP_SYS_ADMIN_BIT: u32 = 21;
    let hex = status
        .lines()
        .find_map(|l| l.strip_prefix("CapEff:"))?
        .trim();
    let mask = u64::from_str_radix(hex, 16).ok()?;
    Some(mask & (1 << CAP_SYS_ADMIN_BIT) != 0)
}

#[cfg(test)]
mod tests {
    use super::{assemble_dir_listing, cap_eff_has_sys_admin, to_file_attr};
    use fuser::{FileType, INodeNo};
    use musefs_core::Attr;
    use std::time::{Duration, SystemTime};

    #[test]
    fn converts_dir_and_file_attrs() {
        let fallback = SystemTime::UNIX_EPOCH + Duration::from_secs(1000);

        let dir = Attr {
            inode: 1,
            is_dir: true,
            size: 0,
            mtime_secs: 0,
        };
        let fa = to_file_attr(&dir, 501, 20, fallback);
        assert_eq!(fa.ino, INodeNo(1));
        assert_eq!(fa.kind, FileType::Directory);
        assert_eq!(fa.perm, 0o555);
        assert_eq!(fa.uid, 501);
        assert_eq!(fa.gid, 20);
        // mtime_secs == 0 falls back to the supplied mount time.
        assert_eq!(fa.mtime, fallback);

        let file = Attr {
            inode: 9,
            is_dir: false,
            size: 4096,
            mtime_secs: 1_700_000_000,
        };
        let fa = to_file_attr(&file, 501, 20, fallback);
        assert_eq!(fa.kind, FileType::RegularFile);
        assert_eq!(fa.perm, 0o444);
        assert_eq!(fa.size, 4096);
        assert_eq!(fa.blocks, 8); // 4096 / 512
        assert_eq!(
            fa.mtime,
            SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000)
        );
    }

    #[test]
    fn assemble_dir_listing_puts_dot_and_dotdot_first() {
        let entries = vec![
            ("Song.flac".to_string(), 42, false),
            ("Sub".to_string(), 43, true),
        ];
        let listing = assemble_dir_listing(7, 3, entries, None);
        assert_eq!(listing.len(), 4);
        assert_eq!(listing[0], (7, FileType::Directory, ".".to_string()));
        assert_eq!(listing[1], (3, FileType::Directory, "..".to_string()));
        assert_eq!(
            listing[2],
            (42, FileType::RegularFile, "Song.flac".to_string())
        );
        assert_eq!(listing[3], (43, FileType::Directory, "Sub".to_string()));
    }

    #[test]
    fn cap_eff_parser_root_mask_has_sys_admin() {
        assert_eq!(
            cap_eff_has_sys_admin("CapPrm:\t0000003fffffffff\nCapEff:\t0000003fffffffff\n"),
            Some(true)
        );
    }

    #[test]
    fn cap_eff_parser_zero_mask_lacks_sys_admin() {
        assert_eq!(
            cap_eff_has_sys_admin("CapEff:\t0000000000000000\n"),
            Some(false)
        );
    }

    #[test]
    fn cap_eff_parser_missing_line_returns_none() {
        assert_eq!(cap_eff_has_sys_admin("Name:\tfoo\nUid:\t1000\n"), None);
    }

    #[test]
    fn cap_eff_parser_garbage_hex_returns_none() {
        assert_eq!(cap_eff_has_sys_admin("CapEff:\tnothex\n"), None);
    }
}
