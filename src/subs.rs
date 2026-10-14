//! Subtitles: SubRip, WebVTT and ASS/SSA read into cues, then written as one 3GPP timed text
//! (`tx3g`) track, the text track MP4 players show. Bold, italic and underline survive; ASS
//! positions, colours and effects cannot be carried and are dropped.

use std::cmp::Reverse;
use std::collections::{BTreeSet, BinaryHeap};

use crate::bytes::Wr;
use crate::{Result, bail};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Srt,
    Vtt,
    Ass,
}

/// One cue: its time in milliseconds, its text, and runs of style as
/// (first char, end char, face flags: 1 bold, 2 italic, 4 underline).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cue {
    pub start: i64,
    pub end: i64,
    pub text: String,
    pub styles: Vec<(u16, u16, u8)>,
}

/// What kind of subtitles `b` holds, judged from its start.
pub fn sniff(b: &[u8]) -> Option<Kind> {
    let head = std::str::from_utf8(&b[..b.len().min(4096)]).ok().or_else(|| {
        // A cut in the middle of a character at the 4 KiB mark is not a reason to give up.
        (1..4).find_map(|k| std::str::from_utf8(&b[..b.len().min(4096).saturating_sub(k)]).ok())
    })?;
    let head = head.trim_start_matches('\u{feff}').trim_start();
    if head.starts_with("WEBVTT") {
        Some(Kind::Vtt)
    } else if head.starts_with("[Script Info]") || head.contains("[Events]") {
        Some(Kind::Ass)
    } else {
        let mut lines = head.lines().map(str::trim);
        let first = lines.next()?;
        let second = lines.next()?;
        (first.chars().all(|c| c.is_ascii_digit()) && second.contains("-->") && second.contains(','))
            .then_some(Kind::Srt)
    }
}

/// Text in, plain text plus style runs out, built a character at a time.
#[derive(Default)]
struct Styled {
    text: String,
    chars: u16,
    face: u8,
    runs: Vec<(u16, u16, u8)>,
}

impl Styled {
    fn push(&mut self, c: char) {
        match self.runs.last_mut() {
            Some(r) if r.2 == self.face && r.1 == self.chars => r.1 += 1,
            _ if self.face != 0 => self.runs.push((self.chars, self.chars + 1, self.face)),
            _ => {}
        }
        self.text.push(c);
        self.chars = self.chars.saturating_add(1);
    }

    fn set(&mut self, flag: u8, on: bool) {
        if on { self.face |= flag } else { self.face &= !flag }
    }

    fn finish(mut self, start: i64, end: i64) -> Option<Cue> {
        while self.text.ends_with('\n') {
            self.text.pop();
            self.chars -= 1;
        }
        for r in &mut self.runs {
            r.1 = r.1.min(self.chars);
        }
        self.runs.retain(|r| r.0 < r.1);
        (end > start && !self.text.trim().is_empty()).then_some(Cue {
            start,
            end,
            text: self.text,
            styles: self.runs,
        })
    }
}

/// HTML-like markup of SubRip and WebVTT: `<b>`, `<i>`, `<u>` kept as style, other tags (voices,
/// classes, inline times) dropped, entities decoded.
fn markup(lines: &[&str], start: i64, end: i64) -> Option<Cue> {
    let mut s = Styled::default();
    for (n, line) in lines.iter().enumerate() {
        if n > 0 {
            s.push('\n');
        }
        let mut rest = *line;
        // Once a line has no closing `>` or `}` left, no later `<` or `{` opens a tag: not
        // searching again keeps a line of brackets from taking quadratic time.
        let (mut angle, mut brace) = (true, true);
        while !rest.is_empty() {
            if angle && rest.starts_with('<') && rest.find('>').is_none() {
                angle = false;
            }
            if brace && rest.starts_with('{') && rest.find('}').is_none() {
                brace = false;
            }
            if angle
                && rest.starts_with('<')
                && let Some(close) = rest.find('>')
            {
                let tag = rest[1..close].trim().to_ascii_lowercase();
                let (off, name) = match tag.strip_prefix('/') {
                    Some(t) => (true, t.to_string()),
                    None => (false, tag.split([' ', '.']).next().unwrap_or("").to_string()),
                };
                match name.as_str() {
                    "b" => s.set(1, !off),
                    "i" => s.set(2, !off),
                    "u" => s.set(4, !off),
                    _ => {}
                }
                rest = &rest[close + 1..];
                continue;
            }
            if brace && rest.starts_with('{') {
                // ASS override tags that some SubRip files carry, like {\an8}.
                if let Some(close) = rest.find('}') {
                    rest = &rest[close + 1..];
                    continue;
                }
            }
            if rest.starts_with('&') {
                let ents = [
                    ("&amp;", '&'),
                    ("&lt;", '<'),
                    ("&gt;", '>'),
                    ("&nbsp;", '\u{a0}'),
                    ("&lrm;", '\u{200e}'),
                    ("&rlm;", '\u{200f}'),
                ];
                if let Some((e, c)) = ents.iter().find(|(e, _)| rest.starts_with(e)) {
                    s.push(*c);
                    rest = &rest[e.len()..];
                    continue;
                }
            }
            let c = rest.chars().next().unwrap();
            s.push(c);
            rest = &rest[c.len_utf8()..];
        }
    }
    s.finish(start, end)
}

