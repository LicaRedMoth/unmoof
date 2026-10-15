#!/usr/bin/env python3
"""Where each video and audio stream of a fetched stream should start, on the clock its
renditions share, worked out from the specifications alone: unmoof's output is checked against
it, since ffmpeg cannot be the judge here. It starts every input at zero unless told -copyts,
and with -copyts it shows video late by any negative composition offset.

usage: sync.py WORKDIR   (the OUTDIR of fetch.py: src0, src1, ... with list.txt and pto.txt)

Prints JSON: {"video": [seconds, ...], "audio": [seconds, ...], "damaged": bool} in the order
of the sources; damaged is set when some picture shows more than 10 s from its decoding, which
no encoder does: the spec then says nothing about where the stream starts.
  fMP4: the earliest tfdt + duration + composition offset of a track, through its edit list
        (ISO 14496-12 8.6.6), less the DASH presentationTimeOffset.
  TS:   the earliest PTS from the first keyframe on, in the first file, via ffprobe.
  HLS packed audio: the transportStreamTimestamp in the ID3 tag of the first file.
"""
import json, os, struct, subprocess, sys


def boxes(b, p=0, end=None):
    end = len(b) if end is None else end
    while p + 8 <= end:
        size, t = struct.unpack('>I4s', b[p:p + 8])
        h = 8
        if size == 1:
            size, h = struct.unpack('>Q', b[p + 8:p + 16])[0], 16
        elif size == 0:
            size = end - p
        if size < h or p + size > end:
            return
        yield t, b[p + h:p + size]
        p += size


def child(payload, name):
    return next((d for t, d in boxes(payload) if t == name), None)


def full(d):
    return d[0], int.from_bytes(d[1:4], 'big'), d[4:]


def init_tracks(b):
    """{track id: {kind, timescale, delay (s), media_time or None, default_duration}}"""
    moov = child(b, b'moov')
    if moov is None:
        return {}
    v, _, mvhd = full(child(moov, b'mvhd'))
    movie_ts = struct.unpack('>I', mvhd[8:12] if v == 0 else mvhd[16:20])[0]
    tracks = {}
    for t, trak in boxes(moov):
        if t != b'trak':
            continue
        v, _, tkhd = full(child(trak, b'tkhd'))
        tid = struct.unpack('>I', tkhd[8:12] if v == 0 else tkhd[16:20])[0]
        mdia = child(trak, b'mdia')
        v, _, mdhd = full(child(mdia, b'mdhd'))
        ts = struct.unpack('>I', mdhd[8:12] if v == 0 else mdhd[16:20])[0]
        handler = full(child(mdia, b'hdlr'))[2][4:8]
        delay, media_time = 0, None
        edts = child(trak, b'edts')
        if edts is not None and child(edts, b'elst') is not None:
            v, _, e = full(child(edts, b'elst'))
            n, w = struct.unpack('>I', e[:4])[0], 12 if v == 0 else 20
            for i in range(n):
                dur, mt, _ = struct.unpack('>IiI' if v == 0 else '>QqI', e[4 + i * w:4 + (i + 1) * w])
                if mt == -1:
                    delay += dur
                else:
                    media_time = mt
                    break
        kind = {b'vide': 'video', b'soun': 'audio'}.get(handler)
        if kind:
            tracks[tid] = {'kind': kind, 'ts': ts, 'delay': delay / movie_ts, 'media_time': media_time,
                           'default_dur': 0}
    mvex = child(moov, b'mvex')
    for t, d in boxes(mvex or b''):
        if t == b'trex':
            tid, _, dur = struct.unpack('>III', d[4:16])
            if tid in tracks:
                tracks[tid]['default_dur'] = dur
    return tracks


