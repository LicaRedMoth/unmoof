//! Laying out the progressive file: chunks, their interleaving, edit lists and the `moov`.

use crate::bytes::{Wr, fourcc};
use crate::codec;
use crate::demux::Track;
use crate::ts;
use crate::tsfeed::TsReading;
use crate::{Remuxer, Result, bail};

/// Movie timescale of the output, in which edit lists place each track: at 90 kHz a track
/// starts within 6 microseconds of its time, where milliseconds would leave half a millisecond.
const MOVIE_TS: u32 = 90_000;
/// A chunk closes once it spans this much media time, unless the caller sets another span.
const CHUNK_SECS: f64 = 0.5;
/// Tracks of one stream start within seconds of each other; a larger gap means unrelated clocks.
const CLOCK_GAP_SECS: f64 = 60.0;

const UNITY: [u32; 9] = [0x1_0000, 0, 0, 0, 0x1_0000, 0, 0, 0, 0x4000_0000];

/// A byte range of an input file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Range {
    pub file: u32,
    pub offset: u64,
    pub len: u64,
}

#[derive(Debug)]
pub struct TrackReport {
    pub id: u32,
    pub handler: String,
    pub codec: String,
    pub samples: usize,
    /// Presented duration, in seconds.
    pub duration: f64,
    /// Where the track starts in the movie, in seconds.
    pub start: f64,
    /// Gaps and overlaps between fragments that were closed up.
    pub fixes: u32,
    /// Audio frames put back on their grid: the largest correction, in seconds.
    pub snapped: Option<f64>,
}

/// A stretch of the `mdat` payload.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Piece {
    /// Bytes copied from an input file as they are.
    Copy(Range),
    /// Bytes made by unmoof itself: subtitle samples.
    Data(Vec<u8>),
    /// Samples cut out of an MPEG-TS input file: [`Plan::render`] makes the `len` bytes.
    Render { file: u32, len: u64, source: usize, parts: Vec<(usize, u32, u32)> },
}

impl Piece {
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn len(&self) -> u64 {
        match self {
            Piece::Copy(r) => r.len,
            Piece::Data(d) => d.len() as u64,
            Piece::Render { len, .. } => *len,
        }
    }
}

pub struct Plan {
    /// `ftyp`, `moov` and the `mdat` header.
    pub header: Vec<u8>,
    /// The `mdat` payload that follows the header, in this order.
    pub pieces: Vec<Piece>,
    /// The streams of each MPEG-TS source and whether it is packed audio, for [`Plan::render`].
    ts_streams: Vec<TsReading>,
    /// Size of the whole file.
    pub size: u64,
    pub tracks: Vec<TrackReport>,
    pub warnings: Vec<String>,
}

/// Where a track sits on the movie timeline.
struct Layout {
    /// Added to every composition offset so that none is negative.
    shift: i64,
    /// Empty edit before the track, in the movie timescale.
    delay: u64,
    /// First media time presented, in the track timescale, counted from the first sample.
    media_time: i64,
    /// Presented duration, in the movie timescale.
    media_dur: u64,
    /// Sum of the sample durations, in the track timescale.
    total: u64,
}

impl Layout {
    fn edited(&self) -> bool {
        self.delay > 0 || self.media_time != 0
    }

    fn duration(&self, timescale: u32) -> u64 {
        if self.edited() {
            self.delay.saturating_add(self.media_dur)
        } else {
            rescale(self.total, timescale, MOVIE_TS)
        }
    }
}

struct Chunk {
    track: usize,
    count: u32,
    /// For MPEG-TS: index of the first sample among its stream's samples in the file.
    first: u32,
    sdi: u32,
    file: u32,
    off: u64,
    len: u64,
    /// Movie time of its first sample, in seconds, for interleaving.
    time: f64,
}

/// A chunk as the sample tables see it: where it lands in the `mdat` payload.
#[derive(Clone, Copy)]
struct Placed {
    pos: u64,
    count: u32,
    sdi: u32,
}

