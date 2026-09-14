mod common;

use std::collections::BTreeMap;
use std::time::Duration;

use musefs_core::{MountConfig, Musefs, scan_directory};
use musefs_db::{Db, Tag};

use common::corpus::{CorpusParams, Format, Target, prepare};
use proptest::prelude::*;

/// A small single-album FLAC corpus with `n` tracks. The returned `Target` owns
/// the tempdir — keep it alive for the whole test.
fn small_corpus(n: usize) -> Target {
    prepare(&CorpusParams::single(Format::Flac, 1, n))
}

fn config() -> MountConfig {
    let mut config = MountConfig::default();
    config.template = "$artist/$album/$title".into();
    config.poll_interval = Duration::ZERO;
    config.case_insensitive = false;
    config
}

fn config_ci() -> MountConfig {
    let mut new_config = config();
    new_config.case_insensitive = true;
    new_config.read_ahead_budget = 64 * 1024 * 1024;
    new_config.read_ahead_prefetch = false;
    new_config.skip_on_missing = false;
    new_config.trust_backing_mtime = false;
    new_config
}

fn config_skip() -> MountConfig {
    let mut new_config = config();
    new_config.skip_on_missing = true;
    new_config.trust_backing_mtime = false;
    new_config
}

/// (rendered tree path -> inode) for every FILE, walking from root. Tests compare
/// only the PATH KEYS across two independent Musefs instances: their inode-allocator
/// histories differ, so inode numbers legitimately differ between instances. (Inode
/// stability within one instance across refreshes is gated by the Stage B B5 debug_assert.)
fn tree_fingerprint(fs: &Musefs) -> BTreeMap<String, u64> {
    let mut out = BTreeMap::new();
    let mut stack = vec![(1u64, String::new())];
    while let Some((ino, prefix)) = stack.pop() {
        for (name, child, is_dir) in fs.readdir(ino).unwrap() {
            let path = if prefix.is_empty() {
                name.clone()
            } else {
                format!("{prefix}/{name}")
            };
            if is_dir {
                stack.push((child, path));
            } else {
                out.insert(path, child);
            }
        }
    }
    out
}

#[test]
fn incremental_refresh_matches_full_rebuild_over_edits() {
    let target = small_corpus(8);
    let db_path = target.db_path.clone();
    let corpus = target.corpus_dir.clone();

    let db = Db::open(&db_path).unwrap();
    scan_directory(&db, &corpus).unwrap();
    let fs = Musefs::open(Db::open(&db_path).unwrap(), config()).unwrap();

    let writer = Db::open(&db_path).unwrap();
    let ids: Vec<i64> = writer.list_tracks().unwrap().iter().map(|t| t.id).collect();

    writer
        .replace_tags(
            ids[0],
            &[Tag::new("ARTIST", "Zed", 0), Tag::new("TITLE", "moved", 0)],
        )
        .unwrap();
    fs.poll_refresh().unwrap();
    writer
        .replace_tags(ids[1], &[Tag::new("ALBUM", "NewAlbum", 0)])
        .unwrap();
    fs.poll_refresh().unwrap();
    writer.delete_track(ids[2]).unwrap();
    fs.poll_refresh().unwrap();

    let reference = Musefs::open(Db::open(&db_path).unwrap(), config()).unwrap();

    assert_eq!(
        tree_fingerprint(&fs).keys().collect::<Vec<_>>(),
        tree_fingerprint(&reference).keys().collect::<Vec<_>>(),
        "incremental and full-rebuild paths must match"
    );
}

