use super::synth::{boxed, build_udta, freeform_binary_prefix, patch_chunk_offsets};
use super::*;
use crate::input::{BlobLen, PictureType};

/// Build a 32-bit-size box: [size][type][payload].
fn bx(kind: &[u8; 4], payload: &[u8]) -> Vec<u8> {
    let mut v = u32::try_from(8 + payload.len())
        .unwrap()
        .to_be_bytes()
        .to_vec();
    v.extend_from_slice(kind);
    v.extend_from_slice(payload);
    v
}

#[test]
fn walks_top_level_boxes() {
    let mut buf = bx(b"ftyp", b"M4A ");
    buf.extend(bx(b"free", b"\x00\x00"));
    let boxes = child_boxes(&buf).unwrap();
    assert_eq!(boxes.len(), 2);
    assert_eq!(&boxes[0].kind, b"ftyp");
    assert_eq!(boxes[0].payload(&buf), b"M4A ");
    assert_eq!(&boxes[1].kind, b"free");
}

#[test]
fn find_box_and_nested_path() {
    let mut hdlr_payload = vec![0u8; 8];
    hdlr_payload.extend_from_slice(b"soun");
    hdlr_payload.extend_from_slice(&[0u8; 12]);
    let moov = bx(
        b"moov",
        &bx(b"trak", &bx(b"mdia", &bx(b"hdlr", &hdlr_payload))),
    );

    let m = find_box(&moov, b"moov").unwrap().unwrap();
    let (start, len) = find_path(m.payload(&moov), &[b"trak", b"mdia", b"hdlr"])
        .unwrap()
        .unwrap();
    assert_eq!(&m.payload(&moov)[start..start + len][8..12], b"soun");
}

#[test]
fn rejects_truncated_box() {
    let buf = [0u8, 0, 0, 99, b'm', b'o', b'o', b'v']; // claims 99, only 8 present
    assert!(child_boxes(&buf).is_err());
}

/// Minimal accepted MP4: ftyp, then (per `moov_first`) moov(one soun trak with
/// an stco) and mdat. `mdat_payload` is the verbatim audio.
fn mk_mp4(moov_first: bool, mdat_payload: &[u8], stco_entries: &[u32]) -> Vec<u8> {
    let mut stco = vec![0u8; 4];
    stco.extend_from_slice(&u32::try_from(stco_entries.len()).unwrap().to_be_bytes());
    for e in stco_entries {
        stco.extend_from_slice(&e.to_be_bytes());
    }
    let mut hdlr_p = vec![0u8; 8];
    hdlr_p.extend_from_slice(b"soun");
    hdlr_p.extend_from_slice(&[0u8; 12]);
    let minf = bx(b"minf", &bx(b"stbl", &bx(b"stco", &stco)));
    let mdia = bx(b"mdia", &[bx(b"hdlr", &hdlr_p), minf].concat());
    let trak = bx(b"trak", &mdia);
    let moov = bx(b"moov", &[bx(b"mvhd", &[0u8; 8]), trak].concat());
    let mdat = bx(b"mdat", mdat_payload);
    let ftyp = bx(b"ftyp", b"M4A isom");
    if moov_first {
        [ftyp, moov, mdat].concat()
    } else {
        [ftyp, mdat, moov].concat()
    }
}

#[test]
fn locates_audio_moov_first_and_last() {
    for moov_first in [true, false] {
        let buf = mk_mp4(moov_first, b"AUDIODATA", &[0]);
        let b = locate_audio(&buf).unwrap();
        assert_eq!(b.audio_length, 9);
        assert_eq!(&buf[usize_from(b.audio_offset)..][..9], b"AUDIODATA");
    }
}

#[test]
fn rejects_fragmented_video_and_multi_mdat() {
    let base = mk_mp4(true, b"X", &[0]);
    let mut frag = base.clone();
    frag.extend(bx(b"moof", b"\x00"));
    assert!(locate_audio(&frag).is_err());

    let mut two = base.clone();
    two.extend(bx(b"mdat", b"Y"));
    assert!(locate_audio(&two).is_err());

    let mut hdlr_p = vec![0u8; 8];
    hdlr_p.extend_from_slice(b"vide");
    hdlr_p.extend_from_slice(&[0u8; 12]);
    let video_moov = bx(b"moov", &bx(b"trak", &bx(b"mdia", &bx(b"hdlr", &hdlr_p))));
    let vbuf = [bx(b"ftyp", b"M4A "), video_moov, bx(b"mdat", b"Z")].concat();
    assert!(locate_audio(&vbuf).is_err());
}

/// A `soun` trak built the way `mk_mp4` does (hdlr + minf/stbl/stco), for
/// reuse when hand-assembling a moov to exercise a specific reject branch.
fn soun_trak() -> Vec<u8> {
    let mut stco = vec![0u8; 4];
    stco.extend_from_slice(&1u32.to_be_bytes());
    stco.extend_from_slice(&0u32.to_be_bytes());
    let mut hdlr_p = vec![0u8; 8];
    hdlr_p.extend_from_slice(b"soun");
    hdlr_p.extend_from_slice(&[0u8; 12]);
    let minf = bx(b"minf", &bx(b"stbl", &bx(b"stco", &stco)));
    let mdia = bx(b"mdia", &[bx(b"hdlr", &hdlr_p), minf].concat());
    bx(b"trak", &mdia)
}

#[test]
fn rejects_mvex_in_moov() {
    // A moov carrying an mvex box (movie-extends header => fragmented) is
    // rejected even though it otherwise holds a single valid soun trak.
    let moov = bx(
        b"moov",
        &[bx(b"mvhd", &[0u8; 8]), bx(b"mvex", b"\x00"), soun_trak()].concat(),
    );
    let buf = [bx(b"ftyp", b"M4A isom"), moov, bx(b"mdat", b"X")].concat();
    assert!(locate_audio(&buf).is_err());
}

#[test]
fn rejects_multi_trak() {
    // Two trak children in moov is rejected (musefs serves single-track audio).
    let moov = bx(
        b"moov",
        &[bx(b"mvhd", &[0u8; 8]), soun_trak(), soun_trak()].concat(),
    );
    let buf = [bx(b"ftyp", b"M4A isom"), moov, bx(b"mdat", b"X")].concat();
    assert!(locate_audio(&buf).is_err());
}

#[test]
fn reads_structure_parts() {
    let buf = mk_mp4(false, b"AUDIODATA", &[0]); // moov last
    let s = read_structure(&buf).unwrap();
    assert_eq!(&s.ftyp[4..8], b"ftyp");
    assert_eq!(&s.moov[4..8], b"moov");
    assert_eq!(&s.mdat_header[4..8], b"mdat");
    assert_eq!(s.mdat_payload_len, 9);
    assert_eq!(&buf[usize_from(s.mdat_payload_offset)..][..9], b"AUDIODATA");
}

fn data_atom(type_code: u32, value: &[u8]) -> Vec<u8> {
    let mut p = type_code.to_be_bytes().to_vec();
    p.extend_from_slice(&0u32.to_be_bytes()); // locale
    p.extend_from_slice(value);
    bx(b"data", &p)
}

/// Accepted file with udta/meta/ilst injected (meta is a FullBox).
fn mp4_with_ilst(ilst_atoms: &[u8], moov_first: bool) -> Vec<u8> {
    let ilst = bx(b"ilst", ilst_atoms);
    let mut hdlr = vec![0u8; 8];
    hdlr.extend_from_slice(b"mdir");
    hdlr.extend_from_slice(b"appl");
    hdlr.extend_from_slice(&[0u8; 9]);
    let mut meta = vec![0u8; 4]; // FullBox version/flags
    meta.extend(bx(b"hdlr", &hdlr));
    meta.extend(ilst);
    let udta = bx(b"udta", &bx(b"meta", &meta));

    let mut hdlr_p = vec![0u8; 8];
    hdlr_p.extend_from_slice(b"soun");
    hdlr_p.extend_from_slice(&[0u8; 12]);
    let mut stco = vec![0u8; 4];
    stco.extend_from_slice(&1u32.to_be_bytes());
    stco.extend_from_slice(&0u32.to_be_bytes());
    let minf = bx(b"minf", &bx(b"stbl", &bx(b"stco", &stco)));
    let trak = bx(
        b"trak",
        &bx(b"mdia", &[bx(b"hdlr", &hdlr_p), minf].concat()),
    );
    let moov = bx(b"moov", &[bx(b"mvhd", &[0u8; 8]), trak, udta].concat());
    let ftyp = bx(b"ftyp", b"M4A ");
    let mdat = bx(b"mdat", b"AUDIO");
    if moov_first {
        [ftyp, moov, mdat].concat()
    } else {
        [ftyp, mdat, moov].concat()
    }
}

#[test]
fn reads_text_and_track_tags() {
    let atoms = [
        bx(b"\xa9nam", &data_atom(1, b"Song")),
        bx(b"aART", &data_atom(1, b"Band")),
        bx(b"trkn", &data_atom(0, &[0, 0, 0, 3, 0, 0, 0, 0])),
    ]
    .concat();
    let buf = mp4_with_ilst(&atoms, true);
    let tags = read_tags(&buf);
    assert!(tags.contains(&("title".into(), "Song".into())));
    assert!(tags.contains(&("albumartist".into(), "Band".into())));
    assert!(tags.contains(&("tracknumber".into(), "3".into())));
}

#[test]
fn read_tags_reads_all_values_of_multi_value_text_atom() {
    // A single text atom may carry several `data` sub-boxes (the iTunes
    // multiple-value convention); every value must round-trip, not just the first.
    let atoms = bx(
        b"\xa9gen",
        &[data_atom(1, b"Rock"), data_atom(1, b"Pop")].concat(),
    );
    let buf = mp4_with_ilst(&atoms, true);
    let tags = read_tags(&buf);
    assert!(tags.contains(&("genre".into(), "Rock".into())));
    assert!(tags.contains(&("genre".into(), "Pop".into())));
}

#[test]
fn read_tags_trkn_includes_total_as_n_of_m() {
    // trkn body: [reserved 2][number 2][total 2][reserved 2]; "3 of 12" -> "3/12".
    let atoms = bx(b"trkn", &data_atom(0, &[0, 0, 0, 3, 0, 12, 0, 0]));
    let buf = mp4_with_ilst(&atoms, true);
    let tags = read_tags(&buf);
    assert!(tags.contains(&("tracknumber".into(), "3/12".into())));
}

#[test]
fn read_tags_trkn_zero_total_is_bare_number() {
    let atoms = bx(b"trkn", &data_atom(0, &[0, 0, 0, 3, 0, 0, 0, 0]));
    let buf = mp4_with_ilst(&atoms, true);
    let tags = read_tags(&buf);
    assert!(tags.contains(&("tracknumber".into(), "3".into())));
}

#[test]
fn read_tags_disk_includes_total_as_n_of_m() {
    let atoms = bx(b"disk", &data_atom(0, &[0, 0, 0, 1, 0, 2]));
    let buf = mp4_with_ilst(&atoms, true);
    let tags = read_tags(&buf);
    assert!(tags.contains(&("discnumber".into(), "1/2".into())));
}

#[test]
fn read_tags_decodes_integer_atoms() {
    // iTunes binary integer atoms (type code 21): tmpo (BPM, u16), cpil & pgap
    // (boolean flags). These were previously dropped at scan time.
    let atoms = [
        bx(b"tmpo", &data_atom(21, &300u16.to_be_bytes())),
        bx(b"cpil", &data_atom(21, &[1])),
        bx(b"pgap", &data_atom(21, &[1])),
    ]
    .concat();
    let buf = mp4_with_ilst(&atoms, true);
    let tags = read_tags(&buf);
    assert!(tags.contains(&("bpm".into(), "300".into())), "got {tags:?}");
    assert!(
        tags.contains(&("compilation".into(), "1".into())),
        "got {tags:?}"
    );
    assert!(
        tags.contains(&("gapless".into(), "1".into())),
        "got {tags:?}"
    );
}

#[test]
fn reads_cover_art() {
    let jpeg = [0xff, 0xd8, 0xff, 0xe0, 1, 2, 3];
    let buf = mp4_with_ilst(&bx(b"covr", &data_atom(13, &jpeg)), false);
    let pics = read_pictures(&buf, usize::MAX);
    assert_eq!(pics.len(), 1);
    assert_eq!(pics[0].mime, "image/jpeg");
    assert_eq!(pics[0].data, jpeg);
}

#[test]
fn read_side_never_panics_on_garbage() {
    // Empty buffer.
    assert!(read_tags(&[]).is_empty());
    assert!(read_pictures(&[], usize::MAX).is_empty());

    // Random non-MP4 bytes.
    let garbage = b"not an mp4 file at all............";
    assert!(read_tags(garbage).is_empty());
    assert!(read_pictures(garbage, usize::MAX).is_empty());

    // Valid moov but no udta/meta/ilst.
    let no_ilst = mk_mp4(true, b"AUDIO", &[0]);
    assert!(read_tags(&no_ilst).is_empty());
    assert!(read_pictures(&no_ilst, usize::MAX).is_empty());

    // A meta FullBox whose payload is shorter than the 4 version/flags bytes it
    // needs: exercises the `udta.get(meta.payload_start()+4..meta.end())?` guard.
    let truncated_meta = bx(b"udta", &bx(b"meta", &[0u8, 0]));
    let moov = bx(b"moov", &[bx(b"mvhd", &[0u8; 8]), truncated_meta].concat());
    let ftyp = bx(b"ftyp", b"M4A ");
    let mdat = bx(b"mdat", b"AUDIO");
    let lying = [ftyp, moov, mdat].concat();
    assert!(read_tags(&lying).is_empty());
    assert!(read_pictures(&lying, usize::MAX).is_empty());
}

#[test]
fn read_tags_survives_malformed_box_after_ilst() {
    // A single malformed sibling box trailing `ilst` inside `meta` must not
    // suppress the well-formed `ilst`: the path-to-ilst walk is lenient, matching
    // the metadata extractors' "seed what you can" contract (#542).
    let ilst = bx(b"ilst", &bx(b"\xa9nam", &data_atom(1, b"Song")));
    let mut hdlr = vec![0u8; 8];
    hdlr.extend_from_slice(b"mdir");
    hdlr.extend_from_slice(b"appl");
    hdlr.extend_from_slice(&[0u8; 9]);
    let mut meta = vec![0u8; 4]; // FullBox version/flags
    meta.extend(bx(b"hdlr", &hdlr));
    meta.extend(ilst);
    meta.extend_from_slice(&100u32.to_be_bytes()); // box claims 100 bytes...
    meta.extend_from_slice(b"junk"); // ...but only 8 are present -> malformed
    let udta = bx(b"udta", &bx(b"meta", &meta));
    let moov = bx(b"moov", &[bx(b"mvhd", &[0u8; 8]), udta].concat());
    let buf = [bx(b"ftyp", b"M4A "), moov, bx(b"mdat", b"AUDIO")].concat();
    let tags = read_tags(&buf);
    assert!(
        tags.contains(&("title".into(), "Song".into())),
        "got {tags:?}"
    );
}

#[test]
fn read_tags_reads_quicktime_bare_meta() {
    // A QuickTime-style `meta` has no FullBox version/flags prefix; its children
    // begin immediately. The +4 FullBox skip would land mid-header and drop every
    // tag, so the version/flags prefix is only consumed when present (#543).
    let ilst = bx(b"ilst", &bx(b"\xa9nam", &data_atom(1, b"Song")));
    let mut hdlr = vec![0u8; 8];
    hdlr.extend_from_slice(b"mdir");
    hdlr.extend_from_slice(b"appl");
    hdlr.extend_from_slice(&[0u8; 9]);
    let meta = [bx(b"hdlr", &hdlr), ilst].concat(); // no version/flags prefix
    let udta = bx(b"udta", &bx(b"meta", &meta));
    let moov = bx(b"moov", &[bx(b"mvhd", &[0u8; 8]), udta].concat());
    let buf = [bx(b"ftyp", b"M4A "), moov, bx(b"mdat", b"AUDIO")].concat();
    let tags = read_tags(&buf);
    assert!(
        tags.contains(&("title".into(), "Song".into())),
        "got {tags:?}"
    );
}

#[test]
fn build_udta_no_art_round_trips() {
    let tags = vec![
        TagInput::new("title", "Song"),
        TagInput::new("tracknumber", "5"),
    ];
    let (segs, streamed) = build_udta(&tags, &[], &[], None).unwrap();
    assert_eq!(streamed, 0);
    let prefix = materialize_udta(&segs);
    let b = read_box(&prefix, 0).unwrap();
    assert_eq!(&b.kind, b"udta");
    assert_eq!(b.total_len, prefix.len());
    // Wrap in a moov and read back through our own reader.
    let buf = [
        bx(b"ftyp", b"M4A "),
        bx(b"moov", &prefix),
        bx(b"mdat", b"A"),
    ]
    .concat();
    let tags = read_tags(&buf);
    assert!(tags.contains(&("title".into(), "Song".into())));
    assert!(tags.contains(&("tracknumber".into(), "5".into())));
}

#[test]
fn build_udta_with_art_reserves_size_without_image() {
    let art = ArtInput {
        art_id: 1,
        mime: "image/png".into(),
        description: String::new(),
        picture_type: PictureType::new(3).unwrap(),
        width: 0,
        height: 0,
        depth: 0,
        colors: 0,
        data_len: BlobLen::new(100).unwrap(),
    };
    let (segs, streamed) = build_udta(&[TagInput::new("title", "T")], &[], &[art], None).unwrap();
    assert_eq!(streamed, 100);
    // The image streams as the final segment; the udta size field accounts for it.
    assert!(matches!(
        segs.last(),
        Some(Segment::ArtImage { len, .. }) if len.get() == 100
    ));
    let inline_total: usize = segs
        .iter()
        .filter_map(|s| match s {
            Segment::Inline(b) => Some(b.len()),
            _ => None,
        })
        .sum();
    let Segment::Inline(head) = &segs[0] else {
        panic!("first udta segment is inline framing");
    };
    let declared = u32::from_be_bytes(head[0..4].try_into().unwrap()) as usize;
    assert_eq!(declared, inline_total + 100);
    // The leading inline ends right after the covr/data header (image streams next).
    assert!(head.windows(4).any(|w| w == b"covr"));
}

