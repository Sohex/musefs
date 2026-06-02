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
    write("mp4", "seed0", &fixtures::m4a(&[9u8; 32]));
    // m4a seed with trailing entropy so arb_binary_tags/arb_arts yield non-empty inputs.
    let mut m4a_bin = fixtures::m4a(&[9u8; 32]);
    m4a_bin.extend_from_slice(&[0x01; 64]);
    write("mp4", "seed_binary", &m4a_bin);
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
