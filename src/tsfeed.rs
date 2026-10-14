//! Turning the samples of MPEG-TS files into tracks: times unwrapped and joined across files,
//! and the sample descriptions made from the parameter sets and ADTS headers.

use crate::codec::{self, Adts, Mpa, Sps};
use crate::demux::{Sample, Track};
use crate::ts::{self, Es};
use crate::{bail, Remuxer, Result};

const WRAP: i64 = 1 << 33;
const CLOCK: i64 = 90_000;

pub struct TsState {
    /// The streams read, in PMT order, and the track each one became once it had a sample.
    pub pids: Vec<(Es, u16)>,
    /// HLS packed audio rather than MPEG-TS: one stream, its time in an ID3 tag.
    pub packed: bool,
    tracks: Vec<Option<usize>>,
    /// Parameter sets of each H.264 stream, by id, as first seen.
    sps: Vec<Vec<(u32, Vec<u8>)>>,
    adts: Vec<Option<Adts>>,
    mpa: Vec<Option<Mpa>>,
    /// The last time seen, for unwrapping the 33-bit clock.
    last: Option<i64>,
    /// Added to every time, to join files whose clocks jump.
    offset: i64,
    /// Where the next file should start showing, at 90 kHz.
    next: Option<i64>,
    /// The latest video decode time, at 90 kHz.
    last_dts: Option<i64>,
    warned_sps: bool,
    warned_adts: bool,
    dropped: usize,
}

impl TsState {
    /// Unwraps a 33-bit time to the one closest to the last time seen.
    fn unwrap(&mut self, t: u64) -> i64 {
        let t = t as i64;
        let v = match self.last {
            None => t,
            Some(last) => t + ((last - t) as f64 / WRAP as f64).round() as i64 * WRAP,
        };
        self.last = Some(v);
        v
    }
}

impl Remuxer {
    pub(crate) fn read_ts(&mut self, source: usize, file: u32, data: &[u8]) -> Result<()> {
        if self.sources[source].ts.is_none() {
            let packed = !ts::sniff(data) && ts::sniff_packed(data);
            let (pids, skipped) = if packed {
                let Some(es) = ts::packed_kind(data) else { bail!("source {source}: no AAC or MPEG audio frames found") };
                (vec![(es, 0)], Vec::new())
            } else {
                ts::streams(data)?
            };
            if pids.is_empty() {
                bail!("source {source}: the first TS file has no H.264, AAC or MPEG audio stream");
            }
            if !skipped.is_empty() {
                let list: Vec<String> = skipped.iter().map(|t| format!("0x{t:02x}")).collect();
                self.warnings.push(format!(
                    "source {source}: TS streams of type {} are not supported and left out",
                    list.join(", ")
                ));
            }
            let n = pids.len();
            self.sources[source].ts = Some(TsState {
                pids,
                packed,
                tracks: vec![None; n],
                sps: vec![Vec::new(); n],
                adts: vec![None; n],
                mpa: vec![None; n],
                last: None,
                offset: 0,
                next: None,
                last_dts: None,
                warned_sps: false,
                warned_adts: false,
                dropped: 0,
            });
        }
        let mut st = self.sources[source].ts.take().unwrap();
        let result = self.read_ts_file(source, file, data, &mut st);
        self.sources[source].ts = Some(st);
        result
    }

