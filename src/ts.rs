//! MPEG-TS input (H.264, AAC, MPEG audio) and HLS packed audio (bare frames behind an ID3 tag).
//!
//! TS samples lie scattered over 188-byte packets, so a file is read twice: once for the sample
//! tables, once more to cut the samples out ([`crate::Plan::render`]). Both readings go through
//! [`units`], so they agree on every sample.

use crate::codec::{self, Adts, Mpa};
use crate::{Result, bail};

/// Stream types read from the PMT.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Es {
    H264,
    Aac,
    /// MP1, MP2 or MP3.
    Mpa,
}

impl Es {
    fn from_stream_type(t: u8) -> Option<Es> {
        match t {
            0x1b => Some(Es::H264),
            0x0f => Some(Es::Aac),
            0x03 | 0x04 => Some(Es::Mpa),
            _ => None,
        }
    }

    fn stream_type(self) -> u8 {
        match self {
            Es::H264 => 0x1b,
            Es::Aac => 0x0f,
            Es::Mpa => 0x03,
        }
    }
}

/// One sample as the MP4 holds it.
pub struct Unit {
    pub pts: Option<u64>,
    pub dts: Option<u64>,
    /// Index among the frames that start in the same PES, for AAC.
    pub frame_in_pes: u32,
    pub data: Vec<u8>,
    pub sync: bool,
    pub adts: Option<Adts>,
    pub mpa: Option<Mpa>,
}

/// Where the 188-byte packets start: the first sync byte that two more follow at the packet
/// pitch. Junk in front, such as the fake PNG headers some CDNs put on segments, is skipped.
pub fn sync_offset(b: &[u8]) -> Option<usize> {
    let limit = b.len().min(4 << 20);
    (0..limit).find(|&i| {
        b[i] == 0x47 && (1..3).all(|k| b.get(i + k * 188).is_none_or(|&x| x == 0x47)) && i + 188 <= b.len()
    })
}

/// True if `b` looks like MPEG-TS, possibly behind junk.
pub fn sniff(b: &[u8]) -> bool {
    sync_offset(b).is_some_and(|i| b.len() >= i + 188 * 2 || b.len() == i + 188)
}

struct Pes {
    pid: u16,
    pts: Option<u64>,
    dts: Option<u64>,
    payload: Vec<u8>,
}

/// Streams of the program and the PES of the ones read, in file order.
struct Demuxed {
    streams: Vec<(u8, u16)>,
    pes: Vec<Pes>,
    broken: usize,
}

fn read_ts(b: &[u8], known: &[(u8, u16)]) -> Result<Demuxed> {
    let Some(start) = sync_offset(b) else { bail!("no MPEG-TS packets found") };
    let mut streams: Vec<(u8, u16)> = known.to_vec();
    let mut pmt_pid: Option<u16> = None;
    let mut bufs: Vec<(u16, Vec<u8>)> = Vec::new();
    let mut psi: Vec<(u16, Vec<u8>)> = Vec::new();
    let mut out = Demuxed { streams: Vec::new(), pes: Vec::new(), broken: 0 };

    let mut p = start;
    while p + 188 <= b.len() {
        let pkt = &b[p..p + 188];
        p += 188;
        if pkt[0] != 0x47 {
            // Lost sync: find it again rather than give up on the rest of the file.
            match sync_offset(&b[p - 188..]) {
                Some(o) => {
                    p = p - 188 + o;
                    out.broken += 1;
                    continue;
                }
                None => break,
            }
        }
        if pkt[1] & 0x80 != 0 {
            out.broken += 1;
            continue;
        }
        let pusi = pkt[1] & 0x40 != 0;
        let pid = ((pkt[1] as u16 & 0x1f) << 8) | pkt[2] as u16;
        let afc = (pkt[3] >> 4) & 3;
        if afc & 1 == 0 {
            continue;
        }
        let mut at = 4;
        if afc & 2 != 0 {
            at += 1 + pkt[4] as usize;
        }
        if at >= 188 {
            continue;
        }
        let payload = &pkt[at..];

        if pid == 0 || Some(pid) == pmt_pid {
            if let Some(section) = gather_psi(&mut psi, pid, pusi, payload) {
                if pid == 0 {
                    pmt_pid = read_pat(&section).or(pmt_pid);
                } else if let Some(s) = read_pmt(&section) {
                    for (t, pid) in s {
                        if !streams.iter().any(|&(_, p)| p == pid) {
                            streams.push((t, pid));
                        }
                    }
                }
            }
            continue;
        }
        if !streams.iter().any(|&(t, sp)| sp == pid && Es::from_stream_type(t).is_some()) {
            continue;
        }
        let slot = match bufs.iter().position(|(bp, _)| *bp == pid) {
            Some(i) => i,
            None => {
                bufs.push((pid, Vec::new()));
                bufs.len() - 1
            }
        };
        if pusi {
            let done = std::mem::take(&mut bufs[slot].1);
            if let Some(pes) = parse_pes(pid, &done) {
                out.pes.push(pes);
            }
            bufs[slot].1.extend_from_slice(payload);
        } else if !bufs[slot].1.is_empty() {
            // Payload before the first start of a PES in this file belongs to a PES cut off
            // by the previous file, and is dropped.
            bufs[slot].1.extend_from_slice(payload);
        }
    }
    for (pid, buf) in bufs {
        if let Some(pes) = parse_pes(pid, &buf) {
            out.pes.push(pes);
        }
    }
    out.streams = streams;
    Ok(out)
}

