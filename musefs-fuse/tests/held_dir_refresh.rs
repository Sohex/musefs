//! E2E: a directory handle held across a refresh, rewound and read again with
//! `readdirplus`, cannot point a name at another track.
//!
//! An open directory handle keeps serving the listing it was opened on (#675),
//! so after a refresh a rewind sends the old generation's names and inodes
//! again, now with attrs and the full entry TTL. If a name had moved to another
//! track, and the old listing still paired it with the inode of the track that
//! used to hold it, `open` of that name would reach the wrong track for as long
//! as the entry is cached.
//!
//! It cannot, because of how the virtual tree numbers nodes: an inode is keyed by
//! the rendered path and never reissued once retired. A name that moves between
//! tracks keeps its inode, which then resolves to whichever track holds the name
//! now, so the old listing's pairs are the new generation's own. This drives the
//! case that would break that, a name taken from one track by another, and
//! compares the mount that re-read its old listing with one that did not.
//!
//! Run with:
//!   cargo test -p musefs-fuse --test held_dir_refresh -- --ignored --nocapture

use std::fs::File;
use std::os::fd::OwnedFd;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::time::Duration;

use musefs_core::{Musefs, scan_directory};
use musefs_fuse::FuseConfig;

mod common;
use common::{config, make_flac};

/// The value of a single-sample Prometheus metric in `text`.
fn metric(text: &str, name: &str) -> u64 {
    let line = text
        .lines()
        .find(|l| l.starts_with(name) && l.as_bytes().get(name.len()) == Some(&b' '))
        .unwrap_or_else(|| panic!("{name} missing from the metrics body"));
    line[name.len() + 1..].trim().parse().unwrap()
}

fn readdirplus_calls(mountpoint: &Path) -> u64 {
    // `/proc`-style: st_size is 0, so read to EOF rather than trusting it.
    let bytes = std::fs::read(mountpoint.join(".musefs-metrics").join("metrics")).unwrap();
    metric(
        &String::from_utf8(bytes).unwrap(),
        "musefs_readdirplus_total",
    )
}

/// Every name `dir` yields from its current position to the end, `.` and `..`
/// left out, sorted.
fn read_names(dir: &mut rustix::fs::Dir) -> Vec<String> {
    let mut names = Vec::new();
    while let Some(entry) = dir.read() {
        let entry = entry.expect("reading the held directory must not fail");
        let name = entry.file_name().to_str().unwrap().to_owned();
        if name != "." && name != ".." {
            names.push(name);
        }
    }
    names.sort();
    names
}

/// Set a track's tags to artist "Art" and `title`, through a connection of its own.
fn retitle(db_path: &Path, track_id: i64, title: &str) {
    let db = musefs_db::Db::open(db_path).unwrap();
    db.replace_tags(
        track_id,
        &[
            musefs_db::Tag::new("artist", "Art", 0),
            musefs_db::Tag::new("title", title, 0),
        ],
    )
    .unwrap();
}

/// What a mount reports and serves under one name.
#[derive(Debug, PartialEq, Eq)]
struct Served {
    ino: u64,
    size: u64,
    bytes: Vec<u8>,
}

fn served(dir: &Path, name: &str) -> Served {
    let meta = std::fs::metadata(dir.join(name)).unwrap();
    Served {
        ino: meta.ino(),
        size: meta.len(),
        bytes: std::fs::read(dir.join(name)).unwrap(),
    }
}

/// What one run of the scenario observed.
struct Outcome {
    /// `X.flac` on the held mount before the refresh.
    before: Served,
    /// `X.flac` on the held mount right after the refresh (and the re-read).
    right_after: Served,
    /// `X (2).flac`'s inode on the held mount right after the refresh.
    displaced_ino: u64,
    /// `X.flac` on the held mount once the attr TTL has passed.
    after_ttl: Served,
    /// `X.flac` on a mount of the same store that never saw the old generation.
    reference: Served,
}

