#!/usr/bin/env python3
# SPDX-License-Identifier: BSD-2-Clause
"""A float32 model of display_pass.hlsl (the display pass prototype, docs/stage-fusion.md), held
against what the decoder wrote.

  uv run --with numpy python3 testing/display_pass/model.py <frames.yuv> <display.yuv> \\
      --size WxH --out WxH [--transfer 16] [--matrix 9] [--primaries 9] [--full] [--colocated] [--depth 8]
      [--out-depth 8] [--peak 1000] [--white 100] [--max-cll N] [--mastering-max N] [--fixed-peak]
      [--bilinear-chroma] [--dither] [--count N]
  uv run --with numpy python3 testing/display_pass/model.py --self-test

<frames.yuv>: the decoded frames as rav1g-dec -o writes them (planar 4:2:0, 16-bit little-endian
samples, the visible size; a byte a sample with --depth 8). <display.yuv>: the frames of a run with
-display WxH -output p010 -o, the same layout at the output size: 10-bit codes (P010), or 8-bit codes
(NV12) with --out-depth 8 (default: as --depth). --transfer/--matrix/--primaries/--full/--colocated:
the colour description the pass used (H.273 code points; --primaries defaults to 9 with --matrix 9, else
1; AV1 chroma_sample_position 2 is --colocated). --peak/--white: the tone map's configured source peak and
SDR white (Rav1gDisplay::source_peak_nits, target_white_nits); --max-cll/--mastering-max: the HDR metadata
the pass takes (source_peak: a PQ frame's peak follows them). --dither: the ordered dither at the last
quantisation (what Rav1gDisplay::dither gives NV12 by default). The input chroma is the Catmull-Rom cubic at
the siting, clamped to the code range (--bilinear-chroma: the bilinear one of the previous version).
The model's arithmetic does not depend on the shader's path (2:1, 1:1, 3:2, the area loop): every ratio is
the area weights of `area`, exactly 2:1 its row order.

Prints per frame and plane how many codes differ and the largest difference, and exits 1 when any
code differs by more than --tolerance (default 1: fp32 on two compilers).
"""

import argparse
import sys

import numpy as np

F = np.float32


def pq_to_nits(e):
    # In float64, then float32: near a signal of 1 the denominator c2 - c3 p cancels to 0.16 and float32 loses
    # about 7 bits there (10^-4 of 10000 nits, two codes of a saturated chroma sample), where the pass
    # interpolates a table of the float64 function (display_pass.rs, within 10^-6).
    p = np.power(np.clip(np.asarray(e, np.float64), 0.0, 1.0), 1.0 / 78.84375)
    return (np.power(np.maximum(p - 0.8359375, 0.0) / (18.8515625 - 18.6875 * p), 1.0 / 0.1593017578125) * 10000).astype(F)


def nits_to_pq(n):
    p = np.power(np.clip(n, F(0), F(10000)) / F(10000), F(0.1593017578125))
    return np.power((F(0.8359375) + F(18.8515625) * p) / (F(1) + F(18.6875) * p), F(78.84375))


def eetf(nits, peak, white):
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


def weights(matrix):
    """The luma weights of an H.273 matrix: BT.2020 (9), BT.601 (5, 6), else BT.709."""
    if matrix == 9:
        return np.array([0.2627, 0.6780, 0.0593], F)
    if matrix in (5, 6):
        return np.array([0.299, 0.587, 0.114], F)
    return np.array([0.2126, 0.7152, 0.0722], F)


def hlg_scene(e):
    e = np.clip(e, F(0), F(1))
    return np.where(e <= F(0.5), e * e / F(3), (np.exp((e - F(0.55991073)) / F(0.17883277)) + F(0.28466892)) / F(12))


# The codes of a depth: black, the range of Y', the middle and the range of chroma, and full range.
CODES = {10: (64, 876, 512, 896, 1023), 8: (16, 219, 128, 224, 255)}


def pixel_linear(y, uv, mode, matrix, wide, full, depth, peak, white):
    # wide: BT.2020 primaries, converted to BT.709.
    w = weights(matrix)
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
    top = np.max(rgb, -1)
    if wide:
        rgb = to_709(rgb)
    return rgb * (eetf(top, F(peak), F(white)) / np.maximum(top, F(1e-6)))[..., None]


