#!/usr/bin/env python3
# SPDX-License-Identifier: BSD-2-Clause
"""The display pass prototype (-displaypass, docs/stage-fusion.md) against its float32 model.

  python testing/display_pass/check.py --tool rav1g-dec.exe --streams <conformance-streams> \\
      [--stream svt/syn-hdr10-10b-1080p.ivf ...] [--frames 4] [--out dir]

For every stream: rav1g-dec decodes the first frames twice, once as they are (-o, planar) and once with
-output p010 -displaypass at half the size (-o: the display images), and model.py computes the display
images from the first run's frames with the sequence's colour description (the COLOUR line of the log)
and counts the codes that differ. Exit code 1 when a code differs by more than one (fp32 on two
compilers), or when a run fails.
"""

import argparse
import os
import re
import subprocess
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
COLOUR = re.compile(r'COLOUR \S+ sequence: primaries (\d+) .*?transfer (\d+) .*?matrix (\d+) .*?range (\w+) chroma pos (\d+)')


def run(cmd, log):
    with open(log, 'w', encoding='utf-8') as f:
        code = subprocess.run(cmd, stdout=f, stderr=subprocess.STDOUT, check=False).returncode
    with open(log, encoding='utf-8', errors='replace') as f:
        return code, f.read()


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument('--tool', required=True)
    ap.add_argument('--streams', required=True)
    ap.add_argument('--stream', action='append')
    ap.add_argument('--frames', type=int, default=4)
    ap.add_argument('--out', default='out/display-pass')
    a = ap.parse_args()
    streams = a.stream or ['svt/syn-hdr10-10b-1080p.ivf', 'svt/syn-tiles-10b-1080p.ivf']
    os.makedirs(a.out, exist_ok=True)
    failed = False
    for stream in streams:
        name = os.path.basename(stream)
        path = os.path.join(a.streams, stream)
        frames = os.path.join(a.out, name + '.frames.yuv')
        display = os.path.join(a.out, name + '.display.yuv')
        code, log = run([a.tool, '-i', path, '-limit', str(a.frames), '-o', frames], os.path.join(a.out, name + '.frames.log'))
        size = re.search(r'RESULT .*? size=(\d+)x(\d+) bitdepth=(\d+)', log)
        colour = COLOUR.search(log)
        if code != 0 or not size or not colour:
            print(f'{name}: the plain decode failed (exit {code})')
            failed = True
            continue
        w, h, depth = map(int, size.groups())
        if depth != 10:
            print(f'{name}: {depth}-bit: the prototype takes 10-bit frames only; skipped')
            continue
        out = f'{w // 2 & ~1}x{h // 2 & ~1}'
        code, log2 = run([a.tool, '-i', path, '-limit', str(a.frames), '-output', 'p010', '-displaypass', out, '-o', display],
                         os.path.join(a.out, name + '.display.log'))
        if code != 0 or 'DISPLAY ' in log2:
            print(f'{name}: the display pass run failed (exit {code})')
            for line in log2.splitlines():
                if 'DISPLAY' in line or 'RESULT' in line or 'OUTPUT' in line:
                    print('  ' + line)
            failed = True
            continue
        primaries, transfer, matrix, rng, csp = colour.groups()
        args = [sys.executable, os.path.join(HERE, 'model.py'), frames, display, '--size', f'{w}x{h}', '--out', out,
                '--transfer', transfer, '--matrix', matrix, '--count', str(a.frames)]
        if rng == 'full':
            args.append('--full')
        if csp == '2':
            args.append('--colocated')
        print(f'{name}: {w}x{h} to {out}, primaries {primaries} transfer {transfer} matrix {matrix} range {rng} '
              f'chroma pos {csp}', flush=True)
        if subprocess.run(args, check=False).returncode != 0:
            failed = True
    sys.exit(1 if failed else 0)


if __name__ == '__main__':
    main()
