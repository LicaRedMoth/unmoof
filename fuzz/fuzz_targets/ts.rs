#![no_main]
//! One MPEG-TS file or HLS packed audio segment, laid out as MP4 and as bare MP3.

mod common;

libfuzzer_sys::fuzz_target!(|data: &[u8]| {
    for mp3 in [false, true] {
        let mut rm = unmoof::Remuxer::new();
        let s = rm.add_source();
        if rm.feed_ts(s, 0, data).is_ok() {
            common::finish_and_check(rm, mp3, &[data]);
        }
    }
});
