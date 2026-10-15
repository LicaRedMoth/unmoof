#!/usr/bin/env python3
"""Runs unmoof over the public test streams of dash.js, Shaka Player and hls.js.

usage: corpus.py [SECONDS]

For each stream: fetch the first SECONDS (tools/fetch.py), remux with unmoof, remux the same
segments with ffmpeg, compare packet by packet (tools/compare.py), check where audio starts
against video by the specifications (tools/sync.py), delete the files. Results go to
out/corpus/results.jsonl, one line per stream, and the run resumes where it stopped.
out/corpus/report.md sums them up. Live, DRM and SAMPLE-AES streams are skipped by design.
"""
import json, os, re, shutil, subprocess, sys, urllib.request

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
OUT = os.path.join(ROOT, 'out', 'corpus')
LISTS = {
    'dash.js': 'https://reference.dashif.org/dash.js/latest/samples/dash-if-reference-player/app/sources.json',
    'shaka': 'https://raw.githubusercontent.com/shaka-project/shaka-player/main/demo/common/assets.js',
    'hls.js': 'https://raw.githubusercontent.com/video-dev/hls.js/master/tests/test-streams.js',
}


def list_text(name):
    path = os.path.join(OUT, 'lists', name.replace('.', '_'))
    if not os.path.exists(path):
        os.makedirs(os.path.dirname(path), exist_ok=True)
        with urllib.request.urlopen(LISTS[name], timeout=60) as r:
            open(path, 'wb').write(r.read())
    return open(path, encoding='utf8').read()


def streams():
    out = []
    d = json.loads(list_text('dash.js'))
    for group in d['items']:
        g = group['name']
        for s in group.get('submenu', []):
            skip = any(k in g for k in ('DRM', 'LIVE', 'Live', 'Smooth')) or 'protData' in s
            out.append(('dash.js', f"{g} / {s.get('name', '')}", s.get('url', ''), 'SKIP live or DRM by list' if skip else None))
    js = list_text('shaka')
    for chunk in js.split('new ShakaDemoAssetInfo(')[1:]:
        m = re.search(r"'(https?://[^']+?\.(?:mpd|m3u8)[^']*)'", chunk)
        name = (re.search(r"'([^']+)'", chunk) or [None, '?'])[1]
        if not m:
            continue
        skip = re.search(r'addLicenseServer|Feature\.LIVE|addKeySystem|clearKeys|OFFLINE', chunk)
        out.append(('shaka', name, m.group(1), 'SKIP live or DRM by list' if skip else None))
    js = list_text('hls.js')
    for m in re.finditer(r"url:\s*'([^']+)'", js):
        tail = js[m.end():m.end() + 400].split('url:')[0]
        name = re.findall(r"(\w+):\s*\{[^{}]*$", js[:m.start()])
        skip = 'live: true' in tail or 'drm' in tail.lower()
        out.append(('hls.js', name[-1] if name else '?', m.group(1), 'SKIP live or DRM by list' if skip else None))
    seen, unique = set(), []
    for s in out:
        if s[2] and s[2] not in seen:
            seen.add(s[2])
            unique.append(s)
    return unique


def run(cmd, timeout):
    try:
        p = subprocess.run(cmd, cwd=ROOT, capture_output=True, text=True, timeout=timeout)
        return p.returncode, (p.stdout + p.stderr).strip()
    except subprocess.TimeoutExpired:
        return -1, f'timeout after {timeout} s'


def sync_check(work, ours):
    """(desynced, note): audio against video in unmoof's output, against tools/sync.py."""
    rc, out = run(['python3', 'tools/sync.py', work], 300)
    if rc != 0:
        return False, 'sync not judged: ' + out[-200:]
    want = json.loads(out)
    rc, out = run(['ffprobe', '-v', 'error', '-show_entries', 'stream=codec_type,start_time', '-of', 'csv=p=0', ours], 60)
    got = {}
    for line in out.split():
        kind, _, t = line.partition(',')
        if t not in ('', 'N/A'):
            got.setdefault(kind, []).append(float(t))
    if not (want['video'] and want['audio'] and got.get('video') and got.get('audio')):
        return False, 'sync: no video and audio to set against each other'
    if want['damaged']:
        return False, 'sync not judged: some pictures carry absurd times'
    off = (got['audio'][0] - got['video'][0]) - (want['audio'][0] - want['video'][0])
    if abs(off) > 0.001:
        return True, f'audio sits {off * 1000:+.3f} ms off where the specs put it against video'
    return False, f'sync as the specs put it ({off * 1000:+.3f} ms)'


