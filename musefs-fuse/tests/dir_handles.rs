//! E2E: a saturated dir-handle table must not cost a client any directory (#616).
//!
//! The cap (#307) bounds how many `opendir` snapshots can be pinned at once. A
//! parallel walker (`bfs` and friends) blows past it on a large mount, and when
//! an over-cap `opendir` failed with `ENFILE` those directories simply vanished
//! from the walk. This pins the whole table with held directory fds and then
//! walks the tree through it: every listing must still be complete, served by
//! `readdir`'s stateless fallback, and the rejections must show up in the
//! telemetry counter (#626) rather than as lost files.
//!
//! Run with:
//!   cargo test -p musefs-fuse --test dir_handles -- --ignored --nocapture

use std::fs::File;
use std::path::Path;

use musefs_core::{Musefs, scan_directory};
use musefs_fuse::FuseConfig;

mod common;
use common::{config, make_flac};

/// `MAX_DIR_HANDLES` from `src/lib.rs`, which an integration test cannot name.
/// The `musefs_dir_handles_max` assertion below fails loudly if the two drift.
const CAP: usize = 1024;
/// Directories to render: enough to pin the whole table and still have some left
/// over to enumerate over-cap.
const DIRS: usize = CAP + 16;
/// File descriptors the test itself needs: one per pinned directory, plus room
/// for the daemon's own backing files and the harness.
const NEEDED_FDS: u64 = CAP as u64 + 256;

fn artist(i: usize) -> String {
    format!("Artist{i:04}")
}

/// Raise `RLIMIT_NOFILE` to `needed` if it is not already there. Returns false
/// when the hard limit forbids it, in which case the test skips rather than
/// failing on an environment it cannot control.
fn raise_nofile(needed: u64) -> bool {
    use rustix::process::{Resource, Rlimit, getrlimit, setrlimit};
    let limit = getrlimit(Resource::Nofile);
    if limit.current.is_none_or(|cur| cur >= needed) {
        return true;
    }
    if limit.maximum.is_some_and(|max| max < needed) {
        return false;
    }
    setrlimit(
        Resource::Nofile,
        Rlimit {
            current: Some(needed),
            maximum: limit.maximum,
        },
    )
    .is_ok()
}

/// Every `.flac` under `dir`, recursively, propagating enumeration errors — an
/// `ENFILE` from `opendir` must fail the test, not silently shrink the count.
fn count_flacs(dir: &Path) -> usize {
    let mut n = 0;
    let entries = std::fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("read_dir({}) failed: {e}", dir.display()));
    for entry in entries {
        let entry = entry.unwrap_or_else(|e| panic!("dir entry in {}: {e}", dir.display()));
        let path = entry.path();
        if entry.file_type().unwrap().is_dir() {
            n += count_flacs(&path);
        } else if path.extension().is_some_and(|e| e == "flac") {
            n += 1;
        }
    }
    n
}

/// The value of a single-sample Prometheus metric in `text`.
fn metric(text: &str, name: &str) -> u64 {
    let line = text
        .lines()
        .find(|l| l.starts_with(name) && l.as_bytes().get(name.len()) == Some(&b' '))
        .unwrap_or_else(|| panic!("{name} missing from the metrics body"));
    line[name.len() + 1..].trim().parse().unwrap()
}

fn read_metrics(mountpoint: &Path) -> String {
    use std::io::Read;
    // `/proc`-style: st_size is 0, so read to EOF rather than trusting it.
    let mut f = File::open(mountpoint.join(".musefs-metrics").join("metrics")).unwrap();
    let mut buf = Vec::new();
    loop {
        let prev = buf.len();
        buf.resize(prev + 8192, 0);
        let n = f.read(&mut buf[prev..]).unwrap();
        buf.truncate(prev + n);
        if n == 0 {
            break;
        }
    }
    String::from_utf8(buf).unwrap()
}

#[test]
#[ignore = "requires /dev/fuse + libfuse; run with --ignored"]
fn over_cap_opendir_still_lists_every_directory() {
    if !raise_nofile(NEEDED_FDS) {
        eprintln!("skipping: RLIMIT_NOFILE cannot reach {NEEDED_FDS}");
        return;
    }

    // One track per artist, so the template renders one directory per artist.
    let backing = tempfile::tempdir().unwrap();
    for i in 0..DIRS {
        let flac = make_flac(
            &[&format!("ARTIST={}", artist(i)), "TITLE=Song"],
            &[0xAAu8; 32],
        );
        std::fs::write(backing.path().join(format!("{i:04}.flac")), &flac).unwrap();
    }
    let db = musefs_db::Db::open_in_memory().unwrap();
    scan_directory(&db, backing.path()).unwrap();
    let core = Musefs::open(db, config()).unwrap();

    let mountpoint = tempfile::tempdir().unwrap();
    let session = musefs_fuse::spawn_with(
        core,
        mountpoint.path(),
        "musefs-dir-handles-e2e",
        FuseConfig {
            expose_metrics: true,
            ..FuseConfig::default()
        },
    )
    .unwrap();
    let root = mountpoint.path();

    // Baseline: an unsaturated walk sees everything.
    assert_eq!(count_flacs(root), DIRS, "baseline walk must be complete");

    // Pin the whole table: each held directory fd is one live opendir snapshot.
    let held: Vec<File> = (0..CAP)
        .map(|i| {
            File::open(root.join(artist(i)))
                .unwrap_or_else(|e| panic!("opendir #{i} (at or under the cap) failed: {e}"))
        })
        .collect();

    let saturated = read_metrics(root);
    assert_eq!(
        metric(&saturated, "musefs_dir_handles_max"),
        CAP as u64,
        "CAP here must track MAX_DIR_HANDLES in src/lib.rs"
    );
    assert_eq!(
        metric(&saturated, "musefs_dir_handles"),
        CAP as u64,
        "the held fds must have pinned the whole table"
    );
    let before = metric(&saturated, "musefs_dir_handle_rejections_total");

    // Every `opendir` from here on is over the cap. Pre-#616 each one replied
    // ENFILE and the directory dropped out of the walk entirely.
    assert_eq!(
        count_flacs(root),
        DIRS,
        "a saturated dir-handle table must not lose directories"
    );

    let after = read_metrics(root);
    assert!(
        metric(&after, "musefs_dir_handle_rejections_total") > before,
        "the degraded path must be visible in musefs_dir_handle_rejections_total (#626)"
    );

    drop(held);
    drop(session);
}
