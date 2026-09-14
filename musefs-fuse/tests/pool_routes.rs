//! E2E: which operations go through the worker pool's admission gate, which
//! run without it, and what each does over the cap (#694).
//!
//! The routing lives in each handler's body, where only reading used to verify
//! it. A `test-support` trace records every job a mount hands its pool, labelled
//! with the op and the route it took, so the routing is pinned by behaviour:
//!
//! - under the cap, metadata jobs queue, and reads take their own lane;
//! - over it, metadata jobs run on the submitting thread, a `readdirplus`
//!   page's first entry among them, while its other entries are dropped unrun;
//! - store refreshes never reach the pool, and `statfs`, `release` and
//!   `releasedir` are answered without touching it.
//!
//! The listing's own route depends on the kernel. Linux negotiates
//! `readdirplus`, so the listing is `readdirplus` and its entries'
//! `readdirplus_attr` jobs. FreeBSD's fusefs does not implement the op
//! (`fuse_internal_send_init` lists `FUSE_DO_READDIRPLUS` as not yet
//! implemented), so it lists with plain `readdir`, which a held handle answers
//! from its listing without the pool. macOS never runs this tier.
//!
//! Run with:
//!   cargo test -p musefs-fuse --test pool_routes -- --ignored --nocapture

use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::Read;
use std::os::fd::OwnedFd;
use std::path::Path;

use musefs_core::{Musefs, scan_directory};
use musefs_fuse::{FuseConfig, PoolRoute, RouteTrace};

mod common;
use common::{config, make_flac};

/// Titles in the one directory: more than a `readdirplus` page's first entry,
/// so a listing over the cap has entries to drop.
const TITLES: usize = 8;

/// The routes the trace saw, by op label.
fn routes(trace: &RouteTrace) -> BTreeMap<&'static str, BTreeSet<PoolRoute>> {
    let mut by_op: BTreeMap<&'static str, BTreeSet<PoolRoute>> = BTreeMap::new();
    for (op, route) in trace.lock().unwrap().iter() {
        by_op.entry(op).or_default().insert(*route);
    }
    by_op
}

fn clear(trace: &RouteTrace) {
    trace.lock().unwrap().clear();
}

/// A one-directory library, mounted with `admission_cap` and a trace.
fn mount(
    backing: &Path,
    name: &str,
    admission_cap: Option<usize>,
    dir_handle_cap: Option<usize>,
) -> (tempfile::TempDir, fuser::BackgroundSession, RouteTrace) {
    let db = musefs_db::Db::open_in_memory().unwrap();
    scan_directory(&db, backing).unwrap();
    let trace = RouteTrace::default();
    let mut fuse_config = FuseConfig::default();
    // No kernel attr caching, so every stat reaches the daemon.
    fuse_config.ttl = std::time::Duration::ZERO;
    fuse_config.keep_cache = false;
    fuse_config.pool_admission_cap = admission_cap;
    fuse_config.dir_handle_cap = dir_handle_cap;
    fuse_config.route_trace = Some(trace.clone());
    let mountpoint = tempfile::tempdir().unwrap();
    let session = musefs_fuse::spawn_with(
        Musefs::open(db, config()).unwrap(),
        mountpoint.path(),
        name,
        fuse_config,
    )
    .unwrap();
    (mountpoint, session, trace)
}

/// Stat, open, read and list: every pool-bound op a client drives.
///
/// The listing stats each entry as it reads it, as a scanner does. On reading
/// `.` it also stats a title the kernel has not looked up yet: a fresh `lookup`
/// is what marks the directory for `readdirplus` (`FUSE_I_ADVISE_RDPLUS`; a
/// revalidating one does not), and it lands between the first reply page and
/// the second. Over the cap that first page ends before its first file entry,
/// so the second page begins at one — the one case where a page's first entry
/// needs the pool, and the route pinned over the cap. A kernel without
/// `readdirplus` lists with `readdir`, and the extra stat is one more `lookup`.
fn drive(root: &Path) {
    let dir = root.join("Art");
    let song = dir.join("Song0.flac");
    std::fs::metadata(&song).unwrap();
    let mut buf = Vec::new();
    File::open(&song).unwrap().read_to_end(&mut buf).unwrap();
    assert!(!buf.is_empty());

    let handle: OwnedFd = File::open(&dir).unwrap().into();
    let mut listing = rustix::fs::Dir::new(handle).unwrap();
    let mut files = 0;
    while let Some(entry) = listing.read() {
        let entry = entry.expect("the listing must not fail");
        let name = entry.file_name().to_str().unwrap().to_owned();
        if name == "." {
            std::fs::metadata(dir.join(format!("Song{}.flac", TITLES - 1))).unwrap();
        } else if name != ".." {
            std::fs::metadata(dir.join(&name)).unwrap();
            files += 1;
        }
    }
    assert_eq!(files, TITLES, "every title must be listed");
}