#[test]
fn incremental_skip_on_missing_matches_full_rebuild_over_key_loss_and_gain() {
    let target = small_corpus(4);
    let db_path = target.db_path.clone();
    let corpus = target.corpus_dir.clone();

    let db = Db::open(&db_path).unwrap();
    scan_directory(&db, &corpus).unwrap();
    let fs = Musefs::open(Db::open(&db_path).unwrap(), config_skip()).unwrap();

    let writer = Db::open(&db_path).unwrap();
    let ids: Vec<i64> = writer.list_tracks().unwrap().iter().map(|t| t.id).collect();

    let full = |label: &str, fs: &Musefs| {
        let reference = Musefs::open(Db::open(&db_path).unwrap(), config_skip()).unwrap();
        let inc = tree_fingerprint(fs);
        assert_eq!(
            inc.keys().collect::<Vec<_>>(),
            tree_fingerprint(&reference).keys().collect::<Vec<_>>(),
            "{label}: incremental must match full rebuild"
        );
        inc.len()
    };

    assert_eq!(full("baseline", &fs), 4);

    // Key loss: drop TITLE (a top-level template field) from ids[0]. Full rebuild
    // skips it, so the incremental path must drop it too.
    writer
        .replace_tags(
            ids[0],
            &[Tag::new("ARTIST", "Zed", 0), Tag::new("ALBUM", "Al", 0)],
        )
        .unwrap();
    fs.poll_refresh().unwrap();
    assert_eq!(full("key loss", &fs), 3);

    // Key gain: restore TITLE; the track must reappear.
    writer
        .replace_tags(
            ids[0],
            &[
                Tag::new("ARTIST", "Zed", 0),
                Tag::new("ALBUM", "Al", 0),
                Tag::new("TITLE", "Back", 0),
            ],
        )
        .unwrap();
    fs.poll_refresh().unwrap();
    assert_eq!(full("key gain", &fs), 4);
}

#[test]
fn case_insensitive_refresh_merges_and_matches_full_rebuild() {
    let target = small_corpus(2);
    let db_path = target.db_path.clone();
    let corpus = target.corpus_dir.clone();

    let db = Db::open(&db_path).unwrap();
    scan_directory(&db, &corpus).unwrap();

    // Make the two tracks' artists differ only by case (same album so they share
    // a parent): under folding the artist dir must MERGE.
    let writer = Db::open(&db_path).unwrap();
    let ids: Vec<i64> = writer.list_tracks().unwrap().iter().map(|t| t.id).collect();
    writer
        .replace_tags(
            ids[0],
            &[
                Tag::new("ARTIST", "Foo", 0),
                Tag::new("ALBUM", "Al", 0),
                Tag::new("TITLE", "One", 0),
            ],
        )
        .unwrap();
    writer
        .replace_tags(
            ids[1],
            &[
                Tag::new("ARTIST", "foo", 0),
                Tag::new("ALBUM", "Al", 0),
                Tag::new("TITLE", "Two", 0),
            ],
        )
        .unwrap();

    let fs = Musefs::open(Db::open(&db_path).unwrap(), config_ci()).unwrap();

    // Exactly one top-level artist directory (the merged "Foo"/"foo").
    let fp = tree_fingerprint(&fs);
    let top_dirs: std::collections::BTreeSet<String> = fp
        .keys()
        .map(|p| p.split('/').next().unwrap().to_string())
        .collect();
    assert_eq!(
        top_dirs.len(),
        1,
        "case-variant artists must merge into one dir"
    );

    // An external edit is still picked up - incremental is bypassed, so this goes
    // through a full folded rebuild - and the result matches a fresh folded build.
    writer
        .replace_tags(
            ids[1],
            &[
                Tag::new("ARTIST", "foo", 0),
                Tag::new("ALBUM", "Al", 0),
                Tag::new("TITLE", "Renamed", 0),
            ],
        )
        .unwrap();
    fs.poll_refresh().unwrap();

    let reference = Musefs::open(Db::open(&db_path).unwrap(), config_ci()).unwrap();
    assert_eq!(
        tree_fingerprint(&fs).keys().collect::<Vec<_>>(),
        tree_fingerprint(&reference).keys().collect::<Vec<_>>(),
        "case-insensitive refresh (full rebuild) must match a fresh folded build"
    );
}

#[test]
fn non_render_column_edit_is_noop_refresh() {
    // Re-running scan_directory over an unchanged corpus changes no rendered
    // path, so the tree must be identical before and after.
    let target = small_corpus(4);
    let db_path = target.db_path.clone();
    let corpus = target.corpus_dir.clone();
    let db = Db::open(&db_path).unwrap();
    scan_directory(&db, &corpus).unwrap();
    let fs = Musefs::open(Db::open(&db_path).unwrap(), config()).unwrap();

    // Re-scan: no rendered path changes.
    let db2 = Db::open(&db_path).unwrap();
    scan_directory(&db2, &corpus).unwrap();

    let before = tree_fingerprint(&fs);
    let rebuilt = fs.poll_refresh().unwrap();
    let after = tree_fingerprint(&fs);
    assert_eq!(before, after, "non-render edit must not change the tree");
    let _ = rebuilt; // tree-equality is the gate, not the bool
}

