/*
 * SPDX-License-Identifier: BSD-2-Clause
 *
 * This source code is subject to the terms of the BSD 2 Clause License and the Alliance for Open Media
 * Patent License 1.0. If the BSD 2 Clause License was not distributed with this source code in the LICENSE
 * file, you can obtain it at www.aomedia.org/license/software. If the Alliance for Open Media Patent
 * License 1.0 was not distributed with this source code in the PATENTS file, you can obtain it at
 * www.aomedia.org/license/patent.
 */

// CDEF filter v3 (AV1 7.15): one 64-thread group per 64x32 luma region (32x16 chroma), two lanes per
// 8x8 luma block. All decisions are independent of wave size and lane order.
//
// Improvements over cdef_filter_v2.hlsl:
//   1. LDS cut from 9728 B (3 waves/SIMD on RDNA2) to 5120 B (6 waves/SIMD) by using a 2-sample
//      horizontal halo (stride 68 luma / 36 chroma), overlapping direction-cost exchange with the
//      unloaded top/bottom halo rows, and reusing the luma LDS for chroma sequentially.
//   2. Direction search split into two passes over the 8x8 block in LDS to cut peak VGPRs well below
//      the 80-VGPR 6-wave threshold.
//   3. Reads one 32-bit active_mask per 64x32 group (from cdef_lists_v2.hlsl) to early-exit fully
//      skipped groups without touching LDS or halos, and skips chroma LDS when strength_uv == 0.
//   4. With CDEF_OUTPUT_SP_TEX defined, fuses the direct semi-planar NV12/P010 output write
//      (optionally omitting frame-buffer writes on unreferenced, non-showable frames, AV1 7.20/7.21).

#pragma warning(disable : 4714)
ByteAddressBuffer cdef_index : register(t0);
ByteAddressBuffer cdef_masks : register(t1);
RWByteAddressBuffer dst : register(u0);

#ifdef CDEF_OUTPUT_SP_TEX
#define SP_TEXTURE
#define SP_LUMA_REGISTER u1
#define SP_CHROMA_REGISTER u2
#include "output_sp.h"
#endif

struct CDefData {
  int4 pl;

  int uv_stride;
  int dst_offset_y;
  int dst_offset_u;
  int dst_offset_v;

  int uv_offset_u;
  int uv_offset_v;
  int index_stride;
  int skips_stride;

  int pri_damping;
  int sec_damping;
  int pli;
  int hbd;

  int bit_depth;
  int out_crop_w;
  int out_crop_h;
  int out_flags;

  int4 cdef_directions[16][2];
  int4 cdef_strength[16];
  int4 cdef_uv_strength[16];
};

cbuffer cb_cdef_data : register(b0) { CDefData data; }

#define CDEF_VERY_LARGE (30000)
#define CDEF_VERY_LARGE_X2 (0x75307530u)

#ifdef CDEF_OUTPUT_SP_TEX
#define CDEF_FLAG_WRITE_FB_Y 1
#define CDEF_FLAG_WRITE_FB_UV 2
#define CDEF_FLAG_OUT_LUMA 4
#define CDEF_FLAG_OUT_CHROMA 8
#define CDEF_FLAG_MONOCHROME 16
#endif

// 2-sample horizontal and vertical halo:
//   Luma:   36 rows x 68 samples = 36 x 34 words = 1224 uints (4896 B)
//   Chroma: 20 rows x 36 samples = 20 x 18 words = 360 uints/plane, 720 uints total (2880 B)
// Total LDS: 1280 uints = 5120 bytes (6 waves/SIMD on RDNA2).
// Layout during luma phase:
//   [0..67]       rows 0..1 (free during direction search -> holds blocks 0..15 side-0 costs [0..63])
//   [68..1155]    rows 2..33 (interior 32 luma rows + left/right 2-sample halo)
//   [1156..1223]  rows 34..35 (free during direction search -> holds blocks 16..31 side-0 costs [1156..1219])
//   [1224..1255]  32 packed dir_var words (dir | (var << 4))
// Layout during chroma phase (reuses lds):
//   [0..359]      U plane (20 x 18 words)
//   [360..719]    V plane (20 x 18 words)
//   [720..1231]   U/V 4-row exchange for CDEF_OUTPUT_SP_TEX (64 lanes x 8 words = 512 words)
#define LUMA_STRIDE 68
#define LUMA_W_STRIDE 34
#define LUMA_ROWS 36
#define CHROMA_STRIDE 36
#define CHROMA_W_STRIDE 18
#define CHROMA_ROWS 20
#define CHROMA_WORDS (CHROMA_ROWS * CHROMA_W_STRIDE)
#define DIR_VAR_BASE 1224
#define CHROMA_EXCH_BASE 720

