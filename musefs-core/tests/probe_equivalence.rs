//! Headline SP1 correctness guard: a tiny-window bounded scan is equivalent (in
//! its parsed DB rows) to a legacy full-file-probe scan, for every format.

mod common;

use common::corpus::{CorpusParams, bench_formats, format_token, generate};
use musefs_db::Db;

/// One comparable track row: `(backing_path, audio_offset, audio_length, tags,
/// art)`, with tags as `(key, value, ordinal)` and art as
/// `(sha256, picture_type, description, ordinal)`.
type NormalizedTrack = (
    std::path::PathBuf,
    u64,
    u64,
    Vec<(String, String, u64)>,
    Vec<(String, u32, String, u64)>,
);

/// Normalize a DB to comparable rows: tracks by path, tags by (key,value,ordinal),
/// art by (sha256, picture_type, description, ordinal). Excludes raw `art.id`
/// (insertion-order rowid) but covers every other observable art field, so the
/// gate catches any drift in picture-type/description parsing too.
fn normalized(db: &Db) -> Vec<NormalizedTrack> {
    let mut out = Vec::new();
    for t in db.list_tracks().unwrap() {
        let tags: Vec<_> = db
            .get_tags(t.id)
            .unwrap()
            .into_iter()
            .map(|tg| (tg.key, tg.value, tg.ordinal))
            .collect();
        let art: Vec<_> = db
            .get_track_art(t.id)
            .unwrap()
            .into_iter()
            .map(|a| {
                let sha = db.get_art(a.art_id).unwrap().unwrap().sha256;
                (sha, a.picture_type, a.description, a.ordinal)
            })
            .collect();
        out.push((
            t.backing_path,
            t.bounds.audio_offset(),
            t.bounds.audio_length(),
            tags,
            art,
        ));
    }
    out.sort();
    out
}

/// QuickTime keyed metadata (#771) reaches the store identically through the
/// legacy whole-file probe and the bounded seek probe — which hands the readers
/// `scan.moov` alone — whether `moov` precedes or follows `mdat`. The iTunes
/// `©nam` beats the keyed title; the keyed artist, the track- and media-level
/// values and the keyed artwork fill in what the iTunes tags lack.
#[test]
fn bounded_probe_ingests_m4a_keyed_metadata_like_the_full_probe() {
    use musefs_format::fuzz_check::fixtures;
    for bytes in [
        fixtures::m4a_keyed(&[9u8; 64]),
        fixtures::m4a_keyed_moov_last(&[9u8; 64]),
    ] {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("keyed.m4a"), &bytes).unwrap();

        let oracle_db = Db::open_in_memory().unwrap();
        musefs_core::scan_directory_full_oracle(&oracle_db, dir.path()).unwrap();
        let oracle = normalized(&oracle_db);

        let bounded_db = Db::open_in_memory().unwrap();
        let mut options = musefs_core::ScanOptions::default();
        options.window = 64;
        musefs_core::scan_directory_with(&bounded_db, dir.path(), &options).unwrap();
        assert_eq!(oracle, normalized(&bounded_db));

        assert_eq!(oracle.len(), 1, "the keyed file was scanned");
        let mut tags: Vec<(&str, &str)> = oracle[0]
            .3
            .iter()
            .map(|(k, v, _)| (k.as_str(), v.as_str()))
            .collect();
        tags.sort_unstable();
        assert_eq!(
            tags,
            vec![
                ("artist", "Keyed Artist"),
                ("comment", "Keyed Comment"),
                ("player.movie.audio.mute", "1"),
                ("title", "Orig M4A"),
            ]
        );
        assert_eq!(oracle[0].4.len(), 1, "the keyed artwork became art");
    }
}

#[test]
fn bounded_probe_equivalent_to_full_for_every_format() {
    for fmt in bench_formats() {
        let dir = tempfile::tempdir().unwrap();
        let params = CorpusParams::single(fmt, /*albums*/ 2, /*tracks*/ 3);
        generate(dir.path(), &params);

        // Oracle: legacy whole-file probe.
        let oracle_db = Db::open_in_memory().unwrap();
        musefs_core::scan_directory_full_oracle(&oracle_db, dir.path()).unwrap();
        let oracle = normalized(&oracle_db);

        // Bounded scan with a 64-byte window → widen path fires on every file.
        let bounded_db = Db::open_in_memory().unwrap();
        let mut options = musefs_core::ScanOptions::default();
        options.window = 64;
        musefs_core::scan_directory_with(&bounded_db, dir.path(), &options).unwrap();
        let bounded = normalized(&bounded_db);

        assert_eq!(
            oracle,
            bounded,
            "format {}: bounded scan diverged from full-probe oracle",
            format_token(fmt)
        );
        assert!(
            !oracle.is_empty(),
            "format {}: scanned nothing",
            format_token(fmt)
        );
    }
}
