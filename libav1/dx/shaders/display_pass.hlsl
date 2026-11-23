/*
 * SPDX-License-Identifier: BSD-2-Clause
 *
 * This source code is subject to the terms of the BSD 2 Clause License and the Alliance for Open Media
 * Patent License 1.0. If the BSD 2 Clause License was not distributed with this source code in the LICENSE
 * file, you can obtain it at www.aomedia.org/license/software. If the Alliance for Open Media Patent
 * License 1.0 was not distributed with this source code in the PATENTS file, you can obtain it at
 * www.aomedia.org/license/patent.
 */

// Display pass, a prototype (docs/stage-fusion.md, option B; -displaypass): the decoded frame, read from
// its frame buffer, goes straight into a display-size P010 texture as BT.709 SDR, instead of the 4K
// output image an application's own stage downscales and tone-maps.
//
// The operations are those of the display stage of the application this is measured for (area
// downscale in display-linear light, exact overlap weights; PQ through the BT.2390 EETF to a 203-nit
// SDR white, HLG through its inverse OETF and OOTF, BT.2020 SDR by gamut only, BT.709 SDR scaled only;
// one late clip; left-sited output chroma mixed in linear light), written here from ST 2084, ITU-R
// BT.2100, BT.2390, BT.2087 and BT.709/BT.1886. testing/display_pass/model.py is the float32 statement
// of them that the pass is held to. Not a decoding stage: the frame buffer is only read, and every frame
// the decoder outputs or references is unchanged.
//
// What makes it fast (docs/stage-fusion.md, "The display pass, fast"): the pass is bound by arithmetic,
// not by its 31 MB of traffic, and most of the arithmetic was the transfer functions of every source
// pixel. So:
//   - PQ: the EOTF (ST 2084) and the tone-map gain (the EETF of the largest channel over that channel,
//     a function of the largest R'G'B' signal alone) are tables the host computes once with the model's
//     float32 functions (output.rs, display_pass_lut), read into group memory, linearly interpolated.
//     The EOTF table has two pieces, 256 intervals up to a signal of 1/32 and 768 above, so that an
//     interpolation error stays below 0.1 code of the output for any grey (the model's check says by how
//     much a pixel may differ, from the condition of its arithmetic: model.py). HLG takes the gain
//     table on log2 nits; SDR computes its power 2.4 directly (no cheaper table is as exact) and loads no
//     table.
//   - BT.2020 to BT.709 is linear and so is the area average: it is applied once to the average of an
//     output pixel, not to every source pixel.
//   - At 2:1 (3840x2160 to 1920x1080, the case the pass is sized for) a thread takes 2x2 output pixels
//     from four rows of its source read with 8-byte loads (a wave reads 256 contiguous bytes a row) and
//     the chroma rows it shares with the quad below it kept in registers; the chroma is interpolated at
//     the four sited positions with constant weights. Other ratios take the area loop of the model.
//   - A group is 256 threads and 64x64 output pixels; the output chroma's left neighbour column comes
//     through group memory from the thread to the left, and one column more on the left of the tile
//     (64 rows) is computed by the first 64 threads, 1.6% more pixels.
// Group memory 12.7 KB (the two tables 6 KB, the column exchange 6.5 KB).

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
  uint cb_y_offset;   // the planes of the frame buffer, 16-bit samples
  uint cb_u_offset;
  uint cb_v_offset;
  uint cb_y_stride;
  uint cb_uv_stride;
  uint cb_flags;      // bits 0-1: 0 PQ, 1 SDR, 2 HLG; bit 2: BT.2020; bit 3: full range; bit 4: chroma between
                      // rows; bit 5: exactly 2:1 with planes at multiples of 4 bytes (the quad path)
  float cb_eotf_a;    // the EOTF table's position of a signal m: min(m * a, m * b + c), at most EOTF_N
  float cb_eotf_b;
  float cb_eotf_c;
  float cb_gain_scale;   // the gain table's position of x (PQ: the largest signal; HLG: log2 nits): x * scale + offset
  float cb_gain_offset;
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