#[test]
fn build_udta_rejects_oversize_art() {
    let art = ArtInput {
        art_id: 1,
        mime: "image/jpeg".into(),
        description: String::new(),
        picture_type: PictureType::new(3).unwrap(),
        width: 0,
        height: 0,
        depth: 0,
        colors: 0,
        data_len: BlobLen::new(u64::from(u32::MAX) + 1).unwrap(),
    };
    assert!(matches!(
        build_udta(&[TagInput::new("title", "T")], &[], &[art], None),
        Err(FormatError::TooLarge)
    ));
}

#[test]
fn build_udta_groups_multi_value_text() {
    // Two consecutive same-key text tags must collapse into ONE ilst atom
    // carrying REPEATED `data` sub-boxes (iTunes multi-value convention),
    // not two separate atoms and not a dropped value.
    let tags = vec![
        TagInput::new("genre", "Rock"),
        TagInput::new("genre", "Metal"),
    ];
    let (segs, streamed) = build_udta(&tags, &[], &[], None).unwrap();
    assert_eq!(streamed, 0);
    let prefix = materialize_udta(&segs);

    // Exactly one `©gen` atom.
    let gen_count = prefix.windows(4).filter(|w| *w == b"\xa9gen").count();
    assert_eq!(
        gen_count, 1,
        "expected exactly one genre atom, got {gen_count}"
    );

    // Locate the `©gen` atom header and parse its children: must be two `data`
    // sub-boxes. The 4-byte kind sits at offset +4 of the box, so back up 4.
    let kind_at = prefix
        .windows(4)
        .position(|w| w == b"\xa9gen")
        .expect("genre atom present");
    let atom = read_box(&prefix, kind_at - 4).unwrap();
    assert_eq!(&atom.kind, b"\xa9gen");
    let children = child_boxes(atom.payload(&prefix)).unwrap();
    let data_count = children.iter().filter(|c| &c.kind == b"data").count();
    assert_eq!(
        data_count, 2,
        "expected two data sub-boxes, got {data_count}"
    );

    // Both values survive into the bytes.
    assert!(prefix.windows(4).any(|w| w == b"Rock"));
    assert!(prefix.windows(5).any(|w| w == b"Metal"));
}

#[test]
fn build_udta_empty_tags_is_valid() {
    // A real file with no tags must still yield a structurally valid (empty)
    // udta, not a malformed box.
    let (segs, streamed) = build_udta(&[], &[], &[], None).unwrap();
    assert_eq!(streamed, 0);
    let prefix = materialize_udta(&segs);
    let b = read_box(&prefix, 0).unwrap();
    assert_eq!(&b.kind, b"udta");
    assert_eq!(b.total_len, prefix.len());
    // Round-trips as having no tags.
    let buf = [
        bx(b"ftyp", b"M4A "),
        bx(b"moov", &prefix),
        bx(b"mdat", b"A"),
    ]
    .concat();
    assert!(read_tags(&buf).is_empty());
}

fn inline_head(layout: &RegionLayout) -> Vec<u8> {
    match &layout.segments()[0] {
        Segment::Inline(b) => b.clone(),
        _ => panic!("expected Inline head"),
    }
}
/// Concatenate a udta segment list into a contiguous buffer, substituting `len`
/// zero bytes for each streamed (BinaryTag/ArtImage) segment. Box-size fields
/// already account for these, so the result parses as a complete udta box.
/// Do NOT use for huge reserved art lengths — read the size field off `segs[0]`.
fn materialize_udta(segments: &[Segment]) -> Vec<u8> {
    let mut out = Vec::new();
    for seg in segments {
        match seg {
            Segment::Inline(b) => out.extend_from_slice(b),
            Segment::BinaryTag { len, .. } | Segment::ArtImage { len, .. } => {
                out.resize(out.len() + usize_from(len.get()), 0);
            }
            other => panic!("unexpected segment in udta: {other:?}"),
        }
    }
    out
}
/// Locate `moov` by reading complete boxes from the front, stopping before
/// the trailing `mdat` header (whose declared size includes the payload that
/// is *not* present in the synthesized head — it streams as BackingAudio).
fn find_moov_in_head(head: &[u8]) -> BoxRef {
    let mut pos = 0;
    loop {
        let b = read_box(head, pos).unwrap();
        if &b.kind == b"moov" {
            return b;
        }
        pos = b.end();
    }
}
fn first_stco(head: &[u8]) -> Vec<u32> {
    let moov = find_moov_in_head(head);
    let mp = moov.payload(head);
    let (sp, sl) = find_path(mp, &[b"trak", b"mdia", b"minf", b"stbl", b"stco"])
        .unwrap()
        .unwrap();
    let stco = &mp[sp..sp + sl];
    let count = u32::from_be_bytes(stco[4..8].try_into().unwrap()) as usize;
    (0..count)
        .map(|i| u32::from_be_bytes(stco[8 + i * 4..12 + i * 4].try_into().unwrap()))
        .collect()
}

#[test]
fn synthesize_no_art_patches_stco() {
    let buf = mk_mp4(true, b"AUDIODATA", &[42, 100]);
    let scan = read_structure(&buf).unwrap();
    let layout = synthesize_layout(&scan, &[TagInput::new("title", "New")], &[], &[]).unwrap();

    match layout.segments().last().unwrap() {
        Segment::BackingAudio { offset, len } => {
            assert_eq!(*offset, scan.mdat_payload_offset);
            assert_eq!(*len, scan.mdat_payload_len);
        }
        _ => panic!("expected BackingAudio tail"),
    }
    let head = inline_head(&layout);
    // The synthesized head is [ftyp][moov][mdat header]; the mdat payload is
    // served verbatim as the BackingAudio tail, so its new position is exactly
    // where the head ends.
    let new_mdat = head.len() as u64;
    let delta = new_mdat - scan.mdat_payload_offset;
    assert_eq!(
        first_stco(&head),
        vec![
            42 + u32::try_from(delta).unwrap(),
            100 + u32::try_from(delta).unwrap()
        ]
    );
    // The new file head re-parses as a valid moov of the declared size.
    let moov = find_moov_in_head(&head);
    assert_eq!(moov.end(), head.len() - scan.mdat_header.len());
}

/// A non-audio trak with the given handler and a one-entry `stco` holding
/// `chunk_offset` — the shape a QuickTime chapter track has.
fn handler_trak(handler: &[u8; 4], chunk_offset: u32) -> Vec<u8> {
    let mut stco = vec![0u8; 4];
    stco.extend_from_slice(&1u32.to_be_bytes());
    stco.extend_from_slice(&chunk_offset.to_be_bytes());
    let mut hdlr_p = vec![0u8; 8];
    hdlr_p.extend_from_slice(handler);
    hdlr_p.extend_from_slice(&[0u8; 12]);
    let minf = bx(b"minf", &bx(b"stbl", &bx(b"stco", &stco)));
    let mdia = bx(b"mdia", &[bx(b"hdlr", &hdlr_p), minf].concat());
    bx(b"trak", &mdia)
}

/// A chaptered `.m4b`: one `soun` trak whose single chunk is at `audio_chunk`,
/// plus a chapter trak (handler `handler`) whose chunk is at `chapter_chunk`.
/// Both chunks live in the same `mdat`, as ffmpeg and mp4v2 emit them.
fn mk_mp4_chaptered(handler: &[u8; 4], audio_chunk: u32, chapter_chunk: u32) -> Vec<u8> {
    let mut stco = vec![0u8; 4];
    stco.extend_from_slice(&1u32.to_be_bytes());
    stco.extend_from_slice(&audio_chunk.to_be_bytes());
    let mut hdlr_p = vec![0u8; 8];
    hdlr_p.extend_from_slice(b"soun");
    hdlr_p.extend_from_slice(&[0u8; 12]);
    let minf = bx(b"minf", &bx(b"stbl", &bx(b"stco", &stco)));
    let soun = bx(
        b"trak",
        &bx(b"mdia", &[bx(b"hdlr", &hdlr_p), minf].concat()),
    );
    let moov = bx(
        b"moov",
        &[
            bx(b"mvhd", &[0u8; 8]),
            soun,
            handler_trak(handler, chapter_chunk),
        ]
        .concat(),
    );
    [
        bx(b"ftyp", b"M4A isom"),
        moov,
        bx(b"mdat", b"AUDIODATACHAPTERS"),
    ]
    .concat()
}

#[test]
fn accepts_one_audio_track_plus_a_chapter_track() {
    // Chapters ride as a second `text`/`sbtl` track, which is the whole reason
    // the .m4b extension exists — rejecting them rejected most audiobooks (#672).
    for handler in [b"text", b"sbtl"] {
        let buf = mk_mp4_chaptered(handler, 0, 9);
        let b = locate_audio(&buf).unwrap();
        assert_eq!(b.audio_length, 17);
        assert_eq!(&buf[usize_from(b.audio_offset)..][..9], b"AUDIODATA");
    }
}

#[test]
fn rejects_a_video_track_alongside_audio_and_names_the_handlers() {
    let buf = mk_mp4_chaptered(b"vide", 0, 9);
    let err = locate_audio(&buf).unwrap_err();
    // The message names what was found, so the skip explains itself instead of
    // landing in an opaque `unparseable` tally.
    let msg = err.to_string();
    assert!(msg.contains("soun"), "{msg}");
    assert!(msg.contains("vide"), "{msg}");
}

#[test]
fn rejects_a_moov_with_no_audio_track() {
    let moov = bx(
        b"moov",
        &[bx(b"mvhd", &[0u8; 8]), handler_trak(b"text", 0)].concat(),
    );
    let buf = [bx(b"ftyp", b"M4A isom"), moov, bx(b"mdat", b"X")].concat();
    let msg = locate_audio(&buf).unwrap_err().to_string();
    assert!(msg.contains("text"), "{msg}");
}

#[test]
fn rejects_a_trak_whose_handler_is_unreadable() {
    // A `trak` with no `mdia/hdlr` cannot be classified; it is described rather
    // than silently accepted.
    let headless = bx(b"trak", &bx(b"mdia", &bx(b"minf", b"")));
    let moov = bx(
        b"moov",
        &[bx(b"mvhd", &[0u8; 8]), soun_trak(), headless].concat(),
    );
    let buf = [bx(b"ftyp", b"M4A isom"), moov, bx(b"mdat", b"X")].concat();
    let msg = locate_audio(&buf).unwrap_err().to_string();
    assert!(msg.contains("<no hdlr>"), "{msg}");
}

/// Every trak's `stco` entries in a synthesized head, in track order.
fn all_stco(head: &[u8]) -> Vec<Vec<u32>> {
    let moov = find_moov_in_head(head);
    let mp = moov.payload(head);
    child_boxes(mp)
        .unwrap()
        .into_iter()
        .filter(|b| &b.kind == b"trak")
        .map(|t| {
            let trak = t.payload(mp);
            let (sp, sl) = find_path(trak, &[b"mdia", b"minf", b"stbl", b"stco"])
                .unwrap()
                .unwrap();
            let stco = &trak[sp..sp + sl];
            let count = u32::from_be_bytes(stco[4..8].try_into().unwrap()) as usize;
            (0..count)
                .map(|i| u32::from_be_bytes(stco[8 + i * 4..12 + i * 4].try_into().unwrap()))
                .collect()
        })
        .collect()
}

#[test]
fn synthesize_patches_the_chapter_tracks_offsets_too() {
    // Patching only the first trak would leave the chapter track pointing into
    // the old layout — worse than rejecting the file (#672).
    let buf = mk_mp4_chaptered(b"text", 0, 9);
    let scan = read_structure(&buf).unwrap();
    let layout = synthesize_layout(&scan, &[TagInput::new("title", "New")], &[], &[]).unwrap();

    let head = inline_head(&layout);
    // The mdat payload is the BackingAudio tail, so it now starts where the head ends.
    let delta = u32::try_from(head.len() as u64 - scan.mdat_payload_offset).unwrap();
    assert_eq!(all_stco(&head), vec![vec![delta], vec![9 + delta]]);
}

/// A Nero chapter-list box: version/flags, a count, then per chapter a 64-bit
/// timestamp and a length-prefixed title. No file offsets, which is why it can be
/// copied through verbatim.
fn chpl_box(titles: &[&str]) -> Vec<u8> {
    let mut p = vec![0u8; 5]; // version/flags + reserved
    p.push(u8::try_from(titles.len()).unwrap());
    for (i, t) in titles.iter().enumerate() {
        p.extend_from_slice(&(i as u64 * 10_000_000).to_be_bytes());
        p.push(u8::try_from(t.len()).unwrap());
        p.extend_from_slice(t.as_bytes());
    }
    bx(b"chpl", &p)
}

#[test]
fn synthesize_carries_the_nero_chapter_list_through() {
    // ffmpeg writes a `chpl` alongside the chapter track by default, so
    // rebuilding `udta` from the store alone would cost chapters on players
    // that read only `chpl` (#672).
    let chpl = chpl_box(&["Chapter One", "Chapter Two"]);
    let udta = bx(b"udta", &[bx(b"meta", &[0u8; 4]), chpl.clone()].concat());
    let moov = bx(
        b"moov",
        &[bx(b"mvhd", &[0u8; 8]), soun_trak(), udta].concat(),
    );
    let buf = [bx(b"ftyp", b"M4A isom"), moov, bx(b"mdat", b"AUDIO")].concat();

    let scan = read_structure(&buf).unwrap();
    let layout = synthesize_layout(&scan, &[TagInput::new("title", "New")], &[], &[]).unwrap();
    let head = inline_head(&layout);

    // The chpl survives byte-for-byte, inside the regenerated udta and after meta.
    let moov_box = find_moov_in_head(&head);
    let mp = moov_box.payload(&head);
    let new_udta = find_box(mp, b"udta").unwrap().unwrap();
    let inner = new_udta.payload(mp);
    let kinds: Vec<[u8; 4]> = child_boxes(inner).unwrap().iter().map(|b| b.kind).collect();
    assert_eq!(kinds, vec![*b"meta", *b"chpl"]);
    let c = find_box(inner, b"chpl").unwrap().unwrap();
    assert_eq!(&inner[c.start..c.end()], &chpl[..]);
    // The declared moov size still matches what was emitted.
    assert_eq!(moov_box.end(), head.len() - scan.mdat_header.len());
}

#[test]
fn synthesize_rejects_a_track_with_no_chunk_offset_box() {
    // Leaving a track unpatched is corruption, so a `stbl` with neither `stco`
    // nor `co64` fails synthesis rather than being skipped.
    let chapterless = bx(
        b"trak",
        &bx(
            b"mdia",
            &[
                bx(b"hdlr", &{
                    let mut p = vec![0u8; 8];
                    p.extend_from_slice(b"text");
                    p.extend_from_slice(&[0u8; 12]);
                    p
                }),
                bx(b"minf", &bx(b"stbl", b"")),
            ]
            .concat(),
        ),
    );
    let moov = bx(
        b"moov",
        &[bx(b"mvhd", &[0u8; 8]), soun_trak(), chapterless].concat(),
    );
    let buf = [bx(b"ftyp", b"M4A isom"), moov, bx(b"mdat", b"X")].concat();
    let scan = read_structure(&buf).unwrap();
    assert!(synthesize_layout(&scan, &[TagInput::new("title", "N")], &[], &[]).is_err());
}

/// Like `mk_mp4` but the soun trak's stbl carries a `co64` (8-byte offsets)
/// box instead of an `stco`. moov-first, since that's all this exercises.
fn mk_mp4_co64(mdat_payload: &[u8], co64_entries: &[u64]) -> Vec<u8> {
    let mut co64 = vec![0u8; 4];
    co64.extend_from_slice(&u32::try_from(co64_entries.len()).unwrap().to_be_bytes());
    for e in co64_entries {
        co64.extend_from_slice(&e.to_be_bytes());
    }
    let mut hdlr_p = vec![0u8; 8];
    hdlr_p.extend_from_slice(b"soun");
    hdlr_p.extend_from_slice(&[0u8; 12]);
    let minf = bx(b"minf", &bx(b"stbl", &bx(b"co64", &co64)));
    let mdia = bx(b"mdia", &[bx(b"hdlr", &hdlr_p), minf].concat());
    let trak = bx(b"trak", &mdia);
    let moov = bx(b"moov", &[bx(b"mvhd", &[0u8; 8]), trak].concat());
    let mdat = bx(b"mdat", mdat_payload);
    let ftyp = bx(b"ftyp", b"M4A isom");
    [ftyp, moov, mdat].concat()
}

fn first_co64(head: &[u8]) -> Vec<u64> {
    let moov = find_moov_in_head(head);
    let mp = moov.payload(head);
    let (sp, sl) = find_path(mp, &[b"trak", b"mdia", b"minf", b"stbl", b"co64"])
        .unwrap()
        .unwrap();
    let co64 = &mp[sp..sp + sl];
    let count = u32::from_be_bytes(co64[4..8].try_into().unwrap()) as usize;
    (0..count)
        .map(|i| u64::from_be_bytes(co64[8 + i * 8..16 + i * 8].try_into().unwrap()))
        .collect()
}

#[test]
fn synthesize_patches_co64() {
    let buf = mk_mp4_co64(b"AUDIODATA", &[42, 100]);
    let scan = read_structure(&buf).unwrap();
    let layout = synthesize_layout(&scan, &[TagInput::new("title", "New")], &[], &[]).unwrap();

    match layout.segments().last().unwrap() {
        Segment::BackingAudio { offset, len } => {
            assert_eq!(*offset, scan.mdat_payload_offset);
            assert_eq!(*len, scan.mdat_payload_len);
        }
        _ => panic!("expected BackingAudio tail"),
    }
    let head = inline_head(&layout);
    // mdat payload is served as the BackingAudio tail, so its new position is
    // exactly where the head ends; co64 offsets shift by the same delta.
    let new_mdat = head.len() as u64;
    let delta = new_mdat - scan.mdat_payload_offset;
    assert_eq!(first_co64(&head), vec![42 + delta, 100 + delta]);
    // The new file head re-parses as a valid moov of the declared size.
    let moov = find_moov_in_head(&head);
    assert_eq!(moov.end(), head.len() - scan.mdat_header.len());
}