def check(origin, name, url, seconds):
    if url.startswith('//'):
        url = 'https:' + url
    work = os.path.join(OUT, 'work')
    shutil.rmtree(work, ignore_errors=True)
    rc, msg = run(['python3', 'tools/fetch.py', url, work, str(seconds)], 600)
    if rc != 0:
        reason = msg.splitlines()[-1] if msg else 'fetch failed'
        return ('SKIPPED' if 'SKIP' in reason else 'FETCH_FAILED'), reason[:300]
    srcs = sorted(d for d in os.listdir(work) if d.startswith('src'))
    lists = [open(os.path.join(work, d, 'list.txt')).read().split() for d in srcs]
    args = []
    for d, files in zip(srcs, lists):
        args += ['-s'] + files
        pto = os.path.join(work, d, 'pto.txt')
        if os.path.exists(pto):
            args += ['--pto', open(pto).read().strip()]
    ours = os.path.join(work, 'unmoof.mp4')
    rc, msg = run(['target/release/unmoof', '-o', ours] + args, 300)
    if rc != 0:
        return 'UNMOOF_ERROR', msg[-400:]
    warnings = [l.strip() for l in msg.splitlines() if 'warning' in l]
    desynced, sync_note = sync_check(work, ours)
    refin, maps = [], []
    for i, files in enumerate(lists):
        cat = os.path.join(work, f'cat{i}')
        with open(cat, 'wb') as o:
            for f in files:
                o.write(open(f, 'rb').read())
        refin += ['-i', cat]
        maps += ['-map', f'{i}:v?', '-map', f'{i}:a?']
    ref = os.path.join(work, 'ffmpeg.mp4')
    # Without -copyts ffmpeg starts every input at zero, losing the clock separate renditions share.
    copyts = ['-copyts'] if len(lists) > 1 else []
    rc, fmsg = run(['ffmpeg', '-v', 'error', '-y'] + copyts + refin + maps + ['-c', 'copy', '-f', 'mp4', ref], 300)
    if rc != 0:
        # No reference to compare with: at least the output must decode cleanly.
        rc, dmsg = run(['ffmpeg', '-v', 'error', '-i', ours, '-f', 'null', '-'], 300)
        status = 'DECODES' if rc == 0 and not dmsg else 'DECODE_ERRORS'
        detail = f'{sync_note}; ffmpeg cannot remux it: {fmsg[:200]}; {dmsg[:200]}'
        return ('DESYNC' if desynced else status), detail
    # ffmpeg is the judge of every packet, not of where the streams start against each other.
    rc, cmp = run(['python3', 'tools/compare.py', ref, ours, '--no-sync'], 600)
    detail = sync_note + ' | ' + ' | '.join(
        l.strip() for l in cmp.splitlines() if 'mismatch' in l or 'moves' in l or 'only in' in l)
    if warnings:
        detail += ' || ' + ' | '.join(warnings)[:300]
    status = 'SAME' if 'SAME' in cmp.splitlines()[-1:] else 'DIFFERENT'
    return ('DESYNC' if desynced else status), detail[:900]


def report(results):
    from collections import Counter
    c = Counter(r['status'] for r in results)
    lines = ['# unmoof against public test streams', '', '| status | streams |', '|---|---|']
    lines += [f'| {k} | {v} |' for k, v in c.most_common()]
    for status in ('DESYNC', 'DIFFERENT', 'UNMOOF_ERROR', 'DECODE_ERRORS', 'DECODES', 'FETCH_FAILED', 'SKIPPED'):
        rows = [r for r in results if r['status'] == status]
        if rows:
            lines += ['', f'## {status} ({len(rows)})', '']
            lines += [f"- **{r['origin']}** {r['name']}: `{r['url']}`  \n  {r['detail']}" for r in rows]
    open(os.path.join(OUT, 'report.md'), 'w').write('\n'.join(lines) + '\n')
    return c


def main(seconds=20):
    os.makedirs(OUT, exist_ok=True)
    path = os.path.join(OUT, 'results.jsonl')
    done = {json.loads(l)['url'] for l in open(path)} if os.path.exists(path) else set()
    todo = streams()
    print(f'{len(todo)} streams, {len(done)} done before', flush=True)
    for i, (origin, name, url, skip) in enumerate(todo):
        if url in done:
            continue
        status, detail = ('SKIPPED', skip) if skip else check(origin, name, url, seconds)
        with open(path, 'a') as o:
            o.write(json.dumps({'origin': origin, 'name': name, 'url': url, 'status': status, 'detail': detail}) + '\n')
        print(f'[{i + 1}/{len(todo)}] {status:13} {origin:8} {name[:60]}', flush=True)
    shutil.rmtree(os.path.join(OUT, 'work'), ignore_errors=True)
    results = [json.loads(l) for l in open(path)]
    print(report(results))


if __name__ == '__main__':
    main(*map(int, sys.argv[1:2]))
