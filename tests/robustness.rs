//! Damaged input that once made unmoof panic, overflow or try to allocate gigabytes, built here
//! byte by byte so the cases need no media files. Each must now end in a clean `Err` or a plan.

use std::time::{Duration, Instant};

use unmoof::{Piece, Remuxer};

fn bx(t: &[u8; 4], body: &[u8]) -> Vec<u8> {
    let mut v = ((8 + body.len()) as u32).to_be_bytes().to_vec();
    v.extend_from_slice(t);
    v.extend_from_slice(body);
    v
}

fn full(t: &[u8; 4], version: u8, flags: u32, body: &[u8]) -> Vec<u8> {
    let mut b = ((u32::from(version) << 24) | flags).to_be_bytes().to_vec();
    b.extend_from_slice(body);
    bx(t, &b)
}

fn cat(parts: &[Vec<u8>]) -> Vec<u8> {
    parts.concat()
}

/// An init segment with one audio track, timescale 1000, whose samples default to 10 bytes.
fn init() -> Vec<u8> {
    init_with(1000)
}

fn init_with(timescale: u32) -> Vec<u8> {
    let mvhd = full(
        b"mvhd",
        0,
        0,
        &[
            &[0u8; 8][..],
            &1000u32.to_be_bytes(),
            &[0u8; 4],
            &[0, 1, 0, 0, 1, 0],
            &[0u8; 10 + 36 + 24],
            &2u32.to_be_bytes(),
        ]
        .concat(),
    );
    let tkhd = full(
        b"tkhd",
        0,
        3,
        &[&[0u8; 8][..], &1u32.to_be_bytes(), &[0u8; 4 + 4 + 8 + 4], &[1, 0, 0, 0], &[0u8; 36 + 8]].concat(),
    );
    let mdhd = full(
        b"mdhd",
        0,
        0,
        &[&[0u8; 8][..], &timescale.to_be_bytes(), &[0u8; 4], &[0x55, 0xc4, 0, 0]].concat(),
    );
    let hdlr = full(b"hdlr", 0, 0, &[&[0u8; 4][..], b"soun", &[0u8; 13]].concat());
    let stsd = full(b"stsd", 0, 0, &[&1u32.to_be_bytes()[..], &bx(b"mp4a", &[0u8; 28])].concat());
    let stbl = bx(b"stbl", &stsd);
    let minf = bx(b"minf", &cat(&[full(b"smhd", 0, 0, &[0u8; 4]), stbl]));
    let mdia = bx(b"mdia", &cat(&[mdhd, hdlr, minf]));
    let trak = bx(b"trak", &cat(&[tkhd, mdia]));
    let trex = full(
        b"trex",
        0,
        0,
        &[
            &1u32.to_be_bytes()[..],
            &1u32.to_be_bytes(),
            &1024u32.to_be_bytes(),
            &10u32.to_be_bytes(),
            &[0u8; 4],
        ]
        .concat(),
    );
    bx(b"moov", &cat(&[mvhd, trak, bx(b"mvex", &trex)]))
}

/// A fragment of track 1: `tfhd` with the given flags and fields, `tfdt` at `time`, and a
/// `trun` of `count` default samples whose data starts `offset` bytes from the base.
fn fragment(tfhd_flags: u32, tfhd_fields: &[u8], time: u64, count: u32, offset: i32) -> Vec<u8> {
    let tfhd = full(b"tfhd", 0, tfhd_flags, &[&1u32.to_be_bytes()[..], tfhd_fields].concat());
    let tfdt = full(b"tfdt", 1, 0, &time.to_be_bytes());
    let trun = full(b"trun", 0, 0x01, &[count.to_be_bytes(), offset.to_be_bytes()].concat());
    let moof = bx(
        b"moof",
        &cat(&[full(b"mfhd", 0, 0, &1u32.to_be_bytes()), bx(b"traf", &cat(&[tfhd, tfdt, trun]))]),
    );
    cat(&[moof, bx(b"mdat", &[0u8; 40])])
}

