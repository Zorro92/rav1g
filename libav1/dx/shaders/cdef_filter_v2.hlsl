/*
 * Copyright 2020 Google LLC
 *
 */

/*
 * Copyright (c) 2020, Alliance for Open Media. All rights reserved
 *
 * This source code is subject to the terms of the BSD 2 Clause License and
 * the Alliance for Open Media Patent License 1.0. If the BSD 2 Clause License
 * was not distributed with this source code in the LICENSE file, you can
 * obtain it at www.aomedia.org/license/software. If the Alliance for Open
 * Media Patent License 1.0 was not distributed with this source code in the
 * PATENTS file, you can obtain it at www.aomedia.org/license/patent.
 */

// Two lanes per 8x8 luma block, one group per 64x32 region. The lanes compute
// complementary direction costs and share them through LDS. Each lane writes
// four columns of luma and one chroma plane. All decisions are independent of
// wave size and lane order. The bindings are shared with cdef_filter.hlsl.

#pragma warning(disable : 4714)
ByteAddressBuffer cdef_index : register(t0);
ByteAddressBuffer skips : register(t1);
RWByteAddressBuffer dst : register(u0);

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
  int3 _dummie;

  int4 cdef_directions[16][2];
  int4 cdef_strength[16];
  int4 cdef_uv_strength[16];
};

cbuffer cb_cdef_data : register(b0) { CDefData data; }

#define CDEF_VERY_LARGE (30000)
// Two CDEF_VERY_LARGE samples in one word.
#define CDEF_VERY_LARGE_X2 (0x75307530u)

// Four-sample loads need four columns of padding on either side. The filter
// reaches two rows above and below each plane.
#define LUMA_STRIDE 72
#define LUMA_ROWS 36
#define CHROMA_STRIDE 40
#define CHROMA_ROWS 20
#define LUMA_WORDS (LUMA_ROWS * LUMA_STRIDE / 2)
#define CHROMA_WORDS (CHROMA_ROWS * CHROMA_STRIDE / 2)

groupshared uint window[LUMA_WORDS + 2 * CHROMA_WORDS];
// Four costs per lane, stored separately so adjacent lanes use adjacent words.
groupshared int costs[4][64];

// One 4-sample unit of a source plane, two samples per word. Same in-frame
// test and same masks as the loaders of cdef_filter.hlsl.
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

// Stores one 4-sample unit at sample position (x, y) of a destination plane.
void store_unit(int offset, int stride, int x, int y, uint2 v) {
  if (data.hbd) {
    dst.Store2(offset + y * stride + (x << 1), v);
  } else {
    // Samples of blocks outside the frame can be wider than 8 bits; the
    // original ORs them together shifted, so this does too.
    dst.Store(offset + y * stride + x, (v.x & 0xffffu) | ((v.x >> 16) << 8) | ((v.y & 0xffffu) << 16) | ((v.y >> 16) << 24));
  }
}