groupshared uint lds[1280];

uint2 load_unit(int stride, int offset, int w4, int h, int x4, int y) {
  uint2 v = uint2(CDEF_VERY_LARGE_X2, CDEF_VERY_LARGE_X2);
  if (x4 >= 0 && y >= 0 && x4 < w4 && y < h) {
    if (data.hbd) {
      v = dst.Load2(offset + y * stride + (x4 << 3));
      v.x &= 0x03ff03ffu;
      v.y &= 0x03ff03ffu;
    } else {
      const uint c = dst.Load(offset + y * stride + x4 * 4);
      v.x = (c & 0xffu) | ((c & 0xff00u) << 8);
      v.y = ((c >> 16) & 0xffu) | ((c >> 24) << 16);
    }
  }
  return v;
}

void store_unit(int offset, int stride, int x, int y, uint2 v) {
  if (data.hbd) {
    dst.Store2(offset + y * stride + (x << 1), v);
  } else {
    dst.Store(offset + y * stride + x,
              (v.x & 0xffffu) | ((v.x >> 16) << 8) | ((v.y & 0xffffu) << 16) | ((v.y >> 16) << 24));
  }
}

#ifdef CDEF_OUTPUT_SP_TEX
int4 unpack4(uint2 v) {
  return int4((int)(v.x & 0xffffu), (int)(v.x >> 16), (int)(v.y & 0xffffu), (int)(v.y >> 16));
}
#endif