#[test]
#[ignore = "requires /dev/fuse + libfuse; run with --ignored"]
fn every_op_takes_its_route_through_the_worker_pool() {
    let backing = tempfile::tempdir().unwrap();
    for t in 0..TITLES {
        let flac = make_flac(&["ARTIST=Art", &format!("TITLE=Song{t}")], &[0xAAu8; 4096]);
        std::fs::write(backing.path().join(format!("{t}.flac")), &flac).unwrap();
    }

    // Under the cap.
    {
        let (root, session, trace) = mount(backing.path(), "musefs-routes-queued", None, None);
        drive(root.path());
        let seen = routes(&trace);
        for op in ["lookup", "getattr", "open", "opendir"] {
            assert_eq!(
                seen.get(op),
                Some(&BTreeSet::from([PoolRoute::Queued])),
                "{op} must queue on the pool under the cap: {seen:?}"
            );
        }
        if cfg!(target_os = "linux") {
            assert_eq!(
                seen.get("readdirplus_attr"),
                Some(&BTreeSet::from([PoolRoute::Queued])),
                "readdirplus_attr must queue on the pool under the cap: {seen:?}"
            );
        } else {
            for op in ["readdirplus", "readdirplus_attr", "readdir"] {
                assert!(
                    !seen.contains_key(op),
                    "without readdirplus, a held handle's readdir needs no pool job, \
                     so {op} must not appear: {seen:?}"
                );
            }
        }
        assert_eq!(
            seen.get("read"),
            Some(&BTreeSet::from([PoolRoute::ReadLane])),
            "reads take their own lane, never the metadata gate: {seen:?}"
        );
        for op in ["poll_refresh", "poll_refresh_notify"] {
            assert!(
                !seen.contains_key(op),
                "{op} runs on the refresh lane: {seen:?}"
            );
        }

        // Answered without the pool at all.
        clear(&trace);
        rustix::fs::statvfs(root.path()).unwrap();
        let file = File::open(root.path().join("Art").join("Song0.flac")).unwrap();
        clear(&trace);
        drop(file);
        let dir = File::open(root.path().join("Art")).unwrap();
        clear(&trace);
        drop(dir);
        rustix::fs::statvfs(root.path()).unwrap();
        assert!(
            trace.lock().unwrap().is_empty(),
            "statfs, release and releasedir must not touch the pool: {:?}",
            trace.lock().unwrap()
        );
        drop(session);
    }

    // Over the cap: every metadata job meets it. The directory handle table is
    // full too, so listings take the stateless path, whose rebuild is a job.
    {
        let (root, session, trace) =
            mount(backing.path(), "musefs-routes-over-cap", Some(0), Some(0));
        drive(root.path());
        let seen = routes(&trace);
        let listing = if cfg!(target_os = "linux") {
            "readdirplus"
        } else {
            "readdir"
        };
        for op in ["lookup", "getattr", "open", "opendir", listing] {
            assert_eq!(
                seen.get(op),
                Some(&BTreeSet::from([PoolRoute::InPlace])),
                "over the cap {op} must run on the submitting thread, not be refused: {seen:?}"
            );
        }
        if cfg!(target_os = "linux") {
            assert_eq!(
                seen.get("readdirplus_attr"),
                Some(&BTreeSet::from([PoolRoute::InPlace, PoolRoute::Dropped])),
                "over the cap a readdirplus page's first entry runs in place and the \
                 rest are dropped unrun: {seen:?}"
            );
        } else {
            for op in ["readdirplus", "readdirplus_attr"] {
                assert!(
                    !seen.contains_key(op),
                    "a kernel without readdirplus never sends {op}: {seen:?}"
                );
            }
        }
        assert_eq!(
            seen.get("read"),
            Some(&BTreeSet::from([PoolRoute::ReadLane])),
            "the metadata cap does not apply to reads: {seen:?}"
        );
        drop(session);
    }
}
