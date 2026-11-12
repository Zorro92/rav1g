#!/usr/bin/env python3
"""Smoke test of the timing run's inter breakdown (-gputimedetail, -gputimeisolate) on WARP.

  python testing/conformance/gputime_smoke.py --tool <warp_decoder.exe> --streams <dir> --out <dir>
                                              --stream <name>[:<temporal units>[:scaled]] [--stream ...]
                                              [--threads N]

<dir> of --streams is what testing/conformance/prepare_streams.sh wrote. Each stream is decoded four times,
with its reference check: with -gputime, with -gputimedetail, with -gputimeisolate, and with -gputimedetail
-interfast (the inter blocks by motion vector class). The times of a software rasterizer say nothing about a
GPU and are not looked at beyond being there. What is checked:

  - every run decodes the stream as its reference says (the barriers of -gputimeisolate change no sample);
  - the plain -gputime summary has no line of the breakdown;
  - the breakdown is there, with times for the passes, and in the isolate run with barriers added and a
    time for every dispatch;
  - the two independent sides of the counters agree on every frame (the summary's "inter counters" line:
    the dispatches the recording made against the blocks the tile threads pushed);
  - the numbers are plausible: the luma predicted per frame is not more than the frame, the reference bytes
    loaded are not less than the footprint, the footprint by motion vector is not more than the footprint
    and not less than what is written;
  - the counters of the detail run and of the isolate run are the same (they do not depend on the mode);
  - a stream given with ":scaled" has frames with scaled references, listed as <section>/scaled;
  - with -interfast the sections single and comp_avg are listed by class (single.copy, comp_avg.ch, ...) and
    not at all without one, except for frames with scaled references, which keep the original path; the
    reference bytes loaded are then not less than the footprint by motion vector (they are less than the
    footprint: that is the point), and the block statistics are those of the run without the flag.

Written to --out: <stream>.<mode>.log, and summary.md with the breakdown of every run (also appended to
$GITHUB_STEP_SUMMARY when that is set). Exit code 1 when a check fails, 2 when a run could not be done.
"""
import argparse
import os
import re
import subprocess
import sys

MODES = (('gputime', '-gputime'), ('detail', '-gputimedetail'), ('isolate', '-gputimeisolate'),
         ('fast', '-gputimedetail -interfast'))


def gputime_lines(text):
    return [line[len('GPUTIME '):] for line in text.splitlines() if line.startswith('GPUTIME ')]


