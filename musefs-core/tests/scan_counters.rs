//! Pipeline / revalidate / widen-fallback mutation guards for `scan.rs`.
//!
//! These exercise the parts of the scan pipeline that need a real `Db` and real
//! backing files: batch-flush cadence, the bounded-read widen + whole-file
//! fallback, and the revalidate skip-pass counters.

use musefs_core::{
    ScanOptions, revalidate, revalidate_with, scan_directory, scan_directory_full_oracle,
    scan_directory_with,
};
use musefs_db::Db;

/// Minimal valid FLAC: marker + last STREAMINFO (34-byte body) + audio payload.
fn flac_minimal(audio: &[u8]) -> Vec<u8> {
    let mut b = b"fLaC".to_vec();
    b.push(0x80); // last-block flag | STREAMINFO (type 0)
    b.extend_from_slice(&[0, 0, 34]);
    b.extend(std::iter::repeat_n(0u8, 34));
    b.extend_from_slice(audio);
    b
}

/// FLAC with a large PICTURE block (so the bounded probe must widen past a tiny
/// window). marker + STREAMINFO (not last) + PICTURE (last) + audio.
fn flac_with_big_art(data_len: usize, audio: &[u8]) -> Vec<u8> {
    let mut v = b"fLaC".to_vec();
    v.push(0x00); // STREAMINFO (type 0), not last
    v.extend_from_slice(&[0, 0, 34]);
    v.extend(std::iter::repeat_n(0u8, 34));

    let mut body = Vec::new();
    body.extend_from_slice(&3u32.to_be_bytes()); // picture type (front cover)
    let mime = b"image/png";
    body.extend_from_slice(&u32::try_from(mime.len()).unwrap().to_be_bytes());
    body.extend_from_slice(mime);
    body.extend_from_slice(&0u32.to_be_bytes()); // description length
    body.extend_from_slice(&0u32.to_be_bytes()); // width
    body.extend_from_slice(&0u32.to_be_bytes()); // height
    body.extend_from_slice(&0u32.to_be_bytes()); // depth
    body.extend_from_slice(&0u32.to_be_bytes()); // colors
    body.extend_from_slice(&u32::try_from(data_len).unwrap().to_be_bytes());
    // Distinct, position-sensitive bytes so a misparse is observable.
    body.extend((0u8..=200).cycle().take(data_len));
    v.push(0x86); // last-block flag (0x80) | PICTURE (0x06)
    let blen = body.len();
    v.extend_from_slice(&[
        u8::try_from((blen >> 16) & 0xFF).unwrap(),
        u8::try_from((blen >> 8) & 0xFF).unwrap(),
        u8::try_from(blen & 0xFF).unwrap(),
    ]);
    v.extend_from_slice(&body);
    v.extend_from_slice(audio);
    v
}

/// Normalize a DB to comparable `(path, audio_offset, audio_length)` rows.
fn rows(db: &Db) -> Vec<(String, u64, u64)> {
    let mut r: Vec<_> = db
        .list_tracks()
        .unwrap()
        .into_iter()
        .map(|t| {
            (
                t.backing_path,
                t.bounds.audio_offset(),
                t.bounds.audio_length(),
            )
        })
        .collect();
    r.sort();
    r
}

// === Widen / whole-file-fallback (probe_file, lines 211-235) ===

/// A FLAC whose metadata (a large PICTURE) exceeds a tiny scan window scans to
/// the SAME rows as a full-file oracle. Drives the widen loop (L213-228) and,
/// for files that never `Complete` within retries, the whole-file fallback
/// (L232). Guards L232 fallback guard `<`→`<=`/`==`/`>` (a mis-set guard either
/// re-reads or skips the fallback, changing the parsed art/offset).
// kills scan L232 `(prefix.len() as u64) < file_len`→`<=`/`==`/`>` (oracle match)
#[test]
fn widen_then_fallback_matches_oracle_under_tiny_window() {
    let dir = tempfile::tempdir().unwrap();
    // Big-art FLAC: bounded probe must widen well past a 64-byte window.
    std::fs::write(
        dir.path().join("big.flac"),
        flac_with_big_art(5000, b"AUDIOPAYLOAD-BIG"),
    )
    .unwrap();
    // Small FLAC entirely inside the window: prefix.len() == file_len, the
    // fallback guard must be false (no redundant re-read), still correct.
    std::fs::write(
        dir.path().join("small.flac"),
        flac_minimal(b"AUDIOPAYLOAD-SMALL"),
    )
    .unwrap();

    let oracle_db = Db::open_in_memory().unwrap();
    scan_directory_full_oracle(&oracle_db, dir.path()).unwrap();
    let oracle = rows(&oracle_db);

    let bounded_db = Db::open_in_memory().unwrap();
    let stats = scan_directory_with(
        &bounded_db,
        dir.path(),
        &ScanOptions {
            window: 64,
            ..Default::default()
        },
    )
    .unwrap();

    assert_eq!(stats.scanned, 2);
    assert_eq!(rows(&bounded_db), oracle, "bounded widen/fallback diverged");
    assert!(!oracle.is_empty());
}

