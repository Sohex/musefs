mod common;

use common::{build_wav, fmt_pcm_16bit_mono};
use musefs_format::FormatError;
use musefs_format::wav::{locate_audio, read_structure};

#[test]
fn locate_finds_data_bounds() {
    let data = vec![0x11u8; 10];
    let wav = build_wav(&[(b"fmt ", fmt_pcm_16bit_mono()), (b"data", data.clone())]);
    let bounds = locate_audio(&wav).unwrap();
    assert_eq!(bounds.audio_length, 10);
    assert_eq!(
        &wav[usize::try_from(bounds.audio_offset).unwrap()
            ..usize::try_from(bounds.audio_offset + bounds.audio_length).unwrap()],
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
    let front = &wav[..usize::try_from(bounds.audio_offset).unwrap()];
    let scan = read_structure(front).unwrap();
    assert_eq!(scan.fmt, fmt_pcm_16bit_mono());
    assert_eq!(scan.fact, None);
}
