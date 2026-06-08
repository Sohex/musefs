//! Pure assertions and minimal-file fixtures shared by proptest, the fuzz
//! crate, and musefs-core tests. Gated behind `cfg(test)` or the `fuzzing`
//! feature so it never ships in release builds.

use crate::layout::{RegionLayout, Segment};

/// Property A — the synthesized layout serves the backing audio range
/// `[audio_offset, audio_offset + audio_length)` exactly once, contiguously,
/// with no backing-audio run split by a non-audio segment, and the served
/// length is `header_len + audio_length`. Non-audio segments (e.g. a WAV
/// RIFF word-align pad) may precede or follow the contiguous backing run;
/// what is forbidden is a non-audio segment that interrupts the run (which
/// would corrupt the served audio). Holds for every format and any tags/art.
pub fn assert_backing_covers_audio(audio_offset: u64, audio_length: u64, layout: &RegionLayout) {
    let mut expected = audio_offset;
    let mut covered = 0u64;
    let mut seen_backing = false;
    let mut backing_ended = false;
    for seg in layout.segments() {
        match seg {
            Segment::BackingAudio { offset, len } | Segment::OggAudio { offset, len, .. } => {
                assert!(
                    !backing_ended,
                    "backing audio run is split by a non-backing segment"
                );
                assert_eq!(
                    *offset, expected,
                    "backing segment not contiguous at {expected}"
                );
                expected += *len;
                covered += *len;
                seen_backing = true;
            }
            _ => {
                if seen_backing {
                    backing_ended = true;
                }
            }
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

/// Minimal valid files per format, for proptest/fuzz seeds/interop. FLAC and
/// M4A are ported from `musefs-core/tests/common/mod.rs`; WAV is hand-built
/// (RIFF/WAVE with a `fmt ` + `data` chunk); MP3 is a bare MPEG frame sync;
/// Ogg is lifted from `musefs-format/src/ogg/mod.rs`'s `opus_headers` helper.
pub mod fixtures {
    /// Build a FLAC metadata block (4-byte header + body) independently of production code.
    pub fn flac_block(block_type: u8, body: &[u8], is_last: bool) -> Vec<u8> {
        let mut out = Vec::new();
        let first = (if is_last { 0x80 } else { 0 }) | (block_type & 0x7F);
        out.push(first);
        let len = body.len();
        out.push(u8::try_from((len >> 16) & 0xFF).expect("masked to 0xFF"));
        out.push(u8::try_from((len >> 8) & 0xFF).expect("masked to 0xFF"));
        out.push(u8::try_from(len & 0xFF).expect("masked to 0xFF"));
        out.extend_from_slice(body);
        out
    }

    /// A structurally valid STREAMINFO body: 44100 Hz, 2 channels, 16-bit, unknown frame/sample counts.
    pub fn streaminfo_body() -> Vec<u8> {
        let mut b = vec![
            0x10, 0x00, // min block size = 4096
            0x10, 0x00, // max block size = 4096
            0x00, 0x00, 0x00, // min frame size = 0 (unknown)
            0x00, 0x00, 0x00, // max frame size = 0 (unknown)
            0x0A, 0xC4, 0x42,
            0xF0, // sample_rate=44100, channels=2, bps=16, top of total samples
            0x00, 0x00, 0x00, 0x00, // remaining total-samples bits = 0
        ];
        b.extend_from_slice(&[0u8; 16]); // MD5 signature = 0
        b
    }

    /// Minimal VORBIS_COMMENT body with the given already-formatted `KEY=value` comments.
    pub fn vorbis_comment_body(vendor: &str, comments: &[&str]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&u32::try_from(vendor.len()).unwrap().to_le_bytes());
        out.extend_from_slice(vendor.as_bytes());
        out.extend_from_slice(&u32::try_from(comments.len()).unwrap().to_le_bytes());
        for c in comments {
            out.extend_from_slice(&u32::try_from(c.len()).unwrap().to_le_bytes());
            out.extend_from_slice(c.as_bytes());
        }
        out
    }

    /// Assemble a full FLAC byte stream: marker + blocks (last-flag auto-set on the final block) + audio.
    pub fn make_flac(blocks: &[(u8, Vec<u8>)], audio: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(b"fLaC");
        for (i, (bt, body)) in blocks.iter().enumerate() {
            let is_last = i == blocks.len() - 1;
            out.extend_from_slice(&flac_block(*bt, body, is_last));
        }
        out.extend_from_slice(audio);
        out
    }

    /// FLAC = `fLaC` + STREAMINFO + VORBIS_COMMENT + `audio`.
    pub fn flac(audio: &[u8]) -> Vec<u8> {
        make_flac(
            &[
                (0, streaminfo_body()),
                (4, vorbis_comment_body("orig", &["TITLE=Orig"])),
            ],
            audio,
        )
    }

    fn bx(kind: &[u8; 4], payload: &[u8]) -> Vec<u8> {
        let mut v = u32::try_from(8 + payload.len())
            .unwrap()
            .to_be_bytes()
            .to_vec();
        v.extend_from_slice(kind);
        v.extend_from_slice(payload);
        v
    }
    fn m4a_data_atom(type_code: u32, value: &[u8]) -> Vec<u8> {
        let mut p = type_code.to_be_bytes().to_vec();
        p.extend_from_slice(&0u32.to_be_bytes());
        p.extend_from_slice(value);
        bx(b"data", &p)
    }

    /// Minimal moov-first M4A (ported verbatim from tests/common::minimal_m4a).
    pub fn m4a(mdat_payload: &[u8]) -> Vec<u8> {
        m4a_with_extra_ilst(&[], mdat_payload)
    }

    /// `m4a` plus a `covr` atom holding two `data` children (jpeg + png) — the
    /// iTunes multiple-artwork convention. Seeds the fuzzers' multi-art read
    /// path; `m4a`'s byte output is unchanged.
    pub fn m4a_two_covers(mdat_payload: &[u8]) -> Vec<u8> {
        let covr = bx(
            b"covr",
            &[
                m4a_data_atom(13, &[0xFF, 0xD8, 0xFF, 0xE0, 1, 2, 3]),
                m4a_data_atom(14, &[0x89, b'P', b'N', b'G', 4, 5]),
            ]
            .concat(),
        );
        m4a_with_extra_ilst(&covr, mdat_payload)
    }

    fn m4a_with_extra_ilst(extra_ilst_atoms: &[u8], mdat_payload: &[u8]) -> Vec<u8> {
        let ilst_atoms = [
            bx(b"\xa9nam", &m4a_data_atom(1, b"Orig M4A")),
            bx(b"\xa9ART", &m4a_data_atom(1, b"Orig Artist")),
            extra_ilst_atoms.to_vec(),
        ]
        .concat();
        let ilst = bx(b"ilst", &ilst_atoms);
        let mut meta_hdlr = vec![0u8; 8];
        meta_hdlr.extend_from_slice(b"mdir");
        meta_hdlr.extend_from_slice(b"appl");
        meta_hdlr.extend_from_slice(&[0u8; 9]);
        let mut meta = vec![0u8; 4];
        meta.extend(bx(b"hdlr", &meta_hdlr));
        meta.extend(ilst);
        let udta = bx(b"udta", &bx(b"meta", &meta));
        let mut soun_hdlr = vec![0u8; 8];
        soun_hdlr.extend_from_slice(b"soun");
        soun_hdlr.extend_from_slice(&[0u8; 12]);
        let mut stco = vec![0u8; 4];
        stco.extend_from_slice(&1u32.to_be_bytes());
        stco.extend_from_slice(&0u32.to_be_bytes());
        let minf = bx(b"minf", &bx(b"stbl", &bx(b"stco", &stco)));
        let trak = bx(
            b"trak",
            &bx(b"mdia", &[bx(b"hdlr", &soun_hdlr), minf].concat()),
        );
        let moov = bx(b"moov", &[bx(b"mvhd", &[0u8; 8]), trak, udta].concat());
        let mut out = [bx(b"ftyp", b"M4A isom"), moov, bx(b"mdat", mdat_payload)].concat();
        // Point the single `stco` chunk offset at the real `mdat` payload start. A real
        // M4A's chunk offsets are absolute file positions; leaving it 0 means a retag
        // that shrinks the `moov` patches the offset below zero and synthesis fails
        // (TooLarge). With the true offset, the patched value lands at the new payload
        // position. The first `stco` occurrence is the box type (it precedes `mdat`).
        let mdat_payload_offset = u32::try_from(out.len() - mdat_payload.len()).unwrap();
        let stco = out
            .windows(4)
            .position(|w| w == b"stco")
            .expect("stco present");
        let entry = stco + 4 + 4 + 4; // past "stco" type + version/flags + entry count
        out[entry..entry + 4].copy_from_slice(&mdat_payload_offset.to_be_bytes());
        out
    }

    /// 16-bit PCM mono WAV — hand-built RIFF/WAVE container with a `fmt ` chunk
    /// (PCM, 1 channel, 44100 Hz, 16-bit) and a `data` chunk holding the raw
    /// little-endian sample bytes. Avoids hound (a dev-dep) so the fixture is
    /// usable from the fuzz crate as well as tests.
    pub fn wav(samples: &[i16]) -> Vec<u8> {
        // fmt  chunk payload: PCM format (16 bytes)
        let mut fmt = Vec::with_capacity(16);
        fmt.extend_from_slice(&1u16.to_le_bytes()); // wFormatTag = PCM
        fmt.extend_from_slice(&1u16.to_le_bytes()); // nChannels = 1
        fmt.extend_from_slice(&44_100u32.to_le_bytes()); // nSamplesPerSec
        fmt.extend_from_slice(&88_200u32.to_le_bytes()); // nAvgBytesPerSec = 44100*2
        fmt.extend_from_slice(&2u16.to_le_bytes()); // nBlockAlign = 2
        fmt.extend_from_slice(&16u16.to_le_bytes()); // wBitsPerSample

        let mut data_payload: Vec<u8> = Vec::with_capacity(samples.len() * 2);
        for &s in samples {
            data_payload.extend_from_slice(&s.to_le_bytes());
        }

        // Chunk helpers: 4-byte id + LE 32-bit size + payload.
        let mut fmt_chunk = b"fmt ".to_vec();
        fmt_chunk.extend_from_slice(&u32::try_from(fmt.len()).unwrap().to_le_bytes());
        fmt_chunk.extend_from_slice(&fmt);

        let mut data_chunk = b"data".to_vec();
        data_chunk.extend_from_slice(&u32::try_from(data_payload.len()).unwrap().to_le_bytes());
        data_chunk.extend_from_slice(&data_payload);

        // RIFF size = 4 ("WAVE") + fmt_chunk.len() + data_chunk.len()
        let riff_size = u32::try_from(4 + fmt_chunk.len() + data_chunk.len()).unwrap();
        let mut out = Vec::with_capacity(12 + fmt_chunk.len() + data_chunk.len());
        out.extend_from_slice(b"RIFF");
        out.extend_from_slice(&riff_size.to_le_bytes());
        out.extend_from_slice(b"WAVE");
        out.extend_from_slice(&fmt_chunk);
        out.extend_from_slice(&data_chunk);
        out
    }

    /// Minimal Ogg Opus stream — ported from `musefs-format::ogg::tests::opus_headers`
    /// + one audio page. Uses the same `page_test_support` helpers used in those tests.
    pub fn ogg_opus() -> Vec<u8> {
        use crate::ogg::page_test_support::{build_header_pub, lace_packet_pub};
        let head = b"OpusHead\x01\x02\x38\x01\x80\xbb\x00\x00\x00\x00\x00".to_vec();
        let tags = b"OpusTags\x06\x00\x00\x00musefs\x00\x00\x00\x00".to_vec();
        let (mut data, _) = build_header_pub(0x1234, &[&head, &tags]);
        let (audio, _) = lace_packet_pub(0x1234, 2, false, 960, &[0u8; 120]);
        data.extend_from_slice(&audio);
        data
    }

    /// Minimal MP3 — a hand-crafted ID3v2.4 header (empty tag) followed by a
    /// minimal MPEG frame sync. `locate_audio` requires only `ID3` marker +
    /// syncsafe size + a valid `0xFF 0xE*` sync at the audio start; the "audio"
    /// bytes that follow are never decoded by the synthesizer.
    pub fn mp3() -> Vec<u8> {
        // Minimal ID3v2.4 header: "ID3" + version 2.4.0 + flags(0) + size(0).
        let mut out = Vec::new();
        out.extend_from_slice(b"ID3");
        out.push(0x04); // major version 4
        out.push(0x00); // revision 0
        out.push(0x00); // flags
        // Syncsafe size = 0 (no frames).
        out.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]);
        // MPEG frame sync: 0xFF + 0xFB (MPEG1, Layer III, 128kbps, 44100Hz, stereo).
        // Followed by 2 bytes to satisfy the `audio_offset + 1 < len` check.
        out.extend_from_slice(&[0xFF, 0xFB, 0x90, 0x00]);
        out
    }
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

    #[test]
    fn accepts_trailing_pad_after_backing() {
        // WAV appends a 1-byte RIFF word-align pad after an odd-sized data chunk.
        let layout = RegionLayout::new(vec![
            Segment::Inline(vec![0u8; 8]),
            Segment::BackingAudio {
                offset: 100,
                len: 1,
            },
            Segment::Inline(vec![0x00]),
        ]);
        assert_backing_covers_audio(100, 1, &layout);
    }

    #[test]
    #[should_panic(expected = "split")]
    fn rejects_backing_split_by_metadata() {
        let layout = RegionLayout::new(vec![
            Segment::BackingAudio {
                offset: 100,
                len: 25,
            },
            Segment::Inline(vec![0xFF]),
            Segment::BackingAudio {
                offset: 125,
                len: 25,
            },
        ]);
        assert_backing_covers_audio(100, 50, &layout);
    }
}

