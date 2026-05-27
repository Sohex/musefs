#![no_main]
use libfuzzer_sys::fuzz_target;
use musefs_fuzz::MAX_INPUT;

// Parsing an untrusted VorbisComment body (with 32-bit length fields) must
// never panic; only return Ok/Err.
fuzz_target!(|data: &[u8]| {
    if data.len() > MAX_INPUT {
        return;
    }
    let _ = musefs_format::parse_vorbis_comment(data);
});