#[test]
fn format_only_change_notifies_old_inode() {
    let target = small_corpus(2);
    let db_path = target.db_path.clone();
    let corpus = target.corpus_dir.clone();
    let db = Db::open(&db_path).unwrap();
    scan_directory(&db, &corpus).unwrap();
    let fs = Musefs::open(Db::open(&db_path).unwrap(), config()).unwrap();

    let writer = Db::open(&db_path).unwrap();
    let id = writer.list_tracks().unwrap()[0].id;
    let old_ino = fs.lookup_track_inode_for_test(id).unwrap();

    // Force a format change directly (no tags trigger), bumping data_version.
    writer
        .set_format_for_test(id, musefs_db::Format::Mp3)
        .unwrap();

    let mut notified = Vec::new();
    fs.poll_refresh_notify(|ino| notified.push(ino)).unwrap();

    assert!(
        notified.contains(&old_ino),
        "format-only move must invalidate the old inode (extension changed)"
    );
}

#[derive(Clone, Debug)]
enum Op {
    Retag(usize, String, String), // retag the i-th LIVE track (forces collisions -> moves)
    Delete(usize),                // delete the i-th LIVE track (remove-cascade + prune)
    Add(String, String),          // add a brand-new DB track row (added-side propagation)
}

#[test]
fn apply_failure_falls_back_to_full_rebuild() {
    let target = small_corpus(4);
    let db_path = target.db_path.clone();
    let corpus = target.corpus_dir.clone();
    let db = Db::open(&db_path).unwrap();
    scan_directory(&db, &corpus).unwrap();
    let fs = Musefs::open(Db::open(&db_path).unwrap(), config()).unwrap();
    let writer = Db::open(&db_path).unwrap();
    let id = writer.list_tracks().unwrap()[0].id;
    writer
        .replace_tags(id, &[Tag::new("TITLE", "moved", 0)])
        .unwrap();

    fs.force_apply_failure_for_test(true);
    fs.poll_refresh().unwrap();

    let reference = Musefs::open(Db::open(&db_path).unwrap(), config()).unwrap();
    // `tree_fingerprint` is a BTreeMap, so `into_keys` already yields sorted keys.
    let fs_keys: Vec<String> = tree_fingerprint(&fs).into_keys().collect();
    let ref_keys: Vec<String> = tree_fingerprint(&reference).into_keys().collect();
    assert_eq!(
        fs_keys, ref_keys,
        "fallback full rebuild must produce a tree identical to a fresh open"
    );
}

#[test]
fn changelog_gap_falls_back_to_full_rebuild() {
    let target = small_corpus(4);
    let db_path = target.db_path.clone();
    let corpus = target.corpus_dir.clone();
    let db = Db::open(&db_path).unwrap();
    scan_directory(&db, &corpus).unwrap();
    let fs = Musefs::open(Db::open(&db_path).unwrap(), config()).unwrap();

    let writer = Db::open(&db_path).unwrap();
    let ids: Vec<i64> = writer.list_tracks().unwrap().iter().map(|t| t.id).collect();
    writer
        .replace_tags(ids[0], &[Tag::new("TITLE", "moved-by-gap", 0)])
        .unwrap();
    // Simulate the ring having pruned past the mount's watermark: drop every
    // retained row. The next poll must detect the gap and full-rebuild.
    let max_seq = writer.changelog_since(0).unwrap().max_seq;
    writer.delete_changelog_through_for_test(max_seq).unwrap();

    assert!(fs.poll_refresh().unwrap());
    assert_eq!(
        fs.gap_fallbacks_for_test(),
        1,
        "a fully truncated ring must be detected as a gap"
    );
    let reference = Musefs::open(Db::open(&db_path).unwrap(), config()).unwrap();
    assert_eq!(
        tree_fingerprint(&fs).into_keys().collect::<Vec<_>>(),
        tree_fingerprint(&reference).into_keys().collect::<Vec<_>>(),
        "gap fallback must produce a tree identical to a fresh open"
    );
}