def chroma_taps(f, bilinear):
    """The weights of the chroma samples base - 1 .. base + 2 at the fraction f (0, 1/4, 1/2 or 3/4) past
    base: the Catmull-Rom cubic (Keys, a = -1/2), or bilinear. float64; every value a multiple of 1/128."""
    f = f.astype(np.float64)
    if bilinear:
        return np.stack([np.zeros_like(f), 1 - f, f, np.zeros_like(f)], -1)
    f2, f3 = f * f, f * f * f
    return np.stack([0.5 * (-f3 + 2 * f2 - f), 0.5 * (3 * f3 - 5 * f2 + 2), 0.5 * (-3 * f3 + 4 * f2 + f), 0.5 * (f3 - f2)],
                    -1)


def chroma_range(depth, full, bilinear):
    """The code range the upsampled chroma is clamped to: limited 16..240 (8 bits, times 4 at 10), full all
    codes; bilinear needs none (it mixes, it does not overshoot)."""
    top = (1 << depth) - 1
    if bilinear or full:
        return 0, top
    unit = 1 << (depth - 8)
    return 16 * unit, 240 * unit


def upsample_chroma(plane, h, w, colocated, depth, full, bilinear):
    """The chroma plane at every luma pixel: chroma sample (i, j) sits at luma (2i, 2j + 1/2) between rows
    (AV1 chroma_sample_position 1, and 0), at (2i, 2j) colocated (2); a separable 4x4 filter of the coded
    samples with the coordinates clamped to the plane, then clamped to the code range. In float64, which is
    exact here (multiples of 1/2048 below 2^24 of them), and so the shader's float32 value whatever its order."""
    py, px = np.mgrid[0:h, 0:w]
    cx = px * 0.5
    cy = (py - (0.0 if colocated else 0.5)) * 0.5
    bx, by = np.floor(cx).astype(np.int64), np.floor(cy).astype(np.int64)
    wx, wy = chroma_taps(cx - bx, bilinear), chroma_taps(cy - by, bilinear)
    lw, lh = (w + 1) // 2 - 1, (h + 1) // 2 - 1  # the last chroma sample: (w + 1) / 2 of them
    src = plane.astype(np.float64)
    acc = np.zeros((h, w), np.float64)
    for i in range(4):
        row = np.clip(by - 1 + i, 0, lh)
        for j in range(4):
            acc += wy[..., i] * wx[..., j] * src[row, np.clip(bx - 1 + j, 0, lw)]
    lo, hi = chroma_range(depth, full, bilinear)
    return np.clip(acc, lo, hi).astype(F)


def source_linear(yp, up, vp, mode, matrix, wide, full, colocated, depth, peak, white, bilinear=False):
    h, w = yp.shape
    cuv = np.stack([upsample_chroma(up, h, w, colocated, depth, full, bilinear),
                    upsample_chroma(vp, h, w, colocated, depth, full, bilinear)], -1)
    return pixel_linear(yp.astype(F), cuv, mode, matrix, wide, full, depth, peak, white)


def source_peak(mode, configured, max_cll=0.0, mastering_max=0.0, fixed=False):
    """The source peak of the EETF for a frame (display_pass.rs `source_peak`, docs/integration.md "The
    source peak"): for PQ (mode 0) MaxCLL when above 0, else the mastering display's largest luminance when
    above 0, the smaller of the two when both are (a MaxCLL above the mastering display's is broken
    metadata), clamped to 400..10000 nits; the configured peak when neither is, for HLG and SDR, and with
    `fixed` (av1_set_display_fixed_peak). The values are what the stream says, or where it says nothing
    (or 0) what the application does."""
    if mode != 0 or fixed:
        return configured
    if max_cll > 0 and mastering_max > 0:
        peak = min(max_cll, mastering_max)
    elif max_cll > 0:
        peak = max_cll
    elif mastering_max > 0:
        peak = mastering_max
    else:
        return configured
    return min(max(peak, 400.0), 10000.0)


