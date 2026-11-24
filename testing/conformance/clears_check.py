#!/usr/bin/env python3
"""Who clears the grids of a pipeline slot, and how much: the default of docs/test-player.md, "Pipeline
slots and clears", held to what it says. (The other ways, -gpuclear, -cpuclear, -syncclear and -parseopt
cleargrid, are retired with interface version 3.)

  python testing/conformance/clears_check.py --tool <rav1g-dec.exe> --streams <dir> --out <dir>
                                             --stream <name>[:<temporal units>] [--stream ...]
                                             [--threads N] [--tool-args "<args>"]

<dir> of --streams is what testing/conformance/prepare_streams.sh wrote. Each stream is decoded with its
reference check and with -gputime, the wavefront of intra prediction at the edges of the tiles.

What is checked, from the lines of the timing run for every frame (the bytes cleared for the frame by the
decode and tile threads, by the completion thread behind it, and by its command list). The first frames of
a run are left out where a slot's first frame counts: it clears on the decode thread, whatever the way.

  - every run decodes the stream as its reference says, and its task line names the way;
  - the completion thread and the GPU clear nothing, in no frame, and the decode and tile threads less
    than 64 KiB a tile (the counters of the tile's block lists).

Written to --out: <stream>.<way>.log and summary.md (also appended to $GITHUB_STEP_SUMMARY when that is set).
Exit code 1 when a check fails, 2 when a run could not be done.
"""
import argparse
import os
import re
import subprocess
import sys

WAYS = (('default', '', 'none needed'),)
# What the decode and tile threads may clear of a frame without a flag, per tile: the tile's descriptor and
# the counters of its block lists.
TILE_BYTES = 64 * 1024
# A frame of the timing run (not the copy of a show_existing_frame): its tiles, whether its command list has
# the stage slot_clear, and the bytes cleared for it.
FRAME = re.compile(r'^GPUTIME frame \d+ no=-?\d+ show=\d+ sef=0 .*? tiles=(\d+) .*?\|(.*?)\|.*\| cleared memset=(\d+) '
                   r'completion=(\d+) gpu=(\d+)$', re.M)
# The frames at the start of a run that are left out of the checks of the decode thread: the first frame
# of each slot is among them.
FIRST = 4


def frames_of(text):
    """Per decoded frame: (decode and tile threads, completion thread, GPU, stage slot_clear, tiles)."""
    return [(int(memset), int(completion), int(gpu), ' slot_clear=' in stages, int(tiles))
            for tiles, stages, memset, completion, gpu in FRAME.findall(text)]


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument('--tool', required=True)
    parser.add_argument('--streams', required=True)
    parser.add_argument('--out', required=True)
    parser.add_argument('--stream', action='append', required=True)
    parser.add_argument('--threads', type=int, default=4)
    parser.add_argument('--tool-args', default='')
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
    summary = ['## The clears of a pipeline slot', '',
               'Bytes cleared per frame, the mean over the measured frames of the timing run.', '',
               '| Stream | Way | CPU, decode and tile threads | CPU, completion thread | GPU | Frames with the stage slot_clear |',
               '| --- | --- | ---: | ---: | ---: | ---: |']
    for item in args.stream:
        name, limit = (item.split(':') + [''])[:2]
        if name not in manifest:
            print('ERROR %s is not a stream of the manifest' % name)
            return 2
        got = {}
        for way, flags, words in WAYS:
            def bad(what, way=way):
                problems.append('%s, %s: %s' % (name, way, what))

            command = [tool] + args.tool_args.split() + manifest[name] + ['-t', str(args.threads)] + flags.split() + \
                ['-gputime', '-progress']
            if limit:
                command += ['-limit', limit]
            run = subprocess.run(command, cwd=args.streams, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
            text = run.stdout.decode('utf-8', 'replace').replace('\r', '')
            with open(os.path.join(out, '%s.%s.log' % (name, way)), 'w', encoding='utf-8') as f:
                f.write(text)
            if run.returncode != 0:
                bad('exit code %d' % run.returncode)
            result = re.search(r'^RESULT\s.*?\bstatus=(\S+)', text, re.M)
            if not result or result.group(1) != 'pass':
                bad('the stream did not decode as its reference says: %s' % (result.group(0) if result else 'no RESULT line'))
            task = re.search(r'^TASK .*$', text, re.M)
            if not task or ', clears %s, ' % words not in task.group(0):
                bad('the task line does not say "clears %s": %s' % (words, task.group(0) if task else 'no TASK line'))
            frames = frames_of(text)
            if len(frames) < 2 * FIRST:
                bad('%d frames in the timing run: too few to tell the first frames of the slots from the rest' % len(frames))
                continue
            got[way] = frames
            mean = [sum(frame[at] for frame in frames) / len(frames) for at in range(3)]
            staged = sum(1 for frame in frames if frame[3])
            print('%s %s: %d frames; decode and tile threads %.0f, completion thread %.0f, GPU %.0f bytes a frame; '
                  'slot_clear in %d frames' % (name, flags or '(no flag)', len(frames), mean[0], mean[1], mean[2], staged))
            summary.append('| %s | %s | %.0f | %.0f | %.0f | %d of %d |' % (
                name, '`%s`' % flags if flags else 'no flag', mean[0], mean[1], mean[2], staged, len(frames)))
        if len(got) != len(WAYS):
            continue

        def check(way, what, holds):
            """`holds`: for every frame of the way, with its index, whether the frame is as it should be."""
            failed = [index for index, frame in enumerate(got[way]) if not holds(index, frame)]
            if failed:
                frame = got[way][failed[0]]
                problems.append('%s, %s: %s, in %d of %d frames; frame %d: decode and tile threads %d, completion thread '
                                '%d, GPU %d bytes, slot_clear %s' % (name, way, what, len(failed), len(got[way]), failed[0],
                                                                     frame[0], frame[1], frame[2], frame[3]))

        idle = lambda index, frame: frame[1:4] == (0, 0, False)
        check('default', 'the completion thread or the GPU clears something', idle)
        check('default', 'the decode and tile threads clear more than %d bytes a tile' % TILE_BYTES,
              lambda index, frame: index < FIRST or frame[0] <= TILE_BYTES * frame[4])

    summary += ['', 'Checks: ' + ('all passed.' if not problems else '%d failed.' % len(problems)), '']
    summary += ['- ' + problem for problem in problems]
    text = '\n'.join(summary) + '\n'
    with open(os.path.join(out, 'summary.md'), 'w', encoding='utf-8') as f:
        f.write(text)
    if os.environ.get('GITHUB_STEP_SUMMARY'):
        with open(os.environ['GITHUB_STEP_SUMMARY'], 'a', encoding='utf-8') as f:
            f.write(text)
    for problem in problems:
        print('FAILED ' + problem)
    print('clears_check.py: %d streams, %d problems' % (len(args.stream), len(problems)))
    return 1 if problems else 0


if __name__ == '__main__':
    sys.exit(main())
