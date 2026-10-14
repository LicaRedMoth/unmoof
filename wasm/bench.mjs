#!/usr/bin/env node
// The workloads of examples/bench.rs, run through unmoof.wasm under Node, from the repo root:
//   node wasm/bench.mjs [path/to/unmoof.wasm] [RUNS]
import { readFileSync, readdirSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { init } from './unmoof.js';

const wasmPath = process.argv[2] || new URL('./unmoof.wasm', import.meta.url).pathname;
const runs = Number(process.argv[3] || 15);
const um = await init(readFileSync(wasmPath));

function boxes(b) {
  const out = [];
  const dv = new DataView(b.buffer, b.byteOffset, b.byteLength);
  for (let p = 0; p + 8 <= b.length;) {
    const size = dv.getUint32(p);
    if (size < 8 || p + size > b.length) break;
    const t = String.fromCharCode(b[p + 4], b[p + 5], b[p + 6], b[p + 7]);
    if (t === 'moov' || t === 'moof') out.push([p, b.subarray(p, p + size)]);
    p += size;
  }
  return out;
}
const segments = (m3u8) => readFileSync(m3u8, 'utf8').split('\n').filter((l) => l && !l.startsWith('#')).map((l) => join(dirname(m3u8), l));
const read = (p) => new Uint8Array(readFileSync(p));

const a = 'testdata/aniboom720';
const ab = [];
let abBytes = 0;
[['60d2572dd2dbe1_init_4.m4s', 'media_4.m3u8'], ['60d2572dd2dbe1_init_1.m4s', 'media_1.m3u8']].forEach(([init, list], src) => {
  for (const p of [`${a}/${init}`, ...segments(`${a}/${list}`)]) {
    const b = read(p);
    abBytes += b.length;
    ab.push([src, boxes(b)]);
  }
});
const ts = readFileSync('testdata/ts/mux_1080/src0/list.txt', 'utf8').split('\n').filter(Boolean).map(read);
const vk = readdirSync('testdata/vk').filter((n) => n.startsWith('seg-')).sort().map((n) => read(`testdata/vk/${n}`));
const pad = (n, w) => String(n).padStart(w, '0');
const clock = (ms) => `${pad(Math.floor(ms / 3600000), 2)}:${pad(Math.floor(ms / 60000) % 60, 2)}:${pad(Math.floor(ms / 1000) % 60, 2)},${pad(ms % 1000, 3)}`;
let srt = '';
for (let i = 0; i < 50000; i++) srt += `${i + 1}\n${clock(i * 700)} --> ${clock(i * 700 + 2500)}\nСтрока <i>${i}</i>\n\n`;
const srtBytes = new TextEncoder().encode(srt);
const sum = (v) => v.reduce((n, b) => n + b.length, 0);

function renderAll(plan, files) {
  let n = plan.header.length;
  plan.pieces.forEach((p, i) => {
    n += p.render ? plan.render(i, files[p.render.file]).length : p.copy ? p.copy.len : p.data.len;
  });
  plan.free();
  return n;
}

const work = [
  ['fmp4 episode plan', abBytes, () => {
    const rm = um.remuxer();
    rm.addSource();
    rm.addSource();
    ab.forEach(([src, bx], id) => bx.forEach(([off, b]) => rm.feed(src, id, off, b)));
    const plan = rm.finish();
    rm.free();
    const size = plan.size;
    plan.free();
    return size;
  }],
  ['ts 1080p remux', sum(ts), () => {
    const rm = um.remuxer();
    const s = rm.addSource();
    ts.forEach((f, i) => rm.feedTs(s, i, f));
    const plan = rm.finish();
    rm.free();
    return renderAll(plan, ts);
  }],
  ['ts mp3 to .mp3', sum(vk), () => {
    const rm = um.remuxer();
    const s = rm.addSource();
    vk.forEach((f, i) => rm.feedTs(s, i, f));
    const plan = rm.finishMp3();
    rm.free();
    return renderAll(plan, vk);
  }],
  ['50k subtitle cues', srtBytes.length, () => {
    const rm = um.remuxer();
    const s = rm.addSource();
    rm.feedSubtitles(s, srtBytes);
    const plan = rm.finish();
    rm.free();
    const size = plan.size;
    plan.free();
    return size;
  }],
];

console.log('workload\truns\tmin ms\tmedian ms\tmax ms\tinput MB\tMB/s\tout');
for (const [name, input, f] of work) {
  const out = f();
  const t = [];
  for (let r = 0; r < runs; r++) {
    const s = performance.now();
    f();
    t.push(performance.now() - s);
  }
  t.sort((x, y) => x - y);
  const med = t[Math.floor(t.length / 2)];
  console.log(`${name}\t${runs}\t${t[0].toFixed(1)}\t${med.toFixed(1)}\t${t.at(-1).toFixed(1)}\t${(input / 1e6).toFixed(1)}\t${(input / 1e6 / (med / 1000)).toFixed(0)}\t${out}`);
}