/// A FLAC that never reaches `Complete` within the widen retries (its bounded
/// parse keeps asking for slightly more than the window grants) must still land
/// the correct bounds via the whole-file fallback at L232. We force this by
/// pinning the window tiny AND verifying the big-art file's art survives a round
/// trip identical to the oracle (covers L225 widen progress + L232 fallback).
// kills scan L225 `want + 1` (widen must make progress to reach the art body)
#[test]
fn widen_preserves_art_bytes_vs_oracle() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("art.flac"),
        flac_with_big_art(4096, b"TAILAUDIO"),
    )
    .unwrap();

    let oracle_db = Db::open_in_memory().unwrap();
    scan_directory_full_oracle(&oracle_db, dir.path()).unwrap();
    let o_track = oracle_db.list_tracks().unwrap().into_iter().next().unwrap();
    let o_art = oracle_db.get_track_art(o_track.id).unwrap();
    let o_sha = oracle_db.get_art(o_art[0].art_id).unwrap().unwrap().sha256;

    // Tiny window forces a multi-step widen to reach the 4 KiB picture body.
    let db = Db::open_in_memory().unwrap();
    scan_directory_with(
        &db,
        dir.path(),
        &ScanOptions {
            window: 16,
            ..Default::default()
        },
    )
    .unwrap();

    let track = db.list_tracks().unwrap().into_iter().next().unwrap();
    assert_eq!(track.bounds.audio_offset(), o_track.bounds.audio_offset());
    assert_eq!(track.bounds.audio_length(), o_track.bounds.audio_length());
    let art = db.get_track_art(track.id).unwrap();
    assert_eq!(art.len(), 1, "the embedded picture must survive the widen");
    let sha = db.get_art(art[0].art_id).unwrap().unwrap().sha256;
    assert_eq!(
        sha, o_sha,
        "widened art bytes must match the oracle exactly"
    );
}

// === Batch flush cadence (run_pipeline, lines 570-595) ===

