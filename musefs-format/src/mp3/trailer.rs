//! Where an MP3's audio ends: the tags that trail it (#768).
//!
//! ID3v2.4 lets a tag follow the audio, and it is "REQUIRED to add a footer to
//! an appended tag", a footer that exists "to speed up the process of locating
//! an ID3v2 tag when searching from the end of a file" (ID3v2.4.0 structure
//! §3.4, <https://id3.org/id3v2.4.0-structure>). §5 tells a reader to "look for
//! a tag footer, scanning from the back of the file", and puts an appended tag
//! "before tags from other tagging systems", such as ID3v1's 128-byte trailer.
//! This module is that backwards scan.

use crate::convert::usize_from;
use crate::error::{FormatError, Result};
use crate::id3v2;
use crate::probe::Extent;

/// Bytes in an ID3v1 trailer.
const ID3V1_LEN: u64 = 128;

/// One tag trailing an MP3's audio: its extent in the file, and whether it is an
/// ID3v2 tag, whose contents are ingested, or an ID3v1 trailer, which is not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Trailing {
    pub(super) start: u64,
    pub(super) end: u64,
    pub(super) id3v2: bool,
}

/// The tags at the end of an MP3, as [`locate_trailer`] found them, for
/// [`super::locate_audio_bounded`] to end the audio by.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mp3Trailer {
    /// Nearest the end of the file first.
    pub(super) tags: Vec<Trailing>,
}

