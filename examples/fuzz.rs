//! A mutation fuzzer on stable Rust: real test streams, damaged at random, fed through the whole
//! pipeline (feed, finish, render) until something panics, overflows or hangs.
//!
//!   cargo run --profile fuzz --example fuzz -- SECONDS [SEED]
//!   cargo run --profile fuzz --example fuzz -- --replay out/fuzz/<case>
//!
//! The `fuzz` profile keeps overflow checks on, so arithmetic that wraps in release counts as a
//! failure. Each failure is saved once per panic location, with its inputs and a replay command.

use std::cell::RefCell;
use std::panic;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use unmoof::{Format, Piece, Remuxer};

/// xorshift64*: no dependency, and the same seed gives the same run.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    fn below(&mut self, n: usize) -> usize {
        if n == 0 { 0 } else { (self.next() % n as u64) as usize }
    }

    fn chance(&mut self, percent: u64) -> bool {
        self.next() % 100 < percent
    }
}

#[derive(Clone)]
struct Input {
    kind: Format,
    bytes: Vec<u8>,
    /// Offsets of box headers, TS packets or lines: where a mutation does the most harm.
    marks: Vec<usize>,
    name: String,
}

#[derive(Clone)]
struct Scenario {
    name: &'static str,
    sources: Vec<Vec<Input>>,
    mp3: bool,
    /// A time offset for one source: its index, ticks and timescale.
    offset: Option<(usize, i64, u32)>,
}

fn marks_of(kind: Format, b: &[u8]) -> Vec<usize> {
    let mut out = Vec::new();
    match kind {
        Format::Fmp4 => {
            fn walk(b: &[u8], from: usize, to: usize, out: &mut Vec<usize>) {
                let mut p = from;
                while p + 8 <= to {
                    out.push(p);
                    let size = u32::from_be_bytes(b[p..p + 4].try_into().unwrap()) as usize;
                    let t = &b[p + 4..p + 8];
                    if size < 8 || p + size > to {
                        break;
                    }
                    let container = [
                        &b"moov"[..],
                        b"trak",
                        b"mdia",
                        b"minf",
                        b"stbl",
                        b"moof",
                        b"traf",
                        b"mvex",
                        b"edts",
                        b"dinf",
                    ];
                    if container.contains(&t) {
                        walk(b, p + 8, p + size, out);
                    }
                    if t == b"stsd" && size > 16 {
                        out.push(p + 16);
                    }
                    p += size;
                }
            }
            walk(b, 0, b.len(), &mut out);
        }
        Format::Ts => out.extend((0..b.len()).step_by(188)),
        Format::PackedAudio => out.extend((0..b.len().min(4096)).step_by(64)),
        Format::Subtitles => {
            out.push(0);
            out.extend(b.iter().enumerate().filter(|&(_, &c)| c == b'\n').map(|(i, _)| i + 1));
        }
    }
    out
}

fn load(paths: &[PathBuf]) -> Option<Vec<Input>> {
    let mut out = Vec::new();
    for p in paths {
        let bytes = std::fs::read(p).ok()?;
        let kind = unmoof::sniff(&bytes[..bytes.len().min(65536)])?;
        let marks = marks_of(kind, &bytes);
        out.push(Input { kind, bytes, marks, name: p.file_name()?.to_string_lossy().into_owned() });
    }
    Some(out)
}

fn files(dir: &str, names: &[&str]) -> Vec<PathBuf> {
    names.iter().map(|n| Path::new(dir).join(n)).collect()
}

fn sorted(dir: &str, prefix: &str, take: usize) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(dir)
        .map(|d| d.filter_map(|e| e.ok().map(|e| e.path())).collect())
        .unwrap_or_default();
    v.retain(|p| p.file_name().is_some_and(|n| n.to_string_lossy().starts_with(prefix)));
    v.sort_by_key(|p| {
        let n = p.file_name().unwrap().to_string_lossy().into_owned();
        let digits: String = n.chars().filter(char::is_ascii_digit).collect();
        (digits.len(), digits)
    });
    v.truncate(take);
    v
}

