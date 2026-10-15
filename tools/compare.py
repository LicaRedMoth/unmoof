#!/usr/bin/env python3
"""Compares two files packet by packet: MD5, size and keyframe flag of every packet, and
presentation times up to a constant shift per stream, which it reports. Uses ffprobe.
With --no-sync, how video moves against audio is reported but not held against the files."""
import json, subprocess, sys

def packets(path):
    out = subprocess.run(
        ['ffprobe', '-v', 'error', '-show_streams', '-show_packets', '-show_data_hash', 'MD5',
         '-show_entries', 'stream=index,codec_type,codec_name,start_time,duration,nb_frames:'
         'packet=stream_index,pts_time,dts_time,duration_time,size,flags,data_hash',
         '-of', 'json', path], capture_output=True, text=True, check=True).stdout
    d = json.loads(out)
    # Streams pair up by kind and order (the second audio with the second audio), so a track
    # one side has and the other lacks, such as captions, does not shift the rest.
    key, seen = {}, {}
    for s in d['streams']:
        kind = s.get('codec_type', '?')
        key[s['index']] = f'{kind}{seen.get(kind, 0)}'
        seen[kind] = seen.get(kind, 0) + 1
    streams = {key[s['index']]: s for s in d['streams']}
    per = {}
    for p in d['packets']:
        per.setdefault(key[p['stream_index']], []).append(p)
    return streams, per

def f(x):
    return None if x in (None, 'N/A') else float(x)

def main(a, b, sync_ms=0.0, tol=0.0015, judge_sync=True):
    sa, pa = packets(a)
    sb, pb = packets(b)
    ok = True
    offsets = {}
    for i in sorted(set(pa) | set(pb)):
        if i not in pa or i not in pb:
            print(f'stream {i} is only in the {"first" if i in pa else "second"} file')
            if i.startswith(('video', 'audio')):
                ok = False
            continue
        A, B = pa.get(i, []), pb.get(i, [])
        kind = sa.get(i, sb.get(i, {})).get('codec_type')
        print(f'stream {i} ({kind}): {len(A)} vs {len(B)} packets; '
              f'start {sa.get(i, {}).get("start_time")} vs {sb.get(i, {}).get("start_time")}; '
              f'duration {sa.get(i, {}).get("duration")} vs {sb.get(i, {}).get("duration")}')
        if len(A) != len(B):
            ok = False
        bad = {'hash': 0, 'size': 0, 'key': 0, 'pts': 0}
        # A whole stream moved in time is told apart from packets out of step with each other.
        diffs = sorted(f(y['pts_time']) - f(x['pts_time']) for x, y in zip(A, B)
                       if f(x.get('pts_time')) is not None and f(y.get('pts_time')) is not None)
        offset = diffs[len(diffs) // 2] if diffs else 0.0
        offsets.setdefault(kind, offset)
        worst = 0.0
        first = None
        for n, (x, y) in enumerate(zip(A, B)):
            errs = []
            if x.get('data_hash') != y.get('data_hash'): bad['hash'] += 1; errs.append('hash')
            if x['size'] != y['size']: bad['size'] += 1; errs.append('size')
            if ('K' in x['flags']) != ('K' in y['flags']): bad['key'] += 1; errs.append('key')
            u, v = f(x.get('pts_time')), f(y.get('pts_time'))
            if (u is None) != (v is None) or (u is not None and abs(v - u - offset) > tol):
                bad['pts'] += 1; errs.append('pts')
            if u is not None and v is not None:
                worst = max(worst, abs(v - u - offset))
            if errs and first is None:
                first = (n, errs, x, y)
        print(f'  mismatches {bad}; stream offset {offset * 1000:+.3f} ms; '
              f'worst deviation from it {worst * 1000:.3f} ms')
        if first:
            n, errs, x, y = first
            ok = False
            print(f'  first at packet {n}: {errs}\n    {x}\n    {y}')
    if 'video' in offsets and 'audio' in offsets:
        sync = (offsets['video'] - offsets['audio']) * 1000
        print(f'video moves {sync:+.3f} ms against audio from the first file to the second'
              + (f' ({sync_ms:+.3f} ms expected)' if sync_ms else ''))
        if judge_sync and abs(sync - sync_ms) > tol * 1000:
            ok = False
    print('SAME' if ok else 'DIFFERENT')
    return ok

if __name__ == '__main__':
    # An optional third argument: how far video is known to move against audio, in ms.
    args = [x for x in sys.argv[1:] if x != '--no-sync']
    sys.exit(0 if main(args[0], args[1], *map(float, args[2:3]), judge_sync='--no-sync' not in sys.argv) else 1)