/// Collects a PSI section over packets; returns it once complete.
fn gather_psi(psi: &mut Vec<(u16, Vec<u8>)>, pid: u16, pusi: bool, payload: &[u8]) -> Option<Vec<u8>> {
    let i = match psi.iter().position(|(p, _)| *p == pid) {
        Some(i) => i,
        None => {
            psi.push((pid, Vec::new()));
            psi.len() - 1
        }
    };
    let buf = &mut psi[i].1;
    if pusi {
        let pointer = *payload.first()? as usize;
        buf.clear();
        buf.extend_from_slice(payload.get(1 + pointer..)?);
    } else if !buf.is_empty() {
        buf.extend_from_slice(payload);
    } else {
        return None;
    }
    if buf.len() < 3 {
        return None;
    }
    let len = 3 + (((buf[1] as usize & 0x0f) << 8) | buf[2] as usize);
    if buf.len() < len {
        return None;
    }
    let section = buf[..len].to_vec();
    buf.clear();
    Some(section)
}

fn read_pat(s: &[u8]) -> Option<u16> {
    if s.first() != Some(&0) || s.len() < 12 {
        return None;
    }
    let end = s.len() - 4;
    let mut i = 8;
    while i + 4 <= end {
        let program = u16::from_be_bytes([s[i], s[i + 1]]);
        let pid = ((s[i + 2] as u16 & 0x1f) << 8) | s[i + 3] as u16;
        if program != 0 {
            return Some(pid);
        }
        i += 4;
    }
    None
}

fn read_pmt(s: &[u8]) -> Option<Vec<(u8, u16)>> {
    if s.first() != Some(&2) || s.len() < 16 {
        return None;
    }
    let end = s.len() - 4;
    let info = ((s[10] as usize & 0x0f) << 8) | s[11] as usize;
    let mut i = 12 + info;
    let mut out = Vec::new();
    while i + 5 <= end {
        let t = s[i];
        let pid = ((s[i + 1] as u16 & 0x1f) << 8) | s[i + 2] as u16;
        let es_info = ((s[i + 3] as usize & 0x0f) << 8) | s[i + 4] as usize;
        out.push((t, pid));
        i += 5 + es_info;
    }
    Some(out)
}

fn ts_time(b: &[u8]) -> u64 {
    ((b[0] as u64 >> 1) & 7) << 30
        | (b[1] as u64) << 22
        | (b[2] as u64 >> 1) << 15
        | (b[3] as u64) << 7
        | b[4] as u64 >> 1
}

