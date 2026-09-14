//! The backing-file freshness stamp: the identity a `tracks` row records for
//! its backing file, compared on every serve to detect an on-disk change that
//! no database write covers. Strengthened past size + whole-second mtime to
//! nanosecond mtime + ctime (#276) so a same-size in-place rewrite — including
//! an adversarial one that resets mtime — cannot evade the guard, and then with
//! the inode (#674) for backing filesystems with coarse timestamps, where those
//! three can agree across a replacement — wherever the filesystem's inode
//! numbers survive a remount (#757).
use std::os::unix::fs::MetadataExt;
use std::path::Path;

const NANOS_PER_SEC: i64 = 1_000_000_000;

/// `(size, mtime_ns, ctime_ns, ino)` captured from one `fstat`.
/// `mtime_ns`/`ctime_ns` are nanoseconds since the Unix epoch (good until
/// ~2262). `ctime` is the adversarial backstop: a writer can reset mtime with
/// `utimensat`, but ctime is bumped by any write and cannot be set backward.
///
/// `ino` closes a case the other three cannot see (#674): a backing filesystem
/// with coarse timestamps — ext3 and HFS+ keep whole seconds, and some SMB and
/// NFS mounts truncate the nanosecond fields — where a same-size replacement
/// inside the granularity window leaves all three identical. It does not help
/// against a true in-place rewrite, which is a POSIX timestamp limitation rather
/// than something musefs can fix; it catches the *replacement* shape, where a
/// tagger writes a temporary file and renames over the original, which is what
/// almost every tagger does.
///
/// It is recorded only where the filesystem's inode numbers are known to survive
/// a remount (#757; see `keeps_inodes`). FAT and exFAT number a file each time
/// it enters the inode cache, and SMB, FUSE and overlayfs mounts can renumber on
/// a remount. Recording such a number would fail every serve of an untouched file
/// after one, so `BackingStamp::recordable` drops it. There the stamp is size,
/// mtime and ctime — on FAT and exFAT effectively size plus a coarse mtime,
/// two-second steps on FAT and 10 ms on exFAT, both reporting ctime as mtime,
/// which is why neither is recommended as backing storage.
///
/// The device number is absent on purpose (#757). An inode is unique only within
/// one filesystem, so a different filesystem appearing at the backing path — a
/// swapped drive, a replaced network mount — could hold a file agreeing on all
/// four fields. But `st_dev` is assigned at mount or detection time rather than
/// stored by the filesystem: network mounts, FUSE, btrfs subvolumes and
/// renumbered disks come back with a different one after a reboot, which would
/// fail every row at once, while a swapped drive at the same mount point often
/// gets the same one. The coincidence it would catch also needs ctime, which the
/// kernel sets when a file is written and nothing can set backward, to agree, so
/// on filesystems with real timestamps a copy onto new storage never matches.
///
/// `None` means "not recorded", not "no inode": a row written before #674, one
/// V4 migrated, or one on a filesystem that keeps none.
/// [`BackingStamp::matches_live`] is what knows that an
/// unrecorded inode cannot discriminate — which is why `PartialEq` is *not* the
/// freshness question. Equality here is ordinary structural equality, and stays
/// transitive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BackingStamp {
    pub size: u64,
    pub mtime_ns: i64,
    pub ctime_ns: i64,
    pub ino: Option<u64>,
}

impl BackingStamp {
    pub fn from_metadata(meta: &std::fs::Metadata) -> BackingStamp {
        BackingStamp {
            size: meta.len(),
            mtime_ns: meta
                .mtime()
                .saturating_mul(NANOS_PER_SEC)
                .saturating_add(meta.mtime_nsec()),
            ctime_ns: meta
                .ctime()
                .saturating_mul(NANOS_PER_SEC)
                .saturating_add(meta.ctime_nsec()),
            // The `!= 0` guard is for the round trip: this is the value the
            // store will hold, and 0 is how the column spells "not recorded",
            // so a stat reporting 0 must not be written back as a real inode.
            // Linux hands out no inode 0 for a file on an ordinary filesystem,
            // but a FUSE server can return whatever it likes under `use_ino`,
            // so this is not a branch that provably never runs — which is
            // exactly why `matches_live` treats a *live* `None` as a failure
            // rather than as a wildcard.
            ino: (meta.ino() != 0).then_some(meta.ino()),
        }
    }