fn rescale(v: u64, from: u32, to: u32) -> u64 {
    ((v as u128 * to as u128 + from as u128 / 2) / from as u128).min(u64::MAX as u128) as u64
}

impl Remuxer {
    /// Lays out the progressive file from everything fed so far.
    pub fn finish(self) -> Result<Plan> {
        let chunk_secs = self.chunk_secs.unwrap_or(CHUNK_SECS);
        let ts_streams = self.ts_readings();
        let mut warnings = self.warnings;
        let mut tracks = Vec::new();
        let mut snapped = Vec::new();
        let mut all = self.tracks;
        for (si, src) in self.sources.iter().enumerate() {
            let Some(sub) = &src.subs else { continue };
            match subtitle_track(si, sub) {
                Some(t) => all.push(t),
                None => warnings.push(format!("source {si}: the subtitles hold no cue")),
            }
            if sub.kind == crate::subs::Kind::Ass {
                warnings.push(format!(
                    "source {si}: ASS styles, positions and effects cannot be carried in MP4; \
                     the text is kept with bold, italic and underline"
                ));
            }
        }
        for mut t in all {
            let src = &self.sources[t.source];
            if let Some(lang) = src.language {
                t.language = lang;
            }
            if let Some(title) = &src.title {
                t.hdlr = crate::codec::hdlr(&t.handler, title);
                t.title = Some(title.clone());
            }
            if t.samples.is_empty() {
                warnings.push(format!(
                    "track {} ({}) of source {} has no samples and is left out",
                    t.src_id,
                    fourcc(&t.handler),
                    t.source
                ));
            } else {
                if &t.codec == b"hvc1" {
                    for &(i, cut) in &t.ps_cuts {
                        t.samples[i].size -= cut;
                    }
                }
                t.close_ts(&mut warnings);
                snapped.push(t.snap_audio());
                tracks.push(t);
            }
        }
        if tracks.is_empty() {
            bail!("no fragment holds a sample");
        }

        // Each track's first presented sample, in seconds on the timeline the sources share.
        let mut starts = Vec::with_capacity(tracks.len());
        let mut lays = Vec::with_capacity(tracks.len());
        for t in &tracks {
            let ts = t.timescale as i64;
            let dts0 = t.samples[0].dts;
            let (mut min_cto, mut min_pts, mut end, mut total) = (0i64, i64::MAX, i64::MIN, 0u64);
            for s in &t.samples {
                let pts = s.dts + s.cto as i64;
                min_cto = min_cto.min(s.cto as i64);
                min_pts = min_pts.min(pts);
                end = end.max(pts + s.dur as i64);
                total += s.dur as u64;
            }
            // An edit list hides what comes before its media time, like the priming of AAC.
            let (first, start) = match t.edit {
                Some(e) => {
                    let first = min_pts.max(e.media_time);
                    (first, (first - e.media_time) as f64 / ts as f64 + e.delay)
                }
                None => (min_pts, min_pts as f64 / ts as f64),
            };
            starts.push(start - self.sources[t.source].time_offset);
            lays.push(Layout {
                shift: -min_cto,
                delay: 0,
                media_time: first - dts0 - min_cto,
                media_dur: rescale((end - first).max(0) as u64, t.timescale, MOVIE_TS),
                total,
            });
        }
        // Subtitles from a sidecar file start with the movie, wherever its other tracks start.
        let t0 = tracks
            .iter()
            .zip(&starts)
            .filter(|(t, _)| !t.relative)
            .map(|(_, &s)| s)
            .fold(f64::INFINITY, f64::min);
        let t0 = if t0.is_finite() { t0 } else { 0.0 };
        for ((l, s), t) in lays.iter_mut().zip(&starts).zip(&tracks) {
            l.delay = if t.relative { 0 } else { ((s - t0) * MOVIE_TS as f64).round() as u64 };
            if !t.relative && s - t0 > CLOCK_GAP_SECS {
                warnings.push(format!(
                    "track {} of source {} starts {:.0} s after the first track; if its source keeps \
                     another clock, give it a time offset",
                    t.src_id,
                    t.source,
                    s - t0
                ));
            }
        }

        // Chunks: runs of samples that lie back to back in one input file.
        let mut chunks = Vec::new();
        for (ti, t) in tracks.iter().enumerate() {
            let l = &lays[ti];
            let ts = t.timescale as f64;
            let span = ((chunk_secs * ts) as i64).max(1);
            let dts0 = t.samples[0].dts;
            let rendered = t.ts_stream.is_some();
            let mut cur: Option<(Chunk, i64)> = None;
            for s in &t.samples {
                if let Some((c, first_dts)) = cur.as_mut() {
                    let follows = if rendered {
                        c.first as u64 + c.count as u64 == s.off
                    } else {
                        c.off + c.len == s.off
                    };
                    if c.file == s.file && follows && c.sdi == s.sdi && s.dts - *first_dts < span {
                        c.count += 1;
                        c.len += s.size as u64;
                        continue;
                    }
                }
                chunks.extend(cur.take().map(|(c, _)| c));
                let time =
                    (s.dts - dts0 + l.shift - l.media_time) as f64 / ts + l.delay as f64 / MOVIE_TS as f64;
                cur = Some((
                    Chunk {
                        track: ti,
                        count: 1,
                        first: if rendered { s.off as u32 } else { 0 },
                        sdi: s.sdi,
                        file: s.file,
                        off: s.off,
                        len: s.size as u64,
                        time,
                    },
                    s.dts,
                ));
            }
            chunks.extend(cur.map(|(c, _)| c));
        }
        // The sort is stable, so each track keeps its own order.
        chunks.sort_by(|a, b| a.time.total_cmp(&b.time));

        let mut placed: Vec<Vec<Placed>> = vec![Vec::new(); tracks.len()];
        let mut pieces: Vec<Piece> = Vec::new();
        let mut payload = 0u64;
        for c in &chunks {
            placed[c.track].push(Placed { pos: payload, count: c.count, sdi: c.sdi });
            let t = &tracks[c.track];
            if !t.data.is_empty() {
                let bytes = &t.data[c.off as usize..(c.off + c.len) as usize];
                match pieces.last_mut() {
                    Some(Piece::Data(d)) => d.extend_from_slice(bytes),
                    _ => pieces.push(Piece::Data(bytes.to_vec())),
                }
                payload += c.len;
                continue;
            }
            match (t.ts_stream, pieces.last_mut()) {
                (None, Some(Piece::Copy(r))) if r.file == c.file && r.offset + r.len == c.off => {
                    r.len += c.len
                }
                (None, _) => pieces.push(Piece::Copy(Range { file: c.file, offset: c.off, len: c.len })),
                (Some(stream), Some(Piece::Render { file, len, source, parts }))
                    if *file == c.file && *source == t.source =>
                {
                    *len += c.len;
                    match parts.last_mut() {
                        Some((s, first, n)) if *s == stream && *first + *n == c.first => *n += c.count,
                        _ => parts.push((stream, c.first, c.count)),
                    }
                }
                (Some(stream), _) => pieces.push(Piece::Render {
                    file: c.file,
                    len: c.len,
                    source: t.source,
                    parts: vec![(stream, c.first, c.count)],
                }),
            }
            payload += c.len;
        }

        let ftyp = ftyp(&tracks);
        let big_mdat = payload + 8 > u32::MAX as u64;
        let mdat_header = if big_mdat { 16 } else { 8 };
        let mut co64 = false;
        let moov = loop {
            let probe = moov(&tracks, &lays, &placed, 0, co64);
            let base = (ftyp.len() + probe.len() + mdat_header) as u64;
            if !co64 && base + payload > u32::MAX as u64 {
                co64 = true;
                continue;
            }
            break moov(&tracks, &lays, &placed, base, co64);
        };
        let mut w = Wr { v: ftyp };
        w.bytes(&moov);
        if big_mdat {
            w.u32(1);
            w.bytes(b"mdat");
            w.u64(payload + 16);
        } else {
            w.u32(payload as u32 + 8);
            w.bytes(b"mdat");
        }
        let header = w.v;

        let reports = tracks
            .iter()
            .zip(&lays)
            .zip(snapped)
            .enumerate()
            .map(|(i, ((t, l), snapped))| TrackReport {
                id: i as u32 + 1,
                handler: fourcc(&t.handler),
                codec: fourcc(&t.codec),
                samples: t.samples.len(),
                duration: (l.duration(t.timescale) - l.delay) as f64 / MOVIE_TS as f64,
                start: l.delay as f64 / MOVIE_TS as f64,
                fixes: t.fixes,
                snapped,
            })
            .collect();
        Ok(Plan {
            size: header.len() as u64 + payload,
            header,
            pieces,
            ts_streams,
            tracks: reports,
            warnings,
        })
    }
}

