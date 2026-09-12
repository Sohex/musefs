//! E2E: `readdirplus` must answer with the same attrs a `lookup` would (#667).
//!
//! The op folds the per-entry `lookup` into the directory read, so the attrs it
//! reports are the ones the kernel caches and hands to the client's `stat` — a
//! wrong one there is invisible to any check that stats the same mount again,
//! because the second stat is answered from that same cached attr. So the
//! ground truth comes from a second mount of the same store, walked by `stat`
//! alone: its attr cache is filled by `lookup`/`getattr`, the path this one
//! replaces.
//!
//! Run with:
//!   cargo test -p musefs-fuse --test readdirplus -- --ignored --nocapture

use std::collections::BTreeMap;
use std::fs::File;
use std::path::{Path, PathBuf};

use musefs_core::{Musefs, scan_directory};
use musefs_fuse::FuseConfig;

mod common;
use common::{config, make_flac};

/// Artists, and titles per artist. Kept narrow deliberately: with
/// `FUSE_READDIRPLUS_AUTO` the kernel is guaranteed to use `readdirplus` for the
/// first page of a listing, so a directory that fits in one page is one whose
/// attrs all come from the op under test.
const ARTISTS: usize = 5;
const TITLES: usize = 20;

fn artist(i: usize) -> String {
    format!("Artist{i}")
}

/// Titles of differing length, so the synthesized files differ in size and a
/// mixed-up attr shows as a mismatch rather than a coincidence.
fn title(i: usize) -> String {
    format!("Song{i}{}", "x".repeat(i))
}

/// Every path under `dir`, with the size and file type the walk sees. Uses
/// `DirEntry::metadata`, which is what a scanner does and what `readdirplus`
/// exists to answer: each stat is served from the attr cache the listing filled.
fn walk_with_metadata(dir: &Path, out: &mut BTreeMap<PathBuf, (u64, bool)>, root: &Path) {
    let entries =
        std::fs::read_dir(dir).unwrap_or_else(|e| panic!("read_dir({}): {e}", dir.display()));
    for entry in entries {
        let entry = entry.unwrap_or_else(|e| panic!("dir entry in {}: {e}", dir.display()));
        let path = entry.path();
        let meta = entry
            .metadata()
            .unwrap_or_else(|e| panic!("metadata({}): {e}", path.display()));
        let relative = path.strip_prefix(root).unwrap().to_path_buf();
        out.insert(relative, (meta.len(), meta.is_dir()));
        if meta.is_dir() {
            walk_with_metadata(&path, out, root);
        }
    }
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
fn readdirplus_attrs_match_what_lookup_reports() {
    let backing = tempfile::tempdir().unwrap();
    for a in 0..ARTISTS {
        for t in 0..TITLES {
            // Distinct audio lengths too, so two files never share a size by
            // accident.
            let audio = vec![0xAAu8; 32 + a * TITLES + t];
            let flac = make_flac(
                &[
                    &format!("ARTIST={}", artist(a)),
                    &format!("TITLE={}", title(t)),
                ],
                &audio,
            );
            std::fs::write(backing.path().join(format!("{a}-{t}.flac")), &flac).unwrap();
        }
    }
    let db_path = backing.path().join("m.db");
    {
        let db = musefs_db::Db::open(&db_path).unwrap();
        scan_directory(&db, backing.path()).unwrap();
    }

    let walked_mount = tempfile::tempdir().unwrap();
    let walked = musefs_fuse::spawn_with(
        Musefs::open(musefs_db::Db::open(&db_path).unwrap(), config()).unwrap(),
        walked_mount.path(),
        "musefs-readdirplus-e2e",
        FuseConfig {
            expose_metrics: true,
            ..FuseConfig::default()
        },
    )
    .unwrap();

    let mut seen = BTreeMap::new();
    walk_with_metadata(walked_mount.path(), &mut seen, walked_mount.path());

    // The listing itself: every artist directory and every title, plus the
    // synthetic telemetry namespace.
    let files = seen.values().filter(|(_, is_dir)| !is_dir).count();
    assert_eq!(
        files,
        ARTISTS * TITLES + 1,
        "every track must be listed, plus the metrics file"
    );
    for a in 0..ARTISTS {
        assert!(
            seen.get(Path::new(&artist(a))).is_some_and(|(_, d)| *d),
            "{} must be listed as a directory",
            artist(a)
        );
    }

    let text = read_metrics(walked_mount.path());
    assert!(
        metric(&text, "musefs_readdirplus_total") > 0,
        "the kernel must be sending readdirplus: the capability is negotiated at \
         mount, so a dropped init flag shows up here and nowhere else (#667)"
    );

    // Ground truth: the same store, mounted separately and never walked, so
    // every attr below is one `lookup`/`getattr` produced.
    let statted_mount = tempfile::tempdir().unwrap();
    let statted = musefs_fuse::spawn_with(
        Musefs::open(musefs_db::Db::open(&db_path).unwrap(), config()).unwrap(),
        statted_mount.path(),
        "musefs-readdirplus-e2e-ref",
        FuseConfig::default(),
    )
    .unwrap();

    for (relative, (size, is_dir)) in &seen {
        if relative.starts_with(".musefs-metrics") {
            continue; // synthetic, and absent without --expose-metrics
        }
        let reference = statted_mount.path().join(relative);
        let meta = std::fs::metadata(&reference)
            .unwrap_or_else(|e| panic!("stat({}): {e}", reference.display()));
        assert_eq!(
            (*size, *is_dir),
            (meta.len(), meta.is_dir()),
            "readdirplus disagrees with lookup about {}",
            relative.display()
        );
    }

    drop(statted);
    drop(walked);
}
