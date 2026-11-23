/*
 * SPDX-License-Identifier: BSD-2-Clause
 *
 * This source code is subject to the terms of the BSD 2 Clause License and the Alliance for Open Media
 * Patent License 1.0. If the BSD 2 Clause License was not distributed with this source code in the LICENSE
 * file, you can obtain it at www.aomedia.org/license/software. If the Alliance for Open Media Patent
 * License 1.0 was not distributed with this source code in the PATENTS file, you can obtain it at
 * www.aomedia.org/license/patent.
 */

// The display pass (docs/stage-fusion.md, option B; Rav1gSettings::display, docs/integration.md): the
// decoded frame, read from its frame buffer (or, for a film grain frame, from the grained copy the film
// grain pass wrote into the back buffer), goes straight into a display-size texture as BT.709 SDR (P010
// or NV12), instead of the full-size output image an application's own stage downscales and tone-maps.
//
// The operations are those of the display stage of the application this is made for (area downscale in
// display-linear light, exact overlap weights; PQ through the BT.2390 EETF to the SDR white's 2.03
// times, HLG through its inverse OETF and OOTF, BT.2020 SDR by gamut only, BT.709 SDR scaled only; one
// late clip; left-sited output chroma mixed in linear light), written here from ST 2084, ITU-R BT.2100,
// BT.2390, BT.2087 and BT.709/BT.1886. testing/display_pass/model.py is the float32 statement of them
// that the pass is held to. Not a decoding stage: the frame buffer is only read, and every frame the
// decoder outputs or references is unchanged.
//
// What makes it fast (docs/stage-fusion.md, "The display pass, fast"):
//   - The reads of a quad (below) are 12 loads of 8 bytes with no branch between them, issued together:
//     the frame's sample size is a shift and a mask, not a branch around every load. The first version
//     branched on the sample size at every one of its 36 loads a quad, and the compiler waited for each
//     load before the next (RDNA2 listing: buffer_load_dword, s_waitcnt vmcnt(0), 36 times a quad):
//     a quad cost 36 memory latencies, which 8 waves a SIMD could not hide (console: 1.98 ms a 4K
//     frame for SDR, against ~0.6 ms of arithmetic).
//   - PQ: the EOTF (ST 2084) and the tone-map gain (the EETF of the largest channel over that channel,
//     a function of the largest R'G'B' signal alone) are tables the host makes once a decoder
//     (crates/rav1g/src/host/display_pass.rs): a cubic an interval (16 bytes), within 1e-6 of the
//     function, the float32 level of the model's own arithmetic; one 16-byte group memory read and three
//     FMAs a value. HLG takes the gain table on log2 nits; SDR computes its power 2.4 directly (no table
//     is cheaper at the same accuracy) and loads no table.
//   - BT.2020 to BT.709 is linear and so is the area average: it is applied once to the average of an
//     output pixel, not to every source pixel, and the gain is folded into the sum.
//   - At 2:1 (3840x2160 to 1920x1080, the case the pass is sized for) a thread takes 2x2 output pixels
//     from four rows of its source read with 8-byte loads (a wave reads 256 contiguous bytes a row) and
//     four chroma rows of three columns, one 8-byte load each a plane; the chroma is interpolated at the
//     sited positions with constant weights. Other ratios take the area loop of the model.
//   - A group is 256 threads and 64x64 output pixels; the output chroma's left neighbour column comes
//     through group memory from the thread to the left, and one column more on the left of the tile
//     (64 rows) is computed by the first 32 threads, a quad each (1.6% more pixels).
// Group memory 12.5 KB (the tables 5.5 KB, the column exchange 6 KB, the edge column 0.75 KB); SDR reads
// no tables (9 KB in the RDNA2 listing).
//
// The 8-byte reads of the quad path reach up to two chroma samples past the last visible one of a row:
// inside the 16-sample border every frame buffer row has (frame.rs, `h_border`), and the values are not
// used.