/// Walk the tags trailing an MP3's audio, backwards from the end of the file:
/// appended ID3v2.4 tags, each found by a footer that agrees with its header
/// (see [`id3v2::appended_tag_start`]), and at most one ID3v1 trailer, either
/// after them, where §5 places it, or before them, where a writer appending at
/// end of file leaves it.
///
/// `tail` is the file's last `tail.len()` bytes, and `file_len` its size. The
/// walk does not know where the audio begins, so it reports every tag it can
/// see; [`super::locate_audio_bounded`] keeps only those after the frame sync.
/// `NeedMore { up_to }` counts back from the end of the file: retry with the
/// file's last `up_to` bytes, which is always more than `tail` holds. More than
/// [`id3v2::MAX_APPENDED_TAGS`] appended tags, or a `tail` longer than the file,
/// is `Malformed`.
pub fn locate_trailer(tail: &[u8], file_len: u64) -> Result<Extent<Mp3Trailer>> {
    let tail_start = file_len
        .checked_sub(tail.len() as u64)
        .ok_or(FormatError::Malformed)?;
    let mut tags = Vec::new();
    let mut end = file_len;
    let mut appended = 0;
    let mut id3v1_seen = false;
    loop {
        match id3v2::appended_tag_start(tail, tail_start, end) {
            Extent::NeedMore { up_to } => return Ok(Extent::NeedMore { up_to }),
            Extent::Complete(Some(start)) => {
                if appended == id3v2::MAX_APPENDED_TAGS {
                    return Err(FormatError::Malformed);
                }
                appended += 1;
                tags.push(Trailing {
                    start,
                    end,
                    id3v2: true,
                });
                end = start;
                continue;
            }
            Extent::Complete(None) => {}
        }
        // No footer ends here. One ID3v1 trailer may, with more appended tags in
        // front of it.
        let Some(start) = end.checked_sub(ID3V1_LEN).filter(|_| !id3v1_seen) else {
            break;
        };
        let Some(offset) = start.checked_sub(tail_start) else {
            return Ok(Extent::NeedMore {
                up_to: file_len - start,
            });
        };
        if !tail[usize_from(offset)..].starts_with(b"TAG") {
            break;
        }
        id3v1_seen = true;
        tags.push(Trailing {
            start,
            end,
            id3v2: false,
        });
        end = start;
    }
    Ok(Extent::Complete(Mp3Trailer { tags }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fuzz_check::fixtures::{id3v1_trailer, id3v24_text_tag, with_id3v24_footer};

    fn appended(title: &str) -> Vec<u8> {
        with_id3v24_footer(&id3v24_text_tag(&[("title", title)]))
    }

    fn whole(file: &[u8]) -> Result<Vec<Trailing>> {
        match locate_trailer(file, file.len() as u64)? {
            Extent::Complete(t) => Ok(t.tags),
            Extent::NeedMore { up_to } => panic!("a whole file asked for {up_to} bytes"),
        }
    }

    fn id3v2(start: usize, end: usize) -> Trailing {
        Trailing {
            start: start as u64,
            end: end as u64,
            id3v2: true,
        }
    }

    fn id3v1(start: usize) -> Trailing {
        Trailing {
            start: start as u64,
            end: (start + 128) as u64,
            id3v2: false,
        }
    }

    const AUDIO: &[u8] = &[0xFF, 0xFB, 0x90, 0x00];

    #[test]
    fn a_file_with_nothing_at_its_end_has_no_trailer() {
        assert_eq!(whole(AUDIO).unwrap(), vec![]);
        assert_eq!(whole(&[0x41; 300]).unwrap(), vec![]);
        assert_eq!(whole(&[]).unwrap(), vec![]);
    }

    #[test]
    fn tags_are_listed_nearest_the_end_first_with_their_extents() {
        let tag = appended("Back");
        let mut file = AUDIO.to_vec();
        file.extend(&tag);
        file.extend(appended("Last"));
        file.extend(id3v1_trailer());
        let a = AUDIO.len();
        let b = a + tag.len();
        let v1 = file.len() - 128;
        assert_eq!(
            whole(&file).unwrap(),
            vec![id3v1(v1), id3v2(b, v1), id3v2(a, b)]
        );
    }

    #[test]
    fn an_id3v1_trailer_may_precede_the_appended_tags() {
        let mut file = AUDIO.to_vec();
        file.extend(id3v1_trailer());
        let v1_end = file.len();
        file.extend(appended("Back"));
        assert_eq!(
            whole(&file).unwrap(),
            vec![id3v2(v1_end, file.len()), id3v1(AUDIO.len())]
        );
    }

    #[test]
    fn only_one_id3v1_trailer_is_recognised() {
        let mut file = AUDIO.to_vec();
        file.extend(id3v1_trailer());
        file.extend(id3v1_trailer());
        assert_eq!(whole(&file).unwrap(), vec![id3v1(AUDIO.len() + 128)]);
    }

    #[test]
    fn the_appended_run_is_capped() {
        let tag = with_id3v24_footer(&id3v24_text_tag(&[]));
        let mut at_cap = AUDIO.to_vec();
        for _ in 0..id3v2::MAX_APPENDED_TAGS {
            at_cap.extend(&tag);
        }
        assert_eq!(whole(&at_cap).unwrap().len(), id3v2::MAX_APPENDED_TAGS);
        at_cap.extend(&tag);
        assert_eq!(whole(&at_cap), Err(FormatError::Malformed));
    }

    #[test]
    fn a_tail_longer_than_the_file_is_malformed() {
        assert_eq!(locate_trailer(&[0; 11], 10), Err(FormatError::Malformed));
    }

    /// Each `NeedMore` names exactly how far back from the end the next thing
    /// the walk must see begins: the tag's header, then the footer in front of
    /// it, then where an ID3v1 trailer in front of that would start.
    #[test]
    fn a_short_tail_asks_for_exactly_the_bytes_the_walk_needs_next() {
        let mut body = id3v24_text_tag(&[("title", "Wide")]);
        body.extend(std::iter::repeat_n(0u8, 300));
        let n = u32::try_from(body.len() - 10).unwrap();
        body[6..10].copy_from_slice(&[
            ((n >> 21) & 0x7F) as u8,
            ((n >> 14) & 0x7F) as u8,
            ((n >> 7) & 0x7F) as u8,
            (n & 0x7F) as u8,
        ]);
        let tag = with_id3v24_footer(&body);
        let mut file = vec![0xFF; 1000];
        file.extend(&tag);
        let len = file.len() as u64;
        let from_end = |n: u64| &file[file.len() - usize_from(n)..];

        let mut asked = Vec::new();
        let mut have = 10;
        loop {
            match locate_trailer(from_end(have), len).unwrap() {
                Extent::NeedMore { up_to } => {
                    asked.push(up_to);
                    have = up_to;
                }
                Extent::Complete(t) => {
                    assert_eq!(t.tags, vec![id3v2(1000, file.len())]);
                    break;
                }
            }
        }
        let t = tag.len() as u64;
        assert_eq!(asked, vec![t, t + 10, t + 128]);
    }
}
