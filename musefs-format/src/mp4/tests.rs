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
fn build_udta_no_art_round_trips() {
    let tags = vec![
        TagInput::new("title", "Song"),
        TagInput::new("tracknumber", "5"),
    ];
    let (segs, streamed) = build_udta(&tags, &[], &[]).unwrap();
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
        data_len: BlobLen::new(100).unwrap(),
    };
    let (segs, streamed) = build_udta(&[TagInput::new("title", "T")], &[], &[art]).unwrap();
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
        data_len: BlobLen::new(u64::from(u32::MAX) + 1).unwrap(),
    };
    assert!(matches!(
        build_udta(&[TagInput::new("title", "T")], &[], &[art]),
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
    let (segs, streamed) = build_udta(&tags, &[], &[]).unwrap();
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
    let (segs, streamed) = build_udta(&[], &[], &[]).unwrap();
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

    let (key, value) = read_freeform(&inner).unwrap();
    assert_eq!(key, "musicbrainz_albumid"); // folded via vocabulary
    assert_eq!(value, "abc-123");
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

    let (key, value) = read_freeform(&inner).unwrap();
    assert_eq!(key, "My Custom Field"); // not in vocabulary -> verbatim name
    assert_eq!(value, "hello");
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

    assert!(read_freeform(&inner).is_none()); // binary-typed data is skipped
}

#[test]
fn build_udta_round_trips_freeform_and_vocabulary() {
    let tags = vec![
        TagInput::new("title", "Song"),
        TagInput::new("tracknumber", "3"),
        TagInput::new("MyRating", "5"), // user-defined -> ----
        TagInput::new("musicbrainz_albumid", "abc-123"), // vocabulary -> ----
    ];
    let (segs, _streamed) = build_udta(&tags, &[], &[]).unwrap();
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
    let (key, value) = read_freeform(&inner).unwrap();
    assert_eq!(key, ""); // empty name, not in vocabulary -> verbatim ""
    assert_eq!(value, "");
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
    assert!(read_freeform(&inner).is_none());
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
    let (key, value) = read_freeform(&inner).unwrap();
    assert_eq!(key, "MusicBrainz Album Id"); // empty mean -> not folded
    assert_eq!(value, "abc-123");
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
            data_len: BlobLen::new(10).unwrap(),
        };
        let (segs, _) = build_udta(&[TagInput::new("title", "T")], &[], &[art]).unwrap();
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
        data_len: BlobLen::new(10).unwrap(),
    };
    let (segs, _) = build_udta(&[TagInput::new("title", "T")], &[], &[art]).unwrap();
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
        data_len: BlobLen::new(len).unwrap(),
    };
    let arts = [art(1, "image/jpeg", 10), art(2, "image/png", 20)];
    let (segs, streamed) = build_udta(&[TagInput::new("title", "T")], &[], &arts).unwrap();
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
        data_len: BlobLen::new(len).unwrap(),
    };
    let arts = [art(1, "image/jpeg", 5), art(2, "image/png", 9)];
    let (segs, _) = build_udta(&[TagInput::new("title", "Song")], &[], &arts).unwrap();
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
            data_len: BlobLen::new(data_len).unwrap(),
        }
    }
    // Derive the fixed overhead from the udta size field (segs[0] inline), with
    // data_len 1 (BlobLen is non-zero), without materializing any image bytes.
    let (segs0, _) = build_udta(&[TagInput::new("title", "T")], &[], &[art(1)]).unwrap();
    let Segment::Inline(h0) = &segs0[0] else {
        panic!("inline head")
    };
    let overhead = u64::from(u32::from_be_bytes(h0[0..4].try_into().unwrap())) - 1;
    let max_len = u64::from(u32::MAX) - overhead;

    let (segs_max, streamed) =
        build_udta(&[TagInput::new("title", "T")], &[], &[art(max_len)]).unwrap();
    assert_eq!(streamed, max_len);
    let Segment::Inline(h_max) = &segs_max[0] else {
        panic!("inline head")
    };
    assert_eq!(
        u32::from_be_bytes(h_max[0..4].try_into().unwrap()),
        u32::MAX
    );

    assert!(matches!(
        build_udta(&[TagInput::new("title", "T")], &[], &[art(max_len + 1)]),
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
        data_len: BlobLen::new(data_len).unwrap(),
    };
    assert_eq!(
        build_udta(&[], &[], &[mk(u64::MAX)]).err(),
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
        build_udta(&[], &bins, &[]).err(),
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
