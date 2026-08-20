use musefs_core::{NodeKind, VirtualTree};

#[test]
fn builds_directories_and_files_with_lookup() {
    let tree = VirtualTree::build(&[
        (10, "Pink Floyd/Animals/Pigs.flac".into()),
        (11, "Pink Floyd/Animals/Dogs.flac".into()),
        (12, "Pink Floyd/Meddle/Echoes.flac".into()),
    ]);

    let artist = tree
        .lookup(VirtualTree::ROOT, "Pink Floyd")
        .expect("artist dir");
    let animals = tree.lookup(artist, "Animals").expect("album dir");
    assert!(tree.is_dir(animals));

    let pigs = tree.lookup(animals, "Pigs.flac").expect("file");
    assert_eq!(tree.track_id(pigs), Some(10));
    assert!(!tree.is_dir(pigs));

    assert_eq!(tree.children(animals).expect("children").len(), 2);
    assert!(tree.lookup(animals, "Pigs.flac").is_some());
    assert!(tree.lookup(animals, "Dogs.flac").is_some());
}

#[test]
fn disambiguates_colliding_file_names() {
    let tree = VirtualTree::build(&[
        (1, "A/song.flac".into()),
        (2, "A/song.flac".into()),
        (3, "A/song.flac".into()),
    ]);
    let a = tree.lookup(VirtualTree::ROOT, "A").unwrap();
    assert_eq!(tree.children(a).unwrap().len(), 3);
    assert!(tree.lookup(a, "song.flac").is_some());
    assert!(tree.lookup(a, "song (2).flac").is_some());
    assert!(tree.lookup(a, "song (3).flac").is_some());
}

#[test]
fn root_node_is_a_directory() {
    let tree = VirtualTree::build(&[]);
    assert!(tree.is_dir(VirtualTree::ROOT));
    assert_eq!(
        tree.node(VirtualTree::ROOT)
            .map(|n| matches!(n.kind, NodeKind::Dir)),
        Some(true)
    );
}

#[test]
fn parent_of_root_is_root_and_children_point_back() {
    let tree = VirtualTree::build(&[(1, "Alice/Song.flac".into())]);
    assert_eq!(tree.parent(VirtualTree::ROOT), Some(VirtualTree::ROOT));

    let alice = tree.lookup(VirtualTree::ROOT, "Alice").unwrap();
    assert_eq!(tree.parent(alice), Some(VirtualTree::ROOT));

    let song = tree.lookup(alice, "Song.flac").unwrap();
    assert_eq!(tree.parent(song), Some(alice));

    assert_eq!(tree.parent(99999), None);
}
