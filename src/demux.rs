//! Reading init segments (`moov`) and fragments (`moof`) into tracks and samples.

use crate::bytes::{Rd, boxes, fourcc};
use crate::{Remuxer, Result, bail};

/// More samples than this in one `trun` is a damaged box, not a stream.
const MAX_TRUN_SAMPLES: usize = 1 << 20;
/// Over 150 hours of 60 fps video: beyond this a track is damaged, and it would only eat memory.
pub const MAX_TRACK_SAMPLES: usize = 1 << 25;

#[derive(Default)]
pub struct Source {
    /// The init segment as first seen, so a repeat of it can be told from a different one.
    pub moov: Option<Vec<u8>>,
    /// Source track ID → index into `Remuxer::tracks`.
    tracks: Vec<(u32, usize)>,
    /// Set when the source is MPEG-TS.
    pub ts: Option<crate::tsfeed::TsState>,
    /// Language and title the caller knows from the playlist, for every track of the source.
    pub language: Option<u16>,
    pub title: Option<String>,
    /// Seconds the caller's manifest moves the source earlier: DASH `presentationTimeOffset`.
    pub time_offset: f64,
    /// Set when the source is subtitles.
    pub subs: Option<SubSource>,
}

pub struct SubSource {
    pub kind: crate::subs::Kind,
    pub cues: Vec<crate::subs::Cue>,
    /// Times follow the stream's clock (HLS WebVTT) rather than starting with the movie.
    pub tied: bool,
}

#[derive(Clone, Copy, Default)]
pub struct Trex {
    sdi: u32,
    dur: u32,
    size: u32,
    flags: u32,
}

/// The edit list of a source track, as far as it shifts or delays the track.
#[derive(Clone, Copy)]
pub struct Edit {
    /// Empty edits before the media, in seconds.
    pub delay: f64,
    /// Media time presented first, in the track timescale.
    pub media_time: i64,
}

#[derive(Clone, Copy)]
pub struct Sample {
    /// Decode time in the track timescale, as the fragments state it.
    pub dts: i64,
    pub dur: u32,
    pub size: u32,
    /// Composition offset, signed as version 1 of `trun` allows.
    pub cto: i32,
    pub sync: bool,
    pub sdi: u32,
    pub file: u32,
    pub off: u64,
}

pub struct Track {
    pub source: usize,
    pub src_id: u32,
    pub handler: [u8; 4],
    pub codec: [u8; 4],
    pub timescale: u32,
    pub language: u16,
    pub layer: i16,
    pub alt_group: i16,
    pub volume: u16,
    pub matrix: [u8; 36],
    pub width: u32,
    pub height: u32,
    /// Raw boxes carried over unchanged.
    pub hdlr: Vec<u8>,
    pub media_header: Vec<u8>,
    pub stsd: Vec<u8>,
    pub edit: Option<Edit>,
    trex: Trex,
    pub samples: Vec<Sample>,
    /// Decode time the next sample takes unless a `tfdt` says otherwise.
    next_dts: i64,
    /// Small gaps and overlaps between fragments that were closed up.
    pub fixes: u32,
    /// For MPEG-TS input: which stream of the source it is. Its samples are cut out of the TS
    /// files again when the output is written, and `Sample::off` counts samples, not bytes.
    pub ts_stream: Option<usize>,
    /// A name for players to show, such as the studio of a dub.
    pub title: Option<String>,
    /// Sample bytes made here rather than found in an input file, for subtitles. Their samples
    /// name file `u32::MAX`, and `Sample::off` is an offset into this.
    pub data: Vec<u8>,
    /// Times count from the start of the movie, not from a clock the other tracks share.
    pub relative: bool,
}