#[test]
fn removed_track_is_pruned_and_refresh_recovers_after_gap() {
    // After a gap-driven full rebuild, subsequent incremental refreshes still
    // work: the watermark re-anchors to the ring and deletes propagate.
    let target = small_corpus(4);
    let db_path = target.db_path.clone();
    let corpus = target.corpus_dir.clone();
    let db = Db::open(&db_path).unwrap();
    scan_directory(&db, &corpus).unwrap();
    let fs = Musefs::open(Db::open(&db_path).unwrap(), config()).unwrap();
    let writer = Db::open(&db_path).unwrap();
    let ids: Vec<i64> = writer.list_tracks().unwrap().iter().map(|t| t.id).collect();

    let max_seq = writer.changelog_since(0).unwrap().max_seq;
    writer.delete_changelog_through_for_test(max_seq).unwrap();
    writer.delete_track(ids[0]).unwrap();
    assert!(fs.poll_refresh().unwrap());
    // Truncation-then-edit leaves min_seq == last_seq + 1: contiguous, NOT a
    // gap. Both polls must stay on the incremental path (and the second one
    // starts from a retained, mutated-in-place snapshot).
    assert_eq!(
        fs.gap_fallbacks_for_test(),
        0,
        "an adjacent (min_seq == last_seq + 1) ring read is not a gap"
    );

    writer.delete_track(ids[1]).unwrap();
    assert!(fs.poll_refresh().unwrap());
    assert_eq!(fs.gap_fallbacks_for_test(), 0);

    let reference = Musefs::open(Db::open(&db_path).unwrap(), config()).unwrap();
    assert_eq!(
        tree_fingerprint(&fs).into_keys().collect::<Vec<_>>(),
        tree_fingerprint(&reference).into_keys().collect::<Vec<_>>()
    );
}

#[test]
fn pruned_ring_prefix_is_a_gap_and_full_rebuild_recovers_lost_change() {
    // A TRUE prune gap: the ring still holds rows, but its window starts past
    // the watermark + 1. The pruned prefix held a real change; only the gap
    // path (full rebuild) can recover it.
    let target = small_corpus(4);
    let db_path = target.db_path.clone();
    let corpus = target.corpus_dir.clone();
    let db = Db::open(&db_path).unwrap();
    scan_directory(&db, &corpus).unwrap();
    let fs = Musefs::open(Db::open(&db_path).unwrap(), config()).unwrap();
    let writer = Db::open(&db_path).unwrap();
    let ids: Vec<i64> = writer.list_tracks().unwrap().iter().map(|t| t.id).collect();

    let last_seq = writer.changelog_since(0).unwrap().max_seq; // == fs watermark
    writer
        .replace_tags(ids[0], &[Tag::new("TITLE", "lost-in-pruned-prefix", 0)])
        .unwrap();
    let x_rows = writer.changelog_since(last_seq).unwrap().max_seq;
    writer
        .replace_tags(ids[1], &[Tag::new("TITLE", "still-in-ring", 0)])
        .unwrap();
    // Prune exactly X's rows: the ring now starts at last_seq + (x_rows-last_seq) + 1
    // > last_seq + 1, and X's change is no longer derivable from it.
    writer.delete_changelog_through_for_test(x_rows).unwrap();

    assert!(fs.poll_refresh().unwrap());
    assert_eq!(
        fs.gap_fallbacks_for_test(),
        1,
        "a pruned-prefix ring (min_seq > last_seq + 1) must be detected as a gap"
    );
    let reference = Musefs::open(Db::open(&db_path).unwrap(), config()).unwrap();
    assert_eq!(
        tree_fingerprint(&fs).into_keys().collect::<Vec<_>>(),
        tree_fingerprint(&reference).into_keys().collect::<Vec<_>>(),
        "the gap rebuild must recover the change lost from the pruned prefix"
    );
}