    pub fn from_track(t: &musefs_db::Track) -> BackingStamp {
        BackingStamp {
            size: t.backing_size,
            mtime_ns: t.backing_mtime_ns,
            ctime_ns: t.backing_ctime_ns,
            ino: t.backing_ino,
        }
    }

    pub fn from_identity(i: &musefs_db::TrackIdentity) -> BackingStamp {
        BackingStamp {
            size: i.backing_size,
            mtime_ns: i.backing_mtime_ns,
            ctime_ns: i.backing_ctime_ns,
            ino: i.backing_ino,
        }
    }

    /// Does `live` — a stamp just taken from the file on disk — describe the
    /// same backing bytes this stored stamp was recorded for?
    ///
    /// Asymmetric on purpose, and deliberately not `==`. The question is not
    /// "are these equal" but "has anything the *stored* side actually knows
    /// about changed". A stored stamp with no recorded inode has nothing to say
    /// about that field, and treating that as a mismatch would fail every row
    /// in a just-migrated store on its first serve.
    ///
    /// The wildcard is one-directional, and only the stored side gets it. A
    /// *live* stat that cannot supply an inode is a failure, not a pass: the
    /// stored side knowing a value the live side cannot produce is a
    /// disagreement about the file, and the whole point of this guard is to
    /// fail closed on anything it cannot rule out. Making the rule symmetric —
    /// `self.ino.zip(live.ino).is_none_or(...)` reads neatly and does exactly
    /// that — would let a replaced file pass on equal size and timestamps
    /// whenever the new stat reported inode 0, which a FUSE server can do.
    ///
    /// Written as a method rather than a `PartialEq` impl because the rule is
    /// not an equivalence relation — a stored stamp with no inode matches two
    /// live stamps that do not match each other — and an `==` that is not
    /// transitive is a trap for the next reader. Fill a missing inode by
    /// running `musefs revalidate`, which re-probes the rows whose inode is
    /// missing — except on a filesystem that keeps none (#757).
    pub fn matches_live(&self, live: &BackingStamp) -> bool {
        self.size == live.size
            && self.mtime_ns == live.mtime_ns
            && self.ctime_ns == live.ctime_ns
            && match self.ino {
                // Nothing recorded: this field cannot decide either way.
                None => true,
                // Recorded: the live stat has to produce the same value. `None`
                // here is a live stat that could not supply one at all, and
                // that is a failure like any other mismatch.
                Some(stored) => live.ino == Some(stored),
            }
    }

    /// Whole-second mtime for the FUSE `getattr` display surface (never the raw
    /// nanosecond value, which would advertise a ~10^18-second timestamp).
    ///
    /// Floors rather than truncates (#696). A pre-epoch backing file carries a
    /// negative stamp, and truncating division rounds toward zero, which for a
    /// negative value rounds the displayed second *up* — 1969-12-31 23:59:58.5
    /// would advertise as 23:59:59. `mtime_ns` is built from a `timespec` whose
    /// `tv_nsec` is non-negative, so the floor is exactly the `st_mtime` the
    /// backing file reports and the round trip through this method is lossless.
    pub fn display_secs(&self) -> i64 {
        self.mtime_ns.div_euclid(NANOS_PER_SEC)
    }

    /// The stamp to *store* for a file, from a live one: the inode survives only
    /// where the filesystem keeps inode numbers at all (#757), as [`keeps_inodes`]
    /// or [`keeps_inodes_at`] answered for the file this stamp was read from.
    ///
    /// Only a stamp being recorded, or compared against one that was, goes
    /// through this. A live stamp checked on the serve path keeps whatever inode
    /// it read: a stored `None` is already the wildcard
    /// [`matches_live`](Self::matches_live) needs.
    #[must_use]
    pub(crate) fn recordable(self, keeps_inodes: bool) -> BackingStamp {
        BackingStamp {
            ino: self.ino.filter(|_| keeps_inodes),
            ..self
        }
    }
}