#[cfg(test)]
mod fixtures_tests {
    use super::fixtures;

    #[test]
    fn flac_fixture_parses() {
        let f = fixtures::flac(&[1, 2, 3, 4, 5, 6, 7, 8]);
        let scan = crate::flac::locate_audio(&f).unwrap();
        assert_eq!(scan.audio_length, 8);
    }

    #[test]
    fn flac_block_encodes_24bit_length_big_endian() {
        // Pin all three length bytes of the 24-bit block-length field. The body is
        // sized so each byte is distinct and non-zero (0x030201 = 197_121), which
        // distinguishes `>> 16`/`>> 8` from `<< 16`/`<< 8` (a left shift would zero
        // the high/mid bytes). The header is 4 bytes: type|last, then 3 length bytes.
        let len = 0x03_02_01usize;
        let body = vec![0xABu8; len];
        let block = fixtures::flac_block(4, &body, false);
        assert_eq!(block[0], 4, "block type, last-flag clear");
        assert_eq!(
            &block[1..4],
            &[0x03, 0x02, 0x01],
            "24-bit big-endian length"
        );
        assert_eq!(block.len(), 4 + len);
    }

    #[test]
    fn m4a_fixture_parses() {
        let f = fixtures::m4a(&[9u8; 16]);
        let b = crate::mp4::locate_audio(&f).unwrap();
        assert_eq!(b.audio_length, 16);
    }