impl Track {
    pub fn blank(source: usize) -> Track {
        Track {
            source,
            src_id: 0,
            handler: *b"    ",
            codec: *b"    ",
            timescale: 0,
            language: 0x55c4, // "und"
            layer: 0,
            alt_group: 0,
            volume: 0,
            matrix: [0; 36],
            width: 0,
            height: 0,
            hdlr: Vec::new(),
            media_header: Vec::new(),
            stsd: Vec::new(),
            edit: None,
            trex: Trex::default(),
            samples: Vec::new(),
            next_dts: 0,
            fixes: 0,
            ts_stream: None,
            title: None,
            data: Vec::new(),
            relative: false,
        }
    }

    fn is_audio(&self) -> bool {
        &self.handler == b"soun"
    }

    /// Puts the frames of a fixed-frame audio codec back on their grid: packagers that came from
    /// MPEG-TS round frame times, and strict players stutter on the wobble. A step of more than
    /// half a frame is a real gap. Returns the largest correction in seconds, if any.
    pub fn snap_audio(&mut self) -> Option<f64> {
        if !self.is_audio() || !matches!(&self.codec, b"mp4a" | b"ac-3" | b"ec-3") || self.samples.len() < 2 {
            return None;
        }
        let mut counts: Vec<(u32, usize)> = Vec::new();
        for s in &self.samples {
            match counts.iter_mut().find(|(d, _)| *d == s.dur) {
                Some((_, n)) => *n += 1,
                None => counts.push((s.dur, 1)),
            }
        }
        let frame = counts.iter().max_by_key(|(_, n)| *n).unwrap().0 as i64;
        if counts.len() == 1 || frame == 0 {
            return None;
        }
        // Wobble keeps nearly every duration within a tenth of the frame; the few beyond are gaps.
        let close = self.samples.iter().filter(|s| (s.dur as i64 - frame).abs() * 10 <= frame).count();
        if close * 10 < self.samples.len() * 9 {
            return None;
        }
        let (mut anchor, mut k, mut worst, mut prev) = (self.samples[0].dts, 0i64, 0i64, i64::MIN);
        let mut grid = Vec::with_capacity(self.samples.len());
        for s in &self.samples {
            let mut g = anchor + k * frame;
            if (s.dts - g).abs() * 2 > frame && s.dts > prev {
                anchor = s.dts;
                k = 0;
                g = anchor;
            }
            worst = worst.max((s.dts - g).abs());
            grid.push(g);
            prev = g;
            k += 1;
        }
        if worst == 0 {
            return None;
        }
        // The last frame keeps its own duration: the end of a stream may cut it short.
        let last = self.samples.len() - 1;
        for i in 0..last {
            self.samples[i].dts = grid[i];
            self.samples[i].dur = (grid[i + 1] - grid[i]).min(i64::from(u32::MAX)) as u32;
        }
        self.samples[last].dts = grid[last];
        Some(worst as f64 / self.timescale as f64)
    }
}

impl Remuxer {
    pub(crate) fn read_moov(&mut self, source: usize, payload: &[u8], raw: &[u8]) -> Result<()> {
        if self.sources[source].ts.is_some() {
            bail!("source {source} mixes MPEG-TS and fMP4 files");
        }
        if let Some(prev) = &self.sources[source].moov {
            if prev.as_slice() == raw {
                return Ok(());
            }
            bail!(
                "source {source} has a second init segment that differs from the first; \
                 a quality switch inside one source is not supported"
            );
        }
        self.sources[source].moov = Some(raw.to_vec());

        let mut movie_timescale = 1000;
        let mut traks = Vec::new();
        let mut trexs = Vec::new();
        for b in boxes(payload) {
            let b = b?;
            match &b.typ {
                b"mvhd" => {
                    let mut r = Rd::new(b.payload);
                    let (v, _) = r.full()?;
                    r.skip(if v == 1 { 16 } else { 8 })?;
                    movie_timescale = r.u32()?.max(1);
                }
                b"trak" => traks.push(b.payload),
                b"mvex" => {
                    for e in boxes(b.payload) {
                        let e = e?;
                        if &e.typ == b"trex" {
                            let mut r = Rd::new(e.payload);
                            r.full()?;
                            let id = r.u32()?;
                            let t = Trex { sdi: r.u32()?, dur: r.u32()?, size: r.u32()?, flags: r.u32()? };
                            trexs.push((id, t));
                        }
                    }
                }
                _ => {}
            }
        }
        if traks.is_empty() {
            bail!("source {source}: the init segment declares no track");
        }
        for t in traks {
            let mut track = self.read_trak(source, t, movie_timescale)?;
            if let Some((_, trex)) = trexs.iter().find(|(id, _)| *id == track.src_id) {
                track.trex = *trex;
            }
            self.sources[source].tracks.push((track.src_id, self.tracks.len()));
            self.tracks.push(track);
        }
        Ok(())
    }