def check_run(name, mode, text, area_mpx, scaled, problems):
    """The checks of one run. Returns the counter lines of the breakdown, for the comparison between modes."""
    def bad(what):
        problems.append('%s, %s: %s' % (name, mode, what))

    lines = gputime_lines(text)
    result = re.search(r'^RESULT\s.*?\bstatus=(\S+)', text, re.M)
    if not result or result.group(1) != 'pass':
        bad('the stream did not decode as its reference says: %s' % (result.group(0) if result else 'no RESULT line'))
    if not any(line.startswith('==== summary:') for line in lines):
        bad('no summary')
        return []
    breakdown = [line for line in lines if line.startswith(('-- inter', 'inter ', 'list '))]
    if mode == 'gputime':
        if breakdown:
            bad('the plain timing run has lines of the breakdown: %s' % breakdown[0])
        return []
    head = [line for line in lines if line.startswith('-- inter breakdown')]
    match = head and re.search(r': (\d+) frames with an inter stage of (\d+) measured', head[0])
    if not match or int(match.group(1)) < 1:
        bad('no frame with an inter stage in the breakdown: %s' % (head[0] if head else 'no header'))
        return []
    frames = int(match.group(1))
    if (mode == 'isolate') != ('-gputimeisolate' in head[0]):
        bad('the header names the wrong mode: %s' % head[0])
    if scaled:
        match = re.search(r'(\d+) of them with scaled references', head[0])
        if not match or int(match.group(1)) < 1 or not any(re.match(r'inter \S+/scaled ', line) for line in lines):
            bad('no frame with scaled references, or no section listed as scaled: %s' % head[0])
    agree = [line for line in lines if line.startswith('inter counters')]
    if len(agree) != 1 or 'agree on all %d frames' % frames not in agree[0]:
        bad('the counters of the two sides: %s' % (agree[0] if agree else 'no line'))
    for needed in ('inter pass 1 (to sync0)', 'inter passes 1 to 3', 'inter stage (gpu inter)'):
        row = [line for line in lines if line.startswith(needed)]
        if not row or int(row[0].split()[-7]) != frames:
            bad('%s: %s' % (needed, row[0] if row else 'no line'))
    kinds = set(line.split()[2] for line in lines if re.match(r'inter (single|comp_avg|warp)\S* +(ovl|excl) ', line))
    if kinds != set(['excl' if mode == 'isolate' else 'ovl']):
        bad('the sections have the kinds %s' % sorted(kinds))
    lists = [line for line in lines if line.startswith('list ')]
    if not lists:
        bad('no dispatch lines')
    if mode == 'isolate':
        added = [line for line in lines if line.startswith('isolate mode added per frame')]
        match = added and re.search(r'UAV barriers mean ([\d.]+)', added[0])
        if not match or float(match.group(1)) <= 0.0:
            bad('no barriers added: %s' % (added[0] if added else 'no line'))
        if any(len(line.split()) != 10 for line in lists):
            bad('a dispatch line without a time')
    elif any(len(line.split()) != 7 for line in lists):
        bad('a dispatch line with a time')
    blocks = [line for line in lines if line.startswith('inter blocks per frame')]
    match = blocks and re.search(r': ([\d.]+) blocks, ([\d.]+) Mpx luma', blocks[0])
    if not match or float(match.group(1)) <= 0 or not 0.0 < float(match.group(2)) <= area_mpx:
        bad('blocks and luma per frame (the frame has room for %.3f Mpx): %s' % (area_mpx, blocks[0] if blocks else 'no line'))
    stage = [line for line in lines if line.startswith('inter stage per frame')]
    match = stage and re.search(r'loaded ([\d.]+) MB, footprint ([\d.]+) MB, footprint by motion vector ([\d.]+) MB, '
                                r'samples written ([\d.]+) MB', stage[0])
    if match:
        loaded, foot, foot_mv, written = (float(x) for x in match.groups())
        if mode == 'fast':
            if not (foot >= foot_mv >= written > 0.0 and loaded >= foot_mv):
                bad('footprint >= footprint by motion vector >= written, loaded >= footprint by motion vector does '
                    'not hold: %s' % stage[0])
        elif not (loaded >= foot >= foot_mv >= written > 0.0):
            bad('loaded >= footprint >= footprint by motion vector >= written does not hold: %s' % stage[0])
    else:
        bad('no line of the bytes of the stage')
    if not any(re.match(r'frame \d+ inter mode=%d ' % (2 if mode == 'isolate' else 1), line) for line in lines):
        bad('no per-frame line of the breakdown')
    # -interfast: the sections by class, and none of the two they replace outside frames with scaled references.
    by_class = [line for line in lines if re.match(r'inter (single\.(copy|h|v|2d)|comp_avg\.[chv2]{2}) ', line)]
    plain = [line for line in lines if re.match(r'inter (single|comp_avg) ', line)]
    if mode == 'fast':
        if not by_class:
            bad('-interfast: no section by motion vector class')
        if plain:
            bad('-interfast: a section that is not by class on a frame without scaled references: %s' % plain[0])
    elif by_class:
        bad('a section by motion vector class without -interfast: %s' % by_class[0])
    # What does not depend on the mode: the work table without its last two columns (from the times), the
    # block lines, and the dispatch lines without their times.
    work = [' '.join(line.split()[:14]) for line in lines if re.match(r'inter \S+ +\d+ +[\d.]+ +\d+ +[\d.]+ +\d+ ', line)]
    if not work:
        bad('no line in the work table')
    return work + blocks + [line for line in lines if line.startswith(('inter reference fetches', 'inter filter'))] + \
        [' '.join(line.split()[:7]) for line in lists]


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument('--tool', required=True)
    parser.add_argument('--streams', required=True)
    parser.add_argument('--out', required=True)
    parser.add_argument('--stream', action='append', required=True)
    parser.add_argument('--threads', type=int, default=4)
    args = parser.parse_args()
    tool = os.path.abspath(args.tool)
    out = os.path.abspath(args.out)
    os.makedirs(out, exist_ok=True)

    manifest = {}
    with open(os.path.join(args.streams, 'manifest.txt'), encoding='utf-8') as f:
        for line in f:
            name = re.search(r'(?:^|\s)-name\s+(\S+)', line)
            if name:
                manifest[name.group(1)] = line.split()

    problems = []
    summary = ['## Inter breakdown of the timing run on WARP', '',
               'A smoke test: the times are those of a CPU rasterizer and say nothing about a GPU; the counters are '
               'the stream\'s.', '']
    for item in args.stream:
        name, limit, expect = (item.split(':') + ['', ''])[:3]
        if expect not in ('', 'scaled'):
            print('ERROR --stream %s: not <name>[:<temporal units>[:scaled]]' % item)
            return 2
        if name not in manifest:
            print('ERROR %s is not a stream of the manifest' % name)
            return 2
        counters = {}
        for mode, flag in MODES:
            command = [tool] + manifest[name] + ['-t', str(args.threads)] + flag.split() + ['-progress']
            if limit:
                command += ['-limit', limit]
            run = subprocess.run(command, cwd=args.streams, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
            text = run.stdout.decode('utf-8', 'replace').replace('\r', '')
            with open(os.path.join(out, '%s.%s.log' % (name, mode)), 'w', encoding='utf-8') as f:
                f.write(text)
            if run.returncode != 0:
                problems.append('%s, %s: exit code %d' % (name, mode, run.returncode))
            size = re.search(r'TASK \S+: stream (\d+)x(\d+)', text)
            # A block at the right or the lower edge is predicted whole: room for a superblock more each way.
            area = (int(size.group(1)) + 128) * (int(size.group(2)) + 128) / 1e6 if size else 0.0
            counters[mode] = check_run(name, mode, text, area, expect == 'scaled', problems)
            lines = gputime_lines(text)
            shown = [line for line in lines if line.startswith(('-- inter', 'inter ', 'list ', 'excl', 'is from', 'isolate',
                                                                 'timestamps added', 'gpu inter', 'gpu recon'))]
            print('%s %s: exit code %d, %d GPUTIME lines, %d of the breakdown' % (name, flag, run.returncode, len(lines),
                                                                                 len(shown)))
            if mode != 'gputime':
                summary += ['### %s, %s%s' % (name, flag, ', %s temporal units' % limit if limit else ''), '', '```']
                summary += shown + ['```', '']
        if counters.get('detail') != counters.get('isolate'):
            differing = [a for a, b in zip(counters['detail'], counters['isolate']) if a != b][:1]
            problems.append('%s: the counters of -gputimedetail and -gputimeisolate differ%s' % (
                name, ': ' + differing[0] if differing else ' in their number of lines'))
        # The blocks of the stream do not depend on how they are listed.
        stream_lines = [[line for line in counters.get(mode, [])
                         if line.startswith(('inter blocks per frame', 'inter reference fetches', 'inter filter'))]
                        for mode in ('detail', 'fast')]
        if stream_lines[0] != stream_lines[1]:
            differing = [a for a, b in zip(stream_lines[0], stream_lines[1]) if a != b][:1]
            problems.append('%s: the block statistics with and without -interfast differ%s' % (
                name, ': ' + differing[0] if differing else ' in their number of lines'))
    summary += ['Checks: ' + ('all passed.' if not problems else '%d failed.' % len(problems)), '']
    summary += ['- ' + problem for problem in problems]
    text = '\n'.join(summary) + '\n'
    with open(os.path.join(out, 'summary.md'), 'w', encoding='utf-8') as f:
        f.write(text)
    if os.environ.get('GITHUB_STEP_SUMMARY'):
        with open(os.environ['GITHUB_STEP_SUMMARY'], 'a', encoding='utf-8') as f:
            f.write(text)
    for problem in problems:
        print('FAILED ' + problem)
    print('gputime_smoke.py: %d streams, %d problems' % (len(args.stream), len(problems)))
    return 1 if problems else 0


if __name__ == '__main__':
    sys.exit(main())
