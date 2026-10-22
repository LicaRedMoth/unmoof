//! Turning the samples of TS files and packed audio into tracks: times unwrapped and joined
//! across files, sample descriptions made from parameter sets and frame headers.

use crate::codec::{self, Adts, Mpa};
use crate::demux::{Sample, Track};
use crate::ts::{self, Es};
use crate::{Remuxer, Result, bail};

const WRAP: i64 = 1 << 33;
const CLOCK: i64 = 90_000;

pub struct TsState {
    /// The streams read, in PMT order, and the track each one became once it had a sample.
    pub pids: Vec<(Es, u16)>,
    /// HLS packed audio rather than MPEG-TS: one stream, its time in an ID3 tag.
    pub packed: bool,
    tracks: Vec<Option<usize>>,
    /// The SPS of each video stream, by id, as first seen.
    sps: Vec<Vec<(u32, Vec<u8>)>>,
    adts: Vec<Option<Adts>>,
    mpa: Vec<Option<Mpa>>,
    /// The last time seen, for unwrapping the 33-bit clock.
    last: Option<i64>,
    /// Added to every time, to join files whose clocks jump.
    offset: i64,
    /// Where the next file should start showing, at 90 kHz.
    next: Option<i64>,
    /// Where each stream's last frame ended, at 90 kHz.
    ends: Vec<Option<i64>>,
    /// The latest video decode time, at 90 kHz.
    last_dts: Option<i64>,
    warned_sps: bool,
    warned_adts: bool,
    dropped: usize,
    /// For each HEVC stream still written as hvc1, what its sample entry was made from.
    hevc: Vec<Option<HevcSets>>,
}

/// The parameter sets and SPS an HEVC sample entry was made from.
struct HevcSets {
    vpss: Vec<Vec<u8>>,
    spss: Vec<Vec<u8>>,
    ppss: Vec<Vec<u8>>,
    sps: codec::HevcSps,
}

impl HevcSets {
    fn all(&self) -> Vec<Vec<u8>> {
        [&self.vpss[..], &self.spss[..], &self.ppss[..]].concat()
    }
}

