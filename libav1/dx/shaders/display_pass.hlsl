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
// late clip; the input chroma upsampled with a Catmull-Rom cubic at the stream's siting; left-sited output
// chroma mixed in linear light), written here from ST 2084, ITU-R BT.2100,
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
//     from four rows of its source read with 8-byte loads (a wave reads 256 contiguous bytes a row), one
//     source row a step: its luma (8 bytes) and the four chroma rows its Catmull-Rom taps need, of U and V,
//     five columns each (one 8- or 12-byte read), 9 reads issued together. The taps of the row's four
//     source pixels come from those 4x5 samples a plane: filtered down once a column (5 dot products of
//     four, the weights constants of the row's parity and the siting), then across (2 more) and clamped:
//     with the extraction of the 40 samples about 23 instructions a source pixel more than bilinear
//     (~0.2 ms a 4K frame at the console's 1.86 TFLOP/s, an estimate).
//     No group memory and no more syncs; the overlapping rows of the steps are read again from the cache
//     (36 reads a quad against 16 of the bilinear version), which keeps one row's reads in registers:
//     PQ 110 VGPRs (4 waves a SIMD, as before), SDR and HLG 96 (4 waves, from 8 and 6).
//   - At 1:1 (a 4K frame on a 4K display: tone map, gamut and quantisation, no downscale) and at 3:2
//     (3840x2160 to 2560x1440) a thread takes 2x2 output pixels from a block of 2x2 or 3x3 source pixels
//     (quad_block), one source row a step as at 2:1 (its luma, and the four chroma rows of its taps, five
//     columns a plane); every source pixel decoded once and weighted by its exact overlap (1:1: 1; 3:2: 1 and
//     1/2 a side). Other ratios take the area loop of the model (a source pixel: its four tap rows of four
//     columns a plane, a row a step).
//   - The encode after the late clip is a plain power (the values are 0..1); the optional ordered dither
//     of the last quantisation (`threshold`) is integer arithmetic at the output sample, no table.
//   - A group is 256 threads and 64x64 output pixels; the output chroma's left neighbour column comes
//     through group memory from the thread to the left, and one column more on the left of the tile
//     (64 rows) is computed by the first 32 threads, a quad each (1.6% more pixels).
// Group memory 12.5 KB (the tables 5.5 KB, the column exchange 6 KB, the edge column 0.75 KB); SDR reads
// no tables (9 KB in the RDNA2 listing).
//
// The chroma reads reach up to two samples left of the first visible one of a row and four past the last:
// inside the 16-sample border every frame buffer row has on both sides (frame.rs, `h_border`), and the
// values are not used (the columns are clamped to the plane after the read).

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
                      // NV12 (else 10-bit, P010); bit 8: exactly 1:1 (the block path, n 2); bit 9:
                      // exactly 3:2 (the block path, n 3), both with planes at multiples of 4 bytes; bit
                      // 10: the ordered dither at the quantisation (else rounded); bit 11: bilinear input
                      // chroma (else Catmull-Rom; av1_set_display_bilinear_chroma)
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
// most demanding path (RDNA2, LLPC, testing/shader_regs, with the bilinear chroma of interface version 2:
// PQ 112, HLG 65, SDR 63 VGPRs, 4, 6 and 8 waves a SIMD; all three in one shader 114, 4 waves for every
// mode; with the Catmull-Rom chroma PQ 110, HLG and SDR 96, 4 waves each).
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
bool one_to_one() { return (cb_flags & 256u) != 0; }
bool three_to_two() { return (cb_flags & 512u) != 0; }
bool dithered() { return (cb_flags & 1024u) != 0; }
bool bilinear_chroma() { return (cb_flags & 2048u) != 0; }

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