fn parse_pes(pid: u16, b: &[u8]) -> Option<Pes> {
    if b.len() < 9 || b[0..3] != [0, 0, 1] {
        return None;
    }
    let declared = u16::from_be_bytes([b[4], b[5]]) as usize;
    let end = if declared == 0 { b.len() } else { (6 + declared).min(b.len()) };
    let flags = b[7];
    let hdr = 9 + b[8] as usize;
    if hdr > end {
        return None;
    }
    let pts = (flags & 0x80 != 0 && b.len() >= 14).then(|| ts_time(&b[9..14]));
    let dts = (flags & 0xc0 == 0xc0 && b.len() >= 19).then(|| ts_time(&b[14..19]));
    Some(Pes { pid, pts, dts, payload: b[hdr..end].to_vec() })
}

/// Streams by kind and PID.
pub type Pids = Vec<(Es, u16)>;

/// The program's streams that unmoof reads, in PMT order, and the types of those it leaves out.
pub fn streams(b: &[u8]) -> Result<(Pids, Vec<u8>)> {
    let d = read_ts(b, &[])?;
    let read = d.streams.iter().filter_map(|&(t, pid)| Es::from_stream_type(t).map(|e| (e, pid))).collect();
    let skipped =
        d.streams.iter().filter(|(t, _)| Es::from_stream_type(*t).is_none()).map(|(t, _)| *t).collect();
    Ok((read, skipped))
}

/// The samples of every stream of `pids` in a TS file, per stream, in decode order; `pids` also
/// stands in for a missing PMT. A PES need not start on a frame (old Apple segmenters cut
/// anywhere), so the PES are joined first; a PES time belongs to the first frame starting in it.
pub fn samples(b: &[u8], pids: &[(Es, u16)]) -> Result<(Vec<Vec<Unit>>, usize)> {
    let known: Vec<(u8, u16)> = pids.iter().map(|&(e, pid)| (e.stream_type(), pid)).collect();
    let d = read_ts(b, &known)?;
    let mut es: Vec<(Vec<u8>, Vec<Mark>)> = pids.iter().map(|_| (Vec::new(), Vec::new())).collect();
    for pes in d.pes {
        let Some(i) = pids.iter().position(|&(_, p)| p == pes.pid) else { continue };
        let (buf, marks) = &mut es[i];
        marks.push(Mark { at: buf.len(), pts: pes.pts, dts: pes.dts.or(pes.pts) });
        buf.extend_from_slice(&pes.payload);
    }
    let mut broken = d.broken;
    let mut out = Vec::with_capacity(pids.len());
    for (i, (buf, marks)) in es.iter().enumerate() {
        out.push(match pids[i].0 {
            Es::H264 => h264_units(buf, marks),
            Es::Aac => aac_units(buf, marks, &mut broken),
            Es::Mpa => mpa_units(buf, marks, &mut broken),
        });
    }
    Ok((out, broken))
}

/// Where a PES starts in the joined stream, and its times.
struct Mark {
    at: usize,
    pts: Option<u64>,
    dts: Option<u64>,
}

/// Gives out PES times to the frames that start in each PES, first come first served.
struct Clock<'a> {
    marks: &'a [Mark],
    used: usize,
}

impl Clock<'_> {
    /// The PES that byte `at` lies in, and whether a frame starting there is the first to.
    fn at(&mut self, at: usize) -> (Option<&Mark>, bool) {
        let i = self.marks.partition_point(|m| m.at <= at);
        if i == 0 {
            return (None, false);
        }
        let first = i > self.used;
        if first {
            self.used = i;
        }
        (Some(&self.marks[i - 1]), first)
    }
}

/// Starts of the NAL units in an Annex B stream: (position of the start code as ffmpeg finds it,
/// first byte of the NAL).
fn nal_starts(b: &[u8]) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut i = 0;
    while i + 3 <= b.len() {
        if b[i] == 0 && b[i + 1] == 0 && b[i + 2] == 1 {
            let sc = if i > 0 && b[i - 1] == 0 { i - 1 } else { i };
            out.push((sc, i + 3));
            i += 3;
        } else {
            i += 1;
        }
    }
    out
}