/// `HH:MM:SS,mmm` (SubRip), `HH:MM:SS.mmm` or `MM:SS.mmm` (WebVTT), in milliseconds.
fn clock(t: &str) -> Option<i64> {
    let t = t.trim().replace(',', ".");
    let (hms, frac) = t.split_once('.').unwrap_or((&t, "0"));
    let mut v = 0i64;
    for part in hms.split(':') {
        v = v.checked_mul(60)?.checked_add(part.trim().parse::<i64>().ok()?)?;
    }
    let digits: String = frac.chars().take_while(char::is_ascii_digit).collect();
    let ms = match digits.len() {
        0 => 0,
        1 => digits.parse::<i64>().ok()? * 100,
        2 => digits.parse::<i64>().ok()? * 10,
        _ => digits[..3].parse::<i64>().ok()?,
    };
    v.checked_mul(1000)?.checked_add(ms)
}

fn timing(line: &str) -> Option<(i64, i64)> {
    let (a, b) = line.split_once("-->")?;
    let b = b.split_whitespace().next()?;
    Some((clock(a)?, clock(b)?))
}

/// Cue blocks of SubRip and WebVTT: a timing line, then text lines up to a blank line.
fn blocks(text: &str, offset: i64) -> Vec<Cue> {
    let mut out = Vec::new();
    let lines: Vec<&str> = text.lines().collect();
    let mut i = 0;
    while i < lines.len() {
        if let Some((a, b)) = lines[i].contains("-->").then(|| timing(lines[i])).flatten() {
            let mut j = i + 1;
            while j < lines.len() && !lines[j].trim().is_empty() {
                j += 1;
            }
            out.extend(markup(&lines[i + 1..j], a.saturating_add(offset), b.saturating_add(offset)));
            i = j;
        } else {
            i += 1;
        }
    }
    out
}

/// The cues of a WebVTT file and whether its times are tied to the stream's clock: HLS puts
/// `X-TIMESTAMP-MAP=MPEGTS:<90 kHz time>,LOCAL:<cue time>` in the header to say so.
fn vtt(text: &str) -> (Vec<Cue>, bool) {
    let header_end = text.find("\n\n").unwrap_or(text.len());
    let mut offset = 0;
    let mut tied = false;
    for line in text[..header_end].lines() {
        if let Some(map) = line.trim().strip_prefix("X-TIMESTAMP-MAP=") {
            let (mut mpegts, mut local) = (0i64, 0i64);
            for kv in map.split(',') {
                match kv.split_once(':') {
                    Some(("MPEGTS", v)) => mpegts = v.trim().parse().unwrap_or(0),
                    Some(("LOCAL", v)) => local = clock(v).unwrap_or(0),
                    _ => {}
                }
            }
            offset = mpegts / 90 - local;
            tied = true;
        }
    }
    // NOTE, STYLE and REGION blocks hold no cue timing, so `blocks` passes over them.
    (blocks(&text[header_end.min(text.len())..], offset), tied)
}

