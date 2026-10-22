#!/usr/bin/env python3
"""Runs unmoof over the public test streams of dash.js, Shaka Player and hls.js: packets judged
against ffmpeg, sync against tools/sync.py. Results go to out/corpus/results.jsonl and report.md.

usage: corpus.py [SECONDS] [--again URL-PART...]   (--again runs the streams named again)
"""
import json
import os
import re
import shutil
import subprocess
import sys
import urllib.request
from collections import Counter, namedtuple

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
OUT = os.path.join(ROOT, 'out', 'corpus')
LISTS = {
    'dash.js': 'https://reference.dashif.org/dash.js/latest/samples/dash-if-reference-player/app/sources.json',
    'shaka': 'https://raw.githubusercontent.com/shaka-project/shaka-player/main/demo/common/assets.js',
    'hls.js': 'https://raw.githubusercontent.com/video-dev/hls.js/master/tests/test-streams.js',
}
SKIP_BY_LIST = 'SKIP live or DRM by list'
STATUS_ORDER = ('DESYNC', 'DIFFERENT', 'UNMOOF_ERROR', 'DECODE_ERRORS', 'DECODES', 'FETCH_FAILED', 'SKIPPED')

Stream = namedtuple('Stream', 'origin name url skip')


def list_text(name):
    """A player's list of test streams, downloaded once and kept in out/corpus/lists."""
    path = os.path.join(OUT, 'lists', name.replace('.', '_'))
    if not os.path.exists(path):
        os.makedirs(os.path.dirname(path), exist_ok=True)
        with urllib.request.urlopen(LISTS[name], timeout=60) as response:
            data = response.read()
        with open(path, 'wb') as f:
            f.write(data)
    return open(path, encoding='utf8').read()


def dash_js_streams():
    found = []
    for group in json.loads(list_text('dash.js'))['items']:
        title = group['name']
        for s in group.get('submenu', []):
            skip = any(k in title for k in ('DRM', 'LIVE', 'Live', 'Smooth')) or 'protData' in s
            found.append(Stream('dash.js', f"{title} / {s.get('name', '')}", s.get('url', ''),
                                SKIP_BY_LIST if skip else None))
    return found


def shaka_streams():
    found = []
    for chunk in list_text('shaka').split('new ShakaDemoAssetInfo(')[1:]:
        url = re.search(r"'(https?://[^']+?\.(?:mpd|m3u8)[^']*)'", chunk)
        if not url:
            continue
        name = re.search(r"'([^']+)'", chunk)
        skip = re.search(r'addLicenseServer|Feature\.LIVE|addKeySystem|clearKeys|OFFLINE', chunk)
        found.append(Stream('shaka', name.group(1) if name else '?', url.group(1),
                            SKIP_BY_LIST if skip else None))
    return found


def hls_js_streams():
    js = list_text('hls.js')
    found = []
    for m in re.finditer(r"url:\s*'([^']+)'", js):
        # The stream's own settings run from its url to the next url.
        settings = js[m.end():m.end() + 400].split('url:')[0]
        names = re.findall(r"(\w+):\s*\{[^{}]*$", js[:m.start()])
        skip = 'live: true' in settings or 'drm' in settings.lower()
        found.append(Stream('hls.js', names[-1] if names else '?', m.group(1),
                            SKIP_BY_LIST if skip else None))
    return found


def streams():
    """Every stream of the three lists, each URL once."""
    seen, unique = set(), []
    for s in dash_js_streams() + shaka_streams() + hls_js_streams():
        if s.url and s.url not in seen:
            seen.add(s.url)
            unique.append(s)
    return unique


def run(command, timeout):
    """(exit code, stdout and stderr) of a command run from the repository root."""
    try:
        p = subprocess.run(command, cwd=ROOT, capture_output=True, text=True, timeout=timeout)
        return p.returncode, (p.stdout + p.stderr).strip()
    except subprocess.TimeoutExpired:
        return -1, f'timeout after {timeout} s'