/// The text track of a subtitle source: one `tx3g` sample per stretch of screen time.
fn subtitle_track(source: usize, sub: &crate::demux::SubSource) -> Option<Track> {
    let mut cues = sub.cues.clone();
    cues.sort_by_key(|c| (c.start, c.end));
    // Segmented WebVTT repeats a cue in every segment it spans.
    cues.dedup();
    let first = cues.first()?.start;
    let from = if sub.tied { first } else { 0.min(first) };
    let mut t = Track::blank(source);
    t.handler = *b"sbtl";
    t.codec = *b"tx3g";
    t.timescale = 1000;
    t.hdlr = crate::codec::hdlr(b"sbtl", "SubtitleHandler");
    t.media_header = crate::codec::nmhd();
    t.stsd = crate::subs::tx3g_stsd();
    t.relative = !sub.tied;
    for (start, dur, bytes) in crate::subs::samples(&cues, from) {
        t.samples.push(crate::demux::Sample {
            dts: start,
            dur,
            size: bytes.len() as u32,
            cto: 0,
            sync: true,
            sdi: 1,
            file: u32::MAX,
            off: t.data.len() as u64,
        });
        t.data.extend_from_slice(&bytes);
    }
    Some(t)
}

/// True if the sample entry is `mp4a` holding MPEG-1 or MPEG-2 audio (object type 0x6b or 0x69).
fn is_mpeg_audio(t: &Track) -> bool {
    let Some(at) = t.stsd.windows(4).position(|w| w == b"esds") else { return false };
    let d = t.stsd.get(at + 8..).unwrap_or(&[]);
    // A descriptor length takes 1 to 4 bytes, the high bit marking that another follows.
    let after_len = |mut i: usize| {
        while d.get(i).is_some_and(|&b| b & 0x80 != 0) {
            i += 1;
        }
        i + 1
    };
    if d.first() != Some(&0x03) {
        return false;
    }
    // ES_Descriptor (ISO/IEC 14496-1 7.2.6.5): ES_ID, flags, then the optional fields they name.
    let mut i = after_len(1) + 2;
    let Some(&flags) = d.get(i) else { return false };
    i += 1;
    if flags & 0x80 != 0 {
        i += 2;
    }
    if flags & 0x40 != 0 {
        i += 1 + d.get(i).copied().unwrap_or(0) as usize;
    }
    if flags & 0x20 != 0 {
        i += 2;
    }
    d.get(i) == Some(&0x04) && matches!(d.get(after_len(i + 1)), Some(0x6b | 0x69))
}