/// The Dialogue lines of an ASS/SSA script. Override tags keep only bold, italic and underline;
/// vector drawings (`\p1`) are skipped, as they are shapes, not words.
fn ass(text: &str) -> Vec<Cue> {
    let mut out = Vec::new();
    let mut fields: Vec<String> = Vec::new();
    let mut in_events = false;
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_events = line.eq_ignore_ascii_case("[events]");
            continue;
        }
        if !in_events {
            continue;
        }
        if let Some(f) = line.strip_prefix("Format:") {
            fields = f.split(',').map(|x| x.trim().to_ascii_lowercase()).collect();
            continue;
        }
        let Some(d) = line.strip_prefix("Dialogue:") else { continue };
        if fields.is_empty() {
            fields = "layer,start,end,style,name,marginl,marginr,marginv,effect,text"
                .split(',')
                .map(String::from)
                .collect();
        }
        let parts: Vec<&str> = d.splitn(fields.len(), ',').collect();
        let field =
            |name: &str| fields.iter().position(|f| f == name).and_then(|i| parts.get(i)).map(|s| s.trim());
        let (Some(a), Some(b), Some(t)) =
            (field("start"), field("end"), fields.last().and_then(|_| parts.last()))
        else {
            continue;
        };
        let (Some(a), Some(b)) = (clock(a), clock(b)) else { continue };
        let mut s = Styled::default();
        let mut drawing = false;
        let mut rest = *t;
        while !rest.is_empty() {
            if rest.starts_with('{') {
                // An override block left open runs to the end of the line.
                let close = rest.find('}').unwrap_or(rest.len());
                for tag in rest[1..close].split('\\').filter(|x| !x.is_empty()) {
                    let on = |v: &str| v.chars().next().is_some_and(|c| c != '0');
                    if let Some(v) = tag
                        .strip_prefix('b')
                        .filter(|v| v.chars().all(|c| c.is_ascii_digit()) && !v.is_empty())
                    {
                        s.set(1, on(v));
                    } else if let Some(v) = tag.strip_prefix('i').filter(|v| v.len() == 1) {
                        s.set(2, on(v));
                    } else if let Some(v) = tag.strip_prefix('u').filter(|v| v.len() == 1) {
                        s.set(4, on(v));
                    } else if let Some(v) = tag
                        .strip_prefix('p')
                        .filter(|v| v.chars().all(|c| c.is_ascii_digit()) && !v.is_empty())
                    {
                        drawing = on(v);
                    } else if tag == "r" {
                        s.face = 0;
                    }
                }
                rest = &rest[(close + 1).min(rest.len())..];
                continue;
            }
            if let Some(r) = rest.strip_prefix("\\N").or_else(|| rest.strip_prefix("\\n")) {
                if !drawing {
                    s.push('\n');
                }
                rest = r;
                continue;
            }
            if let Some(r) = rest.strip_prefix("\\h") {
                if !drawing {
                    s.push('\u{a0}');
                }
                rest = r;
                continue;
            }
            let c = rest.chars().next().unwrap();
            if !drawing {
                s.push(c);
            }
            rest = &rest[c.len_utf8()..];
        }
        out.extend(s.finish(a, b));
    }
    out
}

/// Reads a subtitle file into cues, and says whether its times follow the stream's clock
/// (HLS WebVTT) rather than starting with the movie.
pub fn parse(b: &[u8]) -> Result<(Kind, Vec<Cue>, bool)> {
    let Some(kind) = sniff(b) else { bail!("not SubRip, WebVTT or ASS subtitles") };
    let text = String::from_utf8_lossy(b);
    let text = text.trim_start_matches('\u{feff}').replace("\r\n", "\n").replace('\r', "\n");
    Ok(match kind {
        Kind::Srt => (kind, blocks(&text, 0), false),
        Kind::Vtt => {
            let (cues, tied) = vtt(&text);
            (kind, cues, tied)
        }
        Kind::Ass => (kind, ass(&text), false),
    })
}

/// One `tx3g` sample per stretch of time in which the same cues show. Cues that overlap, as
/// signs and dialogue do in ASS, share the screen one above the other. Returns
/// (start ms, duration ms, sample bytes), starting at `from`.
pub fn samples(cues: &[Cue], from: i64) -> Vec<(i64, u32, Vec<u8>)> {
    let mut edges: Vec<i64> = cues.iter().flat_map(|c| [c.start, c.end]).filter(|&t| t >= from).collect();
    edges.push(from);
    edges.sort_unstable();
    edges.dedup();
    let mut by_start: Vec<&Cue> = cues.iter().collect();
    by_start.sort_by_key(|c| c.start);
    // A sweep over the edges: each cue joins the set of shown cues once and leaves it once, so
    // even tens of thousands of overlapping cues take n log n.
    let mut next = 0;
    let mut active: BTreeSet<usize> = BTreeSet::new();
    let mut ends: BinaryHeap<Reverse<(i64, usize)>> = BinaryHeap::new();
    let mut out: Vec<(i64, u32, Vec<u8>)> = Vec::new();
    for w in edges.windows(2) {
        let (a, b) = (w[0], w[1]);
        while next < by_start.len() && by_start[next].start <= a {
            if by_start[next].end > a {
                active.insert(next);
                ends.push(Reverse((by_start[next].end, next)));
            }
            next += 1;
        }
        while let Some(&Reverse((end, i))) = ends.peek() {
            if end > a {
                break;
            }
            ends.pop();
            active.remove(&i);
        }
        let mut text = String::new();
        let mut chars = 0u32;
        let mut styles: Vec<(u32, u32, u8)> = Vec::new();
        // No screen holds more than a few dozen lines; past that a file is damaged or hostile.
        for c in active.iter().take(32).map(|&i| by_start[i]) {
            if !text.is_empty() {
                text.push('\n');
                chars += 1;
            }
            styles.extend(c.styles.iter().map(|&(s, e, f)| (u32::from(s) + chars, u32::from(e) + chars, f)));
            text.push_str(&c.text);
            chars += c.text.chars().count() as u32;
        }
        // A sample states its text length in 16 bits.
        if text.len() > usize::from(u16::MAX) {
            let mut cut = usize::from(u16::MAX);
            while !text.is_char_boundary(cut) {
                cut -= 1;
            }
            text.truncate(cut);
            chars = text.chars().count() as u32;
        }
        let styles: Vec<(u16, u16, u8)> = styles
            .into_iter()
            .filter(|&(s, _, _)| s < chars)
            .map(|(s, e, f)| (s as u16, e.min(chars) as u16, f))
            .collect();
        let mut w = Wr::default();
        w.u16(text.len() as u16);
        w.bytes(text.as_bytes());
        if !styles.is_empty() {
            let at = w.open(b"styl");
            w.u16(styles.len() as u16);
            for (s, e, f) in styles {
                w.u16(s);
                w.u16(e);
                w.u16(1); // font id, as the sample entry's font table names it
                w.bytes(&[f, 16]);
                w.u32(0xffff_ffff);
            }
            w.close(at);
        }
        // Back-to-back empty samples merge, so a gap is one empty sample however many cues end.
        match out.last_mut() {
            Some((_, dur, prev)) if text.is_empty() && prev.len() == 2 => {
                *dur = dur.saturating_add((b - a).min(i64::from(u32::MAX)) as u32);
            }
            _ => out.push((a, (b - a).min(i64::from(u32::MAX)) as u32, w.v)),
        }
    }
    out
}