ByteAddressBuffer lut_src : register(t0);
RWByteAddressBuffer pool : register(u0);

#define SP_TEXTURE
#define SP_LUMA_REGISTER u1
#define SP_CHROMA_REGISTER u2
#include "output_sp.h"

cbuffer cb_display : register(b0) {
  uint cb_src_w;      // visible size of the frame
  uint cb_src_h;
  uint cb_out_w;      // output size, even
  uint cb_out_h;
  uint cb_y_offset;   // the planes of the frame buffer: 16-bit samples, 8-bit with bit 6 of the flags
  uint cb_u_offset;
  uint cb_v_offset;
  uint cb_y_stride;
  uint cb_uv_stride;
  uint cb_flags;      // bits 0-1: 0 PQ, 1 SDR, 2 HLG; bit 2: BT.2020 primaries (to BT.709); bit 3: full
                      // range; bit 4: chroma between rows; bit 5: exactly 2:1 with planes at multiples of
                      // 4 bytes (the quad path); bit 6: 8-bit samples (else 16-bit); bit 7: 8-bit codes,
                      // NV12 (else 10-bit, P010)
  float cb_eotf_a;    // the EOTF table's position of a signal m: min(m * a, m * b + c), at most EOTF_N
  float cb_eotf_b;
  float cb_eotf_c;
  float cb_gain_s1;      // the gain table's position of x (PQ: the largest signal; HLG: log2 nits):
  float cb_gain_o1;      // min(x * s1 + o1, x * s2 + o2), 0 to GAIN_N
  float cb_gain_s2;
  float cb_gain_o2;
  float cb_exact_below;  // PQ: below this largest signal the peak's nits are computed, not interpolated
  // Y'CbCr codes to R'G'B', the sequence's matrix and range (uniform: scalar registers): Y' = Y * ys + yo,
  // Cb = U * cs + co (and Cr of V), R' = Y' + kr Cr, B' = Y' + kb Cb, G' = Y' + kgu Cb + kgv Cr.
  float cb_ys;
  float cb_yo;
  float cb_cs;
  float cb_co;
  float cb_kr;
  float cb_kb;
  float cb_kgu;
  float cb_kgv;
};

#define EOTF_N 256u
#define GAIN_N 96u
#define GAIN_BASE 257u
#define LUT_ENTRIES 354u
#define TILE 64
#define THREADS 256u

groupshared float4 lut[LUT_ENTRIES];  // a cubic an interval, a + t (b + t (c + t d)): the EOTF's, the gain's
groupshared float3 ex[2 * THREADS];  // [quad parity][thread]: 1/8 of the right column of the thread's quad
groupshared float3 edge[TILE];       // the column left of the tile, two rows a thread of the first 32

// The transfer mode a pipeline is made for: display_pass_sdr.hlsl (1) and display_pass_hlg.hlsl (2)
// define it, this file alone is PQ (0). One pipeline a mode, because a shader's registers are those of its
// most demanding path (RDNA2, LLPC, testing/shader_regs: PQ 112, HLG 65, SDR 63 VGPRs, 4, 6 and 8 waves a
// SIMD; all three in one shader 114, 4 waves for every mode).
// DISPLAY_MODE_FROM_FLAGS (the C++ check, testing/display_pass): bits 0-1 of the flags, every mode in one.
#if defined(DISPLAY_MODE_FROM_FLAGS)
uint mode() { return cb_flags & 3u; }
#else
#ifndef DISPLAY_MODE
#define DISPLAY_MODE 0
#endif
uint mode() { return DISPLAY_MODE; }
#endif
bool bt2020() { return (cb_flags & 4u) != 0; }
bool full_range() { return (cb_flags & 8u) != 0; }
bool between_rows() { return (cb_flags & 16u) != 0; }
bool quad_path() { return (cb_flags & 32u) != 0; }
bool eight_bit() { return (cb_flags & 64u) != 0; }
bool nv12_out() { return (cb_flags & 128u) != 0; }