#[test]
fn synthesize_co64_offset_near_i64_max_does_not_overflow() {
    // A `co64` chunk offset near i64::MAX must not panic when shifted by the
    // (positive) relocation delta. The patch cast the u64 offset to i64 and added
    // delta, overflowing i64 even though the true u64 result still fits — a fuzz
    // crash (attempt to add with overflow). The relocated offset is computed in
    // u64, so this synthesizes cleanly.
    let entry = i64::MAX as u64; // 0x7FFF_FFFF_FFFF_FFFF
    let buf = mk_mp4_co64(b"AUDIODATA", &[entry]);
    let scan = read_structure(&buf).unwrap();
    let layout = synthesize_layout(&scan, &[TagInput::new("title", "New")], &[], &[]).unwrap();
    let head = inline_head(&layout);
    let delta = head.len() as u64 - scan.mdat_payload_offset;
    assert_eq!(first_co64(&head), vec![entry + delta]);
}

#[test]
fn synthesize_with_art_splits_for_streaming() {
    let buf = mk_mp4(false, b"AUDIODATA", &[0]);
    let scan = read_structure(&buf).unwrap();
    let art = ArtInput {
        art_id: 7,
        mime: "image/jpeg".into(),
        description: String::new(),
        picture_type: PictureType::new(3).unwrap(),
        width: 0,
        height: 0,
        depth: 0,
        colors: 0,
        data_len: BlobLen::new(50).unwrap(),
    };
    let layout = synthesize_layout(&scan, &[TagInput::new("title", "T")], &[], &[art]).unwrap();
    let segs = layout.segments();
    assert!(matches!(segs[1], Segment::ArtImage { art_id: 7, len, .. } if len.get() == 50));
    assert!(matches!(segs[2], Segment::Inline(_))); // mdat header
    assert!(matches!(segs.last().unwrap(), Segment::BackingAudio { .. }));
}

#[test]
fn synthesize_picks_first_nonempty_art() {
    // With multiple non-empty arts, the real art must be served.
    let buf = mk_mp4(false, b"AUDIODATA", &[0]);
    let scan = read_structure(&buf).unwrap();
    let real = ArtInput {
        art_id: 9,
        mime: "image/png".into(),
        description: String::new(),
        picture_type: PictureType::new(3).unwrap(),
        width: 0,
        height: 0,
        depth: 0,
        colors: 0,
        data_len: BlobLen::new(40).unwrap(),
    };
    let layout = synthesize_layout(&scan, &[TagInput::new("title", "T")], &[], &[real]).unwrap();
    let segs = layout.segments();
    assert!(
        segs.iter()
            .any(|s| matches!(s, Segment::ArtImage { art_id: 9, len, .. } if len.get() == 40)),
        "the first nonempty art must be served"
    );
}

#[test]
fn synthesize_handles_zero_length_mdat() {
    let buf = mk_mp4(true, b"", &[0]); // empty mdat payload
    let scan = read_structure(&buf).unwrap();
    assert_eq!(scan.mdat_payload_len, 0);
    let layout = synthesize_layout(&scan, &[TagInput::new("title", "Z")], &[], &[]).unwrap();
    match layout.segments().last().unwrap() {
        Segment::BackingAudio { offset, len } => {
            assert_eq!(*offset, scan.mdat_payload_offset);
            assert_eq!(*len, 0);
        }
        _ => panic!("expected BackingAudio tail"),
    }
}

#[test]
fn box_header_parses_8_byte_16_byte_and_size0() {
    // 8-byte header: size 16, type "moov".
    let mut h = 16u32.to_be_bytes().to_vec();
    h.extend_from_slice(b"moov");
    let bh = box_header(&h, 1000).unwrap();
    assert_eq!(&bh.kind, b"moov");
    assert_eq!(bh.header_len, 8);
    assert_eq!(bh.total_len, 16);

    // 64-bit largesize: size32==1, then u64 size = 40.
    let mut h = 1u32.to_be_bytes().to_vec();
    h.extend_from_slice(b"mdat");
    h.extend_from_slice(&40u64.to_be_bytes());
    let bh = box_header(&h, 1000).unwrap();
    assert_eq!(bh.header_len, 16);
    assert_eq!(bh.total_len, 40);

    // size32==0 means "extends to EOF" -> total_len == remaining.
    let mut h = 0u32.to_be_bytes().to_vec();
    h.extend_from_slice(b"mdat");
    let bh = box_header(&h, 500).unwrap();
    assert_eq!(bh.header_len, 8);
    assert_eq!(bh.total_len, 500);
}

#[test]
fn box_header_rejects_impossible_sizes() {
    // total_len < header_len.
    let mut h = 4u32.to_be_bytes().to_vec();
    h.extend_from_slice(b"moov");
    assert_eq!(box_header(&h, 1000), Err(FormatError::Malformed));
    // total_len > remaining.
    let mut h = 2000u32.to_be_bytes().to_vec();
    h.extend_from_slice(b"moov");
    assert_eq!(box_header(&h, 100), Err(FormatError::Malformed));
    // header shorter than 8 bytes.
    assert_eq!(box_header(&[0u8; 4], 1000), Err(FormatError::Malformed));
}

#[test]
fn read_structure_from_matches_buffer_path() {
    // Both moov-first and moov-last (moov-last is the audiobook spike case).
    for moov_first in [true, false] {
        let buf = mk_mp4(moov_first, &vec![0xABu8; 4096], &[0]);
        let from_buf = read_structure(&buf).unwrap();
        let mut cur = std::io::Cursor::new(buf.clone());
        let from_stream = read_structure_from(&mut cur, buf.len() as u64).unwrap();
        assert_eq!(from_stream, from_buf);
    }
}

#[test]
fn read_structure_from_never_reads_mdat_payload() {
    // moov LAST: reaching it requires skipping the mdat payload.
    let buf = mk_mp4(false, &vec![0xCDu8; 100_000], &[0]);
    let scan = read_structure(&buf).unwrap();
    let pay_start = scan.mdat_payload_offset;
    let pay_end = pay_start + scan.mdat_payload_len;

    // A reader that records every byte range it is asked to read.
    struct Tracking {
        inner: std::io::Cursor<Vec<u8>>,
        touched: Vec<(u64, u64)>,
    }
    impl std::io::Read for Tracking {
        fn read(&mut self, b: &mut [u8]) -> std::io::Result<usize> {
            let off = self.inner.position();
            let n = std::io::Read::read(&mut self.inner, b)?;
            self.touched.push((off, off + n as u64));
            Ok(n)
        }
    }
    impl std::io::Seek for Tracking {
        fn seek(&mut self, p: std::io::SeekFrom) -> std::io::Result<u64> {
            self.inner.seek(p)
        }
    }

    let mut tr = Tracking {
        inner: std::io::Cursor::new(buf.clone()),
        touched: Vec::new(),
    };
    let from_stream = read_structure_from(&mut tr, buf.len() as u64).unwrap();
    assert_eq!(from_stream, scan);
    for (s, e) in &tr.touched {
        assert!(
            *e <= pay_start || *s >= pay_end,
            "read [{s},{e}) overlaps mdat payload [{pay_start},{pay_end})"
        );
    }
}

#[test]
fn read_freeform_extracts_name_and_value() {
    // Build a minimal `----` atom: mean + name + data(UTF-8).
    let mut mean_body = 0u32.to_be_bytes().to_vec();
    mean_body.extend_from_slice(b"com.apple.iTunes");
    let mut name_body = 0u32.to_be_bytes().to_vec();
    name_body.extend_from_slice(b"MusicBrainz Album Id");
    let mut data = 1u32.to_be_bytes().to_vec(); // type 1 = UTF-8
    data.extend_from_slice(&0u32.to_be_bytes()); // locale
    data.extend_from_slice(b"abc-123");
    let mut inner = boxed(b"mean", &mean_body).unwrap();
    inner.extend(boxed(b"name", &name_body).unwrap());
    inner.extend(boxed(b"data", &data).unwrap());

    assert_eq!(
        read_freeform(&inner),
        vec![("musicbrainz_albumid".to_string(), "abc-123".to_string())]
    );
}

#[test]
fn read_freeform_unknown_name_passes_through_verbatim() {
    let mut mean_body = 0u32.to_be_bytes().to_vec();
    mean_body.extend_from_slice(b"com.apple.iTunes");
    let mut name_body = 0u32.to_be_bytes().to_vec();
    name_body.extend_from_slice(b"My Custom Field");
    let mut data = 1u32.to_be_bytes().to_vec(); // type 1 = UTF-8
    data.extend_from_slice(&0u32.to_be_bytes()); // locale
    data.extend_from_slice(b"hello");
    let mut inner = boxed(b"mean", &mean_body).unwrap();
    inner.extend(boxed(b"name", &name_body).unwrap());
    inner.extend(boxed(b"data", &data).unwrap());

    assert_eq!(
        read_freeform(&inner),
        vec![("My Custom Field".to_string(), "hello".to_string())]
    );
}

#[test]
fn read_freeform_reads_all_data_boxes() {
    // A `----` atom with two UTF-8 `data` sub-boxes must yield both values.
    let mut name_body = 0u32.to_be_bytes().to_vec();
    name_body.extend_from_slice(b"My Custom Field");
    let mut inner = boxed(b"name", &name_body).unwrap();
    inner.extend(data_atom(1, b"one"));
    inner.extend(data_atom(1, b"two"));
    assert_eq!(
        read_freeform(&inner),
        vec![
            ("My Custom Field".to_string(), "one".to_string()),
            ("My Custom Field".to_string(), "two".to_string()),
        ]
    );
}

#[test]
fn read_freeform_skips_binary_typed_data() {
    let mut name_body = 0u32.to_be_bytes().to_vec();
    name_body.extend_from_slice(b"My Custom Field");
    let mut data = 0u32.to_be_bytes().to_vec(); // type 0 = binary, not text
    data.extend_from_slice(&0u32.to_be_bytes()); // locale
    data.extend_from_slice(&[0xff, 0x00, 0x01]);
    let mut inner = boxed(b"name", &name_body).unwrap();
    inner.extend(boxed(b"data", &data).unwrap());

    assert!(read_freeform(&inner).is_empty()); // binary-typed data is skipped
}

#[test]
fn build_udta_round_trips_freeform_and_vocabulary() {
    let tags = vec![
        TagInput::new("title", "Song"),
        TagInput::new("tracknumber", "3"),
        TagInput::new("MyRating", "5"), // user-defined -> ----
        TagInput::new("musicbrainz_albumid", "abc-123"), // vocabulary -> ----
    ];
    let (segs, _streamed) = build_udta(&tags, &[], &[], None).unwrap();
    let udta = materialize_udta(&segs);
    // build_udta returns a full `udta` box; read_tags expects a buffer containing
    // moov/udta/meta/ilst, so wrap udta in a minimal moov for the round trip.
    let moov = boxed(b"moov", &udta).unwrap();

    let tags = read_tags(&moov);
    for expected in [
        ("title", "Song"),
        ("tracknumber", "3"),
        ("MyRating", "5"),
        ("musicbrainz_albumid", "abc-123"),
    ] {
        assert!(
            tags.contains(&(expected.0.to_string(), expected.1.to_string())),
            "missing {expected:?} in {tags:?}"
        );
    }
}

#[test]
fn build_udta_round_trips_track_and_disc_totals() {
    // A "N/M" tracknumber/discnumber must emit the total into the trkn/disk atom
    // and read back unchanged (the prior code parsed only "N" and wrote total 0,
    // and dropped a "N/M" value entirely on parse failure).
    let tags = vec![
        TagInput::new("tracknumber", "3/12"),
        TagInput::new("discnumber", "1/2"),
    ];
    let (segs, _streamed) = build_udta(&tags, &[], &[], None).unwrap();
    let udta = materialize_udta(&segs);
    let moov = boxed(b"moov", &udta).unwrap();
    let tags = read_tags(&moov);
    assert!(
        tags.contains(&("tracknumber".to_string(), "3/12".to_string())),
        "got {tags:?}"
    );
    assert!(
        tags.contains(&("discnumber".to_string(), "1/2".to_string())),
        "got {tags:?}"
    );
}

#[test]
fn build_udta_round_trips_integer_atoms() {
    // bpm 300 has a non-zero high byte, so a big-endian decode is required (a
    // byte-swapped read would yield 44, not 300).
    let tags = vec![
        TagInput::new("bpm", "300"),
        TagInput::new("compilation", "1"),
        TagInput::new("gapless", "1"),
    ];
    let (segs, _s) = build_udta(&tags, &[], &[], None).unwrap();
    let udta = materialize_udta(&segs);
    // Emitted as the real iTunes atoms, not generic `----` freeform atoms, with
    // exact widths: 8 (atom hdr) + 8 (data hdr) + 8 (type+locale) + value bytes.
    let atom_size = |kind: &[u8; 4]| -> u32 {
        let pos = udta.windows(4).position(|w| w == &kind[..]).unwrap();
        u32::from_be_bytes(udta[pos - 4..pos].try_into().unwrap())
    };
    assert_eq!(atom_size(b"tmpo"), 26, "tmpo value must be 2 bytes");
    assert_eq!(atom_size(b"cpil"), 25, "cpil value must be 1 byte");
    assert_eq!(atom_size(b"pgap"), 25, "pgap value must be 1 byte");

    let moov = boxed(b"moov", &udta).unwrap();
    let read = read_tags(&moov);
    assert!(read.contains(&("bpm".into(), "300".into())), "got {read:?}");
    assert!(
        read.contains(&("compilation".into(), "1".into())),
        "got {read:?}"
    );
    assert!(
        read.contains(&("gapless".into(), "1".into())),
        "got {read:?}"
    );
}

#[test]
fn read_box_rejects_overflowing_extended_size() {
    // The extended-size path (size32 == 1) reads a 64-bit box length from
    // untrusted input. Before the checked_add fix, `pos + total` overflowed
    // usize in debug (panic) or wrapped silently in release (accepting a
    // bogus length). This test feeds size32=1 with a u64::MAX extended size
    // and asserts the parser returns an error rather than panicking.
    // Bytes: [00 00 00 01] (size32=1) + b"moov" + [FF FF FF FF FF FF FF FF] (u64::MAX)
    let mut bytes = 1u32.to_be_bytes().to_vec(); // size32 = 1 → extended-size
    bytes.extend_from_slice(b"moov");
    bytes.extend_from_slice(&u64::MAX.to_be_bytes()); // huge 64-bit size
    assert!(
        read_structure(&bytes).is_err(),
        "must return an error, not panic"
    );
}

#[test]
fn read_structure_from_handles_largesize_mdat() {
    // Re-encode a normal fixture's mdat with a 64-bit largesize header (the
    // real >4GB audiobook shape) and confirm both readers agree.
    fn largesize_mdat(payload: &[u8]) -> Vec<u8> {
        let total = 16 + payload.len() as u64;
        let mut v = 1u32.to_be_bytes().to_vec(); // size32 == 1
        v.extend_from_slice(b"mdat");
        v.extend_from_slice(&total.to_be_bytes()); // 64-bit largesize
        v.extend_from_slice(payload);
        v
    }
    let normal = mk_mp4(true, &[0xABu8; 64], &[0]); // [ftyp][moov][mdat]
    let scan = read_structure(&normal).unwrap();
    let payload_start = usize_from(scan.mdat_payload_offset);
    let mdat_box_start = payload_start - scan.mdat_header.len(); // normal 8-byte header
    let payload = normal[payload_start..].to_vec();
    let mut buf = normal[..mdat_box_start].to_vec(); // ftyp + moov
    buf.extend(largesize_mdat(&payload));

    let from_buf = read_structure(&buf).unwrap();
    let mut cur = std::io::Cursor::new(buf.clone());
    let from_stream = read_structure_from(&mut cur, buf.len() as u64).unwrap();
    assert_eq!(from_stream, from_buf);
    assert_eq!(from_stream.mdat_header.len(), 16); // largesize header
    assert_eq!(from_stream.mdat_payload_len, payload.len() as u64);
}

#[test]
fn box_header_accepts_empty_payload_box() {
    // total_len == header_len (an 8-byte box, no payload) must be accepted.
    // `< -> <=` would make the equal case reject.
    let mut h = 8u32.to_be_bytes().to_vec();
    h.extend_from_slice(b"free");
    let bh = box_header(&h, 1000).unwrap();
    assert_eq!(bh.header_len, 8);
    assert_eq!(bh.total_len, 8);
}

#[test]
fn read_box_size0_extends_to_end_from_offset() {
    // A size-0 box ("extends to EOF") at pos > 0: total_len must be
    // buf.len() - pos. `- -> +` (buf.len() + pos) and `- -> /` (buf.len() / pos)
    // both diverge. The box is placed at pos = 8 with pos + 8 <= buf.len() so the
    // be_u32 size read and the kind slice both succeed BEFORE the size-0 branch.
    let mut buf = bx(b"free", b""); // 8-byte box at pos 0
    buf.extend_from_slice(&0u32.to_be_bytes()); // size32 = 0 at pos 8
    buf.extend_from_slice(b"mdat"); // kind at pos 12..16
    buf.extend_from_slice(b"AUDIOPAYLOAD"); // 12 payload bytes
    assert_eq!(buf.len(), 28);
    let b = read_box(&buf, 8).unwrap();
    assert_eq!(&b.kind, b"mdat");
    assert_eq!(b.total_len, buf.len() - 8); // 20
}

#[test]
fn read_structure_from_rejects_box_overrunning_eof() {
    // box_header's `remaining` arg is `file_len - pos`. Inflating the mdat box's
    // declared size past the bytes remaining must be rejected. `- -> +` inflates
    // `remaining` to `file_len + pos`, wrongly accepting the overrun (returns Ok).
    let mut buf = mk_mp4(true, b"AUDIO", &[0]); // [ftyp][moov][mdat], mdat last
    let scan = read_structure(&buf).unwrap();
    let mdat_start = usize_from(scan.mdat_payload_offset - scan.mdat_header.len() as u64);
    let real = u32::from_be_bytes(buf[mdat_start..mdat_start + 4].try_into().unwrap());
    buf[mdat_start..mdat_start + 4].copy_from_slice(&(real + 100).to_be_bytes());
    let mut cur = std::io::Cursor::new(buf.clone());
    assert!(read_structure_from(&mut cur, buf.len() as u64).is_err());
}