    fn read_ts_file(&mut self, source: usize, file: u32, data: &[u8], st: &mut TsState) -> Result<()> {
        let (units, broken) = ts::units(data, &st.pids, st.packed)?;
        if broken > 0 {
            self.warnings.push(format!("file {file}: {broken} damaged TS packets or stray bytes skipped"));
        }

        // A file whose clock does not go on from where the last one ended starts a new stretch
        // of timeline (an HLS discontinuity, or a missing segment); it is joined on, so that it
        // is shown right after the last frame shown before it.
        let times: Vec<(i64, i64)> = units
            .iter()
            .filter_map(|u| u.iter().find_map(|u| u.dts.map(|d| (u.pts.unwrap_or(d), d))))
            .map(|(p, d)| (st.unwrap(p), st.unwrap(d)))
            .collect();
        let first = times.iter().map(|&(p, _)| p).min();
        if let (Some(first), Some(next)) = (first, st.next) {
            let at = first + st.offset;
            if (at - next).abs() > CLOCK {
                self.warnings.push(format!(
                    "file {file}: the clock jumps by {:+.3} s; the file is joined on where the last one ended",
                    (at - next) as f64 / CLOCK as f64
                ));
                st.offset += next - at;
                // Decoding must still go forward: a file starting on a reordered frame may need
                // a little more room.
                let first_dts = times.iter().map(|&(_, d)| d).min().unwrap() + st.offset;
                if let Some(last) = st.last_dts {
                    if first_dts <= last {
                        st.offset += last - first_dts + 1;
                    }
                }
            }
        }

        let mut next = st.next.unwrap_or(i64::MIN);
        let dropped = st.dropped;
        for (i, list) in units.iter().enumerate() {
            for (k, u) in list.iter().enumerate() {
                let Some(ti) = self.ts_track(source, i, u, st) else {
                    st.dropped += 1;
                    continue;
                };
                // A video frame that starts in a PES after another frame has no time of its own:
                // it follows the frame before it at the same pace.
                let (dts, pts) = match u.dts {
                    Some(d) => {
                        let dts = st.unwrap(d) + st.offset;
                        (dts, u.pts.map(|p| st.unwrap(p) + st.offset).unwrap_or(dts))
                    }
                    None => {
                        let s = &self.tracks[ti].samples;
                        let Some(last) = s.last().filter(|_| st.pids[i].0 == Es::H264) else {
                            st.dropped += 1;
                            continue;
                        };
                        let step = if s.len() >= 2 { last.dts - s[s.len() - 2].dts } else { 3003 }.max(1);
                        (last.dts + step, last.dts + step + last.cto as i64)
                    }
                };
                let t = &mut self.tracks[ti];
                let (dts_t, dur, cto) = match st.pids[i].0 {
                    Es::H264 => (dts, 0u32, (pts - dts) as i32),
                    Es::Aac | Es::Mpa => {
                        let rate = t.timescale as i64;
                        let frame = match u.adts {
                            Some(a) => 1024 * a.blocks as i64,
                            None => u.mpa.map_or(1152, |m| m.samples) as i64,
                        };
                        let at = (dts * rate + CLOCK / 2).div_euclid(CLOCK) + u.frame_in_pes as i64 * frame;
                        next = next.max(dts + (u.frame_in_pes as i64 + 1) * frame * CLOCK / rate);
                        (at, frame as u32, 0)
                    }
                };
                if st.pids[i].0 == Es::H264 {
                    let step = t.samples.last().map(|s| dts_t - s.dts).filter(|&d| d > 0).unwrap_or(3003);
                    next = next.max(dts_t + cto as i64 + step);
                    st.last_dts = Some(st.last_dts.map_or(dts_t, |l| l.max(dts_t)));
                }
                t.samples.push(Sample {
                    dts: dts_t,
                    dur,
                    size: u.data.len() as u32,
                    cto,
                    sync: u.sync,
                    sdi: 1,
                    file,
                    off: k as u64,
                });
            }
        }
        if next > i64::MIN {
            st.next = Some(next);
        }
        if st.dropped > dropped {
            self.warnings.push(format!(
                "file {file}: {} frames before the first one a player can start from were left out",
                st.dropped - dropped
            ));
        }
        Ok(())
    }

