#!/usr/bin/env python3
"""Where each video and audio stream of a fetch.py download should start, worked out from the specs
alone, so unmoof's sync is not judged by ffmpeg, which places separate inputs its own way.

usage: sync.py WORKDIR   (prints JSON: {"video": [s, ...], "audio": [s, ...], "damaged": bool})
"""
import json
import os
import struct
import subprocess
import sys

from isobmff import boxes, child, children, full_box

# No encoder shows a picture this many seconds after decoding it; such a time is damaged.
MAX_REORDER = 10


def be32(data, at):
    return struct.unpack('>I', data[at:at + 4])[0]


def time_field(payload, version, at):
    """A 32-bit field in version 0 boxes, 64-bit in version 1; at counts in version 0."""
    if version == 0:
        return be32(payload, at)
    return struct.unpack('>Q', payload[at:at + 8])[0]


def edit_list(trak):
    """(seconds of empty edits, media time of the first edit that shows media or None)."""
    edts = child(trak, b'edts')
    elst = child(edts, b'elst') if edts else None
    if elst is None:
        return 0, None

    version, _, body = full_box(elst)
    width = 12 if version == 0 else 20
    empty = 0
    for i in range(be32(body, 0)):
        entry = body[4 + i * width:4 + (i + 1) * width]
        duration, media_time, _ = struct.unpack('>IiI' if version == 0 else '>QqI', entry)
        if media_time != -1:
            return empty, media_time
        empty += duration
    return empty, None


def init_tracks(init):
    """{track id: track} for the video and audio tracks of an init segment."""
    moov = child(init, b'moov')
    if moov is None:
        return {}
    version, _, mvhd = full_box(child(moov, b'mvhd'))
    movie_timescale = be32(mvhd, 8 if version == 0 else 16)

    tracks = {}
    for kind, trak in children(moov):
        if kind != b'trak':
            continue
        version, _, tkhd = full_box(child(trak, b'tkhd'))
        track_id = be32(tkhd, 8 if version == 0 else 16)
        mdia = child(trak, b'mdia')
        version, _, mdhd = full_box(child(mdia, b'mdhd'))
        handler = full_box(child(mdia, b'hdlr'))[2][4:8]
        if handler not in (b'vide', b'soun'):
            continue

        empty, media_time = edit_list(trak)
        tracks[track_id] = {
            'kind': 'video' if handler == b'vide' else 'audio',
            'timescale': be32(mdhd, 8 if version == 0 else 16),
            'delay': empty / movie_timescale,
            'media_time': media_time,
            'default_duration': 0,
        }

    mvex = child(moov, b'mvex')
    for kind, trex in children(mvex or b''):
        if kind != b'trex':
            continue
        track_id, _, duration = struct.unpack('>III', trex[4:16])
        if track_id in tracks:
            tracks[track_id]['default_duration'] = duration
    return tracks


def trun_times(trun, dts, default_duration):
    """(presentation times of the run's samples, the decode time after them)."""
    version, flags, body = full_box(trun)
    count = be32(body, 0)
    p = 4
    if flags & 0x01:  # data_offset
        p += 4
    if flags & 0x04:  # first_sample_flags
        p += 4

    times = []
    for _ in range(count):
        duration, offset = default_duration, 0
        if flags & 0x100:
            duration = be32(body, p)
            p += 4
        if flags & 0x200:  # size
            p += 4
        if flags & 0x400:  # flags
            p += 4
        if flags & 0x800:
            offset = struct.unpack('>i' if version else '>I', body[p:p + 4])[0]
            p += 4
        times.append(dts + offset)
        dts += duration
    return times, dts


