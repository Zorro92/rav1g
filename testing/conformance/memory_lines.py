#!/usr/bin/env python3
"""The "MEMORY" log lines of a decoder program for a few streams and configurations, and their comparison.

  python memory_lines.py run --tool <exe> --streams <dir> --out <dir>
  python memory_lines.py compare --reference <dir> --candidate <dir>

run: decodes the first temporal units of a few streams of the conformance set with -v, in several decoder
configurations (slots, low memory parts, the buffers of -intrav2 and -interfast), and writes the lines of
the decoder that start with "MEMORY" to <out>/<case>.txt: the budget line, what the created decoder holds
(by type, per slot, the largest buffers, the host pool), and the steps of its release. Left out or made
equal is what differs from run to run on one machine: the "MEMORY dxgi" lines (the video memory the adapter
reports), the usage number of the "MEMORY destroy" lines, and the size of the host pool (it counts from the
first address of the application's memory that is a multiple of 256; what is in use of it is compared).

compare: the files of two runs must be the same, line for line. That is how the Rust decoder (rav1g-dec) is
held against the C++ decoder (warp_decoder of branch cpp, at the commit the workflow pins): same buffers,
same sizes, same decisions, same text.

Exit code: 0 equal (or run done), 1 different, 2 the run could not be done.
"""
import argparse
import os
import re
import subprocess
import sys

# (case, stream of the manifest, temporal units, flags)
CASES = [
    ('2160p-8bit', 'nf-dvbdash-a1-8b-2160p.ivf', 2, []),
    ('1080p-10bit', 'nf-chimera-10b-1080p.ivf', 2, []),
    ('1080p-10bit-slots4', 'nf-chimera-10b-1080p.ivf', 2, ['-slots', '4']),
    ('1080p-10bit-slots3-lowmem', 'nf-chimera-10b-1080p.ivf', 2, ['-slots', '3', '-lowmem']),
    ('1080p-10bit-nolowmem', 'nf-chimera-10b-1080p.ivf', 2, ['-lowmemparts', '0']),
    ('1080p-10bit-intrav2-interfast', 'nf-chimera-10b-1080p.ivf', 2, ['-intrav2', '-interfast']),
    ('720p-8bit-threads4', 'nf-chimera-8b-720p.ivf', 2, ['-t', '4']),
    ('16x16', 'av1-1-b8-01-size-16x16.ivf', 0, []),
    ('svc-two-layers', 'av1-1-b8-22-svc-L2T1.ivf', 0, []),
    ('film-grain', 'av1-1-b8-23-film_grain-50.ivf', 0, []),
]

LINE = re.compile(r'^(?:decoder: |DECODER ERROR: )(MEMORY .*)$')


def memory_lines(text):
    lines = []
    for line in text.replace('\r', '').split('\n'):
        match = LINE.match(line)
        if not match:
            continue
        line = match.group(1)
        if line.startswith('MEMORY dxgi '):
            continue
        if line.startswith('MEMORY destroy '):
            line = re.sub(r'dxgi local usage [0-9.]+ MiB', 'dxgi local usage <n> MiB', line)
        # The host pool counts from the first address in the application's memory that is a multiple of
        # 256, so its size is that of the configuration less what the program's allocator put in front.
        line = re.sub(r'host pool (\d+) of \d+ bytes', r'host pool \1 of <n> bytes', line)
        lines.append(line)
    return lines


def run(args):
    manifest = {}
    with open(os.path.join(args.streams, 'manifest.txt'), encoding='utf-8') as f:
        for line in f:
            name = re.search(r'(?:^|\s)-name\s+(\S+)', line)
            if name:
                manifest[name.group(1)] = line.split()
    os.makedirs(args.out, exist_ok=True)
    tool = os.path.abspath(args.tool)
    failed = False
    for case, stream, limit, flags in CASES:
        if stream not in manifest:
            print('ERROR %s: the manifest has no stream %s' % (case, stream))
            failed = True
            continue
        command = [tool] + manifest[stream] + ['-v'] + flags
        if limit:
            command += ['-limit', str(limit)]
        done = subprocess.run(command, cwd=args.streams, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
        text = done.stdout.decode('utf-8', 'replace')
        lines = memory_lines(text)
        # With -limit a stream ends before its last frame: the md5 comparison of the task is not what this
        # run is about, only that the decoder was created, decoded and was released.
        if done.returncode not in (0, 1) or not any(l.startswith('MEMORY decoder created:') for l in lines) \
                or not any(l.startswith('MEMORY destroy end') for l in lines):
            print('ERROR %s: exit code %d, %d MEMORY lines' % (case, done.returncode, len(lines)))
            print('\n'.join(text.replace('\r', '').split('\n')[-8:]))
            failed = True
            continue
        with open(os.path.join(args.out, case + '.txt'), 'w', encoding='utf-8', newline='\n') as f:
            f.write('\n'.join(lines) + '\n')
        result = re.search(r'^RESULT .*?status=(\w+)', text.replace('\r', ''), re.M)
        print('%s: %d lines; the task: %s' % (case, len(lines), result.group(1) if result else 'no RESULT line'))
    return 2 if failed else 0


def compare(args):
    different = 0
    for case, _, _, _ in CASES:
        texts = []
        for directory in (args.reference, args.candidate):
            path = os.path.join(directory, case + '.txt')
            if not os.path.isfile(path):
                print('ERROR %s: no %s' % (case, path))
                return 2
            with open(path, encoding='utf-8') as f:
                texts.append(f.read().split('\n'))
        if texts[0] == texts[1]:
            print('%s: equal, %d lines' % (case, len(texts[0]) - 1))
            continue
        different += 1
        print('%s: DIFFERENT' % case)
        for i in range(max(len(texts[0]), len(texts[1]))):
            a = texts[0][i] if i < len(texts[0]) else '<no line>'
            b = texts[1][i] if i < len(texts[1]) else '<no line>'
            if a != b:
                print('  line %d\n    %s: %s\n    %s: %s' % (i + 1, args.reference_name, a, args.candidate_name, b))
    if different:
        print('%d of %d cases differ' % (different, len(CASES)))
        return 1
    print('The MEMORY lines of %s and %s are the same in all %d cases' % (args.reference_name, args.candidate_name,
                                                                      len(CASES)))
    return 0


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    commands = parser.add_subparsers(dest='command', required=True)
    r = commands.add_parser('run')
    r.add_argument('--tool', required=True)
    r.add_argument('--streams', required=True)
    r.add_argument('--out', required=True)
    c = commands.add_parser('compare')
    c.add_argument('--reference', required=True)
    c.add_argument('--candidate', required=True)
    c.add_argument('--reference-name', default='warp_decoder')
    c.add_argument('--candidate-name', default='rav1g-dec')
    args = parser.parse_args()
    return run(args) if args.command == 'run' else compare(args)


if __name__ == '__main__':
    sys.exit(main())