// Luma sample x + k of a run at a multiple of 4 bytes (k below 4, a constant of the caller, as `eight`).
float luma_sample(Run r, uint k, bool eight) {
  if (eight) return (float)((r.w.x >> (8u * k)) & 255u);
  const uint word = k >= 2u ? r.w.y : r.w.x;
  return (float)((k & 1u) != 0 ? word >> 16 : word & 0xffffu);
}

// Sample k of a run whose sample x is anywhere in its first dword (the block paths: k + first below 8 at 8
// bits, below 4 at 16).
float run_at(Run r, uint k) {
  const uint pos = (r.first + k) << (3u + sample_shift());
  return (float)(((pos >= 32u ? r.w.y : r.w.x) >> (pos & 31u)) & sample_mask());
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
// EETF of the peak over the peak, 1/nits. Constant below the table's start (below the EETF's knee). The
// position is on two lines that meet at the source peak's signal (display_pass.rs): the smaller of them
// when the intervals below it are the denser (peaks up to about 1200 nits), the larger when those above it
// are (higher peaks, which the HDR metadata gives; interface version 2 took the smaller always, and read
// the wrong intervals of the table there). A uniform choice: the constants decide.
float gain_table(float x) {
  const float a = mad(x, cb_gain_s1, cb_gain_o1);
  const float b = mad(x, cb_gain_s2, cb_gain_o2);
  const float t = clamp(cb_gain_s1 >= cb_gain_s2 ? min(a, b) : max(a, b), 0.0f, (float)GAIN_N);
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

// The input chroma at a luma pixel (docs/integration.md, "Input chroma"): the separable 4x4 Catmull-Rom
// cubic (B 0, C 1/2; Keys' a = -1/2) of the coded chroma samples at the pixel's place, before any transfer
// or matrix, clamped to the code range of the frame's depth and range (limited 16..240 at 8 bits, 64..960
// at 10; full 0..255, 0..1023). The siting is the sequence's (AV1 6.4.2, chroma_sample_position, with the
// assumed one): chroma sample (i, j) sits at luma (2i, 2j + 1/2) between rows (bit 4 of the flags), at
// (2i, 2j) colocated. So a luma column 2t is chroma column t itself, 2t + 1 halfway between t and t + 1
// (taps t - 1 .. t + 2, -1 9 9 -1 over 16); a luma row 2t between rows is at chroma row t - 1/4 (taps t - 2
// .. t + 1, -3 29 111 -9 over 128) and 2t + 1 at t + 1/4 (taps t - 1 .. t + 2, -9 111 29 -3 over 128);
// colocated 2t is chroma row t (taps t - 2 .. t + 1 at 0 0 1 0) and 2t + 1 halfway (taps t - 1 .. t + 2).
// The taps of luma row y start at chroma row ((y + 1) >> 1) - 2 at either siting, with the weights of y's
// parity; sample coordinates outside the plane are clamped to it. Every weight is a multiple of 1/128 and
// every code an integer below 1024, so every product and partial sum is a multiple of 1/2048 below 2^24 of
// them: exact in float32, the same value in any order of the arithmetic (the model's, model.py). With bit
// 11 of the flags (av1_set_display_bilinear_chroma) the weights are the bilinear ones of the previous
// version, (0, 1 - f, f, 0), unclamped: the same chroma as it, exactly.
struct ChromaFilter {
  float4 h;       // a luma column 2t + 1: chroma columns t - 1 .. t + 2
  float4 even;    // a luma row 2t: chroma rows t - 2 .. t + 1
  float4 odd;     // a luma row 2t + 1: chroma rows t - 1 .. t + 2
  float4 lo, hi;  // the code range of the result
};

ChromaFilter chroma_filter() {
  ChromaFilter f;
  const bool bl = bilinear_chroma();
  const float top = eight_bit() ? 255.0f : 1023.0f;
  const float unit = eight_bit() ? 1.0f : 4.0f;
  f.h = bl ? float4(0.0f, 0.5f, 0.5f, 0.0f) : float4(-0.0625f, 0.5625f, 0.5625f, -0.0625f);
  if (between_rows()) {
    f.even = bl ? float4(0.0f, 0.25f, 0.75f, 0.0f) : float4(-0.0234375f, 0.2265625f, 0.8671875f, -0.0703125f);
    f.odd = bl ? float4(0.0f, 0.75f, 0.25f, 0.0f) : float4(-0.0703125f, 0.8671875f, 0.2265625f, -0.0234375f);
  } else {
    f.even = float4(0.0f, 0.0f, 1.0f, 0.0f);
    f.odd = f.h;
  }
  const float lo = bl || full_range() ? 0.0f : 16.0f * unit;
  const float hi = bl || full_range() ? top : 240.0f * unit;
  f.lo = float4(lo, lo, lo, lo);
  f.hi = float4(hi, hi, hi, hi);
  return f;
}

// Five chroma samples of a row, columns c - 1 .. c + 3 (s, then t).
struct Row5 {
  float4 s;  // c - 1 .. c + 2
  float t;   // c + 3
};

// Three dwords from the byte `at & 3` of the first on: a funnel shift, with no shift by 32.
uint3 from_byte(uint3 w, uint at) {
  const uint sh = (at & 3u) * 8u;
  return uint3((w.x >> sh) | ((w.y << 1u) << (31u - sh)), (w.y >> sh) | ((w.z << 1u) << (31u - sh)), w.z >> sh);
}

// The samples of a row whose column c - 1 is at the byte `at` (any place in the first dword of w).
// `eight`: 8-bit samples (a constant of the caller, or the uniform eight_bit()).
Row5 row5(uint3 w, uint at, bool eight) {
  const uint3 b = from_byte(w, at);
  Row5 r;
  if (eight) {
    r.s = float4((float)(b.x & 255u), (float)((b.x >> 8) & 255u), (float)((b.x >> 16) & 255u), (float)(b.x >> 24));
    r.t = (float)(b.y & 255u);
  } else {
    r.s = float4((float)(b.x & 0xffffu), (float)(b.x >> 16), (float)(b.y & 0xffffu), (float)(b.y >> 16));
    r.t = (float)(b.z & 0xffffu);
  }
  return r;
}

// The columns outside the plane (c from 0 to the last column, `last`) clamped to it.
Row5 clamp_row5(Row5 r, int c, int last) {
  if (c == 0) r.s.x = r.s.y;
  if (c + 1 > last) r.s.z = r.s.y;
  if (c + 2 > last) r.s.w = r.s.z;
  if (c + 3 > last) r.t = r.s.w;
  return r;
}

float dot4(float4 a, float4 b) { return mad(a.w, b.w, mad(a.z, b.z, mad(a.y, b.y, a.x * b.x))); }

// The five columns of a luma row's four tap rows a, b, c, d (window rows), filtered down with its
// weights w. Down first, then across: ten values a plane and step in flight instead of twenty, and the
// columns outside the plane clamped once, on the result (clamping a column commutes with filtering down).
Row5 chroma_down(Row5 a, Row5 b, Row5 c, Row5 d, float4 w) {
  Row5 r;
  r.s = float4(dot4(w, float4(a.s.x, b.s.x, c.s.x, d.s.x)), dot4(w, float4(a.s.y, b.s.y, c.s.y, d.s.y)),
               dot4(w, float4(a.s.z, b.s.z, c.s.z, d.s.z)), dot4(w, float4(a.s.w, b.s.w, c.s.w, d.s.w)));
  r.t = dot4(w, float4(a.t, b.t, c.t, d.t));
  return r;
}

// A row filtered down, across at the luma columns 2c, 2c + 1, 2c + 2, 2c + 3 (chroma columns c, c + 1/2,
// c + 1, c + 3/2), clamped to the code range.
float4 chroma_across(Row5 r, ChromaFilter f) {
  return clamp(float4(r.s.y, dot4(f.h, r.s), r.s.z, dot4(f.h, float4(r.s.y, r.s.z, r.s.w, r.t))), f.lo, f.hi);
}

// The dwords of a chroma row from its column c - 1 on, rounded down to 4 bytes, and that column's byte.
uint3 load_chroma(uint offset, int row, int c, bool eight, out uint at) {
  at = offset + (uint)row * cb_uv_stride + ((uint)(c - 1) << (eight ? 0u : 1u));
  if (eight) {
    const uint2 w = pool.Load2(at & ~3u);
    return uint3(w.x, w.y, 0u);
  }
  return pool.Load3(at & ~3u);
}

// One pixel of the frame: its luma and its chroma (above). The area loop's, the slow path of the sizes
// that are not exactly 2:1, 1:1 or 3:2: the four tap rows one after the other, two 12-byte reads each (the
// bilinear version waited for nine reads one by one), so that the loop's registers stay those of one row.
float4 sample_linear(int2 p, bool eight) {
  p = clamp(p, int2(0, 0), int2((int)cb_src_w - 1, (int)cb_src_h - 1));
  const ChromaFilter f = chroma_filter();
  const int c = p.x >> 1;
  const int last = (int)((cb_src_w + 1u) / 2u) - 1;  // (w + 1) / 2 chroma samples for an odd width
  const int last_row = (int)((cb_src_h + 1u) / 2u) - 1;
  const int r0 = ((p.y + 1) >> 1) - 2;
  const float4 w = (p.y & 1) != 0 ? f.odd : f.even;
  const bool half = (p.x & 1) != 0;  // column 2c is chroma column c, 2c + 1 halfway to c + 1
  const float y_code = sample_at(cb_y_offset, cb_y_stride, p);
  float u = 0.0f, v = 0.0f;
  [loop] for (int j = 0; j < 4; j++) {
    const int row = clamp(r0 + j, 0, last_row);
    uint au, av;
    const uint3 wu = load_chroma(cb_u_offset, row, c, eight, au);
    const uint3 wv = load_chroma(cb_v_offset, row, c, eight, av);
    const Row5 ru = clamp_row5(row5(wu, au, eight), c, last);
    const Row5 rv = clamp_row5(row5(wv, av, eight), c, last);
    const float wj = j == 0 ? w.x : j == 1 ? w.y : j == 2 ? w.z : w.w;
    u = mad(wj, half ? dot4(f.h, ru.s) : ru.s.y, u);
    v = mad(wj, half ? dot4(f.h, rv.s) : rv.s.y, v);
  }
  return decode_pixel(y_code, clamp(u, f.lo.x, f.hi.x), clamp(v, f.lo.x, f.hi.x), mode());
}

// One output pixel (clamped to the output: the column left of the image repeats its first): the
// average of its footprint, each source pixel weighted by the overlap of its unit square with the
// footprint, display-linear BT.709.
float3 area_linear(int2 o, bool eight) {
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
      sum = add_light(sum, sample_linear(int2(sx, sy), eight), w);
      total += w;
    }
  }
  return to_display(sum / max(total, 1e-6f));
}

