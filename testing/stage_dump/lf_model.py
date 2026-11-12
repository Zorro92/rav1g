#!/usr/bin/env python3
"""A model of the 8-bit loop filter shaders, run on the stages of a dump.

  lf_model.py <frame.stages> [--lists <frame.stages or .digest>] [--variant written|console-0.2.25]

The dump of a frame (-dumpstages, testing/stage_dump/stage_dump.py) has the frame before the loop
filter (stage `intra`), after the vertical edges (`lf_vert`) and after the horizontal ones (`lf_horz`),
and, from format 2 on, what the filter reads besides the frame: the constants (`lf_params`) and the
edge lists (`lf_blocks`). This script filters the dump's own `intra` with the arithmetic of
libav1/dx/shaders/loopfilter_vert.hlsl and compares the result with the dump's `lf_vert`, then filters
that `lf_vert` with the arithmetic of loopfilter_hor.hlsl and compares with `lf_horz`: per plane, the
number of samples that differ, and how many edges took which branch of the filter. A stage that the
model reproduces was computed as the shader is written; one it does not was not.

--lists takes the constants and the edge lists from another dump of the same frame: a dump of an
older package has none, and a digest keeps both as they are. When that file is a digest, the model's
output is also compared with its row hashes of `lf_vert` and `lf_horz`. The lists are the work of
gen_lf_blocks_vert/hor on the mode info; another machine's lists are the right ones when its stages
in front of the loop filter are equal (stage_dump.py compare).

--variant console-0.2.25 changes one thing: in the 8-tap branch of the vertical filter the first step
on the q side adds p1.w - q0.x where the shader has q0.w - p1.x. That is what the console did with
the shaders of package 0.2.25.0 (docs/gpu-av1-decoder.md, "A paired read before an overlapping
in-place write"): with it the model gives the console's `lf_vert` of that package to the last sample.

8-bit frames only. No dependencies beyond the Python standard library and stage_dump.py.
"""
import argparse
import hashlib
import os
import sys
from collections import Counter

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import stage_dump as sd  # noqa: E402


def clamp(value, low, high):
    return low if value < low else high if value > high else value


