"""The two parsers of the decoder, timed in pairs on one machine (docs/phase2-rav1d.md, step 3).

rav1g-dec decodes each stream with -gputime, once with -parser c and once with -parser rav1d, the two
in turns and each several times, and the CPU timers of the decode thread are read from the summaries:

- entropy_sum, entropy_wall: the parse of the tiles, summed over the tile threads and as wall time. With
  rav1d the sink is inside (the mode info records, the grid, the block lists), and so is rav1d's set-up
  and wrap-up of a frame's parse;
- setup: the frame set-up of the host (a slot, the clears);
- record: the recording of the command list;
- frame: the decode thread from the set-up to the submission, which is what the parser is part of;
- decode call: the whole call for a temporal unit, with the headers.

A run is a pair on one runner: the numbers of other runs are not comparable, the ratio of a pair is. On
WARP the GPU is a CPU rasteriser that competes with the decode thread; the console's numbers decide.

usage: python parser_timing.py --tool <rav1g-dec.exe> --streams <conformance streams> --out <directory>
           --stream <name> [--stream <name> ...] [--rounds N] [--threads N]

Exit code: 0 both parsers gave timers for every stream, 1 a run failed, 2 the check could not be done.
"""

import argparse
import os
import re
import subprocess
import sys

TIMERS = ['entropy_sum', 'entropy_wall', 'setup', 'record', 'frame']
PARSERS = ['c', 'rav1d']


def timers_of(text):
    """{timer: (frames, mean, p50, p95)} of the summary of a timing run, with the decode call."""
    out = {}
    for line in text.splitlines():
        found = re.match(r'GPUTIME cpu (\w+)\s+(\d+)\s+([\d.]+)\s+([\d.]+)\s+([\d.]+)\s+[\d.]+', line)
        if found and found.group(1) in TIMERS:
            out[found.group(1)] = (int(found.group(2)), float(found.group(3)), float(found.group(4)),
                                   float(found.group(5)))
        found = re.match(r'GPUTIME decode call \(aom_codec_decode\)\s+(\d+)\s+([\d.]+)\s+([\d.]+)\s+([\d.]+)', line)
        if found:
            out['decode call'] = (int(found.group(1)), float(found.group(2)), float(found.group(3)),
                                  float(found.group(4)))
    return out


def main():
    parser = argparse.ArgumentParser(description=__doc__.split('\n', 1)[0])
    parser.add_argument('--tool', required=True)
    parser.add_argument('--streams', required=True)
    parser.add_argument('--out', required=True)
    parser.add_argument('--stream', action='append', required=True, help='a stream of the manifest, by name')
    parser.add_argument('--rounds', type=int, default=3)
    parser.add_argument('--threads', type=int, default=1)
    args = parser.parse_args()
    os.makedirs(args.out, exist_ok=True)
    inputs = {}
    try:
        with open(os.path.join(args.streams, 'manifest.txt'), encoding='utf-8') as f:
            for line in f:
                name = re.search(r'(?:^|\s)-name\s+(\S+)', line)
                path = re.search(r'(?:^|\s)-i\s+(\S+)', line)
                if name and path:
                    inputs[name.group(1)] = path.group(1)
    except OSError as error:
        print('parser_timing.py: %s' % error)
        return 2
    tool = os.path.abspath(args.tool)
    failed = False
    lines = ['## The two parsers, timed in pairs (rav1g-dec -gputime -t %d, %d rounds each, WARP)' % (
        args.threads, args.rounds), '',
        'Milliseconds a frame: the mean over the rounds of the means, and the smallest and largest mean of a '
        'round. entropy_sum and entropy_wall with rav1d have the sink inside.', '']
    for stream in args.stream:
        if stream not in inputs:
            print('parser_timing.py: the manifest has no stream %s' % stream)
            return 2
        means = {parser_name: {} for parser_name in PARSERS}
        frames = 0
        for round_number in range(args.rounds):
            # In turns, and the first of a pair in turns too: neither parser always runs on the warmer
            # machine.
            order = PARSERS if round_number % 2 == 0 else PARSERS[::-1]
            for parser_name in order:
                command = [tool, '-name', stream, '-i', inputs[stream], '-gputime', '-t', str(args.threads),
                           '-parser', parser_name, '-timeout', '1200']
                try:
                    done = subprocess.run(command, cwd=args.streams, stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
                                          stdin=subprocess.DEVNULL, timeout=1800)
                except subprocess.TimeoutExpired:
                    print('ERROR %s -parser %s: no end within 30 minutes' % (stream, parser_name))
                    failed = True
                    continue
                text = done.stdout.decode('utf-8', 'replace').replace('\r', '')
                with open(os.path.join(args.out, '%s-%s-%d.log' % (stream, parser_name, round_number)), 'w',
                          encoding='utf-8') as f:
                    f.write(text)
                timers = timers_of(text)
                if done.returncode != 0 or any(timer not in timers for timer in TIMERS + ['decode call']):
                    print('ERROR %s -parser %s: exit code %d, timers %s' % (
                        stream, parser_name, done.returncode, ', '.join(sorted(timers)) or 'none'))
                    failed = True
                    continue
                frames = timers['frame'][0]
                for timer, (_, mean, _, _) in timers.items():
                    means[parser_name].setdefault(timer, []).append(mean)
        lines += ['### %s (%d frames timed)' % (stream, frames), '',
                  '| Timer | -parser c | -parser rav1d | rav1d / c |', '| --- | ---: | ---: | ---: |']
        for timer in TIMERS + ['decode call']:
            cells = []
            averages = []
            for parser_name in PARSERS:
                values = means[parser_name].get(timer, [])
                if not values:
                    cells.append('none')
                    averages.append(None)
                    continue
                average = sum(values) / len(values)
                averages.append(average)
                cells.append('%.3f (%.3f to %.3f)' % (average, min(values), max(values)))
            ratio = '%.2f' % (averages[1] / averages[0]) if averages[0] and averages[1] is not None else '-'
            lines.append('| %s | %s | %s | %s |' % (timer, cells[0], cells[1], ratio))
        lines.append('')
    text = '\n'.join(lines)
    print(text)
    with open(os.path.join(args.out, 'parser-timing.md'), 'w', encoding='utf-8') as f:
        f.write(text + '\n')
    if os.environ.get('GITHUB_STEP_SUMMARY'):
        with open(os.environ['GITHUB_STEP_SUMMARY'], 'a', encoding='utf-8') as f:
            f.write(text + '\n')
    return 1 if failed else 0


if __name__ == '__main__':
    sys.exit(main())