// The frame buffer's samples: log2 of their bytes, their mask (uniform).
uint sample_shift() { return eight_bit() ? 0u : 1u; }
uint sample_mask() { return eight_bit() ? 255u : 65535u; }

float3 mad3(float a, float3 b, float3 c) { return float3(mad(a, b.x, c.x), mad(a, b.y, c.y), mad(a, b.z, c.z)); }

// v (not negative) with the sign of e: the transfer functions are decoded with the sign kept.
float with_sign(float v, float e) { return asfloat(asuint(v) | (asuint(e) & 0x80000000u)); }

// The code of a sample of the frame buffer: 16 bits a sample with the value in the low 10, or a byte. No
// branch on the size: a shift and a mask.
float sample_at(uint offset, uint stride, int2 p) {
  const uint at = offset + (uint)p.y * stride + ((uint)p.x << sample_shift());
  return (float)((pool.Load(at & ~3u) >> ((at & 3u) * 8u)) & sample_mask());
}

// Eight bytes of a row from its sample x on: the dword pair at the address rounded down to 4, and the
// sample of x's place in it (0, or 2 for an 8-bit sample x at 2 mod 4).
struct Run {
  uint2 w;
  uint first;
};

Run load_run(uint offset, uint stride, int row, uint x) {
  const uint at = offset + (uint)row * stride + (x << sample_shift());
  Run r;
  r.w = pool.Load2(at & ~3u);
  r.first = (at & 3u) >> sample_shift();
  return r;
}

// Sample x + k of a run (k + first below 8 at 8 bits, below 4 at 16). `eight` and `k` are constants of
// the caller: at 16 bits (where x is at a multiple of 4 bytes, first 0) a half of a dword.
float run_sample(Run r, uint k, bool eight) {
  if (eight) {
    const uint pos = (r.first + k) * 8u;
    return (float)(((pos >= 32u ? r.w.y : r.w.x) >> (pos & 31u)) & 255u);
  }
  const uint word = k >= 2u ? r.w.y : r.w.x;
  return (float)((k & 1u) != 0 ? word >> 16 : word & 0xffffu);
}

// Luma sample x + k of a run at a multiple of 4 bytes (k below 4).
float luma_sample(Run r, uint k, bool eight) {
  if (eight) return (float)((r.w.x >> (8u * k)) & 255u);
  return run_sample(r, k, false);
}

// ST 2084: a PQ signal (0..1) to nits, as the model has it (the exact peak of near-black pixels).
float pq_to_nits(float e) {
  const float p = pow(clamp(e, 0.0f, 1.0f), 1.0f / 78.84375f);
  return pow(max(p - 0.8359375f, 0.0f) / (18.8515625f - 18.6875f * p), 1.0f / 0.1593017578125f) * 10000.0f;
}

float cubic(uint i, float t) {
  const float4 c = lut[i];
  return mad(t, mad(t, mad(t, c.w, c.z), c.y), c.x);
}

// The PQ EOTF of a signal m (not negative), from the table: nits. At the end (EOTF_N, a signal of 1 or
// more) the entry behind the last interval holds the value at 1.
float pq_table(float m) {
  const float t = min(min(m * cb_eotf_a, mad(m, cb_eotf_b, cb_eotf_c)), (float)EOTF_N);
  return cubic((uint)t, frac(t));
}

// The tone-map gain at x (PQ: the largest R'G'B' signal; HLG: log2 of the largest channel's nits): the
// EETF of the peak over the peak, 1/nits. Constant below the table's start (below the EETF's knee).
float gain_table(float x) {
  const float t = clamp(min(mad(x, cb_gain_s1, cb_gain_o1), mad(x, cb_gain_s2, cb_gain_o2)), 0.0f, (float)GAIN_N);
  return cubic(GAIN_BASE + (uint)t, frac(t));
}

