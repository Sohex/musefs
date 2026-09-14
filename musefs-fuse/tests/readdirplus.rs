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

use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::Command;

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
    let mut fuse_config = FuseConfig::default();
    fuse_config.expose_metrics = true;
    let walked = musefs_fuse::spawn_with(
        Musefs::open(musefs_db::Db::open(&db_path).unwrap(), config()).unwrap(),
        walked_mount.path(),
        "musefs-readdirplus-e2e",
        fuse_config,
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
    let plus_calls = metric(&text, "musefs_readdirplus_total");
    // Whether the op is sent at all is the kernel's to decide, and the counter
    // is the only evidence either way — so a runtime check would make this
    // vacuous exactly where it matters. Linux has sent `readdirplus` since 3.9;
    // FreeBSD's fusefs has no such op, and macOS never runs this tier. The
    // comparison below still runs there, over the plain `readdir` + `lookup`
    // path this replaces.
    if cfg!(target_os = "linux") {
        assert!(
            plus_calls > 0,
            "the kernel must be sending readdirplus: the capability is negotiated at \
             mount, so a dropped init flag shows up here and nowhere else (#667)"
        );
    } else {
        eprintln!(
            "note: this kernel served the walk as plain readdir ({plus_calls} readdirplus \
             calls); the attr comparison still runs, but not against the op under test"
        );
    }

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

/// Where [`mapped_file_child`] finds the file to map. Unset, the child does
/// nothing, so a plain `--ignored` run passes it.
const MAPPED_FILE_ENV: &str = "MUSEFS_E2E_MAPPED_FILE";

/// The child half of [`an_over_cap_listing_leaves_a_mapped_file_whole`]: map a
/// served file, list its directory, then touch the mapping's last byte.
///
/// A `readdirplus` entry's attrs land on the inode the kernel already holds for
/// that name, whatever their TTL. If the listing carries a size of 0 for this
/// file, the kernel truncates its page cache and the touch below faults past
/// `i_size`, so the process dies of `SIGBUS` — which is why it runs in a child
/// the parent can watch die.
#[test]
#[ignore = "the child half of an_over_cap_listing_leaves_a_mapped_file_whole"]
#[expect(
    unsafe_code,
    reason = "memmap2::Mmap::map is unsafe; the mapping is the thing under test"
)]
fn mapped_file_child() {
    let Some(path) = std::env::var_os(MAPPED_FILE_ENV) else {
        return;
    };
    let path = PathBuf::from(path);
    let file = File::open(&path).unwrap();
    // SAFETY: a regular file on a read-only mount, mapped for the rest of this
    // short-lived process and never written.
    let map = unsafe { memmap2::Mmap::map(&file).unwrap() };
    assert!(map.len() > 1, "the served file must span more than a byte");
    let first = map[0];

    let listed: Vec<_> = std::fs::read_dir(path.parent().unwrap())
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    assert!(
        listed.iter().any(|name| name == path.file_name().unwrap()),
        "the directory must list the mapped file: {listed:?}"
    );

    let last = map[map.len() - 1];
    std::hint::black_box((first, last));
}

/// A title under `artist` with `audio_len` bytes of audio, written into `dir`.
fn write_track(dir: &Path, artist: &str, title: &str, audio_len: usize) {
    let flac = make_flac(
        &[&format!("ARTIST={artist}"), &format!("TITLE={title}")],
        &vec![0xABu8; audio_len],
    );
    std::fs::write(dir.join(format!("{title}.flac")), &flac).unwrap();
}