fn plan_adds_up(rm: Remuxer) {
    if let Ok(plan) = rm.finish() {
        let total: u64 = plan.header.len() as u64 + plan.pieces.iter().map(Piece::len).sum::<u64>();
        assert_eq!(total, plan.size);
    }
}

#[test]
fn healthy_fragment_lays_out() {
    let mut rm = Remuxer::new();
    let s = rm.add_source();
    rm.feed(s, 0, 0, &cat(&[init(), fragment(0x02_0000, &[], 0, 4, 0)])).unwrap();
    let plan = rm.finish().unwrap();
    assert_eq!(plan.tracks[0].samples, 4);
}

#[test]
fn trun_of_two_billion_default_samples_is_refused_quickly() {
    let t = Instant::now();
    let mut rm = Remuxer::new();
    let s = rm.add_source();
    let err = rm.feed(s, 0, 0, &cat(&[init(), fragment(0x02_0000, &[], 0, 0x7fff_ffff, 0)])).unwrap_err();
    assert!(err.to_string().contains("trun"), "{err}");
    assert!(t.elapsed() < Duration::from_secs(1));
}

#[test]
fn decode_times_at_the_end_of_time_do_not_overflow() {
    let mut rm = Remuxer::new();
    let s = rm.add_source();
    // Just under 2^63: adding one sample's duration used to overflow the decode time.
    let edge = i64::MAX as u64 - 100;
    let data = cat(&[
        init(),
        fragment(0x02_0000, &[], edge, 4, 0),
        fragment(0x02_0000, &[], 0, 4, 0),
        fragment(0x02_0000, &[], u64::MAX, 4, 0),
    ]);
    let _ = rm.feed(s, 0, 0, &data);
    plan_adds_up(rm);
}

#[test]
fn tracks_that_start_ages_apart() {
    // One track at zero, one 2^62 seconds later: the gap between them overflowed in milliseconds.
    let mut rm = Remuxer::new();
    let a = rm.add_source();
    rm.feed(a, 0, 0, &cat(&[init(), fragment(0x02_0000, &[], 0, 4, 0)])).unwrap();
    let b = rm.add_source();
    rm.feed(b, 1, 0, &cat(&[init_with(1), fragment(0x02_0000, &[], 1 << 62, 4, 0)])).unwrap();
    plan_adds_up(rm);
}

#[test]
fn time_offsets_of_any_size() {
    for ticks in [i64::MIN, -1, i64::MAX] {
        let mut rm = Remuxer::new();
        let a = rm.add_source();
        rm.feed(a, 0, 0, &cat(&[init(), fragment(0x02_0000, &[], 0, 4, 0)])).unwrap();
        let b = rm.add_source();
        rm.feed(b, 1, 0, &cat(&[init(), fragment(0x02_0000, &[], 0, 4, 0)])).unwrap();
        rm.set_time_offset(b, ticks, 1).unwrap();
        plan_adds_up(rm);
    }
}

#[test]
fn data_before_the_start_of_the_file_is_refused() {
    let mut rm = Remuxer::new();
    let s = rm.add_source();
    // base-data-offset 0, then a data offset of -100.
    let err = rm.feed(s, 0, 0, &cat(&[init(), fragment(0x01, &0u64.to_be_bytes(), 0, 4, -100)])).unwrap_err();
    assert!(err.to_string().contains("before the start"), "{err}");
}

#[test]
fn data_past_the_end_of_any_file_is_refused() {
    let mut rm = Remuxer::new();
    let s = rm.add_source();
    let err = rm
        .feed(s, 0, 0, &cat(&[init(), fragment(0x01, &(u64::MAX - 5).to_be_bytes(), 0, 4, 0)]))
        .unwrap_err();
    assert!(err.to_string().contains("past any file"), "{err}");
}

#[test]
fn ass_override_block_left_open_in_cyrillic() {
    let ass = "[Script Info]\n\n[Events]\nFormat: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text\n\
               Dialogue: 0,0:00:01.00,0:00:02.00,Default,,0,0,0,,Текст {\\i1 й";
    let mut rm = Remuxer::new();
    let s = rm.add_source();
    rm.feed_subtitles(s, ass.as_bytes()).unwrap();
    plan_adds_up(rm);
}