#define EOTF_N 1024u
#define GAIN_N 512u
#define GAIN_BASE 1025u
#define LUT_WORDS 1540u
#define TILE 64
#define THREADS 256u

groupshared float lut[LUT_WORDS];
groupshared float3 ex[2 * THREADS];  // [quad parity][thread]: 1/8 of the right column of the thread's quad
groupshared float3 edge[TILE];       // the column left of the tile, a row a thread of the first 64

uint mode() { return cb_flags & 3u; }
bool bt2020() { return (cb_flags & 4u) != 0; }
bool full_range() { return (cb_flags & 8u) != 0; }
bool between_rows() { return (cb_flags & 16u) != 0; }
bool quad_path() { return (cb_flags & 32u) != 0; }

float3 mad3(float a, float3 b, float3 c) { return float3(mad(a, b.x, c.x), mad(a, b.y, c.y), mad(a, b.z, c.z)); }

// v (not negative) with the sign of e: the transfer functions are decoded with the sign kept.
float with_sign(float v, float e) { return asfloat(asuint(v) | (asuint(e) & 0x80000000u)); }

float lo16(uint w) { return (float)(w & 0xffffu); }
float hi16(uint w) { return (float)(w >> 16); }

// The 10-bit code of a sample of the frame buffer: 16 bits a sample, the value in the low 10.
float sample_at(uint offset, uint stride, int2 p) {
  const uint addr = offset + (uint)p.y * stride + (uint)p.x * 2u;
  const uint word = pool.Load(addr & ~3u);
  return (addr & 2u) ? hi16(word) : lo16(word);
}

// ST 2084: a PQ signal (0..1) to nits, as the model has it (the exact peak of near-black pixels).
float pq_to_nits(float e) {
  const float p = pow(clamp(e, 0.0f, 1.0f), 1.0f / 78.84375f);
  return pow(max(p - 0.8359375f, 0.0f) / (18.8515625f - 18.6875f * p), 1.0f / 0.1593017578125f) * 10000.0f;
}

// The PQ EOTF of a signal m (not negative), from the table: nits.
float pq_table(float m) {
  // At the end (t = EOTF_N) f is 0 and the word behind the table does not count.
  const float t = min(min(m * cb_eotf_a, mad(m, cb_eotf_b, cb_eotf_c)), (float)EOTF_N);
  const uint i = (uint)t;
  const float f = frac(t);
  const float a = lut[i];
  const float b = lut[i + 1u];
  return mad(f, b - a, a);
}

