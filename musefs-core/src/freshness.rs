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
pub(crate) struct BackingStamp {
    pub size: u64,
    pub mtime_ns: i64,
    pub ctime_ns: i64,
}

#[allow(dead_code)]
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

    /// Whole-second mtime for the FUSE `getattr` display surface (never the raw
    /// nanosecond value, which would advertise a ~10^18-second timestamp).
    pub fn display_secs(&self) -> i64 {
        self.mtime_ns / NANOS_PER_SEC
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