    #[test]
    fn m4a_two_covers_fixture_parses_with_both_pictures() {
        let f = fixtures::m4a_two_covers(&[9u8; 16]);
        let b = crate::mp4::locate_audio(&f).unwrap();
        assert_eq!(b.audio_length, 16);
        let pics = crate::mp4::read_pictures(&f);
        assert_eq!(pics.len(), 2);
        assert_eq!(pics[0].mime, "image/jpeg");
        assert_eq!(pics[0].data, [0xFF, 0xD8, 0xFF, 0xE0, 1, 2, 3]);
        assert_eq!(pics[1].mime, "image/png");
        assert_eq!(pics[1].data, [0x89, b'P', b'N', b'G', 4, 5]);
    }

    #[test]
    fn m4a_stco_offset_points_at_mdat_payload() {
        // The single stco chunk offset must be the absolute file position of the
        // mdat payload, not a placeholder 0 — otherwise a retag that shrinks the
        // moov underflows it and synthesis fails (see fixtures::m4a). Pin both the
        // value (out.len() - payload.len()) and where it is written (stco + 12).
        let payload = b"AUDIOAUDIO";
        let f = fixtures::m4a(payload);
        let expected = u32::try_from(f.len() - payload.len()).unwrap();
        let stco = f
            .windows(4)
            .position(|w| w == b"stco")
            .expect("stco present");
        // [stco][version/flags(4)][entry count(4)][offset(4)]
        let off = u32::from_be_bytes(f[stco + 12..stco + 16].try_into().unwrap());
        assert_eq!(off, expected);
        assert!(off > 0);
    }

    #[test]
    fn wav_fixture_parses() {
        let f = fixtures::wav(&[0i16, 1, -1, 100, -100]);
        let b = crate::wav::locate_audio(&f).unwrap();
        assert_eq!(b.audio_length, 10);
    }

    #[test]
    fn mp3_fixture_parses() {
        let f = fixtures::mp3();
        crate::mp3::locate_audio(&f).unwrap();
    }

    #[test]
    fn ogg_fixture_parses() {
        let f = fixtures::ogg_opus();
        crate::ogg::locate_audio(&f).unwrap();
    }
}