/// `f_type` values of the Linux filesystems whose inode numbers survive a
/// remount (#757), from `linux/magic.h` unless another source is named.
#[cfg(target_os = "linux")]
const PERSISTENT_INODE_FILESYSTEMS: [u64; 18] = [
    0xEF53,      // EXT4_SUPER_MAGIC, which ext2 and ext3 share
    0x9123_683E, // BTRFS_SUPER_MAGIC
    0x5846_5342, // XFS_SUPER_MAGIC
    0x2FC1_2FC1, // ZFS_SUPER_MAGIC (OpenZFS, include/sys/fs/zfs.h)
    0xF2F5_2010, // F2FS_SUPER_MAGIC
    0xCA45_1A4E, // BCACHEFS_SUPER_MAGIC
    0x3153_464A, // JFS_SUPER_MAGIC (fs/jfs/jfs_superblock.h)
    0x5265_4973, // REISERFS_SUPER_MAGIC
    0x7366_746E, // ntfs3's s_magic (fs/ntfs3/super.c)
    0x5346_544E, // NTFS_SB_MAGIC, the older ntfs driver
    0x482B,      // HFSPLUS_SUPER_MAGIC (fs/hfsplus/hfsplus_raw.h)
    0x0102_1994, // TMPFS_MAGIC
    0x7371_7368, // SQUASHFS_MAGIC
    0xE0F5_E1E2, // EROFS_SUPER_MAGIC_V1
    0x9660,      // ISOFS_SUPER_MAGIC
    0x1501_3346, // UDF_SUPER_MAGIC
    0x6969,      // NFS_SUPER_MAGIC
    0x00C3_6400, // CEPH_SUPER_MAGIC
];

/// The type names, as macOS and FreeBSD report them in `f_fstypename`, of the
/// filesystems whose inode numbers survive a remount (#757): APFS and HFS+
/// (`hfs`) on macOS; UFS, ext2fs and tmpfs on FreeBSD; ZFS and NFS on either.
/// Their SMB clients (`smbfs`), FUSE (`macfuse`, `osxfuse`, `fusefs.*`) and FAT
/// and exFAT drivers (`msdos`, `msdosfs`, `exfat`) are left off for the reasons
/// [`keeps_inodes`] gives.
#[cfg(any(test, target_os = "macos", target_os = "freebsd"))]
const PERSISTENT_INODE_FILESYSTEM_NAMES: [&[u8]; 7] =
    [b"apfs", b"hfs", b"ufs", b"ext2fs", b"tmpfs", b"zfs", b"nfs"];

/// Whether the filesystem holding `file` keeps its inode numbers across a
/// remount, and so whether a stamp recorded for it may carry one (#757).
///
/// An allowlist, because a filesystem that renumbers files is worse than one
/// with no numbers at all. A recorded number that changes after a remount fails
/// every serve of an untouched file with `BackingChanged` until a revalidate
/// rewrites the row — and again after the next remount. FAT and exFAT number a
/// file with `iunique()` each time it enters the inode cache. libfuse without
/// `use_ino` (sshfs's default, `exfat-fuse`, many `rclone` and `s3fs` mounts)
/// reports node ids it hands out again after a remount, and even after the
/// kernel forgets a node. CIFS mounted `noserverino`, which the kernel also
/// falls back to on its own, uses `iunique()` numbers, and so does vboxsf;
/// overlayfs's depend on `xino`. `statfs` cannot see `serverino`, `use_ino` or
/// `xino`, so SMB, FUSE and overlayfs are left off rather than guessed at.
///
/// An absent inode costs far less: detection of a same-size replacement inside
/// the timestamp granularity window, the one case the inode exists for. So a
/// filesystem the list does not name, or one `statfs` cannot answer for,
/// records none. That is the opposite of the choice made while FAT and exFAT
/// were the only filesystems known to renumber, which kept the stronger stamp
/// for an unknown filesystem and so took a remounted FUSE library dark.
///
/// Off Linux the filesystem's type name decides, against the same kind of list,
/// on macOS and FreeBSD (`f_fstypename`); anywhere else nothing names the
/// filesystem, and the answer is no.
pub(crate) fn keeps_inodes(file: &std::fs::File) -> bool {
    #[cfg(test)]
    if NO_INODES.with(std::cell::Cell::get) {
        return false;
    }
    #[cfg(target_os = "linux")]
    {
        f_type_keeps_inodes(fs_type(rustix::fs::fstatfs(file)))
    }
    #[cfg(any(target_os = "macos", target_os = "freebsd"))]
    {
        fs_name(rustix::fs::fstatfs(file)).is_some_and(|name| fs_name_keeps_inodes(&name))
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "freebsd")))]
    {
        let _ = file;
        false
    }
}

