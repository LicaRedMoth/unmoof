// unmoof.js: loads unmoof.wasm and wraps it. Works in browsers, extensions and Node.
//
//   const um = await init(await fetch('unmoof.wasm'));
//   const rm = um.remuxer();
//   const v = rm.addSource();
//   for (const [i, f] of files.entries()) await rm.feedFile(v, i, f.size, (o, n) => read(f, o, n));
//   const plan = rm.finish();
//   // the file is plan.header, then each of plan.pieces in order:
//   //   { copy: { file, offset, len } }  bytes of input file `file`, as they are
//   //   { render: { file, len, index } } plan.render(index, wholeTsFile)

/** Loads the module from a Response, URL, ArrayBuffer, typed array or WebAssembly.Module. */
export async function init(source) {
  let instance;
  if (source instanceof WebAssembly.Module) {
    instance = await WebAssembly.instantiate(source, {});
  } else {
    if (typeof source === 'string' || source instanceof URL) source = await fetch(source);
    if (typeof Response !== 'undefined' && source instanceof Response) source = await source.arrayBuffer();
    ({ instance } = await WebAssembly.instantiate(source, {}));
  }
  return new Unmoof(instance.exports);
}

const BOXES_READ = new Set(['moov', 'moof']);

class Unmoof {
  constructor(x) {
    this.x = x;
  }

  /** 'fmp4', 'ts' or null, judged from the first bytes of a file (64 KiB is plenty). */
  sniff(head) {
    return [null, 'fmp4', 'ts'][this.#with(head, (p, n) => this.x.unmoof_sniff(p, n))];
  }

  remuxer() {
    return new Remuxer(this);
  }

  // Copies bytes into module memory for the length of a call.
  #with(bytes, fn) {
    const x = this.x;
    const p = x.unmoof_alloc(bytes.length);
    new Uint8Array(x.memory.buffer, p, bytes.length).set(bytes);
    try {
      return fn(p, bytes.length);
    } finally {
      x.unmoof_dealloc(p, bytes.length);
    }
  }

  withBytes(bytes, fn) {
    return this.#with(bytes, fn);
  }

  copyOut(ptr, len) {
    return new Uint8Array(this.x.memory.buffer, ptr, len).slice();
  }

  text(ptr, len) {
    return new TextDecoder().decode(new Uint8Array(this.x.memory.buffer, ptr, len));
  }
}

class Remuxer {
  #um;
  #h;

  constructor(um) {
    this.#um = um;
    this.#h = um.x.unmoof_new();
  }

  #check(status) {
    if (status !== 0) {
      const x = this.#um.x;
      throw new Error('unmoof: ' + this.#um.text(x.unmoof_buf_ptr(this.#h), x.unmoof_buf_len(this.#h)));
    }
  }

  /** Opens a source: one stream, such as the video of a DASH manifest, and returns its index. */
  addSource() {
    return this.#um.x.unmoof_add_source(this.#h);
  }

  /** How long a chunk may run, which is how finely video and audio interleave. */
  setChunkMs(ms) {
    this.#um.x.unmoof_set_chunk_ms(this.#h, ms);
  }

  /** Feeds whole top-level boxes of an fMP4 file, found at `offset` in input file `file`. */
  feed(source, file, offset, bytes) {
    this.#check(this.#um.withBytes(bytes, (p, n) => this.#um.x.unmoof_feed(this.#h, source, file, offset, p, n)));
  }

  /** Feeds a whole MPEG-TS file. */
  feedTs(source, file, bytes) {
    this.#check(this.#um.withBytes(bytes, (p, n) => this.#um.x.unmoof_feed_ts(this.#h, source, file, p, n)));
  }

  /**
   * Feeds input file `file` of `size` bytes, reading only what is needed: the moov and moof boxes
   * of fMP4, or the whole of an MPEG-TS file. `readAt(offset, length)` resolves to the bytes.
   * Returns 'fmp4' or 'ts'.
   */
  async feedFile(source, file, size, readAt) {
    const head = await readAt(0, Math.min(size, 65536));
    const kind = this.#um.sniff(head);
    if (kind === 'ts') {
      this.feedTs(source, file, size <= head.length ? head : await readAt(0, size));
      return kind;
    }
    if (kind !== 'fmp4') throw new Error(`unmoof: input file ${file} is neither fMP4 nor MPEG-TS`);
    let pos = 0;
    while (pos < size) {
      const h = await readAt(pos, Math.min(16, size - pos));
      if (h.length < 8) throw new Error(`unmoof: a box header at byte ${pos} of input file ${file} is cut short`);
      const dv = new DataView(h.buffer, h.byteOffset, h.byteLength);
      const type = String.fromCharCode(h[4], h[5], h[6], h[7]);
      let boxSize = dv.getUint32(0);
      if (boxSize === 1) {
        if (h.length < 16) throw new Error(`unmoof: a box header at byte ${pos} of input file ${file} is cut short`);
        boxSize = Number(dv.getBigUint64(8));
      } else if (boxSize === 0) {
        boxSize = size - pos;
      }
      if (boxSize < 8 || pos + boxSize > size) {
        if (type === 'mdat') break; // its samples are checked against the file size later
        throw new Error(`unmoof: box '${type}' at byte ${pos} of input file ${file} runs past its end; is the download cut short?`);
      }
      if (BOXES_READ.has(type)) this.feed(source, file, pos, await readAt(pos, boxSize));
      pos += boxSize;
    }
    return kind;
  }

  /** Lays the file out. The remuxer takes no more input afterwards. */
  finish() {
    const x = this.#um.x;
    const p = x.unmoof_finish(this.#h);
    if (p === 0) this.#check(1);
    return new Plan(this.#um, p);
  }

  free() {
    this.#um.x.unmoof_drop(this.#h);
  }
}

class Plan {
  #um;
  #p;

  constructor(um, p) {
    this.#um = um;
    this.#p = p;
    const x = um.x;
    this.header = um.copyOut(x.unmoof_plan_header_ptr(p), x.unmoof_plan_header_len(p));
    x.unmoof_plan_info(p);
    const info = JSON.parse(um.text(x.unmoof_plan_buf_ptr(p), x.unmoof_plan_buf_len(p)));
    this.size = info.size;
    this.tracks = info.tracks;
    this.warnings = info.warnings;
    this.pieces = info.pieces.map((v, index) =>
      v[0] === 0 ? { copy: { file: v[1], offset: v[2], len: v[3] } } : { render: { file: v[1], len: v[2], index } });
  }

  /** The bytes of a render piece, cut out of `tsFile`, the whole MPEG-TS input file it names. */
  render(index, tsFile) {
    const um = this.#um, x = um.x;
    const status = um.withBytes(tsFile, (ptr, n) => x.unmoof_plan_render(this.#p, index, ptr, n));
    const out = um.copyOut(x.unmoof_plan_buf_ptr(this.#p), x.unmoof_plan_buf_len(this.#p));
    if (status !== 0) throw new Error('unmoof: ' + new TextDecoder().decode(out));
    return out;
  }

  free() {
    this.#um.x.unmoof_plan_drop(this.#p);
  }
}
