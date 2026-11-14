#!/usr/bin/env python3
"""The "MEMORY" log lines of a decoder program for a few streams and configurations, and their comparison.

  python memory_lines.py run --tool <exe> --streams <dir> --out <dir>
  python memory_lines.py compare --reference <dir> --candidate <dir>
  python memory_lines.py pools --shares <dir> --pools <dir>

run: decodes the first temporal units of a few streams of the conformance set with -v, in several decoder
configurations (slots, low memory parts, the buffers of -intrav2 and -interfast), and writes the lines of
the decoder that start with "MEMORY" to <out>/<case>.txt: the budget line, what the created decoder holds
(by type, per slot, the largest buffers), and the steps of its release. Left out or made equal is what
differs from run to run on one machine: the "MEMORY dxgi" lines (the video memory the adapter reports) and
the usage number of the "MEMORY destroy" lines. And the host pool: its two numbers in the "decoder
created" line are made equal and the "host memory" line of the Rust decoder is left out.

Why the host pool is not compared: the C++ decoder lies in its host pool (its struct of 67,384 bytes and
its allocator of 7,296), the Rust decoder is an allocation of its own next to a pool that holds only what
the entropy decoder is given (docs/rust-port.md, "The host pool"). So the pool of a configuration is
smaller in the Rust decoder by design, and its total also counts from the first address of the pool's
memory that is a multiple of 256, which differs from program to program. Everything the two decoders have
in common is still compared: the device buffers (number, bytes, by type, per slot, the largest), the frame
buffers, the options, the budget decision and the steps of the release.

compare: the files of two runs must be the same, line for line. That is how the Rust decoder (rav1g-dec) is
held against the C++ decoder (warp_decoder of branch cpp, at the commit the workflow pins): same buffers,
same sizes, same decisions, same text.

pools: the files of a run of the decoder (pools: docs/phase2-rav1d.md, step 5) against those of a run of
the C++ decoder, which has a share of the mode info and coefficient buffers for each slot, case by case.
The two are not the same lines; what must hold:
  - the decoder size, the bit depth and the frame buffer size are the same; with the same slots, the
    frame buffers are the same;
  - the decoder with pools says what its pools are, and each has a chunk for every 64x64 area of the
    decoder size (the mode info pool half of them and the coefficient pool four fifths with the low
    memory parts 2 and 4), the coefficient pool a thirty-second more;
  - with the same slots it takes no more device memory than the decoder with shares.
A case where the slots differ (the library gives a decoder for frames above 2560x1440 two frames in
parse and three slots) is reported and its sizes are not compared.

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
HOST_MEMORY_LINE = re.compile(r'^MEMORY [^:]* host memory: ')


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
        # The host pool is not compared (see above): the line of the Rust decoder about its host memory
        # is left out, and the pool's numbers in the line both decoders have are made equal.
        if HOST_MEMORY_LINE.match(line):
            continue
        # What the pools were used for differs from run to run (which frames were in flight together).
        if line.startswith('MEMORY pools: '):
            line = re.sub(r'\d+', '<n>', line)
        line = re.sub(r'host pool \d+ of \d+ bytes \([0-9.]+ MiB\) in use',
                      'host pool <n> of <n> bytes (<n> MiB) in use', line)
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
        command = [tool] + manifest[stream] + ['-v'] + flags + args.tool_args.split()
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


CREATED = re.compile(r'^MEMORY decoder created: (\d+)x(\d+) (\d+)-bit, slots (\d+), frame buffers (\d+) \+ (\d+) back '
                     r'buffers? of (\d+) bytes .*?; (\d+) device buffers, (\d+) bytes')
POOLS = re.compile(r'^MEMORY decoder created pools: mode info (\d+) chunks of 256 records, [0-9.]+ MiB; coefficients '
                   r'(\d+) chunks of 7680, [0-9.]+ MiB; transform block records (\d+) chunks of 384, ')
LOW_MEMORY = re.compile(r'^MEMORY decoder created options: .*; low memory (\d+) ')


def pools(args):
    """The decoder with pools against the decoder with a share for each slot."""
    failed = 0
    print('| Case | Decoder | Slots, shares / pools | Device buffers with shares, MiB | With pools, MiB | Pools: chunks |')
    print('| --- | --- | --- | ---: | ---: | --- |')
    for case, _, _, _ in CASES:
        found = []
        for directory in (args.shares, args.pools):
            path = os.path.join(directory, case + '.txt')
            if not os.path.isfile(path):
                print('ERROR %s: no %s' % (case, path))
                return 2
            with open(path, encoding='utf-8') as f:
                lines = f.read().split('\n')
            created = next((CREATED.match(line) for line in lines if CREATED.match(line)), None)
            found.append((created, lines))
        problems = []
        (shares, _), (pooled, pooled_lines) = found
        if not shares or not pooled:
            print('ERROR %s: no "MEMORY decoder created" line' % case)
            failed += 1
            continue
        a, b = [list(map(int, created.groups())) for created in (shares, pooled)]
        if a[:3] != b[:3] or a[6] != b[6]:
            problems.append('the decoder size, the bit depth or the frame buffer size differ')
        same_slots = a[3] == b[3]
        if same_slots and a[4:6] != b[4:6]:
            problems.append('the frame buffers differ: %d + %d and %d + %d' % (a[4], a[5], b[4], b[5]))
        line = next((POOLS.match(line) for line in pooled_lines if POOLS.match(line)), None)
        low = next((LOW_MEMORY.match(line) for line in pooled_lines if LOW_MEMORY.match(line)), None)
        chunks = '?'
        if not line or not low:
            problems.append('the decoder with pools has no "MEMORY decoder created pools" or options line')
        else:
            cells = (b[0] // 64) * (b[1] // 64)
            parts = int(low.group(1))
            expected = [(cells + 1) // 2 if parts & 2 else cells,
                        ((cells * 4 + 4) // 5 if parts & 4 else cells) + max(1, cells // 32), cells]
            chunks = '%s, %s, %s' % line.groups()
            if [int(n) for n in line.groups()] != expected:
                problems.append('the pools have %s chunks; %d cells and the low memory parts %d give %s' % (
                    chunks, cells, parts, ', '.join(map(str, expected))))
        if same_slots and b[8] > a[8]:
            problems.append('the decoder with pools takes %d bytes of device memory, %d more than the decoder with '
                            'shares' % (b[8], b[8] - a[8]))
        print('| %s | %dx%d %d-bit | %d / %d | %.1f | %.1f%s | %s |' % (
            case, b[0], b[1], b[2], a[3], b[3], a[8] / 1048576.0, b[8] / 1048576.0,
            '' if same_slots else ' (other slots: not compared)', chunks))
        for problem in problems:
            print('FAIL %s: %s' % (case, problem))
        failed += bool(problems)
    if failed:
        print('%d of %d cases are not as the pools should be' % (failed, len(CASES)))
        return 1
    print('The decoder with pools is as it should be next to the decoder with shares in all %d cases' % len(CASES))
    return 0


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    commands = parser.add_subparsers(dest='command', required=True)
    r = commands.add_parser('run')
    r.add_argument('--tool', required=True)
    r.add_argument('--streams', required=True)
    r.add_argument('--out', required=True)
    r.add_argument('--tool-args', default='', help='further arguments for the tool, e.g. "-parseopt none"')
    c = commands.add_parser('compare')
    c.add_argument('--reference', required=True)
    c.add_argument('--candidate', required=True)
    c.add_argument('--reference-name', default='warp_decoder')
    c.add_argument('--candidate-name', default='rav1g-dec')
    p = commands.add_parser('pools')
    p.add_argument('--shares', required=True, help='the lines of the decoder with a share for each slot (the C++ decoder)')
    p.add_argument('--pools', required=True, help='the lines of the decoder with pools')
    args = parser.parse_args()
    return {'run': run, 'compare': compare, 'pools': pools}[args.command](args)


if __name__ == '__main__':
    sys.exit(main())