#[test]
fn subtitle_screen_over_64_kib_is_cut_to_fit() {
    // 300 different cues of 300 Cyrillic letters, all on screen at once: about 180 KB of text.
    let mut srt = String::new();
    for i in 0..300 {
        srt += &format!("{}\n00:00:01,000 --> 00:00:02,000\n<i>{i}{}</i>\n\n", i + 1, "ж".repeat(300));
    }
    let mut rm = Remuxer::new();
    let s = rm.add_source();
    rm.feed_subtitles(s, srt.as_bytes()).unwrap();
    let plan = rm.finish().unwrap();
    for p in &plan.pieces {
        if let Piece::Data(d) = p {
            // The first sample is the empty one before the cue, then the long one.
            let mut at = 0;
            while at + 2 <= d.len() {
                let n = usize::from(u16::from_be_bytes([d[at], d[at + 1]]));
                assert!(
                    std::str::from_utf8(&d[at + 2..at + 2 + n]).is_ok(),
                    "a sample's text was cut inside a character"
                );
                at += 2 + n;
                if at + 8 <= d.len() && &d[at + 4..at + 8] == b"styl" {
                    at += u32::from_be_bytes(d[at..at + 4].try_into().unwrap()) as usize;
                }
            }
            assert_eq!(at, d.len());
        }
    }
}

/// One 188-byte TS packet: PID, payload start flag, then the payload padded with 0xff.
fn ts_packet(pid: u16, start: bool, payload: &[u8]) -> Vec<u8> {
    let mut p = vec![0x47, (if start { 0x40 } else { 0 }) | (pid >> 8) as u8, pid as u8, 0x10];
    p.extend_from_slice(payload);
    p.resize(188, 0xff);
    p
}

/// A TS file with one H.264 stream on PID 0x100, its one PES holding `es`.
fn ts_with_h264(es: &[u8]) -> Vec<u8> {
    // PAT: program 1 on PMT PID 0x1000. PMT: stream type 0x1b on PID 0x100. CRCs are not checked.
    let pat = [0, 0x00, 0xb0, 13, 0, 1, 0xc1, 0, 0, 0, 1, 0xf0, 0x00, 0, 0, 0, 0];
    let pmt =
        [0, 0x02, 0xb0, 18, 0, 1, 0xc1, 0, 0, 0xe1, 0x00, 0xf0, 0, 0x1b, 0xe1, 0x00, 0xf0, 0, 0, 0, 0, 0];
    let mut pes = vec![0, 0, 1, 0xe0, 0, 0, 0x80, 0x80, 5, 0x21, 0, 1, 0, 1];
    pes.extend_from_slice(es);
    cat(&[ts_packet(0, true, &pat), ts_packet(0x1000, true, &pmt), ts_packet(0x100, true, &pes)])
}

/// A 33-bit PTS or DTS as a PES header writes it, after the 4-bit `prefix`.
fn time_field(prefix: u8, t: u64) -> [u8; 5] {
    [
        prefix << 4 | (t >> 29 & 0x0e) as u8 | 1,
        (t >> 22) as u8,
        (t >> 14 & 0xfe | 1) as u8,
        (t >> 7) as u8,
        (t << 1 & 0xfe | 1) as u8,
    ]
}

/// An ADTS frame of AAC-LC, 48 kHz stereo, `len` bytes long with its header.
fn adts(len: usize) -> Vec<u8> {
    let mut f = vec![
        0xff,
        0xf1,
        0x4c,
        0x80 | (len >> 11) as u8,
        (len >> 3) as u8,
        ((len & 7) << 5) as u8 | 0x1f,
        0xfc,
    ];
    f.resize(len, 0);
    f
}