/// The `stsd` of a timed text track: white 16-point text, centred at the bottom.
pub fn tx3g_stsd() -> Vec<u8> {
    let mut w = Wr::default();
    let stsd = w.open_full(b"stsd", 0, 0);
    w.u32(1);
    let e = w.open(b"tx3g");
    w.zeros(6);
    w.u16(1);
    w.u32(0); // display flags
    w.bytes(&[1, 0xff]); // centred, at the bottom
    w.u32(0x0000_00ff); // background
    w.zeros(8); // default text box
    w.u16(0);
    w.u16(0);
    w.u16(1);
    w.bytes(&[0, 16]);
    w.u32(0xffff_ffff);
    let ftab = w.open(b"ftab");
    w.u16(1);
    w.u16(1);
    w.bytes(&[5]);
    w.bytes(b"Arial");
    w.close(ftab);
    w.close(e);
    w.close(stsd);
    w.v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn srt_with_italics() {
        let (k, cues, tied) =
            parse("1\n00:00:01,000 --> 00:00:02,500\nПривет, <i>мир</i>\n".as_bytes()).unwrap();
        assert_eq!((k, tied), (Kind::Srt, false));
        assert_eq!(
            cues[0],
            Cue { start: 1000, end: 2500, text: "Привет, мир".into(), styles: vec![(8, 11, 2)] }
        );
    }

    #[test]
    fn hls_vtt_follows_the_stream_clock() {
        let v = "WEBVTT\nX-TIMESTAMP-MAP=MPEGTS:900000,LOCAL:00:00:00.000\n\n00:01.000 --> 00:02.000 align:start\n<v Bob>Hi</v>\n";
        let (_, cues, tied) = parse(v.as_bytes()).unwrap();
        assert!(tied);
        assert_eq!((cues[0].start, cues[0].end, cues[0].text.as_str()), (11_000, 12_000, "Hi"));
    }

    #[test]
    fn ass_overrides_and_drawings() {
        let a = "[Script Info]\n\n[Events]\nFormat: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text\n\
                 Dialogue: 0,0:00:01.00,0:00:03.50,Default,,0,0,0,,{\\an8}{\\i1}Да,{\\i0} конечно\\Nдве строки\n\
                 Dialogue: 0,0:00:02.00,0:00:03.00,Sign,,0,0,0,,{\\p1}m 0 0 l 10 10{\\p0}\n";
        let (_, cues, _) = parse(a.as_bytes()).unwrap();
        assert_eq!(cues.len(), 1);
        assert_eq!(cues[0].text, "Да, конечно\nдве строки");
        assert_eq!(cues[0].styles, vec![(0, 3, 2)]);
    }

    #[test]
    fn overlapping_cues_share_the_screen() {
        let c = |s, e, t: &str| Cue { start: s, end: e, text: t.into(), styles: vec![] };
        let out = samples(&[c(0, 2000, "a"), c(1000, 3000, "b")], 0);
        let texts: Vec<_> =
            out.iter().map(|(s, d, b)| (*s, *d, String::from_utf8_lossy(&b[2..]).into_owned())).collect();
        assert_eq!(texts, vec![(0, 1000, "a".into()), (1000, 1000, "a\nb".into()), (2000, 1000, "b".into())]);
    }
}