// BT.2100 HLG: inverse OETF of one channel.
float hlg_scene(float e) {
  e = clamp(e, 0.0f, 1.0f);
  const float high = (exp((e - 0.55991073f) / 0.17883277f) + 0.28466892f) / 12.0f;
  return e <= 0.5f ? e * e / 3.0f : high;
}

// The light of one R'G'B' pixel in the source's primaries (xyz) and its tone-map gain (w), unclipped (an
// overshoot below black stays negative light until the one late clip). The peak's gain is
// eetf(peak) / max(peak, 1e-6) as in the model: the table holds eetf(peak) / peak, and peak / 1e-6 scales
// it below 1e-6 nits. The caller folds the gain into its sum. `m` is the mode: the quad path passes a
// constant, so that its code has no branch.
float4 linear_light(float3 e, uint m) {
  [branch] if (m == 1u) {
    return float4(with_sign(pow(min(abs(e.x), 1.5f), 2.4f), e.x), with_sign(pow(min(abs(e.y), 1.5f), 2.4f), e.y),
                  with_sign(pow(min(abs(e.z), 1.5f), 2.4f), e.z), 1.0f);
  }
  [branch] if (m == 2u) {
    const float3 scene = float3(with_sign(hlg_scene(abs(e.x)), e.x), with_sign(hlg_scene(abs(e.y)), e.y),
                                with_sign(hlg_scene(abs(e.z)), e.z));
    const float ys = max(dot(float3(0.2627f, 0.6780f, 0.0593f), scene), 0.0f);
    const float3 rgb = scene * (1000.0f * pow(ys, 0.2f));
    const float peak = max(rgb.x, max(rgb.y, rgb.z));
    return float4(rgb.x, rgb.y, rgb.z, gain_table(log2(max(peak, 1e-30f))) * saturate(peak * 1e6f));
  }
  const float3 rgb = float3(with_sign(pq_table(abs(e.x)), e.x), with_sign(pq_table(abs(e.y)), e.y),
                            with_sign(pq_table(abs(e.z)), e.z));
  const float emax = max(e.x, max(e.y, e.z));
  float peak = max(rgb.x, max(rgb.y, rgb.z));
  // Below about 1e-6 nits the gain follows the peak itself, which the table does not hold to its last
  // bits: those (rare, near-black) pixels compute it.
  [branch] if (emax > 0.0f && emax < cb_exact_below) peak = pq_to_nits(emax);
  return float4(rgb.x, rgb.y, rgb.z, gain_table(emax) * saturate(peak * 1e6f));
}

float4 decode_pixel(float y_code, float u_code, float v_code, uint m) {
  const float y = mad(y_code, cb_ys, cb_yo);
  const float u = mad(u_code, cb_cs, cb_co);
  const float v = mad(v_code, cb_cs, cb_co);
  return linear_light(float3(mad(cb_kr, v, y), mad(cb_kgu, u, mad(cb_kgv, v, y)), mad(cb_kb, u, y)), m);
}

// sum + w x (the gained light of a pixel).
float3 add_light(float3 sum, float4 p, float w) { return mad3(w * p.w, float3(p.x, p.y, p.z), sum); }

// BT.2087: linear BT.2020 RGB to linear BT.709 RGB.
float3 to_709(float3 c) {
  return float3(dot(float3(1.660491f, -0.5876411f, -0.07284986f), c),
                dot(float3(-0.1245505f, 1.1328999f, -0.00834942f), c),
                dot(float3(-0.01815076f, -0.1005789f, 1.1187297f), c));
}

// The average of an output pixel's footprint, in the source's primaries, to display-linear BT.709.
float3 to_display(float3 c) { return bt2020() ? to_709(c) : c; }