/// A TS file of H.264 pictures on PID 0x100, one PES each with the PTS and DTS given (an IDR
/// picture with a 320x240 Baseline SPS and PPS first, then P pictures), and AAC on PID 0x101,
/// a PES of two frames at each time given.
fn ts_file(pictures: &[(u64, u64)], audio: &[u64]) -> Vec<u8> {
    let mut w = BitWriter::default();
    w.put(66, 8);
    w.put(0, 8);
    w.put(30, 8);
    w.ue(0); // sps id
    w.ue(0); // log2_max_frame_num_minus4
    w.ue(2); // pic_order_cnt_type
    w.ue(1); // max_num_ref_frames
    w.put(0, 1);
    w.ue(19); // pic_width_in_mbs_minus1
    w.ue(14); // pic_height_in_map_units_minus1
    w.put(1, 1); // frame_mbs_only
    w.put(0, 2);
    let sps = w.bytes();
    let pat = [0, 0x00, 0xb0, 13, 0, 1, 0xc1, 0, 0, 0, 1, 0xf0, 0x00, 0, 0, 0, 0];
    let mut streams = Vec::new();
    if !pictures.is_empty() {
        streams.extend_from_slice(&[0x1b, 0xe1, 0x00, 0xf0, 0]);
    }
    if !audio.is_empty() {
        streams.extend_from_slice(&[0x0f, 0xe1, 0x01, 0xf0, 0]);
    }
    let mut pmt = vec![0, 0x02, 0xb0, 13 + streams.len() as u8, 0, 1, 0xc1, 0, 0, 0xe1, 0x00, 0xf0, 0];
    pmt.extend(streams);
    pmt.extend_from_slice(&[0; 4]);
    let mut out = cat(&[ts_packet(0, true, &pat), ts_packet(0x1000, true, &pmt)]);
    for (k, &(pts, dts)) in pictures.iter().enumerate() {
        let mut pes = vec![0, 0, 1, 0xe0, 0, 0, 0x80, 0xc0, 10];
        pes.extend_from_slice(&time_field(3, pts));
        pes.extend_from_slice(&time_field(1, dts));
        pes.extend_from_slice(&[0, 0, 0, 1, 0x09, 0xf0]);
        if k == 0 {
            pes.extend_from_slice(&[0, 0, 0, 1, 0x67]);
            pes.extend_from_slice(&sps);
            pes.extend_from_slice(&[0, 0, 0, 1, 0x68, 0xce, 0x38, 0x80, 0, 0, 0, 1, 0x65, 0x88, 0x84, 0x21]);
        } else {
            pes.extend_from_slice(&[0, 0, 0, 1, 0x41, 0x9a, 0x02, 0x21]);
        }
        out.extend(ts_packet(0x100, true, &pes));
    }
    for &t in audio {
        // 14 bytes of PES header and two frames of 85 fill the packet exactly.
        let mut pes = vec![0, 0, 1, 0xc0, 0, 178, 0x80, 0x80, 5];
        pes.extend_from_slice(&time_field(2, t));
        pes.extend(cat(&[adts(85), adts(85)]));
        out.extend(ts_packet(0x101, true, &pes));
    }
    out
}

fn ts_with_aac(times: &[u64]) -> Vec<u8> {
    ts_file(&[], times)
}

fn ts_with_pictures(times: &[(u64, u64)]) -> Vec<u8> {
    ts_file(times, &[])
}

#[test]
fn audio_gap_in_ts_keeps_the_audio_after_it_in_time() {
    // Two frames at 1 s, then two more at 4 s: the three seconds between are missing.
    let mut rm = Remuxer::new();
    let s = rm.add_source();
    rm.feed_ts(s, 0, &ts_with_aac(&[90_000, 360_000])).unwrap();
    let plan = rm.finish().unwrap();
    let d = plan.tracks[0].duration;
    assert!((d - (3.0 + 2.0 * 1024.0 / 48000.0)).abs() < 0.002, "audio lasts {d} s");
    assert!(plan.warnings.iter().any(|w| w.contains("missing")), "{:?}", plan.warnings);
}

