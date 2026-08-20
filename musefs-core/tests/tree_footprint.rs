//! Resident-footprint gate for the virtual tree (#629).
//!
//! #617 cut the tree's per-track cost by interning node names, but the probe that
//! measured it was a throwaway — nothing then kept the cost from creeping back.
//! This turns that measurement into a committed gate.
//!
//! What is measured: the `VmRSS` delta across `VirtualTree::build_with` alone. The
//! rendered paths are materialized — and their pages touched — BEFORE the baseline
//! sample, so the delta is the marginal cost of the tree plus the inode
//! allocator's path map, given paths the refresh snapshot already owns. That is
//! the accounting a mount actually pays; it is NOT a mounted daemon's total RSS,
//! for which `docs/src/guide/tuning.md` carries the number.
//!
//! Why a plain `#[test]` rather than a bench or an `#[ignore]`d harness: neither
//! `cargo bench` nor `cargo test -- --ignored` runs for `musefs-core` in CI or in
//! the pre-commit hook, so only a non-ignored test gates anything. Living in its
//! own integration-test binary is what makes a process-wide `VmRSS` reading
//! trustworthy — cargo runs test *binaries* concurrently, but this one holds a
//! single test, so nothing else in this process allocates while the sample is
//! taken.
//!
//! The ceiling is deliberately loose. A gate that catches only a large regression
//! is worth far more than one that reddens on a loaded runner.

use musefs_core::convert::usize_from;
use musefs_core::{InodeAllocator, VirtualTree};

/// Default corpus size: large enough that the delta (tens of MB) dwarfs
/// page-granularity noise, small enough to keep the debug-profile workspace suite
/// quick. Override with `MUSEFS_TREE_FOOTPRINT_TRACKS` to reproduce #617's
/// 200,000-track figure.
const DEFAULT_TRACKS: usize = 20_000;

/// Corpus shape: three levels, as `$artist/$album/$title` renders — the shape
/// #617 measured. Per-track cost depends on tree depth, so this must stay fixed
/// for the ceiling below to mean anything.
const ALBUMS_PER_ARTIST: usize = 10;
const TRACKS_PER_ALBUM: usize = 10;

/// Upper bound on bytes of RSS per track. The cost measured after #617 is
/// ~1.3 KB/track and #629 takes it lower; 3 KB leaves better than 2x headroom, so
/// runner noise, allocator arena rounding, or a different malloc cannot redden
/// it — while a doubling of the per-node cost (reverting the name interning is a
/// third of one) still trips it.
const CEILING_BYTES_PER_TRACK: usize = 3072;

/// Lower bound, so a broken measurement — a `VmRSS` that never moves — fails
/// loudly instead of passing vacuously. No sharing scheme stores a node per track
/// in less than this.
const FLOOR_BYTES_PER_TRACK: usize = 128;

/// Current resident set size in bytes, from `/proc/self/status` `VmRSS`. `None`
/// off Linux or when unreadable; the test then reports and skips.
fn rss_bytes() -> Option<usize> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("VmRSS:") {
            let kib: usize = rest.trim().trim_end_matches(" kB").trim().parse().ok()?;
            return Some(kib * 1024);
        }
    }
    None
}

/// `$artist/$album/$title` paths for `tracks` tracks, in the ascending-id order a
/// full rebuild uses.
fn rendered_paths(tracks: usize) -> Vec<(i64, String)> {
    let mut entries = Vec::with_capacity(tracks);
    for i in 0..tracks {
        let artist = i / (ALBUMS_PER_ARTIST * TRACKS_PER_ALBUM);
        let album = (i / TRACKS_PER_ALBUM) % ALBUMS_PER_ARTIST;
        let track = i % TRACKS_PER_ALBUM;
        entries.push((
            i64::try_from(i).unwrap(),
            format!("Artist {artist:05}/Album {album:02}/{track:02} Title {i:06}.flac"),
        ));
    }
    entries
}

#[test]
fn virtual_tree_resident_cost_per_track_stays_under_ceiling() {
    let tracks: usize = std::env::var("MUSEFS_TREE_FOOTPRINT_TRACKS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_TRACKS);
    assert!(tracks > 0, "corpus must hold at least one track");

    // Materialize every rendered path first: the input belongs to the baseline, so
    // what the delta measures is the tree's own marginal cost.
    let entries = rendered_paths(tracks);

    let Some(before) = rss_bytes() else {
        println!("tree footprint: /proc/self/status unavailable, skipping");
        return;
    };
    let mut alloc = InodeAllocator::new(false);
    let tree = VirtualTree::build_with(&entries, &mut alloc);
    let after = rss_bytes().expect("VmRSS stayed readable");

    let (files, dirs) = tree.entry_counts();
    let interned = alloc.interned_path_count();
    let delta = after.saturating_sub(before);
    let per_track = delta / tracks;
    println!(
        "tree footprint: {tracks} tracks -> {files} files + {dirs} dirs, \
         {interned} interned paths, {} KiB resident, {per_track} B/track \
         (ceiling {CEILING_BYTES_PER_TRACK})",
        delta / 1024,
    );

    // Pin the corpus shape the per-track figure is normalized against, so an edit
    // to `rendered_paths` cannot quietly make the budget easier to meet.
    assert_eq!(
        usize_from(files),
        tracks,
        "every entry must materialize a file"
    );
    assert_eq!(
        interned,
        tree.node_count(),
        "a freshly built tree interns exactly one path per node"
    );

    assert!(
        per_track >= FLOOR_BYTES_PER_TRACK,
        "measured {per_track} B/track over {tracks} tracks: implausibly low — the \
         RSS measurement is broken and this gate is vacuous"
    );
    assert!(
        per_track <= CEILING_BYTES_PER_TRACK,
        "virtual tree cost {per_track} B/track over {tracks} tracks, above the \
         {CEILING_BYTES_PER_TRACK} B ceiling (#629)"
    );
}