#[test]
fn read_structure_from_rejects_moof() {
    // A `moof` (fragmented MP4) top-level box must be rejected via the seeking
    // path. Deleting the `b"moof"` match arm drops it to `_ => {}` and accepts.
    let mut buf = mk_mp4(true, b"AUDIO", &[0]);
    buf.extend(bx(b"moof", b"\x00\x00\x00\x00"));
    let mut cur = std::io::Cursor::new(buf.clone());
    assert!(read_structure_from(&mut cur, buf.len() as u64).is_err());
}

#[test]
fn read_structure_from_rejects_duplicate_top_level_boxes() {
    // Each `dup |= X.replace(..).is_some()` accumulates a duplicate. `|= -> &=`
    // can never set `dup` (it starts false), so a duplicate box is wrongly
    // accepted. One duplicated box per kind isolates each of the three `|=` lines.
    let dup = |extra: Vec<u8>| {
        let mut buf = mk_mp4(true, b"AUDIO", &[0]);
        buf.extend(extra);
        let mut cur = std::io::Cursor::new(buf.clone());
        read_structure_from(&mut cur, buf.len() as u64).is_err()
    };
    assert!(dup(bx(b"ftyp", b"M4A isom")), "duplicate ftyp must reject"); // ftyp |= line
    // duplicate moov: reuse the moov from a fresh fixture so it is structurally valid.
    let extra_moov = {
        let other = mk_mp4(true, b"AUDIO", &[0]);
        let s = read_structure(&other).unwrap();
        s.moov
    };
    assert!(dup(extra_moov), "duplicate moov must reject"); // moov |= line
    assert!(dup(bx(b"mdat", b"Y")), "duplicate mdat must reject"); // mdat |= line
}

#[test]
fn read_freeform_accepts_minimal_name_and_data() {
    // name payload == 4 (empty name) and data payload == 8 (empty value) is the
    // boundary of `np.len() < 4 || dp.len() < 8`. Both operands at the boundary,
    // so flipping EITHER `<` to `==`/`<=` makes that side true -> None.
    let name_body = 0u32.to_be_bytes().to_vec(); // exactly 4 bytes
    let mut data = 1u32.to_be_bytes().to_vec(); // type 1 = UTF-8
    data.extend_from_slice(&0u32.to_be_bytes()); // locale -> dp.len() == 8
    let mut inner = boxed(b"name", &name_body).unwrap();
    inner.extend(boxed(b"data", &data).unwrap());
    assert_eq!(read_freeform(&inner), vec![(String::new(), String::new())]);
}

#[test]
fn read_freeform_short_name_returns_none() {
    // name payload 3 bytes (< 4) with a valid 8-byte data payload. `|| -> &&`
    // makes `true && false == false`, falling through to `&np[4..]` (out of bounds
    // -> panic).
    let name_body = vec![0u8, 0, 0]; // 3 bytes
    let mut data = 1u32.to_be_bytes().to_vec();
    data.extend_from_slice(&0u32.to_be_bytes());
    let mut inner = boxed(b"name", &name_body).unwrap();
    inner.extend(boxed(b"data", &data).unwrap());
    assert!(read_freeform(&inner).is_empty());
}

#[test]
fn read_freeform_mean_payload_exactly_4_uses_empty_mean() {
    // mean payload == 4 (FullBox prefix, empty mean). `p.len() >= 4` must take the
    // utf8 branch (mean ""), so the vocabulary does NOT fold the iTunes name.
    // `>= -> <` falls to the default "com.apple.iTunes" mean and wrongly folds.
    let mean_body = vec![0u8, 0, 0, 0]; // exactly 4 bytes
    let mut name_body = 0u32.to_be_bytes().to_vec();
    name_body.extend_from_slice(b"MusicBrainz Album Id");
    let mut data = 1u32.to_be_bytes().to_vec();
    data.extend_from_slice(&0u32.to_be_bytes());
    data.extend_from_slice(b"abc-123");
    let mut inner = boxed(b"mean", &mean_body).unwrap();
    inner.extend(boxed(b"name", &name_body).unwrap());
    inner.extend(boxed(b"data", &data).unwrap());
    assert_eq!(
        read_freeform(&inner),
        vec![("MusicBrainz Album Id".to_string(), "abc-123".to_string())]
    );
}

#[test]
fn read_tags_data_payload_exactly_8_is_read() {
    // A `data` payload of exactly 8 bytes (type+locale, empty value) is the
    // boundary of `dp.len() < 8`. The (empty) value must be read; `< -> ==`/`<= `
    // would skip it.
    let atoms = bx(b"\xa9nam", &data_atom(1, b"")); // dp.len() == 8
    let buf = mp4_with_ilst(&atoms, true);
    assert!(read_tags(&buf).contains(&("title".into(), String::new())));
}

#[test]
fn read_tags_disk_exact_4_byte_value_yields_discnumber() {
    // disk atom, value exactly 4 bytes: `kind == disk` (== branch) `&&`
    // `value.len() >= 4` (>= branch). Kills `== -> !=` (mutant skips a real disk)
    // and `>= -> <` (mutant skips the boundary length).
    let atoms = bx(b"disk", &data_atom(0, &[0, 0, 0, 2])); // disc 2, value len 4
    let buf = mp4_with_ilst(&atoms, true);
    assert!(read_tags(&buf).contains(&("discnumber".into(), "2".into())));
}

#[test]
fn read_tags_disk_short_value_is_skipped() {
    // disk with a value shorter than 4 bytes: the guard is false. `&& -> ||`
    // makes it true and indexes value[2]/value[3] out of bounds (panic).
    let atoms = bx(b"disk", &data_atom(0, &[0, 0])); // value len 2
    let buf = mp4_with_ilst(&atoms, true);
    assert!(!read_tags(&buf).iter().any(|(k, _)| k == "discnumber"));
}

#[test]
fn read_tags_trkn_short_value_is_skipped() {
    // trkn with a value shorter than 4 bytes: `kind == trkn && value.len() >= 4`
    // is false. `&& -> ||` makes it true and indexes value[2]/value[3] (panic).
    let atoms = bx(b"trkn", &data_atom(0, &[0, 0])); // value len 2
    let buf = mp4_with_ilst(&atoms, true);
    assert!(!read_tags(&buf).iter().any(|(k, _)| k == "tracknumber"));
}

#[test]
fn read_pictures_data_payload_exactly_8_is_read() {
    // covr/data payload of exactly 8 bytes (type+locale, empty image) is the
    // boundary of `dp.len() < 8`; the (empty) picture must be read.
    let buf = mp4_with_ilst(&bx(b"covr", &data_atom(13, b"")), true);
    let pics = read_pictures(&buf, usize::MAX);
    assert_eq!(pics.len(), 1);
    assert_eq!(pics[0].mime, "image/jpeg");
    assert!(pics[0].data.is_empty());
}

#[test]
fn read_pictures_recognizes_png() {
    // A covr `data` atom with type code 14 is PNG. Deleting the `14 =>` match arm
    // drops it to `_ => continue` and yields no picture.
    let png = [0x89, b'P', b'N', b'G', 1, 2, 3];
    let buf = mp4_with_ilst(&bx(b"covr", &data_atom(14, &png)), false);
    let pics = read_pictures(&buf, usize::MAX);
    assert_eq!(pics.len(), 1);
    assert_eq!(pics[0].mime, "image/png");
    assert_eq!(pics[0].data, png);
}

#[test]
fn read_pictures_reads_all_data_atoms_in_one_covr() {
    // iTunes convention: multiple artworks are multiple `data` children of
    // one `covr`. An unknown type code skips that child only, not its
    // siblings.
    let jpeg = [0xFF, 0xD8, 0xFF, 1];
    let png = [0x89, b'P', b'N', b'G', 2];
    let covr = bx(
        b"covr",
        &[
            data_atom(13, &jpeg),
            data_atom(99, b"skipped"), // unknown type code: this child only
            data_atom(14, &png),
        ]
        .concat(),
    );
    let buf = mp4_with_ilst(&covr, true);
    let pics = read_pictures(&buf, usize::MAX);
    assert_eq!(pics.len(), 2);
    assert_eq!(pics[0].mime, "image/jpeg");
    assert_eq!(pics[0].data, jpeg);
    assert_eq!(pics[1].mime, "image/png");
    assert_eq!(pics[1].data, png);
}

#[test]
fn read_pictures_skips_art_over_budget() {
    let over = vec![0xFFu8; 5];
    let buf = mp4_with_ilst(&bx(b"covr", &data_atom(13, &over)), true);
    assert!(read_pictures(&buf, 4).is_empty());
}

#[test]
fn read_pictures_accepts_art_exactly_at_budget() {
    let exact = vec![0xFFu8; 4];
    let buf = mp4_with_ilst(&bx(b"covr", &data_atom(13, &exact)), true);
    let pics = read_pictures(&buf, 4);
    assert_eq!(pics.len(), 1);
    assert_eq!(pics[0].data, exact);
}

#[test]
fn read_pictures_reporting_reports_oversize_drop() {
    // The image body (5 bytes) exceeds the 4-byte cap: skipped from the
    // pictures, but reported as a drop with its MIME and exact byte size.
    let over = vec![0xFFu8; 5];
    let buf = mp4_with_ilst(&bx(b"covr", &data_atom(13, &over)), true);
    let (pics, dropped) = read_pictures_reporting(&buf, 4);
    assert!(pics.is_empty());
    assert_eq!(dropped.len(), 1);
    assert_eq!(dropped[0].descriptor, "image/jpeg");
    assert_eq!(dropped[0].bytes, over.len());
}

#[test]
fn read_pictures_reporting_no_drops_when_within_budget() {
    let exact = vec![0xFFu8; 4];
    let buf = mp4_with_ilst(&bx(b"covr", &data_atom(13, &exact)), true);
    let (pics, dropped) = read_pictures_reporting(&buf, 4);
    assert_eq!(pics.len(), 1);
    assert!(dropped.is_empty());
}

#[test]
fn read_pictures_skips_non_data_children_of_covr() {
    // A non-`data` child inside covr (rare but legal) is silently skipped.
    let png = [0x89, b'P', b'N', b'G'];
    let covr = bx(
        b"covr",
        &[bx(b"free", b"pad"), data_atom(14, &png)].concat(),
    );
    let buf = mp4_with_ilst(&covr, false);
    let pics = read_pictures(&buf, usize::MAX);
    assert_eq!(pics.len(), 1);
    assert_eq!(pics[0].mime, "image/png");
    assert_eq!(pics[0].data, png);
}

#[test]
fn build_udta_png_art_uses_type_code_14() {
    // PNG art => covr/data type code 14; JPEG => 13. `== -> !=` flips them.
    for (mime, expected) in [("image/png", 14u32), ("image/jpeg", 13u32)] {
        let art = ArtInput {
            art_id: 1,
            mime: mime.into(),
            description: String::new(),
            picture_type: PictureType::new(3).unwrap(),
            width: 0,
            height: 0,
            depth: 0,
            colors: 0,
            data_len: BlobLen::new(10).unwrap(),
        };
        let (segs, _) = build_udta(&[TagInput::new("title", "T")], &[], &[art], None).unwrap();
        let prefix = materialize_udta(&segs);
        // covr layout: [covr_size u32]["covr"][data_size u32]["data"][type u32][locale u32]
        let cpos = prefix.windows(4).position(|w| w == b"covr").expect("covr");
        assert_eq!(&prefix[cpos + 8..cpos + 12], b"data");
        let type_code = u32::from_be_bytes(prefix[cpos + 12..cpos + 16].try_into().unwrap());
        assert_eq!(type_code, expected, "mime {mime}");
    }
}

#[test]
fn build_udta_art_box_sizes_are_exact() {
    // data_size = 8 + 8 + data_len; covr_size = 8 + data_size. The `+ -> -`/`+ -> *`
    // mutations change the emitted box sizes.
    let art = ArtInput {
        art_id: 1,
        mime: "image/jpeg".into(),
        description: String::new(),
        picture_type: PictureType::new(3).unwrap(),
        width: 0,
        height: 0,
        depth: 0,
        colors: 0,
        data_len: BlobLen::new(10).unwrap(),
    };
    let (segs, _) = build_udta(&[TagInput::new("title", "T")], &[], &[art], None).unwrap();
    let prefix = materialize_udta(&segs);
    let cpos = prefix.windows(4).position(|w| w == b"covr").expect("covr");
    let covr_size = u32::from_be_bytes(prefix[cpos - 4..cpos].try_into().unwrap());
    let data_size = u32::from_be_bytes(prefix[cpos + 4..cpos + 8].try_into().unwrap());
    assert_eq!(data_size, 8 + 8 + 10); // 26
    assert_eq!(covr_size, 8 + data_size); // 34
}

#[test]
fn build_udta_multiple_arts_one_covr_n_data_atoms() {
    let art = |id: i64, mime: &str, len: u64| ArtInput {
        art_id: id,
        mime: mime.into(),
        description: String::new(),
        picture_type: PictureType::new(3).unwrap(),
        width: 0,
        height: 0,
        depth: 0,
        colors: 0,
        data_len: BlobLen::new(len).unwrap(),
    };
    let arts = [art(1, "image/jpeg", 10), art(2, "image/png", 20)];
    let (segs, streamed) = build_udta(&[TagInput::new("title", "T")], &[], &arts, None).unwrap();
    assert_eq!(streamed, 30);

    // Exactly one covr atom, sized for both data atoms: 8 + Σ(16 + len).
    let prefix = materialize_udta(&segs);
    let covr_positions: Vec<usize> = prefix
        .windows(4)
        .enumerate()
        .filter_map(|(i, w)| (w == b"covr").then_some(i))
        .collect();
    assert_eq!(covr_positions.len(), 1);
    let cpos = covr_positions[0];
    let covr_size = u32::from_be_bytes(prefix[cpos - 4..cpos].try_into().unwrap());
    assert_eq!(covr_size, 8 + (16 + 10) + (16 + 20));

    // First data atom: jpeg (type 13), size 16+10; second: png (14), 16+20.
    let d1 = cpos + 4;
    assert_eq!(&prefix[d1 + 4..d1 + 8], b"data");
    assert_eq!(
        u32::from_be_bytes(prefix[d1..d1 + 4].try_into().unwrap()),
        26
    );
    assert_eq!(
        u32::from_be_bytes(prefix[d1 + 8..d1 + 12].try_into().unwrap()),
        13
    );
    let d2 = d1 + 26;
    assert_eq!(&prefix[d2 + 4..d2 + 8], b"data");
    assert_eq!(
        u32::from_be_bytes(prefix[d2..d2 + 4].try_into().unwrap()),
        36
    );
    assert_eq!(
        u32::from_be_bytes(prefix[d2 + 8..d2 + 12].try_into().unwrap()),
        14
    );

    // Streamed segments: one ArtImage per art, in input order.
    let art_segs: Vec<(i64, u64)> = segs
        .iter()
        .filter_map(|s| match s {
            Segment::ArtImage { art_id, len } => Some((*art_id, len.get())),
            _ => None,
        })
        .collect();
    assert_eq!(art_segs, vec![(1, 10), (2, 20)]);
}

#[test]
fn build_udta_two_arts_round_trips_through_read_pictures() {
    // materialize_udta zero-fills streamed payloads, so assert order +
    // mime only (mime derives from the inline type code, which survives).
    let art = |id: i64, mime: &str, len: u64| ArtInput {
        art_id: id,
        mime: mime.into(),
        description: String::new(),
        picture_type: PictureType::new(3).unwrap(),
        width: 0,
        height: 0,
        depth: 0,
        colors: 0,
        data_len: BlobLen::new(len).unwrap(),
    };
    let arts = [art(1, "image/jpeg", 5), art(2, "image/png", 9)];
    let (segs, _) = build_udta(&[TagInput::new("title", "Song")], &[], &arts, None).unwrap();
    let prefix = materialize_udta(&segs);
    let buf = [
        bx(b"ftyp", b"M4A "),
        bx(b"moov", &prefix),
        bx(b"mdat", b"A"),
    ]
    .concat();
    let pics = read_pictures(&buf, usize::MAX);
    assert_eq!(pics.len(), 2);
    assert_eq!(pics[0].mime, "image/jpeg");
    assert_eq!(pics[0].data.len(), 5);
    assert_eq!(pics[1].mime, "image/png");
    assert_eq!(pics[1].data.len(), 9);
}

#[test]
fn build_udta_udta_size_exactly_u32_max_is_ok() {
    // The guard is `udta_size > u32::MAX` (strict). udta_size == u32::MAX must be
    // accepted; `> -> >=` rejects the exact boundary. data_len is reserved as a
    // number (no image bytes), so the boundary is cheap to hit.
    fn art(data_len: u64) -> ArtInput {
        ArtInput {
            art_id: 1,
            mime: "image/jpeg".into(),
            description: String::new(),
            picture_type: PictureType::new(3).unwrap(),
            width: 0,
            height: 0,
            depth: 0,
            colors: 0,
            data_len: BlobLen::new(data_len).unwrap(),
        }
    }
    // Derive the fixed overhead from the udta size field (segs[0] inline), with
    // data_len 1 (BlobLen is non-zero), without materializing any image bytes.
    let (segs0, _) = build_udta(&[TagInput::new("title", "T")], &[], &[art(1)], None).unwrap();
    let Segment::Inline(h0) = &segs0[0] else {
        panic!("inline head")
    };
    let overhead = u64::from(u32::from_be_bytes(h0[0..4].try_into().unwrap())) - 1;
    let max_len = u64::from(u32::MAX) - overhead;

    let (segs_max, streamed) =
        build_udta(&[TagInput::new("title", "T")], &[], &[art(max_len)], None).unwrap();
    assert_eq!(streamed, max_len);
    let Segment::Inline(h_max) = &segs_max[0] else {
        panic!("inline head")
    };
    assert_eq!(
        u32::from_be_bytes(h_max[0..4].try_into().unwrap()),
        u32::MAX
    );

    assert!(matches!(
        build_udta(
            &[TagInput::new("title", "T")],
            &[],
            &[art(max_len + 1)],
            None
        ),
        Err(FormatError::TooLarge)
    ));
}

