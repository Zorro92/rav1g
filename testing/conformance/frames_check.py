#!/usr/bin/env python3
"""Frames in parallel and the pools (docs/phase2-rav1d.md, steps 4 and 5): what a decoder is created
with, what its budget guard makes of it, a parse that waits for memory, what a panic in the parse does,
and what the parse of two frames at a time takes.

  python testing/conformance/frames_check.py --tool <rav1g-dec.exe> --streams <dir> --out <dir>
                                             --large <name>[:<temporal units>] --small <name>[:<temporal units>]
                                             [--dense <name>[:<temporal units>]] [--tool-args "<args>"] [--no-timing]

<dir> of --streams is what testing/conformance/prepare_streams.sh wrote. --large is a stream above
2560x1440 (where the library's defaults are two threads and two frames in parse), --small one at or below.
Every run decodes its stream with the reference check.

What is checked:

  - **What is created.** The CONFIGURATION line of the program (rav1g_decoder_configuration) against what
    the flags ask for: -parseframes 1 to 4 on as many threads, more frames than threads (as many frames as
    threads), four slots, and the library's defaults (no flag, and -t 0 -parseframes 0 -slots 0): two
    threads, two frames in parse and three slots for the large stream, one, one and two for the small one;
    one frame in parse where the slots are fixed at two. A decoder has one slot more than frames in parse.
  - **The budget guard.** With a budget (-budget) between what the decoder of one frame in parse needs and
    what that of two needs, a task that asks for two frames in parse gets a decoder with one frame in
    parse and two slots, says so in a "MEMORY frames in parse" line, and decodes: asking for frames in
    parse does not make a creation fail. With a budget that fits it gets two and three. The same for the
    library's default of the large stream.
  - **A parse that waits for memory** (-parseopt poolwait: behind its first piece of a pool a frame finds
    the pool empty while an older frame holds a piece of it). The large and the small stream, with one
    frame in parse and with two and four: the frames are those of the reference, and the "MEMORY pools"
    line of the decoder says that tiles waited for the GPU to finish an older frame. And --dense, a
    stream whose frames come close to the worst case of the decoder's size (the lossless stream of the
    set), without the switch: it decodes, and the line says how often a tile found the pool empty (not a
    check: whether the frame on the GPU is through by then is a matter of timing).
  - **A panic in the parse** (-parseopt testpanic: the sink panics in the third frame, on the thread that
    parses it): the task ends with an error that says so, on the thread of the decode call, on a worker
    thread with one frame in parse, and on a worker with two; it does not hang.
  - **Timing**, not a check: the large stream with one and with two frames in parse, in turns
    (-gputime). On WARP the stages run on the same processors, so the wall time of a frame is the
    rasteriser's; the time of the parse is what compares.

Written to --out: one log per run and summary.md (also appended to $GITHUB_STEP_SUMMARY when that is set).
Exit code 1 when a check fails, 2 when a run could not be done.
"""
import argparse
import os
import re
import subprocess
import sys

# The margin the budget guard keeps at a creation (AV1_MEMORY_GUARD_MARGIN), MiB.
MARGIN = 48
CONFIGURATION = re.compile(r'^CONFIGURATION .*?: threads (\d+) \(asked -?\d+\), frames in parse (\d+) \(asked -?\d+\), '
                           r'slots (\d+) \(asked -?\d+\); device buffers ([0-9.]+) MiB', re.M)