def self_test():
    """The rule of source_peak on the cases of docs/integration.md (Bale's console checks among them)."""
    cases = [
        ((0, 1000, 4057, 1000), 1000),  # MaxCLL above the mastering display's: the mastering display's
        ((0, 1000, 1184, 4000), 1184),  # MaxCLL
        ((0, 1000, 0, 0), 1000),  # none: the configured peak
        ((0, 600, 0, 0), 600),  # the configured peak is not clamped
        ((0, 1000, 1500, 0), 1500),  # MaxCLL only
        ((0, 1000, 0, 4000), 4000),  # the mastering display only
        ((0, 1000, 250, 0), 400),  # clamped below
        ((0, 1000, 0, 100), 400),
        ((0, 1000, 12000, 0), 10000),  # clamped above
        ((0, 1000, 0, 50000), 10000),
        ((2, 1000, 4000, 4000), 1000),  # HLG: configured
        ((1, 1000, 4000, 4000), 1000),  # SDR: configured (no tone map)
    ]
    for (mode, configured, cll, mastering), want in cases:
        got = source_peak(mode, configured, cll, mastering)
        assert got == want, f'source_peak{(mode, configured, cll, mastering)} = {got}, not {want}'
    assert source_peak(0, 1000, 4000, 4000, fixed=True) == 1000
    # The chroma filter: the weights at the quarter phases are multiples of 1/128 that sum to 1, and the
    # cubic passes through the samples, reproduces a line and a parabola.
    w = chroma_taps(np.array([0.0, 0.25, 0.5, 0.75]), False)
    assert (w * 128 == np.round(w * 128)).all() and (w.sum(-1) == 1).all()
    assert (w[0] == [0, 1, 0, 0]).all() and (w[2] * 16 == [-1, 9, 9, -1]).all()
    assert (w[1] * 128 == [-9, 111, 29, -3]).all() and (w[3] * 128 == [-3, 29, 111, -9]).all()
    x = np.arange(-1, 3, dtype=np.float64)
    for f, row in zip([0.0, 0.25, 0.5, 0.75], w):
        assert row @ (3 * x + 5) == 3 * f + 5 and row @ (x * x) == f * f
    print('model.py: self test passed')


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
    y = e @ weights(1)
    return y, (e[..., 2] - y) / F(2 * (1 - 0.0722)), (e[..., 0] - y) / F(2 * (1 - 0.2126))


# The ordered dither of the last quantisation (display_pass.hlsl `threshold`): the R2 sequence's lattice
# frac(x / g + y / g^2) of the plastic number g, in 32-bit fixed point, its top 8 bits: (t + 1/2) / 256.
R2_X, R2_Y = 0xc13fa9a9, 0x91e10da6


def threshold(h, w, transposed=False):
    y, x = np.mgrid[0:h, 0:w].astype(np.uint64)
    if transposed:
        x, y = y, x
    t = ((x * R2_X + y * R2_Y) & 0xffffffff) >> 24
    return (t.astype(F) * F(1 / 256) + F(0.5 / 256)).astype(F)