/// Scanning more than BATCH_FILES (256) tiny files persists EVERY file exactly
/// once. The file-count flush threshold (`batch.len() >= BATCH_FILES`, L575/585)
/// fires mid-corpus, so a broken flush cadence (`>=`→`<`, `||`→`&&`) would drop
/// or duplicate writes — caught by the exact scanned-count and track-count.
// kills scan L575/585 `batch.len() >= BATCH_FILES` cadence; L573/583 batch_bytes accumulation
#[test]
fn scans_more_than_batch_files_persists_all_once() {
    let n = 300usize; // > BATCH_FILES (256), so at least one mid-scan flush
    let dir = tempfile::tempdir().unwrap();
    for i in 0..n {
        // Distinct audio so each file is a distinct row (no dedupe surprises).
        std::fs::write(
            dir.path().join(format!("t{i:04}.flac")),
            flac_minimal(format!("AUDIO-{i}").as_bytes()),
        )
        .unwrap();
    }
    let db = Db::open_in_memory().unwrap();
    let stats = scan_directory_with(
        &db,
        dir.path(),
        &ScanOptions {
            jobs: 4,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(stats.scanned, n as u64, "every file must be scanned once");
    assert_eq!(
        db.list_tracks().unwrap().len(),
        n,
        "every file persisted once"
    );

    // Idempotent re-scan: still exactly n rows (catches duplicate writes from a
    // wrong flush cadence).
    let stats2 = scan_directory_with(
        &db,
        dir.path(),
        &ScanOptions {
            jobs: 4,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(stats2.scanned, n as u64);
    assert_eq!(db.list_tracks().unwrap().len(), n);
}

/// Byte-threshold flushing: with a tiny `batch_bytes` and art-bearing
/// files, the byte branch (`batch_bytes >= cap`, L575/585) drives flushes. All
/// tracks and their art must persist. Guards the `batch_bytes +=` accumulation
/// (L573/583, `+=`→`*=`) and the `||` flush disjunction.
// kills scan L573/583 `batch_bytes += unit.weight` `+=`→`*=`; L575/585 byte branch
#[test]
fn byte_threshold_flush_persists_all_art() {
    let n = 20usize;
    let dir = tempfile::tempdir().unwrap();
    for i in 0..n {
        std::fs::write(
            dir.path().join(format!("a{i:03}.flac")),
            flac_with_big_art(64, format!("AUD-{i}").as_bytes()),
        )
        .unwrap();
    }
    // Cap below a couple files' cumulative art so the byte branch flushes often.
    let db = Db::open_in_memory().unwrap();
    let stats = scan_directory_with(
        &db,
        dir.path(),
        &ScanOptions {
            jobs: 4,
            batch_bytes: 100,
            ..Default::default()
        },
    )
    .unwrap();

    assert_eq!(stats.scanned, n as u64);
    let tracks = db.list_tracks().unwrap();
    assert_eq!(tracks.len(), n);
    for t in tracks {
        assert_eq!(
            db.get_track_art(t.id).unwrap().len(),
            1,
            "each track's art must persist through byte-threshold flushing"
        );
    }
}

// === Revalidate counters (revalidate_with, lines 667-712) ===

/// Revalidating an unchanged tree buckets every file as `unchanged`. A count of
/// N (not 0) kills `unchanged += 1`→`-=`/`*=` (both give 0/garbage from 0).
// kills scan L682 `unchanged += 1`→`-=`/`*=`
#[test]
fn revalidate_unchanged_count_matches_file_count() {
    let n = 5usize;
    let dir = tempfile::tempdir().unwrap();
    for i in 0..n {
        std::fs::write(
            dir.path().join(format!("u{i}.flac")),
            flac_minimal(format!("AUDIO-{i}").as_bytes()),
        )
        .unwrap();
    }
    let db = Db::open_in_memory().unwrap();
    scan_directory(&db, dir.path()).unwrap();

    let stats = revalidate_with(&db, dir.path(), &ScanOptions::default()).unwrap();
    assert_eq!(
        stats.unchanged, n as u64,
        "all files unchanged → unchanged == N"
    );
    assert_eq!(stats.updated, 0);
    assert_eq!(stats.pruned, 0);
    assert_eq!(stats.failed, 0);
}

/// `RevalidateStats.failed == scan.failed + skip_failed` (L711). We produce a
/// nonzero `scan.failed` with `skip_failed == 0`: an unreadable (chmod 000) new
/// `.flac` is a *changed* candidate (not in the existing set), passes the
/// skip-pass (metadata + canonicalize succeed), then fails inside `probe_file`
/// when `File::open` is denied → `scan.failed += 1`. With `skip_failed == 0`,
/// `+`→`*` gives `1*0 == 0 != 1` → killed. (`+`→`-` gives `1-0 == 1`, NOT
/// distinguished — see report.)
// kills scan L711 `scan.failed + skip_failed` `+`→`*` (nonzero scan.failed, zero skip_failed)
#[test]
fn revalidate_failed_carries_scan_failure() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("ok.flac"), flac_minimal(b"AUDIO-OK")).unwrap();

    let db = Db::open_in_memory().unwrap();
    let s0 = scan_directory(&db, dir.path()).unwrap();
    assert_eq!(s0.scanned, 1);
    assert_eq!(s0.failed, 0);

    // Add a NEW unreadable .flac: it is a changed candidate (not yet in the DB),
    // survives the skip-pass, then probe_file's File::open is denied → failed.
    let denied = dir.path().join("denied.flac");
    std::fs::write(&denied, flac_minimal(b"AUDIO-DENIED")).unwrap();
    std::fs::set_permissions(&denied, std::fs::Permissions::from_mode(0o000)).unwrap();

    // chmod-000 denial is meaningless when running as root (root bypasses file
    // permissions) — e.g. the FreeBSD CI/VM runs as root. Probe it: if the file
    // still opens despite mode 000, permissions aren't enforced for us, so this
    // test can't exercise the probe_file failure path. Skip rather than fail.
    if std::fs::File::open(&denied).is_ok() {
        eprintln!(
            "skipping revalidate_failed_carries_scan_failure: file permissions not enforced (running as root?)"
        );
        std::fs::set_permissions(&denied, std::fs::Permissions::from_mode(0o644)).unwrap();
        return;
    }

    let stats = revalidate(&db, dir.path()).unwrap();

    // Restore perms so the TempDir can be cleaned up.
    std::fs::set_permissions(&denied, std::fs::Permissions::from_mode(0o644)).unwrap();

    assert_eq!(stats.unchanged, 1, "ok.flac is unchanged");
    assert_eq!(
        stats.failed, 1,
        "failed must carry the re-probe scan failure (skip_failed == 0)"
    );
}