/// The seeds: small cuts of every kind of stream the regression knows.
fn scenarios() -> Vec<Scenario> {
    let t = "testdata";
    let c = |s: &str| format!("{t}/synth/codecs/{s}");
    let mut defs: Vec<(&'static str, Vec<Vec<PathBuf>>, bool)> = vec![
        (
            "dash-two-dubs",
            (0..3)
                .map(|n| {
                    let mut v = files(&c("dubs"), &[&format!("init-stream{n}.m4s")]);
                    v.extend(sorted(&c("dubs"), &format!("chunk-stream{n}-"), 2));
                    v
                })
                .collect(),
            false,
        ),
        (
            "dash-vp9-opus",
            (0..2)
                .map(|n| {
                    let mut v = files(&c("vp9_opus"), &[&format!("init-stream{n}.m4s")]);
                    v.extend(sorted(&c("vp9_opus"), &format!("chunk-stream{n}-"), 2));
                    v
                })
                .collect(),
            false,
        ),
        ("fmp4-negative-cto", vec![files(&format!("{t}/synth"), &["frag_neg.mp4"])], false),
        (
            "alloha-hls-fmp4",
            vec![files(&format!("{t}/alloha480"), &["init-c1-f1-v1-a1.mp4", "seg-1-f1-v1-a1.m4s"])],
            false,
        ),
        ("ts-h264-aac", vec![sorted(&format!("{t}/synth/ts_plain"), "s", 2)], false),
        ("ts-apple-split-pes", vec![sorted(&format!("{t}/ts/apple_bipbop/src0"), "0", 1)], false),
        ("ts-mp3", vec![sorted(&format!("{t}/synth/mp3/hls_mp3"), "s", 2)], true),
        ("ts-vk-mp3-id3", vec![sorted(&format!("{t}/vk"), "seg-", 2)], true),
        ("packed-aac", vec![sorted(&format!("{t}/synth/mp3/packed_aac"), "s", 2)], false),
        ("packed-mp3", vec![sorted(&format!("{t}/synth/mp3/packed_mp3"), "s", 2)], true),
        (
            "srt",
            vec![
                files(&format!("{t}/synth/subs"), &["sub_v_frag.mp4"]),
                files(&format!("{t}/synth/subs"), &["sub.srt"]),
            ],
            false,
        ),
        (
            "hls-vtt",
            vec![
                files(&format!("{t}/synth/subs/hls"), &["v0.ts"]),
                files(&format!("{t}/synth/subs/hls"), &["s0.vtt", "s1.vtt"]),
            ],
            false,
        ),
        (
            "ass",
            vec![
                files(&format!("{t}/synth/subs"), &["sub_v_frag.mp4"]),
                files(&format!("{t}/synth/subs"), &["s.ass"]),
            ],
            false,
        ),
    ];
    let mut out = Vec::new();
    for (name, sources, mp3) in defs.drain(..) {
        let loaded: Option<Vec<Vec<Input>>> = sources.iter().map(|s| load(s)).collect();
        match loaded {
            Some(s) if s.iter().all(|f| !f.is_empty()) => {
                out.push(Scenario { name, sources: s, mp3, offset: None })
            }
            _ => eprintln!("skipping seed {name}: its test data is missing"),
        }
    }
    out
}

const INTERESTING: [u64; 16] = [
    0,
    1,
    2,
    7,
    8,
    0x7f,
    0x80,
    0xff,
    0x7fff,
    0x8000,
    0xffff,
    0x7fff_ffff,
    0x8000_0000,
    0xffff_ffff,
    0x7fff_ffff_ffff_ffff,
    0xffff_ffff_ffff_ffff,
];

fn mutate(rng: &mut Rng, f: &mut Input, donor: &[u8]) {
    let ops = 1 + rng.below(4);
    for _ in 0..ops {
        let len = f.bytes.len();
        if len == 0 {
            f.bytes.extend((0..1 + rng.below(64)).map(|_| rng.next() as u8));
            continue;
        }
        // Half the time aim at a structural spot: a box header, a packet start, a line start.
        let at = if !f.marks.is_empty() && rng.chance(50) {
            (f.marks[rng.below(f.marks.len())] + rng.below(24)).min(len - 1)
        } else {
            rng.below(len)
        };
        match rng.below(9) {
            0 => f.bytes[at] ^= 1 << rng.below(8),
            1 => f.bytes[at] = INTERESTING[rng.below(9)] as u8,
            2 | 3 => {
                let width = [2usize, 4, 4, 8][rng.below(4)];
                let v = if rng.chance(20) {
                    (len as u64).wrapping_add(rng.below(3) as u64).wrapping_sub(1)
                } else {
                    INTERESTING[rng.below(INTERESTING.len())]
                };
                let bytes = v.to_be_bytes();
                for k in 0..width.min(len - at) {
                    f.bytes[at + k] = bytes[8 - width + k];
                }
            }
            4 => f.bytes.truncate(at),
            5 => {
                let n = rng.below(len - at + 1).min(4096);
                f.bytes.drain(at..at + n);
            }
            6 => {
                let n = rng.below(len - at + 1).min(4096);
                let piece = f.bytes[at..at + n].to_vec();
                let to = rng.below(f.bytes.len() + 1);
                f.bytes.splice(to..to, piece);
            }
            7 => {
                let piece: Vec<u8> = (0..1 + rng.below(32)).map(|_| rng.next() as u8).collect();
                f.bytes.splice(at..at, piece);
            }
            _ => {
                if !donor.is_empty() {
                    let from = rng.below(donor.len());
                    let n = rng.below(donor.len() - from + 1).min(2048).min(len - at);
                    f.bytes[at..at + n].copy_from_slice(&donor[from..from + n]);
                }
            }
        }
    }
}