    /// The track of stream `i`, made from the first sample that can describe it. Video waits
    /// for an access unit that carries its SPS and PPS.
    fn ts_track(&mut self, source: usize, i: usize, u: &ts::Unit, st: &mut TsState) -> Option<usize> {
        match st.pids[i].0 {
            Es::H264 => {
                let mut spss = Vec::new();
                let mut ppss = Vec::new();
                for n in codec::nals(&u.data) {
                    match n[0] & 0x1f {
                        7 => spss.push(n.to_vec()),
                        8 => ppss.push(n.to_vec()),
                        _ => {}
                    }
                }
                if let Some(ti) = st.tracks[i] {
                    for s in &spss {
                        if let Some(p) = codec::parse_sps(s) {
                            let known = st.sps[i].iter().find(|(id, _)| *id == p.id);
                            if known.is_some_and(|(_, b)| b != s) && !st.warned_sps {
                                st.warned_sps = true;
                                self.warnings.push(format!(
                                    "source {source}: the video's SPS changes mid-stream ({}x{} now); \
                                     players may show it wrongly",
                                    p.width, p.height
                                ));
                            }
                        }
                    }
                    return Some(ti);
                }
                let sps: Sps = spss.first().and_then(|s| codec::parse_sps(s))?;
                if ppss.is_empty() {
                    return None;
                }
                spss.dedup();
                ppss.dedup();
                st.sps[i] = spss.iter().filter_map(|s| codec::parse_sps(s).map(|p| (p.id, s.clone()))).collect();
                let mut t = Track::blank(source);
                t.src_id = st.pids[i].1 as u32;
                t.handler = *b"vide";
                t.codec = *b"avc1";
                t.timescale = CLOCK as u32;
                t.width = ((sps.width as u64 * sps.sar.0 as u64 / sps.sar.1 as u64) as u32) << 16;
                t.height = sps.height << 16;
                t.hdlr = codec::hdlr(b"vide", "VideoHandler");
                t.media_header = codec::vmhd();
                t.stsd = codec::avc1_stsd(&spss, &ppss, &sps);
                t.ts_stream = Some(i);
                Some(self.add_ts_track(i, t, st))
            }
            Es::Aac => {
                let a = u.adts?;
                if let Some(ti) = st.tracks[i] {
                    if st.adts[i].is_some_and(|f| !f.same_config(&a)) && !st.warned_adts {
                        st.warned_adts = true;
                        self.warnings.push(format!(
                            "source {source}: the AAC configuration changes mid-stream; the first one is kept"
                        ));
                    }
                    return Some(ti);
                }
                st.adts[i] = Some(a);
                Some(self.add_audio_track(source, i, a.rate(), codec::mp4a_stsd(&a, 0), st))
            }
            Es::Mpa => {
                let m = u.mpa?;
                if let Some(ti) = st.tracks[i] {
                    if st.mpa[i].is_some_and(|f| !f.same_config(&m)) && !st.warned_adts {
                        st.warned_adts = true;
                        self.warnings.push(format!(
                            "source {source}: the MPEG audio format changes mid-stream; the first one is kept"
                        ));
                    }
                    return Some(ti);
                }
                st.mpa[i] = Some(m);
                Some(self.add_audio_track(source, i, m.rate, codec::mpa_stsd(&m), st))
            }
        }
    }

    fn add_audio_track(&mut self, source: usize, i: usize, rate: u32, stsd: Vec<u8>, st: &mut TsState) -> usize {
        let mut t = Track::blank(source);
        t.src_id = st.pids[i].1 as u32;
        t.handler = *b"soun";
        t.codec = *b"mp4a";
        t.timescale = rate;
        t.volume = 0x100;
        t.hdlr = codec::hdlr(b"soun", "SoundHandler");
        t.media_header = codec::smhd();
        t.stsd = stsd;
        t.ts_stream = Some(i);
        self.add_ts_track(i, t, st)
    }

    fn add_ts_track(&mut self, i: usize, t: Track, st: &mut TsState) -> usize {
        self.tracks.push(t);
        st.tracks[i] = Some(self.tracks.len() - 1);
        self.tracks.len() - 1
    }
}

impl Track {
    /// Durations of video read from MPEG-TS, which states times only: each frame lasts until
    /// the next one is decoded, the last as long as the one before it.
    pub fn close_ts(&mut self) {
        if self.ts_stream.is_none() || &self.handler != b"vide" {
            return;
        }
        let n = self.samples.len();
        for i in 0..n.saturating_sub(1) {
            let d = self.samples[i + 1].dts - self.samples[i].dts;
            self.samples[i].dur = d.clamp(1, u32::MAX as i64) as u32;
            if d <= 0 {
                self.fixes += 1;
            }
        }
        if n >= 2 {
            self.samples[n - 1].dur = self.samples[n - 2].dur;
        } else if n == 1 {
            self.samples[0].dur = 3003;
        }
    }
}