fn h264_units(b: &[u8], marks: &[Mark]) -> Vec<Unit> {
    let nals = nal_starts(b);
    // Access unit boundaries: an AUD; an SPS, PPS or SEI after a picture; or the first slice
    // (first_mb_in_slice 0) of another picture.
    let mut starts: Vec<usize> = Vec::new();
    let mut has_vcl = false;
    for (k, &(_, n)) in nals.iter().enumerate() {
        let Some(&h) = b.get(n) else { continue };
        let t = h & 0x1f;
        let vcl = (1..=5).contains(&t);
        let first_slice = vcl && b.get(n + 1).is_some_and(|x| x & 0x80 != 0);
        let begins = match t {
            9 => true,
            6 | 7 | 8 | 14 | 15 | 16 | 17 | 18 => has_vcl,
            _ => first_slice && has_vcl,
        };
        if begins || starts.is_empty() {
            starts.push(k);
            has_vcl = false;
        }
        has_vcl |= vcl;
    }
    let mut clock = Clock { marks, used: 0 };
    let mut out = Vec::with_capacity(starts.len());
    for (j, &k) in starts.iter().enumerate() {
        let from = nals[k].0;
        let to = starts.get(j + 1).map(|&k2| nals[k2].0).unwrap_or(b.len());
        let mut data = Vec::with_capacity(to - from + 16);
        codec::annexb_to_avcc(&b[from..to], &mut data);
        if data.is_empty() {
            continue;
        }
        let (mark, first) = clock.at(nals[k].1);
        let (pts, dts) = match (mark, first) {
            (Some(m), true) => (m.pts, m.dts),
            _ => (None, None),
        };
        let sync = codec::nals(&data).any(|n| n[0] & 0x1f == 5);
        out.push(Unit { pts, dts, frame_in_pes: 0, data, sync, adts: None, mpa: None });
    }
    out
}

fn aac_units(b: &[u8], marks: &[Mark], broken: &mut usize) -> Vec<Unit> {
    let mut clock = Clock { marks, used: 0 };
    let mut out = Vec::new();
    let (mut base, mut k) = (None, 0u32);
    let mut p = 0;
    let mut junk = false;
    while p + 7 <= b.len() {
        if let Some(n) = id3_len(&b[p..]) {
            p += n;
            continue;
        }
        let Some(a) = Adts::parse(&b[p..]) else {
            if !junk {
                *broken += 1;
                junk = true;
            }
            p += 1;
            continue;
        };
        junk = false;
        if p + a.frame_len > b.len() {
            break; // cut off by the end of the file
        }
        match clock.at(p) {
            (Some(m), true) if m.pts.is_some() => {
                base = m.pts;
                k = 0;
            }
            _ => k += 1,
        }
        out.push(Unit {
            pts: base,
            dts: base,
            frame_in_pes: k,
            data: b[p + a.header_len..p + a.frame_len].to_vec(),
            sync: true,
            adts: Some(a),
            mpa: None,
        });
        p += a.frame_len;
    }
    out
}

/// MPEG audio frames; unlike ADTS the sample keeps its header, as MP4 stores MP3 frames whole.
fn mpa_units(b: &[u8], marks: &[Mark], broken: &mut usize) -> Vec<Unit> {
    let mut clock = Clock { marks, used: 0 };
    let mut out = Vec::new();
    let (mut base, mut k) = (None, 0u32);
    let mut p = 0;
    let mut junk = false;
    while p + 4 <= b.len() {
        if let Some(n) = id3_len(&b[p..]) {
            p += n;
            continue;
        }
        let Some(m) = Mpa::parse(&b[p..]) else {
            if !junk {
                *broken += 1;
                junk = true;
            }
            p += 1;
            continue;
        };
        junk = false;
        if p + m.frame_len > b.len() {
            break;
        }
        match clock.at(p) {
            (Some(mk), true) if mk.pts.is_some() => {
                base = mk.pts;
                k = 0;
            }
            _ => k += 1,
        }
        out.push(Unit {
            pts: base,
            dts: base,
            frame_in_pes: k,
            data: b[p..p + m.frame_len].to_vec(),
            sync: true,
            adts: None,
            mpa: Some(m),
        });
        p += m.frame_len;
    }
    out
}

