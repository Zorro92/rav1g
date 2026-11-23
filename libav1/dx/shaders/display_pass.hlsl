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
// BT.2100, BT.2390, BT.2087 and BT.709/BT.1886, in the same fp32 order so that its results can be held
// to that stage's CPU reference. Not a decoding stage: the frame buffer is only read, and every frame
// the decoder outputs or references is unchanged.
//
// A group takes 32x8 output pixels. Each output pixel's linear value (the area average) is computed
// once into group memory, with one column more on the left: the chroma footprint of the first chroma
// sample reaches it. Then every lane writes four luma samples and one chroma pair.

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
  uint cb_flags;      // bits 0-1: 0 PQ, 1 SDR, 2 HLG; bit 2: BT.2020; bit 3: full range; bit 4: chroma between rows
  float cb_peak;      // source peak, nits (1000)
  float cb_white;     // SDR white, nits (100: the 203-nit endpoint of the EETF)
};

#define TILE_W 32
#define TILE_H 8
#define VALS_W (TILE_W + 1)

groupshared float3 vals[TILE_H * VALS_W];

uint mode() { return cb_flags & 3u; }
bool bt2020() { return (cb_flags & 4u) != 0; }
bool full_range() { return (cb_flags & 8u) != 0; }

// The 10-bit code of a sample of the frame buffer: 16 bits a sample, the value in the low 10.
float sample_at(uint offset, uint stride, int2 p) {
  const uint addr = offset + (uint)p.y * stride + (uint)p.x * 2u;
  const uint word = pool.Load(addr & ~3u);
  return float((addr & 2u) ? (word >> 16) : (word & 0xffffu));
}

// ST 2084: a PQ signal (0..1) to nits, and back.
float pq_to_nits(float e) {
  const float p = pow(clamp(e, 0.0, 1.0), 1.0 / 78.84375);
  return pow(max(p - 0.8359375, 0.0) / (18.8515625 - 18.6875 * p), 1.0 / 0.1593017578125) * 10000.0;
}
float nits_to_pq(float nits) {
  const float p = pow(clamp(nits, 0.0, 10000.0) / 10000.0, 0.1593017578125);
  return pow((0.8359375 + 18.8515625 * p) / (1.0 + 18.6875 * p), 78.84375);
}

// BT.2390 EETF in the PQ domain, knee from the target, normalised so the target (2.03 x white) is 1.
float eetf(float nits) {
  nits = min(max(nits, 0.0), cb_peak);
  const float target = min(cb_white * 2.03, 10000.0);
  if (cb_peak <= target) return min(nits / target, 1.0);
  const float peak = nits_to_pq(cb_peak);
  const float s = nits_to_pq(nits) / peak;
  const float top = nits_to_pq(target) / peak;
  const float knee = 2.0 * top - 1.0;
  float mapped = s;
  if (s >= knee) {
    const float t = (s - knee) / (1.0 - knee);
    const float t2 = t * t;
    const float t3 = t2 * t;
    mapped = (2.0 * t3 - 3.0 * t2 + 1.0) * knee + (t3 - 2.0 * t2 + t) * (1.0 - knee) + (-2.0 * t3 + 3.0 * t2) * top;
  }
  return clamp(pq_to_nits(mapped * peak) / target, 0.0, 1.0);
}

float3 luma_weights(bool wide) {
  return wide ? float3(0.2627, 0.6780, 0.0593) : float3(0.2126, 0.7152, 0.0722);
}

// BT.2087: linear BT.2020 RGB to linear BT.709 RGB.
float3 to_709(float3 c) {
  return float3(dot(float3(1.660491, -0.5876411, -0.07284986), c),
                dot(float3(-0.1245505, 1.1328999, -0.00834942), c),
                dot(float3(-0.01815076, -0.1005789, 1.1187297), c));
}

