use std::io::Read;

use musefs_latencyfs::LatencyMount;

#[test]
#[ignore = "requires /dev/fuse; run with --ignored"]
fn reads_a_file_through_the_mount() {
    let backing = tempfile::tempdir().unwrap();
    std::fs::create_dir(backing.path().join("sub")).unwrap();
    std::fs::write(backing.path().join("sub/hello.txt"), b"hello world").unwrap();

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

    // readdir sees the entry.
    let names: Vec<String> = std::fs::read_dir(mp.join("sub"))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert!(names.contains(&"hello.txt".to_string()));
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
    // The bytes landed in the backing dir, not a tmpfs overlay (true passthrough).
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

    // The backing dir reflects the changes (true passthrough).
    assert!(!backing.path().join("data.bin").exists());
}
