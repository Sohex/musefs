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