# (label, flags, (threads, frames in parse, slots) for the large stream, the same for the small one).
CREATED = (
    ('one frame in parse', '-parseframes 1 -t 2', (2, 1, 2), (2, 1, 2)),
    ('two', '-parseframes 2 -t 2', (2, 2, 3), (2, 2, 3)),
    ('two, four slots', '-parseframes 2 -t 4 -slots 4', (4, 2, 4), (4, 2, 4)),
    ('three', '-parseframes 3 -t 3', (3, 3, 4), (3, 3, 4)),
    ('four', '-parseframes 4 -t 8', (8, 4, 4), (8, 4, 4)),
    ('more frames than threads', '-parseframes 4 -t 2', (2, 2, 3), (2, 2, 3)),
    ('no flag', '', (2, 2, 3), (1, 1, 2)),
    ('the defaults of the library', '-parseframes 0 -t 0 -slots 0', (2, 2, 3), (1, 1, 2)),
    ('the default frames in parse with two slots', '-parseframes 0 -t 4 -slots 2', (4, 1, 2), (4, 1, 2)),
)
TIMERS = ('wall between decoded frames', 'cpu entropy/tile decode, wall', 'cpu entropy/tile decode, thread sum',
          'cpu decode thread, setup to submit', 'decode call (aom_codec_decode)')
TIMING = (('1 frame in parse, 2 threads', '-parseframes 1 -t 2'), ('2 frames in parse, 2 threads', '-parseframes 2 -t 2'),
          ('2 frames in parse, 3 threads', '-parseframes 2 -t 3'), ('3 frames in parse, 3 threads', '-parseframes 3 -t 3'))
