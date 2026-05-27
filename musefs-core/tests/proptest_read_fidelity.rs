mod common;
use common::write_flac;
use musefs_core::{read_at, HeaderCache, Mode};
use musefs_db::{Db, Format, NewTrack, Tag};
use proptest::prelude::*;

fn build(audio: &[u8], title: &str) -> (tempfile::TempDir, Db, i64, Vec<u8>) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("song.flac");
    let (audio_offset, audio_length) = write_flac(&path, &["TITLE=Orig"], audio);
    let meta = std::fs::metadata(&path).unwrap();
    let db = Db::open_in_memory().unwrap();
    let id = db
        .upsert_track(&NewTrack {
            backing_path: path.to_string_lossy().to_string(),
            format: Format::Flac,
            audio_offset,
            audio_length,
            backing_size: meta.len() as i64,
            backing_mtime: meta
                .modified()
                .unwrap()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs() as i64,
        })
        .unwrap();
    db.replace_tags(id, &[Tag::new("title", title, 0)]).unwrap();
    (dir, db, id, audio.to_vec())
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn read_at_preserves_backing_audio(
        audio in proptest::collection::vec(any::<u8>(), 1..512),
        title in "[ -~]{0,32}",
    ) {
        let (_dir, db, id, original) = build(&audio, &title);
        let resolved = HeaderCache::new(Mode::Synthesis).resolve(&db, id).unwrap();
        let whole = read_at(&resolved, &db, 0, resolved.total_len).unwrap();
        prop_assert_eq!(whole.len() as u64, resolved.total_len);
        let served_audio = &whole[resolved.layout.header_len() as usize..];
        prop_assert_eq!(served_audio, &original[..]);
    }
}