def renditions(work):
    """[(directory, files)] for each rendition fetch.py saved."""
    found = []
    for name in sorted(os.listdir(work)):
        if name.startswith('src'):
            directory = os.path.join(work, name)
            found.append((directory, open(os.path.join(directory, 'list.txt')).read().split()))
    return found


def is_hevc_ts(path):
    """Whether a file is MPEG-TS holding HEVC, which unmoof writes as hvc1."""
    with open(path, 'rb') as f:
        head = f.read(8)
    if head[4:8] in (b'ftyp', b'moov', b'styp'):
        return False
    _, codecs = run(['ffprobe', '-v', 'error', '-select_streams', 'v', '-show_entries', 'stream=codec_name',
                     '-of', 'csv=p=0', path], 60)
    return codecs.splitlines()[:1] == ['hevc']


def ffmpeg_reference(work, sources, out):
    """Remuxes the joined files of each rendition with ffmpeg; (exit code, messages)."""
    inputs, maps = [], []
    for i, (_, files) in enumerate(sources):
        joined = os.path.join(work, f'cat{i}')
        with open(joined, 'wb') as o:
            for path in files:
                with open(path, 'rb') as f:
                    o.write(f.read())
        inputs += ['-i', joined]
        maps += ['-map', f'{i}:v?', '-map', f'{i}:a?']

    # Without -copyts ffmpeg starts every input at zero, losing the clock separate renditions share.
    options = ['-copyts'] if len(sources) > 1 else []
    if any(is_hevc_ts(os.path.join(work, f'cat{i}')) for i in range(len(sources))):
        options += ['-bsf:v', 'filter_units=remove_types=32|33|34', '-tag:v', 'hvc1']
    command = ['ffmpeg', '-v', 'error', '-y'] + options + inputs + maps + ['-c', 'copy', '-f', 'mp4', out]
    return run(command, 300)


def spec_sync(work, ours):
    """(desynced, note): where audio starts against video in our output, by tools/sync.py."""
    rc, out = run(['python3', 'tools/sync.py', work], 300)
    if rc != 0:
        return False, 'sync not judged: ' + out[-200:]
    want = json.loads(out)

    _, out = run(['ffprobe', '-v', 'error', '-show_entries', 'stream=codec_type,start_time',
                  '-of', 'csv=p=0', ours], 60)
    got = {}
    for line in out.split():
        kind, _, start = line.partition(',')
        if start not in ('', 'N/A'):
            got.setdefault(kind, []).append(float(start))

    if not (want['video'] and want['audio'] and got.get('video') and got.get('audio')):
        return False, 'sync: no video and audio to set against each other'
    if want['damaged']:
        return False, 'sync not judged: some pictures carry absurd times'
    off = (got['audio'][0] - got['video'][0]) - (want['audio'][0] - want['video'][0])
    if abs(off) > 0.001:
        return True, f'audio sits {off * 1000:+.3f} ms off where the specs put it against video'
    return False, f'sync as the specs put it ({off * 1000:+.3f} ms)'


