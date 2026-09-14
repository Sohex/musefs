//! Which of an MP3's ID3v2 tags wins when it carries more than one (#767, #768).
//!
//! The tags are merged in file order. Each tag after the first either updates
//! the merge or replaces it, as the document for that tag's own version says:
//!
//! - ID3v2.3.0 §4.19
//!   (<https://mutagen-specs.readthedocs.io/en/latest/id3/id3v2.3.0.html>):
//!   "Every tag that is picked up after the initial/first tag is to be
//!   considered as an update of the previous one." v2.3 has no update flag; its
//!   extended header defines only a CRC flag (§3.2). The ID3v2.2 document
//!   (<https://mutagen-specs.readthedocs.io/en/latest/id3/id3v2.2.html>) says
//!   nothing about several tags, so a v2.2 tag follows the rule of v2.3, its
//!   successor.
//! - ID3v2.4.0 structure §5
//!   (<https://mutagen-specs.readthedocs.io/en/latest/id3/id3v2.4.0-structure.html>):
//!   "For every new tag that is found, the old tag should be discarded unless
//!   the update flag in the extended header (section 3.2) is set." §3.2 defines
//!   the flag: "If this flag is set, the present tag is an update of a tag found
//!   earlier in the present file or stream. If frames defined as unique are
//!   found in the present tag, they are to override any corresponding ones found
//!   in the earlier tag."
//!
//! Which frames are unique, and by what, each frames document sets out frame by
//! frame: §4 of the v2.3 document, and the v2.4 frames document
//! (<https://mutagen-specs.readthedocs.io/en/latest/id3/id3v2.4.0-frames.html>).
//!
//! "Earlier" is file order: §5's search finds appended tags scanning backwards,
//! but §3.2 speaks of a tag "earlier in the present file or stream", and in a
//! stream, the case both documents' rules were written for, tags arrive in file
//! order. The v2.4 document does not address several prepended tags back to
//! back; merging such a run in file order is musefs's reading.

use std::collections::HashSet;
use std::ops::Range;

use super::{Mp3Bounds, id3v2_alloc_safe, read_binary_tags, read_pictures, read_tags};
use crate::id3v2;
use crate::input::{EmbeddedBinaryTag, EmbeddedPicture};

/// What the scan ingests from an MP3's ID3v2 tags, merged across all of them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct Mp3Metadata {
    /// Text tags, including the `POPM` and MusicBrainz `UFID` promotions.
    pub tags: Vec<(String, String)>,
    pub pictures: Vec<EmbeddedPicture>,
    pub binary_tags: Vec<EmbeddedBinaryTag>,
}

/// Read every ID3v2 tag `bounds` located, and merge them in file order.
///
/// `front` holds the file from offset 0, and `tail` the last `tail.len()` bytes
/// of a file `file_len` long; one buffer may serve as both. Each tag is read out
/// of whichever holds it whole, sliced to exactly its own extent so the parser
/// cannot reach past its end. A tag neither holds whole is skipped.
///
/// - A v2.2 or v2.3 tag, or a v2.4 tag with the update flag, overrides only its
///   unique frames (see `Mp3Metadata::update_with`).
/// - A v2.4 tag without the update flag replaces everything merged before it.
/// - A tag the allocation guard will not parse contributes nothing and replaces
///   nothing: discarding tags that could be read for one that cannot would lose
///   metadata the file does carry.
///
/// Only the later tag's own version decides between the first two, so a v2.3 tag
/// updates a v2.4 tag before it, and an unflagged v2.4 tag replaces a v2.3 one.
/// The first tag readable starts the merge either way.
pub fn read_metadata(front: &[u8], tail: &[u8], file_len: u64, bounds: &Mp3Bounds) -> Mp3Metadata {
    let tail_start = file_len.saturating_sub(tail.len() as u64);
    let mut merged = Mp3Metadata::default();
    for extent in &bounds.id3v2_tags {
        let Some(tag) = within(front, 0, extent).or_else(|| within(tail, tail_start, extent))
        else {
            continue;
        };
        let Some(contents) = Mp3Metadata::read(tag) else {
            continue;
        };
        if updates_earlier(tag) {
            merged.update_with(contents, tag[3]);
        } else {
            merged = contents;
        }
    }
    merged
}

