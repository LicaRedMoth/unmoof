#!/usr/bin/env node
// The unmoof command again, on unmoof.wasm under Node: the same arguments and the same file,
// so the module can be checked against the native build byte for byte.
//   node cli.mjs -o OUT.mp4 -s FILE... [-s FILE...]... [--chunk MS]
import { openSync, readSync, fstatSync, writeSync, closeSync, readFileSync, renameSync } from 'node:fs';
import { init } from './unmoof.js';

const args = process.argv.slice(2);
let out = null, chunk = null;
const sources = [];
for (let i = 0; i < args.length; i++) {
  if (args[i] === '-o') out = args[++i];
  else if (args[i] === '--chunk') chunk = Number(args[++i]);
  else if (args[i] === '-s') sources.push([]);
  else if (sources.length) sources.at(-1).push(args[i]);
  else throw new Error(`${args[i]}: a file must follow -s`);
}
if (!out || !sources.length) {
  console.error('usage: node cli.mjs -o OUT.mp4 -s FILE... [-s FILE...]... [--chunk MS]');
  process.exit(2);
}

const t0 = performance.now();
const um = await init(readFileSync(new URL('./unmoof.wasm', import.meta.url)));
const rm = um.remuxer();
if (chunk) rm.setChunkMs(chunk);
const files = [];
for (const group of sources) {
  const s = rm.addSource();
  for (const path of group) {
    const fd = openSync(path, 'r');
    const size = fstatSync(fd).size;
    const readAt = async (offset, len) => {
      const b = Buffer.alloc(len);
      readSync(fd, b, 0, len, offset);
      return new Uint8Array(b.buffer, b.byteOffset, len);
    };
    await rm.feedFile(s, files.length, size, readAt);
    files.push({ path, fd, size });
  }
}
const plan = rm.finish();
const t1 = performance.now();

const part = out + '.part';
const o = openSync(part, 'w');
writeSync(o, plan.header);
let ts = null;
for (const piece of plan.pieces) {
  if (piece.copy) {
    const { file, offset, len } = piece.copy;
    const b = Buffer.alloc(len);
    readSync(files[file].fd, b, 0, len, offset);
    writeSync(o, b);
  } else {
    const { file, index } = piece.render;
    if (!ts || ts.file !== file) ts = { file, bytes: readFileSync(files[file].path) };
    writeSync(o, plan.render(index, ts.bytes));
  }
}
closeSync(o);
renameSync(part, out);
for (const f of files) closeSync(f.fd);
console.error(`${out}: ${(plan.size / 1e6).toFixed(1)} MB, ${plan.pieces.length} pieces, ` +
  `plan in ${(t1 - t0).toFixed(0)} ms, total ${(performance.now() - t0).toFixed(0)} ms`);
for (const w of plan.warnings) console.error('  warning: ' + w);
