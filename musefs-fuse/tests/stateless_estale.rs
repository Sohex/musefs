//! E2E: a stateless enumeration that cannot be resumed fails with `ESTALE`
//! (#695), end to end through a mount.
//!
//! The unit tests pin `stateless_page`'s decision; this pins the rest of the
//! path, for both handlers that make it: `readdir` and `readdirplus` turning
//! `StatelessPage::Stale` into an `ESTALE` reply, and the kernel handing that
//! errno to `getdents`. Forcing both caps through the `test-support` seams makes
//! it deterministic: every `opendir` is served statelessly, and the listing
//! cache holds one listing, so a second directory's enumeration evicts the
//! first's.
//!
//! Run with:
//!   cargo test -p musefs-fuse --test stateless_estale -- --ignored --nocapture

use std::fs::File;
use std::os::fd::OwnedFd;
use std::path::Path;
use std::time::Duration;

use musefs_core::{Musefs, scan_directory};
use musefs_fuse::FuseConfig;

mod common;
use common::{config, make_flac};

/// Tracks in the wide directory: enough that its listing spans several reply
/// pages, so an enumeration can be left part-way through.
const WIDE: usize = 300;

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

fn read_all(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("read_dir({}): {e}", dir.display()))
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    names
}

/// Set the narrow directory's one track's title, through a connection of its
/// own, and wait for the mount to publish it.
fn retitle_narrow(db_path: &Path, narrow: &Path, title: &str) {
    {
        let db = musefs_db::Db::open(db_path).unwrap();
        let id = db
            .list_tracks()
            .unwrap()
            .into_iter()
            .find(|t| t.backing_path.ends_with("n.flac"))
            .unwrap()
            .id;
        db.replace_tags(
            id,
            &[
                musefs_db::Tag::new("artist", "Narrow", 0),
                musefs_db::Tag::new("title", title, 0),
            ],
        )
        .unwrap();
    }
    let published = narrow.join(format!("{title}.flac"));
    for _ in 0..100 {
        if published.exists() {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(published.exists(), "the refresh must publish {title}");
}

/// Leave a wide enumeration after its first page, evict its listing with the
/// narrow directory's, replace the generation, then resume. `advise_plus`
/// stats a wide entry first, which is what makes the kernel resume with
/// `readdirplus` rather than `readdir`.
fn resume_after_eviction_and_refresh(
    mountpoint: &Path,
    db_path: &Path,
    advise_plus: bool,
    title: &str,
) -> (Option<rustix::io::Errno>, u64) {
    let wide = mountpoint.join("Wide");
    let narrow = mountpoint.join("Narrow");

    let handle: OwnedFd = File::open(&wide).unwrap().into();
    let mut partial = rustix::fs::Dir::new(handle).unwrap();
    partial
        .read()
        .expect("the wide directory has entries")
        .expect("its first page must be served");

    // Another directory's enumeration takes the one cache slot, evicting it.
    assert!(!read_all(&narrow).is_empty());
    retitle_narrow(db_path, &narrow, title);
    if advise_plus {
        std::fs::metadata(wide.join("Song0299.flac")).unwrap();
    }

    let plus_before = readdirplus_calls(mountpoint);
    let mut outcome = None;
    while let Some(entry) = partial.read() {
        if let Err(err) = entry {
            outcome = Some(err);
            break;
        }
    }
    (outcome, readdirplus_calls(mountpoint) - plus_before)
}

#[test]
#[ignore = "requires /dev/fuse + libfuse; run with --ignored"]
fn an_unresumable_stateless_enumeration_fails_with_estale() {
    let backing = tempfile::tempdir().unwrap();
    for i in 0..WIDE {
        let flac = make_flac(
            &["ARTIST=Wide", &format!("TITLE=Song{i:04}")],
            &[0xAAu8; 32],
        );
        std::fs::write(backing.path().join(format!("w{i:04}.flac")), &flac).unwrap();
    }
    let flac = make_flac(&["ARTIST=Narrow", "TITLE=Only"], &[0xBBu8; 32]);
    std::fs::write(backing.path().join("n.flac"), &flac).unwrap();
    let db_dir = tempfile::tempdir().unwrap();
    let db_path = db_dir.path().join("m.db");
    {
        let db = musefs_db::Db::open(&db_path).unwrap();
        scan_directory(&db, backing.path()).unwrap();
    }

    let mountpoint = tempfile::tempdir().unwrap();
    let mut fuse_config = FuseConfig::default();
    fuse_config.dir_handle_cap = Some(0);
    fuse_config.stateless_listing_cap = Some(1);
    fuse_config.expose_metrics = true;
    // Every stat reaches the daemon, so the one that advises readdirplus is a
    // real lookup rather than a cached dentry.
    fuse_config.ttl = Duration::ZERO;
    let session = musefs_fuse::spawn_with(
        Musefs::open(musefs_db::Db::open(&db_path).unwrap(), config()).unwrap(),
        mountpoint.path(),
        "musefs-stateless-estale",
        fuse_config,
    )
    .unwrap();

    // Resumed with plain `readdir`: the kernel has no reason to want attrs.
    let (outcome, plus) =
        resume_after_eviction_and_refresh(mountpoint.path(), &db_path, false, "First");
    assert_eq!(
        outcome,
        Some(rustix::io::Errno::STALE),
        "readdir: an unresumable stateless enumeration must fail with ESTALE"
    );
    if cfg!(target_os = "linux") {
        assert_eq!(plus, 0, "precondition: this resume was served by readdir");
    }

    // Resumed with `readdirplus`, after a stat marks the directory for it.
    let (outcome, plus) =
        resume_after_eviction_and_refresh(mountpoint.path(), &db_path, true, "Second");
    assert_eq!(
        outcome,
        Some(rustix::io::Errno::STALE),
        "readdirplus: an unresumable stateless enumeration must fail with ESTALE"
    );
    if cfg!(target_os = "linux") {
        assert!(
            plus > 0,
            "precondition: this resume was served by readdirplus"
        );
    }

    // A new enumeration of the directory starts on the current generation.
    assert_eq!(
        read_all(&mountpoint.path().join("Wide")).len(),
        WIDE,
        "a fresh enumeration succeeds"
    );

    drop(session);
}
