#!/usr/bin/env python3
"""The output pass in the timing run, by the kind of the output images (docs/output.md).

  python testing/conformance/output_cost.py --tool <rav1g-dec.exe> --streams <dir> --out <dir>
                                            --stream <name> [--stream ...] [--threads N]

Each stream is decoded with -gputime and its reference check once for every kind: the planes in a buffer
(the default), the semi-planar output in a texture that shaders may write (-output p010), in one they may
not (p010-copy), and in a buffer (p010-buffer). The stages of the output pass are film_grain, copy_plane
(the copy shader for the planes film grain did not write) and output_copy (the copies into the image; on
the direct path of -output p010 there is none, and the stage is the time of two state changes). The table
has the median of each stage over the frames that ran it, and which path the decoder took for the texture.

On WARP the times are those of a CPU rasterizer: they say how the kinds compare there, not what a GPU
does. The check fails when a run does not decode its stream as the reference says, and nothing else.
"""

import argparse
import os
import re
import subprocess
import sys

KINDS = (('planes', ''), ('p010', '-output p010'), ('p010-copy', '-output p010-copy'),
         ('p010-buffer', '-output p010-buffer'))
STAGES = ('film_grain', 'copy_plane', 'output_copy')


def main():
    parser = argparse.ArgumentParser(description=__doc__.split('\n', 1)[0])
    parser.add_argument('--tool', required=True)
    parser.add_argument('--streams', required=True)
    parser.add_argument('--out', required=True)
    parser.add_argument('--stream', action='append', required=True, help='e.g. svt/syn-tiles-10b-1080p.ivf')
    parser.add_argument('--threads', type=int, default=4)
    parser.add_argument('--tool-args', default='')
    args = parser.parse_args()
    os.makedirs(args.out, exist_ok=True)
    problems = []
    rows = []
    for stream in args.stream:
        for kind, flags in KINDS:
            command = ([os.path.abspath(args.tool)] + args.tool_args.split() + ['-v', '-gputime'] + flags.split() +
                       ['-t', str(args.threads), '-i', stream, '-check', stream + '.md5', '-aom_md5'])
            try:
                run = subprocess.run(command, cwd=args.streams, stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
                                     timeout=1800)
                text, code = run.stdout.decode('utf-8', 'replace').replace('\r', ''), run.returncode
            except subprocess.TimeoutExpired:
                text, code = '', -1
            name = '%s-%s.log' % (re.sub(r'[^A-Za-z0-9.-]', '_', stream), kind)
            with open(os.path.join(args.out, name), 'w', encoding='utf-8') as f:
                f.write(text)
            if code != 0 or 'status=pass' not in text:
                problems.append('%s, %s: exit code %d, not decoded as the reference says' % (stream, kind, code))
            stages = {}
            for line in text.split('\n'):
                match = re.match(r'GPUTIME gpu (\S+) +(\d+) +([\d.]+) +([\d.]+) ', line)
                if match and match.group(1) in STAGES:
                    stages[match.group(1)] = '%s (%s frames)' % (match.group(4), match.group(2))
            busy = re.search(r'GPUTIME gpu busy \(first to last timestamp\) +\d+ +[\d.]+ +([\d.]+)', text)
            path = sorted(set(re.findall(r'OUTPUT (?:NV12|P010) (direct|staged)', text)))
            rows.append((stream, kind, [stages.get(stage, '-') for stage in STAGES], busy.group(1) if busy else '-',
                         '/'.join(path) if path else '-'))
    lines = ['| Stream | Images | film_grain | copy_plane | output_copy | gpu busy | Path |',
             '| --- | --- | ---: | ---: | ---: | ---: | --- |']
    for stream, kind, stages, busy, path in rows:
        lines.append('| %s | %s | %s | %s | %s | %s | %s |' % (stream, kind, stages[0], stages[1], stages[2], busy, path))
    print('\n'.join(lines))
    for problem in problems:
        print('FAILED %s' % problem)
    if os.environ.get('GITHUB_STEP_SUMMARY'):
        with open(os.environ['GITHUB_STEP_SUMMARY'], 'a', encoding='utf-8') as f:
            f.write('## The output pass by the kind of the output images (-gputime, median ms of a frame, WARP)\n\n')
            f.write('\n'.join(lines) + '\n\n')
            f.write(''.join('- FAILED %s\n' % problem for problem in problems))
    return 1 if problems else 0


if __name__ == '__main__':
    sys.exit(main())