/// The move-a-name scenario: a mount holds a directory handle open across a
/// refresh in which a lower track id takes `X.flac` from the track that held
/// it. With `reread`, the held handle is rewound and read again after the
/// refresh, which the kernel answers with `readdirplus` from the old listing.
fn run(reread: bool, tag: &str) -> Outcome {
    // Two tracks told apart by their audio, so served bytes say which is which.
    let backing = tempfile::tempdir().unwrap();
    std::fs::write(
        backing.path().join("one.flac"),
        make_flac(&["ARTIST=Art", "TITLE=One"], &[0xAAu8; 3000]),
    )
    .unwrap();
    std::fs::write(
        backing.path().join("two.flac"),
        make_flac(&["ARTIST=Art", "TITLE=Two"], &[0xBBu8; 5000]),
    )
    .unwrap();
    let db_dir = tempfile::tempdir().unwrap();
    let db_path = db_dir.path().join("m.db");
    let (low, high) = {
        let db = musefs_db::Db::open(&db_path).unwrap();
        scan_directory(&db, backing.path()).unwrap();
        let mut ids: Vec<i64> = db.list_tracks().unwrap().iter().map(|t| t.id).collect();
        ids.sort_unstable();
        assert_eq!(ids.len(), 2);
        (ids[0], ids[1])
    };
    // The higher id holds "X"; the lower one is about to take the name from it,
    // since a collision ranks the lower id at the undecorated name.
    retitle(&db_path, low, "Other");
    retitle(&db_path, high, "X");

    let held_mount = tempfile::tempdir().unwrap();
    let mut fuse_config = FuseConfig::default();
    fuse_config.expose_metrics = true;
    let held_session = musefs_fuse::spawn_with(
        Musefs::open(musefs_db::Db::open(&db_path).unwrap(), config()).unwrap(),
        held_mount.path(),
        &format!("musefs-held-dir-{tag}"),
        fuse_config,
    )
    .unwrap();
    let art = held_mount.path().join("Art");

    let handle: OwnedFd = File::open(&art).unwrap().into();
    let mut held = rustix::fs::Dir::new(handle).unwrap();
    assert_eq!(read_names(&mut held), ["Other.flac", "X.flac"]);
    let before = served(&art, "X.flac");

    retitle(&db_path, low, "X");
    // Any metadata op polls the store; wait until the new generation serves.
    let displaced = art.join("X (2).flac");
    for _ in 0..100 {
        if displaced.exists() {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(displaced.exists(), "the refresh must publish the collision");

    if reread {
        let plus_before = readdirplus_calls(held_mount.path());
        held.rewind();
        let names = read_names(&mut held);
        assert!(
            names.contains(&"X.flac".to_string()),
            "the held handle still lists its own generation: {names:?}"
        );
        if cfg!(target_os = "linux") {
            assert!(
                readdirplus_calls(held_mount.path()) > plus_before,
                "the re-read must be served by readdirplus, the op under test"
            );
        }
    }

    let right_after = served(&art, "X.flac");
    let displaced_ino = std::fs::metadata(&displaced).unwrap().ino();
    // Past the 1 s attr and entry TTL, so the next stat is a fresh getattr.
    std::thread::sleep(Duration::from_millis(1500));
    let after_ttl = served(&art, "X.flac");

    let reference_mount = tempfile::tempdir().unwrap();
    let reference_session = musefs_fuse::spawn_with(
        Musefs::open(musefs_db::Db::open(&db_path).unwrap(), config()).unwrap(),
        reference_mount.path(),
        &format!("musefs-held-dir-{tag}-ref"),
        FuseConfig::default(),
    )
    .unwrap();
    let reference = served(&reference_mount.path().join("Art"), "X.flac");

    drop(held);
    drop(reference_session);
    drop(held_session);
    Outcome {
        before,
        right_after,
        displaced_ino,
        after_ttl,
        reference,
    }
}

#[test]
#[ignore = "requires /dev/fuse + libfuse; run with --ignored"]
fn a_held_directory_reread_after_a_refresh_cannot_point_a_name_at_another_track() {
    let reread = run(true, "reread");
    let control = run(false, "control");

    assert_ne!(
        reread.reference.bytes.last(),
        reread.before.bytes.last(),
        "precondition: X.flac moved to the other track across the refresh"
    );

    // The name keeps its inode, and the track that lost it gets a new one: the
    // path-keyed numbering that makes the old listing's pairs the new ones.
    assert_eq!(
        reread.right_after.ino, reread.before.ino,
        "X.flac must keep its inode across the refresh"
    );
    assert_ne!(
        reread.displaced_ino, reread.before.ino,
        "the displaced track must not keep X.flac's inode under its new name"
    );

    // Right after the re-read, X.flac serves either the track that holds the name
    // now or exactly what the same refresh leaves without the re-read. What the
    // kernel still holds for a name's inode in the attr TTL right after a
    // refresh is the refresh notifier's to invalidate, not the directory
    // handle's; the re-read's attrs may already replace it (they are the
    // current track's), but they can never name a third state.
    let reread_now = (reread.right_after.size, &reread.right_after.bytes);
    assert!(
        reread_now == (reread.reference.size, &reread.reference.bytes)
            || reread_now == (control.right_after.size, &control.right_after.bytes),
        "right after the re-read X.flac served {} bytes, which is neither the track \
         holding the name now ({} bytes) nor what the refresh alone leaves ({} bytes)",
        reread.right_after.bytes.len(),
        reread.reference.bytes.len(),
        control.right_after.bytes.len()
    );

    // Once the TTL has passed, X.flac is the new generation's on both mounts.
    for (label, outcome) in [("reread", &reread), ("control", &control)] {
        assert_eq!(
            (outcome.after_ttl.size, &outcome.after_ttl.bytes),
            (outcome.reference.size, &outcome.reference.bytes),
            "{label}: past the TTL X.flac must serve the track that holds the name now"
        );
    }
}
