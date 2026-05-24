use musefs_core::{NodeKind, VirtualTree};

#[test]
fn builds_directories_and_files_with_lookup() {
    let tree = VirtualTree::build(&[
        (10, "Pink Floyd/Animals/Pigs.flac".to_string()),
        (11, "Pink Floyd/Animals/Dogs.flac".to_string()),
        (12, "Pink Floyd/Meddle/Echoes.flac".to_string()),
    ]);

    let artist = tree.lookup(VirtualTree::ROOT, "Pink Floyd").expect("artist dir");
    let animals = tree.lookup(artist, "Animals").expect("album dir");
    assert!(tree.is_dir(animals));

    let pigs = tree.lookup(animals, "Pigs.flac").expect("file");
    assert_eq!(tree.track_id(pigs), Some(10));
    assert!(!tree.is_dir(pigs));

    let kids = tree.children(animals).expect("children");
    assert_eq!(kids.len(), 2);
    assert!(kids.contains_key("Pigs.flac"));
    assert!(kids.contains_key("Dogs.flac"));
}

#[test]
fn disambiguates_colliding_file_names() {
    let tree = VirtualTree::build(&[
        (1, "A/song.flac".to_string()),
        (2, "A/song.flac".to_string()),
        (3, "A/song.flac".to_string()),
    ]);
    let a = tree.lookup(VirtualTree::ROOT, "A").unwrap();
    let kids = tree.children(a).unwrap();
    assert_eq!(kids.len(), 3);
    assert!(kids.contains_key("song.flac"));
    assert!(kids.contains_key("song (2).flac"));
    assert!(kids.contains_key("song (3).flac"));
}

#[test]
fn root_node_is_a_directory() {
    let tree = VirtualTree::build(&[]);
    assert!(tree.is_dir(VirtualTree::ROOT));
    assert_eq!(tree.node(VirtualTree::ROOT).map(|n| matches!(n.kind, NodeKind::Dir)), Some(true));
}
