#!/usr/bin/env python3
"""Downloads the first SECONDS of an HLS or DASH stream, one directory per rendition, to test unmoof.
Each OUTDIR/srcN gets the files, list.txt naming them in order and, for DASH, pto.txt.

usage: fetch.py URL OUTDIR SECONDS [CODEC]   (CODEC: the video to prefer, avc1 by default)
"""
import os
import re
import struct
import subprocess
import sys
import time
import urllib.parse
import urllib.request
import xml.etree.ElementTree as ET
from dataclasses import dataclass

USER_AGENT = 'unmoof-test-fetch/0.1'


@dataclass
class Segment:
    url: str
    byte_range: tuple = None  # (first byte, last byte)
    key: tuple = None  # AES-128: (key url, IV in hex)


def get(url, byte_range=None):
    """The body at url, retried a few times; RuntimeError when it cannot be had."""
    request = urllib.request.Request(url, headers={'User-Agent': USER_AGENT})
    if byte_range:
        request.add_header('Range', 'bytes=%d-%d' % byte_range)

    for attempt in range(4):
        try:
            with urllib.request.urlopen(request, timeout=60) as response:
                return response.read()
        except Exception as e:
            if attempt == 3:
                raise RuntimeError(f'{url}: {e}')
            time.sleep(2)


# HLS


def attributes(line):
    """The KEY=value attributes of a playlist tag, quotes taken off."""
    found = re.findall(r'([A-Z0-9-]+)=("[^"]*"|[^,]*)', line)
    return {key: value.strip('"') for key, value in found}


def parse_byte_range(spec, previous_end):
    """An EXT-X-BYTERANGE value, LENGTH[@OFFSET], as (first byte, last byte)."""
    length, _, offset = spec.partition('@')
    first = int(offset) if offset else previous_end
    return first, first + int(length) - 1


def hls_media(url, seconds):
    """The init segment and the segments of the first SECONDS of a media playlist."""
    text = get(url).decode()
    if '#EXT-X-ENDLIST' not in text and '#EXT-X-PLAYLIST-TYPE:VOD' not in text:
        raise RuntimeError('SKIP live playlist')

    segments = []
    duration, total, previous_end, byte_range, key, sequence = None, 0.0, 0, None, None, 0
    for line in text.splitlines():
        if line.startswith('#EXT-X-MEDIA-SEQUENCE:'):
            sequence = int(line.split(':')[1])
        elif line.startswith('#EXT-X-KEY'):
            a = attributes(line)
            method = a.get('METHOD', 'NONE')
            if method == 'AES-128':
                key = (urllib.parse.urljoin(url, a['URI']), a.get('IV'))
            elif method == 'NONE':
                key = None
            else:
                raise RuntimeError(f'SKIP encrypted ({method})')
        elif line.startswith('#EXT-X-MAP'):
            a = attributes(line)
            init_range = parse_byte_range(a['BYTERANGE'], 0) if 'BYTERANGE' in a else None
            segments.append(Segment(urllib.parse.urljoin(url, a['URI']), init_range))
        elif line.startswith('#EXTINF:'):
            duration = float(line[8:].split(',')[0])
        elif line.startswith('#EXT-X-BYTERANGE:'):
            byte_range = parse_byte_range(line[17:], previous_end)
        elif line and not line.startswith('#'):
            # Without an IV of its own, a segment's IV is its media sequence number.
            segment_key = None
            if key:
                segment_key = (key[0], key[1] or '0x%032x' % sequence)
            segments.append(Segment(urllib.parse.urljoin(url, line), byte_range, segment_key))
            sequence += 1
            if byte_range:
                previous_end = byte_range[1] + 1
            byte_range = None

            total += duration or 0
            if total >= seconds:
                break
    return segments


