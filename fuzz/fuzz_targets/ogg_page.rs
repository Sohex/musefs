#![no_main]
use libfuzzer_sys::fuzz_target;
use musefs_fuzz::MAX_INPUT;

// parse_page must never panic on arbitrary bytes at an arbitrary position.
fuzz_target!(|data: &[u8]| {
    if data.len() > MAX_INPUT {
        return;
    }
    let _ = musefs_format::ogg::parse_page(data, 0);
});