// BT.2100 HLG: inverse OETF of one channel, and the OOTF of a 1000-nit display.
float hlg_scene(float e) {
  e = clamp(e, 0.0, 1.0);
  return e <= 0.5 ? e * e / 3.0 : (exp((e - 0.55991073) / 0.17883277) + 0.28466892) / 12.0;
}

// Display-linear BT.709 RGB of one pixel of the frame, unclipped: the decode keeps the sign of each
// channel (an overshoot below black stays negative light until the one late clip).
float3 pixel_linear(float y, float2 uv) {
  const bool wide = bt2020();
  const float3 w = luma_weights(wide);
  y = full_range() ? y / 1023.0 : (y - 64.0) / 876.0;
  uv = (uv - 512.0) / (full_range() ? 1023.0 : 896.0);
  const float r = y + 2.0 * (1.0 - w.r) * uv.y;
  const float b = y + 2.0 * (1.0 - w.b) * uv.x;
  const float g = (y - w.r * r - w.b * b) / w.g;
  const float3 e = float3(r, g, b);
  const float3 m = abs(e);
  const float3 s = float3(sign(e));
  float3 rgb;
  [branch] if (mode() == 1u) {
    rgb = s * pow(abs(min(m, 1.5)), 2.4);
    if (wide) rgb = to_709(rgb);
    return rgb;
  }
  [branch] if (mode() == 2u) {
    const float3 scene = s * float3(hlg_scene(m.r), hlg_scene(m.g), hlg_scene(m.b));
    const float ys = max(dot(float3(0.2627, 0.6780, 0.0593), scene), 0.0);
    rgb = scene * (1000.0 * pow(ys, 0.2));
  } else {
    rgb = s * float3(pq_to_nits(m.r), pq_to_nits(m.g), pq_to_nits(m.b));
  }
  const float peak_channel = max(rgb.r, max(rgb.g, rgb.b));
  if (wide) rgb = to_709(rgb);
  return rgb * (eetf(peak_channel) / max(peak_channel, 1e-6));
}

// One pixel of the frame: its luma and its chroma, bilinear between the four nearest chroma samples at
// the pixel's place (chroma sample (i, j) sits at luma (2i, 2j + 0.5) when bit 4 of the flags is set,
// at (2i, 2j) when not).
float3 sample_linear(int2 p) {
  p = clamp(p, int2(0, 0), int2(cb_src_w, cb_src_h) - 1);
  const float2 offset = float2(0.0, (cb_flags & 16u) ? 0.5 : 0.0);
  const float2 c = (float2(p) - offset) * 0.5;
  const int2 base = int2(floor(c));
  const float2 f = c - float2(base);
  const int2 last = int2(cb_src_w / 2u, cb_src_h / 2u) - 1;
  const int2 p00 = clamp(base, int2(0, 0), last);
  const int2 p10 = clamp(base + int2(1, 0), int2(0, 0), last);
  const int2 p01 = clamp(base + int2(0, 1), int2(0, 0), last);
  const int2 p11 = clamp(base + int2(1, 1), int2(0, 0), last);
  const float2 c00 = float2(sample_at(cb_u_offset, cb_uv_stride, p00), sample_at(cb_v_offset, cb_uv_stride, p00));
  const float2 c10 = float2(sample_at(cb_u_offset, cb_uv_stride, p10), sample_at(cb_v_offset, cb_uv_stride, p10));
  const float2 c01 = float2(sample_at(cb_u_offset, cb_uv_stride, p01), sample_at(cb_v_offset, cb_uv_stride, p01));
  const float2 c11 = float2(sample_at(cb_u_offset, cb_uv_stride, p11), sample_at(cb_v_offset, cb_uv_stride, p11));
  const float2 uv = lerp(lerp(c00, c10, f.x), lerp(c01, c11, f.x), f.y);
  return pixel_linear(sample_at(cb_y_offset, cb_y_stride, p), uv);
}