impl Remuxer {
    /// Lays out a bare MP3 file: the frames of the one MPEG audio track, in order, with no
    /// container around them, which is all an `.mp3` file is.
    pub fn finish_mp3(self) -> Result<Plan> {
        let ts_streams = self.ts_readings();
        let mut warnings = self.warnings;
        let mut found = None;
        for t in self.tracks {
            if t.samples.is_empty() {
                continue;
            }
            if found.is_none() && is_mpeg_audio(&t) {
                found = Some(t);
            } else {
                warnings.push(format!(
                    "track {} ({}) is left out of the MP3 file",
                    t.src_id,
                    fourcc(&t.handler)
                ));
            }
        }
        let Some(t) = found else { bail!("no MP3 (MPEG audio) track to write as .mp3") };
        if t.fixes > 0 {
            warnings.push("an MP3 file keeps no times: gaps between segments are closed up".into());
        }
        let mut pieces: Vec<Piece> = Vec::new();
        let mut size = 0u64;
        for s in &t.samples {
            size += s.size as u64;
            match (t.ts_stream, pieces.last_mut()) {
                (None, Some(Piece::Copy(r))) if r.file == s.file && r.offset + r.len == s.off => {
                    r.len += s.size as u64
                }
                (None, _) => {
                    pieces.push(Piece::Copy(Range { file: s.file, offset: s.off, len: s.size as u64 }))
                }
                (Some(_), Some(Piece::Render { file, len, parts, .. }))
                    if *file == s.file
                        && parts.last().is_some_and(|&(_, f, n)| f as u64 + n as u64 == s.off) =>
                {
                    *len += s.size as u64;
                    parts.last_mut().unwrap().2 += 1;
                }
                (Some(stream), _) => pieces.push(Piece::Render {
                    file: s.file,
                    len: s.size as u64,
                    source: t.source,
                    parts: vec![(stream, s.off as u32, 1)],
                }),
            }
        }
        let total: u64 = t.samples.iter().map(|s| s.dur as u64).sum();
        let report = TrackReport {
            id: 1,
            handler: fourcc(&t.handler),
            codec: "mp3".into(),
            samples: t.samples.len(),
            duration: total as f64 / t.timescale as f64,
            start: 0.0,
            fixes: t.fixes,
            snapped: None,
        };
        Ok(Plan { header: Vec::new(), pieces, ts_streams, size, tracks: vec![report], warnings })
    }
}