#[test]
fn patch_chunk_offsets_stco_overflow_and_underflow_boundaries() {
    // kept = a single soun trak with one stco entry (offset 0). v = 0 + delta is
    // guarded by `v < 0 || v > u32::MAX`. Boundary deltas pin every guard mutant;
    // delta 0 (accepted) also pins the `:590` `+ -> *` bound at i = 0.
    let mut k = soun_trak();
    assert!(patch_chunk_offsets(&mut k, 0).is_ok()); // v == 0

    let mut k = soun_trak();
    assert!(patch_chunk_offsets(&mut k, i64::from(u32::MAX)).is_ok()); // v == u32::MAX

    let mut k = soun_trak();
    assert!(matches!(
        patch_chunk_offsets(&mut k, i64::from(u32::MAX) + 1), // v == u32::MAX + 1
        Err(FormatError::TooLarge)
    ));

    let mut k = soun_trak();
    assert!(matches!(
        patch_chunk_offsets(&mut k, -1), // v == -1
        Err(FormatError::TooLarge)
    ));
}

#[test]
fn patch_chunk_offsets_rejects_count_past_table() {
    // stco declares 2 entries but only 1 entry's bytes are present (followed by an
    // unrelated `free` box for padding). `pos + entry > start + len` must reject
    // the 2nd entry. `+ -> -` shrinks the bound and reads into the `free` box
    // instead of erroring (returns Ok).
    let mut stco = vec![0u8; 4]; // version/flags
    stco.extend_from_slice(&2u32.to_be_bytes()); // count = 2 (a lie)
    stco.extend_from_slice(&0u32.to_be_bytes()); // only 1 entry present
    let stbl = bx(
        b"stbl",
        &[bx(b"stco", &stco), bx(b"free", &[0u8; 8])].concat(),
    );
    let mut kept = bx(b"trak", &bx(b"mdia", &bx(b"minf", &stbl)));
    assert!(matches!(
        patch_chunk_offsets(&mut kept, 0),
        Err(FormatError::Malformed)
    ));
}

#[test]
fn patch_chunk_offsets_co64_zero_offset_is_ok() {
    // co64 path guard is `v < 0`. offset 0 + delta 0 => v == 0 must be accepted;
    // `< -> ==`/`<= ` reject the boundary.
    let mut co64 = vec![0u8; 4]; // version/flags
    co64.extend_from_slice(&1u32.to_be_bytes()); // count 1
    co64.extend_from_slice(&0u64.to_be_bytes()); // offset 0
    let stbl = bx(b"stbl", &bx(b"co64", &co64));
    let mut kept = bx(b"trak", &bx(b"mdia", &bx(b"minf", &stbl)));
    assert!(patch_chunk_offsets(&mut kept, 0).is_ok());
}

/// Build a `----` freeform atom with an explicit data `type_code` and raw value.
fn freeform_atom_typed(mean: &str, name: &str, type_code: u32, value: &[u8]) -> Vec<u8> {
    let mut mean_body = 0u32.to_be_bytes().to_vec();
    mean_body.extend_from_slice(mean.as_bytes());
    let mut name_body = 0u32.to_be_bytes().to_vec();
    name_body.extend_from_slice(name.as_bytes());
    let mut data_body = type_code.to_be_bytes().to_vec();
    data_body.extend_from_slice(&0u32.to_be_bytes()); // locale
    data_body.extend_from_slice(value);
    let mut inner = boxed(b"mean", &mean_body).unwrap();
    inner.extend(boxed(b"name", &name_body).unwrap());
    inner.extend(boxed(b"data", &data_body).unwrap());
    boxed(b"----", &inner).unwrap()
}

/// Wrap an `ilst` body in the moov/udta/meta/ilst boxes `ilst_region` expects.
fn moov_with_ilst(ilst_body: &[u8]) -> Vec<u8> {
    let ilst = boxed(b"ilst", ilst_body).unwrap();
    let mut meta = 0u32.to_be_bytes().to_vec(); // FullBox version/flags
    meta.extend(boxed(b"hdlr", &[0u8; 25]).unwrap());
    meta.extend_from_slice(&ilst);
    let udta = boxed(b"udta", &boxed(b"meta", &meta).unwrap()).unwrap();
    boxed(b"moov", &udta).unwrap()
}

#[test]
fn read_binary_tags_extracts_opaque_freeform_skips_text() {
    let serato = vec![0x00, 0xff, 0x10, 0x42, 0x99];
    let binary = freeform_atom_typed("com.serato.dj", "analysis", 0, &serato);
    let text = freeform_atom_typed("com.apple.iTunes", "MOOD", 1, b"calm");
    let moov = moov_with_ilst(&[binary, text].concat());

    let tags = read_binary_tags(&moov, usize::MAX);
    assert_eq!(tags.len(), 1, "only the binary `----` is opaque");
    assert_eq!(tags[0].key, "----:com.serato.dj:analysis");
    assert_eq!(tags[0].payload, serato);

    // The text `----` is the text path's job, never opaque.
    assert!(
        read_binary_tags(&moov, usize::MAX)
            .iter()
            .all(|t| t.key != "----:com.apple.iTunes:MOOD")
    );
}

#[test]
fn read_binary_tags_handles_data_box_length_boundary() {
    // A `data` box shorter than the 8-byte `[type][locale]` header is malformed:
    // it must be skipped, never indexed into (no panic).
    let mut short_inner = boxed(b"mean", &{
        let mut b = 0u32.to_be_bytes().to_vec();
        b.extend_from_slice(b"com.serato.dj");
        b
    })
    .unwrap();
    short_inner.extend(
        boxed(b"name", &{
            let mut b = 0u32.to_be_bytes().to_vec();
            b.extend_from_slice(b"short");
            b
        })
        .unwrap(),
    );
    short_inner.extend(boxed(b"data", &[0u8; 5]).unwrap()); // 5 < 8: truncated header
    let short = boxed(b"----", &short_inner).unwrap();

    // A `data` box of exactly 8 bytes (binary type 0, no value) is well-formed
    // with an empty payload — it must be emitted, not skipped.
    let empty = freeform_atom_typed("com.serato.dj", "empty", 0, b"");
    let moov = moov_with_ilst(&[short, empty].concat());

    let tags = read_binary_tags(&moov, usize::MAX);
    assert_eq!(tags.len(), 1, "short data skipped, 8-byte data emitted");
    assert_eq!(tags[0].key, "----:com.serato.dj:empty");
    assert!(tags[0].payload.is_empty());
}

#[test]
fn read_binary_tags_skips_payload_over_budget() {
    // A `----` value of 5 bytes with a budget of 4: skipped before any copy.
    let over = vec![0xABu8; 5];
    let atom = freeform_atom_typed("com.serato.dj", "analysis", 0, &over);
    let moov = moov_with_ilst(&atom);
    assert!(read_binary_tags(&moov, 4).is_empty());
}

#[test]
fn read_binary_tags_accepts_payload_exactly_at_budget() {
    // Boundary: value length == budget is still extracted.
    let exact = vec![0xABu8; 4];
    let atom = freeform_atom_typed("com.serato.dj", "analysis", 0, &exact);
    let moov = moov_with_ilst(&atom);
    let tags = read_binary_tags(&moov, 4);
    assert_eq!(tags.len(), 1);
    assert_eq!(tags[0].payload, exact);
}

#[test]
fn child_boxes_lenient_recovers_prefix_before_malformed() {
    // A well-formed box followed by one claiming a size that overruns the buffer:
    // the strict `child_boxes` errors, but the lenient walk returns the prefix.
    let mut buf = bx(b"free", b"ok");
    buf.extend_from_slice(&[0, 0, 0, 99, b'b', b'a', b'd', b'!']); // claims 99, 8 present
    assert!(child_boxes(&buf).is_err());
    let boxes = child_boxes_lenient(&buf);
    assert_eq!(
        boxes.len(),
        1,
        "the good box before the malformed one survives"
    );
    assert_eq!(&boxes[0].kind, b"free");
}

#[test]
fn read_tags_recovers_atoms_before_a_malformed_sibling() {
    // ilst = [good ©nam title][malformed atom]. One garbled atom must not discard
    // the well-formed tags that precede it (#524).
    let mut ilst = bx(b"\xa9nam", &data_atom(1, b"Title One"));
    ilst.extend_from_slice(&[0, 0, 0, 99, b'b', b'a', b'd', b'!']); // overruns buffer
    let moov = moov_with_ilst(&ilst);
    let tags = read_tags(&moov);
    assert!(
        tags.contains(&("title".to_string(), "Title One".to_string())),
        "title before the malformed atom is recovered: {tags:?}"
    );
}

#[test]
fn read_tags_recovers_data_before_a_malformed_data_sibling() {
    // A good `data` followed by a malformed `data` inside one atom: the inner walk
    // must keep the good value rather than discarding the whole atom (#524).
    let mut inner = data_atom(1, b"Good");
    inner.extend_from_slice(&[0, 0, 0, 99, b'd', b'a', b't', b'a']); // overruns buffer
    let ilst = bx(b"\xa9nam", &inner);
    let moov = moov_with_ilst(&ilst);
    let tags = read_tags(&moov);
    assert!(
        tags.contains(&("title".to_string(), "Good".to_string())),
        "the good data box before the malformed one is recovered: {tags:?}"
    );
}

#[test]
fn read_binary_tags_recovers_binary_after_a_text_data() {
    // A `----` atom whose first `data` is type-1 text and a later one is binary:
    // the binary path must inspect every `data`, not just the first (#525).
    let mut mean_body = 0u32.to_be_bytes().to_vec();
    mean_body.extend_from_slice(b"com.apple.iTunes");
    let mut name_body = 0u32.to_be_bytes().to_vec();
    name_body.extend_from_slice(b"MIXED");
    let mut inner = boxed(b"mean", &mean_body).unwrap();
    inner.extend(boxed(b"name", &name_body).unwrap());
    inner.extend(data_atom(1, b"text-value")); // type 1 text first
    inner.extend(data_atom(0, &[0xDE, 0xAD])); // binary second
    let atom = boxed(b"----", &inner).unwrap();
    let moov = moov_with_ilst(&atom);

    let tags = read_binary_tags(&moov, usize::MAX);
    assert_eq!(
        tags.len(),
        1,
        "the binary value after the text one is recovered"
    );
    assert_eq!(tags[0].key, "----:com.apple.iTunes:MIXED");
    assert_eq!(tags[0].payload, vec![0xDE, 0xAD]);
}

#[test]
fn read_freeform_recovers_before_malformed_trailing_child() {
    // A `----` atom with a valid `name` + text `data` followed by a malformed
    // trailing child: the lenient `name` lookup must still recover the value
    // rather than the strict `find_box` dropping the whole atom (#524).
    let mut name_body = 0u32.to_be_bytes().to_vec();
    name_body.extend_from_slice(b"My Custom Field");
    let mut inner = boxed(b"name", &name_body).unwrap();
    inner.extend(data_atom(1, b"ok"));
    inner.extend_from_slice(&[0, 0, 0, 99, b'b', b'a', b'd', b'!']); // overruns the buffer
    assert_eq!(
        read_freeform(&inner),
        vec![("My Custom Field".to_string(), "ok".to_string())]
    );
}

#[test]
fn read_binary_tags_recovers_before_malformed_trailing_child() {
    // A `----` atom with valid mean/name/binary-data followed by a malformed
    // trailing child: the lenient name/mean lookup must still recover the tag (#524).
    let mut mean_body = 0u32.to_be_bytes().to_vec();
    mean_body.extend_from_slice(b"com.serato.dj");
    let mut name_body = 0u32.to_be_bytes().to_vec();
    name_body.extend_from_slice(b"analysis");
    let mut inner = boxed(b"mean", &mean_body).unwrap();
    inner.extend(boxed(b"name", &name_body).unwrap());
    inner.extend(data_atom(0, &[0xDE, 0xAD]));
    inner.extend_from_slice(&[0, 0, 0, 99, b'b', b'a', b'd', b'!']); // overruns the buffer
    let atom = boxed(b"----", &inner).unwrap();
    let moov = moov_with_ilst(&atom);
    let tags = read_binary_tags(&moov, usize::MAX);
    assert_eq!(
        tags.len(),
        1,
        "valid binary tag survives a malformed sibling"
    );
    assert_eq!(tags[0].key, "----:com.serato.dj:analysis");
    assert_eq!(tags[0].payload, vec![0xDE, 0xAD]);
}

#[test]
fn read_binary_tags_reporting_reports_oversize_drop() {
    // A 5-byte value over the 4-byte cap: skipped from the tags, but reported
    // as a drop with its `----:<mean>:<name>` key and exact byte size.
    let over = vec![0xABu8; 5];
    let atom = freeform_atom_typed("com.serato.dj", "analysis", 0, &over);
    let moov = moov_with_ilst(&atom);
    let (tags, dropped) = read_binary_tags_reporting(&moov, 4);
    assert!(tags.is_empty());
    assert_eq!(dropped.len(), 1);
    assert_eq!(dropped[0].descriptor, "----:com.serato.dj:analysis");
    assert_eq!(dropped[0].bytes, over.len());
}

#[test]
fn read_binary_tags_reporting_skips_oversize_text_without_reporting() {
    // An oversized *text* (type 1) freeform is the text path's job, never a
    // binary-tag drop — it must not be reported here.
    let over = vec![b'x'; 5];
    let atom = freeform_atom_typed("com.apple.iTunes", "MOOD", 1, &over);
    let moov = moov_with_ilst(&atom);
    let (tags, dropped) = read_binary_tags_reporting(&moov, 4);
    assert!(tags.is_empty());
    assert!(dropped.is_empty());
}

#[test]
fn synthesize_interleaves_binary_freeform_segment() {
    let buf = mk_mp4(true, b"AUDIODATA", &[42, 100]);
    let scan = read_structure(&buf).unwrap();
    let payload = vec![0xde, 0xad, 0xbe, 0xef, 0x00, 0x01];
    let bins = vec![BinaryTagInput {
        key: "----:com.serato.dj:analysis".into(),
        payload_id: 7,
        len: BlobLen::new(payload.len() as u64).unwrap(),
    }];
    let layout = synthesize_layout(&scan, &[TagInput::new("title", "T")], &bins, &[]).unwrap();

    // Exactly one streamed BinaryTag carrying our handle + length.
    let bt: Vec<_> = layout
        .segments()
        .iter()
        .filter_map(|s| match s {
            Segment::BinaryTag { payload_id, len } => Some((*payload_id, len.get())),
            _ => None,
        })
        .collect();
    assert_eq!(bt, vec![(7, payload.len() as u64)]);

    // Audio is still served verbatim as the trailing BackingAudio run.
    match layout.segments().last().unwrap() {
        Segment::BackingAudio { offset, len } => {
            assert_eq!(*offset, scan.mdat_payload_offset);
            assert_eq!(*len, scan.mdat_payload_len);
        }
        _ => panic!("expected BackingAudio tail"),
    }

    // Box sizes are self-consistent: materialize the served file (binary payload
    // + backing audio substituted) and re-parse. `read_structure` validates every
    // moov/mdat box size, so a green re-parse proves the ----/ilst/meta/udta/moov
    // sizes all account for the streamed payload — and the opaque `----` survives
    // the round trip byte-identically.
    //
    // NOTE: do NOT use `inline_head`/`find_moov_in_head` here — the moov box now
    // spans multiple segments (the streamed BinaryTag splits it), so `read_box`
    // on `segments[0]` alone returns `Malformed`. Materialize the whole file.
    let mut served = Vec::new();
    for seg in layout.segments() {
        match seg {
            Segment::Inline(b) => served.extend_from_slice(b),
            Segment::BinaryTag { .. } => served.extend_from_slice(&payload),
            Segment::BackingAudio { offset, len } => {
                let s = usize_from(*offset);
                served.extend_from_slice(&buf[s..s + usize_from(*len)]);
            }
            other => panic!("unexpected segment: {other:?}"),
        }
    }
    read_structure(&served).expect("synthesized file re-parses to a valid moov/mdat");
    // `read_binary_tags` returns a bare Vec (no promotion for MP4) and emits the
    // raw `mean:name` key WITHOUT folding through the vocabulary — `com.serato.dj`
    // is not in any vocabulary entry, so the key is preserved verbatim.
    let reparsed = read_binary_tags(&served, usize::MAX);
    assert_eq!(reparsed.len(), 1);
    assert_eq!(reparsed[0].key, "----:com.serato.dj:analysis");
    assert_eq!(reparsed[0].payload, payload);
}

#[test]
fn synthesize_new_moov_size_exactly_u32_max_is_ok() {
    // `if new_moov_size > u32::MAX` is strict. new_moov_size == u32::MAX must be
    // accepted; `> -> ==`/`>= ` reject the exact boundary. data_len (the art size)
    // is reserved as a number, so the boundary is cheap.
    fn art(data_len: u64) -> ArtInput {
        ArtInput {
            art_id: 1,
            mime: "image/jpeg".into(),
            description: String::new(),
            picture_type: PictureType::new(3).unwrap(),
            width: 0,
            height: 0,
            depth: 0,
            colors: 0,
            data_len: BlobLen::new(data_len).unwrap(),
        }
    }
    let buf = mk_mp4(true, b"AUDIO", &[0]);
    let scan = read_structure(&buf).unwrap();
    let tags = [TagInput::new("title", "T")];

    // Synthesize once with a 1-byte art. The head is [ftyp][moov], where the moov
    // box header declares new_moov_size = overhead + 1. The actual moov bytes in the
    // head are new_moov_size - 1 (the art is a separate ArtImage segment). So the
    // head length = ftyp.len() + (overhead + 1 - 1) = ftyp.len() + overhead.
    let layout1 = synthesize_layout(&scan, &tags, &[], &[art(1)]).unwrap();
    let head_len = inline_head(&layout1).len();
    let overhead = (head_len as u64) - (scan.ftyp.len() as u64);
    let max_len = u64::from(u32::MAX) - overhead;

    assert!(max_len > 0, "overhead {overhead} must be < u32::MAX");
    // Boundary accepted
    assert!(synthesize_layout(&scan, &tags, &[], &[art(max_len)]).is_ok());
    // Boundary+1 rejected
    assert!(matches!(
        synthesize_layout(&scan, &tags, &[], &[art(max_len + 1)]),
        Err(FormatError::TooLarge)
    ));
}

