//! E2E: a track whose rendered root component is a name the FUSE layer injects
//! at the mount root (#681).
//!
//! Its own test binary, deliberately. The `.musefs-metrics` body carries
//! process-global gauges (resident set size, and the syscall counters under the
//! `metrics` feature), so a second test mounting and reading in the same process
//! perturbs what `metrics_e2e` reads — see the note at the top of that file.
//!
//! Run with:
//!   cargo test -p musefs-fuse --test reserved_root_names -- --ignored

use musefs_core::{Musefs, scan_directory};
use musefs_fuse::FuseConfig;

mod common;
use common::{config, make_flac};

fn fuse_config() -> FuseConfig {
    let mut fuse_config = FuseConfig::default();
    fuse_config.expose_metrics = true;
    fuse_config
}

/// #681: a track whose rendered root component is the synthetic name. The
/// reservation in the virtual tree pushes it to `.musefs-metrics (2)`, so
/// `readdir` and `lookup` agree and the user's subtree stays reachable — before
/// the fix the name appeared twice with different inodes and the tracks under it
/// could not be reached at all.
#[test]
#[ignore = "requires /dev/fuse + libfuse; run with --ignored"]
fn a_track_rendering_to_the_metrics_name_stays_reachable() {
    use std::os::unix::fs::{DirEntryExt, MetadataExt};

    let backing = tempfile::tempdir().unwrap();
    let audio_bytes: Vec<u8> = (0..=255).cycle().take(256).collect();
    let flac = make_flac(&["ARTIST=.musefs-metrics", "TITLE=Song"], &audio_bytes);
    std::fs::write(backing.path().join("a.flac"), &flac).unwrap();
    let db = musefs_db::Db::open_in_memory().unwrap();
    scan_directory(&db, backing.path()).unwrap();
    let fs = Musefs::open(db, config()).unwrap();

    let mountpoint = tempfile::tempdir().unwrap();
    let session =
        musefs_fuse::spawn_with(fs, mountpoint.path(), "musefs-681-e2e", fuse_config()).unwrap();

    // readdir names are unique, and the synthetic entry keeps the base name.
    let rows: Vec<(String, u64)> = std::fs::read_dir(mountpoint.path())
        .unwrap()
        .map(|e| {
            let e = e.unwrap();
            (e.file_name().into_string().unwrap(), e.ino())
        })
        .collect();
    let mut names: Vec<&str> = rows.iter().map(|(n, _)| n.as_str()).collect();
    names.sort_unstable();
    let mut unique = names.clone();
    unique.dedup();
    assert_eq!(
        names, unique,
        "readdir must not list any name twice, got: {names:?}"
    );
    // Only the metrics names: macOS also lists the Spotlight marker at the root,
    // and this test is about the entry the track collides with.
    let metrics: Vec<&str> = names
        .iter()
        .copied()
        .filter(|n| n.starts_with(".musefs-metrics"))
        .collect();
    assert_eq!(
        metrics,
        vec![".musefs-metrics", ".musefs-metrics (2)"],
        "the real entry must be pushed off the synthetic name"
    );

    // Every listed inode is the one `lookup` resolves the same name to.
    for (name, ino) in &rows {
        let stat = std::fs::metadata(mountpoint.path().join(name)).unwrap();
        assert_eq!(
            stat.ino(),
            *ino,
            "readdir and lookup disagree on the inode of {name}"
        );
    }

    // Both surfaces work: telemetry under the synthetic name, the track under the
    // pushed one.
    assert!(
        mountpoint.path().join(".musefs-metrics/metrics").exists(),
        "the telemetry file must still be served"
    );
    let song = mountpoint
        .path()
        .join(".musefs-metrics (2)")
        .join("Song.flac");
    assert!(song.is_file(), "the track must be reachable at {song:?}");
    assert!(!std::fs::read(&song).unwrap().is_empty());

    drop(session);
}
