//! Contract round-trip emitter (#204): open an externally-written DB read-only,
//! synthesize each track's served bytes via the serve path, and write them out
//! for an independent reader (mutagen) to verify. Mirrors `interop_emit.rs` but
//! sources rows from an EXISTING DB (scan + python-musefs writes) rather than
//! building one in-process.
use musefs_core::{BackingReader, HeaderCache, Mode, ReadAhead, ReadAheadPool, read_at_with_file};
use musefs_db::Db;
use std::sync::{Arc, Mutex};

#[test]
#[ignore = "contract emitter; run with MUSEFS_DB + MUSEFS_INTEROP_DIR set (see scripts/contract-roundtrip.sh)"]
fn emit_contract_fixtures() {
    let db_path = std::env::var("MUSEFS_DB").expect("set MUSEFS_DB");
    let out = std::env::var("MUSEFS_INTEROP_DIR").expect("set MUSEFS_INTEROP_DIR");
    let out = std::path::Path::new(&out);
    std::fs::create_dir_all(out).unwrap();

    let db = Db::open_readonly(&db_path).expect("open externally-written DB read-only");
    let cache = HeaderCache::new(Mode::Synthesis);

    let tracks = db.list_tracks().expect("list tracks");
    assert!(!tracks.is_empty(), "externally-written DB has no tracks");

    for track in tracks {
        let resolved = cache.resolve(&db, track.id).expect("resolve track");
        let file = std::fs::File::open(&resolved.backing_path).expect("open backing file");
        let pool = ReadAheadPool::new(0);
        let buf = Arc::new(Mutex::new(ReadAhead::new(0)));
        let br = BackingReader::new(&file, &buf, &pool, 0, resolved.total_len);
        let bytes = read_at_with_file(&resolved, &db, &br, 0, resolved.total_len)
            .expect("synthesize served bytes");
        // Name the output by track id + the backing file's extension, so the
        // independent reader can pick the right parser.
        let ext = std::path::Path::new(&track.backing_path)
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("bin");
        std::fs::write(out.join(format!("{}.{ext}", track.id)), &bytes).unwrap();
    }
}
