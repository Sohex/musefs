use musefs_format::{LayoutError, RegionLayout, Segment};

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

#[test]
fn empty_single_segment_layout_fails_validation() {
    let layout = RegionLayout::new(vec![Segment::Inline(vec![])]);
    assert_eq!(layout.validate(), Err(LayoutError::EmptySegment));
}

#[test]
fn valid_layout_passes_validation() {
    let layout = RegionLayout::new(vec![
        Segment::Inline(b"header".to_vec()),
        Segment::BackingAudio {
            offset: 0,
            len: 100,
        },
    ]);
    assert!(layout.validate().is_ok());
}

#[test]
fn empty_backing_segment_passes_validation() {
    let layout = RegionLayout::new(vec![Segment::BackingAudio { offset: 0, len: 0 }]);
    assert!(layout.validate().is_ok());
}

#[test]
fn total_overflow_detected() {
    let layout = RegionLayout::new(vec![
        Segment::BackingAudio {
            offset: 0,
            len: u64::MAX,
        },
        Segment::BackingAudio { offset: 0, len: 1 },
    ]);
    assert_eq!(layout.validate(), Err(LayoutError::TotalOverflow));
}

#[test]
fn cached_totals_equal_segment_sum() {
    let layout = RegionLayout::validated(vec![
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
    ])
    .unwrap();
    assert_eq!(layout.total_len(), 10 + 100 + 5 + 1000);
    assert_eq!(layout.header_len(), 10 + 100 + 5);
    let sum: u64 = layout.segments().iter().map(Segment::len).sum();
    assert_eq!(layout.total_len(), sum);
}