impl Plan {
    /// Makes the bytes of a [`Piece::Render`]: its samples, cut out of `data`, the whole TS file
    /// it names, as fed before.
    pub fn render(&self, piece: usize, data: &[u8]) -> Result<Vec<u8>> {
        let Some(Piece::Render { len, source, parts, .. }) = self.pieces.get(piece) else {
            bail!("piece {piece} is not a TS piece");
        };
        let reading = &self.ts_streams[*source];
        let (units, _) = ts::units(data, &reading.pids, reading.packed)?;
        let mut out = Vec::with_capacity(*len as usize);
        for &(stream, first, n) in parts {
            let Some(run) = units.get(stream).and_then(|u| u.get(first as usize..(first + n) as usize))
            else {
                bail!("a TS file holds fewer samples than when it was fed; has it changed?");
            };
            match reading.strip.get(stream).and_then(Option::as_ref) {
                Some(known) => {
                    run.iter().for_each(|u| out.extend(codec::strip_parameter_sets(&u.data, known)))
                }
                None => run.iter().for_each(|u| out.extend_from_slice(&u.data)),
            }
        }
        if out.len() as u64 != *len {
            bail!("a TS file gives {} bytes of samples where {len} were planned; has it changed?", out.len());
        }
        Ok(out)
    }
}

fn ftyp(tracks: &[Track]) -> Vec<u8> {
    let mut w = Wr::default();
    let at = w.open(b"ftyp");
    w.bytes(b"isom");
    w.u32(0x200);
    w.bytes(b"isomiso2");
    if tracks.iter().any(|t| matches!(&t.codec, b"avc1" | b"avc3")) {
        w.bytes(b"avc1");
    }
    w.bytes(b"mp41");
    w.close(at);
    w.v
}