// One pixel of the frame: its luma and its chroma, bilinear between the four nearest chroma samples at
// the pixel's place (chroma sample (i, j) sits at luma (2i, 2j + 0.5) when bit 4 of the flags is set,
// at (2i, 2j) when not).
float4 sample_linear(int2 p) {
  p = clamp(p, int2(0, 0), int2((int)cb_src_w - 1, (int)cb_src_h - 1));
  const float2 c = (float2(p) - float2(0.0f, between_rows() ? 0.5f : 0.0f)) * 0.5f;
  const int2 base = int2(floor(c));
  const float2 f = c - float2(base);
  const int2 last = int2((int)(cb_src_w / 2u) - 1, (int)(cb_src_h / 2u) - 1);
  const int2 p00 = clamp(base, int2(0, 0), last);
  const int2 p10 = clamp(base + int2(1, 0), int2(0, 0), last);
  const int2 p01 = clamp(base + int2(0, 1), int2(0, 0), last);
  const int2 p11 = clamp(base + int2(1, 1), int2(0, 0), last);
  const float u00 = sample_at(cb_u_offset, cb_uv_stride, p00);
  const float u10 = sample_at(cb_u_offset, cb_uv_stride, p10);
  const float u01 = sample_at(cb_u_offset, cb_uv_stride, p01);
  const float u11 = sample_at(cb_u_offset, cb_uv_stride, p11);
  const float v00 = sample_at(cb_v_offset, cb_uv_stride, p00);
  const float v10 = sample_at(cb_v_offset, cb_uv_stride, p10);
  const float v01 = sample_at(cb_v_offset, cb_uv_stride, p01);
  const float v11 = sample_at(cb_v_offset, cb_uv_stride, p11);
  const float ut = mad(u10 - u00, f.x, u00);
  const float ub = mad(u11 - u01, f.x, u01);
  const float vt = mad(v10 - v00, f.x, v00);
  const float vb = mad(v11 - v01, f.x, v01);
  return decode_pixel(sample_at(cb_y_offset, cb_y_stride, p), mad(ub - ut, f.y, ut), mad(vb - vt, f.y, vt), mode());
}

// One output pixel (clamped to the output: the column left of the image repeats its first): the
// average of its footprint, each source pixel weighted by the overlap of its unit square with the
// footprint, display-linear BT.709.
float3 area_linear(int2 o) {
  o = clamp(o, int2(0, 0), int2((int)cb_out_w - 1, (int)cb_out_h - 1));
  const float2 scale = float2((float)cb_src_w, (float)cb_src_h) / float2((float)cb_out_w, (float)cb_out_h);
  const float2 lo = float2(o) * scale;
  const float2 hi = lo + scale;
  const int2 first = int2(floor(lo));
  const int2 last = min(int2(ceil(hi)) - int2(1, 1), int2((int)cb_src_w - 1, (int)cb_src_h - 1));
  float3 sum = float3(0.0f, 0.0f, 0.0f);
  float total = 0.0f;
  [loop] for (int sy = first.y; sy <= last.y; sy++) {
    const float wy = min((float)(sy + 1), hi.y) - max((float)sy, lo.y);
    [loop] for (int sx = first.x; sx <= last.x; sx++) {
      const float w = wy * (min((float)(sx + 1), hi.x) - max((float)sx, lo.x));
      sum = add_light(sum, sample_linear(int2(sx, sy)), w);
      total += w;
    }
  }
  return to_display(sum / max(total, 1e-6f));
}

struct Quad {
  float3 v00, v10, v01, v11;  // v<column><row>, display-linear BT.709
};

// Three chroma samples (columns c, c + 1, c + 2) of a row: the third repeats the second at the right
// edge, where c + 1 is the last column.
float3 chroma3(Run r, bool edge, bool eight) {
  const float s1 = run_sample(r, 1u, eight);
  return float3(run_sample(r, 0u, eight), s1, edge ? s1 : run_sample(r, 2u, eight));
}