def fmp4_starts(files):
    """[(kind, seconds)] for the tracks of an init segment and its media segments."""
    tracks = init_tracks(open(files[0], 'rb').read())
    first, next_dts = {}, {}
    for path in files:
        data = open(path, 'rb').read()
        for kind, p, header, size in boxes(data):
            if kind != b'moof':
                continue
            for kind2, traf in children(data[p + header:p + size]):
                if kind2 != b'traf':
                    continue
                _, flags, tfhd = full_box(child(traf, b'tfhd'))
                track_id = be32(tfhd, 0)
                if track_id not in tracks:
                    continue

                # tfhd: base_data_offset, then sample_description_index, then default duration.
                default_duration = tracks[track_id]['default_duration']
                at = 4 + (8 if flags & 0x01 else 0) + (4 if flags & 0x02 else 0)
                if flags & 0x08:
                    default_duration = be32(tfhd, at)

                tfdt = child(traf, b'tfdt')
                if tfdt is not None:
                    version, _, body = full_box(tfdt)
                    next_dts[track_id] = time_field(body, version, 0)
                dts = next_dts.get(track_id, 0)

                for kind3, trun in children(traf):
                    if kind3 != b'trun':
                        continue
                    times, dts = trun_times(trun, dts, default_duration)
                    if times:
                        first[track_id] = min([first.get(track_id, times[0])] + times)
                next_dts[track_id] = dts

    starts = []
    for track_id, t in tracks.items():
        if track_id not in first:
            continue
        pts = first[track_id]
        if t['media_time'] is None:
            start = pts / t['timescale']
        else:
            start = (max(pts, t['media_time']) - t['media_time']) / t['timescale'] + t['delay']
        starts.append((t['kind'], start))
    return starts, False


def ts_starts(path):
    """[(kind, seconds)] for the streams of a TS file, and whether a picture time is damaged."""
    entries = 'stream=index,codec_type,time_base:packet=stream_index,pts,dts,flags'
    probe = subprocess.run(['ffprobe', '-v', 'error', '-show_entries', entries, '-of', 'json', path],
                           capture_output=True, text=True)
    if probe.returncode != 0:
        return [], False
    found = json.loads(probe.stdout)

    starts, damaged = [], False
    for stream in found['streams']:
        kind = stream['codec_type']
        if kind not in ('video', 'audio'):
            continue
        num, den = map(int, stream['time_base'].split('/'))
        packets = []
        for packet in found['packets']:
            if packet['stream_index'] == stream['index'] and packet.get('pts') not in (None, 'N/A'):
                packets.append(packet)

        if kind == 'video':
            # unmoof starts video at its first keyframe, and sets damaged times aside.
            keys = [i for i, packet in enumerate(packets) if 'K' in packet['flags']]
            packets = packets[keys[0]:] if keys else []
            sane = []
            for packet in packets:
                dts = int(packet.get('dts') or packet['pts'])
                if abs(int(packet['pts']) - dts) * num / den <= MAX_REORDER:
                    sane.append(packet)
            damaged = damaged or len(sane) < len(packets)
            packets = sane

        if packets:
            starts.append((kind, min(int(packet['pts']) for packet in packets) * num / den))
    return starts, damaged


def packed_start(head):
    """[('audio', seconds)] from the transportStreamTimestamp ID3 tag of HLS packed audio."""
    owner = b'com.apple.streaming.transportStreamTimestamp\0'
    at = head.find(owner)
    if not head.startswith(b'ID3') or at < 0:
        return [], False
    at += len(owner)
    ticks = struct.unpack('>Q', head[at:at + 8])[0] & ((1 << 33) - 1)
    return [('audio', ticks / 90000)], False


def presentation_offset(directory):
    """The DASH presentationTimeOffset fetch.py saved for a rendition, in seconds."""
    path = os.path.join(directory, 'pto.txt')
    if not os.path.exists(path):
        return 0.0
    ticks, timescale = open(path).read().split('/')
    return int(ticks) / int(timescale)


def starts(work):
    result = {'video': [], 'audio': [], 'damaged': False}
    for name in sorted(os.listdir(work)):
        directory = os.path.join(work, name)
        if not name.startswith('src'):
            continue
        files = open(os.path.join(directory, 'list.txt')).read().split()
        head = open(files[0], 'rb').read(4096)

        if head[4:8] in (b'ftyp', b'moov', b'styp'):
            found, damaged = fmp4_starts(files)
        elif head.startswith(b'ID3') and b'transportStreamTimestamp' in head:
            found, damaged = packed_start(head)
        else:
            found, damaged = ts_starts(files[0])

        result['damaged'] = result['damaged'] or damaged
        offset = presentation_offset(directory)
        for kind, seconds in found:
            result[kind].append(seconds - offset)
    return result


if __name__ == '__main__':
    if len(sys.argv) != 2:
        sys.exit(__doc__)
    print(json.dumps(starts(sys.argv[1])))
