mod common;

use std::collections::BTreeMap;
use std::time::Duration;

use musefs_core::{Mode, MountConfig, Musefs, scan_directory};
use musefs_db::{Db, Tag};

use common::corpus::{CorpusParams, Format, Target, prepare};
use proptest::prelude::*;

/// A small single-album FLAC corpus with `n` tracks. The returned `Target` owns
/// the tempdir — keep it alive for the whole test.
fn small_corpus(n: usize) -> Target {
    prepare(&CorpusParams::single(Format::Flac, 1, n))
}

fn config() -> MountConfig {
    MountConfig {
        template: "$artist/$album/$title".into(),
        fallbacks: BTreeMap::new(),
        default_fallback: "Unknown".into(),
        mode: Mode::Synthesis,
        poll_interval: Duration::ZERO,
        case_insensitive: false,
    }
}

fn config_ci() -> MountConfig {
    MountConfig {
        case_insensitive: true,
        ..config()
    }
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
    // Re-running scan_directory over an unchanged corpus bumps data_version but
    // changes no rendered path, so the tree must be identical before and after.
    let target = small_corpus(4);
    let db_path = target.db_path.clone();
    let corpus = target.corpus_dir.clone();
    let db = Db::open(&db_path).unwrap();
    scan_directory(&db, &corpus).unwrap();
    let fs = Musefs::open(Db::open(&db_path).unwrap(), config()).unwrap();

    // Re-scan: touches updated_at (data_version bump) but no rendered path changes.
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
        .upsert_art(&musefs_db::NewArt {
            mime: "image/png".into(),
            width: None,
            height: None,
            data: vec![0u8; 8],
        })
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
                        backing_path: format!("/virt/added-{add_seq}.flac"),
                        format: musefs_db::Format::Flac,
                        audio_offset: 0, audio_length: 1, backing_size: 1, backing_mtime_ns: 0, backing_ctime_ns: 0,
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