// One output pixel: the average of its footprint, each source pixel weighted by the overlap of its unit
// square with the footprint.
float3 area_linear(int2 o) {
  o = clamp(o, int2(0, 0), int2(cb_out_w, cb_out_h) - 1);
  const float2 scale = float2(cb_src_w, cb_src_h) / float2(cb_out_w, cb_out_h);
  const float2 lo = float2(o) * scale;
  const float2 hi = lo + scale;
  const int2 first = int2(floor(lo));
  const int2 last = min(int2(ceil(hi)) - 1, int2(cb_src_w, cb_src_h) - 1);
  float3 sum = float3(0.0, 0.0, 0.0);
  float total = 0.0;
  [loop] for (int sy = first.y; sy <= last.y; sy++) {
    const float wy = min(float(sy + 1), hi.y) - max(float(sy), lo.y);
    [loop] for (int sx = first.x; sx <= last.x; sx++) {
      const float w = wy * (min(float(sx + 1), hi.x) - max(float(sx), lo.x));
      sum += w * sample_linear(int2(sx, sy));
      total += w;
    }
  }
  return sum / max(total, 1e-6);
}

// The late clip (not for BT.709 SDR, which is scaled only), inverse BT.1886 with the sign kept, BT.709
// Y'CbCr.
float3 encode_output(float3 c) {
  if (!(mode() == 1u && !bt2020())) c = clamp(c, 0.0, 1.0);
  const float3 s = float3(sign(c));
  const float3 e = s * pow(min(abs(c), 3.0), 1.0 / 2.4);
  const float y = dot(luma_weights(false), e);
  return float3(y, (e.b - y) / (2.0 * (1.0 - 0.0722)), (e.r - y) / (2.0 * (1.0 - 0.2126)));
}

[numthreads(64, 1, 1)] void main(uint3 group : SV_GroupID, uint lane : SV_GroupIndex) {
  const int x0 = (int)group.x * TILE_W;
  const int y0 = (int)group.y * TILE_H;
  [loop] for (uint i = lane; i < TILE_H * VALS_W; i += 64u) {
    const int col = (int)(i % VALS_W);
    const int row = (int)(i / VALS_W);
    vals[i] = area_linear(int2(x0 - 1 + col, y0 + row));
  }
  GroupMemoryBarrierWithGroupSync();

  // Luma: four samples a lane, a row of 32 for every 32 lanes.
  [unroll] for (uint k = 0; k < 4u; k++) {
    const uint j = lane + 64u * k;
    const int col = (int)(j % TILE_W);
    const int row = (int)(j / TILE_W);
    const int2 o = int2(x0 + col, y0 + row);
    if (o.x < (int)cb_out_w && o.y < (int)cb_out_h) {
      const float y = encode_output(vals[row * VALS_W + col + 1]).x;
      sp_luma[o] = (uint)clamp(floor(64.0 + 876.0 * y + 0.5), 64.0, 940.0) << 6;
    }
  }

  // Chroma: one sample of a 16x4 tile a lane, left-sited: the output pixels' linear values mixed
  // 1/4, 1/2, 1/4 across columns 2i-1..2i+1 and 1/2, 1/2 over rows 2j, 2j+1, then clipped and encoded once.
  const int ci = (int)(lane % 16u);
  const int cj = (int)(lane / 16u);
  const int2 c = int2(x0 / 2 + ci, y0 / 2 + cj);
  if (c.x < (int)(cb_out_w / 2u) && c.y < (int)(cb_out_h / 2u)) {
    float3 acc = float3(0.0, 0.0, 0.0);
    [unroll] for (int dy = 0; dy < 2; dy++) {
      const int r = (2 * cj + dy) * VALS_W + 2 * ci;
      acc += 0.125 * vals[r];
      acc += 0.25 * vals[r + 1];
      acc += 0.125 * vals[r + 2];
    }
    const float2 uv = encode_output(acc).yz;
    const uint2 code = (uint2)clamp(floor(512.0 + 896.0 * uv + 0.5), 64.0, 960.0);
    sp_chroma[c] = code << 6;
  }
}
