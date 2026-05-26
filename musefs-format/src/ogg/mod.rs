mod crc;
mod page;

use crate::error::{FormatError, Result};

/// The codec carried inside an Ogg logical bitstream that we synthesize.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Codec {
    Opus,
    Vorbis,
    OggFlac,
}

fn detect_codec(first_packet: &[u8]) -> Result<Codec> {
    if first_packet.len() >= 8 && &first_packet[0..8] == b"OpusHead" {
        Ok(Codec::Opus)
    } else if first_packet.len() >= 7 && &first_packet[0..7] == b"\x01vorbis" {
        Ok(Codec::Vorbis)
    } else if first_packet.len() >= 5 && &first_packet[0..5] == b"\x7FFLAC" {
        Ok(Codec::OggFlac)
    } else {
        Err(FormatError::Malformed)
    }
}

/// For OggFLAC, packet 0 is `0x7F "FLAC" major minor count(2, BE) "fLaC" STREAMINFO`.
/// The 16-bit big-endian count is the number of metadata-block packets that follow
/// packet 0.
fn oggflac_following_packets(first_packet: &[u8]) -> Result<usize> {
    if first_packet.len() < 9 {
        return Err(FormatError::Malformed);
    }
    Ok(u16::from_be_bytes([first_packet[7], first_packet[8]]) as usize)
}

/// The parsed Ogg header region: codec, serial, the reassembled header packets,
/// the number of header pages, and where audio begins.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OggHeader {
    pub codec: Codec,
    pub serial: u32,
    pub packets: Vec<Vec<u8>>,
    pub header_pages: u32,
    pub audio_offset: u64,
}

/// Parse the header region from the front of a logical bitstream. `data` may be the
/// whole file or just `[0, audio_offset)`; either way parsing stops once all header
/// packets are reassembled.
pub fn read_header(data: &[u8]) -> Result<OggHeader> {
    let first_page = page::parse_page(data, 0)?;
    let serial = first_page.serial;

    // Reassemble the first packet to detect the codec and (for OggFLAC) the count.
    let first = page::read_packets(data, 1)?;
    let first_pkt = first.first().ok_or(FormatError::Malformed)?;
    let codec = detect_codec(&first_pkt.data)?;

    let want = match codec {
        Codec::Opus => 2,
        Codec::Vorbis => 3,
        Codec::OggFlac => 1 + oggflac_following_packets(&first_pkt.data)?,
    };

    let pkts = page::read_packets(data, want)?;
    if pkts.len() != want {
        return Err(FormatError::Malformed);
    }
    let last = pkts.last().unwrap();
    Ok(OggHeader {
        codec,
        serial,
        packets: pkts.iter().map(|p| p.data.clone()).collect(),
        header_pages: last.pages_through_end,
        audio_offset: last.end_offset as u64,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ogg::page::{build_header, lace_packet};

    fn opus_headers() -> Vec<u8> {
        let head = b"OpusHead\x01\x02\x38\x01\x80\xbb\x00\x00\x00\x00\x00".to_vec();
        let tags = b"OpusTags\x06\x00\x00\x00musefs\x00\x00\x00\x00".to_vec();
        let (bytes, _) = build_header(0x1234, &[&head, &tags]);
        bytes
    }

    #[test]
    fn reads_opus_header() {
        let mut data = opus_headers();
        // Append one audio page so audio_offset lands before EOF.
        let (audio, _) = lace_packet(0x1234, 2, false, 960, &vec![0u8; 100]);
        let header_len = data.len();
        data.extend_from_slice(&audio);

        let h = read_header(&data).unwrap();
        assert_eq!(h.codec, Codec::Opus);
        assert_eq!(h.serial, 0x1234);
        assert_eq!(h.packets.len(), 2);
        assert_eq!(h.audio_offset, header_len as u64);
        assert_eq!(h.header_pages, 2);
    }
}
