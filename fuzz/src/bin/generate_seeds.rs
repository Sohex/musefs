use musefs_format::fuzz_check::fixtures;
use std::fs;
use std::path::Path;

fn write(target: &str, name: &str, bytes: &[u8]) {
    // Root the corpus path at the fuzz crate (CARGO_MANIFEST_DIR) so the
    // generator works regardless of the caller's working directory.
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("corpus")
        .join(target);
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join(name), bytes).unwrap();
}

fn main() {
    write("flac", "seed0", &fixtures::flac(&[1, 2, 3, 4, 5, 6, 7, 8]));
    write("mp3", "seed0", &fixtures::mp3());
    // An MP3 seed that already carries a binary GEOB ID3 frame, so the binary-tag
    // synthesis path gets immediate coverage instead of waiting for the fuzzer to
    // mutate its way to a non-empty arb_binary_tags from the empty-tag seed0.
    write("mp3", "seed_binary", &fixtures::mp3_with_binary_frame());
    write("mp4", "seed0", &fixtures::m4a(&[9u8; 32]));
    // m4a seed with a larger mdat payload: the extra bytes lengthen `data` so the
    // fuzz target's `Unstructured` yields non-empty arb_binary_tags/arb_arts, while
    // keeping the file well-formed (trailing bytes after `mdat` would make
    // read_structure reject it, skipping synthesize_layout entirely).
    write("mp4", "seed_binary", &fixtures::m4a(&[0x01; 96]));
    // Multi-art seed: a covr atom with two `data` children reaches the
    // read_pictures inner loop from the corpus, not only via mutation.
    write(
        "mp4",
        "seed_two_covers",
        &fixtures::m4a_two_covers(&[9u8; 32]),
    );
    write("ogg", "seed0", &fixtures::ogg_opus());
    write("ogg_page", "seed0", &fixtures::ogg_opus());
    write("vorbiscomment", "seed0", &fixtures::ogg_opus());
    write(
        "wav",
        "seed0",
        &fixtures::wav(&[0i16, 1, -1, 100, -100, 32767, -32768]),
    );
    println!("seeds written under fuzz/corpus/");
}