/// Track-id reuse (#678). Without `AUTOINCREMENT`, SQLite hands out
/// `max(rowid) + 1`, so deleting the highest-numbered track and ingesting another
/// file gave the newcomer the freed id, and the incremental refresh, already
/// holding that id, could keep serving the old file under it. The DB layer pins
/// that no id is reused. This pins the substitution where it bit, with the
/// delete and the ingest both landing between two polls.
#[test]
fn a_freed_id_is_not_handed_to_the_next_track_between_polls() {
    let target = small_corpus(2);
    let db_path = target.db_path.clone();
    let corpus = target.corpus_dir.clone();
    let db = Db::open(&db_path).unwrap();
    scan_directory(&db, &corpus).unwrap();
    let fs = Musefs::open(Db::open(&db_path).unwrap(), config()).unwrap();

    let tracks = db.list_tracks().unwrap();
    let freed = tracks.iter().map(|t| t.id).max().unwrap();
    let old_path = tracks
        .iter()
        .find(|t| t.id == freed)
        .unwrap()
        .backing_path
        .clone();

    // The substitute is the same bytes under a new name, so only the id and the
    // path tell the two rows apart: exactly what a reused id hid.
    db.delete_track(freed).unwrap();
    let new_path = old_path.with_file_name("substitute.flac");
    std::fs::rename(&old_path, &new_path).unwrap();
    scan_directory(&db, &corpus).unwrap();
    let newcomer = db
        .list_tracks()
        .unwrap()
        .into_iter()
        .find(|t| t.backing_path.ends_with("substitute.flac"))
        .expect("the substitute was ingested");
    assert!(
        newcomer.id > freed,
        "id {} reused the freed {freed}",
        newcomer.id
    );

    assert!(fs.poll_refresh().unwrap());
    let reference = Musefs::open(Db::open(&db_path).unwrap(), config()).unwrap();
    let live = tree_fingerprint(&fs);
    assert_eq!(
        live.keys().collect::<Vec<_>>(),
        tree_fingerprint(&reference).keys().collect::<Vec<_>>(),
        "the substitution must reach the live tree"
    );
    // Every file the live tree lists must serve: an entry still resolving to the
    // renamed-away path fails its read.
    for &ino in live.values() {
        let size = fs.getattr(ino).unwrap().size;
        let served = fs.read(ino, None, 0, size).unwrap();
        assert_eq!(served.len() as u64, size);
    }
}

/// A track id rewritten in place must leave the live tree the way a fresh open
/// would see it (#762). The schema refuses the rekey, so this drops the refusal
/// for the one statement — the shape a `writable_schema` writer produces — and
/// checks that the changelog alone still carries the refresh to the right tree.
/// Before it logged `OLD.id`, the incremental path saw the new id as an addition
/// and never saw the old one leave, so the mount listed both.
#[test]
fn a_rekeyed_track_leaves_no_ghost_in_the_live_tree() {
    let target = small_corpus(2);
    let db_path = target.db_path.clone();
    let corpus = target.corpus_dir.clone();
    let db = Db::open(&db_path).unwrap();
    scan_directory(&db, &corpus).unwrap();
    let fs = Musefs::open(Db::open(&db_path).unwrap(), config()).unwrap();
    let ids: Vec<i64> = db.list_tracks().unwrap().iter().map(|t| t.id).collect();

    // Childless first: with foreign keys enforced, a track that still has
    // children cannot be rekeyed at all.
    let raw = rusqlite::Connection::open(&db_path).unwrap();
    raw.pragma_update(None, "foreign_keys", true).unwrap();
    for table in ["tags", "track_art", "structural_blocks"] {
        raw.execute(
            &format!("DELETE FROM {table} WHERE track_id = ?1"),
            [ids[0]],
        )
        .unwrap();
    }
    assert!(fs.poll_refresh().unwrap());

    let refusal: Vec<String> = raw
        .prepare("SELECT sql FROM sqlite_master WHERE name = 'tracks_reject_rekey'")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    raw.execute_batch("DROP TRIGGER IF EXISTS tracks_reject_rekey")
        .unwrap();
    raw.execute(
        "UPDATE tracks SET id = (SELECT max(id) FROM tracks) + 100 WHERE id = ?1",
        [ids[0]],
    )
    .unwrap();
    // Restored verbatim, so the reference open below passes schema identity.
    for sql in &refusal {
        raw.execute_batch(sql).unwrap();
    }

    assert!(fs.poll_refresh().unwrap());
    let reference = Musefs::open(Db::open(&db_path).unwrap(), config()).unwrap();
    assert_eq!(
        tree_fingerprint(&fs).into_keys().collect::<Vec<_>>(),
        tree_fingerprint(&reference).into_keys().collect::<Vec<_>>(),
        "the old id must leave the tree when the row is rekeyed"
    );
}

