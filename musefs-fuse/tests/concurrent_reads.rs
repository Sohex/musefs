//! Concurrent reads through a real FUSE mount: the same file from many threads
//! and many files in parallel, driving the DbPool::PerThread worker pool.
//! `--ignored` (needs /dev/fuse); runs in the e2e job and the TSan job.

use std::sync::{Arc, Barrier};

use fuser::BackgroundSession;
use musefs_core::{Musefs, scan_directory};

mod common;
use common::{config, make_flac};

// ---------------------------------------------------------------------------

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn setup_mount() -> (tempfile::TempDir, tempfile::TempDir, BackgroundSession) {
    let backing = tempfile::tempdir().unwrap();
    for i in 0u8..8 {
        let audio: Vec<u8> = (0..(128 * 1024))
            .map(|b| (b as u8).wrapping_add(i))
            .collect();
        let flac = make_flac(&[&format!("ARTIST=A{i}"), &format!("TITLE=S{i}")], &audio);
        std::fs::write(backing.path().join(format!("t{i}.flac")), &flac).unwrap();
    }
    let db_path = backing.path().join("m.db");
    let db = musefs_db::Db::open(&db_path).unwrap();
    scan_directory(&db, backing.path()).unwrap();
    let fs = Musefs::open(db, config()).unwrap();
    let mountpoint = tempfile::tempdir().unwrap();
    let session = musefs_fuse::spawn(fs, mountpoint.path(), "musefs-concurrent-reads").unwrap();
    (backing, mountpoint, session)
}

fn list_songs(mnt: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut v = Vec::new();
    for artist in std::fs::read_dir(mnt).unwrap() {
        let artist = artist.unwrap().path();
        if artist.is_dir() {
            for song in std::fs::read_dir(&artist).unwrap() {
                v.push(song.unwrap().path());
            }
        }
    }
    v.sort();
    v
}

#[test]
#[ignore = "requires /dev/fuse; run with: cargo test -p musefs-fuse -- --ignored"]
fn same_file_many_threads_through_mount() {
    let (_backing, mnt, _session) = setup_mount();
    let songs = list_songs(mnt.path());
    let target = songs[0].clone();
    let reference = std::fs::read(&target).unwrap();

    const THREADS: usize = 12;
    let barrier = Arc::new(Barrier::new(THREADS));
    let target = Arc::new(target);
    let reference = Arc::new(reference);
    let handles: Vec<_> = (0..THREADS)
        .map(|_| {
            let (barrier, target, reference) = (barrier.clone(), target.clone(), reference.clone());
            std::thread::spawn(move || {
                barrier.wait();
                for _ in 0..20 {
                    assert_eq!(&std::fs::read(&*target).unwrap(), &*reference);
                }
            })
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }
}

#[test]
#[ignore = "requires /dev/fuse; run with: cargo test -p musefs-fuse -- --ignored"]
fn many_files_in_parallel_through_mount() {
    let (_backing, mnt, _session) = setup_mount();
    let songs = Arc::new(list_songs(mnt.path()));
    let references: Vec<Vec<u8>> = songs.iter().map(|p| std::fs::read(p).unwrap()).collect();
    let references = Arc::new(references);

    let n = songs.len();
    let barrier = Arc::new(Barrier::new(n));
    let handles: Vec<_> = (0..n)
        .map(|t| {
            let (songs, references, barrier) = (songs.clone(), references.clone(), barrier.clone());
            std::thread::spawn(move || {
                barrier.wait();
                for k in 0..15 {
                    let idx = (t + k) % songs.len();
                    assert_eq!(std::fs::read(&songs[idx]).unwrap(), references[idx]);
                }
            })
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }
}