struct Quad {
  float3 v00, v10, v01, v11;  // v<column><row>, display-linear BT.709
};

// One source row of a quad: four luma samples (l) and their chroma (u, v; at the chroma columns c, c + 1/2,
// c + 1, c + 3/2). Adds the left two pixels' light to left, the right two's to right.
void add_row(inout float3 left, inout float3 right, Run l, float4 u, float4 v, uint m, bool eight) {
  const float4 p0 = decode_pixel(luma_sample(l, 0u, eight), u.x, v.x, m);
  const float4 p1 = decode_pixel(luma_sample(l, 1u, eight), u.y, v.y, m);
  const float4 p2 = decode_pixel(luma_sample(l, 2u, eight), u.z, v.z, m);
  const float4 p3 = decode_pixel(luma_sample(l, 3u, eight), u.w, v.w, m);
  left = add_light(add_light(left, p0, 1.0f), p1, 1.0f);
  right = add_light(add_light(right, p2, 1.0f), p3, 1.0f);
}

// A row of the chroma window of the 2:1 path, columns c - 1 .. c + 3 for an even c (planes at multiples of
// 4 bytes): at 16 bits the three dwords from column c - 2 (c - 1 the high half of the first), at 8 bits the
// two from c - 1 rounded down to 4 bytes (c - 1 at byte 1 or 3). The reads reach two samples left of column
// 0 and three right of the last: inside the 16-sample border of every frame buffer row, and not used.
uint3 load_window_2to1(uint offset, int row, uint c, bool eight) {
  const uint base = offset + (uint)row * cb_uv_stride;
  if (eight) {
    const uint2 w = pool.Load2((base + c - 1u) & ~3u);
    return uint3(w.x, w.y, 0u);
  }
  return pool.Load3(base + 2u * c - 4u);
}

