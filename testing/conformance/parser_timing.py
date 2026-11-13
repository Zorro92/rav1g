"""The two parsers of the decoder, timed in pairs on one machine (docs/phase2-rav1d.md, step 3).

rav1g-dec decodes each stream with -gputime, once with -parser c and once with -parser rav1d (the
default of the library, with its default switches nolfmask and tilethread), the two in turns and each
several times, and the CPU timers of the decode thread are read from the summaries. Every --switches
adds a third, fourth, ... decoder to the turns: -parser rav1d with that -parseopt list, e.g. none for
rav1d's parse without the two switches, or time for the default with the calls of the sink timed
(docs/phase2-rav1d.md, "Where the time goes"). The timers:

- entropy_sum, entropy_wall: the parse of the tiles, summed over the tile threads and as wall time. With
  rav1d the sink is inside (the mode info records, the grid, the block lists), and so is rav1d's set-up
  and wrap-up of a frame's parse;
- setup: the frame set-up of the host (a slot, the clears);
- record: the recording of the command list;
- frame: the decode thread from the set-up to the submission, which is what the parser is part of;
- decode call: the whole call for a temporal unit, with the headers;
- sink_lists, sink_calls: parts of entropy_sum with rav1d: the block lists, the calls of the sink
  (-parseopt time).

A run is a pair on one runner: the numbers of other runs are not comparable, the ratio of a pair is. On
WARP the GPU is a CPU rasteriser that competes with the decode thread; the console's numbers decide.

usage: python parser_timing.py --tool <rav1g-dec.exe> --streams <conformance streams> --out <directory>
           --stream <name> [--stream <name> ...] [--rounds N] [--threads N] [--switches <list> ...]

Exit code: 0 both parsers gave timers for every stream, 1 a run failed, 2 the check could not be done.
"""

import argparse
import os
import re
import subprocess
import sys

TIMERS = ['entropy_sum', 'entropy_wall', 'setup', 'record', 'frame']
# Timers of the parser of phase 2: in the table, not required of a run.
SINK_TIMERS = ['sink_lists', 'sink_calls']


def timers_of(text):
    """{timer: (frames, mean, p50, p95)} of the summary of a timing run, with the decode call."""
    out = {}
    for line in text.splitlines():
        found = re.match(r'GPUTIME cpu (\w+)\s+(\d+)\s+([\d.]+)\s+([\d.]+)\s+([\d.]+)\s+[\d.]+', line)
        if found and found.group(1) in TIMERS + SINK_TIMERS:
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
    parser.add_argument('--switches', action='append', default=[],
                        help='a -parseopt list: one more decoder in the turns, -parser rav1d with it')
    args = parser.parse_args()
    # The decoders that take turns: a name for the table and the arguments.
    variants = [('-parser c', ['-parser', 'c']), ('-parser rav1d (default switches)', ['-parser', 'rav1d'])]
    variants += [('rav1d, -parseopt %s' % switches, ['-parser', 'rav1d', '-parseopt', switches])
                 for switches in args.switches]
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
        'round. entropy_sum and entropy_wall with rav1d have the sink inside. The ratios are to -parser c.', '']
    for stream in args.stream:
        if stream not in inputs:
            print('parser_timing.py: the manifest has no stream %s' % stream)
            return 2
        means = [{} for _ in variants]
        frames = 0
        for round_number in range(args.rounds):
            # In turns, and the first of a round in turns too: no decoder always runs on the warmer
            # machine.
            order = list(range(len(variants)))
            if round_number % 2:
                order.reverse()
            for index in order:
                parser_name, parser_args = variants[index]
                command = [tool, '-name', stream, '-i', inputs[stream], '-gputime', '-t', str(args.threads),
                           '-timeout', '1200'] + parser_args
                try:
                    done = subprocess.run(command, cwd=args.streams, stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
                                          stdin=subprocess.DEVNULL, timeout=1800)
                except subprocess.TimeoutExpired:
                    print('ERROR %s %s: no end within 30 minutes' % (stream, parser_name))
                    failed = True
                    continue
                text = done.stdout.decode('utf-8', 'replace').replace('\r', '')
                log = '%s-%s-%d.log' % (stream, re.sub(r'[^A-Za-z0-9]+', '-', parser_name).strip('-'), round_number)
                with open(os.path.join(args.out, log), 'w', encoding='utf-8') as f:
                    f.write(text)
                timers = timers_of(text)
                if done.returncode != 0 or any(timer not in timers for timer in TIMERS + ['decode call']):
                    print('ERROR %s %s: exit code %d, timers %s' % (
                        stream, parser_name, done.returncode, ', '.join(sorted(timers)) or 'none'))
                    failed = True
                    continue
                frames = timers['frame'][0]
                for timer, (_, mean, _, _) in timers.items():
                    means[index].setdefault(timer, []).append(mean)
        lines += ['### %s (%d frames timed)' % (stream, frames), '',
                  '| Timer | ' + ' | '.join(name for name, _ in variants) + ' |',
                  '| --- |' + ' ---: |' * len(variants)]
        for timer in TIMERS + ['decode call'] + SINK_TIMERS:
            cells = []
            first = None
            for index in range(len(variants)):
                values = means[index].get(timer, [])
                if not values:
                    cells.append('none')
                    continue
                average = sum(values) / len(values)
                cell = '%.3f (%.3f to %.3f)' % (average, min(values), max(values))
                if index == 0:
                    first = average
                elif first and timer not in SINK_TIMERS:
                    cell += ', %.2f' % (average / first)
                cells.append(cell)
            lines.append('| %s | %s |' % (timer, ' | '.join(cells)))
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