/// A changelog row whose `track_id` is not an integer (#760), from a store
/// written with its constraints off. It used to be a conversion error, and an
/// error moves no watermark, so every later poll re-read the same window and
/// failed on the same row: the mount stopped picking up external edits. The
/// refresh cannot tell which track the row named, so it treats it as a gap.
#[test]
fn a_malformed_changelog_row_falls_back_instead_of_stalling() {
    let target = small_corpus(2);
    let db_path = target.db_path.clone();
    let corpus = target.corpus_dir.clone();
    let db = Db::open(&db_path).unwrap();
    scan_directory(&db, &corpus).unwrap();
    let fs = Musefs::open(Db::open(&db_path).unwrap(), config()).unwrap();
    let writer = Db::open(&db_path).unwrap();
    let ids: Vec<i64> = writer.list_tracks().unwrap().iter().map(|t| t.id).collect();

    let raw = rusqlite::Connection::open(&db_path).unwrap();
    raw.pragma_update(None, "ignore_check_constraints", true)
        .unwrap();
    raw.execute(
        "INSERT INTO track_changes (track_id) VALUES ('not an id')",
        [],
    )
    .unwrap();
    writer
        .replace_tags(ids[0], &[Tag::new("TITLE", "past-the-bad-row", 0)])
        .unwrap();

    assert!(
        fs.poll_refresh().unwrap(),
        "the poll must refresh, not fail"
    );
    assert_eq!(fs.gap_fallbacks_for_test(), 1, "an unreadable row is a gap");

    // Not stuck: the watermark moved past the bad row, so the next edit is an
    // ordinary incremental refresh.
    writer
        .replace_tags(ids[1], &[Tag::new("TITLE", "after-the-gap", 0)])
        .unwrap();
    assert!(fs.poll_refresh().unwrap());
    assert_eq!(fs.gap_fallbacks_for_test(), 1);

    let reference = Musefs::open(Db::open(&db_path).unwrap(), config()).unwrap();
    assert_eq!(
        tree_fingerprint(&fs).into_keys().collect::<Vec<_>>(),
        tree_fingerprint(&reference).into_keys().collect::<Vec<_>>(),
    );
}

#[test]
fn empty_ring_with_zero_watermark_polls_incremental() {
    // A data_version bump with no changelog rows and no watermark (the ring was
    // empty at open) is NOT a gap: nothing can have been missed.
    let target = small_corpus(2);
    let db_path = target.db_path.clone();
    let corpus = target.corpus_dir.clone();
    let db = Db::open(&db_path).unwrap();
    scan_directory(&db, &corpus).unwrap();
    let pre = Db::open(&db_path).unwrap();
    let max_seq = pre.changelog_since(0).unwrap().max_seq;
    pre.delete_changelog_through_for_test(max_seq).unwrap();

    // Opened on an empty ring: watermark 0.
    let fs = Musefs::open(Db::open(&db_path).unwrap(), config()).unwrap();
    // An orphan art insert bumps data_version without touching `tracks`, so the
    // ring stays empty.
    let writer = Db::open(&db_path).unwrap();
    writer
        .upsert_art(&musefs_db::NewArt { data: vec![0u8; 8] })
        .unwrap();

    assert!(fs.poll_refresh().unwrap());
    assert_eq!(
        fs.gap_fallbacks_for_test(),
        0,
        "empty ring + zero watermark must stay on the incremental path"
    );
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]
    #[test]
    fn incremental_equivalent_to_full_under_random_edits(
        ops in proptest::collection::vec(
            prop_oneof![
                (0usize..8, "[A-B]", "[x-y]").prop_map(|(i, a, t)| Op::Retag(i, a, t)),
                (0usize..8).prop_map(Op::Delete),
                ("[A-B]", "[x-y]").prop_map(|(a, t)| Op::Add(a, t)),
            ], 0..24)
    ) {
        let target = small_corpus(6);
        let db_path = target.db_path.clone();
        let corpus = target.corpus_dir.clone();
        let db = Db::open(&db_path).unwrap();
        scan_directory(&db, &corpus).unwrap();
        let fs = Musefs::open(Db::open(&db_path).unwrap(), config()).unwrap();
        let writer = Db::open(&db_path).unwrap();
        let mut add_seq = 0u32;

        for op in ops {
            // Re-query the live id set each step (deletes/adds change it).
            let live: Vec<i64> = writer.list_tracks().unwrap().iter().map(|t| t.id).collect();
            // ARTIST is fixed to "X" throughout; album/title carry the random
            // variation. Artist-level collisions are out of scope here — the
            // album+title space already drives disambiguation and path moves.
            match op {
                Op::Retag(i, album, title) if !live.is_empty() => {
                    writer.replace_tags(live[i % live.len()], &[
                        Tag::new("ARTIST", "X", 0),
                        Tag::new("ALBUM", &album, 0),
                        Tag::new("TITLE", &title, 0),
                    ]).unwrap();
                }
                Op::Delete(i) if !live.is_empty() => {
                    writer.delete_track(live[i % live.len()]).unwrap();
                }
                Op::Add(album, title) => {
                    add_seq += 1;
                    // DB-only track: tree-building never reads the backing file, and
                    // both fs and reference read the same DB, so equivalence holds.
                    let new = musefs_db::NewTrack {
                        backing_path: std::path::PathBuf::from(format!(
                            "/virt/added-{add_seq}.flac"
                        )),
                        format: musefs_db::Format::Flac,
                        audio_offset: 0, audio_length: 1, backing_size: 1, backing_mtime_ns: 0, backing_ctime_ns: 0,
                        backing_ino: None,
};
                    // Surface DB errors instead of vacuously skipping the op.
                    let id = writer.upsert_track(&new).unwrap();
                    writer
                        .replace_tags(
                            id,
                            &[
                                Tag::new("ARTIST", "X", 0),
                                Tag::new("ALBUM", &album, 0),
                                Tag::new("TITLE", &title, 0),
                            ],
                        )
                        .unwrap();
                }
                _ => {}
            }
            fs.poll_refresh().unwrap();
            let reference = Musefs::open(Db::open(&db_path).unwrap(), config()).unwrap();
            let incr_keys: Vec<String> = tree_fingerprint(&fs).into_keys().collect();
            let full_keys: Vec<String> = tree_fingerprint(&reference).into_keys().collect();
            prop_assert_eq!(incr_keys, full_keys);
        }
    }
}