// Single-pass direction cost search with sliding-window reduction: each diagonal, shallow, and steep
// bin is squared and folded into its scalar cost as soon as its last row contributes. Every array
// index depends only on the inner unrolled column `n`, so the outer `i` loop stays rolled (keeping
// peak live VGPRs low and preventing LDS-load hoisting across row pairs) with zero scratch.
int4 direction_costs(int w0, int coeff_shift, int mirror) {
  int i;

  int diag[9] = {0, 0, 0, 0, 0, 0, 0, 0, 0};
  int shal[5] = {0, 0, 0, 0, 0};
  int steep[8] = {0, 0, 0, 0, 0, 0, 0, 0};
  int d = 0;
  int h = 0;
  int v = 0;
  int rows = 0;

  [loop] for (i = 0; i < 8; i += 2) {
    int row0 = 0;
    int row1 = 0;
    [unroll] for (int n = 0; n < 4; n++) {
      const int w = w0 + i * LUMA_W_STRIDE + (n ^ (mirror * 3));
      const uint a = lds[w];
      const uint b = lds[w + LUMA_W_STRIDE];
      const int a0 = ((int)((a >> (mirror * 16)) & 0xffffu) >> coeff_shift) - 128;
      const int a1 = ((int)((a >> ((1 - mirror) * 16)) & 0xffffu) >> coeff_shift) - 128;
      const int b0 = ((int)((b >> (mirror * 16)) & 0xffffu) >> coeff_shift) - 128;
      const int b1 = ((int)((b >> ((1 - mirror) * 16)) & 0xffffu) >> coeff_shift) - 128;
      const int j = 2 * n;

      diag[j] += a0;
      diag[j + 1] += a1 + b0;
      diag[j + 2] += b1;

      const int horizontal0 = a0 + a1;
      const int horizontal1 = b0 + b1;
      const int vertical0 = a0 + b0;
      const int vertical1 = a1 + b1;
      shal[n] += horizontal0;
      shal[n + 1] += horizontal1;
      steep[j] += vertical0;
      steep[j + 1] += vertical1;
      row0 += horizontal0;
      row1 += horizontal1;
    }
    rows += row0 * row0 + row1 * row1;

    // Retire completed diagonal[i] and diagonal[i+1] and shift sliding window down by 2.
    {
      const int w0_div = i == 0 ? 840 : (i == 2 ? 280 : (i == 4 ? 168 : 120));
      const int w1_div = i == 0 ? 420 : (i == 2 ? 210 : (i == 4 ? 140 : 105));
      d += diag[0] * diag[0] * w0_div + diag[1] * diag[1] * w1_div;
      [unroll] for (int k = 0; k < 7; k++) diag[k] = diag[k + 2];
      diag[7] = 0;
      diag[8] = 0;
    }
    // Retire completed shallow[i] and shallow[i+1] (only for i < 6; at i == 6 all 5 bins finish below).
    if (i < 6) {
      const int w0_div = i == 0 ? 420 : (i == 2 ? 140 : 105);
      const int w1_div = i == 0 ? 210 : 105;
      h += shal[0] * shal[0] * w0_div + shal[1] * shal[1] * w1_div;
      shal[0] = shal[2];
      shal[1] = shal[3];
      shal[2] = shal[4];
      shal[3] = 0;
      shal[4] = 0;
    }
    // Retire completed steep[10 - i/2] (at steep[7]) when i < 6, and shift steep up by 1.
    if (i < 6) {
      const int sw = i == 0 ? 420 : (i == 2 ? 210 : 140);
      v += steep[7] * steep[7] * sw;
      [unroll] for (int k = 7; k > 0; k--) steep[k] = steep[k - 1];
      steep[0] = 0;
    }
  }

  // Remaining diagonal[8..14] sit in diag[0..6].
  d += diag[0] * diag[0] * 120 + diag[1] * diag[1] * 140 + diag[2] * diag[2] * 168 + diag[3] * diag[3] * 210 +
       diag[4] * diag[4] * 280 + diag[5] * diag[5] * 420 + diag[6] * diag[6] * 840;
  // Remaining shallow[6..10] sit in shal[0..4].
  h += (shal[0] * shal[0] + shal[1] * shal[1]) * 105;
  h += shal[2] * shal[2] * 140;
  h += shal[3] * shal[3] * 210;
  h += shal[4] * shal[4] * 420;

  // Remaining steep[0..7] sit in steep[0..7].
  v += steep[0] * steep[0] * 420;
  v += steep[1] * steep[1] * 210;
  v += steep[2] * steep[2] * 140;
  v += (steep[3] * steep[3] + steep[4] * steep[4] + steep[5] * steep[5] + steep[6] * steep[6] + steep[7] * steep[7]) *
       105;

  // Compute column sums only on mirror == 1, with a rolled loop so it reuses dead diag/shal/steep VGPRs.
  int axial = rows;
  if (mirror) {
    int columns[8] = {0, 0, 0, 0, 0, 0, 0, 0};
    [loop] for (i = 0; i < 8; i++) {
      [unroll] for (int n = 0; n < 4; n++) {
        const uint word = lds[w0 + i * LUMA_W_STRIDE + (n ^ 3)];
        columns[2 * n] += ((int)((word >> 16) & 0xffffu) >> coeff_shift) - 128;
        columns[2 * n + 1] += ((int)(word & 0xffffu) >> coeff_shift) - 128;
      }
    }
    int cols = 0;
    [unroll] for (i = 0; i < 8; i++) { cols += columns[i] * columns[i]; }
    axial = cols;
  }

  const int4 result = {d, h, axial * 105, v};
  return result;
}

int2 choose_direction(int4 left, int4 right) {
  const int cost[8] = {left.x, left.y, left.z, right.y, right.x, left.w, right.z, right.w};
  int best_cost = 0;
  int opposite_cost = 0;
  int best_dir = 0;
  [unroll] for (int i = 0; i < 8; i++) {
    if (cost[i] > best_cost) {
      best_cost = cost[i];
      opposite_cost = cost[(i + 4) & 7];
      best_dir = i;
    }
  }
  const int2 result = {best_dir, (best_cost - opposite_cost) >> 10};
  return result;
}

int get_msb(uint n) { return max(0, (int)firstbithigh(n)); }

int constrain(int diff, int threshold, int shift) {
  const int a = abs(diff);
  const int m = min(a, max(0, threshold - (a >> shift)));
  return diff < 0 ? -m : m;
}

int adjust_strength(int strength, int var) {
  const int i = min(get_msb(var >> 6), 12);
  return var ? (strength * (4 + i) + 8) >> 4 : 0;
}