#[test]
fn a_lone_audio_time_far_ahead_is_no_gap() {
    // The third PES says 4900 s, the fourth goes on from the second: a damaged time, not a gap.
    let step = 2 * 1024 * 90_000 / 48_000;
    let mut rm = Remuxer::new();
    let s = rm.add_source();
    let times = [90_000, 90_000 + step, 90_000 + 4900 * 90_000, 90_000 + 3 * step];
    rm.feed_ts(s, 0, &ts_with_aac(&times)).unwrap();
    // Another source a second earlier, so the track is placed by its times, not its length.
    let early = rm.add_source();
    rm.feed_ts(early, 1, &ts_with_aac(&[0])).unwrap();
    let plan = rm.finish().unwrap();
    let d = plan.tracks[0].duration;
    assert!((d - 8.0 * 1024.0 / 48000.0).abs() < 0.002, "audio lasts {d} s");
    assert!(!plan.warnings.iter().any(|w| w.contains("missing")), "{:?}", plan.warnings);
}

#[test]
fn a_picture_shown_hours_after_its_decoding_is_damaged() {
    // 30 fps, each picture shown 100 ms after it is decoded, but the last picture of the first
    // file claims to show 4900 s late. It used to push the second file 4900 s on.
    let pic = |k: u64| (90_000 + k * 3000 + 9000, 90_000 + k * 3000);
    let mut a: Vec<(u64, u64)> = (0..5).map(pic).collect();
    a[4].0 += 4900 * 90_000;
    let b: Vec<(u64, u64)> = (5..10).map(pic).collect();
    let mut rm = Remuxer::new();
    let s = rm.add_source();
    rm.feed_ts(s, 0, &ts_with_pictures(&a)).unwrap();
    rm.feed_ts(s, 1, &ts_with_pictures(&b)).unwrap();
    let plan = rm.finish().unwrap();
    let d = plan.tracks[0].duration;
    assert!((d - 10.0 / 30.0).abs() < 0.002, "video lasts {d} s");
    assert!(!plan.warnings.iter().any(|w| w.contains("jumps")), "{:?}", plan.warnings);
    assert!(plan.warnings.iter().any(|w| w.contains("decoded")), "{:?}", plan.warnings);
}

#[test]
fn audio_muxed_ahead_of_the_video_is_no_jump() {
    // Each file carries its own pictures and the audio of ten seconds later, as some muxers
    // write it: each stream goes on from where it ended, though the file as a whole does not.
    let step = 2 * 1024 * 90_000 / 48_000;
    let pic = |k: u64| (90_000 + k * 3000 + 9000, 90_000 + k * 3000);
    let sound = |j: u64| 990_000 + j * step;
    let a = ts_file(&(0..5).map(pic).collect::<Vec<_>>(), &[sound(0), sound(1)]);
    let b = ts_file(&(5..10).map(pic).collect::<Vec<_>>(), &[sound(2), sound(3)]);
    let mut rm = Remuxer::new();
    let s = rm.add_source();
    rm.feed_ts(s, 0, &a).unwrap();
    rm.feed_ts(s, 1, &b).unwrap();
    let plan = rm.finish().unwrap();
    assert!(!plan.warnings.iter().any(|w| w.contains("jumps")), "{:?}", plan.warnings);
    let start = |h: &str| plan.tracks.iter().find(|t| t.handler == h).unwrap().start;
    assert!((start("soun") - start("vide") - 9.9).abs() < 0.002, "{} {}", start("soun"), start("vide"));
}

#[test]
fn empty_nal_unit_in_ts() {
    // A start code right after a start code: a NAL unit of no bytes, then an IDR slice.
    let ts = ts_with_h264(&[0, 0, 1, 0, 0, 1, 0x65, 0x88, 0x84, 0x00]);
    let mut rm = Remuxer::new();
    let s = rm.add_source();
    let _ = rm.feed_ts(s, 0, &ts);
    plan_adds_up(rm);
}

/// Writes bits for an SPS: fixed fields and Exp-Golomb codes.
#[derive(Default)]
struct BitWriter {
    bits: Vec<bool>,
}

impl BitWriter {
    fn put(&mut self, v: u64, n: u32) {
        for i in (0..n).rev() {
            self.bits.push(v >> i & 1 == 1);
        }
    }