def display(yp, up, vp, ow, oh, mode, matrix, wide, full, colocated, depth=10, out_depth=10, peak=1000, white=100,
            dither=False, bilinear=False):
    lin = source_linear(yp, up, vp, mode, matrix, wide, full, colocated, depth, peak, white, bilinear)
    sh, sw = yp.shape
    vals = area_2to1(lin) if (sw == 2 * ow and sh == 2 * oh) else area(lin, ow, oh)
    black, yr, mid, cr, _ = CODES[out_depth]
    y, _, _ = encode(vals, mode, wide)
    half = F(0.5)
    yq = np.clip(np.floor(F(black) + F(yr) * y + (threshold(oh, ow) if dither else half)), black, black + yr)
    left = np.concatenate([vals[:, :1], vals[:, :-1]], 1)  # column 2i-1, the edge repeated
    acc = np.zeros((oh // 2, ow // 2, 3), F)
    for dy in range(2):
        rows = slice(dy, oh, 2)
        acc = acc + F(0.125) * left[rows, 0::2]
        acc = acc + F(0.25) * vals[rows, 0::2]
        acc = acc + F(0.125) * vals[rows, 1::2]
    _, cb, cr_ = encode(acc, mode, wide)
    tb = threshold(oh // 2, ow // 2) if dither else half
    tr = threshold(oh // 2, ow // 2, True) if dither else half
    uq = np.clip(np.floor(F(mid) + F(cr) * cb + tb), black, 2 * mid - black)
    vq = np.clip(np.floor(F(mid) + F(cr) * cr_ + tr), black, 2 * mid - black)
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
    ap.add_argument('frames_file', metavar='frames.yuv', nargs='?')
    ap.add_argument('display_file', metavar='display.yuv', nargs='?')
    ap.add_argument('--size')
    ap.add_argument('--out')
    ap.add_argument('--transfer', type=int, default=1)
    ap.add_argument('--matrix', type=int, default=1)
    ap.add_argument('--primaries', type=int, default=0)
    ap.add_argument('--out-depth', type=int, default=0, choices=(0, 8, 10))
    ap.add_argument('--peak', type=float, default=1000, help='the configured source peak (Rav1gDisplay::source_peak_nits)')
    ap.add_argument('--max-cll', type=float, default=0, help='the MaxCLL the pass takes (the stream\'s, else the application\'s), 0 none')
    ap.add_argument('--mastering-max', type=float, default=0,
                    help='the mastering display\'s largest luminance the pass takes, nits, 0 none')
    ap.add_argument('--fixed-peak', action='store_true', help='the configured peak whatever the metadata (av1_set_display_fixed_peak)')
    ap.add_argument('--bilinear-chroma', action='store_true', help='bilinear input chroma (av1_set_display_bilinear_chroma)')
    ap.add_argument('--self-test', action='store_true', help='check the peak rule and the chroma weights, then exit')
    ap.add_argument('--white', type=float, default=100)
    ap.add_argument('--full', action='store_true')
    ap.add_argument('--colocated', action='store_true')
    ap.add_argument('--dither', action='store_true', help='the ordered dither of the quantisation (Rav1gDisplay::dither)')
    ap.add_argument('--depth', type=int, default=10, choices=(8, 10))
    ap.add_argument('--count', type=int, default=1 << 30)
    ap.add_argument('--tolerance', type=int, default=1)
    ap.add_argument('--max-differ', type=float, default=1.0,
                    help='the largest share of the codes of a plane that may differ at all (a dither that is not '
                    'there moves codes by one only, within the tolerance, but about half of them)')
    a = ap.parse_args()
    if a.self_test:
        self_test()
        return
    if not (a.frames_file and a.display_file and a.size and a.out):
        ap.error('frames.yuv, display.yuv, --size and --out are needed')
    sw, sh = map(int, a.size.split('x'))
    ow, oh = map(int, a.out.split('x'))
    mode = {16: 0, 18: 2}.get(a.transfer, 1)
    primaries = a.primaries or (9 if a.matrix == 9 else 1)
    wide = primaries == 9
    out_depth = a.out_depth or a.depth
    peak = source_peak(mode, a.peak, a.max_cll, a.mastering_max, a.fixed_peak)
    print(f'source peak {peak:g} nits')
    worst = 0
    share = 0.0
    for n, (src, got) in enumerate(zip(frames(a.frames_file, sw, sh, a.depth), frames(a.display_file, ow, oh, out_depth))):
        if n >= a.count:
            break
        want = display(*[p.astype(F) for p in src], ow, oh, mode, a.matrix, wide, a.full, a.colocated, a.depth, out_depth,
                       peak, a.white, a.dither, a.bilinear_chroma)
        line = []
        far = []
        for name, w_, g in zip('YUV', want, got):
            d = np.abs(w_.astype(np.int64) - g.astype(np.int64))
            worst = max(worst, int(d.max()))
            share = max(share, float((d > 0).mean()))
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
    print(f'largest difference {worst} (tolerance {a.tolerance}); at most {100 * share:.2f}% of a plane\'s codes differ '
          f'(at most {100 * a.max_differ:g}%)')
    sys.exit(1 if worst > a.tolerance or share > a.max_differ else 0)


if __name__ == '__main__':
    main()
