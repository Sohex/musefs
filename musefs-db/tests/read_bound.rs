//! What a `backing_path` reader materializes from a hostile row, measured
//! rather than inferred (#758).
//!
//! Alone in its own test binary on purpose: SQLite's memory high-water mark is
//! process-wide, and any test running beside this one would move it.

use musefs_db::{Db, sqlite_memory_highwater_for_test, sqlite_memory_used};

/// Well past the path cap and well under the length limit, so what refuses the
/// row is the projection rather than SQLite's backstop.
const HOSTILE: i64 = 12 * 1024 * 1024;

/// How much SQLite may grow while refusing the row. A first read fills the
/// connection's page cache whatever it projects — up to about 2 MiB on a
/// `Db::open` connection, and 512 KiB on the read-only ones the mount serves
/// from — so the bound sits above that and far below the hostile value, which
/// the readers used to load whole: 12.6 MiB for the BLOB, measured.
const MAX_GROWTH: u64 = 3 * 1024 * 1024;

/// Every reader that loads a `backing_path`. `get_track_by_path` is not among
/// them: its lookup compares against the path held in the index, which SQLite
/// loads whatever is projected.
const READERS: [&str; 5] = [
    "get_track",
    "list_tracks",
    "tracks_by_fingerprint",
    "track_identity",
    "list_backing_paths",
];

#[test]
fn a_hostile_backing_path_is_refused_without_being_loaded() {
    let fingerprint = "a".repeat(64);
    for (what, path_sql) in [
        ("a BLOB over the cap", "zeroblob(?1)"),
        (
            // `length()` stops at the NUL and reads 1, under the cap.
            "a NUL-truncated TEXT path",
            "'/' || char(0) || substr(replace(hex(zeroblob(?1)), '0', 'a'), 1, ?1)",
        ),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let store = dir.path().join("s.db");
        let writer = Db::open(&store).unwrap();
        {
            let raw = rusqlite::Connection::open(&store).unwrap();
            raw.pragma_update(None, "ignore_check_constraints", true)
                .unwrap();
            raw.execute(
                &format!(
                    "INSERT INTO tracks (backing_path, format, audio_offset, audio_length, \
                     backing_size, backing_mtime_ns, updated_at, fingerprint) \
                     VALUES ({path_sql}, 'flac', 0, 0, 0, 0, 0, ?2)"
                ),
                rusqlite::params![HOSTILE, fingerprint],
            )
            .unwrap();
        }
        // The read-only connection the mount's workers read through, except for
        // `tracks_by_fingerprint`, which only a writable store has.
        let reader = Db::open_readonly(&store).unwrap();

        for name in READERS {
            sqlite_memory_highwater_for_test(true);
            let baseline = sqlite_memory_used();
            let refused = match name {
                "get_track" => reader.get_track(1).is_err(),
                "list_tracks" => reader.list_tracks().is_err(),
                "tracks_by_fingerprint" => writer.tracks_by_fingerprint(&fingerprint).is_err(),
                "track_identity" => reader.track_identity(1).is_err(),
                "list_backing_paths" => reader.list_backing_paths().is_err(),
                other => unreachable!("no reader named {other}"),
            };
            let grew = sqlite_memory_highwater_for_test(false).saturating_sub(baseline);
            assert!(refused, "{what}: {name} must refuse the row");
            assert!(
                grew < MAX_GROWTH,
                "{what}: {name} made SQLite hold {grew} more bytes to refuse it"
            );
        }
    }
}