struct BlockFilter {
  int pri_strength;
  int sec_strength;
  int pri_shift;
  int sec_shift;
  int pri_tap0;
  int pri_tap1;
  int dir;
};

int tap_offset(int dir, int tap, int stride) {
  return data.cdef_directions[dir][tap].y * stride + data.cdef_directions[dir][tap].x;
}

BlockFilter block_filter(int pri_strength, int sec_strength, int dir, int damping, int coeff_shift) {
  BlockFilter f;
  const int p_t = (pri_strength >> coeff_shift) & 1;
  f.pri_strength = pri_strength;
  f.sec_strength = sec_strength;
  f.pri_shift = max(0, damping - get_msb(pri_strength));
  f.sec_shift = max(0, damping - get_msb(sec_strength));
  f.pri_tap0 = 4 - p_t;
  f.pri_tap1 = 2 + p_t;
  f.dir = dir;
  return f;
}

int4 load_even4(int s) {
  const int w = s >> 1;
  const uint a = lds[w];
  const uint b = lds[w + 1];
  const int4 res = {(int)(a & 0xffffu), (int)(a >> 16), (int)(b & 0xffffu), (int)(b >> 16)};
  return res;
}

int4 tap_samples(int s) {
  const int w = s >> 1;
  const uint a = lds[w];
  const uint b = lds[w + 1];
  if (s & 1) {
    const uint c = lds[w + 2];
    const int4 odd = {(int)(a >> 16), (int)(b & 0xffffu), (int)(b >> 16), (int)(c & 0xffffu)};
    return odd;
  }
  const int4 even = {(int)(a & 0xffffu), (int)(a >> 16), (int)(b & 0xffffu), (int)(b >> 16)};
  return even;
}

void add_tap_pair(int s, int offset, int weight, int strength, int shift, int4 x,
                  inout int4 sum, inout int4 lo, inout int4 hi) {
  const int4 pos = tap_samples(s + offset);
  const int4 neg = tap_samples(s - offset);
  [unroll] for (int j = 0; j < 4; j++) {
    const int a = pos[j];
    const int b = neg[j];
    sum[j] += weight * (constrain(a - x[j], strength, shift) + constrain(b - x[j], strength, shift));
    lo[j] = min(lo[j], min(a, b));
    const int a_max = a == CDEF_VERY_LARGE ? 0 : a;
    const int b_max = b == CDEF_VERY_LARGE ? 0 : b;
    hi[j] = max(hi[j], max(a_max, b_max));
  }
}

uint2 filter_unit(int s, BlockFilter f, int stride) {
  const int4 x = load_even4(s);
  int4 sum = {0, 0, 0, 0};
  int4 lo = x;
  int4 hi = x;
  if (f.pri_strength) {
    [loop] for (int t = 0; t < 2; t++) {
      add_tap_pair(s, tap_offset(f.dir, t, stride), t == 0 ? f.pri_tap0 : f.pri_tap1, f.pri_strength, f.pri_shift, x,
                   sum, lo, hi);
    }
  }
  if (f.sec_strength) {
    [loop] for (int k = 0; k < 4; k++) {
      const int t = k >> 1;
      const int d_off = (k & 1) ? 6 : 2;
      add_tap_pair(s, tap_offset(f.dir + d_off, t, stride), 2 - t, f.sec_strength, f.sec_shift, x, sum, lo, hi);
    }
  }
  int4 y;
  [unroll] for (int j = 0; j < 4; j++) {
    y[j] = clamp(x[j] + ((8 + sum[j] - (sum[j] < 0)) >> 4), lo[j], hi[j]);
  }
  return uint2((uint)y.x | ((uint)y.y << 16), (uint)y.z | ((uint)y.w << 16));
}

void copy_rows(int src, int target, int stride, int w4, int height, int x, int y, int rows) {
  [loop] for (int r = 0; r < rows; r++) {
    store_unit(target, stride, x, y + r, load_unit(stride, src, w4, height, x >> 2, y + r));
  }
}

