//! Hand-rolled MP4/M4A box layer: parse the structure, read iTunes metadata, and
//! regenerate `moov` (with patched chunk offsets) to synthesize a re-tagged file
//! whose `mdat` audio payload is served verbatim. Strict: anything outside the
//! supported shape (single audio track, one `mdat`, non-fragmented) is rejected.

use crate::error::{FormatError, Result};
use crate::input::{ArtInput, EmbeddedPicture, TagInput};
use crate::layout::{RegionLayout, Segment};

fn be_u32(b: &[u8], pos: usize) -> Result<u32> {
    let s = b.get(pos..pos + 4).ok_or(FormatError::Malformed)?;
    Ok(u32::from_be_bytes(s.try_into().unwrap()))
}

fn be_u64(b: &[u8], pos: usize) -> Result<u64> {
    let s = b.get(pos..pos + 8).ok_or(FormatError::Malformed)?;
    Ok(u64::from_be_bytes(s.try_into().unwrap()))
}

/// A located box header within some buffer. `start` is relative to that buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct BoxRef {
    kind: [u8; 4],
    start: usize,
    header_len: usize, // 8, or 16 for 64-bit largesize
    total_len: usize,  // header + payload
}

impl BoxRef {
    fn payload_start(&self) -> usize {
        self.start + self.header_len
    }
    fn end(&self) -> usize {
        self.start + self.total_len
    }
    /// `buf` must be the same buffer `read_box` parsed this header from — offsets
    /// are relative to it. The debug assertion catches a wrong-buffer call in tests.
    fn payload<'a>(&self, buf: &'a [u8]) -> &'a [u8] {
        debug_assert!(
            self.end() <= buf.len(),
            "BoxRef::payload called with a buffer it was not parsed from"
        );
        &buf[self.payload_start()..self.end()]
    }
}

fn read_box(buf: &[u8], pos: usize) -> Result<BoxRef> {
    let size32 = be_u32(buf, pos)? as u64;
    let kind: [u8; 4] = buf
        .get(pos + 4..pos + 8)
        .ok_or(FormatError::Malformed)?
        .try_into()
        .unwrap();
    let (header_len, total) = match size32 {
        1 => (16usize, be_u64(buf, pos + 8)?),
        0 => (8usize, (buf.len() - pos) as u64),
        n => (8usize, n),
    };
    let total = total as usize;
    if total < header_len || pos + total > buf.len() {
        return Err(FormatError::Malformed);
    }
    Ok(BoxRef {
        kind,
        start: pos,
        header_len,
        total_len: total,
    })
}

fn child_boxes(buf: &[u8]) -> Result<Vec<BoxRef>> {
    let mut out = Vec::new();
    let mut pos = 0;
    while pos + 8 <= buf.len() {
        let b = read_box(buf, pos)?;
        pos = b.end();
        out.push(b);
    }
    Ok(out)
}

fn find_box(buf: &[u8], kind: &[u8; 4]) -> Result<Option<BoxRef>> {
    Ok(child_boxes(buf)?.into_iter().find(|b| &b.kind == kind))
}

/// Descend a path of box types; return `(payload_start, payload_len)` relative to
/// `buf` for the box at the end of the path, or None if any step is missing.
fn find_path(buf: &[u8], path: &[&[u8; 4]]) -> Result<Option<(usize, usize)>> {
    let mut base = 0usize;
    let mut last = None;
    for kind in path {
        let region = &buf[base..];
        let b = match find_box(region, kind)? {
            Some(b) => b,
            None => return Ok(None),
        };
        let ps = base + b.payload_start();
        last = Some((ps, b.total_len - b.header_len));
        base = ps;
    }
    Ok(last)
}

/// Audio payload bounds within the backing file (the verbatim `mdat` payload).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Mp4Bounds {
    pub audio_offset: u64,
    pub audio_length: u64,
}

/// Validate the supported shape; return the ftyp/moov/mdat boxes (absolute offsets
/// in `buf`). Rejects fragmented, video, multi-track, and multi-`mdat` files.
fn locate(buf: &[u8]) -> Result<(BoxRef, BoxRef, BoxRef)> {
    let top = child_boxes(buf).map_err(|_| FormatError::NotMp4)?;
    if top.iter().any(|b| &b.kind == b"moof") {
        return Err(FormatError::NotMp4);
    }
    let one = |kind: &[u8; 4]| -> Result<BoxRef> {
        let mut it = top.iter().filter(|b| &b.kind == kind);
        let first = it.next().copied().ok_or(FormatError::NotMp4)?;
        if it.next().is_some() {
            return Err(FormatError::NotMp4);
        }
        Ok(first)
    };
    let ftyp = one(b"ftyp")?;
    let moov = one(b"moov")?;
    let mdat = one(b"mdat")?;

    let moov_payload = moov.payload(buf);
    if find_box(moov_payload, b"mvex")?.is_some() {
        return Err(FormatError::NotMp4);
    }
    let traks: Vec<_> = child_boxes(moov_payload)?
        .into_iter()
        .filter(|b| &b.kind == b"trak")
        .collect();
    if traks.len() != 1 {
        return Err(FormatError::NotMp4);
    }
    let trak = traks[0].payload(moov_payload);
    let (hp, hl) = find_path(trak, &[b"mdia", b"hdlr"])?.ok_or(FormatError::NotMp4)?;
    if trak[hp..hp + hl].get(8..12) != Some(b"soun") {
        return Err(FormatError::NotMp4);
    }
    Ok((ftyp, moov, mdat))
}

