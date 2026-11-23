#!/usr/bin/env python3
# SPDX-License-Identifier: BSD-2-Clause
"""A float32 model of display_pass.hlsl (the display pass prototype, docs/stage-fusion.md), held
against what the decoder wrote.

  uv run --with numpy python3 testing/display_pass/model.py <frames.yuv> <display.yuv> \\
      --size WxH --out WxH [--transfer 16] [--matrix 9] [--full] [--colocated] [--depth 8] [--count N]

<frames.yuv>: the decoded frames as rav1g-dec -o writes them (planar 4:2:0, 16-bit little-endian
samples, the visible size; a byte a sample with --depth 8). <display.yuv>: the frames of a run with
-displaypass WxH -output p010 -o, the same layout at the output size: 10-bit codes (P010), or 8-bit
codes (NV12) for an 8-bit stream. --transfer/--matrix/--full/--colocated: the
sequence's colour description (H.273 code points; AV1 chroma_sample_position 2 is --colocated).

Prints per frame and plane how many codes differ and the largest difference, and exits 1 when any
code differs by more than --tolerance (default 1: fp32 on two compilers).
"""

import argparse
import sys

import numpy as np

F = np.float32


def pq_to_nits(e):
    p = np.power(np.clip(e, F(0), F(1)), F(1.0 / 78.84375))
    return np.power(np.maximum(p - F(0.8359375), F(0)) / (F(18.8515625) - F(18.6875) * p), F(1.0 / 0.1593017578125)) * F(10000)


def nits_to_pq(n):
    p = np.power(np.clip(n, F(0), F(10000)) / F(10000), F(0.1593017578125))
    return np.power((F(0.8359375) + F(18.8515625) * p) / (F(1) + F(18.6875) * p), F(78.84375))


def eetf(nits, peak=F(1000), white=F(100)):
    nits = np.minimum(np.maximum(nits, F(0)), peak)
    target = min(white * F(2.03), F(10000))
    if peak <= target:
        return np.minimum(nits / target, F(1))
    pk = nits_to_pq(np.array(peak, F))
    s = nits_to_pq(nits) / pk
    top = nits_to_pq(np.array(target, F)) / pk
    knee = F(2) * top - F(1)
    t = (s - knee) / (F(1) - knee)
    t2 = t * t
    t3 = t2 * t
    hermite = (F(2) * t3 - F(3) * t2 + F(1)) * knee + (t3 - F(2) * t2 + t) * (F(1) - knee) + (F(-2) * t3 + F(3) * t2) * top
    mapped = np.where(s >= knee, hermite, s)
    return np.clip(pq_to_nits(mapped * pk) / target, F(0), F(1))


M709 = np.array([[1.660491, -0.5876411, -0.07284986],
                 [-0.1245505, 1.1328999, -0.00834942],
                 [-0.01815076, -0.1005789, 1.1187297]], F)


def to_709(rgb):
    return np.einsum('ij,...j->...i', M709, rgb).astype(F)


def weights(wide):
    return np.array([0.2627, 0.6780, 0.0593] if wide else [0.2126, 0.7152, 0.0722], F)


def hlg_scene(e):
    e = np.clip(e, F(0), F(1))
    return np.where(e <= F(0.5), e * e / F(3), (np.exp((e - F(0.55991073)) / F(0.17883277)) + F(0.28466892)) / F(12))


# The codes of a depth: black, the range of Y', the middle and the range of chroma, and full range.
CODES = {10: (64, 876, 512, 896, 1023), 8: (16, 219, 128, 224, 255)}