/// Feeds a scenario, lays the file out and renders every TS piece, checking that the plan
/// adds up. Returns Err for a clean rejection, which is fine; a panic is not.
fn run(s: &Scenario) -> Result<(), String> {
    let mut rm = Remuxer::new();
    let mut file = 0u32;
    let mut all: Vec<&[u8]> = Vec::new();
    for src in &s.sources {
        let si = rm.add_source();
        for f in src {
            rm.set_file_size(file, f.bytes.len() as u64);
            let r = match f.kind {
                Format::Fmp4 => rm.feed(si, file, 0, &f.bytes),
                Format::Ts | Format::PackedAudio => rm.feed_ts(si, file, &f.bytes),
                Format::Subtitles => rm.feed_subtitles(si, &f.bytes),
            };
            r.map_err(|e| e.to_string())?;
            all.push(&f.bytes);
            file += 1;
        }
    }
    if let Some((si, ticks, timescale)) = s.offset {
        rm.set_time_offset(si, ticks, timescale).map_err(|e| e.to_string())?;
    }
    let plan = if s.mp3 { rm.finish_mp3() } else { rm.finish() }.map_err(|e| e.to_string())?;
    let mut total = plan.header.len() as u64;
    for (i, p) in plan.pieces.iter().enumerate() {
        total += p.len();
        if let Piece::Render { file, len, .. } = p {
            let got = plan.render(i, all[*file as usize]).map_err(|e| e.to_string())?;
            assert_eq!(got.len() as u64, *len, "a rendered piece is not the length planned");
        }
    }
    assert_eq!(total, plan.size, "the pieces do not add up to the planned size");
    Ok(())
}

thread_local! {
    static PANIC: RefCell<Option<(String, String)>> = const { RefCell::new(None) };
}

fn save(dir: &Path, s: &Scenario, why: &str) {
    let _ = std::fs::create_dir_all(dir);
    let mut cmd = String::from("target/release/unmoof -o out.mp4");
    for (si, src) in s.sources.iter().enumerate() {
        cmd.push_str(" -s");
        for (fi, f) in src.iter().enumerate() {
            let name = format!("s{si}-{fi}-{}", f.name);
            let _ = std::fs::write(dir.join(&name), &f.bytes);
            cmd.push(' ');
            cmd.push_str(&dir.join(&name).display().to_string());
        }
    }
    let _ = std::fs::write(
        dir.join("README.txt"),
        format!("seed scenario: {}\nmp3: {}\noffset: {:?}\n{why}\n\n{cmd}\n", s.name, s.mp3, s.offset),
    );
}