void load_luma_row(int r, int lx, int gx, int gy, int w4_y, int h_y) {
  const int row_base = r * LUMA_W_STRIDE;
  const int src_y = gy - 2 + r;
  const uint2 v = load_unit(data.pl.x, data.pl.y, w4_y, h_y, (gx >> 2) + lx, src_y);
  lds[row_base + 1 + lx * 2] = v.x;
  lds[row_base + 2 + lx * 2] = v.y;
  if (lx == 0) {
    const uint2 left = load_unit(data.pl.x, data.pl.y, w4_y, h_y, (gx >> 2) - 1, src_y);
    lds[row_base] = left.y;
  } else if (lx == 1) {
    const uint2 right = load_unit(data.pl.x, data.pl.y, w4_y, h_y, (gx >> 2) + 16, src_y);
    lds[row_base + LUMA_W_STRIDE - 1] = right.x;
  }
}

#ifdef CDEF_OUTPUT_SP_TEX
void copy_chroma_sp(int uvx, int uvy, int side, int w4_uv, int h_uv, int cwidth, int cheight,
                    bool in_extent, bool write_fb_uv, bool out_chroma, bool is_monochrome) {
  if (out_chroma) {
    const int r0 = side * 2;
    const int neutral = data.hbd ? 512 : 128;
    const int4 neu4 = int4(neutral, neutral, neutral, neutral);
    [loop] for (int k = 0; k < 2; k++) {
      const int cy = uvy + r0 + k;
      if (is_monochrome) {
        if (write_fb_uv && in_extent) {
          const uint2 u = load_unit(data.uv_stride, data.uv_offset_u, w4_uv, h_uv, uvx >> 2, cy);
          const uint2 v = load_unit(data.uv_stride, data.uv_offset_v, w4_uv, h_uv, uvx >> 2, cy);
          store_unit(data.dst_offset_u, data.uv_stride, uvx, cy, u);
          store_unit(data.dst_offset_v, data.uv_stride, uvx, cy, v);
        }
        sp_store_chroma(uvx, cy, neu4, neu4, cwidth, cheight, data.hbd, int2(0, 0));
      } else {
        const uint2 u = load_unit(data.uv_stride, data.uv_offset_u, w4_uv, h_uv, uvx >> 2, cy);
        const uint2 v = load_unit(data.uv_stride, data.uv_offset_v, w4_uv, h_uv, uvx >> 2, cy);
        if (write_fb_uv && in_extent) {
          store_unit(data.dst_offset_u, data.uv_stride, uvx, cy, u);
          store_unit(data.dst_offset_v, data.uv_stride, uvx, cy, v);
        }
        sp_store_chroma(uvx, cy, unpack4(u), unpack4(v), cwidth, cheight, data.hbd, int2(0, 0));
      }
    }
  } else if (write_fb_uv && in_extent) {
    const int uv_src = side ? data.uv_offset_v : data.uv_offset_u;
    const int uv_dst = side ? data.dst_offset_v : data.dst_offset_u;
    copy_rows(uv_src, uv_dst, data.uv_stride, w4_uv, h_uv, uvx, uvy, 4);
  }
}
#endif

// Address in lds where side 0 publishes its 4 direction costs for block_id (0..31).
// Blocks 0..15 use lds[0..63] (rows 0..1, not yet loaded); blocks 16..31 use lds[1156..1219] (rows 34..35).
int cost_slot(int block_id) {
  return block_id < 16 ? block_id * 4 : 1156 + (block_id - 16) * 4;
}

