#!/usr/bin/env python3
"""Downloads the start of an HLS or DASH stream as separate files, for testing unmoof.

usage: fetch.py URL OUTDIR SECONDS [CODEC]

Picks the video rendition of the highest bandwidth and the audio rendition that goes with it
(the default one of its HLS group, or the DASH audio of the highest bandwidth), and fetches the
init segment and media segments until SECONDS of media are in. Each rendition lands in its own
directory, src0, src1, ..., with list.txt naming its files in order: one -s of unmoof each,
and pto.txt holding the DASH presentationTimeOffset as TICKS/TIMESCALE when it is not zero.
Video of the codec named (avc1 when none is) is preferred over other video.
"""
import os, re, struct, subprocess, sys, time, urllib.parse, urllib.request
import xml.etree.ElementTree as ET

UA = 'unmoof-test-fetch/0.1'


def get(url, rng=None):
    for attempt in range(4):
        try:
            req = urllib.request.Request(url, headers={'User-Agent': UA})
            if rng:
                req.add_header('Range', 'bytes=%d-%d' % rng)
            with urllib.request.urlopen(req, timeout=60) as r:
                return r.read()
        except Exception as e:
            if attempt == 3:
                raise RuntimeError(f'{url}: {e}')
            time.sleep(2)


def attrs(line):
    return {k: v.strip('"') for k, v in re.findall(r'([A-Z0-9-]+)=("[^"]*"|[^,]*)', line)}


def byterange(spec, prev_end):
    n, _, off = spec.partition('@')
    off = int(off) if off else prev_end
    return off, off + int(n) - 1


def hls_media(url, seconds):
    """[(url, byte range or None, AES-128 (key url, iv) or None)] for the init segment and
    segments of an HLS media playlist."""
    text = get(url).decode()
    if '#EXT-X-ENDLIST' not in text and '#EXT-X-PLAYLIST-TYPE:VOD' not in text:
        raise RuntimeError('SKIP live playlist')
    lines = text.splitlines()
    out, dur, total, prev_end, rng, key, seq = [], None, 0.0, 0, None, None, 0
    for line in lines:
        if line.startswith('#EXT-X-MEDIA-SEQUENCE:'):
            seq = int(line.split(':')[1])
        if line.startswith('#EXT-X-KEY'):
            a = attrs(line)
            if a.get('METHOD') == 'AES-128':
                key = (urllib.parse.urljoin(url, a['URI']), a.get('IV'))
            elif a.get('METHOD', 'NONE') != 'NONE':
                raise RuntimeError(f"SKIP encrypted ({a.get('METHOD')})")
            else:
                key = None
            continue
        if line.startswith('#EXT-X-MAP'):
            a = attrs(line)
            r = byterange(a['BYTERANGE'], 0) if 'BYTERANGE' in a else None
            out.append((urllib.parse.urljoin(url, a['URI']), r, None))
        elif line.startswith('#EXTINF:'):
            dur = float(line[8:].split(',')[0])
        elif line.startswith('#EXT-X-BYTERANGE:'):
            rng = byterange(line[17:], prev_end)
        elif line and not line.startswith('#'):
            iv = key and (key[1] or '0x%032x' % seq)
            out.append((urllib.parse.urljoin(url, line), rng, key and (key[0], iv)))
            seq += 1
            if rng:
                prev_end = rng[1] + 1
            rng = None
            total += dur or 0
            if total >= seconds:
                break
    return out


def hls(url, seconds, codec):
    text = get(url).decode()
    if '#EXT-X-STREAM-INF' not in text:
        return [hls_media(url, seconds)]
    lines = text.splitlines()
    variants, audio = [], {}
    for i, line in enumerate(lines):
        if line.startswith('#EXT-X-STREAM-INF'):
            variants.append((attrs(line), urllib.parse.urljoin(url, lines[i + 1])))
        elif line.startswith('#EXT-X-MEDIA') and 'TYPE=AUDIO' in line:
            a = attrs(line)
            if 'URI' in a:
                audio.setdefault(a['GROUP-ID'], []).append(a)
    preferred = [v for v in variants if codec in v[0].get('CODECS', '')] or variants
    best, vurl = max(preferred, key=lambda v: int(v[0].get('BANDWIDTH', 0)))
    print('video:', best.get('RESOLUTION'), best.get('CODECS'), best.get('BANDWIDTH'))
    out = [hls_media(vurl, seconds)]
    group = audio.get(best.get('AUDIO'), [])
    if group:
        a = next((a for a in group if a.get('DEFAULT') == 'YES'), group[0])
        print('audio:', a.get('NAME'), a.get('LANGUAGE'))
        out.append(hls_media(urllib.parse.urljoin(url, a['URI']), seconds))
    return out