Row5 row5_2to1(uint3 w, uint c, bool eight) {
  if (eight) return row5(w, c - 1u, true);
  Row5 r;
  r.s = float4((float)(w.x >> 16), (float)(w.y & 0xffffu), (float)(w.y >> 16), (float)(w.z & 0xffffu));
  r.t = (float)(w.z >> 16);
  return r;
}

// The quad of output pixels at o (even) at exactly 2:1: source rows 2 o.y .. 2 o.y + 3, columns 2 o.x ..
// 2 o.x + 3 (chroma columns o.x, o.x + 1/2, o.x + 1, o.x + 3/2), one row a step. A step reads the row's
// luma (8 bytes) and the four chroma rows of its taps of U and V, columns o.x - 1 .. o.x + 3 (one 8- or
// 12-byte read each), all before any arithmetic and with no branch between: 9 reads a step. The five
// columns are filtered down with the weights of the row's parity, then across (chroma_down, chroma_across).
// One row a step keeps the registers at one row's reads and one row's tone map (two rows a step with a
// shared window of five chroma rows: PQ 128, SDR 96 VGPRs on this path alone). `m` (the mode) and `eight`
// (8-bit samples) are constants.
Quad quad_2to1(int2 o, uint m, bool eight) {
  const uint c = (uint)o.x;
  const int last = (int)(cb_src_w / 2u) - 1;
  const int last_row = (int)(cb_src_h / 2u) - 1;
  const ChromaFilter f = chroma_filter();
  float3 a00 = float3(0.0f, 0.0f, 0.0f), a10 = a00, a01 = a00, a11 = a00;
  [loop] for (uint k = 0; k < 4u; k++) {
    const int y = 2 * o.y + (int)k;
    const int r = ((y + 1) >> 1) - 2;  // the row's taps: chroma rows r .. r + 3
    const Run l = load_run(cb_y_offset, cb_y_stride, y, 2u * c);
    uint3 wu[4], wv[4];
    [unroll] for (int j = 0; j < 4; j++) {
      const int row = clamp(r + j, 0, last_row);
      wu[j] = load_window_2to1(cb_u_offset, row, c, eight);
      wv[j] = load_window_2to1(cb_v_offset, row, c, eight);
    }
    const float4 w = (k & 1u) != 0 ? f.odd : f.even;
    const int ci = (int)c;
    const float4 u = chroma_across(clamp_row5(chroma_down(row5_2to1(wu[0], c, eight), row5_2to1(wu[1], c, eight),
                                                          row5_2to1(wu[2], c, eight), row5_2to1(wu[3], c, eight), w),
                                              ci, last), f);
    const float4 v = chroma_across(clamp_row5(chroma_down(row5_2to1(wv[0], c, eight), row5_2to1(wv[1], c, eight),
                                                          row5_2to1(wv[2], c, eight), row5_2to1(wv[3], c, eight), w),
                                              ci, last), f);
    // Rows 0, 1 are the top output pixels', 2, 3 the bottom's: added pixel by pixel in row order, the
    // model's order (k is uniform: the selects are of scalars).
    const bool top = k < 2u;
    float3 left = top ? a00 : a01;
    float3 right = top ? a10 : a11;
    add_row(left, right, l, u, v, m, eight);
    if (top) {
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

// The block paths: the quad of output pixels at o (even) from the n x n source pixels at n o / 2, at
// exactly n:2 in both axes (n 2: 1:1, a 4K frame on a 4K display; n 3: 3:2, 3840x2160 to 2560x1440). Each
// source pixel is decoded once and weighted by the overlap of its unit square with the output pixels'
// footprints, as the model's area weights have it: 1 (n 2), or along an axis 1, 1/2, 0 for the first
// output pixel and 0, 1/2, 1 for the second (n 3), the four sums over 2.25. One source row a step, its
// reads first, all together and with no branch between them: its luma (8 bytes) and the four chroma rows of
// its taps, of U and of V, columns c0 - 1 .. c0 + 3 from the block's first chroma column c0 (8 or 12 bytes
// each; they reach into the 16-sample border of the rows, whose samples are not used); filtered down with
// the weights of its parity, then across; source column 2 c0 + k takes component k of that. A step a row
// keeps the registers of one row (the window of the whole block in registers: PQ 167 VGPRs at 3:2). `n`,
// the mode `m` and `eight` (8-bit samples) are constants of the caller.
// A run of the block paths, whose planes are at multiples of 4 bytes: its first sample is x's place in its
// dword, known from x alone (0 for an even x at 16 bits).
Run load_aligned(uint offset, uint stride, int row, uint x) {
  Run r;
  r.w = pool.Load2((offset + (uint)row * stride + (x << sample_shift())) & ~3u);
  r.first = x & (eight_bit() ? 3u : 1u);
  return r;
}

Quad quad_block(int2 o, uint n, uint m, bool eight) {
  // o is even: the block starts at an even source column and row for n 2.
  const int sx = (o.x >> 1) * (int)n;
  const int sy = (o.y >> 1) * (int)n;
  const ChromaFilter f = chroma_filter();
  const int last = (int)((cb_src_w + 1u) / 2u) - 1;
  const int last_row = (int)((cb_src_h + 1u) / 2u) - 1;
  const int c0 = sx >> 1;
  const bool next = n == 3u && (sx & 1) != 0;  // n 3: the block's first column at chroma column c0 + 1/2
  float3 a00 = float3(0.0f, 0.0f, 0.0f), a10 = a00, a01 = a00, a11 = a00;
  [loop] for (uint i = 0; i < n; i++) {
    const int y = sy + (int)i;
    const int r0 = ((y + 1) >> 1) - 2;  // the row's taps: chroma rows r0 .. r0 + 3
    const Run l = load_aligned(cb_y_offset, cb_y_stride, y, (uint)sx);
    uint3 wu[4], wv[4];
    uint au[4], av[4];
    [unroll] for (int j = 0; j < 4; j++) {
      const int row = clamp(r0 + j, 0, last_row);
      wu[j] = load_chroma(cb_u_offset, row, c0, eight, au[j]);
      wv[j] = load_chroma(cb_v_offset, row, c0, eight, av[j]);
    }
    const float4 w = (y & 1) != 0 ? f.odd : f.even;
    const float4 cu = chroma_across(clamp_row5(chroma_down(row5(wu[0], au[0], eight), row5(wu[1], au[1], eight),
                                                           row5(wu[2], au[2], eight), row5(wu[3], au[3], eight), w),
                                               c0, last), f);
    const float4 cv = chroma_across(clamp_row5(chroma_down(row5(wv[0], av[0], eight), row5(wv[1], av[1], eight),
                                                           row5(wv[2], av[2], eight), row5(wv[3], av[3], eight), w),
                                               c0, last), f);
    float3 left = float3(0.0f, 0.0f, 0.0f), right = left;
    [unroll] for (uint k = 0; k < 3u; k++) {
      if (k < n) {
        // Source column sx + k is chroma column c0 + (k + (sx & 1)) / 2: component k + (sx & 1).
        const float u = k == 0u ? (next ? cu.y : cu.x) : k == 1u ? (next ? cu.z : cu.y) : (next ? cu.w : cu.z);
        const float v = k == 0u ? (next ? cv.y : cv.x) : k == 1u ? (next ? cv.z : cv.y) : (next ? cv.w : cv.z);
        const float4 p = decode_pixel(run_at(l, k), u, v, m);
        if (n == 2u) {
          if (k == 0u) left = add_light(left, p, 1.0f);
          else right = add_light(right, p, 1.0f);
        } else {
          // Columns 0, 1, 2: the left output pixel 1, 1/2, 0; the right 0, 1/2, 1.
          if (k < 2u) left = add_light(left, p, k == 0u ? 1.0f : 0.5f);
          if (k > 0u) right = add_light(right, p, k == 2u ? 1.0f : 0.5f);
        }
      }
    }
    // Rows: n 2 the top output pixels row 0, the bottom row 1; n 3 the top 1, 1/2, 0, the bottom 0, 1/2, 1.
    const float top = n == 2u ? (i == 0u ? 1.0f : 0.0f) : (i == 0u ? 1.0f : i == 1u ? 0.5f : 0.0f);
    const float bottom = n == 2u ? (i == 1u ? 1.0f : 0.0f) : (i == 0u ? 0.0f : i == 1u ? 0.5f : 1.0f);
    a00 = mad3(top, left, a00);
    a10 = mad3(top, right, a10);
    a01 = mad3(bottom, left, a01);
    a11 = mad3(bottom, right, a11);
  }
  if (n == 3u) {
    a00 = a00 / 2.25f;
    a10 = a10 / 2.25f;
    a01 = a01 / 2.25f;
    a11 = a11 / 2.25f;
  }
  Quad q;
  q.v00 = to_display(a00);
  q.v10 = to_display(a10);
  q.v01 = to_display(a01);
  q.v11 = to_display(a11);
  return q;
}

Quad quad_area(int2 o, bool eight) {
  Quad q;
  q.v00 = area_linear(o, eight);
  q.v10 = area_linear(o + int2(1, 0), eight);
  q.v01 = area_linear(o + int2(0, 1), eight);
  q.v11 = area_linear(o + int2(1, 1), eight);
  return q;
}

// The quad of output pixels at o: the block paths (1:1, 3:2) and the 2:1 path with the sample size a
// constant (and the 2:1 path its mode: in the pipelines of display_pass_sdr.hlsl and display_pass_hlg.hlsl it
// is one anyway), each without a branch between its loads and its arithmetic, or the area loop.
Quad quad_at(int2 o) {
  Quad v;
  const uint m = mode();
  [branch] if (one_to_one()) {
    if (eight_bit()) {
      v = quad_block(o, 2u, m, true);
    } else {
      v = quad_block(o, 2u, m, false);
    }
  } else if (three_to_two()) {
    if (eight_bit()) {
      v = quad_block(o, 3u, m, true);
    } else {
      v = quad_block(o, 3u, m, false);
    }
  } else if (!quad_path()) {
    if (eight_bit()) {
      v = quad_area(o, true);
    } else {
      v = quad_area(o, false);
    }
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

// The late clip (not for BT.709 SDR, which is scaled only), inverse BT.1886 with the sign kept. After the
// clip the values are 0..1: the power alone (the same values; pow(0, 1/2.4) is 0).
float3 encode_rgb(float3 c) {
  if (!(mode() == 1u && !bt2020())) {
    c = saturate(c);
    return float3(pow(c.x, 1.0f / 2.4f), pow(c.y, 1.0f / 2.4f), pow(c.z, 1.0f / 2.4f));
  }
  return float3(with_sign(pow(min(abs(c.x), 3.0f), 1.0f / 2.4f), c.x), with_sign(pow(min(abs(c.y), 3.0f), 1.0f / 2.4f), c.y),
                with_sign(pow(min(abs(c.z), 3.0f), 1.0f / 2.4f), c.z));
}

// The threshold of the last quantisation at the output sample p: 0.5 (rounding), or with the dither (bit
// 10, docs/integration.md "The dither") an ordered dither on the R2 sequence (M. Roberts, "The unreasonable
// effectiveness of quasirandom sequences", 2018: the additive recurrence of the plastic number g = 1.3247...,
// frac(x / g + y / g^2)), in 32-bit fixed point: its top 8 bits, (t + 1/2) / 256. A code is floor(v + t),
// so a flat area keeps its mean level, every code is within one of the exact value, and a slow ramp becomes
// a fine pattern without bands. No table, no period of 8 (the cross-hatch of a Bayer matrix), unbiased row
// and column means; tied to the output sample alone, so a still picture stays still. Cr takes the
// transposed lattice (x and y swapped), so that Cb and Cr do not step together.
#define R2_X 0xc13fa9a9u  // 2^32 / g
#define R2_Y 0x91e10da6u  // 2^32 / g^2
float threshold(uint2 p) {
  if (!dithered()) return 0.5f;
  return mad((float)((p.x * R2_X + p.y * R2_Y) >> 24), 1.0f / 256.0f, 0.5f / 256.0f);
}

// BT.709 Y'CbCr, limited range: 10-bit codes in the high bits of P010's 16, or NV12's 8-bit codes; p is the
// output sample (luma: its pixel; chroma: its chroma sample).
uint luma_code(float3 c, uint2 p) {
  const float y = dot(float3(0.2126f, 0.7152f, 0.0722f), encode_rgb(c));
  const float t = threshold(p);
  if (nv12_out()) return (uint)clamp(floor(16.0f + 219.0f * y + t), 16.0f, 235.0f);
  return (uint)clamp(floor(64.0f + 876.0f * y + t), 64.0f, 940.0f) << 6;
}

uint2 chroma_code(float3 c, uint2 p) {
  const float3 e = encode_rgb(c);
  const float y = dot(float3(0.2126f, 0.7152f, 0.0722f), e);
  const float cb = (e.z - y) / (2.0f * (1.0f - 0.0722f));
  const float cr = (e.x - y) / (2.0f * (1.0f - 0.2126f));
  const float tb = threshold(p);
  const float tr = threshold(uint2(p.y, p.x));
  if (nv12_out()) {
    return uint2((uint)clamp(floor(128.0f + 224.0f * cb + tb), 16.0f, 240.0f),
                 (uint)clamp(floor(128.0f + 224.0f * cr + tr), 16.0f, 240.0f));
  }
  return uint2((uint)clamp(floor(512.0f + 896.0f * cb + tb), 64.0f, 960.0f) << 6,
               (uint)clamp(floor(512.0f + 896.0f * cr + tr), 64.0f, 960.0f) << 6);
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
      const uint2 at = uint2((uint)ox, (uint)oy);
      sp_luma[int2(ox, oy)] = luma_code(v.v00, at);
      sp_luma[int2(ox + 1, oy)] = luma_code(v.v10, at + uint2(1u, 0u));
      sp_luma[int2(ox, oy + 1)] = luma_code(v.v01, at + uint2(0u, 1u));
      sp_luma[int2(ox + 1, oy + 1)] = luma_code(v.v11, at + uint2(1u, 1u));
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
    if (inside) sp_chroma[int2(ox / 2, oy / 2)] = chroma_code(own + left, uint2((uint)ox / 2u, (uint)oy / 2u));
  }
}