/// Run-length encoding: (count, value) for each run of equal values.
fn runs<T: PartialEq + Copy>(it: impl Iterator<Item = T>) -> Vec<(u32, T)> {
    let mut out: Vec<(u32, T)> = Vec::new();
    for x in it {
        match out.last_mut() {
            Some((n, v)) if *v == x => *n += 1,
            _ => out.push((1, x)),
        }
    }
    out
}

fn moov(tracks: &[Track], lays: &[Layout], placed: &[Vec<Placed>], base: u64, co64: bool) -> Vec<u8> {
    let mut w = Wr::default();
    let moov = w.open(b"moov");

    let movie_dur = tracks.iter().zip(lays).map(|(t, l)| l.duration(t.timescale)).max().unwrap_or(0);
    let v = (movie_dur > u32::MAX as u64) as u8;
    let at = w.open_full(b"mvhd", v, 0);
    times(&mut w, v);
    w.u32(MOVIE_TS);
    dur(&mut w, v, movie_dur);
    w.u32(0x1_0000); // rate 1.0
    w.u16(0x100); // volume 1.0
    w.zeros(10);
    UNITY.iter().for_each(|&m| w.u32(m));
    w.zeros(24);
    w.u32(tracks.len() as u32 + 1);
    w.close(at);

    for (i, ((t, l), chunks)) in tracks.iter().zip(lays).zip(placed).enumerate() {
        // Tracks of one kind are alternatives, as dubs are: only the first is enabled, and
        // they share an alternate group, so no player mixes two dubs or shows two pictures.
        let same: Vec<usize> = (0..tracks.len()).filter(|&j| tracks[j].handler == t.handler).collect();
        let enabled = same[0] == i;
        let group = if same.len() > 1 {
            match &t.handler {
                b"soun" => 1,
                b"vide" => 2,
                _ => 3,
            }
        } else {
            t.alt_group
        };
        trak(&mut w, i as u32 + 1, t, l, chunks, base, co64, enabled, group);
    }
    w.close(moov);
    w.v
}

fn times(w: &mut Wr, v: u8) {
    if v == 1 { w.zeros(16) } else { w.zeros(8) }
}

fn dur(w: &mut Wr, v: u8, d: u64) {
    if v == 1 { w.u64(d) } else { w.u32(d as u32) }
}

#[allow(clippy::too_many_arguments)]
fn trak(
    w: &mut Wr,
    id: u32,
    t: &Track,
    l: &Layout,
    chunks: &[Placed],
    base: u64,
    co64: bool,
    enabled: bool,
    group: i16,
) {
    let trak = w.open(b"trak");

    let track_dur = l.duration(t.timescale);
    let v = (track_dur > u32::MAX as u64) as u8;
    let at = w.open_full(b"tkhd", v, if enabled { 3 } else { 2 }); // enabled, in movie
    times(w, v);
    w.u32(id);
    w.u32(0);
    dur(w, v, track_dur);
    w.zeros(8);
    w.i16(t.layer);
    w.i16(group);
    let audio = &t.handler == b"soun";
    w.u16(if audio && t.volume == 0 {
        0x100
    } else if audio {
        t.volume
    } else {
        0
    });
    w.u16(0);
    if t.matrix.iter().all(|&b| b == 0) {
        UNITY.iter().for_each(|&m| w.u32(m));
    } else {
        w.bytes(&t.matrix);
    }
    w.u32(t.width);
    w.u32(t.height);
    w.close(at);

    if l.edited() {
        let edts = w.open(b"edts");
        let v = (l.delay > u32::MAX as u64 || l.media_dur > u32::MAX as u64 || l.media_time > i32::MAX as i64)
            as u8;
        let at = w.open_full(b"elst", v, 0);
        w.u32(if l.delay > 0 { 2 } else { 1 });
        let mut entry = |d: u64, m: i64| {
            if v == 1 {
                w.u64(d);
                w.i64(m);
            } else {
                w.u32(d as u32);
                w.i32(m as i32);
            }
            w.u32(0x1_0000); // rate 1.0
        };
        if l.delay > 0 {
            entry(l.delay, -1);
        }
        entry(l.media_dur, l.media_time);
        w.close(at);
        w.close(edts);
    }

    let mdia = w.open(b"mdia");
    let v = (l.total > u32::MAX as u64) as u8;
    let at = w.open_full(b"mdhd", v, 0);
    times(w, v);
    w.u32(t.timescale);
    dur(w, v, l.total);
    w.u16(t.language);
    w.u16(0);
    w.close(at);
    w.bytes(&t.hdlr);

    let minf = w.open(b"minf");
    w.bytes(&t.media_header);
    let dinf = w.open(b"dinf");
    let dref = w.open_full(b"dref", 0, 0);
    w.u32(1);
    let url = w.open_full(b"url ", 0, 1); // data in this file
    w.close(url);
    w.close(dref);
    w.close(dinf);
    stbl(w, t, l, chunks, base, co64);
    w.close(minf);
    w.close(mdia);
    if let Some(title) = &t.title {
        // The QuickTime title string, which ffmpeg (and so mpv) and Apple players read.
        let udta = w.open(b"udta");
        let name = w.open(b"\xa9nam");
        w.u16(title.len() as u16);
        w.u16(0x15c7); // the string's language, "eng" packed as ISO 639-2
        w.bytes(title.as_bytes());
        w.close(name);
        w.close(udta);
    }
    w.close(trak);
}