def dur_s(iso):
    m = re.match(r'P(?:(\d+)D)?T?(?:(\d+)H)?(?:(\d+)M)?(?:([\d.]+)S)?', iso)
    d, h, mi, s = (float(x) if x else 0 for x in m.groups())
    return d * 86400 + h * 3600 + mi * 60 + s


def dash(url, seconds, codec):
    raw = get(url)
    root = ET.fromstring(raw)
    if root.get('type') == 'dynamic':
        raise RuntimeError('SKIP live (dynamic MPD)')
    if b'ContentProtection' in raw:
        raise RuntimeError('SKIP DRM (ContentProtection)')
    ns = {'d': root.tag[1:].split('}')[0]} if root.tag.startswith('{') else {'d': ''}
    q = lambda e, tag: e.find(f'd:{tag}', ns) if ns['d'] else e.find(tag)
    qa = lambda e, tag: e.findall(f'd:{tag}', ns) if ns['d'] else e.findall(tag)
    base = url
    b = q(root, 'BaseURL')
    if b is not None:
        base = urllib.parse.urljoin(base, b.text.strip())
    period = qa(root, 'Period')[0]
    b = q(period, 'BaseURL')
    if b is not None:
        base = urllib.parse.urljoin(base, b.text.strip())
    total = dur_s(root.get('mediaPresentationDuration') or period.get('duration') or 'PT0S')
    picks = {}
    for aset in qa(period, 'AdaptationSet'):
        for rep in qa(aset, 'Representation'):
            mime = rep.get('mimeType') or aset.get('mimeType') or ''
            kind = aset.get('contentType') or mime.split('/')[0]
            codecs = rep.get('codecs') or aset.get('codecs') or ''
            if kind not in ('video', 'audio'):
                continue
            if kind == 'video' and not codecs.startswith(codec) and any(
                    (r.get('codecs') or aset.get('codecs') or '').startswith(codec)
                    for a2 in qa(period, 'AdaptationSet') for r in qa(a2, 'Representation')):
                continue
            bw = int(rep.get('bandwidth', 0))
            if kind not in picks or bw > picks[kind][0]:
                picks[kind] = (bw, aset, rep, codecs)
    out, ptos = [], []
    for kind in ('video', 'audio'):
        if kind not in picks:
            continue
        bw, aset, rep, codecs = picks[kind]
        print(f'{kind}: id={rep.get("id")} {codecs} {bw}')
        rbase = base
        for e in (aset, rep):
            b = q(e, 'BaseURL')
            if b is not None:
                rbase = urllib.parse.urljoin(rbase, b.text.strip())
        tmpl = q(rep, 'SegmentTemplate')
        if tmpl is None:
            tmpl = q(aset, 'SegmentTemplate')
        sb = q(rep, 'SegmentBase')
        if sb is None:
            sb = q(aset, 'SegmentBase')
        if tmpl is not None:
            ts = int(tmpl.get('timescale', 1))

            def fill(t, number=None, time_=None):
                def sub(m):
                    name, fmt = m.group(1), m.group(2)
                    v = {'RepresentationID': rep.get('id'), 'Bandwidth': rep.get('bandwidth'),
                         'Number': number, 'Time': time_}[name]
                    return (fmt % int(v)) if fmt else str(v)
                return urllib.parse.urljoin(rbase, re.sub(r'\$(\w+)(%0\d+d)?\$', sub, t))

            files = [(fill(tmpl.get('initialization')), None)]
            n = int(tmpl.get('startNumber', 1))
            tl = q(tmpl, 'SegmentTimeline')
            got = 0.0
            if tl is not None:
                t = 0
                for s in qa(tl, 'S'):
                    t = int(s.get('t', t))
                    # A negative repeat count runs to the end of the period.
                    r = int(s.get('r', 0))
                    reps = r + 1 if r >= 0 else int(-(-(total or seconds) * ts // int(s.get('d'))))
                    for _ in range(reps):
                        files.append((fill(tmpl.get('media'), n, t), None))
                        t += int(s.get('d'))
                        n += 1
                        got += int(s.get('d')) / ts
                        if got >= seconds:
                            break
                    if got >= seconds:
                        break
            else:
                d = int(tmpl.get('duration')) / ts
                count = int(-(-min(seconds, total or seconds) // d))
                files += [(fill(tmpl.get('media'), n + i), None) for i in range(count)]
            out.append(files)
        elif (sl := q(rep, 'SegmentList') if q(rep, 'SegmentList') is not None else q(aset, 'SegmentList')) is not None:
            ts = int(sl.get('timescale', 1))
            d = int(sl.get('duration', 0)) / ts if sl.get('duration') else 0
            files = []
            ini = q(sl, 'Initialization')
            if ini is not None:
                files.append((urllib.parse.urljoin(rbase, ini.get('sourceURL') or ''), rng_of(ini.get('range'))))
            got = 0.0
            for su in qa(sl, 'SegmentURL'):
                files.append((urllib.parse.urljoin(rbase, su.get('media') or ''), rng_of(su.get('mediaRange'))))
                got += d or 2
                if got >= seconds:
                    break
            out.append(files)
        elif sb is not None or q(rep, 'BaseURL') is not None:
            out.append(sidx_ranges(rbase, sb, seconds))
        else:
            raise RuntimeError(f'{kind}: no SegmentTemplate, SegmentList, SegmentBase or BaseURL')
        segs = [q(e, t) for e in (rep, aset) for t in ('SegmentTemplate', 'SegmentList', 'SegmentBase')]
        inherit = lambda name, default: next((e.get(name) for e in segs if e is not None and e.get(name)), default)
        pto = int(inherit('presentationTimeOffset', 0))
        ptos.append(f"{pto}/{inherit('timescale', 1)}" if pto else None)
    return out, ptos


def rng_of(spec):
    if not spec:
        return None
    a, b = spec.split('-')
    return int(a), int(b)


def sidx_refs(box, at):
    """(first byte, last byte, seconds, is a sidx) of each reference of the sidx box `box`,
    which lies at byte `at` of its file."""
    v = box[8]
    ts = struct.unpack('>I', box[16:20])[0]
    o = 20 + (8 if v == 0 else 16)
    first = int.from_bytes(box[o - (4 if v == 0 else 8):o], 'big')
    count = struct.unpack('>H', box[o + 2:o + 4])[0]
    pos, o, out = at + len(box) + first, o + 4, []
    for _ in range(count):
        ref, dur = struct.unpack('>II', box[o:o + 8])
        n = ref & 0x7fffffff
        out.append((pos, pos + n - 1, dur / ts, bool(ref >> 31)))
        pos += n
        o += 12
    return out


def sidx_ranges(url, sb, seconds):
    """Byte ranges of one file that hold its init segment and the subsegments of the first
    SECONDS, read from its sidx (looked for in the first 64 KiB when the MPD gives no
    indexRange), so a whole movie is not downloaded. A sidx may point at further sidx boxes."""
    index = sb.get('indexRange') if sb is not None else None
    head = get(url, (0, rng_of(index)[1] if index else 65535))
    p = 0
    while p + 8 <= len(head):
        size, t = struct.unpack('>I4s', head[p:p + 8])
        if size < 8:
            break
        if t == b'sidx':
            if p + size > len(head):
                head = get(url, (0, p + size - 1))
            # The init segment stops before the index: ffmpeg cannot read a sidx that points at sidx.
            files, got = [(url, (0, p - 1))], 0.0
            todo = sidx_refs(head[p:p + size], p)
            while todo and got < seconds:
                a, b, dur, nested = todo.pop(0)
                if nested:
                    n = struct.unpack('>I', get(url, (a, a + 7))[:4])[0]
                    todo = sidx_refs(get(url, (a, a + n - 1)), a) + todo
                else:
                    files.append((url, (a, b)))
                    got += dur
            return files
        p += size
    raise RuntimeError(f'{url}: no sidx in its first {len(head)} bytes')


def main(url, outdir, seconds, codec='avc1'):
    seconds = float(seconds)
    if '.mpd' in url:
        sources, ptos = dash(url, seconds, codec)
    else:
        sources = hls(url, seconds, codec)
        ptos = [None] * len(sources)
    for i, (files, pto) in enumerate(zip(sources, ptos)):
        d = os.path.join(outdir, f'src{i}')
        os.makedirs(d, exist_ok=True)
        if pto:
            with open(os.path.join(d, 'pto.txt'), 'w') as f:
                f.write(pto + '\n')
        names = []
        keys = {}
        for k, item in enumerate(files):
            u, rng, aes = (item + (None,))[:3]
            name = f'{k:05d}' + (os.path.splitext(urllib.parse.urlparse(u).path)[1] or '.bin')
            path = os.path.join(d, name)
            if not os.path.exists(path):
                data = get(u, rng)
                if aes:
                    key = keys.setdefault(aes[0], get(aes[0]))
                    iv = aes[1][2:] if aes[1].lower().startswith('0x') else aes[1]
                    data = subprocess.run(['openssl', 'aes-128-cbc', '-d', '-K', key.hex(), '-iv', iv.zfill(32)],
                                          input=data, capture_output=True, check=True).stdout
                with open(path + '.part', 'wb') as f:
                    f.write(data)
                os.replace(path + '.part', path)
            names.append(path)
        with open(os.path.join(d, 'list.txt'), 'w') as f:
            f.write('\n'.join(names) + '\n')
        print(f'src{i}: {len(names)} files')


if __name__ == '__main__':
    if len(sys.argv) not in (4, 5):
        sys.exit(__doc__)
    main(*sys.argv[1:])
