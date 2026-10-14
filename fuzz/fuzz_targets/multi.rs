#![no_main]
//! Several files over several sources, where the joins between fragments and segments live.
//! The first byte picks the layout: bit 0 MP3 output, bits 1-2 how many sources. Then each file
//! is a 3-byte length, a byte naming its source, and the bytes; each goes in as `sniff` says.

mod common;

use unmoof::{Format, Remuxer};

libfuzzer_sys::fuzz_target!(|data: &[u8]| {
    let Some((&cfg, mut rest)) = data.split_first() else { return };
    let mut rm = Remuxer::new();
    let n = 1 + ((cfg >> 1) & 3) as usize;
    let sources: Vec<usize> = (0..n).map(|_| rm.add_source()).collect();
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
