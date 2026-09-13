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
use musefs_core::{Attr, VirtualMtime};

/// Build a `FileAttr` from the fields that actually vary between musefs's nodes,
/// applying the conventions shared by all of them: every timestamp set to
/// `mtime`, `blocks` derived from `size`, and the fixed `rdev = 0` /
/// `blksize = 512` / `flags = 0`.
pub(crate) fn make_attr(
    ino: u64,
    size: u64,
    (kind, perm, nlink): (FileType, u16, u32),
    uid: u32,
    gid: u32,
    mtime: SystemTime,
) -> FileAttr {
    FileAttr {
        ino: INodeNo(ino),
        size,
        blocks: size.div_ceil(512),
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

/// Build the `SystemTime` a node's [`VirtualMtime`] describes.
///
/// Split out because the two signs cannot share one expression:
/// `SystemTime + Duration` only moves forward, so a pre-epoch second needs
/// `checked_sub`. A negative `secs` with a non-negative nanosecond part is
/// exactly POSIX's `timespec` convention — the instant is `secs` seconds plus
/// `nanos`, so `-1` with half a billion nanoseconds is half a second *before*
/// the epoch, not one and a half.
///
/// `checked_*` rather than the operators: a stored second near `i64`'s ends
/// would otherwise panic in the middle of a `getattr`. Such a row is nonsense
/// either way, and reporting the fallback beats taking the mount down.
fn virtual_mtime_to_system_time(m: VirtualMtime, fallback: SystemTime) -> Option<SystemTime> {
    let nanos = Duration::from_nanos(u64::from(m.nanos()));
    if m.secs >= 0 {
        let secs = Duration::from_secs(m.secs.unsigned_abs());
        SystemTime::UNIX_EPOCH.checked_add(secs)?.checked_add(nanos)
    } else {
        let secs = Duration::from_secs(m.secs.unsigned_abs());
        SystemTime::UNIX_EPOCH.checked_sub(secs)?.checked_add(nanos)
    }
    .or(Some(fallback))
}

/// Translate a core `Attr` into a `fuser::FileAttr`. Permission bits come from
/// `dir_mode`/`file_mode` (the mount is read-only, so these are advertised but
/// inert for writes).
///
/// A node with no `mtime` — a synthetic directory, which has no row — reports
/// `fallback_mtime` (the mount time) so tools do not see a 1970 timestamp. That
/// used to be spelled "any `mtime_secs <= 0`", which made a legitimate
/// epoch-zero file report the mount time, and would have done the same to every
/// pre-epoch file once the store stopped refusing one (#696). The distinction is
/// structural now, so the fallback fires for the synthetic case and nothing else.
pub(crate) fn to_file_attr(
    attr: &Attr,
    uid: u32,
    gid: u32,
    file_mode: u16,
    dir_mode: u16,
    fallback_mtime: SystemTime,
) -> FileAttr {
    let mtime = attr.mtime.map_or(fallback_mtime, |m| {
        virtual_mtime_to_system_time(m, fallback_mtime).unwrap_or(fallback_mtime)
    });
    let node = if attr.is_dir {
        (FileType::Directory, dir_mode, 2)
    } else {
        (FileType::RegularFile, file_mode, 1)
    };
    make_attr(attr.inode, attr.size, node, uid, gid, mtime)
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
    use musefs_core::{Attr, VirtualMtime};
    use std::time::{Duration, SystemTime};

    #[test]
    fn converts_dir_and_file_attrs() {
        let fallback = SystemTime::UNIX_EPOCH + Duration::from_secs(1000);

        let dir = Attr {
            inode: 1,
            is_dir: true,
            size: 0,
            mtime: None,
        };
        let fa = to_file_attr(&dir, 501, 20, 0o444, 0o555, fallback);
        assert_eq!(fa.ino, INodeNo(1));
        assert_eq!(fa.kind, FileType::Directory);
        assert_eq!(fa.perm, 0o555);
        assert_eq!(fa.uid, 501);
        assert_eq!(fa.gid, 20);
        // A synthetic node has no mtime and falls back to the mount time.
        assert_eq!(fa.mtime, fallback);

        let file = Attr {
            inode: 9,
            is_dir: false,
            size: 4096,
            mtime: Some(VirtualMtime {
                secs: 1_700_000_000,
                content_version: 0,
            }),
        };
        let fa = to_file_attr(&file, 501, 20, 0o444, 0o555, fallback);
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
    fn to_file_attr_applies_distinct_dir_and_file_modes() {
        let fallback = SystemTime::UNIX_EPOCH + Duration::from_secs(1000);
        let dir = Attr {
            inode: 1,
            is_dir: true,
            size: 0,
            mtime: None,
        };
        let file = Attr {
            inode: 2,
            is_dir: false,
            size: 0,
            mtime: None,
        };
        // Deliberately asymmetric so a dir/file swap is observable.
        let d = to_file_attr(&dir, 0, 0, 0o400, 0o700, fallback);
        let f = to_file_attr(&file, 0, 0, 0o400, 0o700, fallback);
        assert_eq!(d.perm, 0o700, "dir must get dir_mode");
        assert_eq!(f.perm, 0o400, "file must get file_mode");
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

    fn file_with(mtime: Option<VirtualMtime>) -> Attr {
        Attr {
            inode: 9,
            is_dir: false,
            size: 0,
            mtime,
        }
    }

    /// The sentinel collision #696 is about. Zero used to mean both "synthetic"
    /// and "the Unix epoch", so a file whose mtime really is `1970-01-01
    /// 00:00:00` reported the mount time instead.
    #[test]
    fn a_file_at_the_epoch_reports_the_epoch_not_the_mount_time() {
        let fallback = SystemTime::UNIX_EPOCH + Duration::from_secs(1000);
        let fa = to_file_attr(
            &file_with(Some(VirtualMtime {
                secs: 0,
                content_version: 0,
            })),
            0,
            0,
            0o444,
            0o555,
            fallback,
        );
        assert_eq!(fa.mtime, SystemTime::UNIX_EPOCH);
        assert_ne!(fa.mtime, fallback);
    }

    /// A pre-epoch backing file is legitimate — an archival rip, a restored
    /// backup — and the store stopped refusing one in v4. The mount has to show
    /// it rather than substituting the mount time, and the arithmetic has to go
    /// *backwards*: `SystemTime + Duration` only moves forward.
    #[test]
    fn a_pre_epoch_file_reports_a_pre_epoch_time() {
        let fallback = SystemTime::UNIX_EPOCH + Duration::from_secs(1000);
        let fa = to_file_attr(
            &file_with(Some(VirtualMtime {
                secs: -86_400,
                content_version: 0,
            })),
            0,
            0,
            0o444,
            0o555,
            fallback,
        );
        assert_eq!(fa.mtime, SystemTime::UNIX_EPOCH - Duration::from_hours(24));
        assert!(fa.mtime < SystemTime::UNIX_EPOCH);
    }

    /// POSIX's `timespec` convention: a negative second with a non-negative
    /// nanosecond part is `secs + nanos`, so the nanoseconds move the instant
    /// *forward* from the negative second rather than further back.
    #[test]
    fn a_pre_epoch_nanosecond_part_moves_forward_from_the_second() {
        let fallback = SystemTime::UNIX_EPOCH;
        let fa = to_file_attr(
            &file_with(Some(VirtualMtime {
                secs: -2,
                content_version: 500_000_000,
            })),
            0,
            0,
            0o444,
            0o555,
            fallback,
        );
        // -2s + 0.5s = 1.5s before the epoch, not 2.5s.
        assert_eq!(
            fa.mtime,
            SystemTime::UNIX_EPOCH - Duration::from_millis(1500)
        );
    }

    /// #725: two metadata edits inside one wall-clock second synthesize
    /// different bytes under the same `updated_at`. If the mount reports only
    /// the second, a same-length rewrite is invisible to every size-plus-mtime
    /// change detector. The version rides in the sub-second part precisely so
    /// the timestamp moves when the bytes do.
    #[test]
    fn two_versions_in_one_second_report_different_times() {
        let fallback = SystemTime::UNIX_EPOCH;
        let at = |content_version| {
            to_file_attr(
                &file_with(Some(VirtualMtime {
                    secs: 1_700_000_000,
                    content_version,
                })),
                0,
                0,
                0o444,
                0o555,
                fallback,
            )
            .mtime
        };
        assert_ne!(at(7), at(8), "a bump must move the reported mtime");
        // And the whole second is still the second, so a consumer that truncates
        // sees exactly what it saw before this existed.
        assert!(at(8) >= SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000));
        assert!(at(8) < SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_001));
    }

    /// The nanosecond field is a change counter folded into a range the kernel
    /// accepts, not a duration. What it must never do is leave that range.
    #[test]
    fn the_derived_nanoseconds_stay_in_range() {
        for content_version in [0, 1, 999_999_999, 1_000_000_000, 1_000_000_001, i64::MAX] {
            let n = VirtualMtime {
                secs: 0,
                content_version,
            }
            .nanos();
            assert!(n < 1_000_000_000, "{content_version} -> {n}");
        }
        // Distinct within a billion, which is the window that matters.
        assert_ne!(
            VirtualMtime {
                secs: 0,
                content_version: 1
            }
            .nanos(),
            VirtualMtime {
                secs: 0,
                content_version: 2
            }
            .nanos()
        );
    }

    /// A second near `i64`'s ends is nonsense, and the only guarantee worth
    /// making is that it does not panic in the middle of a `getattr`. Whether
    /// such an instant is representable is the platform's business — Linux's
    /// `SystemTime` is a `timespec` whose `tv_sec` is itself an `i64`, so it
    /// takes `i64::MIN` happily — so this asserts the absence of a panic and
    /// not a particular answer.
    #[test]
    fn an_absurd_second_does_not_panic() {
        let fallback = SystemTime::UNIX_EPOCH + Duration::from_secs(1000);
        for secs in [i64::MIN, i64::MAX, i64::MIN + 1, i64::MAX - 1] {
            let fa = to_file_attr(
                &file_with(Some(VirtualMtime {
                    secs,
                    content_version: 999_999_999,
                })),
                0,
                0,
                0o444,
                0o555,
                fallback,
            );
            // Representable or not, something came back and the mount is up.
            let _ = fa.mtime;
        }
    }
}
