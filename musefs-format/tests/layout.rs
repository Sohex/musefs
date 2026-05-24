use musefs_format::{RegionLayout, Segment};

#[test]
fn lengths_sum_segments_and_exclude_audio_from_header() {
    let layout = RegionLayout::new(vec![
        Segment::Inline(vec![0u8; 10]),
        Segment::ArtImage {
            art_id: 7,
            len: 100,
        },
        Segment::Inline(vec![0u8; 5]),
        Segment::BackingAudio {
            offset: 200,
            len: 1000,
        },
    ]);

    assert_eq!(layout.header_len(), 10 + 100 + 5);
    assert_eq!(layout.total_len(), 10 + 100 + 5 + 1000);
}

#[test]
fn segment_len_reports_each_variant() {
    assert_eq!(Segment::Inline(vec![1, 2, 3]).len(), 3);
    assert_eq!(Segment::ArtImage { art_id: 1, len: 42 }.len(), 42);
    assert_eq!(Segment::BackingAudio { offset: 0, len: 9 }.len(), 9);
}
