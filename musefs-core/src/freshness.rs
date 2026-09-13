//! The backing-file freshness stamp: the identity a `tracks` row records for
//! its backing file, compared on every serve to detect an on-disk change that
//! no database write covers. Strengthened past size + whole-second mtime to
//! nanosecond mtime + ctime (#276) so a same-size in-place rewrite — including
//! an adversarial one that resets mtime — cannot evade the guard, and then with
//! the inode (#674) for backing filesystems that store no sub-second timestamps
//! at all, where those three can agree across a replacement.
use std::os::unix::fs::MetadataExt;

const NANOS_PER_SEC: i64 = 1_000_000_000;

/// `(size, mtime_ns, ctime_ns, ino)` captured from one `fstat`.
/// `mtime_ns`/`ctime_ns` are nanoseconds since the Unix epoch (good until
/// ~2262). `ctime` is the adversarial backstop: a writer can reset mtime with
/// `utimensat`, but ctime is bumped by any write and cannot be set backward.
///
/// `ino` closes the one case the other three cannot see (#674): a backing
/// filesystem with no sub-second timestamps — FAT32's two-second mtime and no
/// ctime at all, or ext3/HFS+/some SMB and NFS mounts truncating the nanosecond
/// fields — where a same-size replacement inside the granularity window leaves
/// all three identical. It does not help against a true in-place rewrite, which
/// is a POSIX timestamp limitation rather than something musefs can fix; it
/// catches the *replacement* shape, where a tagger writes a temporary file and
/// renames over the original, which is what almost every tagger does.
///
/// `None` means "not recorded", not "no inode": a row written before #674, or
/// one V4 migrated. [`BackingStamp::matches_live`] is what knows that an
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
            // A live stat always knows the inode. The `!= 0` guard is not for
            // Linux, which never hands out inode 0 for a file, but for the
            // round trip: this is the value the store will hold, and 0 is how
            // the column spells "not recorded".
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
    /// Asymmetric on purpose, and deliberately not `==`. The stored side can
    /// have an unrecorded inode and the live side never does, so the question
    /// is not "are these equal" but "has anything the stored side actually
    /// knows about changed". An unrecorded inode is not a mismatch: it is a
    /// field with nothing to say, and treating it as one would fail every row
    /// in a just-migrated store on its first serve.
    ///
    /// Written as a method rather than a `PartialEq` impl because the sentinel
    /// rule is not an equivalence relation — a stored stamp with no inode
    /// matches two live stamps that do not match each other — and an `==` that
    /// is not transitive is a trap for the next reader. Fill the gap by
    /// running `musefs scan --revalidate`, which re-probes exactly the rows
    /// whose inode is missing.
    pub fn matches_live(&self, live: &BackingStamp) -> bool {
        self.size == live.size
            && self.mtime_ns == live.mtime_ns
            && self.ctime_ns == live.ctime_ns
            // `zip` is the whole sentinel rule: absent on either side yields
            // `None`, which is not a disagreement.
            && self.ino.zip(live.ino).is_none_or(|(a, b)| a == b)
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
}
