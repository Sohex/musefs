//! Headline SP1 correctness guard: a tiny-window bounded scan is equivalent (in
//! its parsed DB rows) to a legacy full-file-probe scan, for every format.

mod common;

use common::corpus::{bench_formats, format_token, generate, CorpusParams};
use musefs_db::Db;

/// One comparable track row: `(backing_path, audio_offset, audio_length, tags,
/// art)`, with tags as `(key, value, ordinal)` and art as
/// `(sha256, picture_type, description, ordinal)`.
type NormalizedTrack = (
    String,
    i64,
    i64,
    Vec<(String, String, i64)>,
    Vec<(String, i64, String, i64)>,
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
        out.push((t.backing_path, t.audio_offset, t.audio_length, tags, art));
    }
    out.sort();
    out
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
        std::env::set_var("MUSEFS_SCAN_WINDOW", "64");
        let bounded_db = Db::open_in_memory().unwrap();
        musefs_core::scan_directory(&bounded_db, dir.path()).unwrap();
        std::env::remove_var("MUSEFS_SCAN_WINDOW");
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