def pixel_linear(y, uv, mode, wide, full, depth=10):
    w = weights(wide)
    black, yr, mid, cr, top = map(F, CODES[depth])
    y = y / top if full else (y - black) / yr
    uv = (uv - mid) / (top if full else cr)
    r = y + F(2) * (F(1) - w[0]) * uv[..., 1]
    b = y + F(2) * (F(1) - w[2]) * uv[..., 0]
    g = (y - w[0] * r - w[2] * b) / w[1]
    e = np.stack([r, g, b], -1).astype(F)
    m = np.abs(e)
    s = np.sign(e).astype(F)
    if mode == 1:
        rgb = s * np.power(np.minimum(m, F(1.5)), F(2.4))
        return to_709(rgb) if wide else rgb
    if mode == 2:
        scene = s * hlg_scene(m)
        ys = np.maximum(scene @ np.array([0.2627, 0.6780, 0.0593], F), F(0))
        rgb = scene * (F(1000) * np.power(ys, F(0.2)))[..., None]
    else:
        rgb = s * pq_to_nits(m)
    peak = np.max(rgb, -1)
    if wide:
        rgb = to_709(rgb)
    return rgb * (eetf(peak) / np.maximum(peak, F(1e-6)))[..., None]


def source_linear(yp, up, vp, mode, wide, full, colocated, depth=10):
    h, w = yp.shape
    py, px = np.mgrid[0:h, 0:w]
    cx = px.astype(F) * F(0.5)
    cy = (py.astype(F) - (F(0) if colocated else F(0.5))) * F(0.5)
    bx = np.floor(cx).astype(np.int64)
    by = np.floor(cy).astype(np.int64)
    fx = (cx - bx).astype(F)[..., None]
    fy = (cy - by).astype(F)[..., None]
    lw, lh = w // 2 - 1, h // 2 - 1
    uv = np.stack([up, vp], -1).astype(F)

    def at(x, y):
        return uv[np.clip(y, 0, lh), np.clip(x, 0, lw)]

    c = (at(bx, by) + (at(bx + 1, by) - at(bx, by)) * fx)
    d = (at(bx, by + 1) + (at(bx + 1, by + 1) - at(bx, by + 1)) * fx)
    cuv = c + (d - c) * fy
    return pixel_linear(yp.astype(F), cuv, mode, wide, full, depth)


def weights_1d(src, out):
    """The overlap of every source unit interval with every output pixel's footprint: (out, src)."""
    w = np.zeros((out, src), F)
    scale = F(src) / F(out)
    for o in range(out):
        lo = F(o) * scale
        hi = lo + scale
        first, last = int(np.floor(lo)), min(int(np.ceil(hi)) - 1, src - 1)
        for i in range(first, last + 1):
            w[o, i] = min(F(i + 1), hi) - max(F(i), lo)
    return w


def area(lin, ow, oh):
    # Each output pixel: its source pixels weighted by the overlap of their unit squares with its
    # footprint (wy * wx, separable), over the sum of the weights. Summed by rows, then columns: the
    # shader sums in another order, a few ulp apart.
    sh, sw, _ = lin.shape
    wy, wx = weights_1d(sh, oh), weights_1d(sw, ow)
    rows = np.einsum('ys,sxc->yxc', wy, lin).astype(F)
    out = np.einsum('xs,ysc->yxc', wx, rows).astype(F)
    total = (wy.sum(1)[:, None] * wx.sum(1)[None, :]).astype(F)
    return (out / np.maximum(total, F(1e-6))[..., None]).astype(F)


def area_2to1(lin):
    # Exact 2:1: four pixels of weight 1 each, the order of the general loop (row by row).
    a = lin[0::2, 0::2] + lin[0::2, 1::2]
    a = a + lin[1::2, 0::2]
    a = a + lin[1::2, 1::2]
    return a / F(4)


def encode(c, mode, wide):
    if not (mode == 1 and not wide):
        c = np.clip(c, F(0), F(1))
    e = np.sign(c).astype(F) * np.power(np.minimum(np.abs(c), F(3)), F(1 / 2.4))
    y = e @ weights(False)
    return y, (e[..., 2] - y) / F(2 * (1 - 0.0722)), (e[..., 0] - y) / F(2 * (1 - 0.2126))