/// [`keeps_inodes`] for a pathname, at the call sites that stat a path rather
/// than hold a descriptor. Follows symlinks, as `std::fs::metadata` does.
pub(crate) fn keeps_inodes_at(path: &Path) -> bool {
    #[cfg(test)]
    if NO_INODES.with(std::cell::Cell::get) {
        return false;
    }
    #[cfg(target_os = "linux")]
    {
        f_type_keeps_inodes(fs_type(rustix::fs::statfs(path)))
    }
    #[cfg(any(target_os = "macos", target_os = "freebsd"))]
    {
        fs_name(rustix::fs::statfs(path)).is_some_and(|name| fs_name_keeps_inodes(&name))
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "freebsd")))]
    {
        let _ = path;
        false
    }
}

/// The filesystem type a `statfs` reported, or `None` when it could not say.
#[cfg(target_os = "linux")]
fn fs_type(stat: rustix::io::Result<rustix::fs::StatFs>) -> Option<u64> {
    stat.ok().and_then(|s| u64::try_from(s.f_type).ok())
}

/// The filesystem type name a BSD-family `statfs` reported (`f_fstypename`, up
/// to its NUL), or `None` when it could not say.
#[cfg(any(target_os = "macos", target_os = "freebsd"))]
fn fs_name(stat: rustix::io::Result<rustix::fs::StatFs>) -> Option<Vec<u8>> {
    let stat = stat.ok()?;
    Some(
        stat.f_fstypename
            .iter()
            .take_while(|&&c| c != 0)
            .map(|&c| c.to_ne_bytes()[0])
            .collect(),
    )
}

/// The off-Linux decision, on a bare filesystem type name, so it is testable on
/// any platform. A name the list does not hold records no inode.
#[cfg(any(test, target_os = "macos", target_os = "freebsd"))]
fn fs_name_keeps_inodes(name: &[u8]) -> bool {
    PERSISTENT_INODE_FILESYSTEM_NAMES.contains(&name)
}

/// The decision itself, on a bare `f_type`, so it is testable without mounting
/// each filesystem. An unrecognised type, or none at all, records no inode, for
/// the reason [`keeps_inodes`] gives.
#[cfg(target_os = "linux")]
fn f_type_keeps_inodes(f_type: Option<u64>) -> bool {
    f_type.is_some_and(|t| PERSISTENT_INODE_FILESYSTEMS.contains(&t))
}

/// One pass's answers to [`keeps_inodes`], one per filesystem.
///
/// `statfs` is not cached by an NFS or SMB client, so a query per file was a
/// network round trip per file on exactly the mounts where a scan is slowest.
/// A pass holds one of these and asks each filesystem once, keyed by the
/// `st_dev` of the stat its caller has already taken. A device number stays put
/// for as long as its filesystem is mounted, which outlasts any pass; a
/// filesystem remounted mid-pass comes back under a new number and is simply
/// asked again.
///
/// The lock is held across the query, so probe workers that meet a new
/// filesystem together ask it once between them rather than once each. That
/// happens once per filesystem per pass, so the lock is never held for long.
#[derive(Debug, Default)]
pub(crate) struct InodeKeeping {
    answers: std::sync::Mutex<std::collections::HashMap<u64, bool>>,
    #[cfg(test)]
    queries: std::sync::atomic::AtomicUsize,
}

impl InodeKeeping {
    /// [`keeps_inodes`] for `file`, whose metadata `meta` was just read.
    pub(crate) fn of_file(&self, file: &std::fs::File, meta: &std::fs::Metadata) -> bool {
        self.answer(meta.dev(), || keeps_inodes(file))
    }

    /// [`keeps_inodes_at`] for `path`, whose metadata `meta` was just read.
    pub(crate) fn at_path(&self, path: &Path, meta: &std::fs::Metadata) -> bool {
        self.answer(meta.dev(), || keeps_inodes_at(path))
    }

