#![no_main]
use arbitrary::{Arbitrary, Unstructured};
use libfuzzer_sys::fuzz_target;
use musefs_format::ogg::{b64_len, b64_window, encode_b64_slice};
use musefs_fuzz::MAX_INPUT;

// A windowed base64 encode must equal the same slice of the full encode.
fuzz_target!(|data: &[u8]| {
    if data.len() > MAX_INPUT || data.is_empty() {
        return;
    }
    let mut u = Unstructured::new(data);
    let img: Vec<u8> = match Vec::arbitrary(&mut u) {
        Ok(v) => v,
        Err(_) => return,
    };
    if img.is_empty() {
        return;
    }
    let total = b64_len(img.len() as u64);
    if total == 0 {
        return;
    }
    let full = encode_b64_slice(&img, 0, total as usize);
    let out_off = match u.int_in_range(0..=total - 1) {
        Ok(v) => v,
        Err(_) => return,
    };
    let take = match u.int_in_range(1..=total - out_off) {
        Ok(v) => v,
        Err(_) => return,
    };
    let win = b64_window(out_off, take, img.len() as u64);
    let windowed = encode_b64_slice(
        &img[win.in_start as usize..(win.in_start + win.in_len) as usize],
        win.skip,
        take as usize,
    );
    assert_eq!(windowed.len(), take as usize, "windowed length != take");
    assert_eq!(
        windowed.as_slice(),
        &full[out_off as usize..(out_off + take) as usize],
        "windowed encode != slice of full encode",
    );
});
