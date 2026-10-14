#![no_main]
//! One SubRip, WebVTT or ASS file.

mod common;

libfuzzer_sys::fuzz_target!(|data: &[u8]| {
    let mut rm = unmoof::Remuxer::new();
    let s = rm.add_source();
    if rm.feed_subtitles(s, data).is_ok() {
        common::finish_and_check(rm, false, &[]);
    }
});
