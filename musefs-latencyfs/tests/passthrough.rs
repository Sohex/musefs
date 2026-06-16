use std::io::Read;

use musefs_latencyfs::LatencyMount;

#[test]
#[ignore = "requires /dev/fuse; run with --ignored"]
fn reads_a_file_through_the_mount() {
    let backing = tempfile::tempdir().unwrap();
    std::fs::create_dir(backing.path().join("sub")).unwrap();
    std::fs::write(backing.path().join("sub/hello.txt"), b"hello world").unwrap();
    std::fs::create_dir(backing.path().join("sub/nested")).unwrap();
    std::os::unix::fs::symlink("hello.txt", backing.path().join("sub/link")).unwrap();

    let mount = LatencyMount::new(backing.path(), "ssd").unwrap();
    // Stat + read through the FUSE mount.
    let mp = mount.path();
    let meta = std::fs::metadata(mp.join("sub/hello.txt")).unwrap();
    assert_eq!(meta.len(), 11);
    let mut s = String::new();
    std::fs::File::open(mp.join("sub/hello.txt"))
        .unwrap()
        .read_to_string(&mut s)
        .unwrap();
    assert_eq!(s, "hello world");

    // readdir reports each entry with the correct file type, so the dirent
    // kind classification (file vs dir vs symlink) is actually exercised.
    let mut kinds: Vec<(String, bool, bool, bool)> = std::fs::read_dir(mp.join("sub"))
        .unwrap()
        .map(|e| {
            let e = e.unwrap();
            // `file_type()` here comes from the dirent the FS returned, without
            // a follow-up stat, so it reflects the kind reported by `readdir`.
            let ft = e.file_type().unwrap();
            (
                e.file_name().to_string_lossy().into_owned(),
                ft.is_file(),
                ft.is_dir(),
                ft.is_symlink(),
            )
        })
        .collect();
    kinds.sort();
    assert_eq!(
        kinds,
        vec![
            ("hello.txt".to_string(), true, false, false),
            ("link".to_string(), false, false, true),
            ("nested".to_string(), false, true, false),
        ]
    );
}

#[test]
#[ignore = "requires /dev/fuse; run with --ignored"]
fn write_fsync_rename_unlink_through_the_mount() {
    let backing = tempfile::tempdir().unwrap();
    let mount = LatencyMount::new(backing.path(), "ssd").unwrap();
    let mp = mount.path();

    // create + write + fsync via normal file ops (the kernel issues create/write/fsync).
    let p = mp.join("data.bin");
    {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&p)
            .unwrap();
        f.write_all(b"abcdef").unwrap();
        f.sync_all().unwrap();
    }
    assert_eq!(std::fs::read(&p).unwrap(), b"abcdef");
    assert!(mount.fsyncs() >= 1, "fsync should have been counted");
    // The bytes landed in the backing dir, not a tmpfs overlay (writes forwarded).
    assert_eq!(
        std::fs::read(backing.path().join("data.bin")).unwrap(),
        b"abcdef"
    );

    // rename + unlink.
    let q = mp.join("renamed.bin");
    std::fs::rename(&p, &q).unwrap();
    assert!(q.exists() && !p.exists());
    std::fs::remove_file(&q).unwrap();
    assert!(!q.exists());

    // The backing dir reflects the changes (rename/unlink forwarded).
    assert!(!backing.path().join("data.bin").exists());
}

#[test]
#[ignore = "requires /dev/fuse; run with --ignored"]
fn mkdir_rmdir_and_statfs_through_the_mount() {
    let backing = tempfile::tempdir().unwrap();
    let mount = LatencyMount::new(backing.path(), "ssd").unwrap();
    let mp = mount.path();

    // mkdir then rmdir, each reflected in the backing dir (forwarded to backing).
    std::fs::create_dir(mp.join("d")).unwrap();
    assert!(backing.path().join("d").is_dir());
    std::fs::remove_dir(mp.join("d")).unwrap();
    assert!(!backing.path().join("d").exists());

    // statfs returns real, non-empty filesystem stats for the mount (not the
    // benign all-zero fallback), exercising the passthrough statvfs path.
    let s = rustix::fs::statvfs(mp).unwrap();
    assert!(s.f_blocks > 0, "statfs should report real block counts");
}

/// Read a directory's raw dirents through the mount as `(name, d_ino)` pairs,
/// including the synthetic `.`/`..` entries that `std::fs::read_dir` hides.
fn raw_dirents(dir: &std::path::Path) -> Vec<(String, u64)> {
    let f = std::fs::File::open(dir).unwrap();
    let mut out = Vec::new();
    for entry in rustix::fs::Dir::read_from(&f).unwrap() {
        let entry = entry.unwrap();
        out.push((
            entry.file_name().to_string_lossy().into_owned(),
            entry.ino(),
        ));
    }
    out
}

#[test]
#[ignore = "requires /dev/fuse; run with --ignored"]
fn readdir_dotdot_resolves_to_the_real_parent() {
    let backing = tempfile::tempdir().unwrap();
    std::fs::create_dir(backing.path().join("sub")).unwrap();
    let mount = LatencyMount::new(backing.path(), "ssd").unwrap();
    let mp = mount.path();

    let ino = |ents: &[(String, u64)], name: &str| -> u64 {
        ents.iter()
            .find(|(n, _)| n == name)
            .unwrap_or_else(|| panic!("missing dirent {name}"))
            .1
    };

    // `readdir` synthesizes the `..` entry's inode from the directory's parent:
    // the root short-circuits to itself, every other dir interns its true
    // parent. A subdir's `..` must therefore point at the mount root, never at
    // the subdir itself (which is what an inverted root check would produce).
    let root = raw_dirents(&mp);
    let sub = raw_dirents(&mp.join("sub"));
    assert_eq!(
        ino(&sub, ".."),
        ino(&root, "."),
        "sub/.. must resolve to the mount root"
    );
    assert_ne!(
        ino(&sub, ".."),
        ino(&sub, "."),
        "sub/.. must not be self-referential"
    );
}