    fn answer(&self, dev: u64, ask: impl FnOnce() -> bool) -> bool {
        let mut answers = self
            .answers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(&keeps) = answers.get(&dev) {
            return keeps;
        }
        #[cfg(test)]
        self.queries
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let keeps = ask();
        answers.insert(dev, keeps);
        keeps
    }

    /// How many filesystem queries this pass has made.
    #[cfg(test)]
    pub(crate) fn queries(&self) -> usize {
        self.queries.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Test seam: answer `keeps` for device `dev` without asking. Unlike
    /// [`pretend_no_inodes`], this reaches every thread holding the pass —
    /// its probe workers included.
    #[cfg(test)]
    pub(crate) fn pretend(&self, dev: u64, keeps: bool) {
        self.answers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(dev, keeps);
    }
}

/// Test support: whether musefs records inode numbers for files under `path`,
/// decided the way a scan decides it, so a test that depends on the answer can
/// state what it expects on the filesystem it happens to run on — ext4 on one
/// machine, overlayfs in a container — rather than assume one.
///
/// Panics, naming the path, when the filesystem cannot be asked: a test with no
/// answer to go on must say so rather than pass on a guess.
#[cfg(any(test, feature = "test-support"))]
pub fn filesystem_keeps_inodes_for_test(path: &Path) -> bool {
    let unanswerable = |e: rustix::io::Errno| -> ! {
        panic!(
            "cannot statfs {}, so whether its filesystem keeps inode numbers is unknown \
             and a test depending on it has nothing to go on: {e}",
            path.display()
        )
    };
    #[cfg(target_os = "linux")]
    {
        let stat = rustix::fs::statfs(path).unwrap_or_else(|e| unanswerable(e));
        f_type_keeps_inodes(fs_type(Ok(stat)))
    }
    #[cfg(any(target_os = "macos", target_os = "freebsd"))]
    {
        let stat = rustix::fs::statfs(path).unwrap_or_else(|e| unanswerable(e));
        fs_name(Ok(stat)).is_some_and(|name| fs_name_keeps_inodes(&name))
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "freebsd")))]
    {
        let _ = (path, unanswerable);
        false
    }
}

