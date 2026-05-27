//! Pure assertions and minimal-file fixtures shared by proptest, the fuzz
//! crate, and musefs-core tests. Gated behind `cfg(test)` or the `fuzzing`
//! feature so it never ships in release builds.

use crate::layout::{RegionLayout, Segment};

/// Property A — the synthesized layout serves the backing audio range
/// `[audio_offset, audio_offset + audio_length)` exactly once, contiguously,
/// with no metadata segment after audio, and the served length is
/// `header_len + audio_length`. Holds for every format and any tags/art.
pub fn assert_backing_covers_audio(audio_offset: u64, audio_length: u64, layout: &RegionLayout) {
    let mut expected = audio_offset;
    let mut covered = 0u64;
    let mut seen_backing = false;
    for seg in layout.segments() {
        match seg {
            Segment::BackingAudio { offset, len } | Segment::OggAudio { offset, len, .. } => {
                assert_eq!(
                    *offset, expected,
                    "backing segment not contiguous at {expected}"
                );
                expected += *len;
                covered += *len;
                seen_backing = true;
            }
            _ => assert!(!seen_backing, "metadata segment after backing audio"),
        }
    }
    assert!(seen_backing, "no backing audio segment present");
    assert_eq!(
        covered, audio_length,
        "backing coverage {covered} != audio length {audio_length}"
    );
    assert_eq!(
        layout.total_len(),
        layout.header_len() + audio_length,
        "total_len != header_len + audio_length",
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::{RegionLayout, Segment};

    #[test]
    fn accepts_a_faithful_layout() {
        // header (inline) + a single backing run [100, 100+50).
        let layout = RegionLayout::new(vec![
            Segment::Inline(vec![0u8; 12]),
            Segment::BackingAudio {
                offset: 100,
                len: 50,
            },
        ]);
        assert_backing_covers_audio(100, 50, &layout);
    }

    #[test]
    fn accepts_contiguous_ogg_runs() {
        let layout = RegionLayout::new(vec![
            Segment::Inline(vec![0u8; 4]),
            Segment::OggAudio {
                offset: 200,
                len: 30,
                seq_delta: 1,
            },
            Segment::OggAudio {
                offset: 230,
                len: 70,
                seq_delta: 1,
            },
        ]);
        assert_backing_covers_audio(200, 100, &layout);
    }

    #[test]
    #[should_panic(expected = "backing coverage")]
    fn rejects_dropped_backing_bytes() {
        // Planted bug: layout only covers 40 of the 50 audio bytes.
        let layout = RegionLayout::new(vec![
            Segment::Inline(vec![0u8; 12]),
            Segment::BackingAudio {
                offset: 100,
                len: 40,
            },
        ]);
        assert_backing_covers_audio(100, 50, &layout);
    }

    #[test]
    #[should_panic(expected = "contiguous")]
    fn rejects_shifted_backing_offset() {
        let layout = RegionLayout::new(vec![Segment::BackingAudio {
            offset: 101,
            len: 50,
        }]);
        assert_backing_covers_audio(100, 50, &layout);
    }
}