/// Over the pool's admission cap a `readdirplus` resolution is not run. The
/// entry used to go out anyway, with a size-0 placeholder and a zero TTL,
/// which the kernel applies to the inode a process already has open and mapped:
/// its page cache was truncated and the next touch of the mapping raised
/// `SIGBUS` (#694). The page must end before an entry with no attrs instead.
#[test]
#[ignore = "requires /dev/fuse + libfuse; run with --ignored"]
fn an_over_cap_listing_leaves_a_mapped_file_whole() {
    let backing = tempfile::tempdir().unwrap();
    for t in 0..3 {
        write_track(backing.path(), "Mapped", &format!("Song{t}"), 256 * 1024);
    }
    let db = musefs_db::Db::open_in_memory().unwrap();
    scan_directory(&db, backing.path()).unwrap();

    let mountpoint = tempfile::tempdir().unwrap();
    let mut fuse_config = FuseConfig::default();
    // Every metadata job meets the cap, so every resolution the op may leave
    // unrun is left unrun.
    fuse_config.pool_admission_cap = Some(0);
    let session = musefs_fuse::spawn_with(
        Musefs::open(db, config()).unwrap(),
        mountpoint.path(),
        "musefs-readdirplus-over-cap",
        fuse_config,
    )
    .unwrap();
    let dir = mountpoint.path().join("Mapped");

    let status = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "mapped_file_child",
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(MAPPED_FILE_ENV, dir.join("Song1.flac"))
        .status()
        .unwrap();
    assert!(
        status.success(),
        "the child must survive touching its mapping after the listing: {status:?} \
         (signal {:?}; SIGBUS means the listing truncated a mapped file)",
        status.signal()
    );

    // The shortened pages still add up to the whole directory, and every size
    // the walk sees is the size of the bytes served.
    let mut seen = BTreeMap::new();
    walk_with_metadata(&dir, &mut seen, &dir);
    let names: BTreeSet<_> = seen.keys().cloned().collect();
    let expected: BTreeSet<_> = (0..3)
        .map(|t| PathBuf::from(format!("Song{t}.flac")))
        .collect();
    assert_eq!(names, expected, "every title must be listed over the cap");
    for (relative, (size, _)) in &seen {
        let served = std::fs::read(dir.join(relative)).unwrap();
        assert_eq!(
            *size,
            u64::try_from(served.len()).unwrap(),
            "{} must report the size it serves",
            relative.display()
        );
    }

    drop(session);
}

/// An entry whose attrs fail to resolve on every attempt — its backing file is
/// gone — still has to be listed, and the listing still has to reach the end.
/// Ending the page before it would, once it is a page's first entry, be an
/// empty reply, which the kernel reads as the end of the directory; an error
/// would fail the whole `getdents`. Either way the names after it vanish.
#[test]
#[ignore = "requires /dev/fuse + libfuse; run with --ignored"]
fn a_file_that_cannot_be_resolved_is_still_listed() {
    let backing = tempfile::tempdir().unwrap();
    for title in ["A", "B", "C"] {
        write_track(backing.path(), "Gone", title, 4096);
    }
    let db = musefs_db::Db::open_in_memory().unwrap();
    scan_directory(&db, backing.path()).unwrap();

    let mountpoint = tempfile::tempdir().unwrap();
    let session = musefs_fuse::spawn_with(
        Musefs::open(db, config()).unwrap(),
        mountpoint.path(),
        "musefs-readdirplus-unresolvable",
        FuseConfig::default(),
    )
    .unwrap();
    std::fs::remove_file(backing.path().join("B.flac")).unwrap();

    // Stat each entry as it is listed, as a scanner does: that is what keeps the
    // kernel on `readdirplus` past the first page, so the failing entry comes
    // back as the first entry of a page of its own.
    let mut listed = BTreeSet::new();
    let mut unresolvable = BTreeSet::new();
    for entry in std::fs::read_dir(mountpoint.path().join("Gone")).unwrap() {
        let entry = entry.expect("the listing itself must not fail");
        let name = entry.file_name().into_string().unwrap();
        if entry.metadata().is_err() {
            unresolvable.insert(name.clone());
        }
        listed.insert(name);
    }
    assert_eq!(
        listed,
        BTreeSet::from(["A.flac".to_string(), "B.flac".into(), "C.flac".into()]),
        "the unresolvable file and every name after it must be listed"
    );
    assert_eq!(
        unresolvable,
        BTreeSet::from(["B.flac".to_string()]),
        "the client's own stat reports the failure, for that file alone"
    );

    drop(session);
}
