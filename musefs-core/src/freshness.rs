//! The backing-file freshness stamp: the identity a `tracks` row records for
//! its backing file, compared on every serve to detect an on-disk change that
//! no database write covers. Strengthened past size + whole-second mtime to
//! nanosecond mtime + ctime (#276) so a same-size in-place rewrite — including
//! an adversarial one that resets mtime — cannot evade the guard.
use std::os::unix::fs::MetadataExt;

const NANOS_PER_SEC: i64 = 1_000_000_000;

/// `(size, mtime_ns, ctime_ns)` captured from one `fstat`. `mtime_ns`/`ctime_ns`
/// are nanoseconds since the Unix epoch (good until ~2262). `ctime` is the
/// adversarial backstop: a writer can reset mtime with `utimensat`, but ctime
/// is bumped by any write and cannot be set backward.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BackingStamp {
    pub size: u64,
    pub mtime_ns: i64,
    pub ctime_ns: i64,
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
        }
    }

    pub fn from_track(t: &musefs_db::Track) -> BackingStamp {
        BackingStamp {
            size: t.backing_size,
            mtime_ns: t.backing_mtime_ns,
            ctime_ns: t.backing_ctime_ns,
        }
    }

    pub fn from_identity(i: &musefs_db::TrackIdentity) -> BackingStamp {
        BackingStamp {
            size: i.backing_size,
            mtime_ns: i.backing_mtime_ns,
            ctime_ns: i.backing_ctime_ns,
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
        };
        assert_eq!(
            a,
            BackingStamp {
                size: 1,
                mtime_ns: 2,
                ctime_ns: 3
            }
        );
        assert_ne!(
            a,
            BackingStamp {
                size: 1,
                mtime_ns: 2,
                ctime_ns: 4
            }
        );
    }
}