/// Does `tag` update the tags found before it, rather than discard them? A v2.2
/// or v2.3 tag always does (ID3v2.3.0 §4.19); a v2.4 tag only with the update
/// flag (ID3v2.4.0 structure §5). `tag` has passed the allocation guard, so its
/// major version is 2, 3 or 4.
fn updates_earlier(tag: &[u8]) -> bool {
    matches!(tag[3], 2 | 3) || id3v2::is_update(tag)
}

/// The file's bytes in `extent`, out of a buffer holding the file from `start`.
fn within<'a>(buf: &'a [u8], start: u64, extent: &Range<u64>) -> Option<&'a [u8]> {
    let from = usize::try_from(extent.start.checked_sub(start)?).ok()?;
    let to = usize::try_from(extent.end.checked_sub(start)?).ok()?;
    buf.get(from..to)
}

impl Mp3Metadata {
    /// One tag's contents, or `None` when the allocation guard refuses to let
    /// it be parsed.
    fn read(tag: &[u8]) -> Option<Self> {
        if !id3v2_alloc_safe(tag) {
            return None;
        }
        let (binary_tags, promoted) = read_binary_tags(tag);
        let mut tags = read_tags(tag);
        tags.extend(promoted);
        Some(Self {
            tags,
            pictures: read_pictures(tag),
            binary_tags,
        })
    }

    /// Fold in `later`, the contents of a tag of major version `version` that
    /// updates the tags before it. Each of its frames overrides the corresponding
    /// frames merged so far, where "corresponding" follows the frames documents,
    /// which v2.3 and v2.4 state alike for all but the binary frames
    /// [`same_unique_frame`] singles out, at the grain the store keeps:
    ///
    /// - text, `TXXX`, `COMM` and `USLT` frames by store key, compared
    ///   case-insensitively as the store compares keys: one text frame "of its
    ///   kind", one `TXXX` "with the same description", one `COMM`/`USLT` "with
    ///   the same language and content descriptor";
    /// - a `POPM` by both keys it promotes to, `rating` and `playcount`;
    /// - an `APIC` by description, "only one with the same content descriptor";
    /// - binary frames as [`same_unique_frame`] decides.
    ///
    /// Everything `later` does not override stays.
    fn update_with(&mut self, later: Self, version: u8) {
        let mut replaced: HashSet<String> = later
            .tags
            .iter()
            .map(|(k, _)| k.to_ascii_lowercase())
            .collect();
        if replaced.contains("rating") {
            replaced.insert("playcount".to_string());
        }
        self.tags
            .retain(|(k, _)| !replaced.contains(&k.to_ascii_lowercase()));
        self.tags.extend(later.tags);

        self.pictures.retain(|p| {
            !later
                .pictures
                .iter()
                .any(|q| q.description == p.description)
        });
        self.pictures.extend(later.pictures);

        self.binary_tags.retain(|b| {
            !later
                .binary_tags
                .iter()
                .any(|l| same_unique_frame(b, l, version))
        });
        self.binary_tags.extend(later.binary_tags);
    }
}