#[test]
fn synthesize_layout_emits_all_nonzero_arts() {
    // Both non-empty arts stream, in input order.
    let art = |id: i64, len: u64| ArtInput {
        art_id: id,
        mime: "image/jpeg".into(),
        description: String::new(),
        picture_type: PictureType::new(3).unwrap(),
        width: 0,
        height: 0,
        depth: 0,
        colors: 0,
        data_len: BlobLen::new(len).unwrap(),
    };
    let buf = mk_mp4(true, b"AUDIO", &[0]);
    let scan = read_structure(&buf).unwrap();
    let layout = synthesize_layout(
        &scan,
        &[TagInput::new("title", "T")],
        &[],
        &[art(1, 5), art(3, 7)],
    )
    .unwrap();
    let art_segs: Vec<(i64, u64)> = layout
        .segments()
        .iter()
        .filter_map(|s| match s {
            Segment::ArtImage { art_id, len } => Some((*art_id, len.get())),
            _ => None,
        })
        .collect();
    assert_eq!(art_segs, vec![(1, 5), (3, 7)]);
}

#[test]
fn read_structure_from_rejects_oversized_moov() {
    use std::io::Cursor;
    let moov_size: u32 = 600 * 1024 * 1024;
    let mut buf = Vec::new();
    buf.extend_from_slice(&16u32.to_be_bytes());
    buf.extend_from_slice(b"ftyp");
    buf.extend_from_slice(&[0u8; 8]);
    buf.extend_from_slice(&16u32.to_be_bytes());
    buf.extend_from_slice(b"mdat");
    buf.extend_from_slice(&[0u8; 8]);
    buf.extend_from_slice(&moov_size.to_be_bytes());
    buf.extend_from_slice(b"moov");
    assert_eq!(buf.len(), 40);
    let file_len = 32 + u64::from(moov_size);
    let mut cur = Cursor::new(buf);
    match read_structure_from(&mut cur, file_len).unwrap_err() {
        Mp4ScanError::MetadataTooLarge {
            box_kind,
            size,
            cap,
        } => {
            assert_eq!(box_kind, "moov");
            assert_eq!(size, u64::from(moov_size));
            assert_eq!(cap, 256 * 1024 * 1024);
        }
        other => panic!("expected MetadataTooLarge, got {other:?}"),
    }
}

#[test]
fn read_structure_from_admits_box_at_exactly_the_cap() {
    use std::io::Cursor;
    let cap: u32 = 256 * 1024 * 1024;
    let mut buf = Vec::new();
    buf.extend_from_slice(&16u32.to_be_bytes());
    buf.extend_from_slice(b"ftyp");
    buf.extend_from_slice(&[0u8; 8]);
    buf.extend_from_slice(&16u32.to_be_bytes());
    buf.extend_from_slice(b"mdat");
    buf.extend_from_slice(&[0u8; 8]);
    buf.extend_from_slice(&cap.to_be_bytes());
    buf.extend_from_slice(b"moov");
    let file_len = 32 + u64::from(cap);
    let mut cur = Cursor::new(buf);
    let err = read_structure_from(&mut cur, file_len).unwrap_err();
    assert!(
        matches!(err, Mp4ScanError::Io(_)),
        "exact-cap box must pass the strict `>` guard (got {err:?})"
    );
}

#[test]
fn build_udta_checked_art_len_rejects_overflow() {
    // A hostile art data_len near u64::MAX must fail closed with TooLarge at
    // the covr_size fold, not panic (debug) / wrap (release).
    let mk = |data_len: u64| crate::input::ArtInput {
        art_id: 1,
        mime: "image/png".to_string(),
        description: String::new(),
        picture_type: PictureType::new(3).unwrap(),
        width: 0,
        height: 0,
        depth: 0,
        colors: 0,
        data_len: BlobLen::new(data_len).unwrap(),
    };
    assert_eq!(
        build_udta(&[], &[], &[mk(u64::MAX)], None).err(),
        Some(FormatError::TooLarge)
    );
}

#[test]
fn build_udta_checked_binary_tag_len_rejects_overflow() {
    // A hostile freeform binary-tag len near u64::MAX must fail closed with
    // TooLarge inside freeform_binary_prefix's data_size/inner_len arithmetic,
    // not panic (debug) / wrap (release) before the u32 box-size narrowing.
    let bins = vec![crate::input::BinaryTagInput {
        key: "----:com.example:x".to_string(),
        payload_id: 1,
        len: BlobLen::new(u64::MAX).unwrap(),
    }];
    assert_eq!(
        build_udta(&[], &bins, &[], None).err(),
        Some(FormatError::TooLarge)
    );
}

#[test]
fn freeform_binary_prefix_checked_outer_box_size_rejects_overflow() {
    // A payload_len that slips past the data_size and inner_len checks can
    // still overflow the outer `8 + inner_len` box-size add. With 1-char
    // mean/name each boxed mean/name is 13 bytes, so inner_len = 26 + data_size
    // = 42 + payload_len; payload_len = u64::MAX - 42 drives inner_len to exactly
    // u64::MAX, so the outer add must fail closed, not panic (debug) / wrap (release).
    assert_eq!(
        freeform_binary_prefix("m", "n", u64::MAX - 42).err(),
        Some(FormatError::TooLarge)
    );
}

// ── QuickTime keyed metadata (`mdta` handler, #771) ──────────────────────────

/// A QuickTime `hdlr` declaring keyed metadata: `[version/flags][pre_defined]
/// ['mdta'][reserved]`, the shape ffmpeg's `mov_write_mdta_hdlr_tag` writes.
fn mdta_hdlr() -> Vec<u8> {
    let mut p = vec![0u8; 8];
    p.extend_from_slice(b"mdta");
    p.extend_from_slice(&[0u8; 13]);
    bx(b"hdlr", &p)
}

/// A `keys` FullBox: `[version/flags][entry_count]` then per entry
/// `[key_size][key_namespace][key_value]`.
fn keys_box(entries: &[(&[u8; 4], &[u8])]) -> Vec<u8> {
    let mut p = vec![0u8; 4];
    p.extend_from_slice(&u32::try_from(entries.len()).unwrap().to_be_bytes());
    for (namespace, name) in entries {
        p.extend_from_slice(&u32::try_from(8 + name.len()).unwrap().to_be_bytes());
        p.extend_from_slice(*namespace);
        p.extend_from_slice(name);
    }
    bx(b"keys", &p)
}

/// A keyed `ilst` item: its box type is the 1-based index into `keys`.
fn keyed_item(index: u32, datas: &[u8]) -> Vec<u8> {
    bx(&index.to_be_bytes(), datas)
}

/// A `data` box with an explicit locale indicator.
fn data_atom_locale(type_code: u32, locale: u32, value: &[u8]) -> Vec<u8> {
    let mut p = type_code.to_be_bytes().to_vec();
    p.extend_from_slice(&locale.to_be_bytes());
    p.extend_from_slice(value);
    bx(b"data", &p)
}

/// The children of a keyed-metadata `meta`: `mdta` hdlr, a `keys` table naming
/// each item's key, and an `ilst` whose `i`th item (index `i + 1`) carries that
/// item's `data` boxes.
fn keyed_meta_children(items: &[(&str, Vec<u8>)]) -> Vec<u8> {
    let names: Vec<(&[u8; 4], &[u8])> =
        items.iter().map(|(n, _)| (b"mdta", n.as_bytes())).collect();
    let ilst: Vec<u8> = items
        .iter()
        .enumerate()
        .flat_map(|(i, (_, datas))| keyed_item(u32::try_from(i + 1).unwrap(), datas))
        .collect();
    [mdta_hdlr(), keys_box(&names), bx(b"ilst", &ilst)].concat()
}

/// A bare (QuickTime-style, no version/flags) keyed-metadata `meta` box, the
/// shape Apple writes at the movie and track levels.
fn keyed_meta(items: &[(&str, Vec<u8>)]) -> Vec<u8> {
    bx(b"meta", &keyed_meta_children(items))
}

/// A FullBox keyed-metadata `meta` inside `udta`, the shape ffmpeg's
/// `-movflags use_metadata_tags` writes (`mov_write_udta_tag` → `mov_write_meta_tag`).
fn ffmpeg_keyed_udta_meta(items: &[(&str, Vec<u8>)]) -> Vec<u8> {
    let mut p = vec![0u8; 4];
    p.extend(keyed_meta_children(items));
    bx(b"meta", &p)
}

/// An iTunes `meta` (FullBox, `mdir` handler) holding `ilst_atoms`.
fn itunes_meta(ilst_atoms: &[u8]) -> Vec<u8> {
    let mut hdlr = vec![0u8; 8];
    hdlr.extend_from_slice(b"mdir");
    hdlr.extend_from_slice(b"appl");
    hdlr.extend_from_slice(&[0u8; 9]);
    let mut meta = vec![0u8; 4];
    meta.extend(bx(b"hdlr", &hdlr));
    meta.extend(bx(b"ilst", ilst_atoms));
    bx(b"meta", &meta)
}

/// An accepted moov-first file: one `soun` trak (with `mdia_extra` appended to
/// its `mdia` and `trak_extra` to the trak), `moov_extra` appended to `moov`, and
/// `audio` as the `mdat` payload. The single `stco` entry holds the real payload
/// offset, so synthesis can relocate it in either direction.
fn mp4_with_keyed(
    moov_extra: &[u8],
    trak_extra: &[u8],
    mdia_extra: &[u8],
    audio: &[u8],
) -> Vec<u8> {
    let mut hdlr_p = vec![0u8; 8];
    hdlr_p.extend_from_slice(b"soun");
    hdlr_p.extend_from_slice(&[0u8; 12]);
    let mut stco = vec![0u8; 4];
    stco.extend_from_slice(&1u32.to_be_bytes());
    stco.extend_from_slice(&0u32.to_be_bytes());
    let minf = bx(b"minf", &bx(b"stbl", &bx(b"stco", &stco)));
    let mdia = bx(
        b"mdia",
        &[bx(b"hdlr", &hdlr_p), minf, mdia_extra.to_vec()].concat(),
    );
    let trak = bx(b"trak", &[mdia, trak_extra.to_vec()].concat());
    let moov = bx(
        b"moov",
        &[bx(b"mvhd", &[0u8; 8]), trak, moov_extra.to_vec()].concat(),
    );
    let mut out = [bx(b"ftyp", b"M4A isom"), moov, bx(b"mdat", audio)].concat();
    let payload_at = u32::try_from(out.len() - audio.len()).unwrap();
    let entry = out.windows(4).position(|w| w == b"stco").unwrap() + 12;
    out[entry..entry + 4].copy_from_slice(&payload_at.to_be_bytes());
    out
}

fn text(value: &str) -> Vec<u8> {
    data_atom(1, value.as_bytes())
}

#[test]
fn read_tags_ingests_movie_level_keyed_metadata() {
    // Apple's movie-level `moov/meta`: well-known keys fold onto the canonical
    // vocabulary; an unknown key is kept under its verbatim name, like an
    // unknown `----` freeform atom.
    let meta = keyed_meta(&[
        ("com.apple.quicktime.title", text("Keyed Title")),
        ("com.apple.quicktime.artist", text("Keyed Artist")),
        ("com.apple.quicktime.album", text("Keyed Album")),
        ("com.apple.quicktime.year", text("2012")),
        (
            "com.apple.quicktime.location.ISO6709",
            text("+27.5916+086.5640+8850/"),
        ),
    ]);
    let buf = mp4_with_keyed(&meta, &[], &[], b"AUDIO");
    assert_eq!(
        read_tags(&buf),
        vec![
            ("title".to_string(), "Keyed Title".to_string()),
            ("artist".to_string(), "Keyed Artist".to_string()),
            ("album".to_string(), "Keyed Album".to_string()),
            ("date".to_string(), "2012".to_string()),
            (
                "com.apple.quicktime.location.ISO6709".to_string(),
                "+27.5916+086.5640+8850/".to_string()
            ),
        ]
    );
}

#[test]
fn read_tags_ingests_every_well_known_quicktime_key() {
    let cases = [
        ("com.apple.quicktime.title", "title"),
        ("com.apple.quicktime.artist", "artist"),
        ("com.apple.quicktime.album", "album"),
        ("com.apple.quicktime.genre", "genre"),
        ("com.apple.quicktime.comment", "comment"),
        ("com.apple.quicktime.copyright", "copyright"),
        ("com.apple.quicktime.year", "date"),
        ("album_artist", "albumartist"),
        ("track", "tracknumber"),
        ("disc", "discnumber"),
    ];
    for (name, canonical) in cases {
        let buf = mp4_with_keyed(&keyed_meta(&[(name, text("v"))]), &[], &[], b"A");
        assert_eq!(
            read_tags(&buf),
            vec![(canonical.to_string(), "v".to_string())],
            "{name}"
        );
    }
}

#[test]
fn read_tags_ingests_ffmpeg_keyed_metadata_in_udta() {
    // ffmpeg `-movflags use_metadata_tags` writes `moov/udta/meta` (FullBox) with
    // an `mdta` handler and its own generic key names. `album_artist`, `track`
    // and `disc` fold; names already canonical (`artist`) need no mapping; the rest
    // (`encoder`) are kept verbatim.
    let meta = ffmpeg_keyed_udta_meta(&[
        ("artist", text("Old Artist")),
        ("album_artist", text("Band")),
        ("track", text("3/12")),
        ("encoder", text("Lavf63.1.101")),
    ]);
    let buf = mp4_with_keyed(&bx(b"udta", &meta), &[], &[], b"AUDIO");
    assert_eq!(
        read_tags(&buf),
        vec![
            ("artist".to_string(), "Old Artist".to_string()),
            ("albumartist".to_string(), "Band".to_string()),
            ("tracknumber".to_string(), "3/12".to_string()),
            ("encoder".to_string(), "Lavf63.1.101".to_string()),
        ]
    );
}

#[test]
fn read_tags_ingests_track_and_media_level_keyed_metadata() {
    // Apple devices write keyed metadata in the audio track too (ExifTool's
    // "AudioKeys": `player.movie.audio.*`); the QTFF allows `trak` and `mdia`.
    let trak_meta = keyed_meta(&[("player.movie.audio.mute", data_atom(75, &[1]))]);
    let mdia_meta = keyed_meta(&[("com.apple.quicktime.comment", text("From mdia"))]);
    let buf = mp4_with_keyed(&[], &trak_meta, &mdia_meta, b"AUDIO");
    assert_eq!(
        read_tags(&buf),
        vec![
            ("player.movie.audio.mute".to_string(), "1".to_string()),
            ("comment".to_string(), "From mdia".to_string()),
        ]
    );
}

#[test]
fn read_tags_itunes_values_win_over_keyed_values_per_key() {
    // Both systems name `artist`: the iTunes `ilst` is what music taggers edit,
    // so it wins outright. A key only the keyed metadata carries still fills in.
    let udta = bx(
        b"udta",
        &itunes_meta(&bx(b"\xa9ART", &text("iTunes Artist"))),
    );
    let meta = keyed_meta(&[
        ("com.apple.quicktime.artist", text("Keyed Artist")),
        ("com.apple.quicktime.genre", text("Keyed Genre")),
    ]);
    let buf = mp4_with_keyed(&[udta, meta].concat(), &[], &[], b"AUDIO");
    assert_eq!(
        read_tags(&buf),
        vec![
            ("artist".to_string(), "iTunes Artist".to_string()),
            ("genre".to_string(), "Keyed Genre".to_string()),
        ]
    );
}

#[test]
fn read_tags_itunes_multi_value_is_not_topped_up_by_keyed() {
    // All-or-nothing per key: two iTunes artists keep exactly their two values.
    let udta = bx(
        b"udta",
        &itunes_meta(&bx(b"\xa9ART", &[text("A"), text("B")].concat())),
    );
    let meta = keyed_meta(&[("ARTIST", text("Keyed"))]);
    let buf = mp4_with_keyed(&[meta, udta].concat(), &[], &[], b"AUDIO");
    assert_eq!(
        read_tags(&buf),
        vec![
            ("artist".to_string(), "A".to_string()),
            ("artist".to_string(), "B".to_string()),
        ]
    );
}

#[test]
fn read_tags_first_keyed_item_wins_per_key() {
    // Movie level beats udta beats track beats media; within one `meta`, the first
    // item that yields a value for a canonical key wins, compared case-insensitively.
    let movie = keyed_meta(&[
        ("com.apple.quicktime.artist", text("Movie")),
        ("artist", text("Movie bare")),
    ]);
    let udta = bx(
        b"udta",
        &ffmpeg_keyed_udta_meta(&[
            ("ARTIST", text("Udta")),
            ("com.apple.quicktime.album", text("Udta Album")),
        ]),
    );
    let trak = keyed_meta(&[
        ("com.apple.quicktime.album", text("Track Album")),
        ("com.apple.quicktime.genre", text("Track Genre")),
    ]);
    let mdia = keyed_meta(&[("com.apple.quicktime.genre", text("Media Genre"))]);
    // `udta` placed before the movie-level meta: precedence is by level, not order.
    let buf = mp4_with_keyed(&[udta, movie].concat(), &trak, &mdia, b"AUDIO");
    assert_eq!(
        read_tags(&buf),
        vec![
            ("artist".to_string(), "Movie".to_string()),
            ("album".to_string(), "Udta Album".to_string()),
            ("genre".to_string(), "Track Genre".to_string()),
        ]
    );
}

#[test]
fn read_tags_unmapped_key_matching_is_case_insensitive_across_sources() {
    // An unknown key keeps its first spelling; a later differently-cased spelling
    // of the same key is the same key and is not added again.
    let movie = keyed_meta(&[("com.example.Mood", text("calm"))]);
    let trak = keyed_meta(&[("COM.EXAMPLE.MOOD", text("loud"))]);
    let buf = mp4_with_keyed(&movie, &trak, &[], b"AUDIO");
    assert_eq!(
        read_tags(&buf),
        vec![("com.example.Mood".to_string(), "calm".to_string())]
    );
}

