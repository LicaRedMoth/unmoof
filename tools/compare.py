#!/usr/bin/env python3
"""Compares two media files packet by packet with ffprobe: MD5, size, keyframe flag and time of
every packet, the times up to one constant shift per stream, which it reports.

usage: compare.py A B [EXPECTED_MS] [--no-sync]   (EXPECTED_MS: how far video is known to move
against audio from A to B; --no-sync reports that move without judging it)
"""
import json
import subprocess
import sys


def packets(path):
    """(streams, packets) by stream key: streams pair up by kind and order, so a track only
    one file has, such as captions, does not shift the rest."""
    entries = ('stream=index,codec_type,codec_name,start_time,duration,nb_frames:'
               'packet=stream_index,pts_time,dts_time,duration_time,size,flags,data_hash')
    command = ['ffprobe', '-v', 'error', '-show_streams', '-show_packets', '-show_data_hash', 'MD5',
               '-show_entries', entries, '-of', 'json', path]
    found = json.loads(subprocess.run(command, capture_output=True, text=True, check=True).stdout)

    keys, seen = {}, {}
    for s in found['streams']:
        kind = s.get('codec_type', '?')
        keys[s['index']] = f'{kind}{seen.get(kind, 0)}'
        seen[kind] = seen.get(kind, 0) + 1

    streams = {keys[s['index']]: s for s in found['streams']}
    by_stream = {}
    for p in found['packets']:
        by_stream.setdefault(keys[p['stream_index']], []).append(p)
    return streams, by_stream


def seconds(value):
    return None if value in (None, 'N/A') else float(value)


def median_shift(a, b):
    """How far the stream moves from a to b: the median difference of packet times."""
    diffs = []
    for x, y in zip(a, b):
        u, v = seconds(x.get('pts_time')), seconds(y.get('pts_time'))
        if u is not None and v is not None:
            diffs.append(v - u)
    diffs.sort()
    return diffs[len(diffs) // 2] if diffs else 0.0


def compare_stream(a, b, shift, tolerance):
    """(mismatch counts, worst time deviation from the shift, the first mismatch or None)."""
    bad = {'hash': 0, 'size': 0, 'key': 0, 'pts': 0}
    worst, first = 0.0, None
    for n, (x, y) in enumerate(zip(a, b)):
        errors = []
        if x.get('data_hash') != y.get('data_hash'):
            errors.append('hash')
        if x['size'] != y['size']:
            errors.append('size')
        if ('K' in x['flags']) != ('K' in y['flags']):
            errors.append('key')

        u, v = seconds(x.get('pts_time')), seconds(y.get('pts_time'))
        if u is not None and v is not None:
            worst = max(worst, abs(v - u - shift))
        if (u is None) != (v is None) or (u is not None and abs(v - u - shift) > tolerance):
            errors.append('pts')

        for e in errors:
            bad[e] += 1
        if errors and first is None:
            first = (n, errors, x, y)
    return bad, worst, first


def main(a, b, sync_ms=0.0, judge_sync=True, tolerance=0.0015):
    streams_a, packets_a = packets(a)
    streams_b, packets_b = packets(b)
    ok = True
    shifts = {}
    for key in sorted(set(packets_a) | set(packets_b)):
        if key not in packets_a or key not in packets_b:
            print(f'stream {key} is only in the {"first" if key in packets_a else "second"} file')
            if key.startswith(('video', 'audio')):
                ok = False
            continue

        pa, pb = packets_a[key], packets_b[key]
        sa, sb = streams_a[key], streams_b[key]
        kind = sa.get('codec_type')
        print(f'stream {key} ({kind}): {len(pa)} vs {len(pb)} packets; '
              f'start {sa.get("start_time")} vs {sb.get("start_time")}; '
              f'duration {sa.get("duration")} vs {sb.get("duration")}')
        if len(pa) != len(pb):
            ok = False

        shift = median_shift(pa, pb)
        shifts.setdefault(kind, shift)
        bad, worst, first = compare_stream(pa, pb, shift, tolerance)
        print(f'  mismatches {bad}; stream offset {shift * 1000:+.3f} ms; '
              f'worst deviation from it {worst * 1000:.3f} ms')
        if first:
            n, errors, x, y = first
            ok = False
            print(f'  first at packet {n}: {errors}\n    {x}\n    {y}')

    if 'video' in shifts and 'audio' in shifts:
        move = (shifts['video'] - shifts['audio']) * 1000
        expected = f' ({sync_ms:+.3f} ms expected)' if sync_ms else ''
        print(f'video moves {move:+.3f} ms against audio from the first file to the second{expected}')
        if judge_sync and abs(move - sync_ms) > tolerance * 1000:
            ok = False

    print('SAME' if ok else 'DIFFERENT')
    return ok


if __name__ == '__main__':
    args = [x for x in sys.argv[1:] if x != '--no-sync']
    if len(args) not in (2, 3):
        sys.exit(__doc__)
    expected = float(args[2]) if len(args) == 3 else 0.0
    sys.exit(0 if main(args[0], args[1], expected, '--no-sync' not in sys.argv) else 1)
