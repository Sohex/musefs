mod common;
use std::collections::HashMap;
use std::io::Cursor;

use common::{flac_block, make_flac, resolve_layout, streaminfo_body, vorbis_comment_body};
use musefs_format::flac::{locate_audio, synthesize_layout};
use musefs_format::{ArtInput, TagInput};

#[test]
fn full_roundtrip_preserved_blocks_multivalue_tags_and_two_pictures() {
    let si = streaminfo_body();
    let seektable = vec![0xEEu8; 36];
    let old_vc = vorbis_comment_body("oldvendor", &["TITLE=Old", "ARTIST=Old"]);
    let audio: Vec<u8> = (0..200u32).map(|i| (i % 251) as u8).collect();
    let file = make_flac(
        &[(0, si.clone()), (3, seektable.clone()), (4, old_vc)],
        &audio,
    );

    let scan = locate_audio(&file).unwrap();
    assert_eq!(scan.preserved.len(), 2); // STREAMINFO + SEEKTABLE

    let tags = vec![
        TagInput::new("title", "Real Title"),
        TagInput::new("album", "Real Album"),
        TagInput::new("artist", "Alice"),
        TagInput::new("artist", "Bob"),
    ];
    let front = vec![0x01u8; 900];
    let back = vec![0x02u8; 700];
    let arts = vec![
        ArtInput {
            art_id: 1,
            mime: "image/png".into(),
            description: "front".into(),
            picture_type: 3,
            width: 600,
            height: 600,
            data_len: front.len() as u64,
        },
        ArtInput {
            art_id: 2,
            mime: "image/png".into(),
            description: "back".into(),
            picture_type: 4,
            width: 600,
            height: 600,
            data_len: back.len() as u64,
        },
    ];

    let layout = synthesize_layout(&scan, &tags, &arts).unwrap();

    let mut art_map = HashMap::new();
    art_map.insert(1i64, front.clone());
    art_map.insert(2i64, back.clone());
    let assembled = resolve_layout(&layout, &file, &art_map, &HashMap::new());

    assert_eq!(assembled.len() as u64, layout.total_len());
    assert_eq!(&assembled[layout.header_len() as usize..], &audio[..]);

    let tag = metaflac::Tag::read_from(&mut Cursor::new(&assembled)).expect("valid FLAC");

    let vc = tag.vorbis_comments().expect("vorbis comments");
    assert_eq!(
        vc.get("TITLE").map(std::vec::Vec::as_slice),
        Some(["Real Title".to_string()].as_slice())
    );
    assert_eq!(
        vc.get("ALBUM").map(std::vec::Vec::as_slice),
        Some(["Real Album".to_string()].as_slice())
    );
    assert_eq!(
        vc.get("ARTIST").map(std::vec::Vec::as_slice),
        Some(["Alice".to_string(), "Bob".to_string()].as_slice())
    );

    let pics: Vec<_> = tag.pictures().collect();
    assert_eq!(pics.len(), 2);
    assert_eq!(pics[0].description, "front");
    assert_eq!(pics[0].data, front);
    assert_eq!(pics[1].description, "back");
    assert_eq!(pics[1].data, back);

    let si_read = tag.get_streaminfo().expect("streaminfo");
    assert_eq!(si_read.sample_rate, 44100);

    let _ = flac_block(3, &seektable, false); // documents intent; body equality checked via scan below
    assert_eq!(scan.preserved[1].block_type, 3);
    assert_eq!(scan.preserved[1].body, seektable);
}