#[test]
fn read_tags_decodes_keyed_value_types() {
    let utf16 = |s: &str, bom: bool| -> Vec<u8> {
        let mut v = Vec::new();
        if bom {
            v.extend_from_slice(&0xFEFFu16.to_be_bytes());
        }
        for u in s.encode_utf16() {
            v.extend_from_slice(&u.to_be_bytes());
        }
        v
    };
    let meta = keyed_meta(&[
        ("utf8", data_atom(1, "héllo".as_bytes())),
        ("utf16", data_atom(2, &utf16("wörld", false))),
        ("utf16bom", data_atom(2, &utf16("bom", true))),
        ("be_signed_1", data_atom(21, &[0xFF])),
        ("be_signed_3", data_atom(21, &[0xFF, 0xFF, 0xFE])),
        ("be_signed_4_pos", data_atom(21, &[0x00, 0x01, 0x00, 0x00])),
        ("be_signed_8", data_atom(21, &i64::MIN.to_be_bytes())),
        ("be_unsigned_2", data_atom(22, &[0xFF, 0xFE])),
        ("be_unsigned_8", data_atom(22, &u64::MAX.to_be_bytes())),
        ("i8", data_atom(65, &[0x80])),
        ("i16", data_atom(66, &(-300i16).to_be_bytes())),
        ("i32", data_atom(67, &(-70_000i32).to_be_bytes())),
        ("i64", data_atom(74, &(-5i64).to_be_bytes())),
        ("u8", data_atom(75, &[200])),
        ("u16", data_atom(76, &65_000u16.to_be_bytes())),
        ("u32", data_atom(77, &4_000_000_000u32.to_be_bytes())),
        ("u64", data_atom(78, &(1u64 << 40).to_be_bytes())),
        ("f32", data_atom(23, &4.5f32.to_be_bytes())),
        ("f64", data_atom(24, &0.1f64.to_be_bytes())),
    ]);
    let buf = mp4_with_keyed(&meta, &[], &[], b"AUDIO");
    let want: Vec<(String, String)> = [
        ("utf8", "héllo"),
        ("utf16", "wörld"),
        ("utf16bom", "bom"),
        ("be_signed_1", "-1"),
        ("be_signed_3", "-2"),
        ("be_signed_4_pos", "65536"),
        ("be_signed_8", "-9223372036854775808"),
        ("be_unsigned_2", "65534"),
        ("be_unsigned_8", "18446744073709551615"),
        ("i8", "-128"),
        ("i16", "-300"),
        ("i32", "-70000"),
        ("i64", "-5"),
        ("u8", "200"),
        ("u16", "65000"),
        ("u32", "4000000000"),
        ("u64", "1099511627776"),
        ("f32", "4.5"),
        ("f64", "0.1"),
    ]
    .iter()
    .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
    .collect();
    assert_eq!(read_tags(&buf), want);
}

#[test]
fn read_tags_skips_keyed_values_it_cannot_represent() {
    // Every one of these is dropped, leaving only the sentinel.
    let meta = keyed_meta(&[
        ("reserved", data_atom(0, b"raw")),
        ("sjis", data_atom(3, b"\x82\xa0")),
        ("utf8_sort", data_atom(4, b"sort")),
        ("utf16_sort", data_atom(5, &[0, b's'])),
        ("jpeg_on_text_key", data_atom(13, &[0xFF, 0xD8])),
        ("bmp", data_atom(27, b"BM")),
        ("nested_meta", data_atom(28, &[0; 8])),
        ("point", data_atom(70, &[0; 8])),
        ("bad_utf8", data_atom(1, &[0xC3])),
        ("utf16_odd", data_atom(2, &[0, b'a', 0])),
        ("utf16_lone_surrogate", data_atom(2, &[0xD8, 0x00])),
        ("be_signed_empty", data_atom(21, &[])),
        ("be_signed_9", data_atom(21, &[0; 9])),
        ("be_unsigned_empty", data_atom(22, &[])),
        ("be_unsigned_9", data_atom(22, &[0; 9])),
        ("i8_wide", data_atom(65, &[0, 0])),
        ("i16_short", data_atom(66, &[0])),
        ("i32_short", data_atom(67, &[0; 3])),
        ("i64_short", data_atom(74, &[0; 7])),
        ("u8_wide", data_atom(75, &[0, 0])),
        ("u16_wide", data_atom(76, &[0; 3])),
        ("u32_wide", data_atom(77, &[0; 5])),
        ("u64_wide", data_atom(78, &[0; 9])),
        ("f32_short", data_atom(23, &[0; 3])),
        ("f64_short", data_atom(24, &[0; 4])),
        ("f32_nan", data_atom(23, &f32::NAN.to_be_bytes())),
        ("f64_inf", data_atom(24, &f64::INFINITY.to_be_bytes())),
        ("short_data", bx(b"data", &[0, 0, 0, 1, 0, 0, 0])),
        ("no_data", bx(b"free", b"")),
        ("sentinel", text("kept")),
    ]);
    let buf = mp4_with_keyed(&meta, &[], &[], b"AUDIO");
    assert_eq!(
        read_tags(&buf),
        vec![("sentinel".to_string(), "kept".to_string())]
    );
}

#[test]
fn read_tags_picks_one_keyed_value_preferring_the_default_locale() {
    // Several `data` boxes in one item are alternative representations (QTFF "Data
    // ordering"), not multiple values: one is chosen. The first default-locale (0)
    // value wins; with none, the last decodable one — the most general, since data
    // is ordered most-specific first.
    let us_eng = u32::from_be_bytes([b'U', b'S', 0x15, 0xC7]);
    let meta = keyed_meta(&[
        (
            "default_later",
            [
                data_atom_locale(1, us_eng, b"localized"),
                data_atom_locale(1, 0, b"default"),
                data_atom_locale(1, 0, b"second default"),
            ]
            .concat(),
        ),
        (
            "all_localized",
            [
                data_atom_locale(1, us_eng, b"first"),
                data_atom_locale(1, us_eng + 1, b"last"),
                data_atom_locale(13, us_eng, &[0xFF, 0xD8]),
            ]
            .concat(),
        ),
        (
            "default_image_then_text",
            [
                data_atom_locale(13, 0, &[0xFF, 0xD8]),
                data_atom_locale(1, us_eng, b"text"),
            ]
            .concat(),
        ),
    ]);
    let buf = mp4_with_keyed(&meta, &[], &[], b"AUDIO");
    assert_eq!(
        read_tags(&buf),
        vec![
            ("default_later".to_string(), "default".to_string()),
            ("all_localized".to_string(), "last".to_string()),
            ("default_image_then_text".to_string(), "text".to_string()),
        ]
    );
}

#[test]
fn read_tags_keyed_index_resolution_is_lenient() {
    // keys: [1] mdta "one", [2] 'udta'-namespace "@cpy" (not a string key), [3]
    // invalid UTF-8, [4] mdta "four". Items: index 0 (reserved), 1, 2, 3, 4, and 9
    // (out of range). Only indices 1 and 4 resolve; a skipped slot does not shift
    // the later indices.
    let keys = keys_box(&[
        (b"mdta", b"one"),
        (b"udta", b"@cpy"),
        (b"mdta", &[0xFF, 0xFE]),
        (b"mdta", b"four"),
    ]);
    let ilst = [
        keyed_item(0, &text("zero")),
        keyed_item(1, &text("1")),
        keyed_item(2, &text("2")),
        keyed_item(3, &text("3")),
        keyed_item(4, &text("4")),
        keyed_item(9, &text("9")),
    ]
    .concat();
    let meta = bx(b"meta", &[mdta_hdlr(), keys, bx(b"ilst", &ilst)].concat());
    let buf = mp4_with_keyed(&meta, &[], &[], b"AUDIO");
    assert_eq!(
        read_tags(&buf),
        vec![
            ("one".to_string(), "1".to_string()),
            ("four".to_string(), "4".to_string()),
        ]
    );
}

#[test]
fn read_tags_keys_table_that_lies_keeps_its_readable_prefix() {
    // entry_count claims u32::MAX (never allocated for), and the third entry's size
    // is below the 8-byte minimum: the two entries before it resolve, nothing after.
    let mut keys_p = vec![0u8; 4];
    keys_p.extend_from_slice(&u32::MAX.to_be_bytes());
    for name in [b"aa", b"bb"] {
        keys_p.extend_from_slice(&10u32.to_be_bytes());
        keys_p.extend_from_slice(b"mdta");
        keys_p.extend_from_slice(name);
    }
    keys_p.extend_from_slice(&7u32.to_be_bytes()); // size < 8: malformed
    keys_p.extend_from_slice(b"mdta");
    keys_p.extend_from_slice(&10u32.to_be_bytes()); // would be a 4th entry
    keys_p.extend_from_slice(b"mdtacc");
    let ilst = [
        keyed_item(1, &text("a")),
        keyed_item(2, &text("b")),
        keyed_item(3, &text("c")),
        keyed_item(4, &text("d")),
    ]
    .concat();
    let meta = bx(
        b"meta",
        &[mdta_hdlr(), bx(b"keys", &keys_p), bx(b"ilst", &ilst)].concat(),
    );
    let buf = mp4_with_keyed(&meta, &[], &[], b"AUDIO");
    assert_eq!(
        read_tags(&buf),
        vec![
            ("aa".to_string(), "a".to_string()),
            ("bb".to_string(), "b".to_string()),
        ]
    );
}

#[test]
fn read_tags_keys_entry_count_bounds_the_table() {
    // entry_count 1 with a second well-formed entry present: only index 1 exists.
    let mut keys_p = vec![0u8; 4];
    keys_p.extend_from_slice(&1u32.to_be_bytes());
    for name in [b"aa", b"bb"] {
        keys_p.extend_from_slice(&10u32.to_be_bytes());
        keys_p.extend_from_slice(b"mdta");
        keys_p.extend_from_slice(name);
    }
    let ilst = [keyed_item(1, &text("a")), keyed_item(2, &text("b"))].concat();
    let meta = bx(
        b"meta",
        &[mdta_hdlr(), bx(b"keys", &keys_p), bx(b"ilst", &ilst)].concat(),
    );
    let buf = mp4_with_keyed(&meta, &[], &[], b"AUDIO");
    assert_eq!(read_tags(&buf), vec![("aa".to_string(), "a".to_string())]);
}

#[test]
fn read_tags_keys_entry_exactly_filling_the_table_resolves() {
    // An entry whose key_size reaches exactly the end of the `keys` payload is
    // well-formed; one byte more overruns and is dropped.
    let exact = keys_box(&[(b"mdta", b"exact")]);
    let ilst = bx(b"ilst", &keyed_item(1, &text("v")));
    let meta = bx(b"meta", &[mdta_hdlr(), exact, ilst.clone()].concat());
    let buf = mp4_with_keyed(&meta, &[], &[], b"A");
    assert_eq!(
        read_tags(&buf),
        vec![("exact".to_string(), "v".to_string())]
    );

    let mut over_p = vec![0u8; 4];
    over_p.extend_from_slice(&1u32.to_be_bytes());
    over_p.extend_from_slice(&14u32.to_be_bytes()); // claims 14, 13 present
    over_p.extend_from_slice(b"mdta");
    over_p.extend_from_slice(b"exact");
    let meta = bx(b"meta", &[mdta_hdlr(), bx(b"keys", &over_p), ilst].concat());
    let buf = mp4_with_keyed(&meta, &[], &[], b"A");
    assert!(read_tags(&buf).is_empty());
}

#[test]
fn read_tags_keyed_meta_survives_garbled_siblings() {
    // A malformed box trailing `ilst` inside the keyed `meta`, and a malformed item
    // after a good one: the good values are still read (#524/#542 contract).
    let ilst = {
        let mut v = keyed_item(1, &text("Good"));
        v.extend_from_slice(&[0, 0, 0, 99, 0, 0, 0, 2]); // item claims 99 bytes
        bx(b"ilst", &v)
    };
    let mut children = [
        mdta_hdlr(),
        keys_box(&[(b"mdta", b"artist"), (b"mdta", b"album")]),
        ilst,
    ]
    .concat();
    children.extend_from_slice(&[0, 0, 0, 50, b'j', b'u', b'n', b'k']);
    let buf = mp4_with_keyed(&bx(b"meta", &children), &[], &[], b"AUDIO");
    assert_eq!(
        read_tags(&buf),
        vec![("artist".to_string(), "Good".to_string())]
    );
}

#[test]
fn read_tags_ignores_index_items_without_an_mdta_handler() {
    // Keys and index items are only meaningful under an `mdta` handler (ffmpeg's
    // `found_hdlr_mdta` gate): a `meta` with another handler, or none, is not
    // keyed metadata.
    let items = [("com.apple.quicktime.artist", text("X"))];
    let mut no_hdlr = keyed_meta_children(&items);
    no_hdlr.drain(..mdta_hdlr().len());
    let mut mdir = keyed_meta_children(&items);
    let at = mdir.windows(4).position(|w| w == b"mdta").unwrap();
    mdir[at..at + 4].copy_from_slice(b"mdir");
    for children in [no_hdlr, mdir] {
        let buf = mp4_with_keyed(&bx(b"meta", &children), &[], &[], b"AUDIO");
        assert!(read_tags(&buf).is_empty());
    }
}

#[test]
fn read_tags_reads_itunes_ilst_beside_an_ffmpeg_keyed_meta_in_udta() {
    // A `udta` holding both an `mdta` meta and an iTunes meta, in either order:
    // the iTunes `ilst` is found (it used to be "the first meta", which lost it
    // when the keyed one came first), and each system's values are read once.
    let keyed =
        ffmpeg_keyed_udta_meta(&[("artist", text("Keyed")), ("album", text("Keyed Album"))]);
    let itunes = itunes_meta(&bx(b"\xa9ART", &text("iTunes")));
    for udta in [
        bx(b"udta", &[keyed.clone(), itunes.clone()].concat()),
        bx(b"udta", &[itunes.clone(), keyed.clone()].concat()),
    ] {
        let buf = mp4_with_keyed(&udta, &[], &[], b"AUDIO");
        assert_eq!(
            read_tags(&buf),
            vec![
                ("artist".to_string(), "iTunes".to_string()),
                ("album".to_string(), "Keyed Album".to_string()),
            ]
        );
    }
}

#[test]
fn read_tags_reads_keyed_metadata_from_a_moov_only_buffer() {
    // The bounded probe hands the readers `scan.moov` alone, not the whole file.
    let buf = mp4_with_keyed(
        &keyed_meta(&[("com.apple.quicktime.title", text("T"))]),
        &[],
        &[],
        b"AUDIO",
    );
    let scan = read_structure(&buf).unwrap();
    assert_eq!(
        read_tags(&scan.moov),
        vec![("title".to_string(), "T".to_string())]
    );
}

#[test]
fn read_pictures_ingests_keyed_artwork() {
    let png = [0x89, b'P', b'N', b'G', 1, 2];
    let jpeg = [0xFF, 0xD8, 0xFF, 0xE0, 3];
    for (type_code, bytes, mime) in [
        (14u32, &png[..], "image/png"),
        (13, &jpeg[..], "image/jpeg"),
    ] {
        let meta = keyed_meta(&[("com.apple.quicktime.artwork", data_atom(type_code, bytes))]);
        let buf = mp4_with_keyed(&meta, &[], &[], b"AUDIO");
        let (pics, dropped) = read_pictures_reporting(&buf, usize::MAX);
        assert!(dropped.is_empty());
        assert_eq!(pics.len(), 1, "{mime}");
        assert_eq!(pics[0].mime, mime);
        assert_eq!(pics[0].data, bytes);
        assert_eq!(pics[0].picture_type, PictureType::new(3).unwrap());
        assert!(pics[0].description.is_empty());
        // Artwork is art, never a text tag.
        assert!(read_tags(&buf).is_empty());
    }
}

#[test]
fn read_pictures_keyed_artwork_is_one_picture_from_the_first_source() {
    // Two representations in one item (large, thumbnail) are one artwork; a second
    // artwork at the track level is a lower-precedence source and is not used. BMP
    // is skipped like a non-JPEG/PNG `covr`, so the PNG after it is chosen.
    let movie = keyed_meta(&[(
        "com.apple.quicktime.artwork",
        [
            data_atom(27, b"BMbmp"),
            data_atom(13, b"large"),
            data_atom(13, b"thumb"),
        ]
        .concat(),
    )]);
    let trak = keyed_meta(&[("com.apple.quicktime.artwork", data_atom(14, b"track"))]);
    let buf = mp4_with_keyed(&movie, &trak, &[], b"AUDIO");
    let pics = read_pictures(&buf, usize::MAX);
    assert_eq!(pics.len(), 1);
    assert_eq!(pics[0].data, b"large");
}

#[test]
fn read_pictures_falls_through_an_artwork_item_with_no_image() {
    // A movie-level artwork item holding no usable image does not claim the slot:
    // the track-level artwork is used.
    let movie = keyed_meta(&[("com.apple.quicktime.artwork", data_atom(27, b"BM"))]);
    let trak = keyed_meta(&[("com.apple.quicktime.artwork", data_atom(14, b"track"))]);
    let buf = mp4_with_keyed(&movie, &trak, &[], b"AUDIO");
    let pics = read_pictures(&buf, usize::MAX);
    assert_eq!(pics.len(), 1);
    assert_eq!(pics[0].data, b"track");
}

#[test]
fn read_pictures_covr_wins_over_keyed_artwork() {
    let udta = bx(b"udta", &itunes_meta(&bx(b"covr", &data_atom(13, b"covr"))));
    let meta = keyed_meta(&[("com.apple.quicktime.artwork", data_atom(14, b"keyed"))]);
    let buf = mp4_with_keyed(&[meta.clone(), udta].concat(), &[], &[], b"AUDIO");
    let pics = read_pictures(&buf, usize::MAX);
    assert_eq!(pics.len(), 1);
    assert_eq!(pics[0].data, b"covr");

    // A `covr` whose only image is an unsupported type yields nothing, so the keyed
    // artwork fills in.
    let udta = bx(b"udta", &itunes_meta(&bx(b"covr", &data_atom(27, b"BM"))));
    let buf = mp4_with_keyed(&[meta, udta].concat(), &[], &[], b"AUDIO");
    let pics = read_pictures(&buf, usize::MAX);
    assert_eq!(pics.len(), 1);
    assert_eq!(pics[0].data, b"keyed");
}

