//! Independent validation: synthesize an m4a layout, materialize it, and parse the
//! result with the `mp4` crate (no shared code with our parser). Key assertion:
//! samples read through OUR patched chunk offsets are byte-identical to the
//! originals — proving the offset surgery.

use musefs_format::{mp4, RegionLayout, Segment, TagInput};
use std::io::Cursor;

fn materialize(layout: &RegionLayout, backing: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    for seg in layout.segments() {
        match seg {
            Segment::Inline(b) => out.extend_from_slice(b),
            Segment::BackingAudio { offset, len } => {
                out.extend_from_slice(&backing[*offset as usize..(*offset + *len) as usize]);
            }
            Segment::ArtImage { .. } => unreachable!("no art in this fixture"),
        }
    }
    out
}

fn samples(bytes: &[u8]) -> Vec<Vec<u8>> {
    // `::mp4` is the external oracle crate; `mp4` (no leading `::`) is our own module.
    let mut r = ::mp4::Mp4Reader::read_header(Cursor::new(bytes), bytes.len() as u64).unwrap();
    let track_id = *r.tracks().keys().next().unwrap();
    let count = r.sample_count(track_id).unwrap();
    (1..=count)
        .filter_map(|i| r.read_sample(track_id, i).unwrap())
        .map(|s| s.bytes.to_vec())
        .collect()
}

#[test]
fn synthesized_m4a_decodes_via_independent_parser() {
    let original = std::fs::read("tests/fixtures/sample.m4a").unwrap();
    let scan = mp4::read_structure(&original).unwrap();
    let layout = mp4::synthesize_layout(
        &scan,
        &[TagInput::new("title", "Rewritten"), TagInput::new("artist", "AA")],
        &[],
    )
    .unwrap();
    let synth = materialize(&layout, &original);

    // Independent parser reads samples through OUR patched offsets; must match.
    assert_eq!(samples(&synth), samples(&original), "patched chunk offsets are wrong");

    // Our own reader sees the rewritten tags in the synthesized output.
    let tags = mp4::read_tags(&synth);
    assert!(tags.contains(&("title".into(), "Rewritten".into())));
    assert!(tags.contains(&("artist".into(), "AA".into())));
}
