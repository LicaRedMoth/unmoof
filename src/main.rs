//! The unmoof command: the segments of a stream in, one MP4 or .mp3 out.

use std::fs::File;
use std::io::{self, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use unmoof::{Error, Format, Piece, Plan, Remuxer};

const USAGE: &str = "\
usage: unmoof -o OUT.mp4 -s FILE... [-s FILE...]...

  -s FILE...  one source: its init segment followed by its media segments in
              playlist order, or a single fMP4 file. Each stream delivered on
              its own, such as DASH audio, is another -s.
  --lang L    after -s: the language of that source's tracks (ISO 639-2: rus, jpn)
  --title T   after -s: a title for them, such as the studio of a dub
  --pto T/S   after -s: the DASH presentationTimeOffset T of that source, in timescale S
  -o OUT      the file to write: an MP4, or with .mp3, the bare frames of an MP3 track
  --chunk MS  the longest a chunk may run, in milliseconds (default 500)";

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("unmoof: {e}");
            ExitCode::FAILURE
        }
    }
}

/// What a -s carries besides its files.
#[derive(Default)]
struct Info {
    lang: Option<String>,
    title: Option<String>,
    pto: Option<(i64, u32)>,
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let mut out: Option<PathBuf> = None;
    let mut chunk_ms: Option<f64> = None;
    let mut sources: Vec<Vec<PathBuf>> = Vec::new();
    let mut infos: Vec<Info> = Vec::new();
    let mut args = std::env::args_os().skip(1);
    while let Some(a) = args.next() {
        match a.to_str() {
            Some("-o") => out = Some(args.next().ok_or("-o needs a path")?.into()),
            Some("-s") => {
                sources.push(Vec::new());
                infos.push(Info::default());
            }
            Some(flag @ ("--lang" | "--title")) => {
                let v = args.next().ok_or(format!("{flag} needs a value"))?.to_string_lossy().into_owned();
                let info = infos.last_mut().ok_or(format!("{flag} must follow -s"))?;
                if flag == "--lang" { info.lang = Some(v) } else { info.title = Some(v) }
            }
            Some("--pto") => {
                let v = args.next().ok_or("--pto needs TICKS/TIMESCALE")?;
                let pto = v
                    .to_str()
                    .and_then(|v| v.split_once('/'))
                    .and_then(|(t, s)| Some((t.parse().ok()?, s.parse().ok()?)))
                    .ok_or("--pto needs TICKS/TIMESCALE, such as 1024/12288")?;
                infos.last_mut().ok_or("--pto must follow -s")?.pto = Some(pto);
            }
            Some("--chunk") => {
                let v = args.next().ok_or("--chunk needs milliseconds")?;
                let ms: f64 = v
                    .to_str()
                    .and_then(|v| v.parse().ok())
                    .filter(|&ms: &f64| ms > 0.0)
                    .ok_or("--chunk needs a positive number of milliseconds")?;
                chunk_ms = Some(ms);
            }
            Some("-h" | "--help") => {
                println!("{USAGE}");
                return Ok(());
            }
            _ => match sources.last_mut() {
                Some(s) => s.push(a.into()),
                None => {
                    return Err(format!("{}: a file must follow -s\n\n{USAGE}", a.to_string_lossy()).into());
                }
            },
        }
    }
    let out = out.ok_or(format!("no output given\n\n{USAGE}"))?;
    if sources.is_empty() || sources.iter().any(|s| s.is_empty()) {
        return Err(format!("every -s needs at least one file\n\n{USAGE}").into());
    }

    let mut rm = Remuxer::new();
    if let Some(ms) = chunk_ms {
        rm.set_chunk_duration(ms / 1000.0);
    }
    let mut files: Vec<(PathBuf, u64)> = Vec::new();
    for (group, info) in sources.iter().zip(&infos) {
        let s = rm.add_source();
        rm.set_track_info(s, info.lang.as_deref(), info.title.as_deref())?;
        if let Some((ticks, timescale)) = info.pto {
            rm.set_time_offset(s, ticks, timescale)?;
        }
        for path in group {
            let id = files.len() as u32;
            let len = scan(&mut rm, s, id, path).map_err(|e| format!("{}: {e}", path.display()))?;
            files.push((path.clone(), len));
        }
    }
    // An .mp3 output is the bare frames of the MP3 track; anything else is an MP4.
    let mp3 = out.extension().is_some_and(|e| e.eq_ignore_ascii_case("mp3"));
    let plan = if mp3 { rm.finish_mp3()? } else { rm.finish()? };
    for r in plan.pieces.iter().filter_map(|p| if let Piece::Copy(r) = p { Some(r) } else { None }) {
        let (path, len) = &files[r.file as usize];
        if r.offset + r.len > *len {
            return Err(format!(
                "{}: samples run to byte {} but the file has {len}; is the download cut short?",
                path.display(),
                r.offset + r.len
            )
            .into());
        }
    }

    let part = out.with_extension("part");
    write(&plan, &files, &part)?;
    std::fs::rename(&part, &out)?;
    report(&plan, &out);
    Ok(())
}