#[test]
fn read_pictures_an_oversize_covr_still_claims_the_art_slot() {
    // An oversize `covr` is reported (and fails the file upstream, #644); the keyed
    // artwork is not silently substituted for it.
    let udta = bx(b"udta", &itunes_meta(&bx(b"covr", &data_atom(13, &[0; 5]))));
    let meta = keyed_meta(&[("com.apple.quicktime.artwork", data_atom(14, b"k"))]);
    let buf = mp4_with_keyed(&[meta, udta].concat(), &[], &[], b"AUDIO");
    let (pics, dropped) = read_pictures_reporting(&buf, 4);
    assert!(pics.is_empty());
    assert_eq!(dropped.len(), 1);
    assert_eq!(dropped[0].descriptor, "image/jpeg");
}

#[test]
fn read_pictures_reporting_caps_keyed_artwork() {
    let meta = keyed_meta(&[("com.apple.quicktime.artwork", data_atom(14, &[7; 5]))]);
    let buf = mp4_with_keyed(&meta, &[], &[], b"AUDIO");
    let (pics, dropped) = read_pictures_reporting(&buf, 4);
    assert!(pics.is_empty());
    assert_eq!(
        dropped,
        vec![OversizeDrop {
            descriptor: "image/png".to_string(),
            bytes: 5
        }]
    );
    let (pics, dropped) = read_pictures_reporting(&buf, 5);
    assert_eq!(pics.len(), 1);
    assert!(dropped.is_empty());
}

/// A FullBox `meta` with a `hdlr` naming `handler`, then `body` verbatim.
fn handler_meta(handler: &[u8; 4], body: &[u8]) -> Vec<u8> {
    let mut hdlr = vec![0u8; 8];
    hdlr.extend_from_slice(handler);
    hdlr.extend_from_slice(&[0u8; 12]);
    let mut p = vec![0u8; 4];
    p.extend(bx(b"hdlr", &hdlr));
    p.extend_from_slice(body);
    bx(b"meta", &p)
}

/// A `soun` `mdia` with one `stco` entry of 0 (patched by [`mp4_around_traks`])
/// followed by `extra` children.
fn soun_mdia(extra: &[u8]) -> Vec<u8> {
    let mut hdlr_p = vec![0u8; 8];
    hdlr_p.extend_from_slice(b"soun");
    hdlr_p.extend_from_slice(&[0u8; 12]);
    let mut stco = vec![0u8; 4];
    stco.extend_from_slice(&1u32.to_be_bytes());
    stco.extend_from_slice(&0u32.to_be_bytes());
    let minf = bx(b"minf", &bx(b"stbl", &bx(b"stco", &stco)));
    bx(
        b"mdia",
        &[bx(b"hdlr", &hdlr_p), minf, extra.to_vec()].concat(),
    )
}

/// `ftyp`, then `moov` = `mvhd` + `moov_children`, then `mdat` holding `audio`.
/// The `n`th `stco` in the file gets the payload offset plus `chunk_offsets[n]`.
fn mp4_around(moov_children: &[u8], audio: &[u8], chunk_offsets: &[u32]) -> Vec<u8> {
    let moov = bx(
        b"moov",
        &[bx(b"mvhd", &[0u8; 8]), moov_children.to_vec()].concat(),
    );
    let mut out = [bx(b"ftyp", b"M4A isom"), moov, bx(b"mdat", audio)].concat();
    let payload_at = u32::try_from(out.len() - audio.len()).unwrap();
    let tables: Vec<usize> = out
        .windows(4)
        .enumerate()
        .filter_map(|(i, w)| (w == b"stco").then_some(i))
        .collect();
    assert_eq!(tables.len(), chunk_offsets.len(), "one offset per stco");
    for (at, add) in tables.into_iter().zip(chunk_offsets) {
        let entry = at + 12;
        out[entry..entry + 4].copy_from_slice(&(payload_at + add).to_be_bytes());
    }
    out
}

/// Materialize a layout with no streamed segments: inline bytes, and backing
/// audio read from `backing`.
fn serve_unstreamed(layout: &RegionLayout, backing: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    for seg in layout.segments() {
        match seg {
            Segment::Inline(b) => out.extend_from_slice(b),
            Segment::BackingAudio { offset, len } => {
                let s = usize_from(*offset);
                out.extend_from_slice(&backing[s..s + usize_from(*len)]);
            }
            other => panic!("unexpected streamed segment: {other:?}"),
        }
    }
    out
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

/// The whole-box bytes of every child of `buf` of type `kind`.
fn child_box_bytes(buf: &[u8], kind: &[u8; 4]) -> Vec<Vec<u8>> {
    child_boxes(buf)
        .unwrap()
        .into_iter()
        .filter(|b| &b.kind == kind)
        .map(|b| buf[b.start..b.end()].to_vec())
        .collect()
}

fn kinds(buf: &[u8]) -> Vec<[u8; 4]> {
    child_boxes(buf).unwrap().iter().map(|b| b.kind).collect()
}

/// Synthesize `buf` with `tags` only, serve it, and check the invariants every
/// keyed-metadata synthesis must hold: the served file re-parses strictly, its
/// audio is byte-identical, it carries no keyed metadata anywhere, and its tags
/// are exactly the store's. Returns the served file and its structure.
fn synthesize_single_system(buf: &[u8], tags: &[TagInput]) -> (Vec<u8>, Mp4Scan) {
    let scan = read_structure(buf).unwrap();
    let layout = synthesize_layout(&scan, tags, &[], &[]).unwrap();
    let served = serve_unstreamed(&layout, buf);
    let served_scan = read_structure(&served).expect("served file re-parses strictly");
    let old_audio = &buf[usize_from(scan.mdat_payload_offset)..];
    assert_eq!(
        &served[usize_from(served_scan.mdat_payload_offset)..],
        old_audio
    );
    assert!(keyed_items(&served).is_empty(), "keyed metadata survived");
    let want: Vec<(String, String)> = tags
        .iter()
        .map(|t| (t.key.clone(), t.value.clone()))
        .collect();
    assert_eq!(read_tags(&served), want);
    (served, served_scan)
}

#[test]
fn synthesize_drops_movie_level_keyed_meta_but_keeps_other_meta_handlers() {
    // After an `artist` edit the served file must not also carry the original
    // keyed `artist` (#771); a `meta` under another handler (here ID3-in-MP4's
    // `ID32`) is not a system the store models and passes through untouched.
    let keyed = keyed_meta(&[("com.apple.quicktime.artist", text("Old Keyed Artist"))]);
    let id32 = handler_meta(b"ID32", &bx(b"ID32", b"\x00\x00ID3-opaque"));
    let ffmpeg_udta = bx(
        b"udta",
        &ffmpeg_keyed_udta_meta(&[("artist", text("Old ffmpeg Artist"))]),
    );
    let trak = bx(b"trak", &soun_mdia(&[]));
    let buf = mp4_around(
        &[keyed, trak, id32.clone(), ffmpeg_udta].concat(),
        b"AUDIODATA",
        &[0],
    );

    let (served, s) = synthesize_single_system(&buf, &[TagInput::new("artist", "New Artist")]);
    assert!(!contains(&served, b"Old Keyed Artist"));
    assert!(!contains(&served, b"Old ffmpeg Artist"));
    let mp = &s.moov[8..];
    assert_eq!(kinds(mp), vec![*b"mvhd", *b"trak", *b"meta", *b"udta"]);
    assert_eq!(child_box_bytes(mp, b"meta"), vec![id32]);
    // The chunk offset follows the audio to its new position.
    assert_eq!(
        all_stco(&served),
        vec![vec![u32::try_from(s.mdat_payload_offset).unwrap()]]
    );
}

#[test]
fn synthesize_drops_a_bare_and_a_fullbox_keyed_meta_alike() {
    let bare = keyed_meta(&[("com.apple.quicktime.title", text("Old Bare"))]);
    let full = ffmpeg_keyed_udta_meta(&[("com.apple.quicktime.album", text("Old Full"))]);
    let trak = bx(b"trak", &soun_mdia(&[]));
    let buf = mp4_around(&[bare, full, trak].concat(), b"AUDIO", &[0]);
    let (served, s) = synthesize_single_system(&buf, &[TagInput::new("title", "T")]);
    assert!(!contains(&served, b"Old Bare") && !contains(&served, b"Old Full"));
    assert_eq!(kinds(&s.moov[8..]), vec![*b"mvhd", *b"trak", *b"udta"]);
}

#[test]
fn synthesize_strips_keyed_meta_from_track_and_media_and_resizes_both() {
    // Removing a nested box shrinks `mdia`, `trak` and `moov`. The stco value is
    // then relocated by the delta the shrunken moov implies: both have to agree
    // for the served file to parse strictly and point at the right audio.
    let trak_meta = keyed_meta(&[(
        "player.movie.audio.gain",
        data_atom(23, &0.5f32.to_be_bytes()),
    )]);
    let mdia_meta = keyed_meta(&[("com.apple.quicktime.comment", text("Old comment"))]);
    let trak_udta = bx(b"udta", &bx(b"tsrp", b"{\"transcript\":true}"));
    let mdia_other = handler_meta(b"mdir", &bx(b"ilst", b""));
    let mdia = soun_mdia(&[mdia_meta.clone(), mdia_other.clone()].concat());
    let trak = bx(
        b"trak",
        &[trak_meta.clone(), mdia.clone(), trak_udta.clone()].concat(),
    );
    let buf = mp4_around(&trak, b"AUDIODATA", &[0]);

    let (served, s) = synthesize_single_system(&buf, &[TagInput::new("title", "New")]);
    assert!(!contains(&served, b"Old comment"));
    assert!(!contains(&served, b"player.movie.audio.gain"));
    let mp = &s.moov[8..];
    let new_trak = child_boxes(mp).unwrap()[1];
    assert_eq!(&new_trak.kind, b"trak");
    assert_eq!(
        new_trak.total_len,
        trak.len() - trak_meta.len() - mdia_meta.len()
    );
    let trak_payload = new_trak.payload(mp);
    assert_eq!(kinds(trak_payload), vec![*b"mdia", *b"udta"]);
    assert_eq!(child_box_bytes(trak_payload, b"udta"), vec![trak_udta]);
    let new_mdia = child_boxes(trak_payload).unwrap()[0];
    assert_eq!(new_mdia.total_len, mdia.len() - mdia_meta.len());
    let mdia_payload = new_mdia.payload(trak_payload);
    assert_eq!(kinds(mdia_payload), vec![*b"hdlr", *b"minf", *b"meta"]);
    assert_eq!(child_box_bytes(mdia_payload, b"meta"), vec![mdia_other]);
    assert_eq!(
        all_stco(&served),
        vec![vec![u32::try_from(s.mdat_payload_offset).unwrap()]]
    );
}

#[test]
fn synthesize_strips_keyed_meta_under_a_largesize_trak_header() {
    // A 64-bit largesize header keeps its form, with the shrunken size.
    let meta = keyed_meta(&[("com.apple.quicktime.title", text("Old"))]);
    let body = [soun_mdia(&[]), meta.clone()].concat();
    let mut trak = 1u32.to_be_bytes().to_vec();
    trak.extend_from_slice(b"trak");
    trak.extend_from_slice(&(16 + body.len() as u64).to_be_bytes());
    trak.extend_from_slice(&body);
    let buf = mp4_around(&trak, b"AUDIODATA", &[0]);

    let (served, s) = synthesize_single_system(&buf, &[TagInput::new("title", "New")]);
    let mp = &s.moov[8..];
    let new_trak = child_boxes(mp).unwrap()[1];
    assert_eq!((new_trak.kind, new_trak.header_len), (*b"trak", 16));
    assert_eq!(new_trak.total_len, trak.len() - meta.len());
    assert_eq!(&mp[new_trak.start..new_trak.start + 4], &1u32.to_be_bytes());
    assert_eq!(
        all_stco(&served),
        vec![vec![u32::try_from(s.mdat_payload_offset).unwrap()]]
    );
}

#[test]
fn synthesize_strip_keeps_bytes_trailing_the_last_track_child() {
    // Up to 7 bytes after a `trak`'s last child are not a box; they are copied
    // through rather than lost when a keyed meta before them is removed.
    let meta = keyed_meta(&[("com.apple.quicktime.title", text("Old"))]);
    let trak = bx(
        b"trak",
        &[soun_mdia(&[]), meta.clone(), b"tail".to_vec()].concat(),
    );
    let buf = mp4_around(&trak, b"AUDIODATA", &[0]);
    let (_, s) = synthesize_single_system(&buf, &[TagInput::new("title", "New")]);
    let mp = &s.moov[8..];
    let new_trak = child_boxes(mp).unwrap()[1];
    assert_eq!(new_trak.total_len, trak.len() - meta.len());
    assert!(new_trak.payload(mp).ends_with(b"tail"));
}

#[test]
fn synthesize_does_not_strip_meta_nested_below_mdia() {
    // Keyed metadata is defined at the movie, track and media levels only; a
    // `meta` deeper down (here inside `minf`) is an unmodelled box, left as is.
    let meta = keyed_meta(&[("com.apple.quicktime.title", text("Deep"))]);
    let mut hdlr_p = vec![0u8; 8];
    hdlr_p.extend_from_slice(b"soun");
    hdlr_p.extend_from_slice(&[0u8; 12]);
    let mut stco = vec![0u8; 4];
    stco.extend_from_slice(&1u32.to_be_bytes());
    stco.extend_from_slice(&0u32.to_be_bytes());
    let minf = bx(
        b"minf",
        &[bx(b"stbl", &bx(b"stco", &stco)), meta.clone()].concat(),
    );
    let trak = bx(
        b"trak",
        &bx(b"mdia", &[bx(b"hdlr", &hdlr_p), minf].concat()),
    );
    let buf = mp4_around(&trak, b"AUDIODATA", &[0]);
    let scan = read_structure(&buf).unwrap();
    let layout = synthesize_layout(&scan, &[], &[], &[]).unwrap();
    let served = serve_unstreamed(&layout, &buf);
    let s = read_structure(&served).unwrap();
    assert_eq!(child_boxes(&s.moov[8..]).unwrap()[1].total_len, trak.len());
    assert!(contains(&served, &meta));
}

#[test]
fn synthesize_strips_keyed_meta_from_a_chaptered_m4b_keeping_chapters() {
    // #672 still holds with keyed metadata in both tracks: the chapter track's
    // chunk offset relocates with the audio's, and the Nero `chpl` survives.
    let audio_meta = keyed_meta(&[("com.apple.quicktime.title", text("Old Title"))]);
    let chapter_meta = keyed_meta(&[("com.apple.quicktime.comment", text("Old Chapter Meta"))]);
    let movie_meta = keyed_meta(&[("com.apple.quicktime.artist", text("Old Artist"))]);
    let soun = bx(b"trak", &[soun_mdia(&[]), audio_meta].concat());
    let text_trak = {
        let mut hdlr_p = vec![0u8; 8];
        hdlr_p.extend_from_slice(b"text");
        hdlr_p.extend_from_slice(&[0u8; 12]);
        let mut stco = vec![0u8; 4];
        stco.extend_from_slice(&1u32.to_be_bytes());
        stco.extend_from_slice(&0u32.to_be_bytes());
        let minf = bx(b"minf", &bx(b"stbl", &bx(b"stco", &stco)));
        let mdia = bx(b"mdia", &[bx(b"hdlr", &hdlr_p), minf].concat());
        bx(b"trak", &[mdia, chapter_meta].concat())
    };
    let chpl = chpl_box(&["One", "Two"]);
    let udta = bx(
        b"udta",
        &[
            itunes_meta(&bx(b"\xa9nam", &text("Old iTunes"))),
            chpl.clone(),
        ]
        .concat(),
    );
    let buf = mp4_around(
        &[movie_meta, soun, text_trak, udta].concat(),
        b"AUDIODATACHAPTERS",
        &[0, 9],
    );

    let (served, s) = synthesize_single_system(&buf, &[TagInput::new("title", "New")]);
    for old in [
        &b"Old Title"[..],
        b"Old Chapter Meta",
        b"Old Artist",
        b"Old iTunes",
    ] {
        assert!(!contains(&served, old));
    }
    let p = u32::try_from(s.mdat_payload_offset).unwrap();
    assert_eq!(all_stco(&served), vec![vec![p], vec![p + 9]]);
    let mp = &s.moov[8..];
    let udta = child_box_bytes(mp, b"udta").remove(0);
    assert!(udta.ends_with(&chpl));
}

#[test]
fn synthesize_strips_the_readable_prefix_of_a_garbled_second_mdia() {
    // `validate_moov` parses only a track's first `mdia`, so a second one can be
    // garbled inside. The reader still ingests the keyed `meta` in its readable
    // prefix; synthesis must drop that same box, copying the unreadable rest
    // through, rather than failing the file or serving the stale value.
    let keyed = keyed_meta(&[("com.apple.quicktime.title", text("Old"))]);
    let junk = [0, 0, 0, 99, b'j', b'u', b'n', b'k'];
    let garbled = bx(b"mdia", &[keyed.clone(), junk.to_vec()].concat());
    let trak = bx(b"trak", &[soun_mdia(&[]), garbled].concat());
    let buf = mp4_around(&trak, b"AUDIODATA", &[0]);
    assert_eq!(
        read_tags(&buf),
        vec![("title".to_string(), "Old".to_string())]
    );

    let (_, s) = synthesize_single_system(&buf, &[TagInput::new("title", "New")]);
    let mp = &s.moov[8..];
    let new_trak = child_boxes(mp).unwrap()[1];
    assert_eq!(new_trak.total_len, trak.len() - keyed.len());
    let trak_payload = new_trak.payload(mp);
    let second = child_boxes(trak_payload).unwrap()[1];
    assert_eq!(second.payload(trak_payload), junk);
}

#[test]
fn read_binary_tags_never_reads_keyed_items() {
    let meta = keyed_meta(&[
        ("com.example.blob", data_atom(0, &[1, 2, 3])),
        ("com.apple.quicktime.artwork", data_atom(13, b"img")),
    ]);
    let buf = mp4_with_keyed(&meta, &[], &[], b"AUDIO");
    let (tags, dropped) = read_binary_tags_reporting(&buf, 0);
    assert!(tags.is_empty());
    assert!(dropped.is_empty());
}