// One source row of a quad: four luma samples (l) at the chroma columns c, c + 1/2, c + 1, c + 3/2, the
// chroma between the chroma rows a and b at f (U ua..ub, V va..vb). Adds the left two pixels' light to
// left, the right two's to right.
void add_row(inout float3 left, inout float3 right, Run l, float3 ua, float3 ub, float3 va, float3 vb, float f, uint m,
             bool eight) {
  const float u0 = mad(ub.x - ua.x, f, ua.x);
  const float u1 = mad(ub.y - ua.y, f, ua.y);
  const float u2 = mad(ub.z - ua.z, f, ua.z);
  const float v0 = mad(vb.x - va.x, f, va.x);
  const float v1 = mad(vb.y - va.y, f, va.y);
  const float v2 = mad(vb.z - va.z, f, va.z);
  const float4 p0 = decode_pixel(luma_sample(l, 0u, eight), u0, v0, m);
  const float4 p1 = decode_pixel(luma_sample(l, 1u, eight), (u0 + u1) * 0.5f, (v0 + v1) * 0.5f, m);
  const float4 p2 = decode_pixel(luma_sample(l, 2u, eight), u1, v1, m);
  const float4 p3 = decode_pixel(luma_sample(l, 3u, eight), (u1 + u2) * 0.5f, (v1 + v2) * 0.5f, m);
  left = add_light(add_light(left, p0, 1.0f), p1, 1.0f);
  right = add_light(add_light(right, p2, 1.0f), p3, 1.0f);
}

// The quad of output pixels at o (even) at exactly 2:1: source rows 2 o.y .. 2 o.y + 3, columns 2 o.x ..
// 2 o.x + 3, which sit at the chroma columns o.x, o.x + 1/2, o.x + 1, o.x + 3/2, in two steps of two
// rows. Source row 2 o.y + k sits at the chroma row o.y + (k - 1/2) / 2 when chroma is between rows
// (o.y - 1/4, + 1/4, + 3/4, + 5/4), at o.y + k / 2 when colocated: a step reads its two rows' luma (an
// 8-byte read each) and three chroma rows from r (o.y - 1 + p between rows, o.y + p colocated, p the
// step) of U and V, three columns each (an 8-byte read), all before any arithmetic and with no branch
// between. Two rows a step keep the registers of the tone map's table reads (PQ: 16 a row) at those of
// two rows. Every interpolated chroma value is a multiple of 1/8 below 1024, exact in float32 in any
// order, as the model's. `m` (the mode) and `eight` (8-bit samples) are constants.
Quad quad_2to1(int2 o, uint m, bool eight) {
  const uint c = (uint)o.x;
  const bool edge = c + 2u > cb_src_w / 2u - 1u;
  const int last_row = (int)(cb_src_h / 2u) - 1;
  const bool between = between_rows();
  float3 a00 = float3(0.0f, 0.0f, 0.0f), a10 = a00, a01 = a00, a11 = a00;
  [loop] for (uint p = 0; p < 2u; p++) {
    const int y = 2 * o.y + 2 * (int)p;
    const int r = (between ? o.y - 1 : o.y) + (int)p;
    const Run l0 = load_run(cb_y_offset, cb_y_stride, y, 2u * c);
    const Run l1 = load_run(cb_y_offset, cb_y_stride, y + 1, 2u * c);
    const int r0 = clamp(r, 0, last_row);
    const int r1 = clamp(r + 1, 0, last_row);
    const int r2 = clamp(r + 2, 0, last_row);
    const Run ur0 = load_run(cb_u_offset, cb_uv_stride, r0, c);
    const Run ur1 = load_run(cb_u_offset, cb_uv_stride, r1, c);
    const Run ur2 = load_run(cb_u_offset, cb_uv_stride, r2, c);
    const Run vr0 = load_run(cb_v_offset, cb_uv_stride, r0, c);
    const Run vr1 = load_run(cb_v_offset, cb_uv_stride, r1, c);
    const Run vr2 = load_run(cb_v_offset, cb_uv_stride, r2, c);
    const float3 u0 = chroma3(ur0, edge, eight), u1 = chroma3(ur1, edge, eight), u2 = chroma3(ur2, edge, eight);
    const float3 v0 = chroma3(vr0, edge, eight), v1 = chroma3(vr1, edge, eight), v2 = chroma3(vr2, edge, eight);
    float3 left = float3(0.0f, 0.0f, 0.0f);
    float3 right = left;
    // The first row between chroma rows r and r + 1 at 3/4 (between) or on r (colocated); the second
    // between r + 1 and r + 2 at 1/4, or halfway between r and r + 1. Summed in row order.
    add_row(left, right, l0, u0, u1, v0, v1, between ? 0.75f : 0.0f, m, eight);
    add_row(left, right, l1, between ? u1 : u0, between ? u2 : u1, between ? v1 : v0, between ? v2 : v1,
            between ? 0.25f : 0.5f, m, eight);
    if (p == 0u) {
      a00 = left;
      a10 = right;
    } else {
      a01 = left;
      a11 = right;
    }
  }
  Quad q;
  q.v00 = to_display(a00 * 0.25f);
  q.v10 = to_display(a10 * 0.25f);
  q.v01 = to_display(a01 * 0.25f);
  q.v11 = to_display(a11 * 0.25f);
  return q;
}