def hls(url, seconds, codec):
    """The renditions of an HLS stream to fetch: the best video variant and its default audio."""
    text = get(url).decode()
    if '#EXT-X-STREAM-INF' not in text:
        return [hls_media(url, seconds)]

    lines = text.splitlines()
    variants, audio_groups = [], {}
    for i, line in enumerate(lines):
        if line.startswith('#EXT-X-STREAM-INF'):
            variants.append((attributes(line), urllib.parse.urljoin(url, lines[i + 1])))
        elif line.startswith('#EXT-X-MEDIA') and 'TYPE=AUDIO' in line:
            a = attributes(line)
            if 'URI' in a:
                audio_groups.setdefault(a['GROUP-ID'], []).append(a)

    preferred = [v for v in variants if codec in v[0].get('CODECS', '')] or variants
    best, video_url = max(preferred, key=lambda v: int(v[0].get('BANDWIDTH', 0)))
    print('video:', best.get('RESOLUTION'), best.get('CODECS'), best.get('BANDWIDTH'))
    renditions = [hls_media(video_url, seconds)]

    group = audio_groups.get(best.get('AUDIO'), [])
    if group:
        defaults = [a for a in group if a.get('DEFAULT') == 'YES']
        audio = defaults[0] if defaults else group[0]
        print('audio:', audio.get('NAME'), audio.get('LANGUAGE'))
        renditions.append(hls_media(urllib.parse.urljoin(url, audio['URI']), seconds))
    return renditions


# DASH


class Mpd:
    """Finds MPD elements whether or not the document uses the DASH namespace."""

    def __init__(self, root):
        self.prefix = root.tag[:root.tag.index('}') + 1] if root.tag.startswith('{') else ''

    def find(self, element, tag):
        return element.find(self.prefix + tag)

    def findall(self, element, tag):
        return element.findall(self.prefix + tag)

    def first(self, elements, tag):
        """The first of the elements, in order, that has a child of that tag."""
        for e in elements:
            found = self.find(e, tag)
            if found is not None:
                return found
        return None


def iso_seconds(text):
    """An ISO 8601 duration such as PT1H2M3.5S, in seconds."""
    m = re.match(r'P(?:(\d+)D)?T?(?:(\d+)H)?(?:(\d+)M)?(?:([\d.]+)S)?', text)
    days, hours, minutes, secs = (float(x) if x else 0 for x in m.groups())
    return days * 86400 + hours * 3600 + minutes * 60 + secs


def parse_range(spec):
    """An MPD byte range, FIRST-LAST, as a tuple; None for none."""
    if not spec:
        return None
    first, last = spec.split('-')
    return int(first), int(last)


def joined_base(mpd, url, *elements):
    """The base URL after the BaseURL of each element in turn."""
    base = url
    for e in elements:
        b = mpd.find(e, 'BaseURL')
        if b is not None:
            base = urllib.parse.urljoin(base, b.text.strip())
    return base


def pick_representations(mpd, period, codec):
    """{'video': (set, representation), 'audio': ...}: the highest bandwidth of each kind,
    video of the given codec when the period has any."""
    reps = []
    for aset in mpd.findall(period, 'AdaptationSet'):
        for rep in mpd.findall(aset, 'Representation'):
            mime = rep.get('mimeType') or aset.get('mimeType') or ''
            kind = aset.get('contentType') or mime.split('/')[0]
            codecs = rep.get('codecs') or aset.get('codecs') or ''
            reps.append((kind, codecs, int(rep.get('bandwidth', 0)), aset, rep))
    has_codec = any(codecs.startswith(codec) for _, codecs, _, _, _ in reps)

    picks = {}
    for kind, codecs, bandwidth, aset, rep in reps:
        if kind not in ('video', 'audio'):
            continue
        if kind == 'video' and has_codec and not codecs.startswith(codec):
            continue
        if kind not in picks or bandwidth > picks[kind][0]:
            picks[kind] = (bandwidth, codecs, aset, rep)

    return {kind: (aset, rep) for kind, (_, _, aset, rep) in picks.items()}