fn stbl(w: &mut Wr, t: &Track, l: &Layout, chunks: &[Placed], base: u64, co64: bool) {
    let s = &t.samples;
    let stbl = w.open(b"stbl");
    w.bytes(&t.stsd);

    let stts = runs(s.iter().map(|x| x.dur));
    let at = w.open_full(b"stts", 0, 0);
    w.u32(stts.len() as u32);
    for (n, d) in stts {
        w.u32(n);
        w.u32(d);
    }
    w.close(at);

    if s.iter().any(|x| x.cto as i64 + l.shift != 0) {
        let ctts = runs(s.iter().map(|x| (x.cto as i64 + l.shift) as u32));
        let at = w.open_full(b"ctts", 0, 0);
        w.u32(ctts.len() as u32);
        for (n, c) in ctts {
            w.u32(n);
            w.u32(c);
        }
        w.close(at);
    }

    if s.iter().any(|x| !x.sync) {
        let at = w.open_full(b"stss", 0, 0);
        w.u32(s.iter().filter(|x| x.sync).count() as u32);
        for (i, _) in s.iter().enumerate().filter(|(_, x)| x.sync) {
            w.u32(i as u32 + 1);
        }
        w.close(at);
    }

    let at = w.open_full(b"stsc", 0, 0);
    let stsc: Vec<(u32, Placed)> = {
        let mut out: Vec<(u32, Placed)> = Vec::new();
        for (i, c) in chunks.iter().enumerate() {
            match out.last() {
                Some((_, p)) if p.count == c.count && p.sdi == c.sdi => {}
                _ => out.push((i as u32 + 1, *c)),
            }
        }
        out
    };
    w.u32(stsc.len() as u32);
    for (first, c) in stsc {
        w.u32(first);
        w.u32(c.count);
        w.u32(c.sdi);
    }
    w.close(at);

    let at = w.open_full(b"stsz", 0, 0);
    if s.iter().all(|x| x.size == s[0].size) {
        w.u32(s[0].size);
        w.u32(s.len() as u32);
    } else {
        w.u32(0);
        w.u32(s.len() as u32);
        s.iter().for_each(|x| w.u32(x.size));
    }
    w.close(at);

    let at = w.open_full(if co64 { b"co64" } else { b"stco" }, 0, 0);
    w.u32(chunks.len() as u32);
    for c in chunks {
        if co64 { w.u64(base + c.pos) } else { w.u32((base + c.pos) as u32) }
    }
    w.close(at);
    w.close(stbl);
}
