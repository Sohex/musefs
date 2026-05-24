#![allow(dead_code)] // some block-type constants are first used in later M1 tasks

use crate::error::{FormatError, Result};

pub(crate) const FLAC_MARKER: &[u8; 4] = b"fLaC";

pub(crate) const BLOCK_STREAMINFO: u8 = 0;
pub(crate) const BLOCK_APPLICATION: u8 = 2;
pub(crate) const BLOCK_SEEKTABLE: u8 = 3;
pub(crate) const BLOCK_VORBIS_COMMENT: u8 = 4;
pub(crate) const BLOCK_CUESHEET: u8 = 5;
pub(crate) const BLOCK_PICTURE: u8 = 6;

/// A preserved FLAC metadata block: its type and its body (excluding the 4-byte header).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MetadataBlock {
    pub block_type: u8,
    pub body: Vec<u8>,
}

/// Result of scanning a FLAC file: where audio begins/ends and the structural blocks to preserve.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FlacScan {
    pub audio_offset: u64,
    pub audio_length: u64,
    pub preserved: Vec<MetadataBlock>,
}

/// Parse the FLAC metadata section, returning the audio boundary and the structural
/// blocks to carry over (STREAMINFO/APPLICATION/SEEKTABLE/CUESHEET). VORBIS_COMMENT,
/// PICTURE, and PADDING are dropped (regenerated or omitted at synthesis time).
pub fn locate_audio(data: &[u8]) -> Result<FlacScan> {
    if data.len() < 4 || &data[0..4] != FLAC_MARKER {
        return Err(FormatError::NotFlac);
    }
    let mut pos = 4usize;
    let mut preserved = Vec::new();
    loop {
        if pos + 4 > data.len() {
            return Err(FormatError::Malformed);
        }
        let header = data[pos];
        let is_last = (header & 0x80) != 0;
        let block_type = header & 0x7F;
        let len = ((data[pos + 1] as usize) << 16)
            | ((data[pos + 2] as usize) << 8)
            | (data[pos + 3] as usize);
        let body_start = pos + 4;
        let body_end = body_start + len;
        if body_end > data.len() {
            return Err(FormatError::Malformed);
        }
        match block_type {
            BLOCK_STREAMINFO | BLOCK_APPLICATION | BLOCK_SEEKTABLE | BLOCK_CUESHEET => {
                preserved.push(MetadataBlock {
                    block_type,
                    body: data[body_start..body_end].to_vec(),
                });
            }
            _ => {}
        }
        pos = body_end;
        if is_last {
            break;
        }
    }
    Ok(FlacScan {
        audio_offset: pos as u64,
        audio_length: (data.len() - pos) as u64,
        preserved,
    })
}