ROUNDS = 2


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument('--tool', required=True)
    parser.add_argument('--streams', required=True)
    parser.add_argument('--out', required=True)
    parser.add_argument('--large', required=True)
    parser.add_argument('--small', required=True)
    parser.add_argument('--dense', default='', help='a stream whose frames come close to the worst case of its size')
    parser.add_argument('--tool-args', default='')
    parser.add_argument('--no-timing', action='store_true')
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
    streams = []
    for item in (args.large, args.small) + ((args.dense,) if args.dense else ()):
        name, limit = (item.split(':') + [''])[:2]
        if name not in manifest:
            print('ERROR %s is not a stream of the manifest' % name)
            return 2
        streams.append((name, ['-limit', limit] if limit else []))
    (large, large_limit), (small, small_limit) = streams[:2]

    problems = []
    runs = [0]

    def run(name, limit, flags, label, timeout=1200):
        """One task: (text, RESULT status, configuration or None); a run that does not end counts as hung."""
        runs[0] += 1
        command = [tool] + args.tool_args.split() + manifest[name] + flags.split() + limit
        log = os.path.join(out, 'run%02d.log' % runs[0])
        try:
            done = subprocess.run(command, cwd=args.streams, stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
                                  timeout=timeout)
            text = done.stdout.decode('utf-8', 'replace').replace('\r', '')
        except subprocess.TimeoutExpired as hung:
            text = (hung.stdout or b'').decode('utf-8', 'replace').replace('\r', '') + \
                '\nHUNG: the run did not end within %d s\n' % timeout
        with open(log, 'w', encoding='utf-8') as f:
            f.write('# %s: %s %s\n%s' % (label, name, flags, text))
        result = re.search(r'^RESULT\s.*?\bstatus=(\S+)', text, re.M)
        created = CONFIGURATION.search(text)
        return (text, result.group(1) if result else 'none',
                (int(created.group(1)), int(created.group(2)), int(created.group(3)), float(created.group(4)))
                if created else None)

    summary = ['## Frames in parallel: what is created, the budget guard, a panic', '',
               '| Stream | Asked for | Flags | Threads | Frames in parse | Slots | Device buffers, MiB | Frames |',
               '| --- | --- | --- | ---: | ---: | ---: | ---: | --- |']
    sizes = {}
    for name, limit, column in ((large, large_limit, 2), (small, small_limit, 3)):
        for row in CREATED:
            label, flags, expected = row[0], row[1], row[column]
            text, status, created = run(name, limit, flags, label)
            where = '%s, %s (%s)' % (name, label, flags)
            if status != 'pass':
                problems.append('%s: the stream did not decode as its reference says (status %s)' % (where, status))
            if not created:
                problems.append('%s: no CONFIGURATION line' % where)
                continue
            if created[:3] != expected:
                problems.append('%s: created with threads %d, frames in parse %d, slots %d; expected %d, %d, %d' % (
                    (where,) + created[:3] + expected))
            sizes[(name, label)] = created[3]
            summary.append('| %s | %s | `%s` | %d | %d | %d | %.1f | %s |' % (
                name, label, flags, created[0], created[1], created[2], created[3],
                'as the reference' if status == 'pass' else status))
            print('%s: threads %d, frames in parse %d, slots %d, %.1f MiB, %s' % ((where,) + created + (status,)))

    # The budget guard, on the large stream: a budget between the two decoders, and one that fits both.
    one = sizes.get((large, 'one frame in parse'))
    two = sizes.get((large, 'two'))
    summary += ['', '| Budget, MiB | Flags | Frames in parse | Slots | Device buffers, MiB | The guard says |',
                '| ---: | --- | ---: | ---: | ---: | --- |']
    if one and two and two > one:
        between = int((one + two) / 2) + MARGIN
        enough = int(two) + MARGIN + 16
        for budget, flags, expected, words in (
                (between, '-parseframes 2 -t 2', (2, 1, 2), 'is created with 1 frame in parse and 2 slots'),
                (between, '-parseframes 0 -t 0 -slots 0', (2, 1, 2), 'is created with 1 frame in parse and 2 slots'),
                (enough, '-parseframes 2 -t 2', (2, 2, 3), 'they fit'),
                (enough, '-parseframes 0 -t 0 -slots 0', (2, 2, 3), 'they fit')):
            flags = '%s -budget %d' % (flags, budget)
            text, status, created = run(large, large_limit, flags + ' -v', 'budget guard')
            where = '%s, %s' % (large, flags)
            line = re.search(r'MEMORY frames in parse: .*', text)
            if status != 'pass':
                problems.append('%s: the stream did not decode as its reference says (status %s): asking for frames '
                                'in parse must not make a task fail' % (where, status))
            if not created or created[:3] != expected:
                problems.append('%s: created with %s; threads %d, frames in parse %d, slots %d were expected' % (
                    (where, 'threads %d, frames in parse %d, slots %d' % created[:3] if created else 'no CONFIGURATION line')
                    + expected))
            if not line or words not in line.group(0):
                problems.append('%s: no "MEMORY frames in parse" line with "%s": %s' % (
                    where, words, line.group(0) if line else 'no such line'))
            summary.append('| %d | `%s` | %s | %s | %s | %s |' % (
                budget, flags, created[1] if created else '?', created[2] if created else '?',
                '%.1f' % created[3] if created else '?', (line.group(0) if line else 'nothing').replace('|', '/')))
            print('%s: %s' % (where, line.group(0) if line else 'no MEMORY frames in parse line'))
    else:
        problems.append('%s: the decoder of two frames in parse (%s MiB) is not larger than that of one (%s MiB)' % (
            large, two, one))

    # A parse that waits for memory: the test switch of the pools, and a stream that fills them.
    pools_line = re.compile(r'MEMORY pools: .* a tile waited for a chunk (\d+) times for the GPU to finish an older '
                            r'frame and (\d+) times for a frame in parse in front of its own')
    summary += ['', '| A parse that waits for memory | Flags | Frames | Waits for the GPU | Waits for a frame in front |',
                '| --- | --- | --- | ---: | ---: |']
    waits = [(name, limit, label, flags + ' -parseopt poolwait', True)
             for name, limit in ((large, large_limit), (small, small_limit))
             for label, flags in (('one frame in parse', '-parseframes 1 -t 2'), ('two', '-parseframes 2 -t 2'),
                                  ('four', '-parseframes 4 -t 8'))]
    if args.dense:
        dense, dense_limit = streams[2]
        waits += [(dense, dense_limit, 'a stream near the worst case, %s' % label, flags, False)
                  for label, flags in (('one frame in parse', '-parseframes 1 -t 1'), ('two', '-parseframes 2 -t 2'))]
    for name, limit, label, flags, must_wait in waits:
        text, status, _ = run(name, limit, flags + ' -v', 'pools', timeout=900)
        where = '%s, %s (%s)' % (name, label, flags)
        line = pools_line.search(text)
        if 'HUNG:' in text:
            problems.append('%s: the task did not end' % where)
        if status != 'pass':
            problems.append('%s: the stream did not decode as its reference says (status %s)' % (where, status))
        if not line:
            problems.append('%s: no "MEMORY pools" line' % where)
        elif must_wait and int(line.group(1)) == 0:
            problems.append('%s: no tile waited for the GPU to finish an older frame' % where)
        summary.append('| %s, %s | `%s` | %s | %s | %s |' % (
            name, label, flags, 'as the reference' if status == 'pass' else status,
            line.group(1) if line else '?', line.group(2) if line else '?'))
        print('%s: status %s, %s' % (where, status, line.group(0) if line else 'no MEMORY pools line'))

    # A panic in the parse: an error that says so, and an end.
    summary += ['', '| A panic in the parse of the third frame | Flags | The task |', '| --- | --- | --- |']
    for label, flags in (('on the thread of the decode call', '-parseopt testpanic -t 1'),
                         ('on a parser thread, one frame in parse', '-parseopt testpanic -t 4'),
                         ('on a parser thread, two frames in parse', '-parseopt testpanic -t 4 -parseframes 2'),
                         ('on a parser thread, three frames in parse', '-parseopt testpanic -t 8 -parseframes 3')):
        text, status, _ = run(small, small_limit, flags, 'panic', timeout=300)
        where = '%s, a panic %s (%s)' % (small, label, flags)
        said = 'stopped with a panic' in text
        if 'HUNG:' in text:
            problems.append('%s: the task did not end' % where)
        if not said:
            problems.append('%s: the log does not say that the parser stopped with a panic' % where)
        if status == 'pass':
            problems.append('%s: the task passed; the stream has more than two frames, so the panic was not reached' % where)
        summary.append('| %s | `%s` | %s |' % (label, flags, 'hung' if 'HUNG:' in text else (
            'ended with the error of the panic' if said and status != 'pass' else 'status %s' % status)))
        print('%s: status %s, %s' % (where, status, 'the panic is reported' if said else 'no report of the panic'))

    if not args.no_timing:
        summary += ['', 'Timing of %s, ms a frame (the mean of %d runs in turns; not a check). On WARP the stages run '
                    'on the processors the parse runs on.' % (large, ROUNDS), '',
                    '| | ' + ' | '.join(TIMERS) + ' |', '| --- |' + ' ---: |' * len(TIMERS)]
        totals = {label: [[] for _ in TIMERS] for label, _ in TIMING}
        for _ in range(ROUNDS):
            for label, flags in TIMING:
                text, status, _ = run(large, large_limit, flags + ' -gputime', 'timing')
                if status != 'pass':
                    problems.append('%s, timing run with %s: status %s' % (large, flags, status))
                for at, timer in enumerate(TIMERS):
                    line = re.search(r'^GPUTIME %s\s+\d+\s+([0-9.]+)' % re.escape(timer), text, re.M)
                    if line:
                        totals[label][at].append(float(line.group(1)))
        for label, _ in TIMING:
            means = ['%.2f' % (sum(values) / len(values)) if values else '?' for values in totals[label]]
            summary.append('| %s | %s |' % (label, ' | '.join(means)))
            print('timing, %s: %s' % (label, ', '.join('%s %s' % pair for pair in zip(TIMERS, means))))

    summary += ['', 'Checks: ' + ('all passed.' if not problems else '%d failed.' % len(problems)), '']
    summary += ['- ' + problem for problem in problems]
    text = '\n'.join(summary) + '\n'
    with open(os.path.join(out, 'summary.md'), 'w', encoding='utf-8') as f:
        f.write(text)
    step_summary = os.environ.get('GITHUB_STEP_SUMMARY')
    if step_summary:
        with open(step_summary, 'a', encoding='utf-8') as f:
            f.write(text)
    for problem in problems:
        print('FAIL %s' % problem)
    print('frames check: %s' % ('all checks passed' if not problems else '%d checks failed' % len(problems)))
    return 1 if problems else 0


if __name__ == '__main__':
    sys.exit(main())
