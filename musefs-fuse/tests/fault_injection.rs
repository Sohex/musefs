//! An injected EIO backing read surfaces as an I/O error through a real FUSE
//! mount, proving the process-global fault seam reaches the worker thread.
#![cfg(feature = "metrics")]

use musefs_core::metrics::{BackingFault, set_backing_fault};
use musefs_core::{Musefs, scan_directory};

mod common;
use common::{config, make_flac};

#[test]
#[ignore = "requires /dev/fuse; run with: cargo test -p musefs-fuse --features metrics -- --ignored"]
fn eio_backing_read_surfaces_through_the_mount() {
    let backing = tempfile::tempdir().unwrap();
    let flac = make_flac(&["ARTIST=Alice", "TITLE=Song"], &vec![0xAB; 256 * 1024]);
    std::fs::write(backing.path().join("a.flac"), &flac).unwrap();

    let db = musefs_db::Db::open_in_memory().unwrap();
    scan_directory(&db, backing.path()).unwrap();
    let fs = Musefs::open(db, config()).unwrap();
    let mountpoint = tempfile::tempdir().unwrap();
    let session = musefs_fuse::spawn(fs, mountpoint.path(), "musefs-fault-test").unwrap();

    let song = mountpoint.path().join("Alice").join("Song.flac");

    // With EIO injected on the next backing read, reading the (audio-bearing)
    // file through the mount must fail with an I/O error, not succeed or hang.
    let _guard = set_backing_fault(BackingFault::Eio);
    let err = std::fs::read(&song).expect_err("read should fail under injected EIO");
    // FUSE maps the reader's CoreError::Io(EIO) straight back to errno EIO, so a
    // tight assertion guards against a false pass from an unrelated failure.
    assert_eq!(
        err.raw_os_error(),
        Some(5),
        "injected EIO should surface as EIO through the mount, got {err:?}"
    );

    drop(session);
}