    fn ue(&mut self, v: u64) {
        let n = 64 - (v + 1).leading_zeros();
        self.put(0, n - 1);
        self.put(v + 1, n);
    }

    fn bytes(mut self) -> Vec<u8> {
        self.bits.push(true); // rbsp stop bit
        while !self.bits.len().is_multiple_of(8) {
            self.bits.push(false);
        }
        self.bits.chunks(8).map(|c| c.iter().fold(0u8, |b, &x| b << 1 | u8::from(x))).collect()
    }
}

#[test]
fn sps_with_an_absurd_picture_size() {
    // Baseline SPS whose width in macroblocks is just under 2^32.
    let mut w = BitWriter::default();
    w.put(66, 8);
    w.put(0, 8);
    w.put(30, 8);
    w.ue(0); // sps id
    w.ue(0); // log2_max_frame_num_minus4
    w.ue(2); // pic_order_cnt_type
    w.ue(1); // max_num_ref_frames
    w.put(0, 1);
    w.ue(u64::from(u32::MAX) - 1); // pic_width_in_mbs_minus1
    w.ue(10);
    w.put(1, 1); // frame_mbs_only
    w.put(0, 2);
    let sps = w.bytes();
    let mut es = vec![0, 0, 0, 1, 0x67];
    es.extend_from_slice(&sps);
    es.extend_from_slice(&[0, 0, 0, 1, 0x68, 0xce, 0x38, 0x80, 0, 0, 0, 1, 0x65, 0x88, 0x84]);
    let mut rm = Remuxer::new();
    let s = rm.add_source();
    let _ = rm.feed_ts(s, 0, &ts_with_h264(&es));
    plan_adds_up(rm);
}

#[test]
fn sample_bomb_is_refused_once_the_file_size_is_known() {
    // 27 truns of a million default 10-byte samples each, in a few hundred bytes.
    let mut data = init();
    for _ in 0..27 {
        data.extend(fragment(0x02_0000, &[], 0, 1 << 20, 0));
    }
    let t = Instant::now();
    let mut rm = Remuxer::new();
    let s = rm.add_source();
    rm.set_file_size(0, data.len() as u64);
    let err = rm.feed(s, 0, 0, &data).unwrap_err();
    assert!(err.to_string().contains("past the end"), "{err}");
    assert!(t.elapsed() < Duration::from_millis(500), "took {:?}", t.elapsed());
}

#[test]
fn a_line_of_unclosed_brackets_takes_linear_time() {
    let srt = format!("1\n00:00:01,000 --> 00:00:02,000\n{}{}\n", "<".repeat(100_000), "{".repeat(100_000));
    let t = Instant::now();
    let mut rm = Remuxer::new();
    let s = rm.add_source();
    rm.feed_subtitles(s, srt.as_bytes()).unwrap();
    plan_adds_up(rm);
    assert!(t.elapsed() < Duration::from_secs(1), "took {:?}", t.elapsed());
}

/// Seconds it takes to lay out `n` subtitle cues that all stay on screen to the end.
fn overlapping_cues(n: usize) -> f64 {
    let mut srt = String::new();
    for i in 0..n {
        srt += &format!(
            "{}\n00:00:{:02},{:03} --> 01:00:00,000\nline {i}\n\n",
            i + 1,
            (i / 1000) % 60,
            i % 1000
        );
    }
    let t = Instant::now();
    let mut rm = Remuxer::new();
    let s = rm.add_source();
    rm.feed_subtitles(s, srt.as_bytes()).unwrap();
    plan_adds_up(rm);
    t.elapsed().as_secs_f64()
}

#[test]
fn overlapping_cues_take_linear_time() {
    // Ten times the cues take some ten times as long; a quadratic sweep takes a hundred. A ratio
    // holds on a busy machine, where a fixed limit in seconds does not.
    let small = overlapping_cues(5_000).min(overlapping_cues(5_000));
    let large = overlapping_cues(50_000).min(overlapping_cues(50_000));
    assert!(large < 30.0 * small, "5,000 cues took {small:.3} s, 50,000 took {large:.3} s");
}
