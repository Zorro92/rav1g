#!/usr/bin/env python3
# SPDX-License-Identifier: BSD-2-Clause
"""The display pass (Rav1gSettings::display, -displaypass, docs/stage-fusion.md) against its float32 model.

  python testing/display_pass/check.py --tool rav1g-dec.exe --streams <conformance-streams> \\
      [--stream svt/syn-hdr10-10b-1080p.ivf ...] [--frames 4] [--out dir]

For every stream: rav1g-dec decodes the first frames once as they are (-o, planar) and then with
-output p010 -displaypass (P010 for a 10-bit stream, NV12 for an 8-bit one) at half the size (the
shader's 2:1 quad path) and at two thirds of it (its area path) (-o: the display images), through the
Rust interface; and at half the size through the interface for C (-capi) with other settings of the
pass: the PQ stream with a 600-nit peak and a 203-nit white and with the transform forced to scale only,
the SDR stream into NV12. model.py computes the display images from the first run's frames with the
sequence's colour description (the COLOUR line of the log) and the same settings, and counts the codes
that differ. Exit code 1 when a code differs by more than one (fp32 on two compilers, the tone map's
tables), or when a run fails.
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
    # PQ and SDR at 10 bits; film grain at 10 and 8 bits (grain is applied before the downscale, so the
    # plain run's frames, which have it, are the model's input).
    streams = a.stream or ['svt/syn-hdr10-10b-1080p.ivf', 'svt/syn-tiles-10b-1080p.ivf', 'aom/av1-1-b10-23-film_grain-50.ivf',
                           'aom/av1-1-b8-23-film_grain-50.ivf']
    # The settings of the -capi runs at half the size, by stream: (name, rav1g-dec flags, model flags).
    extra = {
        'syn-hdr10-10b-1080p.ivf': [
            ('peak600-white203', ['-displaypeak', '600', '-displaywhite', '203'], ['--peak', '600', '--white', '203']),
            ('scale', ['-displaytransform', '1'], ['--transfer', '1', '--primaries', '1']),
        ],
        'syn-tiles-10b-1080p.ivf': [('nv12', ['-displaydepth', '8'], ['--out-depth', '8'])],
    }
    os.makedirs(a.out, exist_ok=True)
    failed = False
    for stream in streams:
        name = os.path.basename(stream)
        path = os.path.join(a.streams, stream)
        frames = os.path.join(a.out, name + '.frames.yuv')
        code, log = run([a.tool, '-i', path, '-limit', str(a.frames), '-o', frames], os.path.join(a.out, name + '.frames.log'))
        size = re.search(r'RESULT .*? size=(\d+)x(\d+) bitdepth=(\d+)', log)
        colour = COLOUR.search(log)
        if code != 0 or not size or not colour:
            print(f'{name}: the plain decode failed (exit {code})')
            failed = True
            continue
        w, h, depth = map(int, size.groups())
        primaries, transfer, matrix, rng, csp = colour.groups()
        half, two_thirds = f'{w // 2 & ~1}x{h // 2 & ~1}', f'{w * 2 // 3 & ~1}x{h * 2 // 3 & ~1}'
        runs = [(half, 'rust', [], []), (two_thirds, 'rust', [], [])]
        runs += [(half, label, ['-capi'] + flags, model_flags) for label, flags, model_flags in extra.get(name, [])]
        for out, label, flags, model_flags in runs:
            display = os.path.join(a.out, f'{name}.{out}.{label}.display.yuv')
            # The decoder's own "DISPLAY" lines say what it was created with; one of the image or of a
            # refused creation is a failure.
            code, log2 = run([a.tool, '-i', path, '-limit', str(a.frames), '-output', 'p010', '-displaypass', out] + flags +
                             ['-o', display], os.path.join(a.out, f'{name}.{out}.{label}.display.log'))
            refused = [line for line in log2.splitlines() if 'DISPLAY the' in line]
            if code != 0 or refused:
                print(f'{name}: the display pass run to {out} ({label}) failed (exit {code})')
                for line in log2.splitlines():
                    if 'DISPLAY' in line or 'RESULT' in line or 'OUTPUT' in line:
                        print('  ' + line)
                failed = True
                continue
            args = [sys.executable, os.path.join(HERE, 'model.py'), frames, display, '--size', f'{w}x{h}', '--out', out,
                    '--transfer', transfer, '--matrix', matrix, '--primaries', primaries, '--depth', str(depth),
                    '--count', str(a.frames)] + model_flags
            if rng == 'full':
                args.append('--full')
            if csp == '2':
                args.append('--colocated')
            print(f'{name}: {w}x{h} to {out} ({label}{", " + " ".join(flags) if flags else ""}), primaries {primaries} '
                  f'transfer {transfer} matrix {matrix} range {rng} chroma pos {csp}', flush=True)
            if subprocess.run(args, check=False).returncode != 0:
                failed = True
    sys.exit(1 if failed else 0)


if __name__ == '__main__':
    main()