// Reflecting the block horizontally exchanges directions 0 and 4, 1 and 3,
// and 5 and 7. It preserves the two axial costs. Both lanes can therefore
// compute the same projections, then each contributes a different axial cost.
int4 direction_costs(int w0, int coeff_shift, int mirror) {
  int diagonal[15] = {0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0};
  int shallow[11] = {0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0};
  int steep[11] = {0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0};
  int columns[8] = {0, 0, 0, 0, 0, 0, 0, 0};
  const int div_table[9] = {0, 840, 420, 280, 210, 168, 140, 120, 105};
  int rows = 0;
  int i;

  [unroll] for (i = 0; i < 8; i += 2) {
    int row0 = 0;
    int row1 = 0;
    [unroll] for (int n = 0; n < 4; n++) {
      const int w = w0 + i * (LUMA_STRIDE / 2) + (n ^ (mirror * 3));
      const uint a = window[w];
      const uint b = window[w + LUMA_STRIDE / 2];
      const int a0 = ((int)((a >> (mirror * 16)) & 0xffffu) >> coeff_shift) - 128;
      const int a1 = ((int)((a >> ((1 - mirror) * 16)) & 0xffffu) >> coeff_shift) - 128;
      const int b0 = ((int)((b >> (mirror * 16)) & 0xffffu) >> coeff_shift) - 128;
      const int b1 = ((int)((b >> ((1 - mirror) * 16)) & 0xffffu) >> coeff_shift) - 128;
      const int j = 2 * n;

      diagonal[i + j] += a0;
      diagonal[i + j + 1] += a1 + b0;
      diagonal[i + j + 2] += b1;

      // Directions 1/3 and 5/7 share the adjacent-sample sums with 2 and 6.
      const int horizontal0 = a0 + a1;
      const int horizontal1 = b0 + b1;
      const int vertical0 = a0 + b0;
      const int vertical1 = a1 + b1;
      shallow[i + n] += horizontal0;
      shallow[i + n + 1] += horizontal1;
      steep[3 - (i >> 1) + j] += vertical0;
      steep[3 - (i >> 1) + j + 1] += vertical1;
      row0 += horizontal0;
      row1 += horizontal1;
      columns[j] += vertical0;
      columns[j + 1] += vertical1;
    }
    rows += row0 * row0 + row1 * row1;
  }

  int d = diagonal[7] * diagonal[7] * div_table[8];
  [unroll] for (i = 0; i < 7; i++) {
    d += (diagonal[i] * diagonal[i] + diagonal[14 - i] * diagonal[14 - i]) * div_table[i + 1];
  }
  int h = 0;
  int v = 0;
  [unroll] for (i = 3; i <= 7; i++) {
    h += shallow[i] * shallow[i];
    v += steep[i] * steep[i];
  }
  h *= div_table[8];
  v *= div_table[8];
  [unroll] for (i = 0; i < 3; i++) {
    h += (shallow[i] * shallow[i] + shallow[10 - i] * shallow[10 - i]) * div_table[2 * i + 2];
    v += (steep[i] * steep[i] + steep[10 - i] * steep[10 - i]) * div_table[2 * i + 2];
  }
  int cols = 0;
  [unroll] for (i = 0; i < 8; i++) { cols += columns[i] * columns[i]; }
  const int4 result = {d, h, (mirror ? cols : rows) * div_table[8], v};
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
  int pri_off0;
  int pri_off1;
  int sec_off0;
  int sec_off1;
  int sec_off2;
  int sec_off3;
};

int tap_offset(int dir, int tap, int stride) {
  return data.cdef_directions[dir][tap].y * stride + data.cdef_directions[dir][tap].x;
}

BlockFilter block_filter(int pri_strength, int sec_strength, int dir, int damping, int coeff_shift, int stride) {
  BlockFilter f;
  const int p_t = (pri_strength >> coeff_shift) & 1;
  f.pri_strength = pri_strength;
  f.sec_strength = sec_strength;
  f.pri_shift = max(0, damping - get_msb(pri_strength));
  f.sec_shift = max(0, damping - get_msb(sec_strength));
  f.pri_tap0 = 4 - p_t;
  f.pri_tap1 = 2 + p_t;
  f.pri_off0 = tap_offset(dir, 0, stride);
  f.pri_off1 = tap_offset(dir, 1, stride);
  f.sec_off0 = tap_offset(dir + 2, 0, stride);
  f.sec_off1 = tap_offset(dir + 2, 1, stride);
  f.sec_off2 = tap_offset(dir + 6, 0, stride);
  f.sec_off3 = tap_offset(dir + 6, 1, stride);
  return f;
}

