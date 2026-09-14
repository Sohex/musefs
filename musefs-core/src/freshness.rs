//! The backing-file freshness stamp: the identity a `tracks` row records for
//! its backing file, compared on every serve to detect an on-disk change that
//! no database write covers. Strengthened past size + whole-second mtime to
//! nanosecond mtime + ctime (#276) so a same-size in-place rewrite — including
//! an adversarial one that resets mtime — cannot evade the guard, and then with
//! the inode (#674) for backing filesystems with coarse timestamps, where those
//! three can agree across a replacement — wherever the filesystem keeps an inode
//! number to record (#757).
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
/// It is recorded only where the filesystem keeps one (#757). FAT and exFAT
/// store no inode numbers: Linux hands one out each time a file enters the inode
/// cache, so an untouched file reports a different number after a remount, or
/// after eviction. Recording it there would fail every serve after a replug, so
/// `BackingStamp::recordable` drops it, and on those filesystems the stamp is
/// effectively size plus a coarse mtime — two-second steps on FAT, 10 ms on
/// exFAT, and both report ctime as mtime. That is why neither is recommended as
/// backing storage.
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

/// `f_type` values from `linux/magic.h` for the filesystems that keep no inode
/// numbers (#757).
#[cfg(target_os = "linux")]
const MSDOS_SUPER_MAGIC: u64 = 0x4d44;
#[cfg(target_os = "linux")]
const EXFAT_SUPER_MAGIC: u64 = 0x2011_BAB0;

/// Whether the filesystem holding `file` keeps inode numbers, and so whether a
/// stamp recorded for it may carry one (#757).
///
/// FAT and exFAT do not. Both Linux drivers assign a number with `iunique()`
/// each time a file enters the inode cache, so an untouched file reports a
/// different one after a remount, and within a mount after eviction. Recording
/// it would fail the stamp of a file that never changed.
///
/// A question `fstatfs` cannot answer is answered yes. That records the inode,
/// as the stamp always did, so a failed query keeps the stronger stamp rather
/// than quietly weakening it.
///
/// Off Linux nothing is asked and the answer is always yes: `f_type` holds a
/// Linux magic number only on Linux, so elsewhere no value in it could name FAT.
pub(crate) fn keeps_inodes(file: &std::fs::File) -> bool {
    #[cfg(test)]
    if NO_INODES.with(std::cell::Cell::get) {
        return false;
    }
    #[cfg(target_os = "linux")]
    {
        f_type_keeps_inodes(fs_type(rustix::fs::fstatfs(file)))
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = file;
        true
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
    #[cfg(not(target_os = "linux"))]
    {
        let _ = path;
        true
    }
}

/// The filesystem type a `statfs` reported, or `None` when it could not say.
#[cfg(target_os = "linux")]
fn fs_type(stat: rustix::io::Result<rustix::fs::StatFs>) -> Option<u64> {
    stat.ok().and_then(|s| u64::try_from(s.f_type).ok())
}

/// The decision itself, on a bare `f_type`, so it is testable without a FAT
/// mount. An unknown type keeps inodes, for the reason [`keeps_inodes`] gives.
#[cfg(target_os = "linux")]
fn f_type_keeps_inodes(f_type: Option<u64>) -> bool {
    !matches!(f_type, Some(MSDOS_SUPER_MAGIC | EXFAT_SUPER_MAGIC))
}

#[cfg(test)]
thread_local! {
    static NO_INODES: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Test seam standing in for a FAT or exFAT mount, which the suite cannot make:
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

    /// #757's two filesystems by their `linux/magic.h` values, against a spread
    /// of ones that do keep inode numbers — local, network and FUSE alike.
    #[cfg(target_os = "linux")]
    #[test]
    fn fat_and_exfat_are_the_filesystems_that_keep_no_inodes() {
        let keeps = |f_type: u64| f_type_keeps_inodes(Some(f_type));
        assert!(!keeps(0x4d44), "FAT");
        assert!(!keeps(0x2011_BAB0), "exFAT");
        for (name, magic) in [
            ("ext4", 0xEF53),
            ("btrfs", 0x9123_683E),
            ("xfs", 0x5846_5342),
            ("tmpfs", 0x0102_1994),
            ("nfs", 0x6969),
            ("smb2", 0xFE53_4D42),
            ("fuse", 0x6573_5546),
        ] {
            assert!(keeps(magic), "{name}");
        }
        assert!(
            f_type_keeps_inodes(None),
            "a filesystem that cannot say keeps the stronger stamp"
        );
    }

    /// Answered yes on every platform for an ordinary filesystem: on Linux by
    /// asking it, and elsewhere without asking at all.
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
        assert!(keeps_inodes(&std::fs::File::open(&p).unwrap()));
        assert!(keeps_inodes_at(&p));
        assert!(
            keeps_inodes_at(&dir.path().join("missing")),
            "a query that fails keeps the stronger stamp"
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