/// Does `later`, from an update tag of major version `version`, override
/// `earlier`? Both are binary frames, kept byte-exact; the uniqueness rules
/// quoted are the frames documents'. Only v2.3 and v2.4 tags have their binary
/// frames extracted.
fn same_unique_frame(earlier: &EmbeddedBinaryTag, later: &EmbeddedBinaryTag, version: u8) -> bool {
    if earlier.key != later.key {
        return false;
    }
    match earlier.key.as_str() {
        // "There may only be one ... frame in each tag." IPLS, RVAD and EQUA are
        // v2.3's alone: the v2.4 frames document does not define them.
        "MCDI" | "ETCO" | "MLLT" | "SYTC" | "RVRB" | "PCNT" | "RBUF" | "POSS" | "OWNE" | "SEEK"
        | "ASPI" | "IPLS" | "RVAD" | "EQUA" => true,
        // v2.3: "There may only be one "USER" frame in a tag." v2.4 allows one
        // "with the same 'Language'", a descriptor left to the catch-all below.
        "USER" if version == 3 => true,
        // "Only one with the same 'Owner identifier'" (UFID, AENC), "only one
        // with the same identification string" (RVA2, EQU2): a NUL-terminated
        // first field.
        "UFID" | "AENC" | "RVA2" | "EQU2" => {
            first_field(&earlier.payload) == first_field(&later.payload)
        }
        // "There may only be one URL link frame of its kind in an tag, except
        // when stated otherwise": WXXX is one per description, and WCOM and WOAR
        // may repeat, "but not with the same content".
        "WXXX" | "WCOM" | "WOAR" => earlier.payload == later.payload,
        id if id.starts_with('W') => true,
        // Unique by their whole contents (PRIV, LINK, COMR, SIGN), or by a
        // descriptor musefs does not decode (GEOB, SYLT, USER, ENCR, GRID): an
        // identical frame is the one duplicate recognised.
        _ => earlier.payload == later.payload,
    }
}

