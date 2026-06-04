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
    // A second, identically-valid MP3 seed labeled for the binary-tag synthesis
    // path. fixtures::mp3() is the only MP3 builder (no parameterized/longer
    // variant exists), and a corrupt seed would make locate_audio reject the
    // file and skip synthesize_layout entirely — so reuse the valid fixture.
    // The fuzzer reaches non-empty arb_binary_tags via mutation from here.
    write("mp3", "seed_binary", &fixtures::mp3());
    write("mp4", "seed0", &fixtures::m4a(&[9u8; 32]));
    // m4a seed with a larger mdat payload: the extra bytes lengthen `data` so the
    // fuzz target's `Unstructured` yields non-empty arb_binary_tags/arb_arts, while
    // keeping the file well-formed (trailing bytes after `mdat` would make
    // read_structure reject it, skipping synthesize_layout entirely).
    write("mp4", "seed_binary", &fixtures::m4a(&[0x01; 96]));
    // Multi-art seed: a covr atom with two `data` children reaches the
    // read_pictures inner loop from the corpus, not only via mutation.
    write("mp4", "seed_two_covers", &fixtures::m4a_two_covers(&[9u8; 32]));
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