def filter_edge(a, kind, lim, mblim, hev_thr, console_8tap):
    """The filter of one edge of one row or column. a: the sixteen samples across the edge, a[8] the
    first one behind it (p0.x .. p0.w, p1.x .. p1.w, q0.x .. q0.w, q1.x .. q1.w of the shader).
    Returns (branch, {index: new sample}), or None when the mask is 0."""
    p0y, p0z, p0w, p1x, p1y, p1z, p1w, q0x, q0y, q0z, q0w, q1x, q1y, q1z = a[1:15]
    mask = abs(p1z - p1w) <= lim and abs(q0x - q0y) <= lim and abs(p1w - q0x) * 2 + (abs(p1z - q0y) >> 1) <= mblim
    mask = mask and ((abs(p1y - p1z) <= lim and abs(q0y - q0z) <= lim) or kind == 0)
    mask = mask and ((abs(p1x - p1y) <= lim and abs(q0z - q0w) <= lim) or kind <= 1)
    if not mask:
        return None
    flat_uv = abs(p1y - p1w) <= 1 and abs(p1z - p1w) <= 1 and abs(q0y - q0x) <= 1 and abs(q0z - q0x) <= 1
    flat = flat_uv and abs(p1x - p1w) <= 1 and abs(q0w - q0x) <= 1
    flat2 = (abs(p0y - p1w) <= 1 and abs(p0z - p1w) <= 1 and abs(p0w - p1w) <= 1 and abs(q1x - q0x) <= 1 and
             abs(q1y - q0x) <= 1 and abs(q1z - q0x) <= 1)
    if kind == 3 and flat and flat2:
        return '14-tap', {
            2: (p0y * 7 + p0z * 2 + p0w * 2 + p1x + p1y + p1z + p1w + q0x + 8) >> 4,
            3: (p0y * 5 + p0z * 2 + p0w * 2 + p1x * 2 + p1y + p1z + p1w + q0x + q0y + 8) >> 4,
            4: (p0y * 4 + p0z + p0w * 2 + p1x * 2 + p1y * 2 + p1z + p1w + q0x + q0y + q0z + 8) >> 4,
            5: (p0y * 3 + p0z + p0w + p1x * 2 + p1y * 2 + p1z * 2 + p1w + q0x + q0y + q0z + q0w + 8) >> 4,
            6: (p0y * 2 + p0z + p0w + p1x + p1y * 2 + p1z * 2 + p1w * 2 + q0x + q0y + q0z + q0w + q1x + 8) >> 4,
            7: (p0y + p0z + p0w + p1x + p1y + p1z * 2 + p1w * 2 + q0x * 2 + q0y + q0z + q0w + q1x + q1y + 8) >> 4,
            8: (p0z + p0w + p1x + p1y + p1z + p1w * 2 + q0x * 2 + q0y * 2 + q0z + q0w + q1x + q1y + q1z + 8) >> 4,
            9: (p0w + p1x + p1y + p1z + p1w + q0x * 2 + q0y * 2 + q0z * 2 + q0w + q1x + q1y + q1z * 2 + 8) >> 4,
            10: (p1x + p1y + p1z + p1w + q0x + q0y * 2 + q0z * 2 + q0w * 2 + q1x + q1y + q1z * 3 + 8) >> 4,
            11: (p1y + p1z + p1w + q0x + q0y + q0z * 2 + q0w * 2 + q1x * 2 + q1y + q1z * 4 + 8) >> 4,
            12: (p1z + p1w + q0x + q0y + q0z + q0w * 2 + q1x * 2 + q1y * 2 + q1z * 5 + 8) >> 4,
            13: (p1w + q0x + q0y + q0z + q0w + q1x * 2 + q1y * 2 + q1z * 7 + 8) >> 4}
    if kind >= 2 and flat:
        out = {}
        v = p1x * 3 + p1y + p1z + p1w + q0x
        out[5] = (v + p1y + 4) >> 3
        v += -p1x + q0y
        out[6] = (v + p1z + 4) >> 3
        v += -p1x + q0z
        out[7] = (v + p1w + 4) >> 3
        v += (p1w - q0x) if console_8tap else (-p1x + q0w)
        out[8] = (v + q0x + 4) >> 3
        v += -p1y + q0w
        out[9] = (v + q0y + 4) >> 3
        v += -p1z + q0w
        out[10] = (v + q0z + 4) >> 3
        return '8-tap', out
    if kind == 1 and flat_uv:
        return '6-tap', {6: (p1y * 3 + p1z * 2 + p1w * 2 + q0x + 4) >> 3,
                         7: (p1y + p1z * 2 + p1w * 2 + q0x * 2 + q0y + 4) >> 3,
                         8: (p1z + p1w * 2 + q0x * 2 + q0y * 2 + q0z + 4) >> 3,
                         9: (p1w + q0x * 2 + q0y * 2 + q0z * 3 + 4) >> 3}
    hev = -1 if (abs(p1w - p1z) > hev_thr or abs(q0x - q0y) > hev_thr) else 0
    ps1, ps0, qs0, qs1 = p1z - 128, p1w - 128, q0x - 128, q0y - 128
    f0 = clamp(ps1 - qs1, -128, 127) & hev
    f0 = clamp(f0 + 3 * (qs0 - ps0), -128, 124)
    f1 = min(f0 + 4, 127) >> 3
    f2 = (f0 + 3) >> 3
    out = {7: clamp(ps0 + f2, -128, 127) + 128, 8: clamp(qs0 - f1, -128, 127) + 128}
    f0 = ((f1 + 1) >> 1) & ~hev
    out[6] = clamp(ps1 + f0, -128, 127) + 128
    out[9] = clamp(qs1 - f0, -128, 127) + 128
    return '4-tap', out