def fmp4_starts(files):
    tracks = init_tracks(open(files[0], 'rb').read())
    first_pts, next_dts = {}, {}
    for f in files:
        b = open(f, 'rb').read()
        for t, moof in boxes(b):
            if t != b'moof':
                continue
            for t2, traf in boxes(moof):
                if t2 != b'traf':
                    continue
                _, fl, tfhd = full(child(traf, b'tfhd'))
                tid = struct.unpack('>I', tfhd[:4])[0]
                if tid not in tracks:
                    continue
                p, dflt = 4, tracks[tid]['default_dur']
                for bit, n in ((0x01, 8), (0x02, 4)):
                    p += n if fl & bit else 0
                if fl & 0x08:
                    dflt = struct.unpack('>I', tfhd[p:p + 4])[0]
                tfdt = child(traf, b'tfdt')
                if tfdt is not None:
                    v, _, d = full(tfdt)
                    next_dts[tid] = struct.unpack('>I' if v == 0 else '>Q', d[:4] if v == 0 else d[:8])[0]
                dts = next_dts.get(tid, 0)
                for t3, trun in boxes(traf):
                    if t3 != b'trun':
                        continue
                    v, fl, d = full(trun)
                    count, p = struct.unpack('>I', d[:4])[0], 4
                    p += (4 if fl & 0x01 else 0) + (4 if fl & 0x04 else 0)
                    for _ in range(count):
                        dur, cto = dflt, 0
                        if fl & 0x100:
                            dur = struct.unpack('>I', d[p:p + 4])[0]
                            p += 4
                        p += (4 if fl & 0x200 else 0) + (4 if fl & 0x400 else 0)
                        if fl & 0x800:
                            cto = struct.unpack('>i' if v else '>I', d[p:p + 4])[0]
                            p += 4
                        first_pts[tid] = min(first_pts.get(tid, dts + cto), dts + cto)
                        dts += dur
                next_dts[tid] = dts
    out = []
    for tid, t in tracks.items():
        if tid not in first_pts:
            continue
        pts = first_pts[tid]
        if t['media_time'] is None:
            start = pts / t['ts']
        else:
            start = (max(pts, t['media_time']) - t['media_time']) / t['ts'] + t['delay']
        out.append((t['kind'], start))
    return out, False


def ts_starts(path):
    r = subprocess.run(['ffprobe', '-v', 'error', '-show_entries',
                        'stream=index,codec_type,time_base:packet=stream_index,pts,dts,flags', '-of', 'json', path],
                       capture_output=True, text=True)
    if r.returncode != 0:
        return [], False
    d = json.loads(r.stdout)
    out, damaged = [], False
    for s in d['streams']:
        if s['codec_type'] not in ('video', 'audio'):
            continue
        num, den = map(int, s['time_base'].split('/'))
        pk = [p for p in d['packets'] if p['stream_index'] == s['index'] and p.get('pts') not in (None, 'N/A')]
        if s['codec_type'] == 'video':
            k = next((i for i, p in enumerate(pk) if 'K' in p['flags']), None)
            pk = pk[k:] if k is not None else []
            sane = [p for p in pk if abs(int(p['pts']) - int(p.get('dts') or p['pts'])) * num / den <= 10]
            damaged |= len(sane) < len(pk)
            pk = sane
        if pk:
            out.append((s['codec_type'], min(int(p['pts']) for p in pk) * num / den))
    return out, damaged


def packed_start(b):
    owner = b'com.apple.streaming.transportStreamTimestamp\0'
    i = b.find(owner, 0, 4096)
    if not b.startswith(b'ID3') or i < 0:
        return [], False
    t = struct.unpack('>Q', b[i + len(owner):i + len(owner) + 8])[0] & ((1 << 33) - 1)
    return [('audio', t / 90000)], False


def starts(work):
    out = {'video': [], 'audio': [], 'damaged': False}
    for d in sorted(x for x in os.listdir(work) if x.startswith('src')):
        files = open(os.path.join(work, d, 'list.txt')).read().split()
        head = open(files[0], 'rb').read(8)
        if head[4:8] in (b'ftyp', b'moov', b'styp'):
            found, damaged = fmp4_starts(files)
        elif head.startswith(b'ID3') and b'transportStreamTimestamp' in open(files[0], 'rb').read(4096):
            found, damaged = packed_start(open(files[0], 'rb').read(4096))
        else:
            found, damaged = ts_starts(files[0])
        out['damaged'] |= damaged
        pto = 0.0
        if os.path.exists(os.path.join(work, d, 'pto.txt')):
            ticks, scale = open(os.path.join(work, d, 'pto.txt')).read().split('/')
            pto = int(ticks) / int(scale)
        for kind, s in found:
            out[kind].append(s - pto)
    return out


if __name__ == '__main__':
    if len(sys.argv) != 2:
        sys.exit(__doc__)
    print(json.dumps(starts(sys.argv[1])))
