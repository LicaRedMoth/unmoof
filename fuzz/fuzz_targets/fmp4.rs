#![no_main]
//! One fMP4 file: init segment and fragments, or anything else that claims to be boxes.

mod common;

libfuzzer_sys::fuzz_target!(|data: &[u8]| {
    let mut rm = unmoof::Remuxer::new();
    let s = rm.add_source();
    rm.set_file_size(0, data.len() as u64);
    if rm.feed(s, 0, 0, data).is_ok() {
        common::finish_and_check(rm, false, &[data]);
    }
});