/// Parse the file and return the `mdat` payload bounds, or an error to skip it.
pub fn locate_audio(buf: &[u8]) -> Result<Mp4Bounds> {
    let (_ftyp, _moov, mdat) = locate(buf)?;
    Ok(Mp4Bounds {
        audio_offset: mdat.payload_start() as u64,
        audio_length: (mdat.total_len - mdat.header_len) as u64,
    })
}

/// Everything `synthesize_layout` needs, read from the backing file once.
#[derive(Debug, Clone)]
pub struct Mp4Scan {
    pub ftyp: Vec<u8>,
    pub moov: Vec<u8>,
    pub mdat_header: Vec<u8>,
    pub mdat_payload_offset: u64,
    pub mdat_payload_len: u64,
}

pub fn read_structure(buf: &[u8]) -> Result<Mp4Scan> {
    let (ftyp, moov, mdat) = locate(buf)?;
    Ok(Mp4Scan {
        ftyp: buf[ftyp.start..ftyp.end()].to_vec(),
        moov: buf[moov.start..moov.end()].to_vec(),
        mdat_header: buf[mdat.start..mdat.payload_start()].to_vec(),
        mdat_payload_offset: mdat.payload_start() as u64,
        mdat_payload_len: (mdat.total_len - mdat.header_len) as u64,
    })
}

fn atom_to_key(kind: &[u8; 4]) -> Option<&'static str> {
    Some(match kind {
        b"\xa9nam" => "title",
        b"\xa9ART" => "artist",
        b"aART" => "albumartist",
        b"\xa9alb" => "album",
        b"\xa9gen" => "genre",
        b"\xa9day" => "date",
        b"\xa9wrt" => "composer",
        _ => return None,
    })
}

/// Locate `moov/udta/meta/ilst`; `meta` is a FullBox (4 version/flags bytes before
/// its children). Returns the ilst payload range absolute within `buf`.
fn ilst_region(buf: &[u8]) -> Option<(usize, usize)> {
    let moov = find_box(buf, b"moov").ok()??;
    let mp = moov.payload(buf);
    let base = moov.payload_start();
    let (up, ul) = find_path(mp, &[b"udta"]).ok()??;
    let udta = &mp[up..up + ul];
    let meta = find_box(udta, b"meta").ok()??;
    let meta_children = udta.get(meta.payload_start() + 4..meta.end())?;
    let il = find_box(meta_children, b"ilst").ok()??;
    let start = base + up + meta.payload_start() + 4 + il.payload_start();
    Some((start, il.total_len - il.header_len))
}

pub fn read_tags(buf: &[u8]) -> Vec<(String, String)> {
    let (start, len) = match ilst_region(buf) {
        Some(r) => r,
        None => return Vec::new(),
    };
    let ilst = &buf[start..start + len];
    let mut out = Vec::new();
    for atom in child_boxes(ilst).unwrap_or_default() {
        let inner = atom.payload(ilst);
        let data = match find_box(inner, b"data") {
            Ok(Some(d)) => d,
            _ => continue,
        };
        let dp = data.payload(inner);
        if dp.len() < 8 {
            continue;
        }
        let value = &dp[8..]; // skip [type 4][locale 4]
        if let Some(key) = atom_to_key(&atom.kind) {
            if let Ok(s) = std::str::from_utf8(value) {
                out.push((key.to_string(), s.to_string()));
            }
        } else if &atom.kind == b"trkn" && value.len() >= 4 {
            out.push((
                "tracknumber".into(),
                u16::from_be_bytes([value[2], value[3]]).to_string(),
            ));
        } else if &atom.kind == b"disk" && value.len() >= 4 {
            out.push((
                "discnumber".into(),
                u16::from_be_bytes([value[2], value[3]]).to_string(),
            ));
        }
    }
    out
}

pub fn read_pictures(buf: &[u8]) -> Vec<EmbeddedPicture> {
    let (start, len) = match ilst_region(buf) {
        Some(r) => r,
        None => return Vec::new(),
    };
    let ilst = &buf[start..start + len];
    let mut out = Vec::new();
    for atom in child_boxes(ilst).unwrap_or_default() {
        if &atom.kind != b"covr" {
            continue;
        }
        let inner = atom.payload(ilst);
        let data = match find_box(inner, b"data") {
            Ok(Some(d)) => d,
            _ => continue,
        };
        let dp = data.payload(inner);
        if dp.len() < 8 {
            continue;
        }
        let mime = match u32::from_be_bytes([dp[0], dp[1], dp[2], dp[3]]) {
            13 => "image/jpeg",
            14 => "image/png",
            _ => continue,
        };
        out.push(EmbeddedPicture {
            mime: mime.to_string(),
            picture_type: 3,
            description: String::new(),
            width: 0,
            height: 0,
            data: dp[8..].to_vec(),
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a 32-bit-size box: [size][type][payload].
    fn bx(kind: &[u8; 4], payload: &[u8]) -> Vec<u8> {
        let mut v = ((8 + payload.len()) as u32).to_be_bytes().to_vec();
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
        stco.extend_from_slice(&(stco_entries.len() as u32).to_be_bytes());
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
            assert_eq!(&buf[b.audio_offset as usize..][..9], b"AUDIODATA");
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
        assert_eq!(&buf[s.mdat_payload_offset as usize..][..9], b"AUDIODATA");
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
        let trak = bx(b"trak", &bx(b"mdia", &[bx(b"hdlr", &hdlr_p), minf].concat()));
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
        let pics = read_pictures(&buf);
        assert_eq!(pics.len(), 1);
        assert_eq!(pics[0].mime, "image/jpeg");
        assert_eq!(pics[0].data, jpeg);
    }
}