fn replay(dir: &Path) {
    let readme = std::fs::read_to_string(dir.join("README.txt")).expect("no README.txt in the case");
    let mp3 = readme.lines().any(|l| l == "mp3: true");
    let offset = readme.lines().find_map(|l| {
        let v = l.strip_prefix("offset: Some((")?.strip_suffix("))")?;
        let mut it = v.split(", ");
        Some((it.next()?.parse().ok()?, it.next()?.parse().ok()?, it.next()?.parse().ok()?))
    });
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with('s'))
        .collect();
    names.sort();
    let mut sources: Vec<Vec<Input>> = Vec::new();
    for n in names {
        let si: usize = n[1..].split('-').next().unwrap().parse().unwrap();
        while sources.len() <= si {
            sources.push(Vec::new());
        }
        let bytes = std::fs::read(dir.join(&n)).unwrap();
        let orig = n.splitn(3, '-').nth(2).unwrap_or("").to_string();
        let kind = if orig.ends_with(".srt") || orig.ends_with(".vtt") || orig.ends_with(".ass") {
            Format::Subtitles
        } else if orig.ends_with(".ts") {
            Format::Ts
        } else if orig.ends_with(".aac") || orig.ends_with(".mp3") {
            Format::PackedAudio
        } else {
            Format::Fmp4
        };
        sources[si].push(Input { kind, bytes, marks: Vec::new(), name: orig });
    }
    let s = Scenario { name: "replay", sources, mp3, offset };
    let t = Instant::now();
    println!("{:?} in {:?}", run(&s), t.elapsed());
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("--replay") {
        return replay(Path::new(&args[1]));
    }
    let secs: u64 = args.first().and_then(|a| a.parse().ok()).unwrap_or(60);
    let seed: u64 = args.get(1).and_then(|a| a.parse().ok()).unwrap_or(0x5eed_u64 ^ secs).max(1);
    let seeds = scenarios();
    assert!(!seeds.is_empty(), "no seed scenario has its test data");
    for s in &seeds {
        if let Err(e) = run(s)
            && (!s.mp3 || !e.contains("MP3"))
        {
            eprintln!("seed {} fails before any mutation: {e}", s.name);
        }
    }
    panic::set_hook(Box::new(|info| {
        let loc = info.location().map(|l| format!("{}:{}", l.file(), l.line())).unwrap_or_default();
        let msg = info
            .payload()
            .downcast_ref::<String>()
            .cloned()
            .or_else(|| info.payload().downcast_ref::<&str>().map(|s| s.to_string()))
            .unwrap_or_default();
        PANIC.with(|p| *p.borrow_mut() = Some((loc, msg)));
    }));

    let mut rng = Rng(seed);
    let out = Path::new("out/fuzz");
    let start = Instant::now();
    let mut last = Instant::now();
    let (mut cases, mut rejected) = (0u64, 0u64);
    let mut found: Vec<String> = Vec::new();
    while start.elapsed() < Duration::from_secs(secs) {
        let mut s = seeds[rng.below(seeds.len())].clone();
        // Now and then disturb the order of files, as a downloader with a bug might.
        if rng.chance(10) {
            let src = rng.below(s.sources.len());
            let n = s.sources[src].len();
            if n > 1 {
                s.sources[src].swap(rng.below(n), rng.below(n));
            }
        }
        if rng.chance(10) {
            let pick = |rng: &mut Rng| INTERESTING[rng.below(INTERESTING.len())];
            let ticks =
                if rng.chance(50) { pick(&mut rng) as i64 } else { (pick(&mut rng) as i64).wrapping_neg() };
            s.offset = Some((rng.below(s.sources.len()), ticks, pick(&mut rng) as u32));
        }
        let donor = {
            let a = &seeds[rng.below(seeds.len())];
            a.sources[0][0].bytes.clone()
        };
        for _ in 0..1 + rng.below(2) {
            let src = rng.below(s.sources.len());
            let fi = rng.below(s.sources[src].len());
            mutate(&mut rng, &mut s.sources[src][fi], &donor);
        }
        let t = Instant::now();
        let r = panic::catch_unwind(|| run(&s));
        let took = t.elapsed();
        cases += 1;
        let failure = match r {
            Ok(Ok(())) => None,
            Ok(Err(_)) => {
                rejected += 1;
                None
            }
            Err(_) => {
                PANIC.with(|p| p.borrow_mut().take()).map(|(loc, msg)| (format!("panic at {loc}"), msg))
            }
        };
        let failure = failure
            .or_else(|| (took > Duration::from_secs(2)).then(|| (format!("slow: {took:?}"), String::new())));
        if let Some((key, msg)) = failure {
            let sig = if key.starts_with("slow") { "slow".to_string() } else { key.clone() };
            if !found.contains(&sig) {
                found.push(sig.clone());
                let dir = out.join(format!("case-{:02}", found.len()));
                save(&dir, &s, &format!("{key}\n{msg}"));
                println!("NEW {key}: {msg} (seed {}, saved to {})", s.name, dir.display());
            }
        }
        if last.elapsed() > Duration::from_secs(15) {
            last = Instant::now();
            let secs = start.elapsed().as_secs_f64();
            println!(
                "{:.0} s: {cases} cases ({:.0}/s), {rejected} rejected cleanly, {} distinct failures",
                secs,
                cases as f64 / secs,
                found.len()
            );
        }
    }
    println!("done: {cases} cases, {rejected} rejected cleanly, {} distinct failures", found.len());
}
