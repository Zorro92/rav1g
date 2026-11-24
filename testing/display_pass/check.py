#!/usr/bin/env python3
# SPDX-License-Identifier: BSD-2-Clause
"""The display pass (Rav1gSettings::display, -displaypass, docs/stage-fusion.md) against its float32 model.

  python testing/display_pass/check.py --tool rav1g-dec.exe --streams <conformance-streams> \\
      [--stream svt/syn-hdr10-10b-1080p.ivf ...] [--frames 4] [--out dir]

For every stream: rav1g-dec decodes the first frames once as they are (-o, planar) and then with
-output p010 -displaypass (P010 for a 10-bit stream, NV12 for an 8-bit one, dithered) at half the size
(the shader's 2:1 quad path), at the full size (the 1:1 block path) and at two thirds of it (the 3:2 block
path where that is exact, 1920x1080 to 1280x720; else the area loop) (-o: the display images), through
the Rust interface; and through the interface for C (-capi) with other settings of the pass: the PQ stream
with a 600-nit peak and a 203-nit white, with the transform forced to scale only, at 0.6 of its size (the
area loop) and into P010 with the ordered dither; the SDR stream into NV12 (dithered), into NV12 rounded,
and at 0.6 of its size. model.py computes the display images from the first run's frames with the
sequence's colour description (the COLOUR line of the log) and the same settings, and counts the codes
that differ. Exit code 1 when a code differs by more than one (fp32 on two compilers, the tone map's
tables), when more than 5% of a dithered plane's codes differ (a dither that is not there or not the
model's moves about half of them by one), or when a run fails.
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
    # The settings of the -capi runs, by stream: (name, size: 'half' or 'area', rav1g-dec flags, model
    # flags). The output depth and the dither follow from the flags (-displaydepth, -displaydither).
    extra = {
        'syn-hdr10-10b-1080p.ivf': [
            ('peak600-white203', 'half', ['-displaypeak', '600', '-displaywhite', '203'], ['--peak', '600', '--white', '203']),
            ('scale', 'half', ['-displaytransform', '1'], ['--transfer', '1', '--primaries', '1']),
            ('area', 'area', [], []),
            ('p010-dither', 'half', ['-displaydither', '2'], []),
        ],
        'syn-tiles-10b-1080p.ivf': [
            ('nv12', 'half', ['-displaydepth', '8'], []),
            ('nv12-rounded', 'half', ['-displaydepth', '8', '-displaydither', '1'], []),
            ('area', 'area', [], []),
        ],
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
        full, area = f'{w + 1 & ~1}x{h + 1 & ~1}', f'{w * 3 // 5 & ~1}x{h * 3 // 5 & ~1}'
        runs = [(half, 'rust', [], []), (full, 'rust', [], []), (two_thirds, 'rust', [], [])]
        runs += [(half if size == 'half' else area, label, ['-capi'] + flags, model_flags)
                 for label, size, flags, model_flags in extra.get(name, [])]
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
            # The image's depth and dither as the decoder resolves them (Rav1gDisplay::depth, dither): depth 0
            # is the stream's, dither 0 dithers 8-bit codes only.
            def flag(name, default):
                return int(flags[flags.index(name) + 1]) if name in flags else default
            out_depth = flag('-displaydepth', 0) or (10 if depth > 8 else 8)
            dither = flag('-displaydither', 0)
            dithered = dither == 2 or (dither == 0 and out_depth == 8)
            args = [sys.executable, os.path.join(HERE, 'model.py'), frames, display, '--size', f'{w}x{h}', '--out', out,
                    '--transfer', transfer, '--matrix', matrix, '--primaries', primaries, '--depth', str(depth),
                    '--out-depth', str(out_depth), '--count', str(a.frames)] + model_flags
            if dithered:
                args += ['--dither', '--max-differ', '0.05']
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