/// How `Plan::render` reads a source's TS files again: its streams, whether it is packed audio,
/// and per stream the parameter sets to leave out of the samples.
#[derive(Clone, Default)]
pub struct TsReading {
    pub pids: Vec<(Es, u16)>,
    pub packed: bool,
    pub strip: Vec<Option<Vec<Vec<u8>>>>,
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
                let Some(es) = ts::packed_kind(data) else {
                    bail!("source {source}: no AAC or MPEG audio frames found")
                };
                (vec![(es, 0)], Vec::new())
            } else {
                ts::streams(data)?
            };
            if pids.is_empty() {
                bail!("source {source}: the first TS file has no H.264, HEVC, AAC or MPEG audio stream");
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
                ends: vec![None; n],
                last_dts: None,
                warned_sps: false,
                warned_adts: false,
                dropped: 0,
                hevc: (0..n).map(|_| None).collect(),
            });
        }
        let mut st = self.sources[source].ts.take().unwrap();
        let result = self.read_ts_file(source, file, data, &mut st);
        self.sources[source].ts = Some(st);
        result
    }

    fn read_ts_file(&mut self, source: usize, file: u32, data: &[u8], st: &mut TsState) -> Result<()> {
        let (mut units, broken) = ts::units(data, &st.pids, st.packed)?;
        if broken > 0 {
            self.warnings.push(format!(
                "file {file}: skipped {broken} damaged TS packets or stretches of bytes that are not frames"
            ));
        }
        for (i, list) in units.iter_mut().enumerate() {
            if st.pids[i].0.is_video() {
                let (n, worst) = mend_picture_times(list);
                if n > 0 {
                    self.warnings.push(format!(
                        "file {file}: {n} pictures claim to be shown up to {:.3} s after they are decoded; \
                         they are shown as late as the others",
                        worst as f64 / CLOCK as f64
                    ));
                }
            }
        }

        // When every stream's clock jumps from the last file, the file is shifted to follow it;
        // one stream alone is no jump, as muxers may carry audio well ahead of the video.
        let times: Vec<(usize, i64, i64)> = units
            .iter()
            .enumerate()
            .filter_map(|(i, u)| u.iter().find_map(|u| u.dts.map(|d| (i, u.pts.unwrap_or(d), d))))
            .map(|(i, p, d)| (i, st.unwrap(p), st.unwrap(d)))
            .collect();
        let first = times.iter().map(|&(_, p, _)| p).min();
        let steps: Vec<bool> = times
            .iter()
            .filter_map(|&(i, p, _)| st.ends[i].map(|end| (p + st.offset - end).abs() > CLOCK))
            .collect();
        if let (Some(first), Some(next)) = (first, st.next) {
            let at = first + st.offset;
            let jumped = if steps.is_empty() { (at - next).abs() > CLOCK } else { steps.iter().all(|&j| j) };
            if jumped {
                self.warnings.push(format!(
                    "file {file}: the clock jumps by {:+.3} s; the file is joined on where the last one ended",
                    (at - next) as f64 / CLOCK as f64
                ));
                st.offset += next - at;
                // Decoding must still go forward: a file starting on a reordered frame may need
                // a little more room.
                let first_dts = times.iter().map(|&(_, _, d)| d).min().unwrap() + st.offset;
                if let Some(last) = st.last_dts
                    && first_dts <= last
                {
                    st.offset += last - first_dts + 1;
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
                        let Some(last) = s.last().filter(|_| st.pids[i].0.is_video()) else {
                            st.dropped += 1;
                            continue;
                        };
                        let step = if s.len() >= 2 { last.dts - s[s.len() - 2].dts } else { 3003 }.max(1);
                        (last.dts + step, last.dts + step + last.cto as i64)
                    }
                };
                let t = &mut self.tracks[ti];
                let (dts_t, dur, cto) = match st.pids[i].0 {
                    Es::H264 | Es::H265 => {
                        (dts, 0u32, (pts - dts).clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32)
                    }
                    Es::Aac | Es::Mpa => {
                        let rate = t.timescale as i64;
                        let frame = match u.adts {
                            Some(a) => 1024 * a.blocks as i64,
                            None => u.mpa.map_or(1152, |m| m.samples) as i64,
                        };
                        let at = (dts * rate + CLOCK / 2).div_euclid(CLOCK) + u.frame_in_pes as i64 * frame;
                        let end = dts + (u.frame_in_pes as i64 + 1) * frame * CLOCK / rate;
                        next = next.max(end);
                        st.ends[i] = Some(st.ends[i].map_or(end, |e| e.max(end)));
                        (at, frame as u32, 0)
                    }
                };
                if st.pids[i].0.is_video() {
                    let step = t.samples.last().map(|s| dts_t - s.dts).filter(|&d| d > 0).unwrap_or(3003);
                    let end = dts_t + cto as i64 + step;
                    next = next.max(end);
                    st.ends[i] = Some(st.ends[i].map_or(end, |e| e.max(end)));
                    st.last_dts = Some(st.last_dts.map_or(dts_t, |l| l.max(dts_t)));
                }
                if let Some(sets) = &st.hevc[i] {
                    let known = sets.all();
                    let mut cut = 0u32;
                    let mut unknown = false;
                    for n in codec::nals(&u.data).filter(|n| codec::is_hevc_parameter_set(n)) {
                        if known.iter().any(|k| k == n) {
                            cut += 4 + n.len() as u32;
                        } else {
                            unknown = true;
                        }
                    }

                    if unknown {
                        // Parameter sets that change mid-stream must stay in the samples: hev1.
                        t.stsd = codec::hevc_stsd(&sets.vpss, &sets.spss, &sets.ppss, &sets.sps, false);
                        t.codec = *b"hev1";
                        t.ps_cuts.clear();
                        st.hevc[i] = None;
                    } else if cut > 0 {
                        t.ps_cuts.push((t.samples.len(), cut));
                    }
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
    /// for an access unit that carries its SPS and PPS, and in HEVC its VPS.
    fn ts_track(&mut self, source: usize, i: usize, u: &ts::Unit, st: &mut TsState) -> Option<usize> {
        match st.pids[i].0 {
            Es::H264 | Es::H265 => {
                let hevc = st.pids[i].0 == Es::H265;
                let (mut vpss, mut spss, mut ppss) = (Vec::new(), Vec::new(), Vec::new());
                for n in codec::nals(&u.data) {
                    match (hevc, codec::nal_type(n[0], hevc)) {
                        (true, 32) => vpss.push(n.to_vec()),
                        (false, 7) | (true, 33) => spss.push(n.to_vec()),
                        (false, 8) | (true, 34) => ppss.push(n.to_vec()),
                        _ => {}
                    }
                }
                // The id, picture size and sample aspect ratio an SPS states.
                let read = |s: &[u8]| {
                    if hevc {
                        codec::parse_hevc_sps(s).map(|p| (p.id, p.width, p.height, p.sar))
                    } else {
                        codec::parse_sps(s).map(|p| (p.id, p.width, p.height, p.sar))
                    }
                };
                if let Some(ti) = st.tracks[i] {
                    for s in &spss {
                        if let Some((id, width, height, _)) = read(s) {
                            let known = st.sps[i].iter().find(|(k, _)| *k == id);
                            if known.is_some_and(|(_, b)| b != s) && !st.warned_sps {
                                st.warned_sps = true;
                                self.warnings.push(format!(
                                    "source {source}: the video's SPS changes mid-stream ({width}x{height} now); \
                                     players may show it wrongly"
                                ));
                            }
                        }
                    }
                    return Some(ti);
                }
                let (_, width, height, sar) = spss.first().and_then(|s| read(s))?;
                if ppss.is_empty() || (hevc && vpss.is_empty()) {
                    return None;
                }
                spss.dedup();
                ppss.dedup();
                vpss.dedup();

                // HEVC starts out as hvc1; a parameter set seen later that differs turns it hev1.
                let stsd = if hevc {
                    let sps = codec::parse_hevc_sps(&spss[0])?;
                    let stsd = codec::hevc_stsd(&vpss, &spss, &ppss, &sps, true);
                    st.hevc[i] =
                        Some(HevcSets { vpss: vpss.clone(), spss: spss.clone(), ppss: ppss.clone(), sps });
                    stsd
                } else {
                    codec::avc1_stsd(&spss, &ppss, &codec::parse_sps(&spss[0])?)
                };
                st.sps[i] = spss.iter().filter_map(|s| read(s).map(|p| (p.0, s.clone()))).collect();
                let mut t = Track::blank(source);
                t.src_id = st.pids[i].1 as u32;
                t.handler = *b"vide";
                t.codec = if hevc { *b"hvc1" } else { *b"avc1" };
                t.timescale = CLOCK as u32;
                let shown = u64::from(width) * u64::from(sar.0) / u64::from(sar.1);
                t.width = (shown.min(0xffff) as u32) << 16;
                t.height = height.min(0xffff) << 16;
                t.hdlr = codec::hdlr(b"vide", "VideoHandler");
                t.media_header = codec::vmhd();
                t.stsd = stsd;
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
                Some(self.add_audio_track(source, i, a.rate(), codec::aac_stsd(&a, 0), st))
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

    fn add_audio_track(
        &mut self,
        source: usize,
        i: usize,
        rate: u32,
        stsd: Vec<u8>,
        st: &mut TsState,
    ) -> usize {
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

    /// How each source's TS files are read again when the output is written.
    pub(crate) fn ts_readings(&self) -> Vec<TsReading> {
        let reading = |st: &TsState| TsReading {
            pids: st.pids.clone(),
            packed: st.packed,
            strip: st.hevc.iter().map(|h| h.as_ref().map(HevcSets::all)).collect(),
        };
        self.sources.iter().map(|s| s.ts.as_ref().map(reading).unwrap_or_default()).collect()
    }

    fn add_ts_track(&mut self, i: usize, t: Track, st: &mut TsState) -> usize {
        self.tracks.push(t);
        st.tracks[i] = Some(self.tracks.len() - 1);
        self.tracks.len() - 1
    }
}

/// No decoder holds a picture back for anything like this long: up to 16 pictures wait.
const MAX_REORDER: i64 = 10 * CLOCK;

/// Gives pictures whose presentation time lies absurdly far from their decode time the offset
/// most pictures of the file have. Returns how many were mended and the worst offset seen.
fn mend_picture_times(list: &mut [ts::Unit]) -> (usize, i64) {
    let offset = |u: &ts::Unit| {
        let d = (u.pts? as i64 - u.dts? as i64).rem_euclid(WRAP);
        Some(if d >= WRAP / 2 { d - WRAP } else { d })
    };
    let mut good: Vec<i64> = list.iter().filter_map(offset).filter(|d| d.abs() <= MAX_REORDER).collect();
    good.sort_unstable();
    let usual = good.get(good.len() / 2).copied().unwrap_or(0);
    let (mut n, mut worst) = (0, 0i64);
    for u in list.iter_mut() {
        if let (Some(d), Some(dts)) = (offset(u), u.dts)
            && d.abs() > MAX_REORDER
        {
            u.pts = Some((dts as i64 + usual).rem_euclid(WRAP) as u64);
            n += 1;
            worst = worst.max(d.abs());
        }
    }
    (n, worst)
}

impl Track {
    /// Durations from TS times: a picture lasts until the next is decoded, the last like the one
    /// before; an audio frame lasts a frame, or up to the next frame across a gap.
    pub fn close_ts(&mut self, warnings: &mut Vec<String>) {
        if self.ts_stream.is_none() {
            return;
        }
        if &self.handler != b"vide" {
            let (mut spikes, mut worst) = (0, 0i64);
            for i in 0..self.samples.len().saturating_sub(1) {
                let (s, next) = (&self.samples[i], &self.samples[i + 1]);
                let gap = next.dts - (s.dts + s.dur as i64);
                if gap * 2 <= s.dur as i64 {
                    continue;
                }
                // A gap is a step the clock stays on; one damaged time is a spike it comes back from.
                if self.samples[i + 2..].iter().take(32).any(|x| x.dts < next.dts) {
                    let at = s.dts + s.dur as i64;
                    self.samples[i + 1].dts = at;
                    spikes += 1;
                    worst = worst.max(gap);
                    self.fixes += 1;
                    continue;
                }
                if gap > self.timescale as i64 / 2 {
                    warnings.push(format!(
                        "track {}: {:.3} s of audio missing after {:.3} s; the gap is kept",
                        self.src_id,
                        gap as f64 / self.timescale as f64,
                        (s.dts - self.samples[0].dts) as f64 / self.timescale as f64
                    ));
                }
                self.samples[i].dur = (s.dur as i64 + gap).min(u32::MAX as i64) as u32;
                self.fixes += 1;
            }
            if spikes > 0 {
                warnings.push(format!(
                    "track {}: {spikes} audio frames carry times up to {:.3} s off the frames around them; \
                     they are played in turn",
                    self.src_id,
                    worst as f64 / self.timescale as f64
                ));
            }
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