def template_segments(mpd, template, rep, base, seconds, total):
    """The segments a SegmentTemplate names, by $Number$ or by its SegmentTimeline."""
    def fill(pattern, number=None, start=None):
        values = {'RepresentationID': rep.get('id'), 'Bandwidth': rep.get('bandwidth'),
                  'Number': number, 'Time': start}

        def substitute(m):
            value = values[m.group(1)]
            return m.group(2) % int(value) if m.group(2) else str(value)
        return urllib.parse.urljoin(base, re.sub(r'\$(\w+)(%0\d+d)?\$', substitute, pattern))

    timescale = int(template.get('timescale', 1))
    number = int(template.get('startNumber', 1))
    segments = [Segment(fill(template.get('initialization')))]

    timeline = mpd.find(template, 'SegmentTimeline')
    if timeline is None:
        length = int(template.get('duration')) / timescale
        count = int(-(-min(seconds, total or seconds) // length))
        for i in range(count):
            segments.append(Segment(fill(template.get('media'), number + i)))
        return segments

    start, got = 0, 0.0
    for s in mpd.findall(timeline, 'S'):
        start = int(s.get('t', start))
        length = int(s.get('d'))
        # A negative repeat count runs to the end of the period.
        repeat = int(s.get('r', 0))
        count = repeat + 1 if repeat >= 0 else int(-(-(total or seconds) * timescale // length))
        for _ in range(count):
            segments.append(Segment(fill(template.get('media'), number, start)))
            start += length
            number += 1
            got += length / timescale
            if got >= seconds:
                return segments
    return segments


def list_segments(mpd, segment_list, base, seconds):
    """The segments a SegmentList names."""
    timescale = int(segment_list.get('timescale', 1))
    length = int(segment_list.get('duration', 0)) / timescale

    segments = []
    init = mpd.find(segment_list, 'Initialization')
    if init is not None:
        segments.append(Segment(urllib.parse.urljoin(base, init.get('sourceURL') or ''),
                                parse_range(init.get('range'))))

    got = 0.0
    for item in mpd.findall(segment_list, 'SegmentURL'):
        segments.append(Segment(urllib.parse.urljoin(base, item.get('media') or ''),
                                parse_range(item.get('mediaRange'))))
        got += length or 2
        if got >= seconds:
            break
    return segments


def sidx_references(box, at):
    """(first byte, last byte, seconds, points at a sidx) of each reference of a sidx box that
    lies at byte `at` of its file."""
    version = box[8]
    timescale = struct.unpack('>I', box[16:20])[0]
    p = 28 if version == 0 else 36
    first_offset = int.from_bytes(box[p - (4 if version == 0 else 8):p], 'big')
    count = struct.unpack('>H', box[p + 2:p + 4])[0]

    refs = []
    position, p = at + len(box) + first_offset, p + 4
    for _ in range(count):
        reference, duration = struct.unpack('>II', box[p:p + 8])
        size = reference & 0x7fffffff
        refs.append((position, position + size - 1, duration / timescale, bool(reference >> 31)))
        position += size
        p += 12
    return refs


def sidx_segments(url, segment_base, seconds):
    """Byte ranges of one file: its init segment, then the subsegments of the first SECONDS its
    sidx lists, so a whole movie is not downloaded. A sidx may point at further sidx boxes."""
    index = segment_base.get('indexRange') if segment_base is not None else None
    head = get(url, (0, parse_range(index)[1] if index else 65535))

    # The sidx may run past the bytes read so far; only its header has to be in them.
    p = 0
    while p + 8 <= len(head):
        size, kind = struct.unpack('>I4s', head[p:p + 8])
        if size < 8:
            break
        if kind != b'sidx':
            p += size
            continue
        if p + size > len(head):
            head = get(url, (0, p + size - 1))

        # The init segment stops before the index: ffmpeg cannot read a sidx that points at sidx.
        segments, got = [Segment(url, (0, p - 1))], 0.0
        todo = sidx_references(head[p:p + size], p)
        while todo and got < seconds:
            first, last, duration, nested = todo.pop(0)
            if nested:
                n = struct.unpack('>I', get(url, (first, first + 7))[:4])[0]
                todo = sidx_references(get(url, (first, first + n - 1)), first) + todo
            else:
                segments.append(Segment(url, (first, last)))
                got += duration
        return segments
    raise RuntimeError(f'{url}: no sidx in its first {len(head)} bytes')


def presentation_offset(mpd, rep, aset):
    """The presentationTimeOffset as TICKS/TIMESCALE, looked for on the representation first;
    None when it is zero."""
    found = []
    for e in (rep, aset):
        for tag in ('SegmentTemplate', 'SegmentList', 'SegmentBase'):
            element = mpd.find(e, tag)
            if element is not None:
                found.append(element)

    def inherited(name, default):
        for e in found:
            if e.get(name):
                return e.get(name)
        return default

    ticks = int(inherited('presentationTimeOffset', 0))
    return f"{ticks}/{inherited('timescale', 1)}" if ticks else None


def dash(url, seconds, codec):
    """(renditions, their presentationTimeOffsets) of the first period of a static MPD."""
    raw = get(url)
    root = ET.fromstring(raw)
    if root.get('type') == 'dynamic':
        raise RuntimeError('SKIP live (dynamic MPD)')
    if b'ContentProtection' in raw:
        raise RuntimeError('SKIP DRM (ContentProtection)')

    mpd = Mpd(root)
    period = mpd.findall(root, 'Period')[0]
    total = iso_seconds(root.get('mediaPresentationDuration') or period.get('duration') or 'PT0S')
    picks = pick_representations(mpd, period, codec)

    renditions, offsets = [], []
    for kind in ('video', 'audio'):
        if kind not in picks:
            continue
        aset, rep = picks[kind]
        print(f"{kind}: id={rep.get('id')} {rep.get('codecs') or aset.get('codecs')} {rep.get('bandwidth')}")
        base = joined_base(mpd, url, root, period, aset, rep)

        template = mpd.first((rep, aset), 'SegmentTemplate')
        segment_list = mpd.first((rep, aset), 'SegmentList')
        segment_base = mpd.first((rep, aset), 'SegmentBase')
        if template is not None:
            segments = template_segments(mpd, template, rep, base, seconds, total)
        elif segment_list is not None:
            segments = list_segments(mpd, segment_list, base, seconds)
        elif segment_base is not None or mpd.find(rep, 'BaseURL') is not None:
            segments = sidx_segments(base, segment_base, seconds)
        else:
            raise RuntimeError(f'{kind}: no SegmentTemplate, SegmentList, SegmentBase or BaseURL')

        renditions.append(segments)
        offsets.append(presentation_offset(mpd, rep, aset))
    return renditions, offsets


# Saving


def download(segment, keys):
    """The bytes of a segment, decrypted when it is AES-128; keys caches the keys by URL."""
    data = get(segment.url, segment.byte_range)
    if not segment.key:
        return data

    key_url, iv = segment.key
    if key_url not in keys:
        keys[key_url] = get(key_url)
    if iv.lower().startswith('0x'):
        iv = iv[2:]
    decrypt = ['openssl', 'aes-128-cbc', '-d', '-K', keys[key_url].hex(), '-iv', iv.zfill(32)]
    return subprocess.run(decrypt, input=data, capture_output=True, check=True).stdout


def save(directory, segments, offset):
    """Writes the segments of one rendition, list.txt and pto.txt; files already there stay."""
    os.makedirs(directory, exist_ok=True)
    if offset:
        with open(os.path.join(directory, 'pto.txt'), 'w') as f:
            f.write(offset + '\n')

    names, keys = [], {}
    for k, segment in enumerate(segments):
        extension = os.path.splitext(urllib.parse.urlparse(segment.url).path)[1] or '.bin'
        path = os.path.join(directory, f'{k:05d}{extension}')
        if not os.path.exists(path):
            with open(path + '.part', 'wb') as f:
                f.write(download(segment, keys))
            os.replace(path + '.part', path)
        names.append(path)

    with open(os.path.join(directory, 'list.txt'), 'w') as f:
        f.write('\n'.join(names) + '\n')
    print(f'{os.path.basename(directory)}: {len(names)} files')


def main(url, outdir, seconds, codec='avc1'):
    seconds = float(seconds)
    if '.mpd' in url:
        renditions, offsets = dash(url, seconds, codec)
    else:
        renditions = hls(url, seconds, codec)
        offsets = [None] * len(renditions)

    for i, (segments, offset) in enumerate(zip(renditions, offsets)):
        save(os.path.join(outdir, f'src{i}'), segments, offset)


if __name__ == '__main__':
    if len(sys.argv) not in (4, 5):
        sys.exit(__doc__)
    main(*sys.argv[1:])