    fn read_trak(&mut self, source: usize, payload: &[u8], movie_timescale: u32) -> Result<Track> {
        let mut t = Track::blank(source);
        for b in boxes(payload) {
            let b = b?;
            match &b.typ {
                b"tkhd" => {
                    let mut r = Rd::new(b.payload);
                    let (v, _) = r.full()?;
                    r.skip(if v == 1 { 16 } else { 8 })?;
                    t.src_id = r.u32()?;
                    r.skip(4 + if v == 1 { 8 } else { 4 } + 8)?;
                    t.layer = r.i16()?;
                    t.alt_group = r.i16()?;
                    t.volume = r.u16()?;
                    r.skip(2)?;
                    t.matrix = r.take(36)?.try_into().unwrap();
                    t.width = r.u32()?;
                    t.height = r.u32()?;
                }
                b"edts" => {
                    for e in boxes(b.payload) {
                        let e = e?;
                        if &e.typ == b"elst" {
                            t.edit = read_elst(e.payload, movie_timescale, &mut self.warnings)?;
                        }
                    }
                }
                b"mdia" => self.read_mdia(&mut t, b.payload)?,
                _ => {}
            }
        }
        if t.timescale == 0 {
            bail!("track {} has no media timescale", t.src_id);
        }
        if t.stsd.is_empty() {
            bail!("track {} has no sample description", t.src_id);
        }
        if &t.codec == b"encv" || &t.codec == b"enca" {
            bail!("track {} is encrypted; DRM-protected streams are not supported", t.src_id);
        }
        if &t.handler == b"vide"
            && (t.width == 0 || t.height == 0)
            && let Some((w, h)) = visual_size(&t.stsd)
        {
            t.width = (w as u32) << 16;
            t.height = (h as u32) << 16;
        }
        Ok(t)
    }

