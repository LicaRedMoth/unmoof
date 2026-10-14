//! Speed of the core on four workloads, all inputs in memory, for comparing builds:
//!   cargo run --release --example bench -- RUNS
//! wasm/bench.mjs runs the same workloads through unmoof.wasm; the `out` column must match.

use std::path::Path;
use std::time::Instant;

use unmoof::{Piece, Remuxer};

/// The top-level `moov` and `moof` boxes of a file and where they lie: all the remuxer reads.
fn boxes(b: &[u8]) -> Vec<(u64, Vec<u8>)> {
    let (mut out, mut p) = (Vec::new(), 0usize);
    while p + 8 <= b.len() {
        let size = u32::from_be_bytes(b[p..p + 4].try_into().unwrap()) as usize;
        if size < 8 || p + size > b.len() {
            break;
        }
        if &b[p + 4..p + 8] == b"moov" || &b[p + 4..p + 8] == b"moof" {
            out.push((p as u64, b[p..p + size].to_vec()));
        }
        p += size;
    }
    out
}

fn segments(m3u8: &str) -> Vec<String> {
    let dir = Path::new(m3u8).parent().unwrap();
    std::fs::read_to_string(m3u8)
        .unwrap()
        .lines()
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(|l| dir.join(l).display().to_string())
        .collect()
}

fn sorted(dir: &str, prefix: &str) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.path().display().to_string())
        .filter(|p| Path::new(p).file_name().unwrap().to_string_lossy().starts_with(prefix))
        .collect();
    v.sort();
    v
}

fn render_all(plan: &unmoof::Plan, files: &[Vec<u8>]) -> u64 {
    let mut n = plan.header.len() as u64;
    for (i, p) in plan.pieces.iter().enumerate() {
        n += match p {
            Piece::Render { file, .. } => plan.render(i, &files[*file as usize]).unwrap().len() as u64,
            other => other.len(),
        };
    }
    n
}

fn main() {
    let runs: usize = std::env::args().nth(1).and_then(|a| a.parse().ok()).unwrap_or(15);
    let a = "testdata/aniboom720";
    let mut ab: Vec<(usize, Vec<(u64, Vec<u8>)>)> = Vec::new();
    let mut ab_bytes = 0u64;
    for (src, (init, list)) in
        [("60d2572dd2dbe1_init_4.m4s", "media_4.m3u8"), ("60d2572dd2dbe1_init_1.m4s", "media_1.m3u8")]
            .iter()
            .enumerate()
    {
        for path in std::iter::once(format!("{a}/{init}")).chain(segments(&format!("{a}/{list}"))) {
            let b = std::fs::read(&path).unwrap();
            ab_bytes += b.len() as u64;
            ab.push((src, boxes(&b)));
        }
    }
    let ts: Vec<Vec<u8>> = std::fs::read_to_string("testdata/ts/mux_1080/src0/list.txt")
        .unwrap()
        .lines()
        .map(|p| std::fs::read(p).unwrap())
        .collect();
    let vk: Vec<Vec<u8>> = sorted("testdata/vk", "seg-").iter().map(|p| std::fs::read(p).unwrap()).collect();
    let mut srt = String::new();
    for i in 0..50_000 {
        let (s, e) = (i * 700, i * 700 + 2500);
        srt += &format!(
            "{}\n{:02}:{:02}:{:02},{:03} --> {:02}:{:02}:{:02},{:03}\nСтрока <i>{i}</i>\n\n",
            i + 1,
            s / 3_600_000,
            s / 60_000 % 60,
            s / 1000 % 60,
            s % 1000,
            e / 3_600_000,
            e / 60_000 % 60,
            e / 1000 % 60,
            e % 1000
        );
    }
    let mb = |v: &[Vec<u8>]| v.iter().map(Vec::len).sum::<usize>() as u64;

    type Work<'a> = Box<dyn Fn() -> u64 + 'a>;
    let work: Vec<(&str, u64, Work)> = vec![
        (
            "fmp4 episode plan",
            ab_bytes,
            Box::new(|| {
                let mut rm = Remuxer::new();
                rm.add_source();
                rm.add_source();
                for (id, (src, bx)) in ab.iter().enumerate() {
                    for (off, b) in bx {
                        rm.feed(*src, id as u32, *off, b).unwrap();
                    }
                }
                rm.finish().unwrap().size
            }),
        ),
        (
            "ts 1080p remux",
            mb(&ts),
            Box::new(|| {
                let mut rm = Remuxer::new();
                let s = rm.add_source();
                for (i, f) in ts.iter().enumerate() {
                    rm.feed_ts(s, i as u32, f).unwrap();
                }
                render_all(&rm.finish().unwrap(), &ts)
            }),
        ),
        (
            "ts mp3 to .mp3",
            mb(&vk),
            Box::new(|| {
                let mut rm = Remuxer::new();
                let s = rm.add_source();
                for (i, f) in vk.iter().enumerate() {
                    rm.feed_ts(s, i as u32, f).unwrap();
                }
                render_all(&rm.finish_mp3().unwrap(), &vk)
            }),
        ),
        (
            "50k subtitle cues",
            srt.len() as u64,
            Box::new(|| {
                let mut rm = Remuxer::new();
                let s = rm.add_source();
                rm.feed_subtitles(s, srt.as_bytes()).unwrap();
                rm.finish().unwrap().size
            }),
        ),
    ];
    println!("workload\truns\tmin ms\tmedian ms\tmax ms\tinput MB\tMB/s\tout");
    for (name, input, f) in &work {
        let out = f(); // warm-up, and the result both builds must agree on
        let mut t: Vec<f64> = (0..runs)
            .map(|_| {
                let s = Instant::now();
                std::hint::black_box(f());
                s.elapsed().as_secs_f64() * 1000.0
            })
            .collect();
        t.sort_by(f64::total_cmp);
        let med = t[t.len() / 2];
        println!(
            "{name}\t{runs}\t{:.1}\t{med:.1}\t{:.1}\t{:.1}\t{:.0}\t{out}",
            t[0],
            t[t.len() - 1],
            *input as f64 / 1e6,
            *input as f64 / 1e6 / (med / 1000.0)
        );
    }
}