// A ctime-only change (mtime forged back after an in-place same-size rewrite)
// must NOT be skipped as "unchanged": revalidate re-probes it.
#[test]
fn revalidate_reprobes_on_ctime_only_change() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("a.flac");
    common::write_flac(&src, &["TITLE=Old"], &[0xAB; 4096]);
    let db_path = dir.path().join("m.db");
    {
        let db = Db::open(&db_path).unwrap();
        scan_directory(&db, dir.path()).unwrap();
    }
    let original_modified = std::fs::metadata(&src).unwrap().modified().unwrap();

    // Rewrite in place (same size, new tag), then forge mtime back. ctime moved.
    common::write_flac(&src, &["TITLE=New"], &[0xCD; 4096]);
    let f = std::fs::OpenOptions::new().write(true).open(&src).unwrap();
    f.set_times(std::fs::FileTimes::new().set_modified(original_modified))
        .unwrap();
    drop(f);

    let db = Db::open(&db_path).unwrap();
    let stats = musefs_core::revalidate(&db, dir.path()).unwrap();
    assert_eq!(stats.updated, 1, "ctime-only change must be re-probed");
}

/// #674's repopulation path. A store migrated into V4 has no recorded inode on
/// any row, and `matches_live` has to ignore the field for those — so the stamp
/// passes on three columns where it should pass on four. Revalidate is what
/// closes that gap, alongside the structural and checksum backfills it already
/// covered, so it must re-probe a row whose inode is missing even though every
/// other field says the file is unchanged.
#[test]
fn revalidate_reprobes_a_row_with_no_recorded_inode() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("a.flac");
    common::write_flac(&src, &["TITLE=T"], &[0xAB; 4096]);
    let db_path = dir.path().join("m.db");
    {
        let db = Db::open(&db_path).unwrap();
        scan_directory(&db, dir.path()).unwrap();
    }

    let db = Db::open(&db_path).unwrap();
    let id = db.list_tracks().unwrap()[0].id;
    assert!(
        db.get_track(id).unwrap().unwrap().backing_ino.is_some(),
        "a fresh scan records the inode"
    );

    // Rewind the column to the sentinel by upserting the row as a build older
    // than #674 would have written it — which is the shape V4 leaves every
    // existing row in, reached through the public writer rather than by
    // standing up a migrated store.
    let before = db.get_track(id).unwrap().unwrap();
    db.upsert_track(&musefs_db::NewTrack {
        backing_path: before.backing_path.clone(),
        format: before.format,
        audio_offset: before.bounds.audio_offset(),
        audio_length: before.bounds.audio_length(),
        backing_size: before.backing_size,
        backing_mtime_ns: before.backing_mtime_ns,
        backing_ctime_ns: before.backing_ctime_ns,
        backing_ino: None,
    })
    .unwrap();
    assert_eq!(db.get_track(id).unwrap().unwrap().backing_ino, None);

    // Nothing about the file changed, so only the missing inode can make this
    // re-probe.
    let stats = musefs_core::revalidate(&db, dir.path()).unwrap();
    assert_eq!(stats.updated, 1, "a row with no inode must be re-probed");
    assert!(
        db.get_track(id).unwrap().unwrap().backing_ino.is_some(),
        "and the re-probe must fill it in"
    );

    // Idempotent: with the inode recorded, the same file is skipped again.
    let stats = musefs_core::revalidate(&db, dir.path()).unwrap();
    assert_eq!(stats.updated, 0, "a complete row is unchanged");
}