#[cfg(test)]
thread_local! {
    static NO_INODES: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Test seam standing in for a filesystem whose inode numbers are not recorded
/// (FAT, an SMB share, a FUSE mount), which the suite cannot make:
/// while the returned guard lives, this thread's [`keeps_inodes`] and
/// [`keeps_inodes_at`] answer no. Thread-local, so it reaches the calling
/// thread's checks — a direct probe, revalidate's skip pass — and not a scan's
/// worker pool.
#[cfg(test)]
pub(crate) fn pretend_no_inodes() -> NoInodes {
    NO_INODES.with(|c| c.set(true));
    NoInodes(())
}

#[cfg(test)]
pub(crate) struct NoInodes(());

#[cfg(test)]
impl Drop for NoInodes {
    fn drop(&mut self) {
        NO_INODES.with(|c| c.set(false));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::MetadataExt;

    #[test]
    fn from_metadata_captures_ns_and_display_secs() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("f");
        std::fs::write(&p, b"hello").unwrap();
        let meta = std::fs::metadata(&p).unwrap();

        let s = BackingStamp::from_metadata(&meta);
        assert_eq!(s.size, 5);
        // A live stat always knows the inode, which is what lets `matches_live`
        // treat an absent one as the *stored* side having nothing to say (#674).
        assert_eq!(s.ino, Some(meta.ino()));
        assert_eq!(s.mtime_ns, meta.mtime() * 1_000_000_000 + meta.mtime_nsec());
        assert_eq!(s.ctime_ns, meta.ctime() * 1_000_000_000 + meta.ctime_nsec());
        // Display is whole-second mtime, never the raw nanosecond value.
        assert_eq!(s.display_secs(), meta.mtime());
    }

    /// #696: a pre-epoch backing file gives a negative stamp, and the displayed
    /// second must be the floor — the `st_mtime` the file itself reports — not
    /// the truncation toward zero that `/` would give.
    #[test]
    fn display_secs_floors_a_pre_epoch_stamp() {
        let secs_of = |mtime_ns| {
            BackingStamp {
                size: 0,
                mtime_ns,
                ctime_ns: 0,
                ino: None,
            }
            .display_secs()
        };

        // Exactly on a second boundary: floor and truncation agree.
        assert_eq!(secs_of(-2 * NANOS_PER_SEC), -2);
        // Mid-second before the epoch: 1969-12-31 23:59:58.5 is second -2, and
        // truncation would round it up to -1.
        assert_eq!(secs_of(-2 * NANOS_PER_SEC + NANOS_PER_SEC / 2), -2);
        // The last nanosecond before the epoch is still second -1, not 0.
        assert_eq!(secs_of(-1), -1);
        // The epoch itself and the first nanosecond after it are second 0.
        assert_eq!(secs_of(0), 0);
        assert_eq!(secs_of(1), 0);
        // Post-epoch is unchanged: floor and truncation agree for positives.
        assert_eq!(secs_of(NANOS_PER_SEC + NANOS_PER_SEC / 2), 1);
    }

    #[test]
    fn equality_is_field_wise() {
        let a = BackingStamp {
            size: 1,
            mtime_ns: 2,
            ctime_ns: 3,
            ino: None,
        };
        assert_eq!(
            a,
            BackingStamp {
                size: 1,
                mtime_ns: 2,
                ctime_ns: 3,
                ino: None,
            }
        );
        assert_ne!(
            a,
            BackingStamp {
                size: 1,
                mtime_ns: 2,
                ctime_ns: 4,
                ino: None,
            }
        );
    }

    fn stamp(size: u64, ino: Option<u64>) -> BackingStamp {
        BackingStamp {
            size,
            mtime_ns: 2,
            ctime_ns: 3,
            ino,
        }
    }

    /// The case #674 exists for. On a filesystem with no sub-second timestamps
    /// a same-size replacement inside the granularity window leaves size, mtime
    /// and ctime identical — every field the stamp had before the inode.
    #[test]
    fn a_replacement_the_timestamps_cannot_see_is_caught_by_the_inode() {
        let stored = stamp(10, Some(111));
        // Byte-for-byte identical on the three old fields.
        assert_eq!(
            (stored.size, stored.mtime_ns, stored.ctime_ns),
            (stamp(10, Some(222)).size, 2, 3)
        );
        assert!(
            !stored.matches_live(&stamp(10, Some(222))),
            "a fresh inode is a replaced file"
        );
        assert!(stored.matches_live(&stamp(10, Some(111))));
    }

    /// An unrecorded inode is a field with nothing to say, not a mismatch.
    /// Without this every row in a just-migrated store would fail its first
    /// serve — the store carries the 0 sentinel until a revalidate fills it in.
    #[test]
    fn an_unrecorded_inode_does_not_fail_the_stamp() {
        assert!(stamp(10, None).matches_live(&stamp(10, Some(999))));
        // And it is still only the inode that is excused: the other three
        // fields decide as they always did.
        assert!(!stamp(11, None).matches_live(&stamp(10, Some(999))));
    }

    /// Each of the four fields must be able to refuse on its own — an `||`
    /// slipped into the chain would let three agreeing fields vouch for a
    /// fourth that does not.
    #[test]
    fn every_field_can_refuse_alone() {
        let stored = BackingStamp {
            size: 1,
            mtime_ns: 2,
            ctime_ns: 3,
            ino: Some(4),
        };
        assert!(stored.matches_live(&stored));
        for (what, live) in [
            ("size", BackingStamp { size: 9, ..stored }),
            (
                "mtime",
                BackingStamp {
                    mtime_ns: 9,
                    ..stored
                },
            ),
            (
                "ctime",
                BackingStamp {
                    ctime_ns: 9,
                    ..stored
                },
            ),
            (
                "inode",
                BackingStamp {
                    ino: Some(9),
                    ..stored
                },
            ),
        ] {
            assert!(
                !stored.matches_live(&live),
                "a changed {what} must fail the stamp on its own"
            );
        }
    }

    /// The wildcard belongs to the stored side alone. A live stat that cannot
    /// supply an inode — `st_ino == 0`, which a FUSE server can return under
    /// `use_ino` — must not excuse a stored inode that is known: equal size and
    /// timestamps would then let a replaced file serve different backing bytes,
    /// which is the exact failure #674 exists to close.
    #[test]
    fn a_live_stat_with_no_inode_does_not_excuse_a_recorded_one() {
        assert!(
            !stamp(10, Some(111)).matches_live(&stamp(10, None)),
            "a live stat that cannot produce the recorded inode fails closed"
        );
        // The stored-side wildcard is untouched, so the two directions really
        // are different rather than both being excused.
        assert!(stamp(10, None).matches_live(&stamp(10, Some(111))));
    }

    /// Why this is a method and not a `PartialEq` impl: the sentinel rule is
    /// not transitive, so spelling it `==` would hand the next reader an
    /// equality that breaks the `Eq` contract.
    #[test]
    fn the_sentinel_rule_is_not_an_equivalence_relation() {
        let unrecorded = stamp(10, None);
        let one = stamp(10, Some(1));
        let two = stamp(10, Some(2));
        assert!(unrecorded.matches_live(&one));
        assert!(unrecorded.matches_live(&two));
        assert!(
            !one.matches_live(&two),
            "transitivity would demand these match; they must not"
        );
        // Structural equality, meanwhile, stays honest about all four fields.
        assert_ne!(unrecorded, one);
    }

    /// The allowlist by `f_type`: the filesystems whose inode numbers survive a
    /// remount get one recorded, and every other — FAT and exFAT, which number
    /// files as they enter the inode cache; SMB, FUSE, 9p, vboxsf and overlayfs,
    /// whose numbers depend on mount options `statfs` cannot see; and anything
    /// unrecognised — gets none.
    #[cfg(target_os = "linux")]
    #[test]
    fn only_filesystems_with_persistent_inode_numbers_get_one_recorded() {
        let keeps = |f_type: u64| f_type_keeps_inodes(Some(f_type));
        for (name, magic) in [
            ("ext2/3/4", 0xEF53),
            ("btrfs", 0x9123_683E),
            ("xfs", 0x5846_5342),
            ("zfs", 0x2FC1_2FC1),
            ("f2fs", 0xF2F5_2010),
            ("bcachefs", 0xCA45_1A4E),
            ("jfs", 0x3153_464A),
            ("reiserfs", 0x5265_4973),
            ("nfs", 0x6969),
            ("ntfs3", 0x7366_746E),
            ("ntfs", 0x5346_544E),
            ("hfsplus", 0x482B),
            ("tmpfs", 0x0102_1994),
            ("squashfs", 0x7371_7368),
            ("erofs", 0xE0F5_E1E2),
            ("iso9660", 0x9660),
            ("udf", 0x1501_3346),
            ("ceph", 0x00C3_6400),
        ] {
            assert!(keeps(magic), "{name} keeps its inode numbers");
        }
        for (name, magic) in [
            ("FAT", 0x4D44),
            ("exFAT", 0x2011_BAB0),
            ("SMB", 0x517B),
            ("CIFS", 0xFF53_4D42),
            ("SMB2", 0xFE53_4D42),
            ("FUSE", 0x6573_5546),
            ("overlayfs", 0x794C_7630),
            ("9p", 0x0102_1997),
            ("vboxsf", 0x786F_4256),
            ("procfs", 0x9FA0),
            ("unrecognised", 0x1234_5678),
        ] {
            assert!(!keeps(magic), "{name} gets no inode recorded");
        }
        assert!(
            !f_type_keeps_inodes(None),
            "a filesystem that cannot say gets no inode recorded"
        );
    }

    /// Off Linux the filesystem's name decides, against the same kind of
    /// allowlist: macOS's and FreeBSD's own disk filesystems and NFS keep their
    /// numbers, while their SMB clients, FUSE mounts and FAT/exFAT drivers do not
    /// promise to.
    #[test]
    fn off_linux_the_filesystem_name_decides() {
        for name in ["apfs", "hfs", "ufs", "zfs", "tmpfs", "nfs", "ext2fs"] {
            assert!(fs_name_keeps_inodes(name.as_bytes()), "{name}");
        }
        for name in [
            "smbfs",
            "msdos",
            "msdosfs",
            "exfat",
            "macfuse",
            "osxfuse",
            "fusefs.sshfs",
            "apfsx",
            "",
        ] {
            assert!(!fs_name_keeps_inodes(name.as_bytes()), "{name:?}");
        }
    }

    /// A pass asks each filesystem once, however many files it meets there.
    #[test]
    fn a_pass_asks_each_device_once() {
        let pass = InodeKeeping::default();
        let asked = std::cell::Cell::new(0);
        let ask = |keeps| {
            asked.set(asked.get() + 1);
            keeps
        };
        assert!(pass.answer(1, || ask(true)));
        assert!(pass.answer(1, || ask(false)), "the first answer stands");
        assert!(!pass.answer(2, || ask(false)));
        assert!(!pass.answer(2, || ask(true)));
        assert_eq!(asked.get(), 2, "one query per device");
        assert_eq!(pass.queries(), 2);

        pass.pretend(3, false);
        assert!(
            !pass.answer(3, || ask(true)),
            "a pretended answer is not asked"
        );
        assert_eq!(pass.queries(), 2);
    }

    /// The one assertion that pins `fs_type`'s decoding. Every other test either
    /// hands `f_type_keeps_inodes` a value directly or derives its expectation
    /// through `fs_type` itself, so a `fs_type` answering the wrong number would
    /// pass them all.
    /// `/proc` is procfs on every Linux system and container, so its magic is
    /// a value known in advance.
    #[cfg(target_os = "linux")]
    #[test]
    fn fs_type_decodes_the_magic_statfs_reports() {
        /// `PROC_SUPER_MAGIC` from `linux/magic.h`.
        const PROC_SUPER_MAGIC: u64 = 0x9fa0;
        assert_eq!(fs_type(rustix::fs::statfs("/proc")), Some(PROC_SUPER_MAGIC));
    }

    /// The descriptor and the path ask the same question and get the answer the
    /// allowlist gives for the filesystem this test runs on, whichever that is.
    #[test]
    fn a_real_filesystem_is_asked_by_descriptor_and_by_path() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("f");
        std::fs::write(&p, b"x").unwrap();
        #[cfg(target_os = "linux")]
        assert!(
            fs_type(rustix::fs::statfs(dir.path())).is_some(),
            "statfs reports a type"
        );
        let expected = filesystem_keeps_inodes_for_test(dir.path());
        assert_eq!(keeps_inodes(&std::fs::File::open(&p).unwrap()), expected);
        assert_eq!(keeps_inodes_at(&p), expected);
        assert!(
            !keeps_inodes_at(&dir.path().join("missing")),
            "a query that fails records no inode"
        );
    }

    #[test]
    fn the_test_seam_answers_no_while_held_and_yes_after() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("f");
        std::fs::write(&p, b"x").unwrap();
        {
            let _fat = pretend_no_inodes();
            assert!(!keeps_inodes(&std::fs::File::open(&p).unwrap()));
            assert!(!keeps_inodes_at(&p));
        }
        assert!(keeps_inodes_at(&p));
    }

    #[test]
    fn recordable_drops_the_inode_and_nothing_else() {
        let live = BackingStamp {
            size: 1,
            mtime_ns: 2,
            ctime_ns: 3,
            ino: Some(4),
        };
        assert_eq!(live.recordable(true), live);
        assert_eq!(live.recordable(false), BackingStamp { ino: None, ..live });
    }

    /// The test-support decision gives a real no as well as a real yes. Every
    /// test that derives its expectation through it runs on whatever the suite's
    /// tempdir is — often a filesystem that keeps inode numbers — so a helper
    /// that always answered yes would pass them all while telling a container's
    /// overlayfs run the wrong thing. `/proc` is procfs on every Linux system and
    /// container, and procfs is not on the allowlist.
    #[cfg(target_os = "linux")]
    #[test]
    fn the_test_support_decision_answers_no_for_a_filesystem_off_the_allowlist() {
        assert!(!filesystem_keeps_inodes_for_test(Path::new("/proc")));
    }

    /// The failure #757 fixes. FAT hands an untouched file a new inode after a
    /// remount: recorded with its inode, the file failed every serve until a
    /// revalidate, while recorded without one it still matches.
    #[test]
    fn a_stamp_recorded_without_the_inode_survives_a_renumbering() {
        let before = stamp(10, Some(111));
        let after_remount = stamp(10, Some(222));
        assert!(!before.matches_live(&after_remount));
        assert!(before.recordable(false).matches_live(&after_remount));
    }
}