def display(yp, up, vp, ow, oh, mode, wide, full, colocated, depth=10):
    lin = source_linear(yp, up, vp, mode, wide, full, colocated, depth)
    sh, sw = yp.shape
    vals = area_2to1(lin) if (sw == 2 * ow and sh == 2 * oh) else area(lin, ow, oh)
    black, yr, mid, cr, _ = CODES[depth]
    y, _, _ = encode(vals, mode, wide)
    yq = np.clip(np.floor(F(black) + F(yr) * y + F(0.5)), black, black + yr)
    left = np.concatenate([vals[:, :1], vals[:, :-1]], 1)  # column 2i-1, the edge repeated
    acc = np.zeros((oh // 2, ow // 2, 3), F)
    for dy in range(2):
        rows = slice(dy, oh, 2)
        acc = acc + F(0.125) * left[rows, 0::2]
        acc = acc + F(0.25) * vals[rows, 0::2]
        acc = acc + F(0.125) * vals[rows, 1::2]
    _, cb, cr_ = encode(acc, mode, wide)
    uq = np.clip(np.floor(F(mid) + F(cr) * cb + F(0.5)), black, 2 * mid - black)
    vq = np.clip(np.floor(F(mid) + F(cr) * cr_ + F(0.5)), black, 2 * mid - black)
    return yq, uq, vq


def frames(path, w, h, depth=10):
    cw, ch = (w + 1) // 2, (h + 1) // 2
    samples = w * h + 2 * cw * ch
    data = np.fromfile(path, '<u2' if depth > 8 else 'u1')
    for i in range(len(data) // samples):
        f = data[i * samples:(i + 1) * samples]
        yield (f[:w * h].reshape(h, w), f[w * h:w * h + cw * ch].reshape(ch, cw), f[w * h + cw * ch:].reshape(ch, cw))


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument('frames_file', metavar='frames.yuv')
    ap.add_argument('display_file', metavar='display.yuv')
    ap.add_argument('--size', required=True)
    ap.add_argument('--out', required=True)
    ap.add_argument('--transfer', type=int, default=1)
    ap.add_argument('--matrix', type=int, default=1)
    ap.add_argument('--full', action='store_true')
    ap.add_argument('--colocated', action='store_true')
    ap.add_argument('--depth', type=int, default=10, choices=(8, 10))
    ap.add_argument('--count', type=int, default=1 << 30)
    ap.add_argument('--tolerance', type=int, default=1)
    a = ap.parse_args()
    sw, sh = map(int, a.size.split('x'))
    ow, oh = map(int, a.out.split('x'))
    mode = {16: 0, 18: 2}.get(a.transfer, 1)
    wide = a.matrix == 9
    worst = 0
    for n, (src, got) in enumerate(zip(frames(a.frames_file, sw, sh, a.depth), frames(a.display_file, ow, oh, a.depth))):
        if n >= a.count:
            break
        want = display(*[p.astype(F) for p in src], ow, oh, mode, wide, a.full, a.colocated, a.depth)
        line = []
        far = []
        for name, w_, g in zip('YUV', want, got):
            d = np.abs(w_.astype(np.int64) - g.astype(np.int64))
            worst = max(worst, int(d.max()))
            line.append(f'{name} {int((d > 0).sum())} differ, max {int(d.max())}')
            for y, x in np.argwhere(d > a.tolerance)[:4]:
                far.append(f'  {name} at ({x}, {y}): model {int(w_[y, x])}, pass {int(g[y, x])}')
        print(f'frame {n}: ' + '; '.join(line))
        if far:
            # The codes too far apart, and the source pixels of the first one's footprint (a 2:1 guess).
            print('\n'.join(far))
            name, at = far[0].split()[0], far[0].split('(')[1].split(')')[0]
            x, y = map(int, at.split(', '))
            sx, sy = (x, y) if name == 'Y' else (2 * x, 2 * y)
            print(f'  source Y around ({2 * sx}, {2 * sy}): {src[0][2 * sy:2 * sy + 2, 2 * sx:2 * sx + 2].tolist()}, '
                  f'U {src[1][sy:sy + 2, sx:sx + 2].tolist()}, V {src[2][sy:sy + 2, sx:sx + 2].tolist()}')
    print(f'largest difference {worst} (tolerance {a.tolerance})')
    sys.exit(1 if worst > a.tolerance else 0)


if __name__ == '__main__':
    main()