// The tone-map gain at x (PQ: the largest R'G'B' signal; HLG: log2 of the largest channel's nits): the
// EETF of the peak over the peak, 1/nits. Constant below the table's start (below the EETF's knee).
float gain_table(float x) {
  const float t = clamp(mad(x, cb_gain_scale, cb_gain_offset), 0.0f, (float)GAIN_N);
  const uint i = (uint)t;
  const float f = frac(t);
  const float a = lut[GAIN_BASE + i];
  const float b = lut[GAIN_BASE + i + 1u];
  return mad(f, b - a, a);
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
// it below 1e-6 nits. The caller folds the gain into its sum.
float4 linear_light(float3 e) {
  const uint m = mode();
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

float4 decode_pixel(float y_code, float u_code, float v_code) {
  const float y = mad(y_code, cb_ys, cb_yo);
  const float u = mad(u_code, cb_cs, cb_co);
  const float v = mad(v_code, cb_cs, cb_co);
  return linear_light(float3(mad(cb_kr, v, y), mad(cb_kgu, u, mad(cb_kgv, v, y)), mad(cb_kb, u, y)));
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
  return decode_pixel(sample_at(cb_y_offset, cb_y_stride, p), mad(ub - ut, f.y, ut), mad(vb - vt, f.y, vt));
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

// U (x) and V (y) of a chroma row at the columns c (even) and c + 1, one dword a plane.
uint2 chroma_pair(int row, uint c) {
  const uint at = (uint)row * cb_uv_stride + c * 2u;
  return uint2(pool.Load(cb_u_offset + at), pool.Load(cb_v_offset + at));
}

// U and V of a chroma row at the column c2 (clamped to the plane, either half of a dword).
float2 chroma_third(int row, uint c2) {
  const uint at = (uint)row * cb_uv_stride + (c2 & ~1u) * 2u;
  const uint u = pool.Load(cb_u_offset + at);
  const uint v = pool.Load(cb_v_offset + at);
  return (c2 & 1u) ? float2(hi16(u), hi16(v)) : float2(lo16(u), lo16(v));
}

// The quad of output pixels at o (even) at exactly 2:1: source rows 2 o.y .. 2 o.y + 3, columns 2 o.x ..
// 2 o.x + 3, which sit at the chroma columns o.x, o.x + 1/2, o.x + 1, o.x + 3/2: a row reads four luma
// samples (8 bytes), and U and V of its two chroma rows at three columns. Every interpolated chroma
// value is a multiple of 1/8 below 1024, exact in float32 in any order, as the model's.
Quad quad_2to1(int2 o) {
  const uint c = (uint)o.x;
  const uint c2 = min(c + 2u, cb_src_w / 2u - 1u);
  const int last_row = (int)(cb_src_h / 2u) - 1;
  float3 acc[4] = { float3(0.0f, 0.0f, 0.0f), float3(0.0f, 0.0f, 0.0f), float3(0.0f, 0.0f, 0.0f), float3(0.0f, 0.0f, 0.0f) };
  [loop] for (uint k = 0; k < 4u; k++) {
    const int y = 2 * o.y + (int)k;
    const float cy = ((float)y - (between_rows() ? 0.5f : 0.0f)) * 0.5f;
    const int base = (int)floor(cy);
    const float f = cy - (float)base;
    const int ra = clamp(base, 0, last_row);
    const int rb = clamp(base + 1, 0, last_row);
    const uint2 pa = chroma_pair(ra, c);
    const uint2 pb = chroma_pair(rb, c);
    const float2 ta = chroma_third(ra, c2);
    const float2 tb = chroma_third(rb, c2);
    const float u0 = mad(lo16(pb.x) - lo16(pa.x), f, lo16(pa.x));
    const float u1 = mad(hi16(pb.x) - hi16(pa.x), f, hi16(pa.x));
    const float u2 = mad(tb.x - ta.x, f, ta.x);
    const float v0 = mad(lo16(pb.y) - lo16(pa.y), f, lo16(pa.y));
    const float v1 = mad(hi16(pb.y) - hi16(pa.y), f, hi16(pa.y));
    const float v2 = mad(tb.y - ta.y, f, ta.y);
    const uint2 l = pool.Load2(cb_y_offset + (uint)y * cb_y_stride + c * 4u);
    const float4 p0 = decode_pixel(lo16(l.x), u0, v0);
    const float4 p1 = decode_pixel(hi16(l.x), (u0 + u1) * 0.5f, (v0 + v1) * 0.5f);
    const float4 p2 = decode_pixel(lo16(l.y), u1, v1);
    const float4 p3 = decode_pixel(hi16(l.y), (u1 + u2) * 0.5f, (v1 + v2) * 0.5f);
    if (k < 2u) {
      acc[0] = add_light(add_light(acc[0], p0, 1.0f), p1, 1.0f);
      acc[1] = add_light(add_light(acc[1], p2, 1.0f), p3, 1.0f);
    } else {
      acc[2] = add_light(add_light(acc[2], p0, 1.0f), p1, 1.0f);
      acc[3] = add_light(add_light(acc[3], p2, 1.0f), p3, 1.0f);
    }
  }
  Quad q;
  q.v00 = to_display(acc[0] * 0.25f);
  q.v10 = to_display(acc[1] * 0.25f);
  q.v01 = to_display(acc[2] * 0.25f);
  q.v11 = to_display(acc[3] * 0.25f);
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

// The late clip (not for BT.709 SDR, which is scaled only), inverse BT.1886 with the sign kept.
float3 encode_rgb(float3 c) {
  if (!(mode() == 1u && !bt2020())) c = saturate(c);
  return float3(with_sign(pow(min(abs(c.x), 3.0f), 1.0f / 2.4f), c.x), with_sign(pow(min(abs(c.y), 3.0f), 1.0f / 2.4f), c.y),
                with_sign(pow(min(abs(c.z), 3.0f), 1.0f / 2.4f), c.z));
}

uint luma_code(float3 c) {
  const float y = dot(float3(0.2126f, 0.7152f, 0.0722f), encode_rgb(c));
  return (uint)clamp(floor(64.0f + 876.0f * y + 0.5f), 64.0f, 940.0f) << 6;
}

// BT.709 Y'CbCr, 10-bit limited range.
uint2 chroma_code(float3 c) {
  const float3 e = encode_rgb(c);
  const float y = dot(float3(0.2126f, 0.7152f, 0.0722f), e);
  const float cb = (e.z - y) / (2.0f * (1.0f - 0.0722f));
  const float cr = (e.x - y) / (2.0f * (1.0f - 0.2126f));
  return uint2((uint)clamp(floor(512.0f + 896.0f * cb + 0.5f), 64.0f, 960.0f) << 6,
               (uint)clamp(floor(512.0f + 896.0f * cr + 0.5f), 64.0f, 960.0f) << 6);
}

[numthreads(256, 1, 1)] void main(uint3 group : SV_GroupID, uint lane : SV_GroupIndex) {
  if (mode() != 1u) {
    [loop] for (uint i = lane; i < LUT_WORDS / 4u; i += THREADS) {
      const uint4 w = lut_src.Load4(i * 16u);
      lut[4u * i] = asfloat(w.x);
      lut[4u * i + 1u] = asfloat(w.y);
      lut[4u * i + 2u] = asfloat(w.z);
      lut[4u * i + 3u] = asfloat(w.w);
    }
  }
  GroupMemoryBarrierWithGroupSync();

  const uint tx = lane % 32u;
  const uint ty = lane / 32u;
  const int x0 = (int)group.x * TILE;
  const int y0 = (int)group.y * TILE;
  const int ox = x0 + 2 * (int)tx;
  const int out_w = (int)cb_out_w;
  const int out_h = (int)cb_out_h;

  // The output column left of the tile: what the first chroma column mixes in.
  if (lane < (uint)TILE) {
    const int oy = y0 + (int)lane;
    edge[lane] = oy < out_h ? area_linear(int2(x0 - 1, oy)) : float3(0.0f, 0.0f, 0.0f);
  }

  [loop] for (uint q = 0; q < 4u; q++) {
    const int oy = y0 + 8 * (int)ty + 2 * (int)q;
    const bool inside = ox < out_w && oy < out_h;
    float3 own = float3(0.0f, 0.0f, 0.0f);
    float3 right = float3(0.0f, 0.0f, 0.0f);
    [branch] if (inside) {
      Quad v;
      [branch] if (quad_path()) {
        v = quad_2to1(int2(ox, oy));
      } else {
        v = quad_area(int2(ox, oy));
      }
      sp_luma[int2(ox, oy)] = luma_code(v.v00);
      sp_luma[int2(ox + 1, oy)] = luma_code(v.v10);
      sp_luma[int2(ox, oy + 1)] = luma_code(v.v01);
      sp_luma[int2(ox + 1, oy + 1)] = luma_code(v.v11);
      // The output chroma sample at (ox / 2, oy / 2), left-sited: columns ox - 1, ox, ox + 1 mixed 1/4,
      // 1/2, 1/4, rows oy and oy + 1 1/2 each; this quad has the last two columns.
      own = 0.25f * (v.v00 + v.v01) + 0.125f * (v.v10 + v.v11);
      right = 0.125f * (v.v10 + v.v11);
    }
    ex[(q & 1u) * THREADS + lane] = right;
    GroupMemoryBarrierWithGroupSync();
    float3 left;
    if (tx == 0u) {
      const float3 e0 = edge[8u * ty + 2u * q];
      const float3 e1 = edge[8u * ty + 2u * q + 1u];
      left = 0.125f * (e0 + e1);
    } else {
      left = ex[(q & 1u) * THREADS + lane - 1u];
    }
    if (inside) sp_chroma[int2(ox / 2, oy / 2)] = chroma_code(own + left);
  }
}