Quad quad_area(int2 o) {
  Quad q;
  q.v00 = area_linear(o);
  q.v10 = area_linear(o + int2(1, 0));
  q.v01 = area_linear(o + int2(0, 1));
  q.v11 = area_linear(o + int2(1, 1));
  return q;
}

// The quad of output pixels at o: the 2:1 path, with its mode a constant (three copies of its code, each
// without a branch between its loads and its arithmetic), or the area loop.
Quad quad_at(int2 o) {
  Quad v;
  const uint m = mode();
  [branch] if (!quad_path()) {
    v = quad_area(o);
  } else if (eight_bit()) {
    if (m == 1u) {
      v = quad_2to1(o, 1u, true);
    } else if (m == 2u) {
      v = quad_2to1(o, 2u, true);
    } else {
      v = quad_2to1(o, 0u, true);
    }
  } else if (m == 1u) {
    v = quad_2to1(o, 1u, false);
  } else if (m == 2u) {
    v = quad_2to1(o, 2u, false);
  } else {
    v = quad_2to1(o, 0u, false);
  }
  return v;
}

// The late clip (not for BT.709 SDR, which is scaled only), inverse BT.1886 with the sign kept.
float3 encode_rgb(float3 c) {
  if (!(mode() == 1u && !bt2020())) c = saturate(c);
  return float3(with_sign(pow(min(abs(c.x), 3.0f), 1.0f / 2.4f), c.x), with_sign(pow(min(abs(c.y), 3.0f), 1.0f / 2.4f), c.y),
                with_sign(pow(min(abs(c.z), 3.0f), 1.0f / 2.4f), c.z));
}

// BT.709 Y'CbCr, limited range: 10-bit codes in the high bits of P010's 16, or NV12's 8-bit codes.
uint luma_code(float3 c) {
  const float y = dot(float3(0.2126f, 0.7152f, 0.0722f), encode_rgb(c));
  if (nv12_out()) return (uint)clamp(floor(16.0f + 219.0f * y + 0.5f), 16.0f, 235.0f);
  return (uint)clamp(floor(64.0f + 876.0f * y + 0.5f), 64.0f, 940.0f) << 6;
}

