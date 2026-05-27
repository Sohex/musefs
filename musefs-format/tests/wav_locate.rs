use musefs_format::wav::{locate_audio, read_structure};
use musefs_format::FormatError;

/// A 16-byte PCM `fmt ` payload: mono, 44.1 kHz, 16-bit.
pub fn fmt_pcm_16bit_mono() -> Vec<u8> {
    let mut f = Vec::new();
    f.extend_from_slice(&1u16.to_le_bytes()); // wFormatTag = PCM
    f.extend_from_slice(&1u16.to_le_bytes()); // channels
    f.extend_from_slice(&44_100u32.to_le_bytes()); // sample rate
    f.extend_from_slice(&88_200u32.to_le_bytes()); // byte rate
    f.extend_from_slice(&2u16.to_le_bytes()); // block align
    f.extend_from_slice(&16u16.to_le_bytes()); // bits per sample
    f
}

/// Build a minimal valid `RIFF/WAVE` file from a list of `(fourcc, payload)` chunks.
pub fn build_wav(chunks: &[(&[u8; 4], Vec<u8>)]) -> Vec<u8> {
    let mut body = Vec::new();
    for (id, payload) in chunks {
        body.extend_from_slice(*id);
        body.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        body.extend_from_slice(payload);
        if payload.len() % 2 == 1 {
            body.push(0x00);
        }
    }
    let mut out = Vec::new();
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&((body.len() + 4) as u32).to_le_bytes());
    out.extend_from_slice(b"WAVE");
    out.extend_from_slice(&body);
    out
}

#[test]
fn locate_finds_data_bounds() {
    let data = vec![0x11u8; 10];
    let wav = build_wav(&[(b"fmt ", fmt_pcm_16bit_mono()), (b"data", data.clone())]);
    let bounds = locate_audio(&wav).unwrap();
    assert_eq!(bounds.audio_length, 10);
    assert_eq!(
        &wav[bounds.audio_offset as usize..(bounds.audio_offset + bounds.audio_length) as usize],
        data.as_slice()
    );
}

#[test]
fn locate_rejects_non_wave_and_rf64() {
    assert_eq!(
        locate_audio(b"not a riff file at all"),
        Err(FormatError::NotWav)
    );
    let mut rf64 = build_wav(&[(b"fmt ", fmt_pcm_16bit_mono()), (b"data", vec![0u8; 4])]);
    rf64[0..4].copy_from_slice(b"RF64");
    assert_eq!(locate_audio(&rf64), Err(FormatError::NotWav));
}

#[test]
fn locate_requires_fmt_and_data() {
    let only_fmt = build_wav(&[(b"fmt ", fmt_pcm_16bit_mono())]);
    assert_eq!(locate_audio(&only_fmt), Err(FormatError::NotWav));
}

#[test]
fn read_structure_extracts_fmt_and_optional_fact() {
    let fact = 12_345u32.to_le_bytes().to_vec();
    let wav = build_wav(&[
        (b"fmt ", fmt_pcm_16bit_mono()),
        (b"fact", fact.clone()),
        (b"data", vec![0u8; 6]),
    ]);
    let scan = read_structure(&wav).unwrap();
    assert_eq!(scan.fmt, fmt_pcm_16bit_mono());
    assert_eq!(scan.fact, Some(fact));
}

#[test]
fn read_structure_works_on_front_only_buffer() {
    // Truncate to exactly the data payload start (what reader's read_front yields):
    // walk must still surface `fmt ` even though `data`'s payload is absent.
    let wav = build_wav(&[(b"fmt ", fmt_pcm_16bit_mono()), (b"data", vec![0u8; 100])]);
    let bounds = locate_audio(&wav).unwrap();
    let front = &wav[..bounds.audio_offset as usize];
    let scan = read_structure(front).unwrap();
    assert_eq!(scan.fmt, fmt_pcm_16bit_mono());
    assert_eq!(scan.fact, None);
}
