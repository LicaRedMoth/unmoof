#!/usr/bin/env python3
"""Downloads the start of an HLS or DASH stream as separate files, for testing unmoof.

usage: fetch.py URL OUTDIR SECONDS [CODEC]

Picks the video rendition of the highest bandwidth and the audio rendition that goes with it
(the default one of its HLS group, or the DASH audio of the highest bandwidth), and fetches the
init segment and media segments until SECONDS of media are in. Each rendition lands in its own
directory, src0, src1, ..., with list.txt naming its files in order: one -s of unmoof each.
Video of the codec named (avc1 when none is) is preferred over other video.
"""
import os, re, sys, time, urllib.parse, urllib.request
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
    """[(url, byte range or None)] for the init segment and segments of an HLS media playlist."""
    lines = get(url).decode().splitlines()
    out, dur, total, prev_end, rng = [], None, 0.0, 0, None
    for line in lines:
        if line.startswith('#EXT-X-KEY') and 'METHOD=NONE' not in line:
            raise RuntimeError('encrypted stream')
        if line.startswith('#EXT-X-MAP'):
            a = attrs(line)
            r = byterange(a['BYTERANGE'], 0) if 'BYTERANGE' in a else None
            out.append((urllib.parse.urljoin(url, a['URI']), r))
        elif line.startswith('#EXTINF:'):
            dur = float(line[8:].split(',')[0])
        elif line.startswith('#EXT-X-BYTERANGE:'):
            rng = byterange(line[17:], prev_end)
        elif line and not line.startswith('#'):
            out.append((urllib.parse.urljoin(url, line), rng))
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
    root = ET.fromstring(get(url))
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
    out = []
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
                    for _ in range(int(s.get('r', 0)) + 1):
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
        elif sb is not None:
            out.append([(rbase, None)])  # one file holds it all
        else:
            raise RuntimeError(f'{kind}: no SegmentTemplate or SegmentBase')
    return out


def main(url, outdir, seconds, codec='avc1'):
    seconds = float(seconds)
    sources = dash(url, seconds, codec) if '.mpd' in url else hls(url, seconds, codec)
    for i, files in enumerate(sources):
        d = os.path.join(outdir, f'src{i}')
        os.makedirs(d, exist_ok=True)
        names = []
        for k, (u, rng) in enumerate(files):
            name = f'{k:05d}' + (os.path.splitext(urllib.parse.urlparse(u).path)[1] or '.bin')
            path = os.path.join(d, name)
            if not os.path.exists(path):
                data = get(u, rng)
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