uint2 chroma_code(float3 c) {
  const float3 e = encode_rgb(c);
  const float y = dot(float3(0.2126f, 0.7152f, 0.0722f), e);
  const float cb = (e.z - y) / (2.0f * (1.0f - 0.0722f));
  const float cr = (e.x - y) / (2.0f * (1.0f - 0.2126f));
  if (nv12_out()) {
    return uint2((uint)clamp(floor(128.0f + 224.0f * cb + 0.5f), 16.0f, 240.0f),
                 (uint)clamp(floor(128.0f + 224.0f * cr + 0.5f), 16.0f, 240.0f));
  }
  return uint2((uint)clamp(floor(512.0f + 896.0f * cb + 0.5f), 64.0f, 960.0f) << 6,
               (uint)clamp(floor(512.0f + 896.0f * cr + 0.5f), 64.0f, 960.0f) << 6);
}

[numthreads(256, 1, 1)] void main(uint3 group : SV_GroupID, uint lane : SV_GroupIndex) {
  if (mode() != 1u) {
    [loop] for (uint i = lane; i < LUT_ENTRIES; i += THREADS) lut[i] = asfloat(lut_src.Load4(i * 16u));
  }
  GroupMemoryBarrierWithGroupSync();

  const uint tx = lane % 32u;
  const uint ty = lane / 32u;
  const int x0 = (int)group.x * TILE;
  const int y0 = (int)group.y * TILE;
  const int ox = x0 + 2 * (int)tx;
  const int out_w = (int)cb_out_w;
  const int out_h = (int)cb_out_h;

  // Step -1: the output column left of the tile, what the first chroma column mixes in: two rows a thread
  // of the first 32, from the quad on the left (its right column; at the image's left edge the first
  // column, which the clamped column -1 repeats). Steps 0-3: the thread's quads. One call site for all.
  [loop] for (int q = -1; q < 4; q++) {
    const bool edge_step = q < 0;
    const int oy = edge_step ? y0 + 2 * (int)lane : y0 + 8 * (int)ty + 2 * q;
    const int qx = edge_step ? max(x0 - 2, 0) : ox;
    const bool inside = (edge_step ? lane < (uint)TILE / 2u : ox < out_w) && oy < out_h;
    Quad v;
    v.v00 = float3(0.0f, 0.0f, 0.0f);
    v.v10 = v.v00;
    v.v01 = v.v00;
    v.v11 = v.v00;
    [branch] if (inside) v = quad_at(int2(qx, oy));
    if (edge_step) {
      if (lane < (uint)TILE / 2u) {
        edge[2u * lane] = x0 > 0 ? v.v10 : v.v00;
        edge[2u * lane + 1u] = x0 > 0 ? v.v11 : v.v01;
      }
      continue;
    }
    float3 own = float3(0.0f, 0.0f, 0.0f);
    float3 right = float3(0.0f, 0.0f, 0.0f);
    if (inside) {
      sp_luma[int2(ox, oy)] = luma_code(v.v00);
      sp_luma[int2(ox + 1, oy)] = luma_code(v.v10);
      sp_luma[int2(ox, oy + 1)] = luma_code(v.v01);
      sp_luma[int2(ox + 1, oy + 1)] = luma_code(v.v11);
      // The output chroma sample at (ox / 2, oy / 2), left-sited: columns ox - 1, ox, ox + 1 mixed 1/4,
      // 1/2, 1/4, rows oy and oy + 1 1/2 each; this quad has the last two columns.
      own = 0.25f * (v.v00 + v.v01) + 0.125f * (v.v10 + v.v11);
      right = 0.125f * (v.v10 + v.v11);
    }
    ex[((uint)q & 1u) * THREADS + lane] = right;
    GroupMemoryBarrierWithGroupSync();
    float3 left;
    if (tx == 0u) {
      const float3 e0 = edge[8u * ty + 2u * (uint)q];
      const float3 e1 = edge[8u * ty + 2u * (uint)q + 1u];
      left = 0.125f * (e0 + e1);
    } else {
      left = ex[((uint)q & 1u) * THREADS + lane - 1u];
    }
    if (inside) sp_chroma[int2(ox / 2, oy / 2)] = chroma_code(own + left);
  }
}
