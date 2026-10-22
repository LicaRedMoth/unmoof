# unmoof

unmoof turns the segments of an HLS or DASH stream into one ordinary MP4: the `moov` first,
every sample in one `mdat`, the file any player can seek and stream. It is a Rust library, a
command-line tool and a WebAssembly module of about 95 KB gzipped, and it needs no ffmpeg.

It reads only what it has to. For fMP4 input it reads the `moov` and `moof` boxes and plans the
rest as byte ranges of the segments, so a browser can build the output from `Blob.slice` calls
without holding the media in memory.

## What it reads and writes

| Input | Codecs | Notes |
|---|---|---|
| fMP4 (HLS, DASH, CMAF) | any: tested with H.264, HEVC, AV1, VP9, AAC, Opus, FLAC | samples are copied as they are |
| MPEG-TS (HLS) | H.264, HEVC, AAC (ADTS), MP1/MP2/MP3 | rebuilt as length-prefixed samples |
| HLS packed audio | AAC, MP3 | timed by the ID3 `transportStreamTimestamp` |
| Subtitles | SubRip, WebVTT, ASS/SSA | written as a `tx3g` text track |

| Output | What it is |
|---|---|
| `.mp4` | a progressive MP4, `moov` first; several sources become several tracks |
| `.mp3` | the bare frames of an MP3 track, for audio-only streams |

## What it takes care of

| Situation in the stream | What unmoof does |
|---|---|
| Audio and video in separate files (DASH, HLS alternate audio) | one source each, kept on the clock they share |
| DASH `presentationTimeOffset` | `set_time_offset` / `--pto` moves that source to its place |
| Negative composition offsets, edit lists, AAC priming | timed as ISO 14496-12 says: `pts = dts + cto`, then the edit list |
| `tfdt` written after `trun` | read first, so the fragment keeps its time |
| HLS discontinuities, segments that restart their clock | joined where the last segment ended |
| A missing stretch of audio | kept as a gap, so the audio after it stays in sync |
| Pictures shown before they are decoded (`pts < dts`) | presentation times kept; decode times moved earlier |
| A picture that claims to show hours after its decoding | shown at the stream's usual offset, with a warning |
| Audio frame times rounded by a packager | put back on the frame grid |
| HEVC parameter sets repeated in TS | moved into the sample entry, written as `hvc1` for Apple players |
| Fake image headers in front of TS segments, 33-bit clock wrap | skipped, unwrapped |
| Damaged or hostile input | a clean error, never a panic or a runaway allocation |

## Command line

```bash
cargo install --path .
```

Each `-s` starts a source: its init segment, then its media segments in playlist order.

```bash
unmoof -o movie.mp4 -s video/init.mp4 video/seg-*.m4s -s audio/init.mp4 audio/seg-*.m4s
```

```bash
unmoof -o episode.mp4 -s segment-*.ts -s --lang rus --title Studio subs.vtt
```

```bash
unmoof -o track.mp3 -s segment-*.ts
```

The options:

| Option | Meaning |
|---|---|
| `-s FILE...` | one source: an init segment and its segments, TS segments, packed audio or subtitles |
| `--lang L` | after `-s`: the language of that source's tracks, ISO 639-2 (`rus`, `jpn`) |
| `--title T` | after `-s`: a title for them, such as the studio of a dub |
| `--pto T/S` | after `-s`: the DASH `presentationTimeOffset` T in timescale S |
| `--chunk MS` | the longest a chunk may run, in milliseconds (500 by default) |
| `-o OUT` | the file to write: `.mp4`, or `.mp3` for the bare frames of an MP3 track |

Playlists, keys and downloads are the caller's business: unmoof works on segments already on disk
or in memory, decrypted. `tools/fetch.py` shows one way to get them for testing.

## Library

```rust,no_run
use std::fs::{self, File};
use std::io::Write;

use unmoof::{Piece, Remuxer};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let paths = ["init.mp4", "seg-1.m4s", "seg-2.m4s"];
    let files: Vec<Vec<u8>> = paths.iter().map(fs::read).collect::<Result<_, _>>()?;

    let mut rm = Remuxer::new();
    let video = rm.add_source();
    for (i, data) in files.iter().enumerate() {
        rm.set_file_size(i as u32, data.len() as u64);
        rm.feed(video, i as u32, 0, data)?;
    }
    let plan = rm.finish()?;

    let mut out = File::create("out.mp4")?;
    out.write_all(&plan.header)?;
    for (i, piece) in plan.pieces.iter().enumerate() {
        match piece {
            Piece::Copy(r) => {
                let start = r.offset as usize;
                out.write_all(&files[r.file as usize][start..start + r.len as usize])?;
            }
            Piece::Render { file, .. } => out.write_all(&plan.render(i, &files[*file as usize])?)?,
            Piece::Data(bytes) => out.write_all(bytes)?,
        }
    }
    Ok(())
}
```

The calls of `Remuxer` and `Plan`:

| Call | Use |
|---|---|
| `add_source()` | opens a source and returns its index |
| `feed(source, file, offset, bytes)` | fMP4 boxes found at `offset` of input file `file` |
| `feed_ts(source, file, bytes)` | a whole TS file or packed audio segment |
| `feed_subtitles(source, bytes)` | a SubRip, WebVTT or ASS file |
| `set_file_size(file, len)` | lets damaged boxes that point past the file be refused early |
| `set_track_info(source, lang, title)` | language and title from the manifest |
| `set_time_offset(source, ticks, timescale)` | the DASH `presentationTimeOffset` |
| `finish()` / `finish_mp3()` | the `Plan`: a header, then pieces to write in order |
| `Plan::render(piece, ts_file)` | the bytes of a TS piece, cut out of the whole TS file again |

`sniff(head)` tells fMP4, TS, packed audio and subtitles apart from a file's first bytes.

## WebAssembly

```bash
cd wasm && cargo build --release --target wasm32-unknown-unknown
```

`wasm/unmoof.js` loads the module and wraps it; it works in browsers, extensions and Node.

```js
import { init } from './unmoof.js';

// files: File objects, the init segment and segments of one stream in playlist order.
const readBytes = async (f, offset, length) => new Uint8Array(await f.slice(offset, offset + length).arrayBuffer());
const bytesOf = async (f) => new Uint8Array(await f.arrayBuffer());

const um = await init(await fetch('unmoof.wasm'));
const rm = um.remuxer();
const video = rm.addSource();
for (const [i, f] of files.entries()) {
  await rm.feedFile(video, i, f.size, (offset, length) => readBytes(f, offset, length));
}
const plan = rm.finish();
rm.free();

// The file is plan.header, then each piece in order.
const parts = [plan.header];
for (const p of plan.pieces) {
  if (p.copy) parts.push(files[p.copy.file].slice(p.copy.offset, p.copy.offset + p.copy.len));
  if (p.render) parts.push(plan.render(p.render.index, await bytesOf(files[p.render.file])));
  if (p.data) parts.push(plan.data(p.data.index));
}
plan.free();
const mp4 = new Blob(parts, { type: 'video/mp4' });
```

`feedFile` reads only the `moov` and `moof` boxes of an fMP4 file, and the whole of other files.
`plan.tracks` and `plan.warnings` tell what came out and what was fixed on the way.

## Checked against public streams

`tools/corpus.py` runs unmoof over the test streams that dash.js, Shaka Player and hls.js list.
Every packet is compared with ffmpeg's `-c copy` remux of the same segments, and where each track
starts is checked against a separate reading of the specifications (`tools/sync.py`), because
ffmpeg places separate inputs its own way.

| Result | Streams |
|---|---|
| Every packet as ffmpeg has it, tracks where the specifications put them | 75 |
| Differs from ffmpeg, where ffmpeg is wrong (below) | 4 |
| Not downloadable: 403, 404, dead hosts | 34 |
| Live or DRM, out of scope | 197 |

| Stream | What is in it | ffmpeg | unmoof |
|---|---|---|---|
| hls.js `fdr` | 145 pictures with `pts < dts` | guesses new times; pictures land 1 tick apart | keeps the times, decodes earlier |
| hls.js `mpegAudioOnly` | ID3 tags between MP3 segments | glues a tag into a frame, which then fails to decode | skips the tags |
| JW Player `new_master` | keyframes claiming to show 4900 s late | keeps them at 4900 s | shows them in line |
| Akamai `caption_test` Elephants Dream | `tfdt` after `trun` | packs 20 s of pictures into 15 s | 20 s, evenly |

## Speed

Minimum of 30 runs on an Intel Pentium B960 (2011, 2 cores), inputs in memory
(`cargo run --release --example bench`, `node wasm/bench.mjs`).

| Workload | Input | Native | WebAssembly (Node) |
|---|---|---|---|
| Plan a 24-minute fMP4 episode, video and audio | 282 MB of segments | 14 ms | 16 ms |
| Remux 1080p H.264 + AAC from TS, samples cut out | 54 MB | 485 ms | 592 ms |
| MP3 from TS to a bare `.mp3` | 9 MB | 27 ms | 32 ms |
| 50,000 overlapping subtitle cues | 3 MB | 206 ms | 275 ms |

## Limits

| Not done | Why |
|---|---|
| Fetching playlists, decrypting AES-128 | the caller's part; unmoof takes the segments |
| DRM, SAMPLE-AES | encrypted samples cannot be remuxed into a playable file |
| Joining DASH periods | each period is a set of sources of its own |
| Quality switches inside one source | the first sample description is kept, with a warning |
| ASS styles, positions and effects | MP4 text tracks have no place for them; bold, italic and underline stay |

## Development

| Check | Command |
|---|---|
| Unit and damaged-input tests | `cargo test` |
| Lints | `cargo clippy --all-targets`, `cargo fmt --check` |
| Every test stream against ffmpeg | `tools/regress.sh` (needs streams in `testdata/`, not in the repository) |
| WebAssembly against the native build, byte for byte | `tools/wasm_check.sh` |
| Fuzzing on stable Rust | `cargo run --profile fuzz --example fuzz -- SECONDS` |
| Fuzzing with libFuzzer | `cd fuzz && cargo +nightly fuzz run ts` |
| The public stream corpus | `tools/corpus.py` |

## License

MIT, see [LICENSE](LICENSE).