#[test]
fn revalidate_changed_file_refreshes_layer_a_preserves_layer_b() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open_in_memory().unwrap();
    let path = dir.path().join("a.flac");
    common::write_flac(&path, &["TITLE=A"], &[0xAA; 30]);
    scan_directory(&db, dir.path()).unwrap();
    let id = db.list_tracks().unwrap()[0].id;
    db.replace_tags(id, &[Tag::new("title", "Curated", 0)])
        .unwrap();

    common::write_flac(&path, &["TITLE=B-on-disk"], &[0xBB; 40]);
    let stats = musefs_core::revalidate(&db, dir.path()).unwrap();

    assert_eq!(stats.updated, 1);
    assert_eq!(stats.pruned, 0);
    let tags = db.get_tags(id).unwrap();
    assert_eq!(tags[0].value, "Curated");
}

#[test]
fn revalidate_ignores_new_files() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open_in_memory().unwrap();
    common::write_flac(&dir.path().join("a.flac"), &["TITLE=A"], &[0xAA; 30]);
    scan_directory(&db, dir.path()).unwrap();

    common::write_flac(&dir.path().join("b.flac"), &["TITLE=B"], &[0xBB; 40]);
    let stats = musefs_core::revalidate(&db, dir.path()).unwrap();

    assert_eq!(stats.updated, 0);
    assert_eq!(db.list_tracks().unwrap().len(), 1);
}

#[test]
fn revalidate_prunes_only_with_flag() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open_in_memory().unwrap();
    let path = dir.path().join("a.flac");
    common::write_flac(&path, &["TITLE=A"], &[0xAA; 30]);
    scan_directory(&db, dir.path()).unwrap();
    std::fs::remove_file(&path).unwrap();

    let stats = musefs_core::revalidate(&db, dir.path()).unwrap();
    assert_eq!(stats.pruned, 0);
    assert_eq!(db.list_tracks().unwrap().len(), 1);

    let mut opts = musefs_core::ScanOptions::default();
    opts.prune = true;
    let stats = musefs_core::revalidate_with(&db, dir.path(), &opts).unwrap();
    assert_eq!(stats.pruned, 1);
    assert_eq!(db.list_tracks().unwrap().len(), 0);
}

#[test]
fn revalidate_backfill_does_not_clobber_tags() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open_in_memory().unwrap();
    let path = dir.path().join("a.flac");
    common::write_flac(&path, &["TITLE=OnDisk"], &[0xAA; 30]);
    scan_directory(&db, dir.path()).unwrap();
    let id = db.list_tracks().unwrap()[0].id;
    db.replace_tags(id, &[Tag::new("title", "Curated", 0)])
        .unwrap();
    db.set_structural_blocks(id, &[]).unwrap();

    let stats = musefs_core::revalidate(&db, dir.path()).unwrap();
    assert_eq!(stats.updated, 1);
    let tags = db.get_tags(id).unwrap();
    assert_eq!(tags[0].value, "Curated");
    assert!(!db.get_structural_blocks(id).unwrap().is_empty());
}
