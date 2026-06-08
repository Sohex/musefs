mod common;
use common::{make_flac, streaminfo_body, vorbis_comment_body};
use musefs_core::{scan_directory, MountConfig, Musefs, VirtualTree};
use std::collections::BTreeMap;

fn config() -> MountConfig {
    MountConfig {
        template: "$artist/$title".to_string(),
        fallbacks: BTreeMap::new(),
        default_fallback: "Unknown".to_string(),
        mode: musefs_core::Mode::Synthesis,
        poll_interval: std::time::Duration::ZERO,
        case_insensitive: false,
    }
}

/// A FLAC with the given vorbis comments and an APPLICATION block (→ a binary row).
/// Block types: STREAMINFO=0, APPLICATION=2, VORBIS_COMMENT=4.
fn flac_with_binary(comments: &[&str]) -> Vec<u8> {
    make_flac(
        &[
            (0u8, streaminfo_body()),
            (2u8, b"testPRIVATE-ANALYSIS".to_vec()),
            (4u8, vorbis_comment_body("v", comments)),
        ],
        &vec![0xCD; 4096],
    )
}

/// Same comments, no binary block.
fn flac_without_binary(comments: &[&str]) -> Vec<u8> {
    make_flac(
        &[
            (0u8, streaminfo_body()),
            (4u8, vorbis_comment_body("v", comments)),
        ],
        &vec![0xCD; 4096],
    )
}

/// Collect every rendered file path under ROOT as "dir/file".
fn rendered_paths(fs: &Musefs) -> Vec<String> {
    let mut out = Vec::new();
    for (dirname, dinode, _) in fs.readdir(VirtualTree::ROOT).unwrap() {
        if dirname == "." || dirname == ".." {
            continue;
        }
        for (fname, _finode, _) in fs.readdir(dinode).unwrap() {
            if fname == "." || fname == ".." {
                continue;
            }
            out.push(format!("{dirname}/{fname}"));
        }
    }
    out.sort();
    out
}

#[test]
fn binary_row_does_not_alter_rendered_tree_path() {
    let comments = ["ARTIST=Alice", "TITLE=Song"];

    // Track WITH a binary (APPLICATION) row.
    let dir_a = tempfile::tempdir().unwrap();
    std::fs::write(dir_a.path().join("a.flac"), flac_with_binary(&comments)).unwrap();
    let db_a = musefs_db::Db::open_in_memory().unwrap();
    scan_directory(&db_a, dir_a.path()).unwrap();
    let fs_a = Musefs::open(db_a, config()).unwrap();

    // Track WITHOUT a binary row.
    let dir_b = tempfile::tempdir().unwrap();
    std::fs::write(dir_b.path().join("b.flac"), flac_without_binary(&comments)).unwrap();
    let db_b = musefs_db::Db::open_in_memory().unwrap();
    scan_directory(&db_b, dir_b.path()).unwrap();
    let fs_b = Musefs::open(db_b, config()).unwrap();

    // Identical rendered path — the binary row never leaked into tags_to_fields.
    assert_eq!(rendered_paths(&fs_a), vec!["Alice/Song.flac".to_string()]);
    assert_eq!(rendered_paths(&fs_a), rendered_paths(&fs_b));
}
