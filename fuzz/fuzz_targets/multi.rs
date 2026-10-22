#![no_main]
//! Several files over several sources. Byte 0: bit 0 MP3 output, bits 1-2 sources, bit 3 a time
//! offset (12 bytes) for the last source; then per file a 3-byte length, a source byte, the bytes.

mod common;

use unmoof::{Format, Remuxer};

libfuzzer_sys::fuzz_target!(|data: &[u8]| {
    let Some((&cfg, mut rest)) = data.split_first() else { return };
    let mut rm = Remuxer::new();
    let n = 1 + ((cfg >> 1) & 3) as usize;
    let sources: Vec<usize> = (0..n).map(|_| rm.add_source()).collect();
    if cfg & 8 != 0 && rest.len() >= 12 {
        let ticks = i64::from_be_bytes(rest[..8].try_into().unwrap());
        let timescale = u32::from_be_bytes(rest[8..12].try_into().unwrap());
        rest = &rest[12..];
        if rm.set_time_offset(sources[n - 1], ticks, timescale).is_err() {
            return;
        }
    }
    let mut files: Vec<&[u8]> = Vec::new();
    while rest.len() >= 4 && files.len() < 16 {
        let len = (u32::from_be_bytes([0, rest[0], rest[1], rest[2]]) as usize).min(rest.len() - 4);
        let src = sources[rest[3] as usize % n];
        let f = &rest[4..4 + len];
        rest = &rest[4 + len..];
        let id = files.len() as u32;
        rm.set_file_size(id, f.len() as u64);
        let r = match unmoof::sniff(f) {
            Some(Format::Fmp4) => rm.feed(src, id, 0, f),
            Some(Format::Ts | Format::PackedAudio) => rm.feed_ts(src, id, f),
            Some(Format::Subtitles) => rm.feed_subtitles(src, f),
            None => continue,
        };
        files.push(f);
        if r.is_err() {
            return;
        }
    }
    common::finish_and_check(rm, cfg & 1 == 1, &files);
});