def check(url, seconds):
    """(status, detail) for one stream: fetch, remux, judge."""
    if url.startswith('//'):
        url = 'https:' + url
    work = os.path.join(OUT, 'work')
    shutil.rmtree(work, ignore_errors=True)

    rc, msg = run(['python3', 'tools/fetch.py', url, work, str(seconds)], 600)
    if rc != 0:
        reason = msg.splitlines()[-1] if msg else 'fetch failed'
        return ('SKIPPED' if 'SKIP' in reason else 'FETCH_FAILED'), reason[:300]

    sources = renditions(work)
    args = []
    for directory, files in sources:
        args += ['-s'] + files
        pto = os.path.join(directory, 'pto.txt')
        if os.path.exists(pto):
            args += ['--pto', open(pto).read().strip()]
    ours = os.path.join(work, 'unmoof.mp4')
    rc, msg = run(['target/release/unmoof', '-o', ours] + args, 300)
    if rc != 0:
        return 'UNMOOF_ERROR', msg[-400:]
    warnings = [line.strip() for line in msg.splitlines() if 'warning' in line]
    desynced, sync_note = spec_sync(work, ours)

    ref = os.path.join(work, 'ffmpeg.mp4')
    rc, ffmpeg_msg = ffmpeg_reference(work, sources, ref)
    if rc != 0:
        # No reference to compare with: at least the output must decode cleanly.
        rc, decode_msg = run(['ffmpeg', '-v', 'error', '-i', ours, '-f', 'null', '-'], 300)
        status = 'DECODES' if rc == 0 and not decode_msg else 'DECODE_ERRORS'
        detail = f'{sync_note}; ffmpeg cannot remux it: {ffmpeg_msg[:200]}; {decode_msg[:200]}'
        return ('DESYNC' if desynced else status), detail

    # ffmpeg judges every packet, not where the streams start against each other.
    _, compared = run(['python3', 'tools/compare.py', ref, ours, '--no-sync'], 600)
    lines = compared.splitlines()
    findings = [line.strip() for line in lines if 'mismatch' in line or 'moves' in line or 'only in' in line]
    detail = sync_note + ' | ' + ' | '.join(findings)
    if warnings:
        detail += ' || ' + ' | '.join(warnings)[:300]
    status = 'SAME' if lines[-1:] == ['SAME'] else 'DIFFERENT'
    return ('DESYNC' if desynced else status), detail[:900]


def report(results):
    """Writes report.md: a count per status, then every stream that is not SAME."""
    counts = Counter(r['status'] for r in results)
    lines = ['# unmoof against public test streams', '', '| status | streams |', '|---|---|']
    for status, n in counts.most_common():
        lines.append(f'| {status} | {n} |')

    for status in STATUS_ORDER:
        rows = [r for r in results if r['status'] == status]
        if not rows:
            continue
        lines += ['', f'## {status} ({len(rows)})', '']
        for r in rows:
            lines.append(f"- **{r['origin']}** {r['name']}: `{r['url']}`  \n  {r['detail']}")

    with open(os.path.join(OUT, 'report.md'), 'w') as f:
        f.write('\n'.join(lines) + '\n')
    return counts


def load(path):
    if not os.path.exists(path):
        return []
    return [json.loads(line) for line in open(path)]


def save(path, results):
    with open(path, 'w') as f:
        for r in results:
            f.write(json.dumps(r) + '\n')


def main(args):
    again = args[args.index('--again') + 1:] if '--again' in args else []
    numbers = args[:args.index('--again')] if '--again' in args else args
    seconds = int(numbers[0]) if numbers else 20

    os.makedirs(OUT, exist_ok=True)
    path = os.path.join(OUT, 'results.jsonl')
    results = load(path)
    if again:
        for r in results:
            if any(part in r['url'] for part in again):
                r['status'], r['detail'] = check(r['url'], seconds)
                print(f"{r['status']:13} {r['origin']:8} {r['name'][:60]}", flush=True)
        save(path, results)
    else:
        done = {r['url'] for r in results}
        todo = streams()
        print(f'{len(todo)} streams, {len(done)} done before', flush=True)
        for i, s in enumerate(todo):
            if s.url in done:
                continue
            status, detail = ('SKIPPED', s.skip) if s.skip else check(s.url, seconds)
            result = {'origin': s.origin, 'name': s.name, 'url': s.url, 'status': status, 'detail': detail}
            results.append(result)
            with open(path, 'a') as f:
                f.write(json.dumps(result) + '\n')
            print(f'[{i + 1}/{len(todo)}] {status:13} {s.origin:8} {s.name[:60]}', flush=True)

    shutil.rmtree(os.path.join(OUT, 'work'), ignore_errors=True)
    print(report(results))


if __name__ == '__main__':
    main(sys.argv[1:])