/// A frame body's first field, up to its NUL terminator.
fn first_field(body: &[u8]) -> &[u8] {
    body.split(|&b| b == 0).next().unwrap_or(body)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ss(n: usize) -> [u8; 4] {
        let n = u32::try_from(n).unwrap();
        [
            ((n >> 21) & 0x7F) as u8,
            ((n >> 14) & 0x7F) as u8,
            ((n >> 7) & 0x7F) as u8,
            (n & 0x7F) as u8,
        ]
    }

    /// A v2.4 tag of `frames`, with an extended header carrying the update flag
    /// (structure §3.2) when `update`.
    fn tag(frames: &[(&[u8; 4], Vec<u8>)], update: bool) -> Vec<u8> {
        let mut body = Vec::new();
        if update {
            body.extend_from_slice(&[0, 0, 0, 6, 0x01, 0x40]);
        }
        for (id, frame) in frames {
            body.extend_from_slice(*id);
            body.extend_from_slice(&ss(frame.len()));
            body.extend_from_slice(&[0, 0]);
            body.extend_from_slice(frame);
        }
        let mut out = vec![b'I', b'D', b'3', 4, 0, if update { 0x40 } else { 0 }];
        out.extend_from_slice(&ss(body.len()));
        out.extend(body);
        out
    }

    fn text(value: &str) -> Vec<u8> {
        let mut v = vec![3];
        v.extend_from_slice(value.as_bytes());
        v
    }

    fn txxx(description: &str, value: &str) -> Vec<u8> {
        let mut v = text(description);
        v.push(0);
        v.extend_from_slice(value.as_bytes());
        v
    }

    fn apic(picture_type: u8, description: &str, data: &[u8]) -> Vec<u8> {
        let mut v = b"\0image/png\0".to_vec();
        v.push(picture_type);
        v.extend_from_slice(description.as_bytes());
        v.push(0);
        v.extend_from_slice(data);
        v
    }

    /// A v2.3 tag of `frames`: plain 32-bit frame sizes, and no extended header.
    fn tag_v23(frames: &[(&[u8; 4], Vec<u8>)]) -> Vec<u8> {
        let mut body = Vec::new();
        for (id, frame) in frames {
            body.extend_from_slice(*id);
            body.extend_from_slice(&u32::try_from(frame.len()).unwrap().to_be_bytes());
            body.extend_from_slice(&[0, 0]);
            body.extend_from_slice(frame);
        }
        let mut out = vec![b'I', b'D', b'3', 3, 0, 0];
        out.extend_from_slice(&ss(body.len()));
        out.extend(body);
        out
    }

    /// A v2.2 tag of `frames`: three-character ids and 24-bit frame sizes.
    fn tag_v22(frames: &[(&[u8; 3], Vec<u8>)]) -> Vec<u8> {
        let mut body = Vec::new();
        for (id, frame) in frames {
            body.extend_from_slice(*id);
            body.extend_from_slice(&u32::try_from(frame.len()).unwrap().to_be_bytes()[1..]);
            body.extend_from_slice(frame);
        }
        let mut out = vec![b'I', b'D', b'3', 2, 0, 0];
        out.extend_from_slice(&ss(body.len()));
        out.extend(body);
        out
    }

    /// A text frame body in ISO-8859-1, the encoding every version defines.
    fn latin1(value: &str) -> Vec<u8> {
        let mut v = vec![0];
        v.extend_from_slice(value.as_bytes());
        v
    }

    fn txxx_latin1(description: &str, value: &str) -> Vec<u8> {
        let mut v = latin1(description);
        v.push(0);
        v.extend_from_slice(value.as_bytes());
        v
    }

    /// A `USER` (terms of use) frame body: encoding, language, text.
    fn user(lang: &[u8; 3], text: &str) -> Vec<u8> {
        let mut v = vec![0];
        v.extend_from_slice(lang);
        v.extend_from_slice(text.as_bytes());
        v
    }

    fn binary_of(m: &Mp3Metadata) -> Vec<(&str, &[u8])> {
        m.binary_tags
            .iter()
            .map(|b| (b.key.as_str(), b.payload.as_slice()))
            .collect()
    }

    /// Merge `tags`, laid end to end as a file's prepended run.
    fn merge(tags: &[Vec<u8>]) -> Mp3Metadata {
        let mut file = Vec::new();
        let mut extents = Vec::new();
        for t in tags {
            extents.push(file.len() as u64..(file.len() + t.len()) as u64);
            file.extend_from_slice(t);
        }
        let bounds = Mp3Bounds {
            audio_offset: file.len() as u64,
            audio_length: 0,
            id3v2_tags: extents,
        };
        read_metadata(&file, &[], file.len() as u64, &bounds)
    }

    fn sorted(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        let mut v: Vec<_> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect();
        v.sort();
        v
    }

    fn tags_of(m: &Mp3Metadata) -> Vec<(String, String)> {
        let mut v = m.tags.clone();
        v.sort();
        v
    }

    #[test]
    fn a_later_tag_replaces_everything_before_it() {
        let m = merge(&[
            tag(
                &[
                    (b"TIT2", text("One")),
                    (b"TPE1", text("Artist")),
                    (b"APIC", apic(3, "cover", b"one")),
                    (b"PRIV", b"owner\0one".to_vec()),
                ],
                false,
            ),
            tag(&[(b"TIT2", text("Two"))], false),
        ]);
        assert_eq!(tags_of(&m), sorted(&[("title", "Two")]));
        assert!(m.pictures.is_empty(), "{:?}", m.pictures);
        assert!(m.binary_tags.is_empty(), "{:?}", m.binary_tags);
    }

    #[test]
    fn an_update_overrides_only_the_text_keys_it_carries() {
        let m = merge(&[
            tag(
                &[
                    (b"TIT2", text("One")),
                    (b"TPE1", text("Artist")),
                    (b"TXXX", txxx("Flavour", "calm")),
                ],
                false,
            ),
            tag(
                &[(b"TIT2", text("Two")), (b"TXXX", txxx("FLAVOUR", "loud"))],
                true,
            ),
        ]);
        assert_eq!(
            tags_of(&m),
            sorted(&[("FLAVOUR", "loud"), ("artist", "Artist"), ("title", "Two")])
        );
    }

    #[test]
    fn an_update_popm_overrides_rating_and_playcount_together() {
        let m = merge(&[
            tag(&[(b"POPM", vec![0, 10, 0, 0, 0, 5])], false),
            tag(&[(b"POPM", vec![0, 20])], true),
        ]);
        assert_eq!(tags_of(&m), sorted(&[("rating", "20")]));
    }

    #[test]
    fn an_update_picture_overrides_the_picture_with_its_description() {
        let m = merge(&[
            tag(
                &[
                    (b"APIC", apic(3, "front", b"old front")),
                    (b"APIC", apic(4, "back", b"old back")),
                ],
                false,
            ),
            tag(&[(b"APIC", apic(3, "front", b"new front"))], true),
        ]);
        let got: Vec<(&str, &[u8])> = m
            .pictures
            .iter()
            .map(|p| (p.description.as_str(), p.data.as_slice()))
            .collect();
        assert_eq!(
            got,
            vec![("back", &b"old back"[..]), ("front", &b"new front"[..])]
        );
    }

    #[test]
    fn update_binary_frames_override_by_their_uniqueness_rule() {
        let m = merge(&[
            tag(
                &[
                    (b"MCDI", b"old toc".to_vec()),
                    (b"UFID", b"one.example\0old".to_vec()),
                    (b"UFID", b"two.example\0kept".to_vec()),
                    (b"WOAF", b"http://old".to_vec()),
                    (b"WCOM", b"http://shop-one".to_vec()),
                    (b"PRIV", b"owner\0same".to_vec()),
                    (b"GEOB", b"\0mime\0file\0desc\0old".to_vec()),
                ],
                false,
            ),
            tag(
                &[
                    (b"MCDI", b"new toc".to_vec()),
                    (b"UFID", b"one.example\0new".to_vec()),
                    (b"WOAF", b"http://new".to_vec()),
                    (b"WCOM", b"http://shop-two".to_vec()),
                    (b"PRIV", b"owner\0same".to_vec()),
                    (b"GEOB", b"\0mime\0file\0desc\0new".to_vec()),
                ],
                true,
            ),
        ]);
        let got: Vec<(&str, &[u8])> = m
            .binary_tags
            .iter()
            .map(|b| (b.key.as_str(), b.payload.as_slice()))
            .collect();
        assert_eq!(
            got,
            vec![
                // Kept from the earlier tag: nothing in the update corresponds.
                ("UFID", &b"two.example\0kept"[..]),
                ("WCOM", b"http://shop-one"),
                ("GEOB", b"\0mime\0file\0desc\0old"),
                // The update's own frames. Its PRIV is identical to the earlier
                // one, which it therefore replaces rather than repeats.
                ("MCDI", b"new toc"),
                ("UFID", b"one.example\0new"),
                ("WOAF", b"http://new"),
                ("WCOM", b"http://shop-two"),
                ("PRIV", b"owner\0same"),
                ("GEOB", b"\0mime\0file\0desc\0new"),
            ]
        );
    }

    #[test]
    fn a_later_v2_3_tag_updates_rather_than_replaces() {
        let m = merge(&[
            tag_v23(&[
                (b"TIT2", latin1("One")),
                (b"TPE1", latin1("Artist")),
                (b"TXXX", txxx_latin1("Flavour", "calm")),
                (b"APIC", apic(3, "front", b"old front")),
                (b"APIC", apic(4, "back", b"old back")),
                (b"PRIV", b"owner\0one".to_vec()),
            ]),
            tag_v23(&[
                (b"TIT2", latin1("Two")),
                (b"TXXX", txxx_latin1("FLAVOUR", "loud")),
                (b"APIC", apic(3, "front", b"new front")),
                (b"PRIV", b"owner\0two".to_vec()),
            ]),
        ]);
        assert_eq!(
            tags_of(&m),
            sorted(&[("FLAVOUR", "loud"), ("artist", "Artist"), ("title", "Two")])
        );
        let pictures: Vec<(&str, &[u8])> = m
            .pictures
            .iter()
            .map(|p| (p.description.as_str(), p.data.as_slice()))
            .collect();
        assert_eq!(
            pictures,
            vec![("back", &b"old back"[..]), ("front", &b"new front"[..])]
        );
        assert_eq!(
            binary_of(&m),
            vec![("PRIV", &b"owner\0one"[..]), ("PRIV", b"owner\0two")]
        );
    }

    #[test]
    fn a_later_v2_2_tag_updates_rather_than_replaces() {
        let m = merge(&[
            tag_v22(&[(b"TT2", latin1("One")), (b"TP1", latin1("Artist"))]),
            tag_v22(&[(b"TT2", latin1("Two"))]),
        ]);
        assert_eq!(
            tags_of(&m),
            sorted(&[("artist", "Artist"), ("title", "Two")])
        );
    }

    #[test]
    fn the_later_tags_own_version_decides_between_update_and_replace() {
        let older = || tag_v23(&[(b"TIT2", latin1("One")), (b"TPE1", latin1("Artist"))]);
        let newer = || tag(&[(b"TIT2", text("One")), (b"TPE1", text("Artist"))], false);
        // A v2.4 tag without the flag discards a v2.3 tag before it (v2.4 §5).
        let m = merge(&[older(), tag(&[(b"TIT2", text("Two"))], false)]);
        assert_eq!(tags_of(&m), sorted(&[("title", "Two")]));
        // A flagged one updates it.
        let m = merge(&[older(), tag(&[(b"TIT2", text("Two"))], true)]);
        assert_eq!(
            tags_of(&m),
            sorted(&[("artist", "Artist"), ("title", "Two")])
        );
        // A v2.3 tag updates a v2.4 tag before it (v2.3 §4.19), and so does v2.2.
        let m = merge(&[newer(), tag_v23(&[(b"TIT2", latin1("Two"))])]);
        assert_eq!(
            tags_of(&m),
            sorted(&[("artist", "Artist"), ("title", "Two")])
        );
        let m = merge(&[newer(), tag_v22(&[(b"TT2", latin1("Two"))])]);
        assert_eq!(
            tags_of(&m),
            sorted(&[("artist", "Artist"), ("title", "Two")])
        );
        // What matters is the tag's version, not the run's: a v2.4 tag without the
        // flag, after a v2.3 update, still replaces the lot.
        let m = merge(&[
            newer(),
            tag_v23(&[(b"TCON", latin1("Genre"))]),
            tag(&[(b"TIT2", text("Three"))], false),
        ]);
        assert_eq!(tags_of(&m), sorted(&[("title", "Three")]));
    }

    #[test]
    fn a_v2_3_update_overrides_the_frames_v2_3_allows_once_per_tag() {
        let m = merge(&[
            tag_v23(&[
                (b"IPLS", latin1("old people")),
                (b"RVAD", b"old volume".to_vec()),
                (b"EQUA", b"old curve".to_vec()),
                (b"USER", user(b"eng", "old terms")),
                (b"PRIV", b"owner\0kept".to_vec()),
            ]),
            tag_v23(&[
                (b"IPLS", latin1("new people")),
                (b"RVAD", b"new volume".to_vec()),
                (b"EQUA", b"new curve".to_vec()),
                (b"USER", user(b"fra", "new terms")),
            ]),
        ]);
        assert_eq!(
            binary_of(&m),
            vec![
                ("PRIV", &b"owner\0kept"[..]),
                ("IPLS", b"\0new people"),
                ("RVAD", b"new volume"),
                ("EQUA", b"new curve"),
                ("USER", b"\0franew terms"),
            ]
        );
    }

    #[test]
    fn a_v2_4_update_keeps_a_user_frame_in_another_language() {
        // v2.4 allows one USER per language, v2.3 one per tag; and the frames
        // only v2.3 defines keep v2.3's rule in whichever tag they turn up.
        let m = merge(&[
            tag(
                &[
                    (b"USER", user(b"eng", "english terms")),
                    (b"RVAD", b"old volume".to_vec()),
                ],
                false,
            ),
            tag(
                &[
                    (b"USER", user(b"fra", "french terms")),
                    (b"RVAD", b"new volume".to_vec()),
                ],
                true,
            ),
        ]);
        assert_eq!(
            binary_of(&m),
            vec![
                ("USER", &b"\0engenglish terms"[..]),
                ("USER", b"\0frafrench terms"),
                ("RVAD", b"new volume"),
            ]
        );
    }

    #[test]
    fn a_tag_that_cannot_be_read_replaces_nothing() {
        let mut unreadable = tag(&[(b"TIT2", text("Two"))], false);
        unreadable[5] = 0x80; // unsynchronisation: the guard refuses the tag
        let m = merge(&[tag(&[(b"TIT2", text("One"))], false), unreadable]);
        assert_eq!(tags_of(&m), sorted(&[("title", "One")]));
    }

    #[test]
    fn each_tag_is_read_from_whichever_buffer_holds_it_whole() {
        let front_tag = tag(&[(b"TIT2", text("Front"))], false);
        let back_tag = tag(&[(b"TPE1", text("Back"))], true);
        let mut file = front_tag.clone();
        file.extend_from_slice(&[0xFF, 0xFB]);
        let back_start = file.len();
        file.extend_from_slice(&back_tag);
        let len = file.len() as u64;
        let bounds = Mp3Bounds {
            audio_offset: front_tag.len() as u64,
            audio_length: 2,
            id3v2_tags: vec![0..front_tag.len() as u64, back_start as u64..len],
        };
        let front = &file[..front_tag.len()];
        let m = read_metadata(front, &file[back_start..], len, &bounds);
        assert_eq!(
            tags_of(&m),
            sorted(&[("artist", "Back"), ("title", "Front")])
        );
        // A tail one byte short of the appended tag holds it nowhere whole: it is
        // skipped rather than read from a partial slice.
        let m = read_metadata(front, &file[back_start + 1..], len, &bounds);
        assert_eq!(tags_of(&m), sorted(&[("title", "Front")]));
    }

    #[test]
    fn the_update_flag_is_extended_header_flag_b_of_a_v2_4_tag() {
        let update = tag(&[(b"TIT2", text("x"))], true);
        assert!(id3v2::is_update(&update));
        let not_update = |f: &dyn Fn(&mut Vec<u8>)| {
            let mut t = update.clone();
            f(&mut t);
            assert!(!id3v2::is_update(&t), "{t:02x?}");
        };
        not_update(&|t| t[15] = 0x00); // an extended header, flag b clear
        not_update(&|t| t[15] = 0x20); // only flag c, the CRC
        not_update(&|t| t[14] = 2); // not v2.4's single flag byte
        not_update(&|t| t[3] = 3); // v2.3 has no update flag
        not_update(&|t| t[5] = 0); // the header declares no extended header
        assert!(!id3v2::is_update(&tag(&[(b"TIT2", text("x"))], false)));
    }

    #[test]
    fn the_guard_reads_a_well_formed_v2_4_extended_header_and_nothing_else() {
        let update = tag(&[(b"PRIV", b"o\0data".to_vec())], true);
        assert!(id3v2_alloc_safe(&update));
        // The frames behind the extended header are the ones walked and ingested.
        let (opaque, _) = read_binary_tags(&update);
        let got: Vec<(&str, &[u8])> = opaque
            .iter()
            .map(|b| (b.key.as_str(), b.payload.as_slice()))
            .collect();
        assert_eq!(got, vec![("PRIV", &b"o\0data"[..])]);
        // An extended header filling the whole tag body is well-formed too.
        assert!(id3v2_alloc_safe(&tag(&[], true)));

        let refused = |f: &dyn Fn(&mut Vec<u8>)| {
            let mut t = update.clone();
            f(&mut t);
            assert!(!id3v2_alloc_safe(&t), "{t:02x?}");
        };
        refused(&|t| t[3] = 3); // v2.3 lays its extended header out differently
        refused(&|t| t[13] |= 0x80); // an extended-header size byte, high bit set
        refused(&|t| t[14] = 2); // not v2.4's single flag byte
        refused(&|t| t[13] = 5); // shorter than its own fixed part
        refused(&|t| t[13] = 0x7F); // longer than the tag body
        refused(&|t| t[20] = 0x7F); // the frame behind it claims more than the body
    }
}