/// The length of an ID3v2 tag at the start of `b`. Audio streams carry them as metadata
/// (an encoder name, a timestamp) between frames.
fn id3_len(b: &[u8]) -> Option<usize> {
    if b.len() < 10 || &b[..3] != b"ID3" || b[6..10].iter().any(|&x| x & 0x80 != 0) {
        return None;
    }
    let size = b[6..10].iter().fold(0usize, |v, &x| v << 7 | x as usize);
    Some(10 + size + if b[5] & 0x10 != 0 { 10 } else { 0 })
}

/// True if `b` starts like an HLS packed audio segment: an ID3 tag, or bare ADTS or MPEG audio.
pub fn sniff_packed(b: &[u8]) -> bool {
    b.starts_with(b"ID3") || Adts::parse(b).is_some() || Mpa::parse(b).is_some()
}

/// The time an HLS packed audio segment starts at, from the PRIV frame Apple's spec puts in its
/// ID3 tag, and where the frames begin.
fn packed_time(b: &[u8]) -> (Option<u64>, usize) {
    let mut at = 0;
    let mut time = None;
    while b.len() >= at + 10 && &b[at..at + 3] == b"ID3" {
        let size = b[at + 6..at + 10].iter().fold(0usize, |v, &x| v << 7 | (x & 0x7f) as usize);
        let footer = if b[at + 5] & 0x10 != 0 { 10 } else { 0 };
        let end = (at + 10 + size + footer).min(b.len());
        let v4 = b[at + 3] == 4;
        let mut f = at + 10;
        while f + 10 <= end && b[f] != 0 {
            let raw = &b[f + 4..f + 8];
            let n = if v4 {
                raw.iter().fold(0usize, |v, &x| v << 7 | (x & 0x7f) as usize)
            } else {
                u32::from_be_bytes(raw.try_into().unwrap()) as usize
            };
            let data = &b[(f + 10).min(end)..(f + 10 + n).min(end)];
            const OWNER: &[u8] = b"com.apple.streaming.transportStreamTimestamp\0";
            if &b[f..f + 4] == b"PRIV" && data.starts_with(OWNER) && data.len() >= OWNER.len() + 8 {
                let t = u64::from_be_bytes(data[OWNER.len()..OWNER.len() + 8].try_into().unwrap());
                time = Some(t & ((1 << 33) - 1));
            }
            f += 10 + n;
        }
        at = end;
    }
    (time, at)
}

/// The kind of frames an HLS packed audio segment holds.
pub fn packed_kind(b: &[u8]) -> Option<Es> {
    let (_, at) = packed_time(b);
    let rest = &b[at.min(b.len())..];
    if Adts::parse(rest).is_some() {
        Some(Es::Aac)
    } else if Mpa::parse(rest).is_some() {
        Some(Es::Mpa)
    } else {
        None
    }
}

/// The samples of an HLS packed audio segment, as one stream.
fn packed_units(b: &[u8], es: Es) -> Result<(Vec<Vec<Unit>>, usize)> {
    let (time, at) = packed_time(b);
    let marks = [Mark { at: 0, pts: time, dts: time }];
    let mut broken = 0;
    let rest = &b[at.min(b.len())..];
    let units = match es {
        Es::Aac => aac_units(rest, &marks, &mut broken),
        Es::Mpa => mpa_units(rest, &marks, &mut broken),
        Es::H264 => bail!("packed audio cannot hold video"),
    };
    Ok((vec![units], broken))
}

/// The samples of a TS file, or of a packed audio segment when `packed`; see [`samples`].
pub fn units(b: &[u8], pids: &[(Es, u16)], packed: bool) -> Result<(Vec<Vec<Unit>>, usize)> {
    if packed { packed_units(b, pids[0].0) } else { samples(b, pids) }
}