    fn read_mdia(&mut self, t: &mut Track, payload: &[u8]) -> Result<()> {
        for b in boxes(payload) {
            let b = b?;
            match &b.typ {
                b"mdhd" => {
                    let mut r = Rd::new(b.payload);
                    let (v, _) = r.full()?;
                    r.skip(if v == 1 { 16 } else { 8 })?;
                    t.timescale = r.u32()?;
                    r.skip(if v == 1 { 8 } else { 4 })?;
                    t.language = r.u16()?;
                }
                b"hdlr" => {
                    let mut r = Rd::new(b.payload);
                    r.skip(8)?;
                    t.handler = r.take(4)?.try_into().unwrap();
                    t.hdlr = b.raw.to_vec();
                }
                b"minf" => {
                    for m in boxes(b.payload) {
                        let m = m?;
                        match &m.typ {
                            b"vmhd" | b"smhd" | b"sthd" | b"nmhd" | b"hmhd" => {
                                t.media_header = m.raw.to_vec()
                            }
                            b"stbl" => {
                                for s in boxes(m.payload) {
                                    let s = s?;
                                    match &s.typ {
                                        b"stsd" => {
                                            t.stsd = s.raw.to_vec();
                                            let mut r = Rd::new(s.payload);
                                            r.skip(8 + 4)?;
                                            t.codec = r.take(4)?.try_into().unwrap();
                                        }
                                        b"stsz" => {
                                            let mut r = Rd::new(s.payload);
                                            r.skip(8)?;
                                            let n = r.u32()?;
                                            if n > 0 {
                                                self.warnings.push(format!(
                                                    "the init segment already lists {n} samples of a track; \
                                                     only the fragments are used"
                                                ));
                                            }
                                        }
                                        _ => {}
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }

    pub(crate) fn read_moof(&mut self, source: usize, file: u32, moof_at: u64, payload: &[u8]) -> Result<()> {
        if self.sources[source].moov.is_none() {
            bail!("source {source}: a fragment comes before the init segment");
        }
        // Where the data of the previous traf ended, the base of a traf that names none.
        let mut prev_end = moof_at;
        for b in boxes(payload) {
            let b = b?;
            if &b.typ != b"traf" {
                continue;
            }
            prev_end = self.read_traf(source, file, moof_at, prev_end, b.payload)?;
        }
        Ok(())
    }

    fn read_traf(
        &mut self,
        source: usize,
        file: u32,
        moof_at: u64,
        prev_end: u64,
        payload: &[u8],
    ) -> Result<u64> {
        let mut it = boxes(payload);
        let tfhd = match it.next() {
            Some(b) => b?,
            None => bail!("an empty traf"),
        };
        if &tfhd.typ != b"tfhd" {
            bail!("a traf does not start with tfhd but with '{}'", fourcc(&tfhd.typ));
        }
        let mut r = Rd::new(tfhd.payload);
        let (_, fl) = r.full()?;
        let id = r.u32()?;
        let Some(&(_, ti)) = self.sources[source].tracks.iter().find(|(s, _)| *s == id) else {
            self.warnings.push(format!("source {source}: a fragment of undeclared track {id} is skipped"));
            return Ok(prev_end);
        };
        let trex = self.tracks[ti].trex;
        let base = if fl & 0x01 != 0 {
            r.u64()?
        } else if fl & 0x02_0000 != 0 {
            moof_at
        } else {
            prev_end
        };
        let sdi = if fl & 0x02 != 0 { r.u32()? } else { trex.sdi }.max(1);
        let def_dur = if fl & 0x08 != 0 { r.u32()? } else { trex.dur };
        let def_size = if fl & 0x10 != 0 { r.u32()? } else { trex.size };
        let def_flags = if fl & 0x20 != 0 { r.u32()? } else { trex.flags };

        let audio = self.tracks[ti].is_audio();
        let limit = self.file_sizes.get(file as usize).copied().flatten();
        let mut cursor = base;
        let mut first_trun = true;
        for b in it {
            let b = b?;
            match &b.typ {
                b"tfdt" => {
                    let mut r = Rd::new(b.payload);
                    let (v, _) = r.full()?;
                    // 2^62 ticks is millennia at any timescale; above it the box is damaged, and
                    // capping it keeps all timeline arithmetic inside i64.
                    let t = if v == 1 { r.u64()? } else { u64::from(r.u32()?) }.min(1 << 62) as i64;
                    resync(&mut self.tracks[ti], t, &mut self.warnings);
                }
                b"trun" => {
                    let mut r = Rd::new(b.payload);
                    let (v, tf) = r.full()?;
                    let n = r.u32()? as usize;
                    if tf & 0x01 != 0 {
                        let Some(c) = base.checked_add_signed(r.i32()? as i64) else {
                            bail!("a trun points its data before the start of the file");
                        };
                        cursor = c;
                    } else if first_trun {
                        cursor = base;
                    }
                    first_trun = false;
                    let first_flags = if tf & 0x04 != 0 { Some(r.u32()?) } else { None };
                    let per = [0x100, 0x200, 0x400, 0x800].iter().filter(|&&f| tf & f != 0).count() * 4;
                    if n > MAX_TRUN_SAMPLES || r.left() < n * per {
                        bail!("a trun lists {n} samples but holds the fields of fewer");
                    }
                    let track = &mut self.tracks[ti];
                    if track.samples.len() + n > MAX_TRACK_SAMPLES {
                        bail!(
                            "track {} has more than {MAX_TRACK_SAMPLES} samples; the stream is damaged",
                            track.src_id
                        );
                    }
                    track.samples.reserve(n);
                    for i in 0..n {
                        let dur = if tf & 0x100 != 0 { r.u32()? } else { def_dur };
                        let size = if tf & 0x200 != 0 { r.u32()? } else { def_size };
                        let flags = if tf & 0x400 != 0 {
                            r.u32()?
                        } else if i == 0 && first_flags.is_some() {
                            first_flags.unwrap()
                        } else {
                            def_flags
                        };
                        let cto = if tf & 0x800 != 0 {
                            let c = r.u32()?;
                            if v == 0 { c.min(i32::MAX as u32) as i32 } else { c as i32 }
                        } else {
                            0
                        };
                        track.samples.push(Sample {
                            dts: track.next_dts,
                            dur,
                            size,
                            cto,
                            // Every audio frame of the codecs carried here decodes alone.
                            sync: audio || flags & 0x1_0000 == 0,
                            sdi,
                            file,
                            off: cursor,
                        });
                        track.next_dts += dur as i64;
                        let Some(c) = cursor.checked_add(size as u64) else {
                            bail!("a trun points its data past any file's end");
                        };
                        if limit.is_some_and(|l| c > l) {
                            bail!("a trun points its data past the end of its {}-byte file", limit.unwrap());
                        }
                        cursor = c;
                    }
                }
                _ => {}
            }
        }
        Ok(cursor)
    }
}

/// Moves a track to the decode time a `tfdt` states, closing the gap or overlap the previous
/// fragment left by stretching or shortening its last sample.
fn resync(t: &mut Track, dts: i64, warnings: &mut Vec<String>) {
    let expected = t.next_dts;
    if t.samples.is_empty() {
        t.next_dts = dts;
        return;
    }
    if dts == expected {
        return;
    }
    let at = (t.samples[t.samples.len() - 1].dts - t.samples[0].dts) as f64 / t.timescale as f64;
    let last = t.samples.last_mut().unwrap();
    let diff = dts - expected;
    if diff > 0 {
        if diff > t.timescale as i64 / 2 {
            warnings.push(format!(
                "track {}: {:.3} s of media missing after {at:.3} s; the frame before the gap is held",
                t.src_id,
                diff as f64 / t.timescale as f64
            ));
        }
        last.dur = (last.dur as i64 + diff).min(u32::MAX as i64) as u32;
    } else if -diff < last.dur as i64 {
        last.dur = (last.dur as i64 + diff) as u32;
    } else {
        warnings.push(format!(
            "track {}: a fragment at {at:.3} s starts {:.3} s before the previous one ends; its time is ignored",
            t.src_id,
            -diff as f64 / t.timescale as f64
        ));
        return;
    }
    t.fixes += 1;
    t.next_dts = dts;
}

fn read_elst(payload: &[u8], movie_timescale: u32, warnings: &mut Vec<String>) -> Result<Option<Edit>> {
    let mut r = Rd::new(payload);
    let (v, _) = r.full()?;
    let n = r.u32()?;
    let mut delay = 0u64;
    let mut edit = None;
    for _ in 0..n {
        let (dur, media_time) =
            if v == 1 { (r.u64()?, r.i64()?) } else { (r.u32()? as u64, r.i32()? as i64) };
        r.skip(4)?;
        if media_time == -1 {
            if edit.is_none() {
                delay += dur;
            }
        } else if edit.is_none() {
            edit = Some(media_time);
        } else {
            warnings.push("an edit list with more than one media edit; only the first is kept".into());
            break;
        }
    }
    Ok(edit.map(|media_time| Edit { delay: delay as f64 / movie_timescale as f64, media_time }))
}

/// Width and height of the first visual sample entry of an `stsd`.
fn visual_size(stsd: &[u8]) -> Option<(u16, u16)> {
    // stsd: header 8, version/flags 4, count 4; entry: header 8, then 24 bytes before width.
    let at = 16 + 8 + 24;
    let b = stsd.get(at..at + 4)?;
    Some((u16::from_be_bytes([b[0], b[1]]), u16::from_be_bytes([b[2], b[3]])))
}