// Four consecutive neighbours span at most three words. Share the words
// across the four output samples. The third word is within the padded row
// even when the offset is even and only two words contribute.
int4 tap_samples(int s) {
  const int w = s >> 1;
  const uint a = window[w];
  const uint b = window[w + 1];
  const uint c = window[w + 2];
  const int4 even = {(int)(a & 0xffffu), (int)(a >> 16), (int)(b & 0xffffu), (int)(b >> 16)};
  const int4 odd = {even.y, even.z, even.w, (int)(c & 0xffffu)};
  return s & 1 ? odd : even;
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
    // Samples are nonnegative. A zero contributes nothing to the maximum,
    // while constrain and the minimum still see the original sentinel.
    const int a_max = a == CDEF_VERY_LARGE ? 0 : a;
    const int b_max = b == CDEF_VERY_LARGE ? 0 : b;
    hi[j] = max(hi[j], max(a_max, b_max));
  }
}

uint2 filter_unit(int s, BlockFilter f) {
  const int4 x = tap_samples(s);
  int4 sum = {0, 0, 0, 0};
  int4 lo = x;
  int4 hi = x;
  if (f.pri_strength) {
    add_tap_pair(s, f.pri_off0, f.pri_tap0, f.pri_strength, f.pri_shift, x, sum, lo, hi);
    add_tap_pair(s, f.pri_off1, f.pri_tap1, f.pri_strength, f.pri_shift, x, sum, lo, hi);
  }
  if (f.sec_strength) {
    add_tap_pair(s, f.sec_off0, 2, f.sec_strength, f.sec_shift, x, sum, lo, hi);
    add_tap_pair(s, f.sec_off2, 2, f.sec_strength, f.sec_shift, x, sum, lo, hi);
    add_tap_pair(s, f.sec_off1, 1, f.sec_strength, f.sec_shift, x, sum, lo, hi);
    add_tap_pair(s, f.sec_off3, 1, f.sec_strength, f.sec_shift, x, sum, lo, hi);
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

void write_rows(int s, int window_stride, int target, int stride, int x, int y, int rows,
                int pri_strength, int sec_strength, int dir, int damping, int coeff_shift) {
  if ((pri_strength | sec_strength) == 0) {
    [loop] for (int r = 0; r < rows; r++) {
      const int w = (s + r * window_stride) >> 1;
      store_unit(target, stride, x, y + r, uint2(window[w], window[w + 1]));
    }
  } else {
    const BlockFilter f = block_filter(pri_strength, sec_strength, dir, damping, coeff_shift, window_stride);
    [loop] for (int r = 0; r < rows; r++) {
      store_unit(target, stride, x, y + r, filter_unit(s + r * window_stride, f));
    }
  }
}

[numthreads(16, 4, 1)] void main(uint3 group : SV_GroupID, uint3 thread : SV_GroupThreadID, uint tid : SV_GroupIndex) {
  const int lx = thread.x;
  const int ly = thread.y;
  const int side = lx & 1;
  const int bx = lx >> 1;
  const int gx = group.x * 64;
  const int gy = group.y * 32;
  const int guvx = gx >> 1;
  const int guvy = gy >> 1;
  const int x = gx + 4 * lx;
  const int y = gy + 8 * ly;
  const int uvx = guvx + 4 * bx;
  const int uvy = guvy + 4 * ly;
  const int uv_src = side ? data.uv_offset_v : data.uv_offset_u;
  const int uv_dst = side ? data.dst_offset_v : data.dst_offset_u;
  const int w4_y = data.pl.z >> 2;
  const int h_y = data.pl.w;
  const int w4_uv = data.pl.z >> 3;
  const int h_uv = data.pl.w >> 1;

  // The two 64x32 groups of a 64x64 block use the same strength index.
  const int index = cdef_index.Load(((group.y >> 1) * data.index_stride + group.x) * 4);
  uint strength_y = 0;
  uint strength_uv = 0;
  if (index >= 0 && index < 16) {
    strength_y = data.cdef_strength[index].x;
    strength_uv = data.cdef_uv_strength[index].x;
  }
  // V2 writes through the last 32x32 region. This group already covers just
  // 32 luma rows, but its right half can be past the last region.
  const bool in_extent = x < (((data.pl.z + 31) >> 5) << 5);
  // Strengths are uniform in the group, so everyone either copies or takes
  // the group sync below, including lanes beyond the extent.
  if ((strength_y | strength_uv) == 0) {
    if (in_extent) {
      copy_rows(data.pl.y, data.dst_offset_y, data.pl.x, w4_y, h_y, x, y, 8);
      copy_rows(uv_src, uv_dst, data.uv_stride, w4_uv, h_uv, uvx, uvy, 4);
    }
    return;
  }

  int r;
  [loop] for (r = ly; r < LUMA_ROWS; r += 4) {
    [loop] for (int c = lx; c < LUMA_STRIDE / 4; c += 16) {
      const uint2 v = load_unit(data.pl.x, data.pl.y, w4_y, h_y, (gx >> 2) - 1 + c, gy - 2 + r);
      const int w = r * (LUMA_STRIDE / 2) + c * 2;
      window[w] = v.x;
      window[w + 1] = v.y;
    }
  }
  const int chroma_base = LUMA_WORDS + side * CHROMA_WORDS;
  [loop] for (r = ly; r < CHROMA_ROWS; r += 4) {
    [loop] for (int c = bx; c < CHROMA_STRIDE / 4; c += 8) {
      const uint2 v = load_unit(data.uv_stride, uv_src, w4_uv, h_uv, (guvx >> 2) - 1 + c, guvy - 2 + r);
      const int w = chroma_base + r * (CHROMA_STRIDE / 2) + c * 2;
      window[w] = v.x;
      window[w + 1] = v.y;
    }
  }
  GroupMemoryBarrierWithGroupSync();

  // Skip rows are padded to the full width of the group. Lanes past the
  // extent still reach the barriers and never use their flags.
  const bool skip = skips.Load(((y >> 3) * data.skips_stride + (x >> 3)) * 4) != 0;
  const int coeff_shift = data.bit_depth - 8;
  const int luma_s0 = (2 + ly * 8) * LUMA_STRIDE + 4 + bx * 8;
  const int t_y = strength_y >> 2;
  const int t_uv = strength_uv >> 2;
  int2 dir_var = {0, 0};
  if ((t_y | t_uv) != 0) {
    const bool search = in_extent && !skip;
    int4 local = {0, 0, 0, 0};
    if (search) {
      local = direction_costs(luma_s0 >> 1, coeff_shift, side);
      costs[0][tid] = local.x;
      costs[1][tid] = local.y;
      costs[2][tid] = local.z;
      costs[3][tid] = local.w;
    }
    // The need for a primary direction is uniform. Both lanes of a block
    // use the same skip flag and either publish and read their costs or copy.
    GroupMemoryBarrierWithGroupSync();
    if (search) {
      const uint peer = tid ^ 1u;
      const int4 other = {costs[0][peer], costs[1][peer], costs[2][peer], costs[3][peer]};
      dir_var = choose_direction(side ? other : local, side ? local : other);
    }
  }
  if (!in_extent) return;

  const int s_y = strength_y & 3;
  const int s_uv = strength_uv & 3;
  const int damping = data.pri_damping + coeff_shift;
  write_rows(luma_s0 + 4 * side, LUMA_STRIDE, data.dst_offset_y, data.pl.x, x, y, 8,
             skip ? 0 : adjust_strength(t_y << coeff_shift, dir_var.y), skip ? 0 : (s_y + (s_y == 3)) << coeff_shift,
             t_y ? dir_var.x : 0, damping, coeff_shift);
  const int chroma_s0 = chroma_base * 2 + (2 + ly * 4) * CHROMA_STRIDE + 4 + bx * 4;
  write_rows(chroma_s0, CHROMA_STRIDE, uv_dst, data.uv_stride, uvx, uvy, 4,
             skip ? 0 : t_uv << coeff_shift, skip ? 0 : (s_uv + (s_uv == 3)) << coeff_shift,
             t_uv ? dir_var.x : 0, damping - 1, coeff_shift);
}