/// Hands the `moov` and `moof` boxes of an fMP4 file to the remuxer; other kinds go whole.
fn scan(rm: &mut Remuxer, source: usize, id: u32, path: &Path) -> Result<u64, Box<dyn std::error::Error>> {
    let mut f = File::open(path)?;
    let len = f.metadata()?.len();
    rm.set_file_size(id, len);
    let mut head = vec![0u8; len.min(64 << 10) as usize];
    f.read_exact(&mut head)?;
    match unmoof::sniff(&head) {
        Some(Format::Fmp4) => {}
        Some(Format::Subtitles) => {
            rm.feed_subtitles(source, &std::fs::read(path)?)?;
            return Ok(len);
        }
        Some(Format::Ts | Format::PackedAudio) => {
            rm.feed_ts(source, id, &std::fs::read(path)?)?;
            return Ok(len);
        }
        None => return Err("neither fMP4, MPEG-TS, HLS packed audio nor subtitles".into()),
    }
    let mut pos = 0u64;
    let mut buf = Vec::new();
    while pos < len {
        let mut head = [0u8; 16];
        let n = (len - pos).min(16) as usize;
        f.seek(SeekFrom::Start(pos))?;
        f.read_exact(&mut head[..n])?;
        let Some((typ, _, size)) = unmoof_header(&head[..n]) else {
            return Err(Error(format!("a box header at byte {pos} is cut short")).into());
        };
        let size = size.unwrap_or(len - pos);
        if size < 8 || pos + size > len {
            if &typ == b"mdat" {
                // The samples it holds are checked against the file length later.
                break;
            }
            return Err(Error(format!(
                "box '{}' at byte {pos} runs past the end of the file; is the download cut short?",
                String::from_utf8_lossy(&typ)
            ))
            .into());
        }
        if &typ == b"moov" || &typ == b"moof" {
            buf.resize(size as usize, 0);
            f.seek(SeekFrom::Start(pos))?;
            f.read_exact(&mut buf)?;
            rm.feed(source, id, pos, &buf)?;
        }
        pos += size;
    }
    Ok(len)
}

fn unmoof_header(b: &[u8]) -> Option<([u8; 4], usize, Option<u64>)> {
    if b.len() < 8 {
        return None;
    }
    let size = u32::from_be_bytes(b[0..4].try_into().unwrap()) as u64;
    let typ: [u8; 4] = b[4..8].try_into().unwrap();
    match size {
        0 => Some((typ, 8, None)),
        1 if b.len() >= 16 => Some((typ, 16, Some(u64::from_be_bytes(b[8..16].try_into().unwrap())))),
        1 => None,
        _ => Some((typ, 8, Some(size))),
    }
}

fn write(plan: &Plan, files: &[(PathBuf, u64)], to: &Path) -> io::Result<()> {
    let mut w = BufWriter::with_capacity(1 << 20, File::create(to)?);
    w.write_all(&plan.header)?;
    let mut open: Option<(u32, File)> = None;
    let mut ts: Option<(u32, Vec<u8>)> = None;
    for (i, piece) in plan.pieces.iter().enumerate() {
        let r = match piece {
            Piece::Copy(r) => r,
            Piece::Data(d) => {
                w.write_all(d)?;
                continue;
            }
            Piece::Render { file, .. } => {
                if ts.as_ref().is_none_or(|(id, _)| id != file) {
                    ts = Some((*file, std::fs::read(&files[*file as usize].0)?));
                }
                let bytes = plan.render(i, &ts.as_ref().unwrap().1).map_err(io::Error::other)?;
                w.write_all(&bytes)?;
                continue;
            }
        };
        if open.as_ref().is_none_or(|(id, _)| *id != r.file) {
            open = Some((r.file, File::open(&files[r.file as usize].0)?));
        }
        let f = &mut open.as_mut().unwrap().1;
        f.seek(SeekFrom::Start(r.offset))?;
        let copied = io::copy(&mut f.take(r.len), &mut w)?;
        if copied != r.len {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "an input file shrank while being read",
            ));
        }
    }
    w.flush()
}

fn report(plan: &Plan, out: &Path) {
    let mb = |b: u64| b as f64 / 1e6;
    eprintln!(
        "{}: {:.1} MB, header {:.2} MB, {} pieces",
        out.display(),
        mb(plan.size),
        mb(plan.header.len() as u64),
        plan.pieces.len()
    );
    for t in &plan.tracks {
        let mut notes = Vec::new();
        if t.fixes > 0 {
            notes.push(format!("{} fragment joins closed up", t.fixes));
        }
        if let Some(s) = t.snapped {
            notes.push(format!("frame times snapped to the grid by up to {:.2} ms", s * 1000.0));
        }
        eprintln!(
            "  track {} {} {}  {} samples  {:.3} s  starts at {:.3} s{}",
            t.id,
            t.handler,
            t.codec,
            t.samples,
            t.duration,
            t.start,
            if notes.is_empty() { String::new() } else { format!("  ({})", notes.join("; ")) }
        );
    }
    for w in &plan.warnings {
        eprintln!("  warning: {w}");
    }
}