def run_filter(frame, plane, direction, lists, params, console_8tap):
    """The buffer of `frame` with the edges of one direction of one plane filtered, and the branches taken."""
    offset, stride, width, height = frame.planes[plane]
    buf = bytearray(frame.data)
    item = [i for i in sd.lf_lists(lists) if i['dir'] == direction and i['plane'] == plane][0]
    words = sd.words_of(params.data, params.num('gen_size'), params.num('lf_size') // 4)
    limits = [(words[12 + 4 * level], words[13 + 4 * level], words[14 + 4 * level]) for level in range(64)]
    branches = Counter()
    if not item['on']:
        return buf, branches
    cols = item['cols']
    # Vertical edges: the blocks with an even number, then the odd ones, as the two dispatches do.
    order = [b for p in (0, 1) for b in range(p, item['count'], 2)] if direction == 'vert' else range(item['count'])
    for block in order:
        edges, _ = sd.lf_block_edges(lists.data, item, block)
        bx, by = block % cols, block // cols
        for wi in range(4):
            for entry, level, kind, _ in edges:
                if not level:
                    continue
                if direction == 'vert':
                    pos, step = offset + (by * 4 + wi) * stride + bx * 64 + entry * 4, 1
                else:
                    pos, step = offset + (by * 64 + entry * 4) * stride + bx * 4 + wi, stride
                if pos - 8 * step < 0 or pos + 7 * step >= len(buf):
                    continue
                result = filter_edge(buf[pos - 8 * step:pos + 8 * step:step], kind, *limits[level],
                                     console_8tap and direction == 'vert')
                if result:
                    branches[result[0]] += 1
                    for k, value in result[1].items():
                        buf[pos + (k - 8) * step] = value
    return buf, branches


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument('dump', help='the .stages file of a frame')
    parser.add_argument('--lists', help='another dump or digest of the same frame with lf_params and lf_blocks')
    parser.add_argument('--variant', choices=('written', 'console-0.2.25'), default='written')
    args = parser.parse_args()
    dumps = {d.stage: d for d in sd.read_file(args.dump)}
    source = {d.stage: d for d in sd.read_file(args.lists)} if args.lists else dumps
    for stage in ('intra', 'lf_vert', 'lf_horz'):
        if stage not in dumps or dumps[stage].data is None:
            raise SystemExit('lf_model.py: %s has no samples of stage %s' % (args.dump, stage))
    for stage in ('lf_params', 'lf_blocks'):
        if stage not in source or source[stage].data is None:
            raise SystemExit('lf_model.py: no %s in %s (a dump of format 2, or --lists)' %
                             (stage, args.lists or args.dump))
    if dumps['intra'].bit_depth != 8 or dumps['intra'].bytes_per_sample != 1:
        raise SystemExit('lf_model.py: an 8-bit frame is needed')
    print('%s, the filter %s' % (args.dump, 'as the shaders are written' if args.variant == 'written' else
                                 'with the 8-tap step of the console, package 0.2.25.0'))
    differs = False
    for direction, before, after in (('vert', 'intra', 'lf_vert'), ('horz', 'lf_vert', 'lf_horz')):
        for plane in range(3):
            offset, stride, width, height = dumps[before].planes[plane]
            if not width or not height:
                continue
            buf, branches = run_filter(dumps[before], plane, direction, source['lf_blocks'], source['lf_params'],
                                       args.variant != 'written')
            rows = [bytes(buf[offset + y * stride:offset + y * stride + width]) for y in range(height)]
            want = dumps[after].rows(plane)
            samples = sum(sum(1 for x in range(width) if a[x] != b[x]) for a, b in zip(rows, want) if a != b)
            differs = differs or samples > 0
            line = '  %s edges, %s: model of %s against %s: %d of %d samples differ' % (
                'vertical' if direction == 'vert' else 'horizontal', sd.PLANE_NAMES[plane], before, after, samples,
                width * height)
            other = source.get(after)
            if args.lists and other is not None and other.row_hashes is not None:
                hashes = other.row_hashes[plane]
                line += '; against the row hashes of %s: %d of %d rows differ' % (
                    os.path.basename(args.lists), sum(1 for y in range(height)
                                                      if hashlib.md5(rows[y]).digest()[:8] != hashes[y]), height)
            print(line)
            print('      edges of a row or column that were filtered: %s' %
                  (', '.join('%s %d' % kv for kv in sorted(branches.items())) or 'none'))
    return 1 if differs else 0


if __name__ == '__main__':
    sys.exit(main())
