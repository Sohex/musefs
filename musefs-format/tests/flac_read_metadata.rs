mod common;
use common::{make_flac, streaminfo_body, vorbis_comment_body};
use musefs_format::flac::{locate_audio, read_metadata};

#[test]
fn read_metadata_on_front_bytes_recovers_preserved_and_offset() {
    let si = streaminfo_body();
    let vc = vorbis_comment_body("v", &["TITLE=X"]);
    let audio = vec![0xAA; 40];
    let file = make_flac(&[(0, si.clone()), (4, vc)], &audio);

    let scan = locate_audio(&file).unwrap();
    let front = &file[..usize::try_from(scan.audio_offset).unwrap()];

    let meta = read_metadata(front).unwrap();
    assert_eq!(meta.audio_offset, scan.audio_offset);
    assert_eq!(meta.preserved, scan.preserved); // STREAMINFO only
}

#[test]
fn locate_audio_still_reports_audio_length() {
    let si = streaminfo_body();
    let audio = vec![0x11; 99];
    let file = make_flac(&[(0, si)], &audio);
    let scan = locate_audio(&file).unwrap();
    assert_eq!(scan.audio_length, 99);
}