[numthreads(16, 4, 1)] void main(uint3 group : SV_GroupID, uint3 thread : SV_GroupThreadID, uint tid : SV_GroupIndex) {
  const int lx = (int)(tid & 15u);
  const int ly = (int)(tid >> 4u);
  const int side = lx & 1;
  const int bx = lx >> 1;
  const int gx = group.x * 64;
  const int gy = group.y * 32;
  const int w4_y = data.pl.z >> 2;
  const int h_y = data.pl.w;
  const int w4_uv = data.pl.z >> 3;
  const int h_uv = data.pl.w >> 1;

#ifdef CDEF_OUTPUT_SP_TEX
  const bool write_fb_y = (data.out_flags & CDEF_FLAG_WRITE_FB_Y) != 0;
  const bool write_fb_uv = (data.out_flags & CDEF_FLAG_WRITE_FB_UV) != 0;
  const bool out_luma = (data.out_flags & CDEF_FLAG_OUT_LUMA) != 0;
  const bool out_chroma = (data.out_flags & CDEF_FLAG_OUT_CHROMA) != 0;
  const bool is_monochrome = (data.out_flags & CDEF_FLAG_MONOCHROME) != 0;
  const int crop_w = data.out_crop_w;
  const int crop_h = data.out_crop_h;
  const int cwidth = (crop_w + 1) >> 1;
  const int cheight = (crop_h + 1) >> 1;
#endif

  const int index = cdef_index.Load(((group.y >> 1) * data.index_stride + group.x) * 4);
  uint strength_y = 0;
  uint strength_uv = 0;
  if (index >= 0 && index < 16) {
    strength_y = data.cdef_strength[index].x;
    strength_uv = data.cdef_uv_strength[index].x;
  }
  const uint active_mask = cdef_masks.Load((group.y * data.index_stride + group.x) * 4);
  const int extent_x = ((data.pl.z + 31) >> 5) << 5;

  // Uniform early exit when CDEF is disabled or every 8x8 block in this 64x32 region is skipped.
  if ((strength_y | strength_uv) == 0u || active_mask == 0u) {
    const int x = gx + 4 * lx;
    const int y = gy + 8 * ly;
    const int uvx = (gx >> 1) + 4 * bx;
    const int uvy = (gy >> 1) + 4 * ly;
    const bool in_extent = x < extent_x;
#ifdef CDEF_OUTPUT_SP_TEX
    if (in_extent && (write_fb_y || out_luma)) {
      [loop] for (int r = 0; r < 8; r++) {
        const uint2 v = load_unit(data.pl.x, data.pl.y, w4_y, h_y, x >> 2, y + r);
        if (write_fb_y) store_unit(data.dst_offset_y, data.pl.x, x, y + r, v);
        if (out_luma) sp_store_luma(x, y + r, unpack4(v), crop_w, crop_h, data.hbd, int2(0, 0));
      }
    }
    copy_chroma_sp(uvx, uvy, side, w4_uv, h_uv, cwidth, cheight, in_extent, write_fb_uv, out_chroma, is_monochrome);
#else
    if (in_extent) {
      const int uv_src = side ? data.uv_offset_v : data.uv_offset_u;
      const int uv_dst = side ? data.dst_offset_v : data.dst_offset_u;
      copy_rows(data.pl.y, data.dst_offset_y, data.pl.x, w4_y, h_y, x, y, 8);
      copy_rows(uv_src, uv_dst, data.uv_stride, w4_uv, h_uv, uvx, uvy, 4);
    }
#endif
    return;
  }

  const int coeff_shift = data.bit_depth - 8;
  const int t_y = strength_y >> 2;
  const int t_uv = strength_uv >> 2;
  const bool need_dir = (t_y | t_uv) != 0;

  // Pass 1: load interior 32 luma rows (r = 2..33) with left/right 2-sample halo.
  // When no direction search is needed, also load top/bottom halo rows in the same pass.
  int r;
  [loop] for (r = 2 + ly; r < 34; r += 4) {
    load_luma_row(r, lx, gx, gy, w4_y, h_y);
  }
  if (!need_dir) {
    const int halo_r = ly < 2 ? ly : 32 + ly;
    load_luma_row(halo_r, lx, gx, gy, w4_y, h_y);
  }
  GroupMemoryBarrierWithGroupSync();

  const int block_id = ly * 8 + bx;
  const bool in_extent = (gx + 4 * lx) < extent_x;
  const bool skip = ((active_mask >> (uint)block_id) & 1u) == 0u;
  const int luma_s0 = (2 + ly * 8) * LUMA_STRIDE + 2 + bx * 8;

  int2 dir_var = {0, 0};
  if (need_dir) {
    const bool search = in_extent && !skip;
    int4 local = {0, 0, 0, 0};
    if (search) {
      local = direction_costs(luma_s0 >> 1, coeff_shift, side);
      if (side == 0) {
        const int cs = cost_slot(block_id);
        lds[cs + 0] = (uint)local.x;
        lds[cs + 1] = (uint)local.y;
        lds[cs + 2] = (uint)local.z;
        lds[cs + 3] = (uint)local.w;
      }
    }
    GroupMemoryBarrierWithGroupSync();

    if (search && side == 1) {
      const int cs = cost_slot(block_id);
      const int4 other = {(int)lds[cs + 0], (int)lds[cs + 1], (int)lds[cs + 2], (int)lds[cs + 3]};
      dir_var = choose_direction(other, local);
      lds[DIR_VAR_BASE + block_id] = (uint)dir_var.x | ((uint)dir_var.y << 4);
    }
    GroupMemoryBarrierWithGroupSync();

    // Now the cost slots (rows 0..1 and 34..35) are free; load the top/bottom 2-row luma halo.
    const int halo_r = ly < 2 ? ly : 32 + ly;
    load_luma_row(halo_r, lx, gx, gy, w4_y, h_y);
    GroupMemoryBarrierWithGroupSync();

    if (search && side == 0) {
      const uint packed = lds[DIR_VAR_BASE + block_id];
      dir_var.x = (int)(packed & 15u);
      dir_var.y = (int)(packed >> 4);
    }
  }

  // Filter and write luma.
  const int x = gx + 4 * lx;
  const int y = gy + 8 * ly;
  const int s_y = strength_y & 3;
  const int damping = data.pri_damping + coeff_shift;
  if (in_extent) {
    const int pri_y = skip ? 0 : adjust_strength(t_y << coeff_shift, dir_var.y);
    const int sec_y = skip ? 0 : (s_y + (s_y == 3)) << coeff_shift;
    const int s0 = luma_s0 + 4 * side;
    if ((pri_y | sec_y) == 0) {
      [loop] for (r = 0; r < 8; r++) {
        const int w = (s0 + r * LUMA_STRIDE) >> 1;
        const uint2 res = uint2(lds[w], lds[w + 1]);
#ifdef CDEF_OUTPUT_SP_TEX
        if (write_fb_y) store_unit(data.dst_offset_y, data.pl.x, x, y + r, res);
        if (out_luma) sp_store_luma(x, y + r, unpack4(res), crop_w, crop_h, data.hbd, int2(0, 0));
#else
        store_unit(data.dst_offset_y, data.pl.x, x, y + r, res);
#endif
      }
    } else {
      const BlockFilter f = block_filter(pri_y, sec_y, t_y ? dir_var.x : 0, damping, coeff_shift);
      [loop] for (r = 0; r < 8; r++) {
        const uint2 res = filter_unit(s0 + r * LUMA_STRIDE, f, LUMA_STRIDE);
#ifdef CDEF_OUTPUT_SP_TEX
        if (write_fb_y) store_unit(data.dst_offset_y, data.pl.x, x, y + r, res);
        if (out_luma) sp_store_luma(x, y + r, unpack4(res), crop_w, crop_h, data.hbd, int2(0, 0));
#else
        store_unit(data.dst_offset_y, data.pl.x, x, y + r, res);
#endif
      }
    }
  }

  const int guvx = gx >> 1;
  const int guvy = gy >> 1;
  const int uvx = guvx + 4 * bx;
  const int uvy = guvy + 4 * ly;
  const int uv_src = side ? data.uv_offset_v : data.uv_offset_u;
  const int uv_dst = side ? data.dst_offset_v : data.dst_offset_u;

  // Uniform fast path when chroma has no CDEF strength (or monochrome output-only): skip chroma LDS.
#ifdef CDEF_OUTPUT_SP_TEX
  if (strength_uv == 0u || (is_monochrome && !write_fb_uv)) {
    copy_chroma_sp(uvx, uvy, side, w4_uv, h_uv, cwidth, cheight, in_extent, write_fb_uv, out_chroma, is_monochrome);
    return;
  }
#else
  if (strength_uv == 0u) {
    if (in_extent) {
      copy_rows(uv_src, uv_dst, data.uv_stride, w4_uv, h_uv, uvx, uvy, 4);
    }
    return;
  }
#endif

  // Sync before reusing lds[0..719] for U (0..359) and V (360..719).
  GroupMemoryBarrierWithGroupSync();

  const int chroma_base = side * CHROMA_WORDS;
  [loop] for (r = ly; r < CHROMA_ROWS; r += 4) {
    const int row_base = chroma_base + r * CHROMA_W_STRIDE;
    const int src_y = guvy - 2 + r;
    const uint2 v = load_unit(data.uv_stride, uv_src, w4_uv, h_uv, (guvx >> 2) + bx, src_y);
    lds[row_base + 1 + bx * 2] = v.x;
    lds[row_base + 2 + bx * 2] = v.y;
    if (bx == 0) {
      const uint2 left = load_unit(data.uv_stride, uv_src, w4_uv, h_uv, (guvx >> 2) - 1, src_y);
      lds[row_base] = left.y;
    } else if (bx == 1) {
      const uint2 right = load_unit(data.uv_stride, uv_src, w4_uv, h_uv, (guvx >> 2) + 8, src_y);
      lds[row_base + CHROMA_W_STRIDE - 1] = right.x;
    }
  }
  GroupMemoryBarrierWithGroupSync();

  const int s_uv = strength_uv & 3;
  const int pri_uv = skip ? 0 : t_uv << coeff_shift;
  const int sec_uv = skip ? 0 : (s_uv + (s_uv == 3)) << coeff_shift;
  const int chroma_s0 = chroma_base * 2 + (2 + ly * 4) * CHROMA_STRIDE + 2 + bx * 4;

#ifdef CDEF_OUTPUT_SP_TEX
  const bool exch_uv = out_chroma && !is_monochrome;
  if (in_extent) {
    if ((pri_uv | sec_uv) == 0) {
      [loop] for (r = 0; r < 4; r++) {
        const int w = (chroma_s0 + r * CHROMA_STRIDE) >> 1;
        const uint2 res = uint2(lds[w], lds[w + 1]);
        if (write_fb_uv) store_unit(uv_dst, data.uv_stride, uvx, uvy + r, res);
        if (exch_uv) {
          lds[CHROMA_EXCH_BASE + tid * 8 + r * 2] = res.x;
          lds[CHROMA_EXCH_BASE + tid * 8 + r * 2 + 1] = res.y;
        }
      }
    } else {
      const BlockFilter f = block_filter(pri_uv, sec_uv, t_uv ? dir_var.x : 0, damping - 1, coeff_shift);
      [loop] for (r = 0; r < 4; r++) {
        const uint2 res = filter_unit(chroma_s0 + r * CHROMA_STRIDE, f, CHROMA_STRIDE);
        if (write_fb_uv) store_unit(uv_dst, data.uv_stride, uvx, uvy + r, res);
        if (exch_uv) {
          lds[CHROMA_EXCH_BASE + tid * 8 + r * 2] = res.x;
          lds[CHROMA_EXCH_BASE + tid * 8 + r * 2 + 1] = res.y;
        }
      }
    }
  }
  if (exch_uv) {
    GroupMemoryBarrierWithGroupSync();
    if (in_extent) {
      const uint u_base = CHROMA_EXCH_BASE + (tid & ~1u) * 8u;
      const uint v_base = u_base + 8u;
      const int r0 = side * 2;
      [loop] for (int k = 0; k < 2; k++) {
        const uint rr = (uint)(r0 + k) * 2u;
        const uint2 u_pack = uint2(lds[u_base + rr], lds[u_base + rr + 1u]);
        const uint2 v_pack = uint2(lds[v_base + rr], lds[v_base + rr + 1u]);
        sp_store_chroma(uvx, uvy + r0 + k, unpack4(u_pack), unpack4(v_pack), cwidth, cheight, data.hbd, int2(0, 0));
      }
    }
  } else if (out_chroma && is_monochrome) {
    const int neutral = data.hbd ? 512 : 128;
    const int4 neu4 = int4(neutral, neutral, neutral, neutral);
    const int r0 = side * 2;
    [loop] for (int k = 0; k < 2; k++) {
      sp_store_chroma(uvx, uvy + r0 + k, neu4, neu4, cwidth, cheight, data.hbd, int2(0, 0));
    }
  }
#else
  if (!in_extent) return;
  if ((pri_uv | sec_uv) == 0) {
    [loop] for (r = 0; r < 4; r++) {
      const int w = (chroma_s0 + r * CHROMA_STRIDE) >> 1;
      store_unit(uv_dst, data.uv_stride, uvx, uvy + r, uint2(lds[w], lds[w + 1]));
    }
  } else {
    const BlockFilter f = block_filter(pri_uv, sec_uv, t_uv ? dir_var.x : 0, damping - 1, coeff_shift);
    [loop] for (r = 0; r < 4; r++) {
      store_unit(uv_dst, data.uv_stride, uvx, uvy + r, filter_unit(chroma_s0 + r * CHROMA_STRIDE, f, CHROMA_STRIDE));
    }
  }
#endif
}
